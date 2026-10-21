//! K2 gate on Zakura regtest: two `zdlc-node`s, driven only through `zdlc-cli`, settle
//! an escrow funded from a deposit link (cooperative close) and a bet funded from
//! treasuries (the loser's node goes down; the winner settles by CET; the loser's node
//! restarts and finds the contract settled).
//!
//! ```sh
//! infra/zakura/scripts/up.sh
//! cargo build -p zdlc --release --bins
//! cargo test -p zdlc-kit --release --test kit -- --ignored --nocapture
//! ```

use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use zdlc::{
    funds, keys, notes, oracle_client::OracleClient, paylink, params::NU7_AT, regtest::Regtest, tx, wallet::Wallet,
};
use zcash_protocol as _;

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin(name: &str) -> PathBuf {
    // target/release/deps/kit-… → target/release/<name>
    std::env::current_exe().unwrap().parent().unwrap().parent().unwrap().join(name)
}

fn node_url() -> String {
    std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into())
}

async fn wait_http(url: &str) {
    for _ in 0..200 {
        if reqwest::get(url).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("{url} did not start");
}

struct Node {
    url: String,
    token: String,
    seed: String,
    args: Vec<String>,
    proc: Option<Proc>,
}

impl Node {
    fn start(&mut self) {
        self.proc = Some(Proc(Command::new(bin("zdlc-node")).args(&self.args).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().unwrap()));
    }

    fn stop(&mut self) {
        self.proc = None;
    }

    /// Runs zdlc-cli against this node and parses its JSON output.
    fn cli(&self, args: &[&str]) -> Value {
        self.try_cli(args).unwrap_or_else(|e| panic!("zdlc-cli {args:?}: {e}"))
    }

    fn try_cli(&self, args: &[&str]) -> Result<Value, String> {
        let out = Command::new(bin("zdlc-cli")).args(["--url", &self.url, "--token-file", &self.token]).args(args).output().unwrap();
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        Ok(serde_json::from_slice(&out.stdout).unwrap_or_else(|_| json!(String::from_utf8_lossy(&out.stdout))))
    }

    /// Polls `show id` until `state`.
    async fn until(&self, id: &str, state: &str, secs: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let c = self.try_cli(&["show", id]).unwrap_or(json!({"state": "unknown", "note": "not here yet"}));
            if c["state"] == state {
                return c;
            }
            assert!(c["state"] != "failed", "{id} failed: {}", c["note"]);
            assert!(Instant::now() < deadline, "{id} stuck at {} (wanted {state}): {}", c["state"], c["note"]);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

struct Stack {
    rt: Regtest,
    chain: String,
    a: Node,
    b: Node,
    arbiters: [OracleClient; 2],
    oracles: String,
    outside: Wallet,
    h0: u32,
    _procs: Vec<Proc>,
    miner: tokio::task::JoinHandle<()>,
}

/// The desk, two arbiter oracles, Alice's and Bob's nodes (treasuries funded), an outside
/// wallet holding shielded funds, and a block a second. Ports from `base`.
async fn stack(base: u16) -> Stack {
    let chain = node_url();
    let rt = Regtest::connect(&chain).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let dir = std::env::temp_dir().join(format!("zdlc-kit-{}-{base}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |f: &str| dir.join(f).to_str().unwrap().to_string();
    std::fs::write(path("admin"), "let-me-resolve").unwrap();
    let port = |n: u16| (base + n).to_string();
    let desk = format!("http://127.0.0.1:{}", port(0));
    let mut procs = vec![Proc(Command::new(bin("zdlc-desk")).args(["--listen", &format!("127.0.0.1:{}", port(0)), "--state", &path("desk.json"), "--node", &chain]).spawn().unwrap())];
    for (n, name) in [(1, "a"), (2, "b")] {
        procs.push(Proc(Command::new(bin("zdlc-oracled"))
            .args(["--listen", &format!("127.0.0.1:{}", port(n)), "--state", &path(&format!("{name}.sealed")), "--sealing-key", &path(&format!("{name}.key")), "--admin-token", &path("admin"), "--node", &chain])
            .spawn()
            .unwrap()));
    }
    let (o1, o2) = (format!("http://127.0.0.1:{}", port(1)), format!("http://127.0.0.1:{}", port(2)));
    for u in [format!("{desk}/health"), format!("{o1}/pubkey"), format!("{o2}/pubkey")] {
        wait_http(&u).await;
    }
    let oracles = format!("{o1},{o2}");
    let mk = |name: &str, port: String, seed: [u8; 32]| {
        std::fs::write(path(&format!("{name}.seed")), hex::encode(seed)).unwrap();
        Node {
            url: format!("http://127.0.0.1:{port}"),
            token: path(&format!("{name}.token")),
            seed: path(&format!("{name}.seed")),
            args: ["--seed", &path(&format!("{name}.seed")), "--token-file", &path(&format!("{name}.token")), "--db", &path(&format!("{name}.db")),
                "--node", &chain, "--desk", &desk, "--oracles", &oracles, "--listen", &format!("127.0.0.1:{port}"), "--block-secs", "1"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            proc: None,
        }
    };
    let seed = |b: u8| [b ^ (base as u8); 32];
    let (sa, sb, so) = (seed(0xA1), seed(0xB2), seed(0x0C));
    let (mut a, mut b) = (mk("alice", port(3), sa), mk("bob", port(4), sb));
    let (wa, wb, outside) = (Wallet::from_seed(sa), Wallet::from_seed(sb), Wallet::from_seed(so));
    let txid = rt.pay(&[(wa.coin_address(0), 1_0000_0000), (wb.coin_address(0), 1_0000_0000), (outside.coin_address(0), 2_0000_0000)]).await.unwrap();
    let tip = rt.tip().await.unwrap();
    let coin = tx::Coin { txid, index: 2, value: 2_0000_0000, spend: tx::Spend::P2pkh(keys::pubkey(&outside.coin_key(0))) };
    let sh = funds::shield(&rt.p, tip, &[(coin, outside.coin_key(0))], outside.deposit_address(0), rt.node.ironwood_anchor(tip).await.unwrap()).unwrap();
    let h0 = rt.send_confirm(&sh).await.unwrap();
    a.start();
    b.start();
    wait_http(&format!("{}/v1/openapi.json", a.url)).await;
    wait_http(&format!("{}/v1/openapi.json", b.url)).await;
    let miner = {
        let rt = Regtest::connect(&chain).await.unwrap();
        tokio::spawn(async move {
            loop {
                let _ = rt.mine(1).await;
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        })
    };
    let arbiters = [OracleClient::new(&o1), OracleClient::new(&o2)];
    Stack { rt, chain, a, b, arbiters, oracles, outside, h0, _procs: procs, miner }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura"]
async fn two_nodes_settle_through_the_cli() {
    let Stack { rt, chain: _, a, mut b, arbiters, oracles: _, outside, h0, _procs, miner } = stack(17930).await;
    let (_ida, idb) = (a.cli(&["info"])["identity"].as_str().unwrap().to_string(), b.cli(&["info"])["identity"].as_str().unwrap().to_string());
    let mut log = vec![];

    // 1. Escrow: Alice (contractor) invoices Bob for 1.5 ZEC; Bob funds it from a deposit
    //    link an outside wallet pays; the arbiters resolve "delivered"; cooperative close.
    let id = a.cli(&["offer", "escrow", "--to", &idb, "--amount", "1.5", "--question", "Milestone 1 delivered?", "--maturity", "15", "--refund", "300"])["id"]
        .as_str()
        .unwrap()
        .to_string();
    b.until(&id, "incoming", 30).await;
    b.cli(&["accept", &id, "--funding", "deposit"]);
    let c = b.until(&id, "awaiting_deposit", 30).await;
    let uri = c["deposit"]["uri"].as_str().unwrap().to_string();
    let (ua, zat, memo) = paylink::parse(&uri).unwrap();
    let mine = notes::unspent(&rt.node, &outside.deposit_fvk(0), h0, h0).await.unwrap();
    let pay = funds::send_shielded(&rt.node, &rt.p, &outside.deposit_sk(0), &mine, paylink::decode_unified(&ua).unwrap().1, Some(zat), outside.deposit_address(0), memo)
        .await
        .unwrap();
    rt.node.send(&pay.raw).await.unwrap();
    println!("[kit] escrow {id}: the outside wallet paid Bob's deposit link ({zat} zat)");
    let funded = a.until(&id, "funded", 120).await;
    b.until(&id, "funded", 60).await;
    for o in &arbiters {
        o.resolve(&format!("zdlc-{id}"), "delivered", "let-me-resolve").await.unwrap();
    }
    let sa_ = a.until(&id, "settled", 180).await;
    let sb_ = b.until(&id, "settled", 60).await;
    println!("[kit] escrow settled: Alice {} / Bob {}", sa_["settlement"], sb_["settlement"]);
    assert_eq!(sa_["settlement"]["how"], "close");
    log.push(json!({"run": "escrow by deposit, closed", "id": id, "deposit_uri": uri, "deposit_zat": zat, "funding_txid": funded["fundingTxid"],
        "alice": sa_["settlement"], "bob": sb_["settlement"]}));

    // 2. Bet from treasuries: Bob's node goes down after funding; Alice wins and settles by
    //    CET; Bob's node restarts and finds it settled.
    let id = a.cli(&["offer", "bet", "--to", &idb, "--question", "Rain on Friday?", "--outcomes", "yes,no", "--offerer-wins", "yes", "--stake", "0.2", "--maturity", "12", "--refund", "300"])["id"]
        .as_str()
        .unwrap()
        .to_string();
    b.until(&id, "incoming", 30).await;
    b.cli(&["accept", &id, "--funding", "treasury"]);
    a.until(&id, "funded", 120).await;
    b.until(&id, "funded", 60).await;
    b.stop();
    for o in &arbiters {
        o.resolve(&format!("zdlc-{id}"), "yes", "let-me-resolve").await.unwrap();
    }
    let won = a.until(&id, "settled", 240).await;
    println!("[kit] bet settled by Alice alone: {}", won["settlement"]);
    assert_eq!(won["settlement"]["how"], "cet");
    b.start();
    wait_http(&format!("{}/v1/openapi.json", b.url)).await;
    let lost = b.until(&id, "settled", 120).await;
    println!("[kit] Bob's restarted node: {}", lost["settlement"]);
    // Alice's CET payout moves into her shielded balance on its own.
    let deadline = Instant::now() + Duration::from_secs(60);
    let shielded = loop {
        let c = a.cli(&["show", &id]);
        if c["shieldedPayout"].is_string() {
            break c["shieldedPayout"].clone();
        }
        assert!(Instant::now() < deadline, "payout not shielded: {}", c["note"]);
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    tokio::time::sleep(Duration::from_secs(3)).await;
    let ev = a.cli(&["events"]);
    let kinds: Vec<String> = ev.as_str().unwrap_or_default().lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|e| e["kind"].as_str().unwrap_or_default().to_string()).collect();
    println!("[kit] Alice's events: {kinds:?}");
    let bal = a.cli(&["balance"]);
    println!("[kit] Alice's balance: {bal}");
    assert!(bal["shieldedZat"].as_u64().unwrap() > 1_5000_0000 + 3000_0000, "close and shielded CET payout");
    assert_eq!(lost["settlement"]["how"], "cet", "Bob's node identifies Alice's CET");
    log.push(json!({"run": "bet from treasuries, CET while the loser was down", "id": id, "alice": won["settlement"], "bob_after_restart": lost["settlement"],
        "alice_events": kinds, "alice_balance": bal, "payout_shield_txid": shielded}));
    miner.abort();
    let _ = &a.seed;
    if let Ok(p) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(p, serde_json::to_string_pretty(&log).unwrap()).unwrap();
    }
}

/// `@zdlc/client` drives two nodes: Alice offers a bet, Bob's event loop accepts it,
/// the arbiters resolve, both settle. Needs `npm run build` in sdk/ts first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura and node"]
async fn ts_client_drives_a_bet() {
    let st = stack(17960).await;
    let ts = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../sdk/ts");
    let token = |n: &Node| std::fs::read_to_string(&n.token).unwrap().trim().to_string();
    let out = tokio::process::Command::new("node")
        .arg("scripts/live.mjs")
        .current_dir(&ts)
        .env("ZDLC_A_URL", &st.a.url)
        .env("ZDLC_A_TOKEN", token(&st.a))
        .env("ZDLC_B_URL", &st.b.url)
        .env("ZDLC_B_TOKEN", token(&st.b))
        .env("ZDLC_ORACLES", &st.oracles)
        .env("ZDLC_ADMIN", "let-me-resolve")
        .output()
        .await
        .unwrap();
    st.miner.abort();
    let stdout = String::from_utf8_lossy(&out.stdout);
    println!("{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "live.mjs failed");
    if let Ok(p) = std::env::var("ZDLC_RESULTS_TS") {
        std::fs::write(p, stdout.lines().last().unwrap_or_default()).unwrap();
    }
    let _ = (&st.rt, &st.chain, &st.arbiters, &st.outside, st.h0);
}

/// The stable-payment template through the CLI: Alice is owed $50 in ZEC at a closed
/// ZEC/USD minute; Bob hedges from his treasury; both oracles attest the four-exchange
/// median; cooperative close. Needs internet access for the price feed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura and internet access"]
async fn stable_payment_through_the_cli() {
    let st = stack(17990).await;
    let idb = st.b.cli(&["info"])["identity"].as_str().unwrap().to_string();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let minute = ((now / 60 - 3) * 60).to_string();
    let id = st.a.cli(&["offer", "stable-payment", "--to", &idb, "--usd", "50.00", "--collateral", "0.1", "--premium", "0.0005", "--maturity", "8", "--refund", "300", "--minute", &minute])["id"]
        .as_str()
        .unwrap()
        .to_string();
    st.b.until(&id, "incoming", 30).await;
    println!("[kit] Bob reviews: {}", st.b.cli(&["show", &id])["summary"]);
    st.b.cli(&["accept", &id, "--funding", "treasury"]);
    let a = st.a.until(&id, "settled", 240).await;
    let b = st.b.until(&id, "settled", 60).await;
    st.miner.abort();
    println!("[kit] stable payment: attested ${}; Alice {} / Bob {}", a["attested"], a["settlement"], b["settlement"]);
    println!("[kit] notes: Alice {} / Bob {}", a["note"], b["note"]);
    assert_eq!(a["settlement"]["how"], "close");
    if let Ok(p) = std::env::var("ZDLC_RESULTS_SV") {
        std::fs::write(p, serde_json::to_string_pretty(&json!({"id": id, "price_usd": a["attested"], "alice": a, "bob": b["settlement"]})).unwrap()).unwrap();
    }
}

fn id_of(v: Value) -> String {
    v["id"].as_str().unwrap().to_string()
}

/// Payroll over two periods (the second offered by the employee's node and accepted by
/// the employer's on their own), and a loan's collateral released on proof of
/// repayment. Needs internet access for ZEC/USD.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura and internet access"]
async fn payroll_and_pay_on_payment() {
    let st = stack(18010).await;
    let idb = st.b.cli(&["info"])["identity"].as_str().unwrap().to_string();
    let mut log = vec![];

    // Payroll: Alice (employee) is owed $20 a period for two periods; Bob (employer).
    let first = id_of(st.a.cli(&["offer", "payroll", "--to", &idb, "--usd", "20.00", "--collateral", "0.05", "--periods", "2", "--period-blocks", "6", "--refund-margin", "240"]));
    st.b.until(&first, "incoming", 30).await;
    st.b.cli(&["accept", &first, "--funding", "treasury"]);
    let p1 = st.a.until(&first, "settled", 240).await;
    println!("[kit] payroll period 1 settled: ${} {}", p1["attested"], p1["settlement"]);
    // Period 2 arrives and is accepted with no one asking.
    let deadline = Instant::now() + Duration::from_secs(60);
    let second = loop {
        let list = st.a.cli(&["contracts"]);
        if let Some(c) = list.as_array().unwrap().iter().find(|c| c["template"]["template"] == "payroll" && c["template"]["period"] == 2) {
            break c["id"].as_str().unwrap().to_string();
        }
        assert!(Instant::now() < deadline, "no second period");
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let p2 = st.a.until(&second, "settled", 240).await;
    let b2 = st.b.until(&second, "settled", 60).await;
    println!("[kit] payroll period 2 ({second}) settled: ${} {}; employer side {}", p2["attested"], p2["settlement"], b2["settlement"]);
    assert_eq!(p2["template"]["series_id"], json!(first));
    log.push(json!({"run": "payroll, 2 periods", "period1": {"id": first, "price": p1["attested"], "settlement": p1["settlement"]},
        "period2": {"id": second, "price": p2["attested"], "settlement": p2["settlement"], "auto_accepted": true}}));

    // A loan: Bob (borrower) locks 0.5 ZEC; he repays 0.3 ZEC to Alice's repay_to from
    // any wallet; the oracles see it by viewing key; the collateral comes back.
    let loan = id_of(st.a.cli(&["offer", "pay-on-payment", "--to", &idb, "--amount", "0.3", "--collateral", "0.5", "--within", "40", "--refund", "400"]));
    let inc = st.b.until(&loan, "incoming", 30).await;
    println!("[kit] Bob reviews the loan: {}", inc["summary"]);
    st.b.cli(&["accept", &loan, "--funding", "treasury"]);
    st.b.until(&loan, "funded", 120).await;
    let repay_to = inc["summary"]["repay_to"].as_str().unwrap();
    let uri = paylink::uri(paylink::To::Ironwood(&paylink::decode_unified(repay_to).unwrap().1), zcash_protocol::consensus::NetworkType::Regtest, 3000_0000, "repay", Some("repay"));
    let out = Command::new(bin("zdlc-dev")).args(["pay", "--node", &st.chain, &uri]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    println!("[kit] {}", String::from_utf8_lossy(&out.stdout).trim());
    let l = st.b.until(&loan, "settled", 240).await;
    println!("[kit] loan: attested {} {}", l["attested"], l["settlement"]);
    assert_eq!(l["attested"], "paid");
    log.push(json!({"run": "pay on payment: repaid", "id": loan, "repay_to": repay_to, "attested": l["attested"], "settlement": l["settlement"]}));
    st.miner.abort();
    if let Ok(p) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(p, serde_json::to_string_pretty(&log).unwrap()).unwrap();
    }
}

/// Seed-only recovery: a bet the arbiters never resolve; both nodes go down after
/// funding; a node restored from Alice's seed with an empty database finds her record
/// and broadcasts the refund once due.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura"]
async fn seed_only_recovery() {
    let Stack { rt, chain: _, mut a, mut b, arbiters: _, oracles: _, outside: _, h0: _, _procs, miner } = stack(18040).await;
    let from = rt.tip().await.unwrap();
    let idb = b.cli(&["info"])["identity"].as_str().unwrap().to_string();
    let id = id_of(a.cli(&["offer", "bet", "--to", &idb, "--question", "Never resolved?", "--outcomes", "yes,no", "--offerer-wins", "yes", "--stake", "0.2", "--maturity", "10", "--refund", "40"]));
    b.until(&id, "incoming", 30).await;
    b.cli(&["accept", &id, "--funding", "treasury"]);
    a.until(&id, "funded", 120).await;
    // Wait for Alice's record (it may need the funding change to confirm first).
    let deadline = Instant::now() + Duration::from_secs(60);
    let rec = loop {
        let c = a.cli(&["show", &id]);
        if c["recordTxid"].is_string() {
            break c;
        }
        assert!(Instant::now() < deadline, "no record: {}", c["note"]);
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    a.stop();
    b.stop();
    // Alice's device is lost: a fresh database, the same seed.
    let db = a.args.iter().position(|x| x == "--db").unwrap() + 1;
    a.args[db] = format!("{}.restored", a.args[db]);
    a.start();
    wait_http(&format!("{}/v1/openapi.json", a.url)).await;
    let early = a.cli(&["recover", "--from", &from.to_string()]);
    println!("[kit] restored before the refund height: {early}");
    let refund = rec["refundHeight"].as_u64().unwrap() as u32;
    while rt.tip().await.unwrap() < refund + 1 {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let due = a.cli(&["recover", "--from", &from.to_string()]);
    println!("[kit] restored after the refund height: {due}");
    let status = due["records"][0]["status"].as_str().unwrap().to_string();
    assert!(status.starts_with("refund sent"), "{status}");
    tokio::time::sleep(Duration::from_secs(4)).await;
    let after = a.cli(&["recover", "--from", &from.to_string()]);
    assert_eq!(after["records"][0]["status"], "settled", "{after}");
    miner.abort();
    if let Ok(p) = std::env::var("ZDLC_RESULTS_REC") {
        std::fs::write(p, serde_json::to_string_pretty(&json!({"id": id, "record_txid": rec["recordTxid"], "before": early, "due": due, "after": after})).unwrap()).unwrap();
    }
}

/// The kit-based desk agents: a maker and a taker process trade a 0.1 ZEC hedge
/// through the desk's quote board. Needs internet access for ZEC/USD.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura and internet access"]
async fn agents_trade_through_the_desk() {
    let st = stack(18070).await;
    let dir = std::env::temp_dir().join(format!("zdlc-agents-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |f: &str| dir.join(f).to_str().unwrap().to_string();
    let desk = st.a.args[st.a.args.iter().position(|x| x == "--desk").unwrap() + 1].clone();
    let (mw, tw) = (Wallet::from_seed([0x5A; 32]), Wallet::from_seed([0x7B; 32]));
    std::fs::write(path("maker.seed"), hex::encode([0x5Au8; 32])).unwrap();
    std::fs::write(path("taker.seed"), hex::encode([0x7Bu8; 32])).unwrap();
    st.rt.pay(&[(mw.coin_address(0), 1_0000_0000), (tw.coin_address(0), 1_0000_0000)]).await.unwrap();
    let common = |seed: &str| vec!["--seed".to_string(), path(seed), "--node".into(), st.chain.clone(), "--desk".into(), desk.clone(), "--oracles".into(), st.oracles.clone()];
    let mut maker = Command::new(bin("zdlc-agent")).arg("maker").args(common("maker.seed")).args(["--contracts", "1", "--log", &path("maker.json")]).spawn().unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let taker = Command::new(bin("zdlc-agent"))
        .arg("taker")
        .args(common("taker.seed"))
        .args(["--collateral", "0.1", "--minute-offset", "-3", "--log", &path("taker.json")])
        .output()
        .unwrap();
    println!("{}", String::from_utf8_lossy(&taker.stdout));
    assert!(taker.status.success(), "{}", String::from_utf8_lossy(&taker.stderr));
    assert!(maker.wait().unwrap().success());
    st.miner.abort();
    let log: Vec<Value> = serde_json::from_slice(&std::fs::read(path("taker.json")).unwrap()).unwrap();
    let settled = log.iter().rev().find(|s| s["event"]["kind"] == "settled").unwrap();
    println!("[kit] agents: {}", settled["event"]);
    assert_eq!(settled["event"]["how"], "close");
    if let Ok(p) = std::env::var("ZDLC_RESULTS_AGENTS") {
        let m: Vec<Value> = serde_json::from_slice(&std::fs::read(path("maker.json")).unwrap()).unwrap();
        std::fs::write(p, serde_json::to_string_pretty(&json!({"taker": log, "maker": m})).unwrap()).unwrap();
    }
}
