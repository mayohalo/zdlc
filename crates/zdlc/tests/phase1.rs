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

use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use serde_json::{json, Value};
use zdlc::{
    contract::{Contract, Event, Party, PartyInfo, Payouts, Terms},
    keys,
    node::Node,
    oracle::NumericOracle,
    params::{self, NU7_AT},
    payout::StableValue,
    tx::{self, Coin, Pay, Spec, Spend},
    txid_from_hex, txid_hex,
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
    node: Node,
    p: zcash_protocol::local_consensus::LocalNetwork,
    next_coinbase: u32,
    log: Vec<Value>,
}

impl Run {
    async fn tip(&self) -> u32 {
        self.node.height().await.unwrap()
    }

    fn note(&mut self, step: &str, v: Value) {
        println!("[{step}] {v}");
        self.log.push(json!({"step": step, "result": v}));
    }

    async fn mine(&self, n: u32) {
        self.node.generate(n).await.unwrap();
    }

    async fn send_confirm(&self, f: &tx::Final) -> u32 {
        self.node.send(&f.raw).await.expect("accepted");
        self.mine(1).await;
        self.node.mined_height(&txid_hex(&f.txid)).await.unwrap().expect("mined")
    }

    /// Pays a coinbase to fresh P2PKH coins for Alice and Bob (their funding inputs).
    async fn faucet(&mut self, tag: &str) -> (Party, Party) {
        let h = self.next_coinbase;
        self.next_coinbase += 1;
        let b = self.node.call("getblock", json!([h.to_string(), 2])).await.unwrap();
        let cb = &b["tx"][0];
        let cb_value = cb["vout"][0]["valueZat"].as_u64().unwrap();
        let miner = keys::miner();
        let coin = Coin {
            txid: txid_from_hex(cb["txid"].as_str().unwrap()).unwrap(),
            index: 0,
            value: cb_value,
            spend: Spend::P2pkh(keys::pubkey(&miner)),
        };
        let mk = |who: &str, seed: u8| {
            let funding_sk = keys::dev_key(&format!("{tag}/{who}/funding"));
            let input_sk = keys::dev_key(&format!("{tag}/{who}/input"));
            let close = FullViewingKey::from(&SpendingKey::from_bytes([seed; 32]).unwrap())
                .address_at(0u32, Scope::External);
            (funding_sk, input_sk, close)
        };
        let (fa, ia, ca) = mk("alice", 0xA1);
        let (fb, ib, cb_addr) = mk("bob", 0xB0);
        let tip = self.tip().await;
        let to_miner = keys::p2pkh(&keys::pubkey(&miner));
        let mut spec = Spec {
            target_height: tip + 1,
            expiry_height: tip + 20,
            lock_height: None,
            inputs: vec![coin],
            outputs: vec![
                Pay::Transparent(keys::p2pkh(&keys::pubkey(&ia)), FAUCET),
                Pay::Transparent(keys::p2pkh(&keys::pubkey(&ib)), FAUCET),
                Pay::Transparent(to_miner, 1),
            ],
            ironwood_anchor: None,
        };
        let fee = tx::fee(&self.p, &spec).unwrap();
        spec.outputs[2] = Pay::Transparent(to_miner, cb_value - 2 * FAUCET - fee);
        let f = tx::extract(tx::sign(tx::build(&self.p, &spec).unwrap(), 0, &miner).unwrap()).unwrap();
        self.node.send(&f.raw).await.expect("faucet");
        let party = |funding_sk, input_sk, close, index| Party {
            funding_sk,
            input_keys: vec![input_sk],
            info: PartyInfo {
                funding_pubkey: keys::pubkey(&funding_sk),
                inputs: vec![Coin {
                    txid: f.txid,
                    index,
                    value: FAUCET,
                    spend: Spend::P2pkh(keys::pubkey(&input_sk)),
                }],
                change: keys::p2pkh(&keys::pubkey(&input_sk)),
                payout: keys::p2pkh(&keys::pubkey(&keys::dev_key(&format!("{tag}/{index}/payout")))),
                close,
            },
        };
        (party(fa, ia, ca, 0), party(fb, ib, cb_addr, 1))
    }

    /// Offer, accept, sign, finish; broadcasts and confirms the funding transaction.
    async fn setup(&mut self, tag: &str, maturity_in: u32, refund_in: u32) -> (Contract, Contract, NumericOracle, Terms) {
        let (alice, bob) = self.faucet(tag).await;
        self.mine(1).await;
        let tip = self.tip().await;
        let o = NumericOracle::new("phase1", &format!("zecusd/{tag}"), DIGITS, UNIT_CENTS);
        let terms = Terms {
            event: Event::Numeric(o.announcement().clone()),
            payouts: Payouts::StableValue(curve()),
            offer_collateral: COLLATERAL,
            accept_collateral: COLLATERAL,
            maturity: tip + maturity_in,
            refund_height: tip + refund_in,
            sign_height: tip + 1,
        };
        let offer = alice.offer(terms.clone());
        let (accept, bob_txs) = bob.accept(&self.p, tip, &offer).unwrap();
        let (sign, a) = alice.sign(&self.p, tip, &offer, &accept).unwrap();
        let (funding, b) = bob.finish(&offer, bob_txs, &sign).unwrap();
        let h = self.send_confirm(&funding).await;
        self.note(
            &format!("{tag}: funded"),
            json!({"height": h, "funding_txid": txid_hex(&funding.txid), "cets": a.txs.cets.len(),
                   "adaptor_sigs_each_way": a.adaptor_count(), "refund_height": terms.refund_height,
                   "branch": format!("{:08x}", funding.branch_id)}),
        );
        (a, b, o, terms)
    }

    async fn outputs(&self, f: &tx::Final) -> Vec<u64> {
        let v = self.node.tx(&txid_hex(&f.txid)).await.unwrap();
        v["vout"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o["valueZat"].as_u64().unwrap())
            .collect()
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
        node: Node::new(url),
        p: params::regtest(Some(NU7_AT)),
        next_coinbase: 1,
        log: vec![],
    };
    assert!(r.tip().await < 100, "needs a fresh chain: down.sh && up.sh");
    // Run in the NU7 era, past the last scheduled upgrade.
    r.node.mine_to(NU7_AT + 10).await.unwrap();
    let c = curve();
    r.note("curve", json!({"D_usd": 100, "pot_zat": c.total(), "floor": price((c.floor_cents() / UNIT_CENTS) as usize),
        "digits": DIGITS, "unit": "$0.10", "rounding_zat": c.rounding, "cets": c.ranges().len()}));

    // 1. $60.00 -> cooperative close to Ironwood.
    let (a, b, mut o, terms) = r.setup("p60", 3, 40).await;
    r.node.mine_to(a.terms.maturity).await.unwrap();
    let att = o.attest(600).unwrap();
    let payout = terms.payout_at(att.value).unwrap();
    let tip = r.tip().await;
    let anchor = orchard::Anchor::empty_tree();
    let spec = a.close_spec(&r.p, tip, payout, anchor).unwrap();
    let pczt = tx::build(&r.p, &spec).unwrap();
    b.check_close(&pczt, &b.close_spec(&r.p, tip, payout, anchor).unwrap()).unwrap();
    let close = tx::extract(b.sign_close(a.sign_close(pczt).unwrap()).unwrap()).unwrap();
    let pool0 = r.node.pool_zat("ironwood").await.unwrap();
    let h = r.send_confirm(&close).await;
    let pool1 = r.node.pool_zat("ironwood").await.unwrap();
    r.note("p60: cooperative close", json!({"price": price(att.value), "payout_alice_bob_zat": [payout.0, payout.1],
        "ironwood_outputs_zat": spec.outputs.iter().map(Pay::value).collect::<Vec<_>>(), "mined_height": h,
        "ironwood_pool_delta": pool1 - pool0, "txid": txid_hex(&close.txid), "bytes": close.raw.len()}));
    assert_eq!((pool1 - pool0) as u64, spec.outputs.iter().map(Pay::value).sum::<u64>());

    // 2. $40.00 -> CET, completed by Alice alone.
    let (a, _b, mut o, terms) = r.setup("p40", 3, 40).await;
    r.node.mine_to(a.terms.maturity).await.unwrap();
    let att = o.attest(400).unwrap();
    let (cet, i) = a.cet_numeric(&att).unwrap();
    let h = r.send_confirm(&cet).await;
    let outs = r.outputs(&cet).await;
    r.note("p40: CET", json!({"price": price(att.value), "payout_alice_bob_zat": terms.payout_at(att.value).unwrap(),
        "cet_index": i, "outputs_zat": outs, "mined_height": h, "txid": txid_hex(&cet.txid)}));
    assert_eq!(outs.iter().sum::<u64>(), a.txs.funding_coin.value - 10_000);

    // 3. $20.00 (below the floor) -> CET paying Alice the whole pot.
    let (a, _b, mut o, terms) = r.setup("p20", 3, 40).await;
    r.node.mine_to(a.terms.maturity).await.unwrap();
    let att = o.attest(200).unwrap();
    let (cet, i) = a.cet_numeric(&att).unwrap();
    let h = r.send_confirm(&cet).await;
    let outs = r.outputs(&cet).await;
    r.note("p20: CET", json!({"price": price(att.value), "payout_alice_bob_zat": terms.payout_at(att.value).unwrap(),
        "cet_index": i, "outputs_zat": outs, "mined_height": h, "txid": txid_hex(&cet.txid)}));
    assert_eq!(outs.len(), 1, "Bob's side is empty below the floor");

    // 4. The oracle never attests -> refund at its height, not before.
    let (a, b, _o, terms) = r.setup("silent", 3, 8).await;
    let refund = b.refund().unwrap();
    r.node.mine_to(terms.refund_height - 1).await.unwrap();
    let early = r.node.send(&refund.raw).await;
    assert!(early.is_err());
    r.mine(1).await;
    let h = r.send_confirm(&refund).await;
    let outs = r.outputs(&refund).await;
    r.note("silent: refund", json!({"refund_height": terms.refund_height, "early_rejected": format!("{:?}", early.err()),
        "mined_height": h, "outputs_zat": outs, "txid": txid_hex(&refund.txid)}));
    assert_eq!(h, terms.refund_height + 1);
    let _ = a;

    if let Ok(path) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(path, serde_json::to_string_pretty(&r.log).unwrap()).unwrap();
    }
}
