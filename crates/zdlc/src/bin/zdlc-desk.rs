//! The zdlc RFQ desk: a quote board and a sealed mailbox over HTTPS (put it behind a TLS
//! terminator). It holds no funds and no keys.
//!
//! ```sh
//! zdlc-desk --listen 127.0.0.1:7900 --state desk.json [--node URL]
//! ```
//!
//! - `POST /rfqs` signed request; `GET /rfqs` open requests
//! - `POST /rfqs/{id}/quotes` signed firm quote; `GET /rfqs/{id}/quotes` its live quotes
//! - `POST /quotes/private` a quote listed nowhere (for memo invitations); `GET /quotes/{id}`
//! - `POST /mailbox` a sealed box; `GET /mailbox/{identity}?since&ts&sig` the owner's boxes
//!
//! With `--node`, quotes are checked against the chain: every funding input must be an
//! unspent output of its key, and `valid_until` in the future. Expired quotes drop off.

use std::{collections::BTreeMap, sync::Arc};

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use zcash_protocol::consensus::NetworkType;
use zdlc::{
    desk::{self, QuoteBody, RfqBody, Signed},
    keys,
    node::Node,
    wire::SealedBox,
};

#[derive(Default, Serialize, Deserialize)]
struct Book {
    rfqs: BTreeMap<String, Signed<RfqBody>>,
    quotes: BTreeMap<String, Signed<QuoteBody>>,
    mail: Vec<(u64, SealedBox)>,
    next_seq: u64,
}

struct App {
    book: Mutex<Book>,
    path: String,
    node: Option<Node>,
}

type Reply = (StatusCode, Json<Value>);

fn bad(e: impl std::fmt::Display) -> Reply {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()})))
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

impl App {
    async fn save(&self, b: &Book) {
        let tmp = format!("{}.tmp", self.path);
        if std::fs::write(&tmp, serde_json::to_vec(b).unwrap()).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    async fn tip(&self) -> Option<u32> {
        match &self.node {
            Some(n) => n.height().await.ok(),
            None => None,
        }
    }

    /// Firmness: each input is an unspent P2PKH output of its key.
    async fn check_inputs(&self, q: &QuoteBody) -> Result<(), String> {
        let Some(node) = &self.node else { return Ok(()) };
        let net = match node.blockchain_info().await.map_err(|e| e.to_string())?["chain"].as_str() {
            Some("main") => NetworkType::Main,
            Some("test") => NetworkType::Test,
            _ => NetworkType::Regtest,
        };
        for c in &q.inputs {
            let pk = secp256k1::PublicKey::from_slice(&hex::decode(&c.pubkey).map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?;
            let addr = keys::encode(&keys::p2pkh(&pk), net);
            let utxos = node.utxos(&addr).await.map_err(|e| e.to_string())?;
            let txid = zdlc::txid_from_hex(&c.txid).map_err(|e| e.to_string())?;
            if !utxos.iter().any(|u| u.0 == txid && u.1 == c.index && u.2 == c.value) {
                return Err(format!("input {}:{} is not an unspent output of its key", c.txid, c.index));
            }
        }
        Ok(())
    }
}

async fn post_rfq(State(app): State<Arc<App>>, Json(s): Json<Signed<RfqBody>>) -> Reply {
    if let Err(e) = s.verify(desk::RFQ_TAG) {
        return bad(e);
    }
    let id = desk::id_of(&s);
    let mut b = app.book.lock().await;
    b.rfqs.insert(id.clone(), s);
    app.save(&b).await;
    (StatusCode::OK, Json(json!({"id": id})))
}

async fn list_rfqs(State(app): State<Arc<App>>) -> Reply {
    let b = app.book.lock().await;
    (StatusCode::OK, Json(json!(b.rfqs.iter().collect::<Vec<_>>())))
}

async fn store_quote(app: &App, s: Signed<QuoteBody>, private: bool) -> Reply {
    if let Err(e) = s.verify(desk::QUOTE_TAG) {
        return bad(e);
    }
    if !private && !app.book.lock().await.rfqs.contains_key(&s.body.rfq_id) {
        return bad("unknown rfq");
    }
    if let Some(tip) = app.tip().await {
        if s.body.valid_until <= tip {
            return bad("quote already expired");
        }
    }
    if let Err(e) = app.check_inputs(&s.body).await {
        return bad(format!("not firm: {e}"));
    }
    let id = desk::id_of(&s);
    let mut b = app.book.lock().await;
    b.quotes.insert(id.clone(), s);
    app.save(&b).await;
    (StatusCode::OK, Json(json!({"id": id})))
}

async fn post_quote(State(app): State<Arc<App>>, Path(rfq): Path<String>, Json(s): Json<Signed<QuoteBody>>) -> Reply {
    if s.body.rfq_id != rfq {
        return bad("rfq id mismatch");
    }
    store_quote(&app, s, false).await
}

async fn post_private_quote(State(app): State<Arc<App>>, Json(s): Json<Signed<QuoteBody>>) -> Reply {
    store_quote(&app, s, true).await
}

async fn list_quotes(State(app): State<Arc<App>>, Path(rfq): Path<String>) -> Reply {
    let tip = app.tip().await;
    let b = app.book.lock().await;
    let live: Vec<_> = b
        .quotes
        .iter()
        .filter(|(_, q)| q.body.rfq_id == rfq && tip.is_none_or(|t| q.body.valid_until > t))
        .collect();
    (StatusCode::OK, Json(json!(live)))
}

async fn get_quote(State(app): State<Arc<App>>, Path(id): Path<String>) -> Reply {
    match app.book.lock().await.quotes.get(&id) {
        Some(q) => (StatusCode::OK, Json(json!(q))),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "unknown quote"}))),
    }
}

async fn post_mail(State(app): State<Arc<App>>, Json(bx): Json<SealedBox>) -> Reply {
    if bx.ct.len() > 4_000_000 {
        return bad("box too large");
    }
    let mut b = app.book.lock().await;
    let seq = b.next_seq;
    b.next_seq += 1;
    b.mail.push((seq, bx));
    app.save(&b).await;
    (StatusCode::OK, Json(json!({"seq": seq})))
}

#[derive(Deserialize)]
struct ReadQ {
    since: u64,
    ts: i64,
    sig: String,
}

async fn read_mail(State(app): State<Arc<App>>, Path(owner): Path<String>, Query(q): Query<ReadQ>) -> Reply {
    if (now() - q.ts).abs() > 300 {
        return bad("stale read request");
    }
    if let Err(e) = desk::check_read_auth(&owner, q.since, q.ts, &q.sig) {
        return bad(e);
    }
    let b = app.book.lock().await;
    let mine: Vec<_> = b.mail.iter().filter(|(s, bx)| *s >= q.since && bx.to == owner).collect();
    (StatusCode::OK, Json(json!(mine)))
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let listen = arg(&args, "--listen").unwrap_or("127.0.0.1:7900".into());
    let path = arg(&args, "--state").unwrap_or("desk.json".into());
    let book: Book = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    println!("zdlc-desk {listen}: {} rfqs, {} quotes, {} boxes", book.rfqs.len(), book.quotes.len(), book.mail.len());
    let app = Arc::new(App { book: Mutex::new(book), path, node: arg(&args, "--node").map(Node::new) });
    let router = Router::new()
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/rfqs", post(post_rfq).get(list_rfqs))
        .route("/rfqs/{id}/quotes", post(post_quote).get(list_quotes))
        .route("/quotes/private", post(post_private_quote))
        .route("/quotes/{id}", get(get_quote))
        .route("/mailbox", post(post_mail))
        .route("/mailbox/{owner}", get(read_mail))
        .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(app);
    let l = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| {
        eprintln!("zdlc-desk: {listen}: {e}");
        std::process::exit(1)
    });
    axum::serve(l, router).await.unwrap();
}
