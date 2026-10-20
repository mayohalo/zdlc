//! Phase 4 products on Zakura regtest (needs internet access for ZEC/USD):
//!
//! 1. A private trade by memo invitation: the maker (its own process and seed) sends a
//!    quote to the taker's Ironwood inbox; the taker (its own process) finds it by trial
//!    decryption and trades through the desk mailbox. No public RFQ.
//! 2. Milestone escrow: the client funds alone; two arbiter oracles (2-of-2) resolve
//!    "delivered"; the contractor, who funded nothing, takes the escrow with the CET.
//! 3. Hedged payroll rolled on chain: month 1 settles into month 2 in one transaction
//!    that spends month 1's funding output, pays each side its P&L and opens month 2.
//!
//! ```sh
//! infra/zakura/scripts/up.sh
//! cargo test -p zdlc --release --test products -- --ignored --nocapture --test-threads=1
//! ```

use std::process::{Child, Command};

use serde_json::{json, Value};
use zdlc::{
    contract::{Event, Party, PartyInfo, Payouts, Roll, Splits, Terms},
    enclave::EventSpec,
    keys,
    oracle::NumericOracle,
    oracle_client::{self, OracleClient},
    params::NU7_AT,
    payout::StableValue,
    regtest::Regtest,
    tx::{Coin, Spend},
    txid_hex,
    wallet::Wallet,
};

struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
    }
}

async fn wait_http(url: &str) {
    for _ in 0..100 {
        if reqwest::get(url).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("{url} did not start");
}

fn node_url() -> String {
    std::env::var("ZDLC_NODE_URL").unwrap_or("http://127.0.0.1:18252/".into())
}

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("zdlc-products-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A wallet party funding from its treasury coin `c` (or nothing).
fn party(w: &Wallet, i: u32, inputs: Vec<Coin>, roll_key: Option<secp256k1::SecretKey>) -> Party {
    Party {
        funding_sk: w.funding_key(i),
        input_keys: vec![w.coin_key(0); inputs.len()],
        close_fvk: w.close_fvk(i),
        roll_key,
        ironwood: None,
        info: PartyInfo {
            funding_pubkey: keys::pubkey(&w.funding_key(i)),
            inputs,
            change: w.coin_address(0),
            payout: w.payout_address(i),
            close: w.close_address(i),
            ironwood: false,
        },
    }
}

fn coin(w: &Wallet, txid: [u8; 32], index: u32, value: u64) -> Coin {
    Coin { txid, index, value, spend: Spend::P2pkh(keys::pubkey(&w.coin_key(0))) }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs infra/zakura and internet access"]
async fn private_trade_by_memo_invitation() {
    let node = node_url();
    let rt = Regtest::connect(&node).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let d = tmp("invite");
    let path = |f: &str| d.join(f).to_str().unwrap().to_string();
    let (ms, ts) = ([0x91u8; 32], [0x92u8; 32]);
    std::fs::write(path("maker.seed"), hex::encode(ms)).unwrap();
    std::fs::write(path("taker.seed"), hex::encode(ts)).unwrap();
    let (mw, tw) = (Wallet::from_seed(ms), Wallet::from_seed(ts));
    rt.pay(&[(mw.coin_address(0), 3000_0000), (mw.coin_address(0), 1000_0000), (tw.coin_address(0), 3000_0000)]).await.unwrap();
    let _desk = Proc(Command::new(env!("CARGO_BIN_EXE_zdlc-desk")).args(["--listen", "127.0.0.1:17901", "--state", &path("desk.json"), "--node", &node]).spawn().unwrap());
    let _a = Proc(Command::new(env!("CARGO_BIN_EXE_zdlc-oracled")).args(["--listen", "127.0.0.1:17821", "--state", &path("a.sealed"), "--sealing-key", &path("a.key")]).spawn().unwrap());
    let _b = Proc(Command::new(env!("CARGO_BIN_EXE_zdlc-oracled")).args(["--listen", "127.0.0.1:17822", "--state", &path("b.sealed"), "--sealing-key", &path("b.key")]).spawn().unwrap());
    for u in ["http://127.0.0.1:17901/health", "http://127.0.0.1:17821/pubkey", "http://127.0.0.1:17822/pubkey"] {
        wait_http(u).await;
    }
    let miner = {
        let rt = Regtest::connect(&node).await.unwrap();
        tokio::spawn(async move {
            loop {
                let _ = rt.mine(1).await;
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    };
    let from = rt.tip().await.unwrap();
    let oracles = "http://127.0.0.1:17821,http://127.0.0.1:17822";
    let inbox = hex::encode(tw.close_address(0).to_raw_address_bytes());
    let mut maker = Command::new(env!("CARGO_BIN_EXE_zdlc-trader"))
        .args(["maker", "--seed", &path("maker.seed"), "--node", &node, "--desk", "http://127.0.0.1:17901", "--oracles", oracles])
        .args(["--cap", "20000000", "--ttl", "80", "--contracts", "1", "--log", &path("maker.json")])
        .args(["--invite", &inbox, "--invite-collateral", "10000000", "--invite-maturity", "4", "--invite-refund", "300"])
        .spawn()
        .unwrap();
    // The taker knows no desk and posts nothing: everything comes from the invitation.
    let st = Command::new(env!("CARGO_BIN_EXE_zdlc-trader"))
        .args(["taker", "--seed", &path("taker.seed"), "--node", &node, "--desk", "http://unused.invalid", "--oracles", oracles])
        .args(["--invitations-from", &from.to_string(), "--event-offset-min", "-3", "--settle", "close", "--log", &path("taker.json")])
        .status()
        .unwrap();
    assert!(st.success(), "taker failed");
    assert!(maker.wait().unwrap().success(), "maker failed");
    miner.abort();
    let tl: Vec<Value> = serde_json::from_slice(&std::fs::read(path("taker.json")).unwrap()).unwrap();
    let ml: Vec<Value> = serde_json::from_slice(&std::fs::read(path("maker.json")).unwrap()).unwrap();
    let steps: Vec<&str> = tl.iter().map(|s| s["step"].as_str().unwrap()).collect();
    assert!(steps.contains(&"invitation found") && steps.contains(&"closed to Ironwood"), "{steps:?}");
    assert!(!steps.contains(&"rfq posted"));
    write_result("invite", json!({"taker": tl, "maker": ml}));
}

fn write_result(name: &str, v: Value) {
    if let Ok(dir) = std::env::var("ZDLC_RESULTS_DIR") {
        std::fs::write(format!("{dir}/products-{name}.json"), serde_json::to_string_pretty(&v).unwrap()).unwrap();
    }
}

#[tokio::test]
#[ignore = "needs infra/zakura"]
async fn milestone_escrow_with_two_arbiters() {
    let node = node_url();
    let rt = Regtest::connect(&node).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    let d = tmp("escrow");
    let path = |f: &str| d.join(f).to_str().unwrap().to_string();
    std::fs::write(path("token"), "let-me-resolve").unwrap();
    let spawn = |port: &str, n: &str| {
        Proc(Command::new(env!("CARGO_BIN_EXE_zdlc-oracled"))
            .args(["--listen", &format!("127.0.0.1:{port}"), "--state", &path(&format!("{n}.sealed")), "--sealing-key", &path(&format!("{n}.key")), "--admin-token", &path("token")])
            .spawn()
            .unwrap())
    };
    let (_a, _b) = (spawn("17831", "a"), spawn("17832", "b"));
    for u in ["http://127.0.0.1:17831/pubkey", "http://127.0.0.1:17832/pubkey"] {
        wait_http(u).await;
    }
    let arbiters = [OracleClient::new("http://127.0.0.1:17831"), OracleClient::new("http://127.0.0.1:17832")];
    let spec = EventSpec::Arbiter { question: "Milestone 1 (API v2) delivered by height H?".into(), outcomes: vec!["delivered".into(), "not delivered".into()] };
    let mut anns = vec![];
    for c in &arbiters {
        anns.push(oracle_client::to_enumerated(&c.announce("milestone-1", &spec).await.unwrap()).unwrap());
    }
    let (cw, kw) = (Wallet::from_seed([0xC1; 32]), Wallet::from_seed([0xC2; 32]));
    let txid = rt.pay(&[(cw.coin_address(0), 2_0000_0000)]).await.unwrap();
    let escrow = 1_5000_0000;
    let client = party(&cw, 1, vec![coin(&cw, txid, 0, 2_0000_0000)], None);
    let contractor = party(&kw, 1, vec![], None); // funds nothing
    let tip = rt.tip().await.unwrap();
    let terms = Terms {
        event: Event::Enumerated(anns.clone()),
        payouts: Payouts::Enumerated(vec![("delivered".into(), 0), ("not delivered".into(), escrow)]),
        offer_collateral: escrow,
        accept_collateral: 0,
        maturity: tip + 5,
        refund_height: tip + 100,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    let offer = client.offer(terms);
    let (accept, txs) = contractor.accept(&rt.p, tip, &offer).unwrap();
    let (sign, _client_c) = client.sign(&rt.p, tip, &offer, &accept).unwrap();
    let (funding, contractor_c) = contractor.finish(&offer, txs, &sign).unwrap();
    let h_fund = rt.send_confirm(&funding).await.unwrap();
    // Before the arbiters decide, nothing is attested.
    assert!(arbiters[0].attest("milestone-1").await.unwrap().is_none());
    for c in &arbiters {
        c.resolve("milestone-1", "delivered", "let-me-resolve").await.unwrap();
    }
    // A second, different decision is refused; a wrong token is refused.
    assert!(arbiters[0].resolve("milestone-1", "not delivered", "let-me-resolve").await.is_err());
    assert!(arbiters[1].resolve("milestone-1", "delivered", "guess").await.is_err());
    let mut scalars = vec![];
    for (c, ann) in arbiters.iter().zip(&anns) {
        let att = c.attest("milestone-1").await.unwrap().unwrap();
        scalars.push(oracle_client::verify_outcome(ann, &att).unwrap().1);
    }
    let cet = contractor_c.cet_outcome("delivered", &scalars, rt.tip().await.unwrap() + 1).unwrap();
    let h = rt.send_confirm(&cet).await.unwrap();
    let outs = rt.outputs(&cet).await.unwrap();
    println!("[escrow] funded {h_fund} by the client alone; delivered; contractor's CET {h}: {outs:?}");
    assert_eq!(outs, vec![escrow + 10_000 - 10_000]);
    write_result("escrow", json!({"funding_height": h_fund, "escrow_zat": escrow, "cet_height": h, "cet_outputs_zat": outs, "txid": txid_hex(&cet.txid)}));
}

fn curve(q: u64) -> StableValue {
    StableValue { d_cents: 100_000_00, q_recipient: q, q_hedger: q, unit_cents: 100, nb_digits: 14, rounding: 2 * q / 100, premium: 0 }
}

#[tokio::test]
#[ignore = "needs infra/zakura"]
async fn payroll_rolled_on_chain() {
    let rt = Regtest::connect(&node_url()).await.unwrap();
    rt.node.mine_to(NU7_AT + 10).await.unwrap();
    // Employee (recipient, owed $1,000 a month) and employer (hedger).
    let (ew, bw) = (Wallet::from_seed([0xE1; 32]), Wallet::from_seed([0xE2; 32]));
    let q = 1_0000_0000; // 1 ZEC a side; $1,000 owed at $1,000 spot
    let txid = rt.pay(&[(ew.coin_address(0), q + 50_0000), (bw.coin_address(0), q + 50_0000), (bw.coin_address(0), 5000_0000)]).await.unwrap();
    let month = |n: u32| NumericOracle::new("payroll", &format!("zecusd-month-{n}"), 14, 100);
    let mut c1 = curve(q);
    c1.d_cents = 1_000_00;

    // Month 1.
    let (mut oa, mut ob) = (month(1), NumericOracle::new("payroll-b", "zecusd-month-1", 14, 100));
    let tip = rt.tip().await.unwrap();
    let t1 = Terms {
        event: Event::Numeric(vec![oa.announcement().clone(), ob.announcement().clone()]),
        payouts: Payouts::StableValue(c1.clone()),
        offer_collateral: q,
        accept_collateral: q,
        maturity: tip + 3,
        refund_height: tip + 100,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    let (e1, b1) = (party(&ew, 1, vec![coin(&ew, txid, 0, q + 50_0000)], None), party(&bw, 1, vec![coin(&bw, txid, 1, q + 50_0000)], None));
    let offer = e1.offer(t1.clone());
    let (acc, txs) = b1.accept(&rt.p, tip, &offer).unwrap();
    let (sg, ec1) = e1.sign(&rt.p, tip, &offer, &acc).unwrap();
    let (fund1, _bc1) = b1.finish(&offer, txs, &sg).unwrap();
    let h1 = rt.send_confirm(&fund1).await.unwrap();
    rt.node.mine_to(t1.maturity).await.unwrap();
    // ZEC fell to $800: the employee is owed 1.25 ZEC for her $1,000.
    let (aa, ab) = (oa.attest(800).unwrap(), ob.attest(800).unwrap());
    let credit = t1.payout_at(aa.value).unwrap();
    let _ = (aa, ab);

    // Month 2, opened by the roll: same size, spot now $800; the employer tops up.
    let (mut oa2, mut ob2) = (month(2), NumericOracle::new("payroll-b", "zecusd-month-2", 14, 100));
    let mut c2 = curve(q);
    c2.d_cents = 800_00;
    let tip = rt.tip().await.unwrap();
    let t2 = Terms {
        event: Event::Numeric(vec![oa2.announcement().clone(), ob2.announcement().clone()]),
        payouts: Payouts::StableValue(c2.clone()),
        offer_collateral: q,
        accept_collateral: q,
        maturity: tip + 3,
        refund_height: tip + 100,
        sign_height: tip + 1,
        dual_sign: false,
        roll: Some(Roll { coin: ec1.txs.funding_coin.clone(), credit }),
        splits: Splits::default(),
    };
    let e2 = party(&ew, 2, vec![], Some(ew.funding_key(1)));
    let b2 = party(&bw, 2, vec![coin(&bw, txid, 2, 5000_0000)], Some(bw.funding_key(1)));
    let offer = e2.offer(t2.clone());
    let (acc, txs) = b2.accept(&rt.p, tip, &offer).unwrap();
    let (sg, _ec2) = e2.sign(&rt.p, tip, &offer, &acc).unwrap();
    let (roll, bc2) = b2.finish(&offer, txs, &sg).unwrap();
    let h_roll = rt.send_confirm(&roll).await.unwrap();
    let v = rt.node.tx(&txid_hex(&roll.txid)).await.unwrap();
    let spends_month1 = v["vin"].as_array().unwrap().iter().any(|i| i["txid"] == txid_hex(&fund1.txid).as_str());
    let outs: Vec<(String, u64)> = v["vout"].as_array().unwrap().iter()
        .map(|o| (o["scriptPubKey"]["addresses"][0].as_str().unwrap_or("").to_string(), o["valueZat"].as_u64().unwrap()))
        .collect();
    let to_employee: u64 = outs.iter().filter(|(a, _)| *a == keys::encode_regtest(&ew.payout_address(2))).map(|(_, v)| v).sum();
    println!("[payroll] month 1 funded {h1}; credits at $800 {credit:?}; roll {h_roll} spends month 1: {spends_month1}; outputs {outs:?}");
    assert!(spends_month1);
    assert!(to_employee > 0, "the employee's surplus is paid out in the roll");

    // Month 2 settles at $1,000 with the CET.
    rt.node.mine_to(t2.maturity).await.unwrap();
    let atts = vec![oa2.attest(1000).unwrap(), ob2.attest(1000).unwrap()];
    let (cet, _) = bc2.cet_numeric(&atts, rt.tip().await.unwrap() + 1).unwrap();
    let h2 = rt.send_confirm(&cet).await.unwrap();
    println!("[payroll] month 2 CET at $1,000 mined {h2}: {:?}", rt.outputs(&cet).await.unwrap());
    write_result("payroll", json!({"month1_funding": h1, "month1_credits_at_800": credit, "roll_height": h_roll, "roll_spends_month1": spends_month1,
        "roll_outputs_zat": outs, "employee_paid_in_roll_zat": to_employee, "month2_cet_height": h2, "month2_outputs_zat": rt.outputs(&cet).await.unwrap()}));
}
