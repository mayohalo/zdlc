//! Phase 3 on Zakura regtest with two `zdlc-oracled` processes as a 2-of-2 multi-oracle.
//! Needs the regtest node (any height) and internet access for the ZEC/USD feed:
//!
//! ```sh
//! infra/zakura/scripts/up.sh
//! cargo test -p zdlc --release --test phase3 -- --ignored --nocapture
//! ```

use std::process::{Child, Command};

use orchard::keys::Scope;
use serde_json::{json, Value};
use zdlc::{
    contract::{Contract, Event, Party, Payouts, Terms},
    enclave::EventSpec,
    keys,
    oracle_client::{self, OracleClient},
    params::NU7_AT,
    payout::StableValue,
    regtest::Regtest,
    tx::{self, Coin, Pay, Spec, Spend},
    txid_hex,
    wallet::Wallet,
};

const COLLATERAL: u64 = 2_0000_0000;

struct Daemon {
    child: Child,
    client: OracleClient,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

fn spawn(dir: &std::path::Path, name: &str, port: u16, node: &str) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_zdlc-oracled"))
        .args(["--listen", &format!("127.0.0.1:{port}")])
        .args(["--state", dir.join(format!("{name}.sealed")).to_str().unwrap()])
        .args(["--sealing-key", dir.join(format!("{name}.key")).to_str().unwrap()])
        .args(["--node", node])
        .spawn()
        .expect("spawn zdlc-oracled");
    Daemon { child, client: OracleClient::new(format!("http://127.0.0.1:{port}")) }
}

async fn up(d: &Daemon) -> Value {
    for _ in 0..100 {
        if let Ok(v) = d.client.info().await {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("oracle did not start");
}

fn note(log: &mut Vec<Value>, step: &str, v: Value) {
    println!("[{step}] {v}");
    log.push(json!({"step": step, "result": v}));
}

fn setup(rt: &Regtest, alice: &Party, bob: &Party, terms: Terms, tip: u32) -> (Contract, Contract, tx::Final) {
    let offer = alice.offer(terms);
    let (accept, bob_txs) = bob.accept(&rt.p, tip, &offer).unwrap();
    let (sign, a) = alice.sign(&rt.p, tip, &offer, &accept).unwrap();
    let (funding, b) = bob.finish(&offer, bob_txs, &sign).unwrap();
    (a, b, funding)
}

#[tokio::test]
#[ignore = "needs infra/zakura and internet access"]
async fn phase3_two_oracles() {
    let url = std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into());
    let rt = Regtest::connect(&url).await.expect("infra/zakura/scripts/up.sh");
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let mut log = vec![];
    let dir = std::env::temp_dir().join(format!("zdlc-phase3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let a = spawn(&dir, "oracle-a", 17801, &url);
    let b = spawn(&dir, "oracle-b", 17802, &url);
    let (ia, ib) = (up(&a).await, up(&b).await);
    note(&mut log, "oracles", json!({"a": ia, "b": ib}));
    assert_ne!(ia["pubkey"], ib["pubkey"]);

    // ---- 1. ZEC/USD, 2-of-2, settled by CET --------------------------------------------
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let minute = (now / 60 - 3) * 60; // a closed minute, so the test need not wait
    let spec = EventSpec::ZecUsd { minute, unit_cents: 100, nb_digits: 14 };
    let id = format!("zecusd-{minute}");
    let (ann_a, ann_b) = (a.client.announce(&id, &spec).await.unwrap(), b.client.announce(&id, &spec).await.unwrap());
    let anns = vec![oracle_client::to_numeric(&ann_a).unwrap(), oracle_client::to_numeric(&ann_b).unwrap()];
    let curve = StableValue {
        d_cents: 2_000_00, // Alice is owed $2,000 against a 4 ZEC pot: floor $500
        q_recipient: COLLATERAL,
        q_hedger: COLLATERAL,
        unit_cents: 100,
        nb_digits: 14,
        rounding: 400_0000, // 1% of the pot
        premium: 0,
    };
    let (alice, bob) = rt.faucet("phase3-price", 3_0000_0000).await.unwrap();
    let tip = rt.tip().await.unwrap();
    let terms = Terms {
        event: Event::Numeric(anns.clone()),
        payouts: Payouts::StableValue(curve),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 2,
        refund_height: tip + 40,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
    };
    let (ca, _cb, funding) = setup(&rt, &alice, &bob, terms.clone(), tip);
    rt.send_confirm(&funding).await.unwrap();
    let (att_a, att_b) = (a.client.wait_attestation(&id).await.unwrap(), b.client.wait_attestation(&id).await.unwrap());
    let va = oracle_client::verify_numeric(&anns[0], &att_a).unwrap();
    let vb = oracle_client::verify_numeric(&anns[1], &att_b).unwrap();
    note(&mut log, "ZEC/USD attested", json!({"minute": minute, "a": att_a.detail, "b": att_b.detail}));
    assert_eq!(va.value, vb.value, "both oracles read the same closed minute");
    let (cet, i) = ca.cet_numeric(&[va.clone(), vb], rt.tip().await.unwrap() + 1).unwrap();
    let h = rt.send_confirm(&cet).await.unwrap();
    let outs = rt.outputs(&cet).await.unwrap();
    note(&mut log, "ZEC/USD CET on two attestations", json!({"price_usd": va.value, "cet_index": i, "of": ca.txs.cets.len(),
        "adaptor_sigs_each_way": ca.adaptor_count(), "owed_alice_bob": terms.payout_at(va.value).unwrap(), "outputs_zat": outs, "mined_at": h,
        "txid": txid_hex(&cet.txid)}));
    // One oracle's attestation alone opens nothing.
    let solo = oracle_client::verify_numeric(&anns[0], &att_a).unwrap();
    assert!(ca.cet_numeric(&[solo], rt.tip().await.unwrap() + 1).is_err());

    // ---- 2. Shielded payment, 2-of-2: a loan repaid on Zcash ---------------------------
    // Alice borrowed off-chain; her collateral returns to her if Bob's Ironwood address
    // receives 0.5 ZEC by the deadline, else Bob takes it. The oracles hold Bob's IVK.
    let wb = Wallet::from_seed([0xB3; 32]);
    let bob_ivk = hex::encode(wb.close_fvk(0).to_ivk(Scope::External).to_bytes());
    let (alice, bob) = rt.faucet("phase3-pay", 3_0000_0000).await.unwrap();
    let tip = rt.tip().await.unwrap();
    let spec = EventSpec::Payment { ivk: bob_ivk, min_zat: 5000_0000, from_height: tip + 1, by_height: tip + 6 };
    let id = format!("repay-{tip}");
    let (pa, pb) = (a.client.announce(&id, &spec).await.unwrap(), b.client.announce(&id, &spec).await.unwrap());
    let pann = vec![oracle_client::to_enumerated(&pa).unwrap(), oracle_client::to_enumerated(&pb).unwrap()];
    let terms = Terms {
        event: Event::Enumerated(pann.clone()),
        payouts: Payouts::Enumerated(vec![("paid".into(), 2 * COLLATERAL), ("unpaid".into(), 0)]),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 6,
        refund_height: tip + 40,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
    };
    let (ca, _cb, funding) = setup(&rt, &alice, &bob, terms, tip);
    rt.send_confirm(&funding).await.unwrap();
    // Too early: not attested before the deadline.
    assert!(a.client.attest(&id).await.unwrap().is_none());
    // Alice repays: 0.5 ZEC to Bob's Ironwood address, from a fresh coin.
    let wa = Wallet::from_seed([0xA3; 32]);
    let txid = rt.pay(&[(wa.coin_address(0), 6000_0000)]).await.unwrap();
    let t = rt.tip().await.unwrap();
    let anchor = rt.node.ironwood_anchor(t).await.unwrap();
    let mut spec = Spec {
        target_height: t + 1,
        expiry_height: t + 20,
        lock_height: None,
        inputs: vec![Coin { txid, index: 0, value: 6000_0000, spend: Spend::P2pkh(keys::pubkey(&wa.coin_key(0))) }],
        outputs: vec![Pay::Ironwood(wb.close_address(0), 5000_0000), Pay::Transparent(wa.coin_address(0), 1)],
        ironwood_anchor: Some(anchor),
    };
    let fee = tx::fee(&rt.p, &spec).unwrap();
    spec.outputs[1] = Pay::Transparent(wa.coin_address(0), 1000_0000 - fee);
    let repay = tx::extract(tx::sign(tx::build(&rt.p, &spec).unwrap(), 0, &wa.coin_key(0)).unwrap()).unwrap();
    let h_repay = rt.send_confirm(&repay).await.unwrap();
    rt.node.mine_to(tip + 6).await.unwrap();
    let (xa, xb) = (a.client.wait_attestation(&id).await.unwrap(), b.client.wait_attestation(&id).await.unwrap());
    let (oa, sa) = oracle_client::verify_outcome(&pann[0], &xa).unwrap();
    let (ob, sb) = oracle_client::verify_outcome(&pann[1], &xb).unwrap();
    assert_eq!((oa.as_str(), ob.as_str()), ("paid", "paid"));
    let cet = ca.cet_outcome("paid", &[sa, sb], rt.tip().await.unwrap() + 1).unwrap();
    let h = rt.send_confirm(&cet).await.unwrap();
    note(&mut log, "shielded payment: repaid, collateral returned", json!({"repay_mined_at": h_repay, "deadline": tip + 6,
        "a": xa.detail, "b": xb.detail, "cet_outputs_zat": rt.outputs(&cet).await.unwrap(), "cet_mined_at": h}));

    // ---- 3. Sealed state: restart and replay ---------------------------------------------
    let next = a.client.info().await.unwrap()["next_nonce"].clone();
    drop(a);
    let a2 = spawn(&dir, "oracle-a", 17801, &url);
    let i2 = up(&a2).await;
    assert_eq!(i2["pubkey"], ia["pubkey"], "key survives restart");
    assert_eq!(i2["next_nonce"], next, "nonce counter survives restart");
    let again = a2.client.attest(&format!("zecusd-{minute}")).await.unwrap().unwrap();
    assert_eq!(again.signatures, att_a.signatures, "a repeat request re-signs the same outcome");
    let sealed = std::fs::read(dir.join("oracle-a.sealed")).unwrap();
    assert!(serde_json::from_slice::<Value>(&sealed).is_err(), "state is not readable without the sealing key");
    note(&mut log, "restart", json!({"next_nonce": next, "replay_identical": true}));

    if let Ok(path) = std::env::var("ZDLC_RESULTS") {
        std::fs::write(path, serde_json::to_string_pretty(&log).unwrap()).unwrap();
    }
}
