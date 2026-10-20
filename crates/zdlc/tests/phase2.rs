//! Phase 2 on Zakura regtest: adversarial cases, dual signing across NU7, the watcher,
//! and the gate: a wallet restored from its seed alone recovers its collateral at the
//! refund height while the other party is offline. Needs a fresh chain (pre-NU7 heights):
//!
//! ```sh
//! infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh
//! cargo test -p zdlc --release --test phase2 -- --ignored --nocapture
//! ```

use serde_json::{json, Value};
use zdlc::{
    contract::{Contract, Event, Party, Payouts, Splits, Terms},
    keys,
    oracle::{DevOracle, NumericOracle},
    params::{self, NU7_AT},
    payout::StableValue,
    recovery::{self, Record},
    regtest::Regtest,
    tx::{self, Coin, Pay, Spec, Spend},
    txid_hex,
    wallet::Wallet,
    watch::{Kit, Status},
};

const COLLATERAL: u64 = 2_0000_0000;

fn curve() -> StableValue {
    StableValue {
        d_cents: 100_00,
        q_recipient: COLLATERAL,
        q_hedger: COLLATERAL,
        unit_cents: 10,
        nb_digits: 12,
        rounding: 800_0000,
        premium: 0,
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

    async fn tip(&self) -> u32 {
        self.rt.tip().await.unwrap()
    }

    fn terms(&self, event: Event, payouts: Payouts, tip: u32, maturity: u32, refund: u32, dual: bool) -> Terms {
        Terms {
            event,
            payouts,
            offer_collateral: COLLATERAL,
            accept_collateral: COLLATERAL,
            maturity,
            refund_height: refund,
            sign_height: tip + 1,
            dual_sign: dual,
            roll: None,
            splits: Splits::default(),
        }
    }

    /// Runs setup; returns both contracts and the signed (not broadcast) funding.
    fn setup(&self, alice: &Party, bob: &Party, terms: Terms, tip: u32) -> (Contract, Contract, tx::Final) {
        let offer = alice.offer(terms);
        let (accept, bob_txs) = bob.accept(&self.rt.p, tip, &offer).unwrap();
        let (sign, a) = alice.sign(&self.rt.p, tip, &offer, &accept).unwrap();
        let (funding, b) = bob.finish(&offer, bob_txs, &sign).unwrap();
        (a, b, funding)
    }

    fn numeric(&self, tag: &str) -> (NumericOracle, Event) {
        let o = NumericOracle::new("phase2", tag, 12, 10);
        let e = Event::Numeric(vec![o.announcement().clone()]);
        (o, e)
    }
}

fn reason(r: Result<String, zdlc::Error>) -> String {
    match r {
        Ok(t) => format!("accepted {t}"),
        Err(e) => e.to_string().split("error: ").last().unwrap_or("").trim_end_matches(&['"', '}', ')', '\\'][..]).to_string(),
    }
}

#[tokio::test]
#[ignore = "needs infra/zakura (fresh chain)"]
async fn phase2_gate() {
    let url = std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into());
    let mut r = Run { rt: Regtest::connect(&url).await.expect("infra/zakura/scripts/up.sh"), log: vec![] };
    assert!(r.tip().await < 100, "needs a fresh chain: down.sh && up.sh");
    r.rt.node.mine_to(110).await.unwrap();

    // ---- Dual signing: funded before NU7, settled after it ------------------------------
    let (alice, bob) = r.rt.faucet("dual", 3_0000_0000).await.unwrap();
    let tip = r.tip().await;
    let (mut o_dual, ev) = r.numeric("dual");
    let refused = r.terms(ev.clone(), Payouts::StableValue(curve()), tip, 190, NU7_AT + 15, false).validate(&r.rt.p, tip);
    let terms = r.terms(ev, Payouts::StableValue(curve()), tip, 190, NU7_AT + 15, true);
    let (dual_a, _dual_b, funding) = r.setup(&alice, &bob, terms, tip);
    let h = r.rt.send_confirm(&funding).await.unwrap();
    r.note("dual: funded under NU6.3, refund after NU7", json!({"height": h, "refund_height": NU7_AT + 15,
        "without_dual_sign": refused.err().map(|e| e.to_string()), "cets": dual_a.txs.cets.len(),
        "adaptor_sigs_each_way": dual_a.adaptor_count()}));

    // ---- Refund racing a late CET -------------------------------------------------------
    let (alice, bob) = r.rt.faucet("race", 3_0000_0000).await.unwrap();
    let tip = r.tip().await;
    let (mut o, ev) = r.numeric("race");
    let (a, b, funding) = r.setup(&alice, &bob, r.terms(ev, Payouts::StableValue(curve()), tip, tip + 3, tip + 6, false), tip);
    r.rt.send_confirm(&funding).await.unwrap();
    r.rt.node.mine_to(tip + 6).await.unwrap();
    let att = o.attest(400).unwrap(); // the oracle is late: attests after the refund height
    let refund = b.refund().unwrap();
    let (cet, _) = a.cet_numeric(&[att.clone()], r.tip().await + 1).unwrap();
    let first = r.rt.node.send(&refund.raw).await;
    let second = r.rt.node.send(&cet.raw).await;
    r.rt.mine(1).await.unwrap();
    let mined = r.rt.node.mined_height(&txid_hex(&refund.txid)).await.unwrap();
    r.note("race: refund and a late CET both valid", json!({"refund": reason(first), "late_cet": reason(second), "refund_mined_at": mined}));
    assert!(mined.is_some());

    // ---- Dust: a payout below 54 zat is dropped to the other side -----------------------
    let (alice, bob) = r.rt.faucet("dust", 3_0000_0000).await.unwrap();
    let tip = r.tip().await;
    let mut o = DevOracle::new("phase2", "dust", &["alice", "bob"]);
    let ev = Event::Enumerated(vec![o.announcement().clone()]);
    let payouts = Payouts::Enumerated(vec![("alice".into(), 2 * COLLATERAL - 30), ("bob".into(), 30)]);
    let (a, _b, funding) = r.setup(&alice, &bob, r.terms(ev, payouts, tip, tip + 2, tip + 20, false), tip);
    r.rt.send_confirm(&funding).await.unwrap();
    let s = o.attest("alice").unwrap();
    let cet = a.cet_outcome("alice", &[s], r.tip().await + 1).unwrap();
    let h = r.rt.send_confirm(&cet).await.unwrap();
    let outs = r.rt.outputs(&cet).await.unwrap();
    // And what the node does with a 30 zat output.
    let w = Wallet::from_seed([0xD5; 32]);
    let txid = r.rt.pay(&[(w.coin_address(0), 100_000)]).await.unwrap();
    let mut spec = Spec {
        target_height: r.tip().await + 1,
        expiry_height: r.tip().await + 20,
        lock_height: None,
        inputs: vec![Coin { txid, index: 0, value: 100_000, spend: Spend::P2pkh(keys::pubkey(&w.coin_key(0))) }],
        outputs: vec![Pay::Transparent(w.payout_address(0), 30), Pay::Transparent(w.coin_address(0), 1)],
        ironwood_anchor: None,
        ironwood_spends: vec![],
    };
    let fee = tx::fee(&r.rt.p, &spec).unwrap();
    spec.outputs[1] = Pay::Transparent(w.coin_address(0), 100_000 - 30 - fee);
    let dusty = tx::extract(tx::sign(tx::build(&r.rt.p, &spec).unwrap(), 0, &w.coin_key(0)).unwrap()).unwrap();
    let dust_reply = reason(r.rt.node.send(&dusty.raw).await);
    r.note("dust: Bob's 30 zat payout", json!({"cet_outputs_zat": outs, "mined_height": h, "plain_30_zat_output": dust_reply}));
    assert_eq!(outs.len(), 1);
    r.rt.mine(1).await.unwrap();

    // ---- Counterparty stops mid-setup: the free option and how it ends ------------------
    // Bob holds a fully signed funding transaction he may never send. Its expiry bounds the
    // option; Alice can also end it at once by spending her funding input elsewhere.
    let (alice, bob) = r.rt.faucet("option", 3_0000_0000).await.unwrap();
    let tip = r.tip().await;
    let (_, ev) = r.numeric("option");
    let (_, _, held) = r.setup(&alice, &bob, r.terms(ev, Payouts::StableValue(curve()), tip, tip + 50, tip + 60, false), tip);
    let expiry = tip + 1 + zdlc::contract::EXPIRY_DELTA;
    r.rt.node.mine_to(expiry).await.unwrap();
    let late = reason(r.rt.node.send(&held.raw).await);
    r.note("free option: Bob sends funding after its expiry", json!({"expiry_height": expiry, "tip": r.tip().await, "reply": late}));
    assert!(!late.starts_with("accepted"));

    let (alice, bob) = r.rt.faucet("cancel", 3_0000_0000).await.unwrap();
    let tip = r.tip().await;
    let (_, ev) = r.numeric("cancel");
    let (_, _, held) = r.setup(&alice, &bob, r.terms(ev, Payouts::StableValue(curve()), tip, tip + 5, tip + 10, false), tip);
    let coin = alice.info.inputs[0].clone();
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 20,
        lock_height: None,
        inputs: vec![coin.clone()],
        outputs: vec![Pay::Transparent(alice.info.change, 1)],
        ironwood_anchor: None,
        ironwood_spends: vec![],
    };
    spec.outputs[0] = Pay::Transparent(alice.info.change, coin.value - tx::fee(&r.rt.p, &spec).unwrap());
    let cancel = tx::extract(tx::sign(tx::build(&r.rt.p, &spec).unwrap(), 0, &alice.input_keys[0]).unwrap()).unwrap();
    r.rt.send_confirm(&cancel).await.unwrap();
    let after = reason(r.rt.node.send(&held.raw).await);
    r.note("free option: Alice spends her input, then Bob sends funding", json!({"reply": after}));
    assert!(!after.starts_with("accepted"));

    // ---- Watcher: refunds a contract both parties abandoned ------------------------------
    let (alice, bob) = r.rt.faucet("watch", 3_0000_0000).await.unwrap();
    let tip = r.tip().await;
    let (_, ev) = r.numeric("watch");
    let (a, _b, funding) = r.setup(&alice, &bob, r.terms(ev, Payouts::StableValue(curve()), tip, tip + 3, tip + 8, false), tip);
    let kit_path = std::env::temp_dir().join(format!("zdlc-kit-{tip}.json"));
    let kit_path = kit_path.to_str().unwrap();
    Kit::new("watch", &a, tip).unwrap().save(kit_path).unwrap();
    drop(a);
    r.rt.send_confirm(&funding).await.unwrap();
    let mut steps = vec![];
    for _ in 0..15 {
        let mut kit = Kit::load(kit_path).unwrap(); // a restart between every pass
        let st = kit.step(&r.rt.node).await.unwrap();
        kit.save(kit_path).unwrap();
        steps.push(json!({"tip": r.tip().await, "status": format!("{st:?}")}));
        if let Status::Settled { by_refund, height, .. } = st {
            r.note("watcher: abandoned contract", json!({"refund_height": tip + 8, "settled_at": height, "by_refund": by_refund, "passes": steps.len()}));
            assert!(by_refund);
            break;
        }
        r.rt.mine(1).await.unwrap();
    }
    assert!(matches!(Kit::load(kit_path).unwrap().status, Status::Settled { .. }), "{steps:?}");

    // ---- Dual signing, continued: settle after NU7 with the second era's CET -------------
    let att = o_dual.attest(400).unwrap();
    r.rt.node.mine_to(NU7_AT - 1).await.unwrap();
    let (pre, _) = dual_a.cet_numeric(&[att.clone()], NU7_AT - 1).unwrap(); // signed for NU6.3
    let (post, _) = dual_a.cet_numeric(&[att.clone()], NU7_AT).unwrap(); // signed for NU7
    let pre_reply = reason(r.rt.node.send(&pre.raw).await);
    let h = r.rt.send_confirm(&post).await.unwrap();
    r.note("dual: CET after activation", json!({"nu6.3_cet": pre_reply, "nu7_cet_mined_at": h,
        "branch": format!("{:08x}", post.branch_id)}));
    assert_eq!(h, NU7_AT);

    // ---- Gate: seed-only recovery, counterparty offline ----------------------------------
    let seed = [0x5E; 32];
    let wallet = Wallet::from_seed(seed);
    let bob_dev = zdlc::regtest::dev_party("gate", "bob", 0xB0);
    let birthday = r.tip().await;
    let txid = r.rt.pay(&[(wallet.coin_address(0), 3_0000_0000), (wallet.coin_address(0), 1000_0000), (bob_dev.change, 3_0000_0000)]).await.unwrap();
    let coin = |index, value| Coin { txid, index, value, spend: Spend::P2pkh(keys::pubkey(&wallet.coin_key(0))) };
    let alice = wallet.party(0, vec![coin(0, 3_0000_0000)]);
    let bob = bob_dev.into_party(txid, 2, 3_0000_0000);
    let tip = r.tip().await;
    let (_, ev) = r.numeric("gate");
    let (a, _b, funding) = r.setup(&alice, &bob, r.terms(ev, Payouts::StableValue(curve()), tip, tip + 4, tip + 12, false), tip);
    // Alice posts her recovery record before the funding goes out.
    let rec = Record::from_contract(&a, 0).unwrap();
    let rec_tx = recovery::record_tx(&r.rt.p, tip, &wallet, &rec, coin(1, 1000_0000), orchard::Anchor::empty_tree()).unwrap();
    let h_rec = r.rt.send_confirm(&rec_tx).await.unwrap();
    let h_fund = r.rt.send_confirm(&funding).await.unwrap();
    let refund_height = a.terms.refund_height;
    let original_refund = a.refund().unwrap().txid;
    // Alice loses everything but her seed; Bob is gone.
    drop((a, alice, wallet));
    let restored = Wallet::from_seed(seed);
    let t0 = std::time::Instant::now();
    let found = recovery::scan(&r.rt.node, &restored, birthday, r.tip().await, 5).await.unwrap();
    let scan_time = t0.elapsed();
    assert_eq!(found.len(), 1, "one record");
    let refund = recovery::rebuild_refund(&r.rt.p, &restored, &found[0]).unwrap();
    assert_eq!(refund.txid, original_refund);
    r.rt.node.mine_to(refund_height - 1).await.unwrap();
    let early = reason(r.rt.node.send(&refund.raw).await);
    r.rt.mine(1).await.unwrap();
    let h = r.rt.send_confirm(&refund).await.unwrap();
    let v = r.rt.node.tx(&txid_hex(&refund.txid)).await.unwrap();
    let mine = keys::encode_regtest(&restored.payout_address(0));
    let to_alice: u64 = v["vout"].as_array().unwrap().iter()
        .filter(|o| o["scriptPubKey"]["addresses"][0] == mine.as_str())
        .map(|o| o["valueZat"].as_u64().unwrap()).sum();
    r.note("GATE: seed-only recovery", json!({"record_mined_at": h_rec, "record_bytes": rec.encode().len(),
        "funding_mined_at": h_fund, "blocks_scanned": r.tip().await - birthday, "scan_ms": scan_time.as_millis(),
        "refund_height": refund_height, "early": early, "refund_mined_at": h, "to_alice_zat": to_alice,
        "alice_payout_address": mine, "txid": txid_hex(&refund.txid)}));
    assert_eq!(h, refund_height + 1);
    assert!(to_alice >= COLLATERAL);
    assert!(params::branch_at(&r.rt.p, h) == params::BRANCH_NU7);

    if let Ok(path) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(path, serde_json::to_string_pretty(&r.log).unwrap()).unwrap();
    }
}
