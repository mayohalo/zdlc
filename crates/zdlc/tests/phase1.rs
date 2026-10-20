//! Phase 1 gate on Zakura regtest: the stable-value contract settles at three prices and
//! refunds once, and its cooperative close lands in Ironwood. Needs a fresh node:
//!
//! ```sh
//! infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh
//! cargo test -p zdlc --release --test phase1 -- --ignored --nocapture
//! ```
//!
//! Alice is the recipient (owed $100, holding 2 ZEC at $50.00); Bob hedges with 2 ZEC.
//! Four contracts on one oracle each:
//! - $60.00, settled by cooperative close to Ironwood;
//! - $40.00, settled by CET (Bob stops answering);
//! - $20.00, below the $25.00 floor, settled by CET: Alice takes the whole pot;
//! - no attestation: refunded at the refund height.

use serde_json::{json, Value};
use zdlc::{
    contract::{Contract, Event, Payouts, Terms},
    oracle::NumericOracle,
    params::NU7_AT,
    payout::StableValue,
    regtest::Regtest,
    tx::{self, Pay},
    txid_hex,
};

const COLLATERAL: u64 = 2_0000_0000;
const FAUCET: u64 = 3_0000_0000;
const UNIT_CENTS: u64 = 10;
const DIGITS: usize = 12;

fn curve() -> StableValue {
    StableValue {
        d_cents: 100_00,
        q_recipient: COLLATERAL,
        q_hedger: COLLATERAL,
        unit_cents: UNIT_CENTS,
        nb_digits: DIGITS,
        rounding: 800_0000,
    }
}

struct Run {
    rt: Regtest,
    log: Vec<Value>,
}

impl Run {
    fn note(&mut self, step: &str, v: Value) {
        println!("[{step}] {v}");
        self.log.push(json!({"step": step, "result": v}));
    }

    /// Offer, accept, sign, finish; broadcasts and confirms the funding transaction.
    async fn setup(&mut self, tag: &str, maturity_in: u32, refund_in: u32) -> (Contract, Contract, NumericOracle, Terms) {
        let (alice, bob) = self.rt.faucet(tag, FAUCET).await.unwrap();
        let tip = self.rt.tip().await.unwrap();
        let o = NumericOracle::new("phase1", &format!("zecusd/{tag}"), DIGITS, UNIT_CENTS);
        let terms = Terms {
            event: Event::Numeric(o.announcement().clone()),
            payouts: Payouts::StableValue(curve()),
            offer_collateral: COLLATERAL,
            accept_collateral: COLLATERAL,
            maturity: tip + maturity_in,
            refund_height: tip + refund_in,
            sign_height: tip + 1,
            dual_sign: false,
        };
        let offer = alice.offer(terms.clone());
        let (accept, bob_txs) = bob.accept(&self.rt.p, tip, &offer).unwrap();
        let (sign, a) = alice.sign(&self.rt.p, tip, &offer, &accept).unwrap();
        let (funding, b) = bob.finish(&offer, bob_txs, &sign).unwrap();
        let h = self.rt.send_confirm(&funding).await.unwrap();
        self.note(
            &format!("{tag}: funded"),
            json!({"height": h, "funding_txid": txid_hex(&funding.txid), "cets": a.txs.cets.len(),
                   "adaptor_sigs_each_way": a.adaptor_count(), "refund_height": terms.refund_height,
                   "branch": format!("{:08x}", funding.branch_id)}),
        );
        (a, b, o, terms)
    }
}

fn price(v: usize) -> String {
    let c = v as u64 * UNIT_CENTS;
    format!("${}.{:02}", c / 100, c % 100)
}

#[tokio::test]
#[ignore = "needs infra/zakura (fresh chain)"]
async fn phase1_gate() {
    let url = std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into());
    let mut r = Run {
        rt: Regtest::connect(&url).await.expect("infra/zakura/scripts/up.sh"),
        log: vec![],
    };
    // Run in the NU7 era, past the last scheduled upgrade.
    r.rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let c = curve();
    r.note("curve", json!({"D_usd": 100, "pot_zat": c.total(), "floor": price((c.floor_cents() / UNIT_CENTS) as usize),
        "digits": DIGITS, "unit": "$0.10", "rounding_zat": c.rounding, "cets": c.ranges().len()}));

    // 1. $60.00 -> cooperative close to Ironwood.
    let (a, b, mut o, terms) = r.setup("p60", 3, 40).await;
    r.rt.node.mine_to(a.terms.maturity).await.unwrap();
    let att = o.attest(600).unwrap();
    let payout = terms.payout_at(att.value).unwrap();
    let tip = r.rt.tip().await.unwrap();
    let anchor = orchard::Anchor::empty_tree();
    let spec = a.close_spec(&r.rt.p, tip, payout, anchor).unwrap();
    let pczt = tx::build(&r.rt.p, &spec).unwrap();
    b.check_close(&pczt, &b.close_spec(&r.rt.p, tip, payout, anchor).unwrap()).unwrap();
    let close = tx::extract(b.sign_close(a.sign_close(pczt).unwrap()).unwrap()).unwrap();
    let pool0 = r.rt.node.pool_zat("ironwood").await.unwrap();
    let h = r.rt.send_confirm(&close).await.unwrap();
    let pool1 = r.rt.node.pool_zat("ironwood").await.unwrap();
    r.note("p60: cooperative close", json!({"price": price(att.value), "payout_alice_bob_zat": [payout.0, payout.1],
        "ironwood_outputs_zat": spec.outputs.iter().map(Pay::value).collect::<Vec<_>>(), "mined_height": h,
        "ironwood_pool_delta": pool1 - pool0, "txid": txid_hex(&close.txid), "bytes": close.raw.len()}));
    assert_eq!((pool1 - pool0) as u64, spec.outputs.iter().map(Pay::value).sum::<u64>());

    // 2. $40.00 -> CET, completed by Alice alone.
    let (a, _b, mut o, terms) = r.setup("p40", 3, 40).await;
    r.rt.node.mine_to(a.terms.maturity).await.unwrap();
    let att = o.attest(400).unwrap();
    let (cet, i) = a.cet_numeric(&att, r.rt.tip().await.unwrap() + 1).unwrap();
    let h = r.rt.send_confirm(&cet).await.unwrap();
    let outs = r.rt.outputs(&cet).await.unwrap();
    r.note("p40: CET", json!({"price": price(att.value), "payout_alice_bob_zat": terms.payout_at(att.value).unwrap(),
        "cet_index": i, "outputs_zat": outs, "mined_height": h, "txid": txid_hex(&cet.txid)}));
    assert_eq!(outs.iter().sum::<u64>(), a.txs.funding_coin.value - 10_000);

    // 3. $20.00 (below the floor) -> CET paying Alice the whole pot.
    let (a, _b, mut o, terms) = r.setup("p20", 3, 40).await;
    r.rt.node.mine_to(a.terms.maturity).await.unwrap();
    let att = o.attest(200).unwrap();
    let (cet, i) = a.cet_numeric(&att, r.rt.tip().await.unwrap() + 1).unwrap();
    let h = r.rt.send_confirm(&cet).await.unwrap();
    let outs = r.rt.outputs(&cet).await.unwrap();
    r.note("p20: CET", json!({"price": price(att.value), "payout_alice_bob_zat": terms.payout_at(att.value).unwrap(),
        "cet_index": i, "outputs_zat": outs, "mined_height": h, "txid": txid_hex(&cet.txid)}));
    assert_eq!(outs.len(), 1, "Bob's side is empty below the floor");

    // 4. The oracle never attests -> refund at its height, not before.
    let (a, b, _o, terms) = r.setup("silent", 3, 8).await;
    let refund = b.refund().unwrap();
    r.rt.node.mine_to(terms.refund_height - 1).await.unwrap();
    let early = r.rt.node.send(&refund.raw).await;
    assert!(early.is_err());
    r.rt.mine(1).await.unwrap();
    let h = r.rt.send_confirm(&refund).await.unwrap();
    let outs = r.rt.outputs(&refund).await.unwrap();
    r.note("silent: refund", json!({"refund_height": terms.refund_height, "early_rejected": format!("{:?}", early.err()),
        "mined_height": h, "outputs_zat": outs, "txid": txid_hex(&refund.txid)}));
    assert_eq!(h, terms.refund_height + 1);
    let _ = a;

    if let Ok(path) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(path, serde_json::to_string_pretty(&r.log).unwrap()).unwrap();
    }
}
