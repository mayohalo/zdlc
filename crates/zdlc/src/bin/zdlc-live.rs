//! zdlc on a live Zcash network (testnet or mainnet), through any Zakura JSON-RPC endpoint.
//!
//! ```sh
//! zdlc-live status
//! zdlc-live run --settle close|cet|refund [--oracles URL,URL] [--price 60.00]
//! zdlc-live sweep
//! zdlc-live pay --to ADDR[,ADDR…] --amount ZAT     (from the treasury, one output per address)
//! ```
//!
//! Options: `--node URL` (or `ZDLC_RPC`; required, never stored); `--seed FILE` (default
//! `~/.zdlc/<network>-seed.hex`, made by `cargo run --example wallet`); `--collateral ZAT`;
//! `--mainnet` (required when the node is on mainnet: real funds).
//!
//! The network and its activation heights come from the node. The wallet's index 0 is the
//! treasury; each run takes two fresh indices (Alice and Bob, both played by this wallet).
//! Collateral is capped: `params::TESTNET_CAP`, `params::MAINNET_CAP`.
//!
//! - `--oracles A,B`: the event is ZEC/USD for a minute about two minutes ahead, announced
//!   by two `zdlc-oracled` services; the contract settles on both attestations (2-of-2).
//!   Without it, a dev oracle attests `--price`.
//! - `--settle refund`: no attestation is used; Alice's state is dropped and her refund
//!   recovered from the seed alone, while Bob does nothing.

use std::{path::PathBuf, time::Instant};

use serde_json::{json, Value};
use zcash_protocol::{consensus::NetworkType, local_consensus::LocalNetwork};
use zdlc::{
    contract::{Event, Payouts, Terms},
    enclave::EventSpec,
    feeds, keys,
    node::Node,
    oracle::NumericOracle,
    oracle_client::{self, OracleClient},
    params::{self, TESTNET_CAP},
    payout::StableValue,
    recovery::{self, Record},
    tx::{self, Coin, Pay, Spec, Spend},
    txid_hex,
    wallet::Wallet,
    Error, Result,
};

const CUSHION: u64 = 20_0000; // funding fees and change
const RECORD_COIN: u64 = 10_0000;

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME")).join(".zdlc")
}

fn taz(zat: u64) -> String {
    format!("{}.{:08} ZEC", zat / 100_000_000, zat % 100_000_000)
}

fn step(title: &str) {
    println!("\n\x1b[1m{title}\x1b[0m");
}

struct Ctx {
    node: Node,
    p: LocalNetwork,
    net: NetworkType,
    name: &'static str,
    seed_path: String,
    log: Vec<Value>,
}

impl Ctx {
    fn addr(&self, a: &zcash_transparent::address::TransparentAddress) -> String {
        keys::encode(a, self.net)
    }

    fn cap(&self) -> u64 {
        if self.net == NetworkType::Main { params::MAINNET_CAP } else { TESTNET_CAP }
    }
}

impl Ctx {
    fn note(&mut self, what: &str, v: Value) {
        println!("   {what}: {v}");
        self.log.push(json!({"step": what, "result": v}));
    }

    fn wallet(&self) -> Result<Wallet> {
        let h = std::fs::read_to_string(&self.seed_path)
            .map_err(|e| Error::Build(format!("seed {}: {e}", self.seed_path)))?;
        let b: [u8; 32] = hex::decode(h.trim())
            .ok()
            .and_then(|b| b.try_into().ok())
            .ok_or(Error::Build("seed must be 32 bytes of hex".into()))?;
        Ok(Wallet::from_seed(b))
    }

    async fn coins(&self, w: &Wallet, i: u32, payout: bool) -> Result<Vec<Coin>> {
        let (a, k) = if payout {
            (w.payout_address(i), w.payout_key(i))
        } else {
            (w.coin_address(i), w.coin_key(i))
        };
        Ok(self
            .node
            .utxos(&self.addr(&a))
            .await?
            .into_iter()
            .map(|(txid, index, value, _)| Coin {
                txid,
                index,
                value,
                spend: Spend::P2pkh(keys::pubkey(&k)),
            })
            .collect())
    }

    async fn send_wait(&self, what: &str, f: &tx::Final) -> Result<u32> {
        let txid = txid_hex(&f.txid);
        self.node.send(&f.raw).await?;
        println!("   {what}: sent {txid}, waiting for a block…");
        let h = self.node.wait_mined(&txid).await?;
        println!("   {what}: mined at {h}");
        Ok(h)
    }
}

fn next_indices(name: &str) -> Result<(u32, u32)> {
    let path = home().join(format!("{name}-state.json"));
    let mut st: Value = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(json!({"next_index": 1}));
    let a = st["next_index"].as_u64().unwrap_or(1) as u32;
    st["next_index"] = json!(a + 2);
    std::fs::write(&path, serde_json::to_vec_pretty(&st).unwrap())
        .map_err(|e| Error::Build(format!("state: {e}")))?;
    Ok((a, a + 1))
}

fn used_indices(name: &str) -> u32 {
    std::fs::read(home().join(format!("{name}-state.json")))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["next_index"].as_u64())
        .unwrap_or(1) as u32
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = match arg(&args, "--node", "").as_str() {
        "" => std::env::var("ZDLC_RPC").unwrap_or_default(),
        u => u.to_string(),
    };
    if url.is_empty() {
        eprintln!("zdlc-live: set --node URL or ZDLC_RPC to a Zakura JSON-RPC endpoint");
        std::process::exit(2);
    }
    let node = Node::new(url);
    let info = match node.blockchain_info().await {
        Ok(i) => i,
        Err(e) => {
            eprintln!("zdlc-live: {e}");
            std::process::exit(1);
        }
    };
    let (net, name) = match info["chain"].as_str() {
        Some("main") => (NetworkType::Main, "mainnet"),
        Some("test") => (NetworkType::Test, "testnet"),
        other => {
            eprintln!("zdlc-live: unsupported chain {other:?}");
            std::process::exit(1);
        }
    };
    if net == NetworkType::Main && !args.iter().any(|a| a == "--mainnet") {
        eprintln!("zdlc-live: the node is on MAINNET; pass --mainnet to move real funds");
        std::process::exit(2);
    }
    let Some(p) = params::from_node(&info["upgrades"]) else {
        eprintln!("zdlc-live: the node does not report NU6.3 (Ironwood)");
        std::process::exit(1);
    };
    let default_seed = home().join(format!("{name}-seed.hex")).to_string_lossy().to_string();
    let mut ctx = Ctx {
        node,
        p,
        net,
        name,
        seed_path: arg(&args, "--seed", &default_seed),
        log: vec![],
    };
    let r = match args.first().map(String::as_str) {
        Some("status") => status(&ctx).await,
        Some("sweep") => sweep(&ctx).await,
        Some("pay") => pay(&ctx, &args).await,
        Some("run") => run(&mut ctx, &args).await,
        _ => Err(Error::Build("usage: zdlc-live status | run --settle close|cet|refund [--oracles A,B] [--price D] | sweep".into())),
    };
    if let Err(e) = r {
        eprintln!("\nzdlc-live: {e}");
        std::process::exit(1);
    }
}

async fn check_chain(ctx: &Ctx) -> Result<u32> {
    let info = ctx.node.blockchain_info().await?;
    let tip = ctx.node.height().await?;
    let want = format!("{:08x}", params::branch_at(&ctx.p, tip + 1));
    if info["consensus"]["nextblock"] != want.as_str() {
        return Err(Error::Rpc(format!("node's next branch {} != zdlc's {want}", info["consensus"]["nextblock"])));
    }
    Ok(tip)
}

async fn status(ctx: &Ctx) -> Result<()> {
    let tip = check_chain(ctx).await?;
    let w = ctx.wallet()?;
    println!("{} tip {tip}, next block branch {:08x}", ctx.name, params::branch_at(&ctx.p, tip + 1));
    let n = used_indices(ctx.name);
    for i in 0..n {
        let c: u64 = ctx.coins(&w, i, false).await?.iter().map(|c| c.value).sum();
        let p: u64 = ctx.coins(&w, i, true).await?.iter().map(|c| c.value).sum();
        if i == 0 || c + p > 0 {
            println!("index {i}: coins {} at {}, payouts {}", taz(c), ctx.addr(&w.coin_address(i)), taz(p));
        }
    }
    Ok(())
}

/// Pays `--amount` to each `--to` transparent address from the treasury.
async fn pay(ctx: &Ctx, args: &[String]) -> Result<()> {
    let tip = check_chain(ctx).await?;
    let w = ctx.wallet()?;
    let amount: u64 = arg(args, "--amount", "").parse().map_err(|_| Error::Build("--amount ZAT".into()))?;
    let to: Vec<zcash_transparent::address::TransparentAddress> = arg(args, "--to", "")
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<zcash_address::ZcashAddress>()
                .map_err(|e| Error::Build(format!("{s}: {e}")))?
                .convert::<zcash_transparent::address::TransparentAddress>()
                .map_err(|e| Error::Build(format!("{s}: {e:?}")))
        })
        .collect::<Result<_>>()?;
    let inputs = ctx.coins(&w, 0, false).await?;
    let have: u64 = inputs.iter().map(|c| c.value).sum();
    let mut outputs: Vec<Pay> = to.iter().map(|a| Pay::Transparent(*a, amount)).collect();
    outputs.push(Pay::Transparent(w.coin_address(0), 1));
    let mut spec = Spec { target_height: tip + 1, expiry_height: tip + 40, lock_height: None, inputs: inputs.clone(), outputs, ironwood_anchor: None };
    let fee = tx::fee(&ctx.p, &spec)?;
    let last = spec.outputs.len() - 1;
    spec.outputs[last] = Pay::Transparent(w.coin_address(0), have.checked_sub(amount * to.len() as u64 + fee).ok_or(Error::Build("treasury too small".into()))?);
    let mut pczt = tx::build(&ctx.p, &spec)?;
    for i in 0..inputs.len() {
        pczt = tx::sign(pczt, i, &w.coin_key(0))?;
    }
    let f = tx::extract(pczt)?;
    ctx.send_wait(&format!("pay {} to {} addresses", taz(amount), to.len()), &f).await?;
    Ok(())
}

/// Moves every transparent coin of indices 1.. (coins and payouts) back to the treasury.
async fn sweep(ctx: &Ctx) -> Result<()> {
    let tip = check_chain(ctx).await?;
    let w = ctx.wallet()?;
    let mut inputs = vec![];
    let mut ks = vec![];
    for i in 1..used_indices(ctx.name) {
        for (payout, k) in [(false, w.coin_key(i)), (true, w.payout_key(i))] {
            for c in ctx.coins(&w, i, payout).await? {
                inputs.push(c);
                ks.push(k);
            }
        }
    }
    if inputs.is_empty() {
        println!("nothing to sweep");
        return Ok(());
    }
    let total: u64 = inputs.iter().map(|c| c.value).sum();
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 40,
        lock_height: None,
        inputs,
        outputs: vec![Pay::Transparent(w.coin_address(0), 1)],
        ironwood_anchor: None,
    };
    spec.outputs[0] = Pay::Transparent(w.coin_address(0), total - tx::fee(&ctx.p, &spec)?);
    let mut pczt = tx::build(&ctx.p, &spec)?;
    for (i, k) in ks.iter().enumerate() {
        pczt = tx::sign(pczt, i, k)?;
    }
    let f = tx::extract(pczt)?;
    ctx.send_wait(&format!("sweep {} from {} coins", taz(spec.outputs[0].value()), ks.len()), &f).await?;
    Ok(())
}

enum Oracles {
    Dev(NumericOracle, usize),
    Pair(Vec<OracleClient>, String),
}

async fn run(ctx: &mut Ctx, args: &[String]) -> Result<()> {
    let settle = arg(args, "--settle", "close");
    if !["close", "cet", "refund"].contains(&settle.as_str()) {
        return Err(Error::Build("--settle must be close, cet or refund".into()));
    }
    let collateral: u64 = arg(args, "--collateral", if ctx.net == NetworkType::Main { "100000" } else { "1000000" })
        .parse()
        .map_err(|_| Error::Build("--collateral is in zatoshis".into()))?;
    let oracle_urls: Vec<String> = arg(args, "--oracles", "").split(',').filter(|u| !u.is_empty()).map(String::from).collect();
    let t_start = Instant::now();

    step("1. Node and wallet");
    let tip = check_chain(ctx).await?;
    let w = ctx.wallet()?;
    let treasury = ctx.coins(&w, 0, false).await?;
    let have: u64 = treasury.iter().map(|c| c.value).sum();
    let (ia, ib) = next_indices(ctx.name)?;
    ctx.note("start", json!({"network": ctx.name, "tip": tip, "branch": format!("{:08x}", params::branch_at(&ctx.p, tip + 1)),
        "treasury": taz(have), "collateral": taz(collateral), "alice_index": ia, "bob_index": ib, "settle": settle,
        "oracles": oracle_urls.len().max(1)}));

    step("2. Split the treasury: Alice's funding and record coins, Bob's funding coin");
    let need = 2 * (collateral + CUSHION) + RECORD_COIN;
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 40,
        lock_height: None,
        inputs: treasury.clone(),
        outputs: vec![
            Pay::Transparent(w.coin_address(ia), collateral + CUSHION),
            Pay::Transparent(w.coin_address(ia), RECORD_COIN),
            Pay::Transparent(w.coin_address(ib), collateral + CUSHION),
            Pay::Transparent(w.coin_address(0), 1),
        ],
        ironwood_anchor: None,
    };
    let fee = tx::fee(&ctx.p, &spec)?;
    let change = have.checked_sub(need + fee).ok_or(Error::Build(format!("treasury {} < {}", taz(have), taz(need + fee))))?;
    spec.outputs[3] = Pay::Transparent(w.coin_address(0), change);
    let mut pczt = tx::build(&ctx.p, &spec)?;
    for i in 0..treasury.len() {
        pczt = tx::sign(pczt, i, &w.coin_key(0))?;
    }
    let split = tx::extract(pczt)?;
    let birthday = ctx.node.height().await?;
    let h = ctx.send_wait("split", &split).await?;
    ctx.note("split", json!({"txid": txid_hex(&split.txid), "height": h, "fee": fee}));
    let coin = |index: u32, value: u64, key: u32| Coin {
        txid: split.txid,
        index,
        value,
        spend: Spend::P2pkh(keys::pubkey(&w.coin_key(key))),
    };
    let alice = w.party(ia, vec![coin(0, collateral + CUSHION, ia)]);
    let bob = w.party(ib, vec![coin(2, collateral + CUSHION, ib)]);

    step("3. Oracle event, terms and setup");
    let tip = ctx.node.height().await?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    // Alice is owed what her collateral is worth now: priced from the last closed minute.
    let (spot_cents, _) = feeds::zecusd_minute((now / 60 - 2) * 60).await?;
    let unit_cents = 100;
    let nb_digits = 14;
    let curve = StableValue {
        d_cents: (spot_cents as u128 * collateral as u128 / 100_000_000) as u64,
        q_recipient: collateral,
        q_hedger: collateral,
        unit_cents,
        nb_digits,
        rounding: (2 * collateral / 100).max(1), // 1% of the pot
        premium: 0,
    };
    let (event, mut oracles) = if oracle_urls.is_empty() {
        let o = NumericOracle::new(ctx.name, &format!("zecusd/{}/{tip}", ctx.name), nb_digits, unit_cents);
        let price: f64 = arg(args, "--price", &format!("{}", spot_cents / 100)).parse().map_err(|_| Error::Build("--price".into()))?;
        let value = (price * 100.0 / unit_cents as f64).round() as usize;
        (Event::Numeric(vec![o.announcement().clone()]), Oracles::Dev(o, value))
    } else {
        let minute = (now / 60 + 2) * 60;
        let id = format!("zecusd-{minute}");
        let spec = EventSpec::ZecUsd { minute, unit_cents, nb_digits };
        let clients: Vec<OracleClient> = oracle_urls.iter().map(OracleClient::new).collect();
        let mut anns = vec![];
        for c in &clients {
            anns.push(oracle_client::to_numeric(&c.announce(&id, &spec).await?)?);
        }
        ctx.note("announced", json!({"event": id, "minute_utc": minute, "oracle_keys": anns.iter().map(|a| hex::encode(a.pubkey.serialize())).collect::<Vec<_>>()}));
        (Event::Numeric(anns), Oracles::Pair(clients, id))
    };
    let (maturity, refund) = if settle == "refund" { (tip + 4, tip + 10) } else { (tip + 6, tip + 40) };
    let terms = Terms {
        event,
        payouts: Payouts::StableValue(curve.clone()),
        offer_collateral: collateral,
        accept_collateral: collateral,
        maturity,
        refund_height: refund,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
    };
    terms.check_cap(ctx.cap())?;
    let t = Instant::now();
    let offer = alice.offer(terms.clone());
    let (accept, bob_txs) = bob.accept(&ctx.p, tip, &offer)?;
    let (sign, a) = alice.sign(&ctx.p, tip, &offer, &accept)?;
    let (funding, b) = bob.finish(&offer, bob_txs, &sign)?;
    ctx.note("setup", json!({"spot_usd_cents": spot_cents, "owed_usd_cents": curve.d_cents, "floor_usd_cents": curve.floor_cents(),
        "cets": a.txs.cets.len(), "adaptor_sigs_each_way": a.adaptor_count(), "maturity": maturity, "refund_height": refund,
        "ms": t.elapsed().as_millis(), "p2sh": ctx.addr(&zdlc::script::p2sh(&a.txs.redeem))}));

    step("4. Alice's recovery record, then funding");
    let anchor = ctx.node.ironwood_anchor(ctx.node.height().await?).await?;
    let rec = Record::from_contract(&a, ia)?;
    let rec_tx = recovery::record_tx(&ctx.p, ctx.node.height().await?, &w, &rec, coin(1, RECORD_COIN, ia), anchor)?;
    let h_rec = ctx.send_wait("recovery record", &rec_tx).await?;
    let h_fund = ctx.send_wait("funding", &funding).await?;
    ctx.note("funded", json!({"record_txid": txid_hex(&rec_tx.txid), "record_height": h_rec,
        "funding_txid": txid_hex(&funding.txid), "funding_height": h_fund, "branch": format!("{:08x}", funding.branch_id)}));

    if settle == "refund" {
        step("5. No attestation is used; Alice loses everything but her seed; Bob is gone");
        let original = a.refund()?.txid;
        drop((a, b, alice, bob));
        let restored = ctx.wallet()?;
        let t = Instant::now();
        let tip = ctx.node.height().await?;
        let found = recovery::scan(&ctx.node, &restored, birthday, tip, ib + 1).await?;
        let rec = found.into_iter().find(|r| r.index == ia).ok_or(Error::Build("record not found".into()))?;
        let refund_tx = recovery::rebuild_refund(&ctx.p, &restored, &rec)?;
        ctx.note("restored", json!({"blocks_scanned": tip - birthday + 1, "scan_ms": t.elapsed().as_millis(),
            "same_refund": refund_tx.txid == original}));
        ctx.node.wait_height(rec.refund_height - 1).await?;
        let early = ctx.node.send(&refund_tx.raw).await;
        ctx.note("refund one block early", json!({"tip": ctx.node.height().await?,
            "reply": early.map_err(|e| e.to_string()).err().unwrap_or("accepted".into())
                .split("error: ").last().unwrap_or("").chars().take(120).collect::<String>()}));
        ctx.node.wait_height(rec.refund_height).await?;
        let h = ctx.send_wait("refund", &refund_tx).await?;
        ctx.note("seed-only refund", json!({"refund_height": rec.refund_height, "mined_at": h,
            "to": ctx.addr(&restored.payout_address(ia)), "txid": txid_hex(&refund_tx.txid)}));
    } else {
        step("5. Maturity: the oracles attest");
        ctx.node.wait_height(maturity).await?;
        let atts = match &mut oracles {
            Oracles::Dev(o, value) => vec![o.attest(*value)?],
            Oracles::Pair(clients, id) => {
                let Event::Numeric(anns) = &terms.event else { unreachable!() };
                let mut v = vec![];
                for (c, ann) in clients.iter().zip(anns) {
                    let att = c.wait_attestation(id).await?;
                    ctx.note("attestation", json!({"oracle": hex::encode(ann.pubkey.serialize()), "detail": att.detail}));
                    v.push(oracle_client::verify_numeric(ann, &att)?);
                }
                v
            }
        };
        let payout = terms.payout_at(atts[0].value)?;
        ctx.note("attested", json!({"price_usd": atts[0].value, "agree": atts.iter().all(|x| x.digits == atts[0].digits),
            "alice": taz(payout.0), "bob": taz(payout.1)}));
        if settle == "close" {
            step("6. Cooperative close to Ironwood");
            let tip = ctx.node.height().await?;
            let anchor = ctx.node.ironwood_anchor(tip).await?;
            let spec = a.close_spec(&ctx.p, tip, payout, anchor)?;
            let pczt = tx::build(&ctx.p, &spec)?;
            b.check_close(&pczt, &b.close_spec(&ctx.p, tip, payout, anchor)?)?;
            let close = tx::extract(b.sign_close(a.sign_close(pczt)?)?)?;
            let h = ctx.send_wait("close", &close).await?;
            ctx.note("closed to Ironwood", json!({"mined_at": h, "outputs": spec.outputs.iter().map(|o| taz(o.value())).collect::<Vec<_>>(),
                "txid": txid_hex(&close.txid)}));
        } else {
            step("6. Bob goes silent: Alice settles with the CET on the oracles' attestations alone");
            let (cet, i) = a.cet_numeric(&atts, ctx.node.height().await? + 1)?;
            let h = ctx.send_wait("CET", &cet).await?;
            ctx.note("CET", json!({"index": i, "mined_at": h, "txid": txid_hex(&cet.txid)}));
        }
    }
    let path = home().join(format!("{}-run-{tip}-{settle}.json", ctx.name));
    std::fs::write(&path, serde_json::to_vec_pretty(&ctx.log).unwrap()).ok();
    println!("\ndone in {:?}; log {}", t_start.elapsed(), path.display());
    Ok(())
}
