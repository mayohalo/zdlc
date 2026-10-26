//! zdlc-room: the interactive escrow demo. One process runs both parties of an escrow,
//! a contractor and a client, as two kit nodes, and serves a page where a visitor
//! plays every role: the client accepts and funds (from the demo wallet or by scanning
//! a deposit link with their own wallet), either side can go offline or try to cheat,
//! and the visitor rules as each of the two arbiters.
//!
//! ```sh
//! zdlc-room --node $ZDLC_RPC --desk http://127.0.0.1:7900 --oracles http://127.0.0.1:7801,http://127.0.0.1:7802 \
//!           --oracle-token-file admin.token [--dir room] [--listen 127.0.0.1:8080] [--amount 0.01] \
//!           [--maturity 2] [--refund 4] [--close-wait 1] [--close-wait-secs 30] [--block-secs 75] \
//!           [--max-rooms 20] [--label testnet] [--explorer 'https://…/tx/{txid}']
//! ```
//!
//! A room lasts about 5 minutes on testnet (75 s blocks): the rulings take effect at the
//! maturity, 2 blocks after the invoice, and the refund unlocks at 4. A side whose
//! counterparty is offline settles alone `--close-wait` blocks after the maturity.
//!
//! The page and its API are public and need no token: they can only run the demo
//! escrow, at `--amount`, between the two demo nodes, with at most `--max-rooms` open at
//! once. The oracles' admin token stays on the server; the page resolves only the events
//! of the rooms it opened.

use std::{collections::HashMap, io::Write, os::unix::fs::OpenOptionsExt, sync::Arc};

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use zdlc_kit::{impls::*, templates::Template, Funding, Zdlc};

const PAGE: &str = include_str!("../../room/index.html");

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("zdlc-room: {m}");
    std::process::exit(1)
}

/// Reads 32 bytes of hex from `path`, creating it (mode 600) if missing.
fn secret(path: &str) -> [u8; 32] {
    if let Ok(h) = std::fs::read_to_string(path) {
        return hex::decode(h.trim()).ok().and_then(|b| b.try_into().ok()).unwrap_or_else(|| die(format!("{path}: 32 bytes of hex")));
    }
    let mut s = [0u8; 32];
    rand10::fill(&mut s);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).unwrap_or_else(|e| die(e));
    writeln!(f, "{}", hex::encode(s)).unwrap_or_else(|e| die(e));
    s
}

fn zat(s: &str) -> u64 {
    let (w, f) = s.split_once('.').unwrap_or((s, ""));
    if f.len() > 8 {
        die(format!("{s}: at most 8 decimals"));
    }
    w.parse::<u64>().ok().zip(format!("{f:0<8}").parse::<u64>().ok()).map(|(w, f)| w * 100_000_000 + f).unwrap_or_else(|| die(format!("{s}: not an amount")))
}

/// The node's reason for refusing a transaction, without the RPC wrapping.
fn reason(e: &str) -> String {
    let r = e.rsplit("consensus validation: ").next().unwrap_or(e);
    r.trim_end_matches(['"', '}', '\\']).to_string()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

struct Cfg {
    amount_zat: u64,
    maturity: u32,
    refund: u32,
    close_wait: (u32, i64),
    max_rooms: usize,
    block_secs: i64,
    explorer: String,
    label: String,
}

/// What the page did in a room, beyond what the nodes record. Saved to `rooms.json`.
#[derive(Default, Serialize, Deserialize)]
struct Room {
    /// Each arbiter's ruling, once given.
    arbiters: Vec<Option<String>>,
    log: Vec<Value>,
}

struct App {
    contractor: Arc<Zdlc>,
    client: Arc<Zdlc>,
    oracles: Vec<String>,
    oracle_token: String,
    http: reqwest::Client,
    rooms: Mutex<HashMap<String, Room>>,
    rooms_file: String,
    cfg: Cfg,
}

type Shared = Arc<App>;

fn err(code: StatusCode, m: impl ToString) -> Response {
    (code, Json(json!({"error": m.to_string()}))).into_response()
}

fn bad(e: impl ToString) -> Response {
    err(StatusCode::BAD_REQUEST, e)
}

impl App {
    fn side(&self, who: &str) -> Option<&Arc<Zdlc>> {
        match who {
            "contractor" => Some(&self.contractor),
            "client" => Some(&self.client),
            _ => None,
        }
    }

    async fn log(&self, id: &str, who: &str, what: impl Into<Value>) {
        let mut rooms = self.rooms.lock().await;
        if let Some(r) = rooms.get_mut(id) {
            r.log.push(json!({"t": now(), "who": who, "what": what.into()}));
        }
        self.save(&rooms);
    }

    fn save(&self, rooms: &HashMap<String, Room>) {
        let tmp = format!("{}.tmp", self.rooms_file);
        let ok = serde_json::to_vec(rooms).ok().and_then(|b| std::fs::write(&tmp, b).ok()).and_then(|_| std::fs::rename(&tmp, &self.rooms_file).ok());
        if ok.is_none() {
            eprintln!("zdlc-room: could not save {}", self.rooms_file);
        }
    }

    /// The room must be one this server opened (the contractor's offer).
    async fn known(&self, id: &str) -> Result<(), Response> {
        match self.contractor.contract(id).await {
            Ok(Some(_)) => {
                self.rooms.lock().await.entry(id.to_string()).or_insert_with(|| Room { arbiters: vec![None; self.oracles.len()], log: vec![] });
                Ok(())
            }
            Ok(None) => Err(err(StatusCode::NOT_FOUND, format!("no room {id}"))),
            Err(e) => Err(bad(e)),
        }
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn info(State(a): State<Shared>) -> Response {
    let i = match a.contractor.info().await {
        Ok(i) => i,
        Err(e) => return bad(e),
    };
    let bal = a.client.balance().await.ok();
    Json(json!({
        "network": a.cfg.label, "tip": i["tip"], "amountZat": a.cfg.amount_zat, "maturityBlocks": a.cfg.maturity,
        "refundBlocks": a.cfg.refund, "closeWaitBlocks": a.cfg.close_wait.0, "closeWaitSecs": a.cfg.close_wait.1, "blockSecs": a.cfg.block_secs, "explorer": a.cfg.explorer, "arbiters": a.oracles.len(),
        "demoWalletZat": bal.map(|b| b.treasury_zat),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct OpenReq {
    #[serde(default)]
    question: Option<String>,
}

/// The contractor invoices the client: a new escrow, a new room.
async fn open(State(a): State<Shared>, Json(r): Json<OpenReq>) -> Response {
    let open = match a.contractor.contracts().await {
        Ok(v) => v.iter().filter(|c| !matches!(c.state, zdlc_kit::State::Settled | zdlc_kit::State::Declined | zdlc_kit::State::Failed)).count(),
        Err(e) => return bad(e),
    };
    if open >= a.cfg.max_rooms {
        return err(StatusCode::TOO_MANY_REQUESTS, format!("{open} rooms are open; try again when one settles"));
    }
    let question: String = r.question.unwrap_or_else(|| "Milestone 1 delivered?".into()).chars().take(80).collect();
    let t = Template::Escrow { amount_zat: a.cfg.amount_zat, question: question.clone(), payees: vec![], maturity_blocks: a.cfg.maturity, refund_blocks: a.cfg.refund };
    match a.contractor.offer(t, &a.client.identity()).await {
        Ok(id) => {
            a.rooms.lock().await.insert(id.clone(), Room { arbiters: vec![None; a.oracles.len()], log: vec![] });
            a.log(&id, "contractor", format!("invoiced the client: \u{201c}{question}\u{201d}")).await;
            Json(json!({"id": id})).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn show(State(a): State<Shared>, Path(id): Path<String>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let tip = a.contractor.info().await.ok().map(|i| i["tip"].clone()).unwrap_or(Value::Null);
    let contractor = a.contractor.contract(&id).await.ok().flatten().map(|r| r.view());
    let client = a.client.contract(&id).await.ok().flatten().map(|r| r.view());
    let rooms = a.rooms.lock().await;
    let room = rooms.get(&id);
    Json(json!({
        "id": id, "tip": tip, "contractor": contractor, "client": client,
        "arbiters": room.map(|r| r.arbiters.clone()).unwrap_or_default(),
        "log": room.map(|r| r.log.clone()).unwrap_or_default(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AcceptReq {
    funding: String,
}

async fn accept(State(a): State<Shared>, Path(id): Path<String>, Json(r): Json<AcceptReq>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let funding = match r.funding.as_str() {
        "treasury" => Funding::Treasury,
        "deposit" => Funding::Deposit,
        f => return bad(format!("funding {f}: treasury or deposit")),
    };
    match a.client.accept(&id, funding).await {
        Ok(()) => {
            a.log(&id, "client", if r.funding == "deposit" { "accepted; funds by deposit link" } else { "accepted; funds from the demo wallet" }).await;
            Json(json!({"id": id})).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn qr(State(a): State<Shared>, Path(id): Path<String>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let Some(d) = a.client.contract(&id).await.ok().flatten().and_then(|r| r.deposit) else {
        return err(StatusCode::NOT_FOUND, "no deposit link");
    };
    match zdlc::paylink::qr_svg(&d.uri) {
        Ok(svg) => ([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct HoldReq {
    held: bool,
}

async fn hold(State(a): State<Shared>, Path((id, who)): Path<(String, String)>, Json(r): Json<HoldReq>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let Some(z) = a.side(&who) else { return bad(format!("no party {who}")) };
    match z.hold(&id, r.held).await {
        Ok(()) => {
            a.log(&id, &who, if r.held { "went offline" } else { "came back online" }).await;
            Json(json!({"held": r.held})).into_response()
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct AttemptReq {
    action: String,
}

async fn attempt(State(a): State<Shared>, Path((id, who)): Path<(String, String)>, Json(r): Json<AttemptReq>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let Some(z) = a.side(&who) else { return bad(format!("no party {who}")) };
    match z.attempt(&id, &r.action).await {
        Ok(v) => {
            let mut v = v;
            if let Some(e) = v["error"].as_str() {
                v["error"] = json!(reason(e));
            }
            let what = match (r.action.as_str(), v["accepted"].as_bool() == Some(true)) {
                ("refund", true) => "sent the refund: accepted (the refund height has passed)".to_string(),
                ("refund", false) => format!("tried to take the refund early: refused ({})", v["error"].as_str().unwrap_or("")),
                (_, true) => "tried to pay itself with forged attestations: accepted (this should never happen)".to_string(),
                (_, false) => format!("tried to pay itself with forged attestations: refused ({})", v["error"].as_str().unwrap_or("")),
            };
            a.log(&id, &who, json!({"text": what, "txid": v["txid"], "accepted": v["accepted"]})).await;
            Json(v).into_response()
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct RuleReq {
    outcome: String,
}

/// Arbiter `n` resolves the room's event.
async fn rule(State(a): State<Shared>, Path((id, n)): Path<(String, usize)>, Json(r): Json<RuleReq>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    if !matches!(r.outcome.as_str(), "delivered" | "not delivered") {
        return bad("outcome: delivered or not delivered");
    }
    let Some(url) = a.oracles.get(n) else { return bad(format!("no arbiter {n}")) };
    let Some(rec) = a.contractor.contract(&id).await.ok().flatten() else { return bad("no room") };
    if a.rooms.lock().await.get(&id).and_then(|room| room.arbiters.get(n).cloned().flatten()).is_some() {
        return bad("this arbiter has already ruled");
    }
    let res = a
        .http
        .post(format!("{}/events/{}/resolve", url.trim_end_matches('/'), rec.event_id))
        .json(&json!({"outcome": r.outcome, "token": a.oracle_token}))
        .send()
        .await;
    match res {
        Ok(resp) if resp.status().is_success() => {
            if let Some(room) = a.rooms.lock().await.get_mut(&id) {
                room.arbiters[n] = Some(r.outcome.clone());
            }
            // Saved with the log line below.
            a.log(&id, &format!("arbiter {}", n + 1), format!("ruled \u{201c}{}\u{201d}", r.outcome)).await;
            Json(json!({"arbiter": n, "outcome": r.outcome})).into_response()
        }
        Ok(resp) => {
            let t = resp.text().await.unwrap_or_default();
            bad(format!("the oracle refused: {t}"))
        }
        Err(e) => bad(format!("oracle: {e}")),
    }
}

/// One demo party. `records`: post recovery records (the contractor puts nothing in and
/// has no treasury to pay for one).
async fn node(args: &[String], dir: &str, name: &str, rpc: &str, cfg: &Cfg, records: bool) -> Arc<Zdlc> {
    let seed = secret(&format!("{dir}/{name}.seed"));
    let wallet = zdlc::wallet::Wallet::from_seed(seed);
    let storage = Arc::new(SqliteStorage::open(&format!("{dir}/{name}.db")).unwrap_or_else(|e| die(e)));
    let mut b = Zdlc::builder()
        .seed(seed)
        .chain(Arc::new(ZakuraRpc::new(rpc)))
        .storage(storage)
        .transport(Arc::new(DeskTransport::new(&arg(args, "--desk", ""), wallet.identity_key())))
        .block_secs(cfg.block_secs)
        .close_wait(cfg.close_wait.0, cfg.close_wait.1)
        .recovery_records(records);
    for o in arg(args, "--oracles", "").split(',').filter(|s| !s.is_empty()) {
        b = b.oracle(Arc::new(HttpOracle::new(o)));
    }
    let z = Arc::new(b.build().await.unwrap_or_else(|e| die(e)));
    tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
    z
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rpc = match arg(&args, "--node", "").as_str() {
        "" => std::env::var("ZDLC_RPC").unwrap_or_else(|_| die("--node URL or ZDLC_RPC")),
        u => u.to_string(),
    };
    let dir = arg(&args, "--dir", "room");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| die(e));
    let oracles: Vec<String> = arg(&args, "--oracles", "").split(',').filter(|s| !s.is_empty()).map(String::from).collect();
    if oracles.is_empty() {
        die("--oracles URL,URL");
    }
    let token_file = arg(&args, "--oracle-token-file", "");
    let oracle_token = std::fs::read_to_string(&token_file).unwrap_or_else(|e| die(format!("--oracle-token-file: {e}"))).trim().to_string();
    let block_secs: i64 = arg(&args, "--block-secs", "75").parse().unwrap_or_else(|_| die("--block-secs"));
    let amount_zat = zat(&arg(&args, "--amount", "0.01"));
    let cfg = Cfg {
        amount_zat,
        maturity: arg(&args, "--maturity", "2").parse().unwrap_or_else(|_| die("--maturity BLOCKS")),
        refund: arg(&args, "--refund", "4").parse().unwrap_or_else(|_| die("--refund BLOCKS")),
        close_wait: (
            arg(&args, "--close-wait", "1").parse().unwrap_or_else(|_| die("--close-wait BLOCKS")),
            arg(&args, "--close-wait-secs", "30").parse().unwrap_or_else(|_| die("--close-wait-secs SECS")),
        ),
        max_rooms: arg(&args, "--max-rooms", "20").parse().unwrap_or_else(|_| die("--max-rooms")),
        block_secs,
        explorer: arg(&args, "--explorer", ""),
        label: arg(&args, "--label", "testnet"),
    };
    let contractor = node(&args, &dir, "contractor", &rpc, &cfg, false).await;
    let client = node(&args, &dir, "client", &rpc, &cfg, true).await;
    // The client's node accepts nothing by itself: the visitor decides.
    println!("zdlc-room: contractor {} client {}", contractor.identity(), client.identity());
    println!("zdlc-room: fund the demo wallet (the client's treasury) at {}", client.treasury_address());
    let rooms_file = format!("{dir}/rooms.json");
    let rooms: HashMap<String, Room> = std::fs::read(&rooms_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let app = Arc::new(App { contractor, client, oracles, oracle_token, http: reqwest::Client::new(), rooms: Mutex::new(rooms), rooms_file, cfg });
    let r = Router::new()
        .route("/", get(page))
        .route("/r/{id}", get(page))
        .route("/api/info", get(info))
        .route("/api/rooms", post(open))
        .route("/api/rooms/{id}", get(show))
        .route("/api/rooms/{id}/accept", post(accept))
        .route("/api/rooms/{id}/qr.svg", get(qr))
        .route("/api/rooms/{id}/{who}/hold", post(hold))
        .route("/api/rooms/{id}/{who}/attempt", post(attempt))
        .route("/api/rooms/{id}/arbiters/{n}", post(rule))
        .with_state(app);
    let listen = arg(&args, "--listen", "127.0.0.1:8080");
    println!("zdlc-room: http://{listen}/");
    let l = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| die(e));
    axum::serve(l, r).await.unwrap_or_else(|e| die(e));
}
