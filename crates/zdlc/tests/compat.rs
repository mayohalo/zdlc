//! dlcspecs oracle compatibility, against kormir (DLC Dev Kit's oracle, 1.1.2).
//!
//! - kormir -> zdlc: a zdlc contract built on a kormir announcement settles on kormir's
//!   attestation (BTC/USD numeric event, and an enumerated event).
//! - zdlc -> dlcspecs: an attestation from zdlc's oracle validates as a dlcspecs
//!   `OracleAttestation` against a dlcspecs announcement of the same event.
//! - mixed 2-of-2: one kormir oracle and one zdlc oracle settle one contract together.

use kormir::{storage::MemoryStorage, Oracle, OracleAnnouncement, OracleAttestation};
use secp256k1_zkp::{Keypair, Secp256k1, SecretKey};
use zdlc::{
    contract::{Event, Party, Payouts, Terms},
    oracle::{self, NumericAnnouncement, NumericAttestation, NumericOracle},
    params::{self, NU7_AT},
    payout::StableValue,
    regtest::dev_party,
};

const COLLATERAL: u64 = 2_0000_0000;
const DIGITS: usize = 17; // BTC/USD in whole dollars, up to $131,071

fn parties(tag: &str) -> (Party, Party) {
    (
        dev_party(tag, "alice", 21).into_party([21; 32], 0, 3_0000_0000),
        dev_party(tag, "bob", 22).into_party([22; 32], 0, 3_0000_0000),
    )
}

/// A BTC-referenced stable-value curve: Alice is owed $120,000 worth of the pot.
fn btc_curve() -> StableValue {
    StableValue {
        d_cents: 12_000_000,
        q_recipient: COLLATERAL,
        q_hedger: COLLATERAL,
        unit_cents: 100,
        nb_digits: DIGITS,
        rounding: 2_000_0000, // 5% of the pot: a coarse curve keeps the test fast
    }
}

fn from_kormir_announcement(a: &OracleAnnouncement) -> NumericAnnouncement {
    NumericAnnouncement {
        pubkey: a.oracle_public_key,
        nonces: a.oracle_event.oracle_nonces.clone(),
        event_id: a.oracle_event.event_id.clone(),
        nb_digits: a.oracle_event.oracle_nonces.len(),
        unit_cents: 100,
    }
}

fn from_kormir_attestation(a: &OracleAttestation, value: usize) -> NumericAttestation {
    NumericAttestation {
        event_id: a.event_id.clone(),
        value,
        digits: a.outcomes.iter().map(|d| d.parse().unwrap()).collect(),
        signatures: a.signatures.clone(),
        scalars: a
            .signatures
            .iter()
            .map(|s| SecretKey::from_slice(dlc::secp_utils::schnorrsig_decompose(s).unwrap().1).unwrap())
            .collect(),
    }
}

fn kormir_oracle(seed: u8) -> Oracle<MemoryStorage> {
    Oracle::from_signing_key(MemoryStorage::new(), SecretKey::from_slice(&[seed; 32]).unwrap()).unwrap()
}

fn terms(event: Event, tip: u32) -> Terms {
    Terms {
        event,
        payouts: Payouts::StableValue(btc_curve()),
        offer_collateral: COLLATERAL,
        accept_collateral: COLLATERAL,
        maturity: tip + 5,
        refund_height: tip + 50,
        sign_height: tip + 1,
        dual_sign: false,
    }
}

#[tokio::test]
async fn zdlc_contract_settles_on_a_kormir_attestation() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 210;
    let k = kormir_oracle(7);
    let ann = k
        .create_numeric_event("btcusd-2026-10-21".into(), DIGITS as u16, false, 0, "usd/btc".into(), 1_792_540_800)
        .await
        .unwrap();
    let (alice, bob) = parties("compat-k");
    let t = terms(Event::Numeric(vec![from_kormir_announcement(&ann)]), tip);
    let offer = alice.offer(t.clone());
    let (accept, bob_txs) = bob.accept(&p, tip, &offer).unwrap();
    let (sign, a) = alice.sign(&p, tip, &offer, &accept).unwrap();
    let (_, b) = bob.finish(&offer, bob_txs, &sign).unwrap();

    let price = 96_000;
    let att = k.sign_numeric_event("btcusd-2026-10-21".into(), price as i64).await.unwrap();
    att.validate(&Secp256k1::verification_only(), &ann).unwrap();
    let z = from_kormir_attestation(&att, price);
    let (cet, i) = a.cet_numeric(&[z.clone()], tip + 6).unwrap();
    let (cet_b, _) = b.cet_numeric(&[z], tip + 6).unwrap();
    assert_eq!(cet.txid, cet_b.txid);
    let (o, ac) = t.payout_at(price).unwrap();
    assert_eq!((a.txs.cets[i].0.offer, a.txs.cets[i].0.accept), (o, ac));
    println!(
        "kormir BTC/USD ${price}: CET {i} of {} pays Alice {o} zat, Bob {ac} zat ({} adaptor signatures)",
        a.txs.cets.len(),
        a.adaptor_count()
    );
}

#[tokio::test]
async fn enumerated_contract_settles_on_a_kormir_attestation() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 210;
    let k = kormir_oracle(8);
    let ann = k
        .create_enum_event("match-1".into(), vec!["alice".into(), "bob".into()], 1_792_540_800)
        .await
        .unwrap();
    let z = oracle::Announcement {
        pubkey: ann.oracle_public_key,
        nonce: ann.oracle_event.oracle_nonces[0],
        event_id: "match-1".into(),
        outcomes: vec!["alice".into(), "bob".into()],
    };
    let (alice, bob) = parties("compat-e");
    let mut t = terms(Event::Enumerated(vec![z]), tip);
    t.payouts = Payouts::Enumerated(vec![("alice".into(), 2 * COLLATERAL), ("bob".into(), 0)]);
    let offer = alice.offer(t);
    let (accept, bob_txs) = bob.accept(&p, tip, &offer).unwrap();
    let (sign, a) = alice.sign(&p, tip, &offer, &accept).unwrap();
    let _ = bob.finish(&offer, bob_txs, &sign).unwrap();
    let att = k.sign_enum_event("match-1".into(), "alice".into()).await.unwrap();
    let s = SecretKey::from_slice(dlc::secp_utils::schnorrsig_decompose(&att.signatures[0]).unwrap().1).unwrap();
    a.cet_outcome("alice", &[s], tip + 6).unwrap();
}

#[tokio::test]
async fn zdlc_oracle_attestation_is_valid_dlcspecs() {
    let secp = Secp256k1::new();
    let mut o = NumericOracle::new("compat", "btcusd-z", DIGITS, 100);
    let a = o.announcement().clone();
    let keypair = Keypair::from_secret_key(&secp, &oracle::zkp_key("oracle/compat"));
    assert_eq!(keypair.x_only_public_key().0, a.pubkey);
    let spec_ann = kormir::create_numeric_event(
        &secp, &keypair, "btcusd-z", 2, DIGITS as u16, false, 0, "usd/btc", 1_792_540_800, &a.nonces,
    )
    .unwrap();
    spec_ann.validate(&secp).unwrap();
    let att = o.attest(96_000).unwrap();
    let spec_att = OracleAttestation {
        event_id: "btcusd-z".into(),
        oracle_public_key: a.pubkey,
        signatures: att.signatures.clone(),
        outcomes: att.digits.iter().map(|d| d.to_string()).collect(),
    };
    spec_att.validate(&secp, &spec_ann).expect("zdlc attestation validates under dlcspecs rules");
}

#[tokio::test]
async fn mixed_two_of_two_kormir_and_zdlc() {
    let p = params::regtest(Some(NU7_AT));
    let tip = 210;
    let k = kormir_oracle(9);
    let kann = k
        .create_numeric_event("btcusd-mixed".into(), DIGITS as u16, false, 0, "usd/btc".into(), 1_792_540_800)
        .await
        .unwrap();
    let mut z = NumericOracle::new("mixed", "btcusd-mixed", DIGITS, 100);
    let event = Event::Numeric(vec![from_kormir_announcement(&kann), z.announcement().clone()]);
    let (alice, bob) = parties("compat-m");
    let offer = alice.offer(terms(event, tip));
    let (accept, bob_txs) = bob.accept(&p, tip, &offer).unwrap();
    let (sign, a) = alice.sign(&p, tip, &offer, &accept).unwrap();
    let _ = bob.finish(&offer, bob_txs, &sign).unwrap();
    let ka = from_kormir_attestation(&k.sign_numeric_event("btcusd-mixed".into(), 96_000).await.unwrap(), 96_000);
    // One oracle alone opens nothing.
    assert!(a.cet_numeric(&[ka.clone()], tip + 6).is_err());
    // Disagreeing oracles open nothing.
    let mut z2 = NumericOracle::new("mixed2", "x", DIGITS, 100);
    let wrong = z2.attest(95_000).unwrap();
    assert!(a.cet_numeric(&[ka.clone(), wrong], tip + 6).is_err());
    // Both agreeing: the CET opens.
    let za = z.attest(96_000).unwrap();
    a.cet_numeric(&[ka, za], tip + 6).unwrap();
}
