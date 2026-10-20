//! A stable-value ZEC payment, end to end on the zdlc regtest node.
//!
//! ```sh
//! infra/zakura/scripts/up.sh
//! cargo run -p zdlc --release --bin zdlc-demo -- [--price 60.00] [--settle close|cet|refund]
//! ```
//!
//! Alice is owed $100 in 30 days and holds 2 ZEC at $50.00. Bob, a hedger, puts up 2 ZEC.
//! At maturity Alice receives ZEC worth $100, capped by the 4 ZEC pot; Bob keeps the rest.

use std::time::Instant;

use zdlc::{
    contract::{Event, Payouts, Terms},
    oracle::NumericOracle,
    params::{self, NU7_AT},
    payout::StableValue,
    regtest::Regtest,
    script,
    tx,
    keys, txid_hex,
};

const COLLATERAL: u64 = 2_0000_0000;
const UNIT_CENTS: u64 = 10;
const DIGITS: usize = 12;

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| default.to_string())
}

fn zec(zat: u64) -> String {
    format!("{}.{:08}", zat / 100_000_000, zat % 100_000_000)
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
        + " ZEC"
}

fn usd(zat: u64, price_cents: u64) -> String {
    let cents = zat as u128 * price_cents as u128 / 100_000_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

fn step(n: u32, title: &str) {
    println!("\n\x1b[1m{n}. {title}\x1b[0m");
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("\nzdlc-demo: {e}");
        std::process::exit(1);
    }
}

async fn run() -> zdlc::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = arg(&args, "--node", "http://127.0.0.1:18252/");
    let price: f64 = arg(&args, "--price", "60.00")
        .parse()
        .map_err(|_| zdlc::Error::Build("--price must be dollars, e.g. 60.00".into()))?;
    let settle = arg(&args, "--settle", "close");
    if !["close", "cet", "refund"].contains(&settle.as_str()) {
        return Err(zdlc::Error::Build("--settle must be close, cet or refund".into()));
    }
    let value = (price * 100.0 / UNIT_CENTS as f64).round() as usize;
    let price_cents = value as u64 * UNIT_CENTS;

    step(1, "Node");
    let rt = Regtest::connect(&url).await?;
    rt.node.mine_to(NU7_AT + 10).await?;
    let tip = rt.tip().await?;
    println!("   Zakura regtest at height {tip}, signing under NU7 (branch {:08x})", params::branch_at(&rt.p, tip + 1));

    step(2, "Terms");
    let curve = StableValue {
        d_cents: 100_00,
        q_recipient: COLLATERAL,
        q_hedger: COLLATERAL,
        unit_cents: UNIT_CENTS,
        nb_digits: DIGITS,
        rounding: 800_0000,
    };
    println!("   Alice (recipient) is owed $100.00 and puts up {} (worth $100.00 at $50.00)", zec(COLLATERAL));
    println!("   Bob (hedger) puts up {}; pot {}", zec(COLLATERAL), zec(curve.total()));
    println!("   Alice is made whole down to ${}.{:02}; below that she takes the whole pot", curve.floor_cents() / 100, curve.floor_cents() % 100);

    step(3, "Oracle announces the ZEC/USD event");
    let tag = format!("demo-{tip}");
    let mut oracle = NumericOracle::new("demo", &format!("zecusd/{tag}"), DIGITS, UNIT_CENTS);
    let a = oracle.announcement().clone();
    println!("   key {}", hex::encode(a.pubkey.serialize()));
    println!("   {DIGITS} nonces, one per binary digit of the price in $0.10 units (up to $409.50)");

    step(4, "Faucet: fresh transparent coins for both parties");
    let (alice, bob) = rt.faucet(&tag, 3_0000_0000).await?;
    let tip = rt.tip().await?;
    println!("   Alice and Bob each hold {}", zec(3_0000_0000));

    let terms = Terms {
        event: Event::Numeric(vec![a]),
        payouts: Payouts::StableValue(curve),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 3,
        refund_height: tip + 8,
        sign_height: tip + 1,
        dual_sign: false,
    };

    step(5, "Offer, accept, sign: every CET and the refund before funding");
    let offer = alice.offer(terms.clone());
    let t = Instant::now();
    let (accept, bob_txs) = bob.accept(&rt.p, tip, &offer)?;
    println!("   Bob checks the terms (maturity {}, refund {}, before any upgrade), derives {} CETs,", terms.maturity, terms.refund_height, bob_txs.cets.len());
    let n: usize = accept.cet_adaptors.iter().map(Vec::len).sum();
    println!("   adaptor-signs them ({n} signatures, one per price-digit prefix) and signs the refund  [{:?}]", t.elapsed());
    let t = Instant::now();
    let (sign, alice_c) = alice.sign(&rt.p, tip, &offer, &accept)?;
    println!("   Alice derives the same transactions, verifies all {n} of Bob's signatures, returns hers");
    println!("   and her signed funding input ({} B PCZT)  [{:?}]", sign.funding_pczt.len(), t.elapsed());
    let t = Instant::now();
    let (funding, bob_c) = bob.finish(&offer, bob_txs, &sign)?;
    println!("   Bob verifies Alice's signatures, checks the funding txid, signs his input  [{:?}]", t.elapsed());

    step(6, "Funding");
    let h = rt.send_confirm(&funding).await?;
    let p2sh = keys::encode_regtest(&script::p2sh(&alice_c.txs.redeem));
    println!("   {} locked in 2-of-2 {p2sh}", zec(alice_c.txs.funding_coin.value));
    println!("   funding {} mined at {h}", txid_hex(&funding.txid));

    let (pay_a, pay_b) = terms.payout_at(value)?;
    match settle.as_str() {
        "refund" => {
            step(7, "The oracle never attests");
            let refund = bob_c.refund()?;
            rt.node.mine_to(terms.refund_height - 1).await?;
            let early = rt.node.send(&refund.raw).await;
            println!("   refund at height {} rejected: {}", rt.tip().await?, early.err().map(|e| e.to_string()).unwrap_or_default().split("error: ").last().unwrap_or("").trim_end_matches(&['"', '}', ')'][..]));
            rt.mine(1).await?;
            let h = rt.send_confirm(&refund).await?;
            step(8, "Refund");
            println!("   mined at {h}: Alice {} and Bob {} back", zec(COLLATERAL), zec(COLLATERAL));
            println!("   txid {}", txid_hex(&refund.txid));
        }
        _ => {
            step(7, &format!("Maturity: the oracle attests ${}.{:02}", price_cents / 100, price_cents % 100));
            rt.node.mine_to(terms.maturity).await?;
            let att = oracle.attest(value)?;
            println!("   digits {:?}", att.digits);
            println!("   Alice is owed {} ({}), Bob {}", zec(pay_a), usd(pay_a, price_cents), zec(pay_b));
            if settle == "close" {
                step(8, "Cooperative close to Ironwood");
                let tip = rt.tip().await?;
                let anchor = orchard::Anchor::empty_tree();
                let spec = alice_c.close_spec(&rt.p, tip, (pay_a, pay_b), anchor)?;
                let t = Instant::now();
                let pczt = tx::build(&rt.p, &spec)?;
                println!("   Alice builds the close and proves it  [{:?}]", t.elapsed());
                bob_c.check_close(&pczt, &bob_c.close_spec(&rt.p, tip, (pay_a, pay_b), anchor)?)?;
                println!("   Bob checks it: spends only the funding output, each output matches its note");
                println!("   commitment, and his own output decrypts to his payout; then both sign");
                let close = tx::extract(bob_c.sign_close(alice_c.sign_close(pczt)?)?)?;
                let before = rt.node.pool_zat("ironwood").await?;
                let h = rt.send_confirm(&close).await?;
                let after = rt.node.pool_zat("ironwood").await?;
                let outs: Vec<String> = spec.outputs.iter().map(|o| zec(o.value())).collect();
                println!("   mined at {h}: Ironwood outputs {} (fee split evenly)", outs.join(" and "));
                println!("   Ironwood pool +{}; no CET ever reached the chain", zec((after - before) as u64));
                println!("   txid {}", txid_hex(&close.txid));
            } else {
                step(8, "Bob stops answering: Alice settles alone with the CET");
                let (cet, i) = alice_c.cet_numeric(&[att.clone()], rt.tip().await? + 1)?;
                println!("   CET #{i}: Bob's adaptor signature for the matching prefix, decrypted with");
                println!("   the oracle's digit signatures, plus Alice's own");
                let h = rt.send_confirm(&cet).await?;
                let outs = rt.outputs(&cet).await?;
                println!("   mined at {h}: outputs {}", outs.iter().map(|v| zec(*v)).collect::<Vec<_>>().join(" and "));
                println!("   txid {}", txid_hex(&cet.txid));
            }
        }
    }
    println!();
    Ok(())
}
