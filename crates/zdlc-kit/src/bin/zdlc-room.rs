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
//! A room lasts about 5 minutes on testnet (75 s blocks): the refund unlocks 4 blocks
//! after the invoice. The contract settles as soon as both arbiters have ruled; a side
//! whose counterparty is offline settles alone `--close-wait` blocks after the rulings.
//! The maturity (2 blocks) only bounds how long the invoice stays valid.
//!
//! The page and its API are public and need no token: they can only run the demo
//! escrow, at `--amount`, between the two demo nodes, with at most `--max-rooms` open at
//! once. The oracles' admin token stays on the server; the page resolves only the events
//! of the rooms it opened.

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

const PAGE: &str = include_str!("../../room/index.html");
const DESK_PAGE: &str = include_str!("../../room/desk.html");

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
    desk: Arc<DeskDemo>,
}

/// The desk demo: a house maker agent always quoting on the desk, and a taker the
/// visitor drives. One trade runs at a time, so every visitor watches the same one.
struct DeskDemo {
    maker: Arc<Zdlc>,
    taker: Arc<Zdlc>,
    taker_key: secp256k1::SecretKey,
    desk: zdlc::desk::DeskClient,
    oracle_keys: Vec<String>,
    trade: std::sync::Mutex<Option<Trade>>,
    file: String,
    args: DeskArgs,
}

struct DeskArgs {
    collateral_zat: u64,
    premium_bps: u64,
    maturity: u32,
    refund: u32,
    minute_offset: i64,
}

/// The current (or last) trade, as the taker reports it. Saved to `trade.json`.
#[derive(Clone, Default, Serialize, Deserialize)]
struct Trade {
    started: u64,
    steps: Vec<Value>,
    contract: Option<String>,
    rfq: Option<String>,
    error: Option<String>,
    done: bool,
}

impl DeskDemo {
    fn save(&self, t: &Trade) {
        if let Ok(b) = serde_json::to_vec(t) {
            let _ = std::fs::write(&self.file, b);
        }
    }

    fn record(&self, step: Value) {
        let mut g = self.trade.lock().unwrap();
        if let Some(t) = g.as_mut() {
            if step["step"] == "rfq posted" {
                t.rfq = step["rfq"].as_str().map(String::from);
            }
            if step["step"] == "offered" {
                t.contract = step["contract"].as_str().map(String::from);
            }
            t.steps.push(json!({"t": now(), "step": step}));
            self.save(t);
        }
    }
}

/// The app once both demo nodes are up. The page is served from the start; until the
/// node answers, every API call says so (503) and startup keeps retrying.
struct Slot(tokio::sync::OnceCell<Arc<App>>);

type Shared = Arc<Slot>;

/// A handler's app, or 503 while the nodes are still connecting.
struct Ready(Arc<App>);

impl axum::extract::FromRequestParts<Shared> for Ready {
    type Rejection = Response;

    async fn from_request_parts(_: &mut axum::http::request::Parts, s: &Shared) -> Result<Self, Response> {
        s.0.get().cloned().map(Ready).ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "connecting to the Zcash node; try again in a moment"))
    }
}

/// Hides addresses from what the page sees: any http(s) URL and any dotted IPv4 address
/// become "[hidden]". Errors from the node, the desk or the oracles end up in contract
/// notes and refusals; the endpoints behind this server are private.
fn scrub(s: &str) -> String {
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
fn ipv4_len(s: &str) -> Option<usize> {
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

fn scrub_json(v: &mut Value) {
    match v {
        Value::String(s) => *s = scrub(s),
        Value::Array(a) => a.iter_mut().for_each(scrub_json),
        Value::Object(o) => o.values_mut().for_each(scrub_json),
        _ => {}
    }
}

/// A JSON response with every string scrubbed.
fn out(mut v: Value) -> Response {
    scrub_json(&mut v);
    Json(v).into_response()
}

fn err(code: StatusCode, m: impl ToString) -> Response {
    (code, Json(json!({"error": scrub(&m.to_string())}))).into_response()
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

async fn info(Ready(a): Ready) -> Response {
    let i = match a.contractor.info().await {
        Ok(i) => i,
        Err(e) => return bad(e),
    };
    let bal = a.client.treasury_zat().await.ok();
    Json(json!({
        "network": a.cfg.label, "tip": i["tip"], "amountZat": a.cfg.amount_zat, "maturityBlocks": a.cfg.maturity,
        "refundBlocks": a.cfg.refund, "closeWaitBlocks": a.cfg.close_wait.0, "closeWaitSecs": a.cfg.close_wait.1, "blockSecs": a.cfg.block_secs, "explorer": a.cfg.explorer, "arbiters": a.oracles.len(),
        "demoWalletZat": bal, "fundAddress": a.client.inbox_address(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct OpenReq {
    #[serde(default)]
    question: Option<String>,
}

/// The contractor invoices the client: a new escrow, a new room.
async fn open(Ready(a): Ready, Json(r): Json<OpenReq>) -> Response {
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

async fn show(Ready(a): Ready, Path(id): Path<String>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let tip = a.contractor.info().await.ok().map(|i| i["tip"].clone()).unwrap_or(Value::Null);
    let contractor = a.contractor.contract(&id).await.ok().flatten().map(|r| r.view());
    let client = a.client.contract(&id).await.ok().flatten().map(|r| r.view());
    let rooms = a.rooms.lock().await;
    let room = rooms.get(&id);
    out(json!({
        "id": id, "tip": tip, "contractor": contractor, "client": client,
        "arbiters": room.map(|r| r.arbiters.clone()).unwrap_or_default(),
        "log": room.map(|r| r.log.clone()).unwrap_or_default(),
    }))
}

#[derive(Deserialize)]
struct AcceptReq {
    funding: String,
}

async fn accept(Ready(a): Ready, Path(id): Path<String>, Json(r): Json<AcceptReq>) -> Response {
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
            a.log(&id, "client", if r.funding == "deposit" { "accepted; deposits from their own wallet (QR code)" } else { "accepted; deposited from the demo wallet" }).await;
            Json(json!({"id": id})).into_response()
        }
        Err(e) => bad(e),
    }
}

async fn qr(Ready(a): Ready, Path(id): Path<String>) -> Response {
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

async fn hold(Ready(a): Ready, Path((id, who)): Path<(String, String)>, Json(r): Json<HoldReq>) -> Response {
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

async fn attempt(Ready(a): Ready, Path((id, who)): Path<(String, String)>, Json(r): Json<AttemptReq>) -> Response {
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
            out(v)
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct RuleReq {
    outcome: String,
}

/// Arbiter `n` resolves the room's event.
async fn rule(Ready(a): Ready, Path((id, n)): Path<(String, usize)>, Json(r): Json<RuleReq>) -> Response {
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
        Err(e) => bad(format!("oracle: {}", e.without_url())),
    }
}

async fn desk_page() -> Html<&'static str> {
    Html(DESK_PAGE)
}

async fn desk_info(Ready(a): Ready) -> Response {
    let d = &a.desk;
    let tip = match d.taker.info().await {
        Ok(i) => i["tip"].clone(),
        Err(e) => return bad(e),
    };
    Json(json!({
        "network": a.cfg.label, "tip": tip, "blockSecs": a.cfg.block_secs, "explorer": a.cfg.explorer,
        "collateralZat": d.args.collateral_zat, "premiumBps": d.args.premium_bps, "refundBlocks": d.args.refund,
        "maker": d.maker.identity(), "taker": d.taker.identity(), "oracles": d.oracle_keys.len(),
        "makerZat": d.maker.treasury_zat().await.ok(), "takerZat": d.taker.treasury_zat().await.ok(),
        "makerFund": d.maker.inbox_address(), "takerFund": d.taker.inbox_address(),
    }))
    .into_response()
}

/// The trade, both sides' view of its contract, and the desk's quotes on its request.
async fn desk_state(Ready(a): Ready) -> Response {
    let d = &a.desk;
    let trade = d.trade.lock().unwrap().clone();
    let Some(t) = trade else { return out(json!({"trade": null})) };
    let (mut taker, mut maker) = (Value::Null, Value::Null);
    if let Some(id) = &t.contract {
        taker = d.taker.contract(id).await.ok().flatten().map(|r| r.view()).unwrap_or_default();
        maker = d.maker.contract(id).await.ok().flatten().map(|r| r.view()).unwrap_or_default();
    }
    let tip = d.taker.info().await.ok().map(|i| i["tip"].clone()).unwrap_or_default();
    out(json!({"trade": t, "taker": taker, "maker": maker, "tip": tip}))
}

/// The visitor's taker asks the desk for a hedge.
async fn desk_trade(Ready(a): Ready) -> Response {
    let d = a.desk.clone();
    {
        let mut g = d.trade.lock().unwrap();
        if g.as_ref().is_some_and(|t| !t.done) {
            return err(StatusCode::CONFLICT, "a trade is already running: watch it settle, then start the next");
        }
        let t = Trade { started: now(), ..Default::default() };
        d.save(&t);
        *g = Some(t);
    }
    tokio::spawn(async move {
        let args = zdlc_kit::agent::TakerArgs {
            collateral_zat: d.args.collateral_zat,
            maturity_blocks: d.args.maturity,
            refund_blocks: d.args.refund,
            minute_offset: d.args.minute_offset,
            quote_wait_s: 5,
        };
        let d2 = d.clone();
        let r = zdlc_kit::agent::run_taker_with(d.taker.clone(), &d.desk, &d.taker_key, d.oracle_keys.clone(), &args, &move |step| d2.record(step)).await;
        let mut g = d.trade.lock().unwrap();
        if let Some(t) = g.as_mut() {
            t.done = true;
            t.error = r.err().map(|e| e.to_string());
            d.save(t);
        }
    });
    out(json!({"started": true}))
}

/// Sweeps a node's shielded inbox into treasury coins every minute. Scanning starts where
/// the last empty pass ended, so it never rescans the chain.
fn spawn_sweeper(z: Arc<Zdlc>, path: String) {
    tokio::spawn(async move {
        loop {
            if let Ok(tip) = z.info().await.map(|i| i["tip"].as_u64().unwrap_or_default() as u32) {
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
async fn node(args: &[String], dir: &str, name: &str, rpc: &str, cfg: &Cfg, records: bool) -> zdlc_kit::Result<Arc<Zdlc>> {
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
    Ok(Arc::new(b.build().await?))
}

/// The desk demo's two agents, each a kit node with its own seed and funding inbox. The
/// maker quotes and accepts on its own, forever; the taker waits for the visitor.
async fn desk_demo(args: &[String], dir: &str, rpc: &str, cfg: &Cfg, oracles: &[String], bank: Arc<Zdlc>) -> Arc<DeskDemo> {
    let (maker, taker) = loop {
        match (node(args, dir, "maker", rpc, cfg, true).await, node(args, dir, "taker", rpc, cfg, true).await) {
            (Ok(m), Ok(t)) => break (m, t),
            (Err(e), _) | (_, Err(e)) => eprintln!("zdlc-room: desk agents waiting for the node: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    };
    for z in [&maker, &taker] {
        tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
    }
    let mut oracle_keys = vec![];
    for o in oracles {
        let k = loop {
            match zdlc::oracle_client::OracleClient::new(o).info().await {
                Ok(i) => break i["pubkey"].as_str().unwrap_or_default().to_string(),
                Err(e) => eprintln!("zdlc-room: oracle: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        };
        oracle_keys.push(k);
    }
    let desk_url = arg(args, "--desk", "");
    let num = |name: &str, default: &str| arg(args, name, default).parse::<i64>().unwrap_or_else(|_| die(name));
    let d = Arc::new(DeskDemo {
        taker_key: zdlc::wallet::Wallet::from_seed(secret(&format!("{dir}/taker.seed"))).identity_key(),
        desk: zdlc::desk::DeskClient::new(&desk_url),
        trade: std::sync::Mutex::new(std::fs::read(format!("{dir}/trade.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())),
        file: format!("{dir}/trade.json"),
        args: DeskArgs {
            collateral_zat: zat(&arg(args, "--desk-collateral", "0.001")),
            premium_bps: num("--desk-premium-bps", "50") as u64,
            maturity: num("--desk-maturity", "2") as u32,
            refund: num("--desk-refund", "8") as u32,
            minute_offset: num("--desk-minute-offset", "1"),
        },
        oracle_keys,
        maker,
        taker,
    });
    // A trade cut short by a restart is over: its contract still settles on its own.
    if let Some(t) = d.trade.lock().unwrap().as_mut() {
        if !t.done {
            t.done = true;
            t.error = Some("the server restarted during this trade".into());
            d.save(t);
        }
    }
    for (z, name) in [(&d.maker, "maker"), (&d.taker, "taker")] {
        spawn_sweeper(z.clone(), format!("{dir}/{name}-inbox.from"));
    }
    // The demo wallet (`bank`, the escrow client's treasury) keeps both agents funded: when
    // one runs low, it pays the agent's inbox, which the sweeper turns into coins.
    {
        let (d, low, top) = (d.clone(), 4 * d.args.collateral_zat + 1_000_000, 2_000_000u64);
        // When each agent was last topped up: a top-up takes a block to arrive and a
        // minute to be swept, so a restart must not send a second one meanwhile.
        let path = format!("{dir}/topups.json");
        tokio::spawn(async move {
            loop {
                let mut last: HashMap<String, u64> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
                for z in [&d.maker, &d.taker] {
                    let recent = last.get(&z.identity()).is_some_and(|t| now() < t + 1800);
                    if !recent && z.treasury_zat().await.is_ok_and(|v| v < low) {
                        match bank.pay_from_treasury(&z.inbox_address(), top).await {
                            Ok(txid) => {
                                println!("zdlc-room: topped up a desk agent from the demo wallet: {txid}");
                                last.insert(z.identity(), now());
                                let _ = std::fs::write(&path, serde_json::to_vec(&last).unwrap_or_default());
                            }
                            Err(e) => eprintln!("zdlc-room: desk top-up: {e}"),
                        }
                    }
                }
                // Long enough for a top-up to confirm and be swept before the next check.
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            }
        });
    }
    // The house maker quotes every request on the desk and accepts the matching offers.
    {
        let (z, desk, key, keys, bps) = (d.maker.clone(), zdlc::desk::DeskClient::new(&desk_url), zdlc::wallet::Wallet::from_seed(secret(&format!("{dir}/maker.seed"))).identity_key(), d.oracle_keys.clone(), d.args.premium_bps);
        tokio::spawn(async move {
            let a = zdlc_kit::agent::MakerArgs { premium_bps: bps, cap_zat: 10_000_000, ttl_blocks: 20, contracts: 0 };
            loop {
                if let Err(e) = zdlc_kit::agent::run_maker(z.clone(), &desk, &key, keys.clone(), &a).await {
                    eprintln!("zdlc-room: maker: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
    }
    d
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
    let listen = arg(&args, "--listen", "127.0.0.1:8080");
    let slot: Shared = Arc::new(Slot(tokio::sync::OnceCell::new()));
    {
        let slot = slot.clone();
        tokio::spawn(async move {
            // Both nodes need the chain to start; retry until it answers.
            let (contractor, client) = loop {
                match (node(&args, &dir, "contractor", &rpc, &cfg, false).await, node(&args, &dir, "client", &rpc, &cfg, true).await) {
                    (Ok(k), Ok(c)) => break (k, c),
                    (Err(e), _) | (_, Err(e)) => eprintln!("zdlc-room: waiting for the node: {e}"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            };
            for z in [&contractor, &client] {
                tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
            }
            // The client's node accepts nothing by itself: the visitor decides.
            println!("zdlc-room: contractor {} client {}", contractor.identity(), client.identity());
            println!("zdlc-room: fund the demo wallet at {} (shielded; swept into coins every minute)", client.inbox_address());
            spawn_sweeper(client.clone(), format!("{dir}/inbox.from"));
            let desk = desk_demo(&args, &dir, &rpc, &cfg, &oracles, client.clone()).await;
            let rooms_file = format!("{dir}/rooms.json");
            let rooms: HashMap<String, Room> = std::fs::read(&rooms_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            let app = Arc::new(App { contractor, client, oracles, oracle_token, http: reqwest::Client::new(), rooms: Mutex::new(rooms), rooms_file, cfg, desk });
            let _ = slot.0.set(app);
        });
    }
    let r = Router::new()
        // Everything under /escrow and /hedge, so a site can serve its own pages beside
        // them. The old /room and /desk addresses redirect, so shared links keep working.
        .route("/", get(|| async { axum::response::Redirect::temporary("/escrow") }))
        .route("/room", get(|| async { axum::response::Redirect::permanent("/escrow") }))
        .route("/room/r/{id}", get(|Path(id): Path<String>| async move { axum::response::Redirect::permanent(&format!("/escrow/r/{id}")) }))
        .route("/desk", get(|| async { axum::response::Redirect::permanent("/hedge") }))
        .route("/escrow", get(page))
        .route("/escrow/r/{id}", get(page))
        .route("/escrow/api/info", get(info))
        .route("/escrow/api/rooms", post(open))
        .route("/escrow/api/rooms/{id}", get(show))
        .route("/escrow/api/rooms/{id}/accept", post(accept))
        .route("/escrow/api/rooms/{id}/qr.svg", get(qr))
        .route("/escrow/api/rooms/{id}/{who}/hold", post(hold))
        .route("/escrow/api/rooms/{id}/{who}/attempt", post(attempt))
        .route("/escrow/api/rooms/{id}/arbiters/{n}", post(rule))
        .route("/hedge", get(desk_page))
        .route("/hedge/api/info", get(desk_info))
        .route("/hedge/api/state", get(desk_state))
        .route("/hedge/api/trade", post(desk_trade))
        .with_state(slot);
    println!("zdlc-room: http://{listen}/escrow and http://{listen}/hedge");
    let l = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| die(e));
    axum::serve(l, r).await.unwrap_or_else(|e| die(e));
}

#[cfg(test)]
mod tests {
    use super::scrub;

    #[test]
    fn scrub_hides_endpoints_and_keeps_the_rest() {
        assert_eq!(scrub("getblockcount: error sending request for url (http://10.0.0.2:18232/)"), "getblockcount: error sending request for url ([hidden])");
        assert_eq!(scrub("oracle 192.168.1.20:7801 refused"), "oracle [hidden] refused");
        assert_eq!(scrub("locked until after block height 1306"), "locked until after block height 1306");
        assert_eq!(scrub("0.01 ZEC, version 1.2.3"), "0.01 ZEC, version 1.2.3");
        assert_eq!(scrub("zcash:utest1abc?amount=0.0102&memo=ekRM"), "zcash:utest1abc?amount=0.0102&memo=ekRM");
    }
}
