//! Shielded funding by QR on Zakura regtest.
//!
//! ```sh
//! infra/zakura/scripts/up.sh
//! cargo test -p zdlc --release --test funding -- --ignored --nocapture --test-threads=1
//! ```

use serde_json::json;
use zdlc::watch::Payment;
use zdlc::{
    keys,
    leg::{self, Leg},
    params::NU7_AT,
    regtest::Regtest,
    tx::{self, Pay, Spec},
    txid_hex,
};

fn node_url() -> String {
    std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into())
}

fn write_result(name: &str, v: serde_json::Value) {
    if let Ok(dir) = std::env::var("ZDLC_RESULTS_DIR") {
        std::fs::write(format!("{dir}/funding-{name}.json"), serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }
}

/// M0: Zakura relays and mines both branches of a leg script: the 2-of-2 any time, the
/// owner's only from H (refused when the next block is H).
#[tokio::test]
#[ignore = "needs infra/zakura"]
async fn leg_script_branches() {
    let rt = Regtest::connect(&node_url()).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let (owner, a, b) = (keys::dev_key("leg/owner"), keys::dev_key("leg/a"), keys::dev_key("leg/b"));
    let tip = rt.tip().await.unwrap();
    let leg = Leg { owner: keys::pubkey(&owner), a: keys::pubkey(&a), b: keys::pubkey(&b), reclaim_height: tip + 6 };
    let txid = rt.pay(&[(leg.address(), 1_0000_0000), (leg.address(), 5000_0000)]).await.unwrap();
    let (c0, c1) = (leg.coin(txid, 0, 1_0000_0000), leg.coin(txid, 1, 5000_0000));
    let to = keys::p2pkh(&keys::pubkey(&keys::dev_key("leg/to")));

    // Both parties, before H.
    let tip = rt.tip().await.unwrap();
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 41,
        lock_height: None,
        inputs: vec![c0.clone()],
        outputs: vec![Pay::Transparent(to, c0.value)],
        ironwood_anchor: None,
        ironwood_spends: vec![],
    };
    let fee = tx::fee(&rt.p, &spec).unwrap();
    spec.outputs = vec![Pay::Transparent(to, c0.value - fee)];
    let pczt = tx::build(&rt.p, &spec).unwrap();
    let (sa, sb) = (leg::sign(&pczt, 0, &a).unwrap(), leg::sign(&pczt, 0, &b).unwrap());
    let joint = tx::extract_with_script_sigs(pczt, vec![leg.joint_script_sig(&sa, &sb)]).unwrap();
    let h_joint = rt.send_confirm(&joint).await.unwrap();
    println!("[leg] 2-of-2 branch mined at {h_joint} (H = {}), fee {fee}", leg.reclaim_height);

    // The owner alone: refused while the next block is below H + 1, accepted after.
    let early = leg::reclaim(&rt.p, rt.tip().await.unwrap(), &leg, &[c1.clone()], to, &owner).unwrap();
    rt.node.mine_to(leg.reclaim_height - 1).await.unwrap();
    let refused = rt.node.send(&early.raw).await;
    assert!(refused.is_err(), "owner branch accepted before H");
    rt.mine(1).await.unwrap();
    let h_owner = rt.send_confirm(&early).await.unwrap();
    println!("[leg] owner branch refused with tip {} ({:?}), mined at {h_owner}", leg.reclaim_height - 1, refused.as_ref().err());
    assert_eq!(h_owner, leg.reclaim_height + 1);
    // A wrong key cannot reclaim.
    assert!(leg::reclaim(&rt.p, h_owner, &leg, &[c1], to, &a).is_err());
    write_result("leg-branches", json!({"reclaim_height": leg.reclaim_height, "joint_height": h_joint, "joint_txid": txid_hex(&joint.txid),
        "joint_bytes": joint.raw.len(), "owner_refused_before_h": refused.err().map(|e| e.to_string()), "owner_height": h_owner,
        "owner_txid": txid_hex(&early.txid), "redeem_bytes": leg.redeem_bytes().len(), "input_size_for_fees": leg.input_size()}));
}

/// M0: one funding transaction spends one party's Ironwood note and the other's
/// transparent coin. The Ironwood party proves it, authorizes its spend and redacts its
/// secrets; the other checks the txid, signs its input and extracts.
#[tokio::test]
#[ignore = "needs infra/zakura"]
async fn ironwood_and_transparent_in_one_funding() {
    use orchard::keys::SpendAuthorizingKey;
    use zdlc::{funds, notes, script, wallet::Wallet};
    let rt = Regtest::connect(&node_url()).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    // An outside wallet holding Ironwood notes pays Alice's deposit address, as a wallet
    // scanning her QR would.
    let outside = Wallet::from_seed([0x0E; 32]);
    let alice = Wallet::from_seed([0xA7; 32]);
    let bob_key = keys::dev_key("m0/bob");
    let bob_addr = keys::p2pkh(&keys::pubkey(&bob_key));
    let txid = rt.pay(&[(outside.coin_address(0), 2_0000_0000), (bob_addr, 1_0000_0000)]).await.unwrap();
    let tip = rt.tip().await.unwrap();
    let coin = tx::Coin { txid, index: 0, value: 2_0000_0000, spend: tx::Spend::P2pkh(keys::pubkey(&outside.coin_key(0))) };
    let anchor = rt.node.ironwood_anchor(tip).await.unwrap();
    let shielded = funds::shield(&rt.p, tip, &[(coin, outside.coin_key(0))], outside.deposit_address(0), anchor).unwrap();
    let h0 = rt.send_confirm(&shielded).await.unwrap();
    let mine = notes::unspent(&rt.node, &outside.deposit_fvk(0), h0, h0).await.unwrap();
    let deposit = 1_0002_0000;
    let paid = funds::send_shielded(&rt.node, &rt.p, &outside.deposit_sk(0), &mine, alice.deposit_address(0), Some(deposit),
        outside.deposit_address(0), Some(b"zDLC1:m0".to_vec())).await.unwrap();
    let h1 = rt.send_confirm(&paid).await.unwrap();
    let found = notes::unspent(&rt.node, &alice.deposit_fvk(0), h1, h1).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].value(), deposit);
    assert!(found[0].memo.starts_with(b"zDLC1:m0"));
    println!("[m0] deposit of {deposit} zat seen at {h1} by Alice's deposit key");

    // The joint funding: Alice's note and Bob's coin into a 2-of-2.
    let (fa, fb) = (keys::dev_key("m0/fund-a"), keys::dev_key("m0/fund-b"));
    let redeem = script::multisig_2of2(&keys::pubkey(&fa), &keys::pubkey(&fb)).unwrap();
    let tip = rt.tip().await.unwrap();
    let (spends, anchor) = notes::spends(&rt.node, &alice.deposit_fvk(0), &found, tip).await.unwrap();
    let bob_coin = tx::Coin { txid, index: 1, value: 1_0000_0000, spend: tx::Spend::P2pkh(keys::pubkey(&bob_key)) };
    let pot = 1_5000_0000;
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 41,
        lock_height: None,
        inputs: vec![bob_coin],
        outputs: vec![Pay::Transparent(script::p2sh(&redeem), pot), Pay::Transparent(bob_addr, 1), Pay::Ironwood(alice.deposit_address(0), 1)],
        ironwood_anchor: Some(anchor),
        ironwood_spends: spends,
    };
    let fee = tx::fee(&rt.p, &spec).unwrap();
    let bob_share = 1_0000_0000 - pot / 2 - fee / 2;
    spec.outputs[1] = Pay::Transparent(bob_addr, bob_share);
    spec.outputs[2] = Pay::Ironwood(alice.deposit_address(0), deposit + 1_0000_0000 - pot - fee - bob_share);
    let t0 = std::time::Instant::now();
    let built = tx::build(&rt.p, &spec).unwrap();
    let prove_ms = t0.elapsed().as_millis();
    let (signed, n) = tx::sign_ironwood(built, &SpendAuthorizingKey::from(&alice.deposit_sk(0))).unwrap();
    assert_eq!(n, 1);
    let txid_alice = tx::txid(&signed).unwrap();
    let for_bob = tx::redact_ironwood(signed, true);
    let bytes = for_bob.clone().serialize().unwrap().len();
    // Bob: same txid from the redacted PCZT, signs his input, extracts and broadcasts.
    assert_eq!(tx::txid(&for_bob).unwrap(), txid_alice);
    let f = tx::extract(tx::sign(for_bob, 0, &bob_key).unwrap()).unwrap();
    assert_eq!(f.txid, txid_alice);
    let h2 = rt.send_confirm(&f).await.unwrap();
    let outs = rt.outputs(&f).await.unwrap();
    println!("[m0] joint funding mined at {h2}: transparent outputs {outs:?}, fee {fee}, proving {prove_ms} ms, redacted PCZT {bytes} B");
    assert_eq!(outs[0], pot);
    // Alice's change comes back to her deposit key.
    let change = notes::unspent(&rt.node, &alice.deposit_fvk(0), h1, h2).await.unwrap();
    assert_eq!(change.len(), 1);
    assert_eq!(change[0].height, h2);
    write_result("ironwood-joint", json!({"deposit_zat": deposit, "deposit_height": h1, "memo_kept": true, "funding_height": h2,
        "funding_txid": txid_hex(&f.txid), "transparent_outputs_zat": outs, "fee_zat": fee, "prove_ms": prove_ms,
        "redacted_pczt_bytes": bytes, "alice_change_zat": change[0].value()}));
}

/// M2, path A with one funder: a contractor invoices (offers) an escrow, the client
/// accepts and funds it from a shielded deposit. An outside wallet pays the deposit link
/// in two parts (underpaid, then topped up); the client's zDLC wallet funds the escrow
/// from both notes; "delivered" pays the contractor by CET.
#[tokio::test]
#[ignore = "needs infra/zakura"]
async fn path_a_escrow_funded_from_a_deposit() {
    use zcash_protocol::consensus::NetworkType;
    use zdlc::{
        contract::{ironwood_deposit, Event, IronwoodFunds, Payouts, Splits, Terms},
        funds, notes, oracle, paylink, watch,
        wallet::Wallet,
    };
    let rt = Regtest::connect(&node_url()).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let (cw, kw, outside) = (Wallet::from_seed([0xC7; 32]), Wallet::from_seed([0xC8; 32]), Wallet::from_seed([0x0F; 32]));
    // The outside wallet holds shielded funds.
    let txid = rt.pay(&[(outside.coin_address(0), 3_0000_0000)]).await.unwrap();
    let tip = rt.tip().await.unwrap();
    let coin = tx::Coin { txid, index: 0, value: 3_0000_0000, spend: tx::Spend::P2pkh(keys::pubkey(&outside.coin_key(0))) };
    let shielded = funds::shield(&rt.p, tip, &[(coin, outside.coin_key(0))], outside.deposit_address(0),
        rt.node.ironwood_anchor(tip).await.unwrap()).unwrap();
    let h0 = rt.send_confirm(&shielded).await.unwrap();

    let escrow = 1_5000_0000;
    let contractor = cw.party(1, vec![]);
    let mut client = kw.party(1, vec![]);
    client.info.ironwood = true;
    let mut o = oracle::DevOracle::new("path-a", "milestone", &["delivered", "not delivered"]);
    let tip = rt.tip().await.unwrap();
    let terms = Terms {
        event: Event::Enumerated(vec![o.announcement().clone()]),
        payouts: Payouts::Enumerated(vec![("delivered".into(), escrow), ("not delivered".into(), 0)]),
        offer_collateral: 0,
        accept_collateral: escrow,
        maturity: tip + 10,
        refund_height: tip + 100,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    // The client's app shows the deposit link for exactly what the contract needs.
    let need = ironwood_deposit(&rt.p, &terms, &contractor.info, &client.info, 1).unwrap();
    let link = paylink::deposit_uri(&kw.deposit_address(1), NetworkType::Regtest, need, "escrow-1");
    println!("[path A] deposit link ({need} zat):\n{link}\n{}", paylink::qr_text(&link).unwrap());
    let from = rt.tip().await.unwrap() + 1;

    // The payer sends 1 ZEC first: underpaid.
    let mine = notes::unspent(&rt.node, &outside.deposit_fvk(0), h0, h0).await.unwrap();
    let first = funds::send_shielded(&rt.node, &rt.p, &outside.deposit_sk(0), &mine, kw.deposit_address(1), Some(1_0000_0000),
        outside.deposit_address(0), Some(b"zDLC1:escrow-1".to_vec())).await.unwrap();
    let h1 = rt.send_confirm(&first).await.unwrap();
    let (state, _) = watch::deposit(&rt.node, &kw.deposit_fvk(1), from, need, 1).await.unwrap();
    println!("[path A] after the first payment: {state:?}");
    let Payment::Underpaid { got, .. } = state else { panic!("{state:?}") };
    // A second note costs one more Ironwood action: the top-up covers it.
    let need = ironwood_deposit(&rt.p, &terms, &contractor.info, &client.info, 2).unwrap();
    let want = need;
    let topup = paylink::deposit_uri(&kw.deposit_address(1), NetworkType::Regtest, want - got, "escrow-1");
    // The payer scans the top-up link.
    let change = notes::unspent(&rt.node, &outside.deposit_fvk(0), h1, h1).await.unwrap();
    let second = funds::send_shielded(&rt.node, &rt.p, &outside.deposit_sk(0), &change, kw.deposit_address(1), Some(want - got),
        outside.deposit_address(0), Some(b"zDLC1:escrow-1".to_vec())).await.unwrap();
    let h2 = rt.send_confirm(&second).await.unwrap();
    let (state, deposits) = watch::deposit(&rt.node, &kw.deposit_fvk(1), from, need, 1).await.unwrap();
    assert_eq!(state, Payment::Paid { got: need, over: 0 });
    assert_eq!(deposits.len(), 2);

    // Setup: the client's wallet builds the funding from both notes as it accepts.
    let tip = rt.tip().await.unwrap();
    let (spends, anchor) = notes::spends(&rt.node, &kw.deposit_fvk(1), &deposits, tip).await.unwrap();
    client.ironwood = Some(IronwoodFunds { sk: kw.deposit_sk(1), spends, anchor, change: kw.deposit_address(1) });
    let offer = contractor.offer(terms.clone());
    let t0 = std::time::Instant::now();
    let (accept, client_txs) = client.accept(&rt.p, tip, &offer).unwrap();
    let accept_ms = t0.elapsed().as_millis();
    let pczt_bytes = accept.funding.as_ref().unwrap().len();
    let (sign, contractor_c) = contractor.sign(&rt.p, tip, &offer, &accept).unwrap();
    let (funding, client_c) = client.finish(&offer, client_txs, &sign).unwrap();
    assert_eq!(funding.txid, contractor_c.txs.funding_txid);
    let h_fund = rt.send_confirm(&funding).await.unwrap();
    let outs = rt.outputs(&funding).await.unwrap();
    println!("[path A] funded at {h_fund} from two shielded notes: transparent outputs {outs:?} (accept with proving {accept_ms} ms, PCZT {pczt_bytes} B)");
    assert_eq!(outs, vec![escrow + client_c.txs.reserve]);
    let (left, _) = watch::deposit(&rt.node, &kw.deposit_fvk(1), from, 1, 1).await.unwrap();
    println!("[path A] the deposit receiver after funding: {left:?}");

    // Delivered: the contractor takes the escrow by CET.
    let s = o.attest("delivered").unwrap();
    let cet = contractor_c.cet_outcome("delivered", &[s], rt.tip().await.unwrap() + 1).unwrap();
    let h_cet = rt.send_confirm(&cet).await.unwrap();
    let paid = rt.outputs(&cet).await.unwrap();
    println!("[path A] delivered: CET at {h_cet} pays the contractor {paid:?}");
    assert_eq!(paid, vec![escrow + client_c.txs.reserve - 10_000]);
    write_result("path-a-escrow", json!({"deposit_link": link, "topup_link": topup, "deposit_zat": need,
        "payments": [{"height": h1, "zat": 1_0000_0000, "state_after": "underpaid"}, {"height": h2, "zat": want - got, "state_after": "paid"}],
        "funding_height": h_fund, "funding_txid": txid_hex(&funding.txid), "funding_outputs_zat": outs, "accept_with_proving_ms": accept_ms,
        "funding_pczt_bytes": pczt_bytes, "deposit_after_funding": format!("{left:?}"), "cet_height": h_cet, "cet_outputs_zat": paid,
        "cet_txid": txid_hex(&cet.txid)}));
}

fn enum_terms(o: &zdlc::oracle::DevOracle, tip: u32, c: u64, refund_in: u32) -> zdlc::contract::Terms {
    use zdlc::contract::{Event, Payouts, Splits, Terms};
    Terms {
        event: Event::Enumerated(vec![o.announcement().clone()]),
        payouts: Payouts::Enumerated(vec![("yes".into(), 2 * c), ("no".into(), 0)]),
        offer_collateral: c,
        accept_collateral: c,
        maturity: tip + 8,
        refund_height: tip + refund_in,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    }
}

/// M4 (a) and M5: both legs paid by plain payments (one from a shielded wallet, one
/// transparent, as from an exchange), swept into the 2-of-2 by the funding transaction
/// and settled by CET; the payout is auto-shielded into the zDLC wallet and withdrawn to
/// a unified address the user scans.
#[tokio::test]
#[ignore = "needs infra/zakura"]
async fn path_b_legs_settle_by_cet() {
    use zcash_protocol::consensus::NetworkType;
    use zdlc::{contract::leg_amounts, funds, notes, oracle, paylink, wallet::Wallet, watch};
    let rt = Regtest::connect(&node_url()).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let (aw, bw, outside) = (Wallet::from_seed([0xB1; 32]), Wallet::from_seed([0xB2; 32]), Wallet::from_seed([0xB3; 32]));
    let i = 2;
    // The outside wallet holds shielded funds.
    let txid = rt.pay(&[(outside.coin_address(0), 2_0000_0000)]).await.unwrap();
    let tip = rt.tip().await.unwrap();
    let coin = tx::Coin { txid, index: 0, value: 2_0000_0000, spend: tx::Spend::P2pkh(keys::pubkey(&outside.coin_key(0))) };
    let sh = funds::shield(&rt.p, tip, &[(coin, outside.coin_key(0))], outside.deposit_address(0), rt.node.ironwood_anchor(tip).await.unwrap()).unwrap();
    let h0 = rt.send_confirm(&sh).await.unwrap();

    let c = 1_0000_0000;
    let mut o = oracle::DevOracle::new("path-b", "q", &["yes", "no"]);
    let tip = rt.tip().await.unwrap();
    let terms = enum_terms(&o, tip, c, 80);
    // Both sides know each other's funding key (the offer's first round), so both legs exist.
    let (mut alice, mut bob) = (aw.party(i, vec![]), bw.party(i, vec![]));
    let leg = |w: &Wallet| Leg {
        owner: keys::pubkey(&w.leg_key(i)),
        a: alice.info.funding_pubkey,
        b: bob.info.funding_pubkey,
        reclaim_height: terms.refund_height,
    };
    let (la, lb) = (leg(&aw), leg(&bw));
    alice.info.inputs = vec![la.coin([0; 32], 0, 1)];
    bob.info.inputs = vec![lb.coin([0; 32], 0, 1)];
    let (need_a, need_b) = leg_amounts(&rt.p, &terms, &alice.info, &bob.info).unwrap();
    let (link_a, link_b) = (paylink::leg_uri(&la.address(), NetworkType::Regtest, need_a, "c2-alice"), paylink::leg_uri(&lb.address(), NetworkType::Regtest, need_b, "c2-bob"));
    println!("[path B] legs: {need_a} / {need_b} zat\n{link_a}\n{link_b}");

    // Alice pays from a shielded wallet; Bob from a transparent one.
    let mine = notes::unspent(&rt.node, &outside.deposit_fvk(0), h0, h0).await.unwrap();
    let pay_a = funds::pay_from_notes(&rt.node, &rt.p, &outside.deposit_sk(0), &mine, funds::To::Transparent(la.address()), Some(need_a), outside.deposit_address(0)).await.unwrap();
    rt.node.send(&pay_a.raw).await.unwrap();
    rt.pay(&[(lb.address(), need_b)]).await.unwrap();
    let (sa, coins_a) = watch::leg(&rt.node, NetworkType::Regtest, &la, need_a, 1).await.unwrap();
    let (sb, coins_b) = watch::leg(&rt.node, NetworkType::Regtest, &lb, need_b, 1).await.unwrap();
    assert_eq!((sa.clone(), sb.clone()), (Payment::Paid { got: need_a, over: 0 }, Payment::Paid { got: need_b, over: 0 }));
    println!("[path B] both legs confirmed: {sa:?} {sb:?}");

    // Setup over the legs: payouts are signed first, then the legs are swept.
    alice.info.inputs = coins_a;
    bob.info.inputs = coins_b;
    alice.input_keys.clear();
    bob.input_keys.clear();
    let tip = rt.tip().await.unwrap();
    let offer = alice.offer(terms.clone());
    let (accept, bob_txs) = bob.accept(&rt.p, tip, &offer).unwrap();
    let (sign, alice_c) = alice.sign(&rt.p, tip, &offer, &accept).unwrap();
    assert_eq!(sign.leg_sigs.len(), 2);
    let (funding, _bob_c) = bob.finish(&offer, bob_txs, &sign).unwrap();
    let h_fund = rt.send_confirm(&funding).await.unwrap();
    let fouts = rt.outputs(&funding).await.unwrap();
    println!("[path B] legs swept into the 2-of-2 at {h_fund}: {fouts:?}");
    assert_eq!(fouts, vec![2 * c + alice_c.txs.reserve]);

    // "yes": Alice's CET, then her payout moves into her zDLC wallet and out to her main wallet.
    let s = o.attest("yes").unwrap();
    let cet = alice_c.cet_outcome("yes", &[s], rt.tip().await.unwrap() + 1).unwrap();
    let h_cet = rt.send_confirm(&cet).await.unwrap();
    let shield = funds::auto_shield(&rt.node, &rt.p, NetworkType::Regtest, &aw, i).await.unwrap().expect("a payout to shield");
    let h_sh = rt.send_confirm(&shield).await.unwrap();
    let held = notes::unspent(&rt.node, &aw.deposit_fvk(i), h_sh, h_sh).await.unwrap();
    let main_ua = paylink::unified_address(&outside.close_address(9), NetworkType::Regtest);
    let wd = funds::withdraw(&rt.node, &rt.p, &aw, i, &held, &main_ua).await.unwrap();
    let h_wd = rt.send_confirm(&wd).await.unwrap();
    let got = zdlc::feeds::received(&rt.node, &outside.close_fvk(9).to_ivk(orchard::keys::Scope::External), h_wd, h_wd).await.unwrap();
    println!("[path B] CET {h_cet}; auto-shielded {} zat at {h_sh}; withdrawn {got} zat to {main_ua} at {h_wd}", held[0].value());
    assert!(got > 2 * c - 50_000);
    write_result("path-b-cet", json!({"leg_links": [link_a, link_b], "leg_zat": [need_a, need_b],
        "alice_leg_paid_from": "Ironwood notes", "bob_leg_paid_from": "transparent", "funding_height": h_fund, "funding_txid": txid_hex(&funding.txid),
        "funding_outputs_zat": fouts, "cet_height": h_cet, "cet_txid": txid_hex(&cet.txid), "auto_shield_height": h_sh,
        "shielded_zat": held[0].value(), "withdraw_height": h_wd, "withdrawn_zat": got}));
}

/// M4 (b) and M5: the counterparty never signs. Alice's leg stays locked until H; a
/// wallet restored from Alice's seed finds her leg record, rebuilds the leg and reclaims
/// it, refused at H - 1.
#[tokio::test]
#[ignore = "needs infra/zakura"]
async fn path_b_owner_reclaims_from_seed() {
    use zcash_protocol::consensus::NetworkType;
    use zdlc::{
        contract::leg_amounts,
        oracle,
        recovery::{self, LegRecord},
        wallet::Wallet,
        watch,
    };
    let rt = Regtest::connect(&node_url()).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let seed = [0xB4; 32];
    let (aw, bw) = (Wallet::from_seed(seed), Wallet::from_seed([0xB5; 32]));
    let i = 3;
    let o = oracle::DevOracle::new("path-b-silent", "q", &["yes", "no"]);
    let tip = rt.tip().await.unwrap();
    let terms = enum_terms(&o, tip, 5000_0000, 12);
    let (mut alice, mut bob) = (aw.party(i, vec![]), bw.party(i, vec![]));
    let la = Leg { owner: keys::pubkey(&aw.leg_key(i)), a: alice.info.funding_pubkey, b: bob.info.funding_pubkey, reclaim_height: terms.refund_height };
    let lb = Leg { owner: keys::pubkey(&bw.leg_key(i)), ..la.clone() };
    alice.info.inputs = vec![la.coin([0; 32], 0, 1)];
    bob.info.inputs = vec![lb.coin([0; 32], 0, 1)];
    let (need_a, _) = leg_amounts(&rt.p, &terms, &alice.info, &bob.info).unwrap();
    // Alice's app posts her leg record, then she pays her leg. Bob never pays or signs.
    let txid = rt.pay(&[(aw.coin_address(i), 1_0000_0000), (la.address(), need_a)]).await.unwrap();
    let rec = LegRecord { index: i, is_offerer: true, counterparty_pubkey: bob.info.funding_pubkey, reclaim_height: terms.refund_height };
    let tip = rt.tip().await.unwrap();
    let coin = tx::Coin { txid, index: 0, value: 1_0000_0000, spend: tx::Spend::P2pkh(keys::pubkey(&aw.coin_key(i))) };
    let rtx = recovery::leg_record_tx(&rt.p, tip, &aw, &rec, coin, rt.node.ironwood_anchor(tip).await.unwrap()).unwrap();
    let h_rec = rt.send_confirm(&rtx).await.unwrap();
    drop((alice, bob, aw));

    // Restored from the seed alone.
    let restored = Wallet::from_seed(seed);
    let found = recovery::scan_legs(&rt.node, &restored, h_rec - 2, rt.tip().await.unwrap(), 5).await.unwrap();
    assert_eq!(found, vec![rec]);
    let leg = found[0].leg(&restored);
    assert_eq!(leg.address(), la.address());
    let (state, coins) = watch::leg(&rt.node, NetworkType::Regtest, &leg, need_a, 1).await.unwrap();
    assert!(matches!(state, Payment::Paid { .. }));
    let early = leg::reclaim(&rt.p, rt.tip().await.unwrap(), &leg, &coins, restored.payout_address(i), &restored.leg_key(i)).unwrap();
    rt.node.mine_to(leg.reclaim_height - 1).await.unwrap();
    let refused = rt.node.send(&early.raw).await;
    assert!(refused.is_err());
    rt.mine(1).await.unwrap();
    let h = rt.send_confirm(&early).await.unwrap();
    let back = rt.outputs(&early).await.unwrap();
    println!("[path B] restored wallet found its leg record (block {h_rec}); reclaim refused at tip {}, mined at {h}: {back:?}", leg.reclaim_height - 1);
    assert_eq!(h, leg.reclaim_height + 1);
    write_result("path-b-reclaim", json!({"leg_zat": need_a, "record_height": h_rec, "reclaim_height": leg.reclaim_height,
        "refused_before_h": refused.err().map(|e| e.to_string()), "reclaim_mined": h, "reclaimed_zat": back, "txid": txid_hex(&early.txid)}));
}

/// M3, path A with two funders, through the desk: the taker funds its side from a
/// shielded deposit (its own process prints the link; the test plays the payer's wallet
/// scanning it) and the maker from firm transparent inputs; closed to Ironwood. Every
/// party is its own process. Needs internet access for the ZEC/USD feed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura and internet access"]
async fn path_a_desk_trade() {
    use std::process::{Command, Stdio};
    use zdlc::{funds, notes, paylink, wallet::Wallet};
    struct Proc(std::process::Child);
    impl Drop for Proc {
        fn drop(&mut self) {
            let _ = self.0.kill();
        }
    }
    let spawn = |bin: &str, args: &[&str]| Proc(Command::new(bin).args(args).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().unwrap());
    let node = node_url();
    let rt = Regtest::connect(&node).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let dir = std::env::temp_dir().join(format!("zdlc-funding-desk-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = |f: &str| dir.join(f).to_str().unwrap().to_string();
    let (maker_seed, taker_seed) = ([0x5Du8; 32], [0x6Au8; 32]);
    std::fs::write(path("maker.seed"), hex::encode(maker_seed)).unwrap();
    std::fs::write(path("taker.seed"), hex::encode(taker_seed)).unwrap();
    let (mw, outside) = (Wallet::from_seed(maker_seed), Wallet::from_seed([0x0D; 32]));
    let txid = rt.pay(&[(mw.coin_address(0), 3000_0000), (outside.coin_address(0), 1_0000_0000)]).await.unwrap();
    let tip = rt.tip().await.unwrap();
    let coin = tx::Coin { txid, index: 1, value: 1_0000_0000, spend: tx::Spend::P2pkh(keys::pubkey(&outside.coin_key(0))) };
    let sh = funds::shield(&rt.p, tip, &[(coin, outside.coin_key(0))], outside.deposit_address(0), rt.node.ironwood_anchor(tip).await.unwrap()).unwrap();
    let h0 = rt.send_confirm(&sh).await.unwrap();

    let _desk = spawn(env!("CARGO_BIN_EXE_zdlc-desk"), &["--listen", "127.0.0.1:17920", "--state", &path("desk.json"), "--node", &node]);
    let _oa = spawn(env!("CARGO_BIN_EXE_zdlc-oracled"), &["--listen", "127.0.0.1:17821", "--state", &path("a.sealed"), "--sealing-key", &path("a.key")]);
    let _ob = spawn(env!("CARGO_BIN_EXE_zdlc-oracled"), &["--listen", "127.0.0.1:17822", "--state", &path("b.sealed"), "--sealing-key", &path("b.key")]);
    for u in ["http://127.0.0.1:17920/health", "http://127.0.0.1:17821/pubkey", "http://127.0.0.1:17822/pubkey"] {
        let mut up = false;
        for _ in 0..100 {
            if reqwest::get(u).await.is_ok() {
                up = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(up, "{u}");
    }
    let (desk, oracles) = ("http://127.0.0.1:17920", "http://127.0.0.1:17821,http://127.0.0.1:17822");
    let miner = {
        let rt = Regtest::connect(&node).await.unwrap();
        tokio::spawn(async move {
            loop {
                let _ = rt.mine(1).await;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    };
    let mut maker = Command::new(env!("CARGO_BIN_EXE_zdlc-trader"))
        .args(["maker", "--seed", &path("maker.seed"), "--node", &node, "--desk", desk, "--oracles", oracles])
        .args(["--cap", "20000000", "--premium-bps", "50", "--ttl", "200", "--contracts", "1", "--log", &path("maker.json")])
        .spawn()
        .unwrap();
    let link_file = path("link.txt");
    let mut taker = Command::new(env!("CARGO_BIN_EXE_zdlc-trader"))
        .args(["taker", "--seed", &path("taker.seed"), "--node", &node, "--desk", desk, "--oracles", oracles])
        .args(["--collateral", "10000000", "--maturity", "12", "--refund", "300", "--event-offset-min", "-3"])
        .args(["--quote-wait", "3", "--settle", "close", "--fund", "deposit", "--link-out", &link_file, "--log", &path("taker.json")])
        .spawn()
        .unwrap();

    // The payer's wallet scans the link.
    let uri = loop {
        if let Ok(u) = std::fs::read_to_string(&link_file) {
            break u;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    };
    let (ua, zat, memo) = paylink::parse(&uri).unwrap();
    let (_, to) = paylink::decode_unified(&ua).unwrap();
    let mine = notes::unspent(&rt.node, &outside.deposit_fvk(0), h0, h0).await.unwrap();
    let pay = funds::send_shielded(&rt.node, &rt.p, &outside.deposit_sk(0), &mine, to, Some(zat), outside.deposit_address(0), memo).await.unwrap();
    rt.node.send(&pay.raw).await.unwrap();
    println!("[desk A] the payer's wallet paid {zat} zat to the taker's deposit link");

    assert!(taker.wait().unwrap().success(), "taker failed");
    assert!(maker.wait().unwrap().success(), "maker failed");
    miner.abort();
    let read = |f: &str| -> Vec<serde_json::Value> { serde_json::from_slice(&std::fs::read(path(f)).unwrap()).unwrap() };
    let (tk, mk) = (read("taker.json"), read("maker.json"));
    let steps: Vec<&str> = tk.iter().map(|s| s["step"].as_str().unwrap()).collect();
    println!("[desk A] taker steps: {steps:?}");
    for s in ["deposit link", "deposit received", "funded", "closed to Ironwood"] {
        assert!(steps.contains(&s), "{s}");
    }
    write_result("path-a-desk", json!({"deposit_link": uri, "deposit_zat": zat, "taker": tk, "maker": mk}));
}
