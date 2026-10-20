//! Phase 1 offline: offer, accept, sign, finish between two parties, then settlement.

use std::time::Instant;

use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use zdlc::{
    contract::{Event, Party, PartyInfo, Payouts, Terms},
    keys, oracle,
    params::{self, NU7_AT},
    payout::StableValue,
    tx::{self, Coin, Spend},
};

const COLLATERAL: u64 = 2_0000_0000;

fn party(tag: &str, coin_txid: u8) -> Party {
    let funding_sk = keys::dev_key(&format!("{tag}/funding"));
    let input_sk = keys::dev_key(&format!("{tag}/input"));
    let close = FullViewingKey::from(&SpendingKey::from_bytes([coin_txid; 32]).unwrap())
        .address_at(0u32, Scope::External);
    Party {
        funding_sk,
        input_keys: vec![input_sk],
        info: PartyInfo {
            funding_pubkey: keys::pubkey(&funding_sk),
            inputs: vec![Coin {
                txid: [coin_txid; 32],
                index: 0,
                value: 3_0000_0000,
                spend: Spend::P2pkh(keys::pubkey(&input_sk)),
            }],
            change: keys::p2pkh(&keys::pubkey(&input_sk)),
            payout: keys::p2pkh(&keys::pubkey(&keys::dev_key(&format!("{tag}/payout")))),
            close,
        },
    }
}

pub fn curve() -> StableValue {
    StableValue {
        d_cents: 100_00,
        q_recipient: COLLATERAL,
        q_hedger: COLLATERAL,
        unit_cents: 10,
        nb_digits: 12,
        rounding: 800_0000,
    }
}

#[test]
fn enumerated_contract_end_to_end() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 120;
    let (alice, bob) = (party("enum/alice", 1), party("enum/bob", 2));
    let mut o = oracle::DevOracle::new("enum", "match-1", &["alice", "bob", "draw"]);
    let terms = Terms {
        event: Event::Enumerated(o.announcement().clone()),
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
    let cet = bob_c.cet_outcome("draw", &s).unwrap();
    let cet_a = alice_c.cet_outcome("draw", &s).unwrap();
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
        event: Event::Numeric(o.announcement().clone()),
        payouts: Payouts::StableValue(curve()),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 50,
        refund_height: tip + 100,
        sign_height: tip + 1,
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
    let (cet, i) = alice_c.cet_numeric(&att).unwrap();
    let (cet_b, j) = bob_c.cet_numeric(&att).unwrap();
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
        event: Event::Numeric(o.announcement().clone()),
        payouts: Payouts::StableValue(curve()),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: 150,
        refund_height: NU7_AT + 10,
        sign_height: 121,
    };
    let err = bob.accept(&p, 120, &alice.offer(terms.clone())).err().unwrap();
    println!("guard: {err}");
    assert!(err.to_string().contains("spans a network upgrade"));
    terms.refund_height = NU7_AT - 1;
    assert!(terms.validate(&p, 120).is_ok());
}
