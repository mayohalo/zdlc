//! Phase 1 offline: offer, accept, sign, finish between two parties, then settlement.

use std::time::Instant;

use zdlc::{
    contract::{Event, Party, Payouts, Splits, Terms},
    oracle,
    params::{self, NU7_AT},
    payout::StableValue,
    tx::{self, Pay},
};

const COLLATERAL: u64 = 2_0000_0000;

fn party(tag: &str, coin_txid: u8) -> Party {
    let who = if coin_txid % 2 == 1 { "alice" } else { "bob" };
    zdlc::regtest::dev_party(tag, who, coin_txid).into_party([coin_txid; 32], 0, 3_0000_0000)
}

pub fn curve() -> StableValue {
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

#[test]
fn enumerated_contract_end_to_end() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 120;
    let (alice, bob) = (party("enum/alice", 1), party("enum/bob", 2));
    let mut o = oracle::DevOracle::new("enum", "match-1", &["alice", "bob", "draw"]);
    let terms = Terms {
        event: Event::Enumerated(vec![o.announcement().clone()]),
        payouts: Payouts::Enumerated(vec![
            ("alice".into(), 2 * COLLATERAL),
            ("bob".into(), 0),
            ("draw".into(), COLLATERAL),
        ]),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 10,
        refund_height: tip + 30,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    let offer = alice.offer(terms);
    let (accept, bob_txs) = bob.accept(&p, tip, &offer).unwrap();
    let (sign, alice_c) = alice.sign(&p, tip, &offer, &accept).unwrap();
    let bob_txid = bob_txs.funding_txid;
    let (funding, bob_c) = bob.finish(&offer, bob_txs, &sign).unwrap();
    assert_eq!(funding.txid, bob_txid, "funding txid known before signing");
    assert_eq!(alice_c.txs.funding_txid, bob_c.txs.funding_txid);
    for (a, b) in alice_c.txs.cets.iter().zip(&bob_c.txs.cets) {
        assert_eq!(a.2, b.2, "CET sighashes agree");
    }
    let s = o.attest("draw").unwrap();
    let cet = bob_c.cet_outcome("draw", &[s], tip + 11).unwrap();
    let cet_a = alice_c.cet_outcome("draw", &[s], tip + 11).unwrap();
    assert_eq!(cet.txid, cet_a.txid, "either side can complete the CET");
    let refund = alice_c.refund().unwrap();
    assert_eq!(refund.lock_time, tip + 30);
    println!("enumerated: funding {} B, CET {} B, refund {} B", funding.raw.len(), cet.raw.len(), refund.raw.len());
}

#[test]
fn stable_value_contract_end_to_end() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 210;
    let (alice, bob) = (party("sv/alice", 3), party("sv/bob", 4));
    let mut o = oracle::NumericOracle::new("sv", "zecusd-at-260", 12, 10);
    let terms = Terms {
        event: Event::Numeric(vec![o.announcement().clone()]),
        payouts: Payouts::StableValue(curve()),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 50,
        refund_height: tip + 100,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    let offer = alice.offer(terms.clone());
    let t0 = Instant::now();
    let (accept, bob_txs) = bob.accept(&p, tip, &offer).unwrap();
    let t_accept = t0.elapsed();
    let t0 = Instant::now();
    let (sign, alice_c) = alice.sign(&p, tip, &offer, &accept).unwrap();
    let t_sign = t0.elapsed();
    let t0 = Instant::now();
    let (funding, bob_c) = bob.finish(&offer, bob_txs, &sign).unwrap();
    let t_finish = t0.elapsed();
    println!(
        "stable value: {} CETs, {} adaptor signatures per side; accept {:?}, sign {:?}, finish {:?}; funding PCZT {} B",
        alice_c.txs.cets.len(),
        alice_c.adaptor_count(),
        t_accept,
        t_sign,
        t_finish,
        sign.funding_pczt.len()
    );
    assert_eq!(funding.txid, alice_c.txs.funding_txid);

    // $40.00: the recipient is owed 2.5 ZEC (2.48 on the 0.08 grid).
    let att = o.attest(400).unwrap();
    let (cet, i) = alice_c.cet_numeric(&[att.clone()], tip + 51).unwrap();
    let (cet_b, j) = bob_c.cet_numeric(&[att.clone()], tip + 51).unwrap();
    assert_eq!((cet.txid, i), (cet_b.txid, j));
    assert_eq!(alice_c.txs.cets[i].0.offer, terms.payout_at(400).unwrap().0);
    println!("CET at $40.00: index {i}, offer {} zat, accept {} zat", alice_c.txs.cets[i].0.offer, alice_c.txs.cets[i].0.accept);

    // Cooperative close at the same price, built by Alice and checked by Bob.
    let payout = terms.payout_at(400).unwrap();
    let spec = alice_c.close_spec(&p, tip + 50, payout, orchard::Anchor::empty_tree()).unwrap();
    let pczt = tx::build(&p, &spec).unwrap();
    let spec_b = bob_c.close_spec(&p, tip + 50, payout, orchard::Anchor::empty_tree()).unwrap();
    bob_c.check_close(&pczt, &spec_b).unwrap();
    let wrong = bob_c.close_spec(&p, tip + 50, (payout.0 + 8_000_000, payout.1 - 8_000_000), orchard::Anchor::empty_tree()).unwrap();
    assert!(bob_c.check_close(&pczt, &wrong).is_err(), "a close paying other amounts is refused");
    let close = tx::extract(bob_c.sign_close(alice_c.sign_close(pczt).unwrap()).unwrap()).unwrap();
    println!("close: {} B", close.raw.len());
}

#[test]
fn upgrade_guard_refuses_a_contract_spanning_nu7() {
    let p = params::regtest(Some(NU7_AT));
    let (alice, bob) = (party("guard/alice", 5), party("guard/bob", 6));
    let o = oracle::NumericOracle::new("guard", "e", 12, 10);
    let mut terms = Terms {
        event: Event::Numeric(vec![o.announcement().clone()]),
        payouts: Payouts::StableValue(curve()),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: 150,
        refund_height: NU7_AT + 10,
        sign_height: 121,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    let err = bob.accept(&p, 120, &alice.offer(terms.clone())).err().unwrap();
    println!("guard: {err}");
    assert!(err.to_string().contains("spans a network upgrade"));
    terms.refund_height = NU7_AT - 1;
    assert!(terms.validate(&p, 120).is_ok());
}

/// Rewrites the `recipient` metadata of every Ironwood output paying `from` to `to`,
/// leaving the note commitment and ciphertext alone: what a lying close builder sends.
fn forge_recipient(pczt: pczt::Pczt, from: [u8; 43], to: [u8; 43]) -> pczt::Pczt {
    fn walk(v: &mut serde_json::Value, from: &serde_json::Value, to: &serde_json::Value, n: &mut usize) {
        match v {
            serde_json::Value::Object(m) => {
                for (k, x) in m.iter_mut() {
                    if k == "recipient" && x == from {
                        *x = to.clone();
                        *n += 1;
                    } else {
                        walk(x, from, to, n);
                    }
                }
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(|x| walk(x, from, to, n)),
            _ => {}
        }
    }
    let bytes = pczt.serialize().unwrap();
    let (head, body) = bytes.split_at(8);
    let v2: pczt::v2::Pczt = postcard::from_bytes(body).unwrap();
    let mut j = serde_json::to_value(&v2).unwrap();
    let mut n = 0;
    walk(&mut j, &serde_json::json!(from.to_vec()), &serde_json::json!(to.to_vec()), &mut n);
    assert!(n > 0, "forged a recipient");
    let v2: pczt::v2::Pczt = serde_json::from_value(j).unwrap();
    let mut out = head.to_vec();
    out.extend(postcard::to_allocvec(&v2).unwrap());
    pczt::Pczt::parse(&out).unwrap()
}

#[test]
fn close_check_binds_metadata_to_the_note() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 210;
    let (alice, bob) = (party("forge/alice", 7), party("forge/bob", 8));
    let o = oracle::NumericOracle::new("forge", "e", 12, 10);
    let terms = Terms {
        event: Event::Numeric(vec![o.announcement().clone()]),
        payouts: Payouts::StableValue(curve()),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 5,
        refund_height: tip + 50,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    let offer = alice.offer(terms.clone());
    let (accept, bob_txs) = bob.accept(&p, tip, &offer).unwrap();
    let (sign, alice_c) = alice.sign(&p, tip, &offer, &accept).unwrap();
    let (_, bob_c) = bob.finish(&offer, bob_txs, &sign).unwrap();
    let payout = terms.payout_at(600).unwrap();
    let anchor = orchard::Anchor::empty_tree();
    let agreed = bob_c.close_spec(&p, tip + 5, payout, anchor).unwrap();

    // Alice builds a close paying Bob's share to her own second address (Mallory)...
    let mallory = zdlc::regtest::dev_party("forge", "mallory", 9).into_party([9; 32], 0, 1);
    let mut evil = agreed.clone();
    for o in evil.outputs.iter_mut() {
        if let Pay::Ironwood(a, v) = o {
            if *a == bob.info.close {
                *o = Pay::Ironwood(mallory.info.close, *v);
            }
        }
    }
    let honest_looking = tx::build(&p, &evil).unwrap();
    assert!(bob_c.check_close(&honest_looking, &agreed).is_err(), "plain redirect refused");

    // ...then rewrites the PCZT metadata so the output claims to pay Bob.
    let forged = forge_recipient(
        honest_looking,
        mallory.info.close.to_raw_address_bytes(),
        bob.info.close.to_raw_address_bytes(),
    );
    let err = bob_c.check_close(&forged, &agreed).unwrap_err();
    println!("forged close: {err}");
    assert!(err.to_string().contains("commitment"), "{err}");

    // A close with another expiry than agreed is refused too.
    let mut late = alice_c.close_spec(&p, tip + 5, payout, anchor).unwrap();
    late.expiry_height += 100;
    let err = bob_c.check_close(&tx::build(&p, &late).unwrap(), &agreed).unwrap_err();
    assert!(err.to_string().contains("expiry"), "{err}");

    // The honest close passes, including Bob's trial decryption of his own output.
    let honest = tx::build(&p, &alice_c.close_spec(&p, tip + 5, payout, anchor).unwrap()).unwrap();
    bob_c.check_close(&honest, &agreed).unwrap();
    alice_c.check_close(&honest, &agreed).unwrap();
}

fn wallet_parties(tag: &str) -> (zdlc::wallet::Wallet, Party, Party) {
    use zdlc::wallet::Wallet;
    let wa = Wallet::from_seed(*blake(&format!("{tag}/alice")));
    let wb = Wallet::from_seed(*blake(&format!("{tag}/bob")));
    let coin = |w: &Wallet, b: u8| tx::Coin {
        txid: [b; 32],
        index: 0,
        value: 3_0000_0000,
        spend: tx::Spend::P2pkh(zdlc::keys::pubkey(&w.coin_key(0))),
    };
    let (a, b) = (wa.party(0, vec![coin(&wa, 11)]), wb.party(0, vec![coin(&wb, 12)]));
    (wa, a, b)
}

fn blake(s: &str) -> Box<[u8; 32]> {
    use sha2::Digest;
    Box::new(sha2::Sha256::digest(s.as_bytes()).into())
}

#[test]
fn dual_signed_contract_covers_both_eras() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 120;
    let (alice, bob) = (party("dual/alice", 13), party("dual/bob", 14));
    let mut o = oracle::NumericOracle::new("dual", "e", 12, 10);
    let mut terms = Terms {
        event: Event::Numeric(vec![o.announcement().clone()]),
        payouts: Payouts::StableValue(curve()),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: 190,
        refund_height: NU7_AT + 15,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    assert!(terms.validate(&p, tip).is_err(), "spanning refused without dual signing");
    terms.dual_sign = true;
    let offer = alice.offer(terms.clone());
    let (accept, bob_txs) = bob.accept(&p, tip, &offer).unwrap();
    let (sign, a) = alice.sign(&p, tip, &offer, &accept).unwrap();
    let (_, _b) = bob.finish(&offer, bob_txs, &sign).unwrap();
    assert_eq!(a.txs.cets.len(), 2 * 48, "one CET set per era");
    println!("dual: {} CETs, {} adaptor signatures each way", a.txs.cets.len(), a.adaptor_count());
    let refund = a.refund().unwrap();
    assert_eq!(refund.branch_id, params::BRANCH_NU7, "refund signed for the era it confirms in");
    let att = o.attest(400).unwrap();
    let (pre, _) = a.cet_numeric(&[att.clone()], 195).unwrap();
    let (post, _) = a.cet_numeric(&[att.clone()], NU7_AT + 1).unwrap();
    assert_eq!((pre.branch_id, post.branch_id), (params::BRANCH_NU6_3, params::BRANCH_NU7));
    terms.refund_height = 10_000;
    let _ = terms;
}

#[test]
fn recovery_record_rebuilds_the_refund() {
    use zdlc::recovery::{rebuild_refund, Record};
    let p = params::regtest(Some(NU7_AT));
    let tip = 210;
    let (wa, alice, bob) = wallet_parties("rec");
    let o = oracle::NumericOracle::new("rec", "e", 12, 10);
    let terms = Terms {
        event: Event::Numeric(vec![o.announcement().clone()]),
        payouts: Payouts::StableValue(curve()),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 5,
        refund_height: tip + 20,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    let offer = alice.offer(terms);
    let (accept, bob_txs) = bob.accept(&p, tip, &offer).unwrap();
    let (sign, a) = alice.sign(&p, tip, &offer, &accept).unwrap();
    let (_, b) = bob.finish(&offer, bob_txs, &sign).unwrap();
    let rec = Record::from_contract(&a, 0).unwrap();
    let bytes = rec.encode();
    assert!(bytes.len() <= 512);
    assert_eq!(Record::decode(&bytes).unwrap(), rec);
    let rebuilt = rebuild_refund(&p, &wa, &rec).unwrap();
    assert_eq!(rebuilt.txid, a.refund().unwrap().txid, "seed + record = the same refund");
    assert_eq!(rebuilt.raw, b.refund().unwrap().raw);
    println!("record: {} bytes of a 512-byte memo", bytes.len());
}
