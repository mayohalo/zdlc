//! What the three demos share: the app state, the ready check, the scrub that keeps
//! private endpoints out of responses, the node builder, inbox sweeps and block timing.

#![allow(unused_imports)]

use std::{collections::HashMap, io::Write, os::unix::fs::OpenOptionsExt, sync::Arc};

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use zdlc_kit::{impls::*, templates::Template, Funding, Zdlc};

use crate::{auction::*, escrow::*, hedge::*};

pub(crate) fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned().unwrap_or_else(|| default.to_string())
}

pub(crate) fn die(m: impl std::fmt::Display) -> ! {
    eprintln!("zdlc-room: {m}");
    std::process::exit(1)
}

/// Reads 32 bytes of hex from `path`, creating it (mode 600) if missing.
pub(crate) fn secret(path: &str) -> [u8; 32] {
    if let Ok(h) = std::fs::read_to_string(path) {
        return hex::decode(h.trim()).ok().and_then(|b| b.try_into().ok()).unwrap_or_else(|| die(format!("{path}: 32 bytes of hex")));
    }
    let mut s = [0u8; 32];
    rand10::fill(&mut s);
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path).unwrap_or_else(|e| die(e));
    writeln!(f, "{}", hex::encode(s)).unwrap_or_else(|e| die(e));
    s
}

pub(crate) fn zat(s: &str) -> u64 {
    let (w, f) = s.split_once('.').unwrap_or((s, ""));
    if f.len() > 8 {
        die(format!("{s}: at most 8 decimals"));
    }
    w.parse::<u64>().ok().zip(format!("{f:0<8}").parse::<u64>().ok()).map(|(w, f)| w * 100_000_000 + f).unwrap_or_else(|| die(format!("{s}: not an amount")))
}

/// The node's reason for refusing a transaction, without the RPC wrapping.
pub(crate) fn reason(e: &str) -> String {
    let r = e.rsplit("consensus validation: ").next().unwrap_or(e);
    r.trim_end_matches(['"', '}', '\\']).to_string()
}

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default()
}

pub(crate) struct Cfg {
    pub(crate) amount_zat: u64,
    pub(crate) maturity: u32,
    pub(crate) refund: u32,
    pub(crate) close_wait: (u32, i64),
    pub(crate) max_rooms: usize,
    pub(crate) block_secs: i64,
    pub(crate) explorer: String,
    pub(crate) label: String,
}

pub(crate) struct App {
    pub(crate) contractor: Arc<Zdlc>,
    pub(crate) client: Arc<Zdlc>,
    pub(crate) oracles: Vec<String>,
    pub(crate) oracle_token: String,
    pub(crate) http: reqwest::Client,
    pub(crate) rooms: Mutex<HashMap<String, Room>>,
    pub(crate) rooms_file: String,
    pub(crate) cfg: Cfg,
    pub(crate) desk: Arc<DeskDemo>,
    pub(crate) auction: Arc<AuctionDemo>,
    pub(crate) node: zdlc::node::Node,
    /// The measured block interval in seconds, and when it was measured.
    pub(crate) block_time: std::sync::Mutex<(u64, f64)>,
}

/// The app once both demo nodes are up. The page is served from the start; until the
/// node answers, every API call says so (503) and startup keeps retrying.
pub(crate) struct Slot(pub(crate) tokio::sync::OnceCell<Arc<App>>);

pub(crate) type Shared = Arc<Slot>;

/// A handler's app, or 503 while the nodes are still connecting.
pub(crate) struct Ready(pub(crate) Arc<App>);

impl axum::extract::FromRequestParts<Shared> for Ready {
    type Rejection = Response;

    async fn from_request_parts(_: &mut axum::http::request::Parts, s: &Shared) -> Result<Self, Response> {
        s.0.get().cloned().map(Ready).ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "connecting to the Zcash node; try again in a moment"))
    }
}

/// Hides addresses from what the page sees: any http(s) URL and any dotted IPv4 address
/// become "[hidden]". Errors from the node, the desk or the oracles end up in contract
/// notes and refusals; the endpoints behind this server are private.
pub(crate) fn scrub(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        let lower = rest.to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            let end = rest.find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | ',' | ']' | '>')).unwrap_or(rest.len());
            out.push_str("[hidden]");
            rest = &rest[end..];
            continue;
        }
        if let Some(n) = ipv4_len(rest) {
            if !out.ends_with(|c: char| c.is_ascii_alphanumeric() || c == '.') {
                out.push_str("[hidden]");
                rest = &rest[n..];
                continue;
            }
        }
        let c = rest.chars().next().expect("not empty");
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

/// The length of a dotted IPv4 address (with an optional :port) at the start of `s`.
pub(crate) fn ipv4_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0;
    for part in 0..4 {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() && i - start < 3 {
            i += 1;
        }
        if i == start || (i < b.len() && b[i].is_ascii_digit()) {
            return None;
        }
        if part < 3 {
            if b.get(i) != Some(&b'.') {
                return None;
            }
            i += 1;
        }
    }
    if b.get(i) == Some(&b':') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    (b.get(i).is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'.')).then_some(i)
}

pub(crate) fn scrub_json(v: &mut Value) {
    match v {
        Value::String(s) => *s = scrub(s),
        Value::Array(a) => a.iter_mut().for_each(scrub_json),
        Value::Object(o) => o.values_mut().for_each(scrub_json),
        _ => {}
    }
}

/// A JSON response with every string scrubbed.
pub(crate) fn out(mut v: Value) -> Response {
    scrub_json(&mut v);
    Json(v).into_response()
}

pub(crate) fn err(code: StatusCode, m: impl ToString) -> Response {
    (code, Json(json!({"error": scrub(&m.to_string())}))).into_response()
}

pub(crate) fn bad(e: impl ToString) -> Response {
    err(StatusCode::BAD_REQUEST, e)
}

impl App {
    /// The recent average block interval, from the last 30 blocks' timestamps (cached
    /// five minutes). Testnet can run far from its 75 s target, and windows sized in
    /// blocks would then be far shorter or longer than meant.
    pub(crate) async fn block_secs(&self) -> f64 {
        let (at, secs) = *self.block_time.lock().unwrap();
        if now() < at + 300 && secs > 0.0 {
            return secs;
        }
        let measured = async {
            let tip = self.node.height().await.ok()?;
            let time = |h: u32| async move {
                let hash = self.node.call("getblockhash", json!([h])).await.ok()?;
                self.node.call("getblock", json!([hash, 1])).await.ok()?["time"].as_u64()
            };
            let (a, b) = (time(tip - 30).await?, time(tip).await?);
            Some(((b.saturating_sub(a)) as f64 / 30.0).clamp(10.0, 300.0))
        }
        .await;
        let secs = measured.unwrap_or(self.cfg.block_secs as f64);
        *self.block_time.lock().unwrap() = (now(), secs);
        secs
    }

    /// `secs` of wall time as blocks at the measured rate, at least `min`.
    pub(crate) async fn blocks(&self, secs: i64, min: u32) -> u32 {
        ((secs as f64 / self.block_secs().await).ceil() as u32).max(min)
    }
}

/// Sweeps a node's shielded inbox into treasury coins every minute. Scanning starts where
/// the last empty pass ended, so it never rescans the chain.
pub(crate) fn spawn_sweeper(z: Arc<Zdlc>, path: String) {
    tokio::spawn(async move {
        loop {
            if let Ok(tip) = z.tip().await {
                let from = std::fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(tip.saturating_sub(100));
                match z.sweep_inbox(from, 10).await {
                    Ok(Some(txid)) => println!("zdlc-room: swept {} into treasury coins: {txid}", path),
                    // Nothing unspent before this tip (less a margin for reorgs).
                    Ok(None) => {
                        let _ = std::fs::write(&path, tip.saturating_sub(5).max(from).to_string());
                    }
                    Err(e) => eprintln!("zdlc-room: sweep {path}: {e}"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

/// One demo party. `records`: post recovery records (the contractor puts nothing in and
/// has no treasury to pay for one).
pub(crate) async fn node(args: &[String], dir: &str, name: &str, rpc: &str, cfg: &Cfg, records: bool, early_close: bool) -> zdlc_kit::Result<Arc<Zdlc>> {
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
        .recovery_records(records)
        .close_before_funded(early_close);
    for o in arg(args, "--oracles", "").split(',').filter(|s| !s.is_empty()) {
        b = b.oracle(Arc::new(HttpOracle::new(o)));
    }
    Ok(Arc::new(b.build().await?))
}

#[cfg(test)]
mod tests {
    use super::scrub;

    #[test]
    pub(crate) fn scrub_hides_endpoints_and_keeps_the_rest() {
        assert_eq!(scrub("getblockcount: error sending request for url (http://10.0.0.2:18232/)"), "getblockcount: error sending request for url ([hidden])");
        assert_eq!(scrub("oracle 192.168.1.20:7801 refused"), "oracle [hidden] refused");
        assert_eq!(scrub("locked until after block height 1306"), "locked until after block height 1306");
        assert_eq!(scrub("0.01 ZEC, version 1.2.3"), "0.01 ZEC, version 1.2.3");
        assert_eq!(scrub("zcash:utest1abc?amount=0.0102&memo=ekRM"), "zcash:utest1abc?amount=0.0102&memo=ekRM");
    }
}
