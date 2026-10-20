//! The zdlc oracle service: one oracle of a 2-of-2 multi-oracle set, built to run inside
//! a TEE enclave (`zdlc::enclave`). Run two, with separate state and sealing keys.
//!
//! ```sh
//! zdlc-oracled --listen 127.0.0.1:7801 --state a.sealed --sealing-key a.key [--node URL]
//! ```
//!
//! - `GET  /pubkey`                         the oracle key, sealer and nonce counter
//! - `POST /events {event_id, spec}`        announce (idempotent): the nonces to build on
//! - `GET  /events/{id}`                    the announcement
//! - `POST /events/{id}/attest`             200 attestation, 202 not yet due, 400 error
//!
//! - `POST /events/{id}/resolve {outcome, token}`  an arbiter's decision (needs `--admin-token FILE`)
//!
//! `--node` (a Zakura JSON-RPC URL) is needed only for shielded-payment events.

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;
use zdlc::{
    enclave::{DevSealer, Enclave, EventSpec},
    node::Node,
};

struct App {
    enclave: Mutex<Enclave>,
    node: Option<Node>,
    admin_token: Option<String>,
}

type Reply = (StatusCode, Json<Value>);

fn bad(e: impl std::fmt::Display) -> Reply {
    (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()})))
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

#[derive(Deserialize)]
struct AnnounceReq {
    event_id: String,
    spec: EventSpec,
}

async fn pubkey(State(app): State<Arc<App>>) -> Reply {
    let e = app.enclave.lock().await;
    (StatusCode::OK, Json(json!({
        "pubkey": hex::encode(e.pubkey().serialize()),
        "sealer": e.sealer(),
        "next_nonce": e.next_nonce(),
    })))
}

async fn announce(State(app): State<Arc<App>>, Json(req): Json<AnnounceReq>) -> Reply {
    match app.enclave.lock().await.announce(&req.event_id, req.spec) {
        Ok(a) => (StatusCode::OK, Json(serde_json::to_value(a).unwrap())),
        Err(e) => bad(e),
    }
}

async fn announcement(State(app): State<Arc<App>>, Path(id): Path<String>) -> Reply {
    match app.enclave.lock().await.announcement(&id) {
        Some(a) => (StatusCode::OK, Json(serde_json::to_value(a).unwrap())),
        None => (StatusCode::NOT_FOUND, Json(json!({"error": "unknown event"}))),
    }
}

async fn attest(State(app): State<Arc<App>>, Path(id): Path<String>) -> Reply {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    match app.enclave.lock().await.attest(&id, now, app.node.as_ref()).await {
        Ok(Some(a)) => (StatusCode::OK, Json(serde_json::to_value(a).unwrap())),
        Ok(None) => (StatusCode::ACCEPTED, Json(json!({"status": "not yet due"}))),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct ResolveReq {
    outcome: String,
    token: String,
}

async fn resolve(State(app): State<Arc<App>>, Path(id): Path<String>, Json(req): Json<ResolveReq>) -> Reply {
    match &app.admin_token {
        Some(t) if *t == req.token => {}
        _ => return (StatusCode::FORBIDDEN, Json(json!({"error": "bad admin token"}))),
    }
    match app.enclave.lock().await.resolve(&id, &req.outcome) {
        Ok(()) => (StatusCode::OK, Json(json!({"resolved": req.outcome}))),
        Err(e) => bad(e),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let listen = arg(&args, "--listen").unwrap_or("127.0.0.1:7801".into());
    let state = arg(&args, "--state").unwrap_or("oracle.sealed".into());
    let key = arg(&args, "--sealing-key").unwrap_or("oracle.key".into());
    let sealer = DevSealer::open(&key).unwrap_or_else(|e| {
        eprintln!("zdlc-oracled: {e}");
        std::process::exit(1)
    });
    let enclave = Enclave::open(&state, Box::new(sealer)).unwrap_or_else(|e| {
        eprintln!("zdlc-oracled: {e}");
        std::process::exit(1)
    });
    println!(
        "zdlc-oracled {} pubkey {} sealer {} next nonce {}",
        listen,
        hex::encode(enclave.pubkey().serialize()),
        enclave.sealer(),
        enclave.next_nonce()
    );
    let app = Arc::new(App {
        enclave: Mutex::new(enclave),
        node: arg(&args, "--node").map(Node::new),
        admin_token: arg(&args, "--admin-token")
            .map(|p| std::fs::read_to_string(&p).unwrap_or_else(|e| {
                eprintln!("zdlc-oracled: admin token {p}: {e}");
                std::process::exit(1)
            }).trim().to_string()),
    });
    let router = Router::new()
        .route("/pubkey", get(pubkey))
        .route("/events", post(announce))
        .route("/events/{id}", get(announcement))
        .route("/events/{id}/attest", post(attest))
        .route("/events/{id}/resolve", post(resolve))
        .with_state(app);
    let l = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| {
        eprintln!("zdlc-oracled: {listen}: {e}");
        std::process::exit(1)
    });
    axum::serve(l, router).await.unwrap();
}
