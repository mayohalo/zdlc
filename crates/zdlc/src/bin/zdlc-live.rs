//! zdlc on Zcash testnet, through any Zakura JSON-RPC endpoint.
//!
//! ```sh
//! zdlc-testnet status
//! zdlc-testnet run --settle close|cet|refund [--price 60.00]
//! zdlc-testnet sweep
//! ```
//!
//! Options: `--node URL` (or `ZDLC_TESTNET_RPC`; required, never stored), `--seed FILE` (default
//! `~/.zdlc/testnet-seed.hex`, made by `cargo run --example testnet_wallet`).
//!
//! The wallet's index 0 is the treasury (the faucet address). Each run takes two fresh
//! indices from `~/.zdlc/testnet-state.json`: Alice and Bob, both played by this wallet.
//! Collateral is 0.01 TAZ a side, under `params::TESTNET_CAP`. `--settle refund` is the
//! Phase 2 gate on testnet: Alice's state is dropped and her refund recovered from the
//! seed alone, while Bob does nothing.

use std::{path::PathBuf, time::Instant};

use serde_json::{json, Value};
use zcash_protocol::{consensus::NetworkType, local_consensus::LocalNetwork};
use zdlc::{
    contract::{Event, Payouts, Terms},
    keys,
    node::Node,
    oracle::NumericOracle,
    params::{self, TESTNET_CAP},
    payout::StableValue,
    recovery::{self, Record},
    tx::{self, Coin, Pay, Spec, Spend},
    txid_hex,
    wallet::Wallet,
    Error, Result,
};

const COLLATERAL: u64 = 100_0000; // 0.01 TAZ
const CUSHION: u64 = 20_0000; // funding fees and change
const RECORD_COIN: u64 = 10_0000;
const UNIT_CENTS: u64 = 10;

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
    format!("{}.{:08} TAZ", zat / 100_000_000, zat % 100_000_000)
}

fn addr(a: &zcash_transparent::address::TransparentAddress) -> String {
    keys::encode(a, NetworkType::Test)
}

fn step(title: &str) {
    println!("\n\x1b[1m{title}\x1b[0m");
}

struct Ctx {
    node: Node,
    p: LocalNetwork,
    seed_path: String,
    log: Vec<Value>,
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
            .utxos(&addr(&a))
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

fn next_indices() -> Result<(u32, u32)> {
    let path = home().join("testnet-state.json");
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

fn used_indices() -> u32 {
    std::fs::read(home().join("testnet-state.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["next_index"].as_u64())
        .unwrap_or(1) as u32
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let default_seed = home().join("testnet-seed.hex").to_string_lossy().to_string();
    let url = match arg(&args, "--node", "").as_str() {
        "" => std::env::var("ZDLC_TESTNET_RPC").unwrap_or_default(),
        u => u.to_string(),
    };
    if url.is_empty() {
        eprintln!("zdlc-testnet: set --node URL or ZDLC_TESTNET_RPC to a testnet Zakura JSON-RPC endpoint");
        std::process::exit(2);
    }
    let mut ctx = Ctx {
        node: Node::new(url),
        p: params::testnet(),
        seed_path: arg(&args, "--seed", &default_seed),
        log: vec![],
    };
    let r = match args.first().map(String::as_str) {
        Some("status") => status(&ctx).await,
        Some("sweep") => sweep(&ctx).await,
        Some("run") => run(&mut ctx, &args).await,
        _ => Err(Error::Build("usage: zdlc-testnet status | run --settle close|cet|refund [--price D] | sweep".into())),
    };
    if let Err(e) = r {
        eprintln!("\nzdlc-testnet: {e}");
        std::process::exit(1);
    }
}

async fn check_chain(ctx: &Ctx) -> Result<u32> {
    let info = ctx.node.blockchain_info().await?;
    if info["chain"] != "test" {
        return Err(Error::Rpc(format!("node is on {}, not testnet", info["chain"])));
    }
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
    println!("testnet tip {tip}, next block branch {:08x}", params::branch_at(&ctx.p, tip + 1));
    let n = used_indices();
    for i in 0..n {
        let c: u64 = ctx.coins(&w, i, false).await?.iter().map(|c| c.value).sum();
        let p: u64 = ctx.coins(&w, i, true).await?.iter().map(|c| c.value).sum();
        if i == 0 || c + p > 0 {
            println!("index {i}: coins {} at {}, payouts {}", taz(c), addr(&w.coin_address(i)), taz(p));
        }
    }
    Ok(())
}

/// Moves every transparent coin of indices 1.. (coins and payouts) back to the treasury.
async fn sweep(ctx: &Ctx) -> Result<()> {
    let tip = check_chain(ctx).await?;
    let w = ctx.wallet()?;
    let mut inputs = vec![];
    let mut ks = vec![];
    for i in 1..used_indices() {
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

async fn run(ctx: &mut Ctx, args: &[String]) -> Result<()> {
    let settle = arg(args, "--settle", "close");
    if !["close", "cet", "refund"].contains(&settle.as_str()) {
        return Err(Error::Build("--settle must be close, cet or refund".into()));
    }
    let price: f64 = arg(args, "--price", "60.00").parse().map_err(|_| Error::Build("--price".into()))?;
    let value = (price * 100.0 / UNIT_CENTS as f64).round() as usize;
    let t_start = Instant::now();

    step("1. Node and wallet");
    let tip = check_chain(ctx).await?;
    let w = ctx.wallet()?;
    let treasury = ctx.coins(&w, 0, false).await?;
    let have: u64 = treasury.iter().map(|c| c.value).sum();
    let (ia, ib) = next_indices()?;
    ctx.note("start", json!({"tip": tip, "branch": format!("{:08x}", params::branch_at(&ctx.p, tip + 1)),
        "treasury": taz(have), "alice_index": ia, "bob_index": ib, "settle": settle}));

    step("2. Split the treasury: Alice's funding and record coins, Bob's funding coin");
    let need = 2 * (COLLATERAL + CUSHION) + RECORD_COIN;
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 40,
        lock_height: None,
        inputs: treasury.clone(),
        outputs: vec![
            Pay::Transparent(w.coin_address(ia), COLLATERAL + CUSHION),
            Pay::Transparent(w.coin_address(ia), RECORD_COIN),
            Pay::Transparent(w.coin_address(ib), COLLATERAL + CUSHION),
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
    let alice = w.party(ia, vec![coin(0, COLLATERAL + CUSHION, ia)]);
    let bob = w.party(ib, vec![coin(2, COLLATERAL + CUSHION, ib)]);

    step("3. Terms and setup");
    let tip = ctx.node.height().await?;
    let curve = StableValue {
        d_cents: 50, // $0.50: 0.01 TAZ at $50.00
        q_recipient: COLLATERAL,
        q_hedger: COLLATERAL,
        unit_cents: UNIT_CENTS,
        nb_digits: 12,
        rounding: 2_0000, // 2% of the pot
    };
    let mut oracle = NumericOracle::new("testnet", &format!("zecusd/testnet/{tip}"), 12, UNIT_CENTS);
    let (maturity, refund) = if settle == "refund" { (tip + 4, tip + 10) } else { (tip + 4, tip + 30) };
    let terms = Terms {
        event: Event::Numeric(oracle.announcement().clone()),
        payouts: Payouts::StableValue(curve),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity,
        refund_height: refund,
        sign_height: tip + 1,
        dual_sign: false,
    };
    terms.check_cap(TESTNET_CAP)?;
    let t = Instant::now();
    let offer = alice.offer(terms.clone());
    let (accept, bob_txs) = bob.accept(&ctx.p, tip, &offer)?;
    let (sign, a) = alice.sign(&ctx.p, tip, &offer, &accept)?;
    let (funding, b) = bob.finish(&offer, bob_txs, &sign)?;
    ctx.note("setup", json!({"cets": a.txs.cets.len(), "adaptor_sigs_each_way": a.adaptor_count(),
        "maturity": maturity, "refund_height": refund, "ms": t.elapsed().as_millis(),
        "p2sh": addr(&zdlc::script::p2sh(&a.txs.redeem))}));

    step("4. Alice's recovery record, then funding");
    let anchor = ctx.node.ironwood_anchor(ctx.node.height().await?).await?;
    let rec = Record::from_contract(&a, ia)?;
    let rec_tx = recovery::record_tx(&ctx.p, ctx.node.height().await?, &w, &rec, coin(1, RECORD_COIN, ia), anchor)?;
    let h_rec = ctx.send_wait("recovery record", &rec_tx).await?;
    let h_fund = ctx.send_wait("funding", &funding).await?;
    ctx.note("funded", json!({"record_txid": txid_hex(&rec_tx.txid), "record_height": h_rec,
        "funding_txid": txid_hex(&funding.txid), "funding_height": h_fund, "branch": format!("{:08x}", funding.branch_id)}));

    match settle.as_str() {
        "refund" => {
            step("5. The oracle never attests; Alice loses everything but her seed; Bob is gone");
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
            ctx.note("GATE: seed-only refund on testnet", json!({"refund_height": rec.refund_height, "mined_at": h,
                "to": addr(&restored.payout_address(ia)), "txid": txid_hex(&refund_tx.txid)}));
        }
        _ => {
            step(&format!("5. Maturity: the oracle attests ${price:.2}"));
            ctx.node.wait_height(maturity).await?;
            let att = oracle.attest(value)?;
            let payout = terms.payout_at(att.value)?;
            ctx.note("attested", json!({"price_cents": att.value as u64 * UNIT_CENTS, "alice": taz(payout.0), "bob": taz(payout.1)}));
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
                step("6. Bob goes silent: Alice settles with the CET");
                let (cet, i) = a.cet_numeric(&att, ctx.node.height().await? + 1)?;
                let h = ctx.send_wait("CET", &cet).await?;
                ctx.note("CET", json!({"index": i, "mined_at": h, "txid": txid_hex(&cet.txid)}));
            }
        }
    }
    let path = home().join(format!("testnet-run-{tip}-{settle}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&ctx.log).unwrap()).ok();
    println!("\ndone in {:?}; log {}", t_start.elapsed(), path.display());
    Ok(())
}
