//! Default contracts build byte-identical transactions across changes to the payout model:
//! the digests are of the funding txid, every CET sighash and the refund sighash as built
//! before beneficiaries existed (commit 0ceda8c).

use sha2::{Digest, Sha256};
use zdlc::{
    contract::{build_txs, Event, Payouts, Splits, Terms},
    oracle,
    params::{self, NU7_AT},
    payout::StableValue,
};

fn digest(terms: &Terms, a: &str, b: &str, fund_b: bool) -> String {
    let p = params::regtest(Some(NU7_AT));
    let alice = zdlc::regtest::dev_party("golden/a", a, 1).into_party([1; 32], 0, 3_0000_0000);
    let mut bob = zdlc::regtest::dev_party("golden/b", b, 2).into_party([2; 32], 0, 3_0000_0000);
    if !fund_b {
        bob.info.inputs.clear();
    }
    let txs = build_txs(&p, terms, &alice.info, &bob.info).unwrap();
    let mut h = Sha256::new();
    h.update(txs.funding_txid);
    for c in &txs.cets {
        h.update(c.2);
    }
    h.update(txs.refund_sighash);
    hex::encode(h.finalize())
}

#[test]
fn default_contracts_unchanged() {
    let tip = 120;
    let o = oracle::DevOracle::new("golden", "m", &["alice", "bob", "draw", "dust"]);
    let base = |payouts, oc, ac| Terms {
        event: Event::Enumerated(vec![o.announcement().clone()]),
        payouts,
        offer_collateral: oc,
        accept_collateral: ac,
        maturity: tip + 10,
        refund_height: tip + 30,
        sign_height: tip + 1,
        dual_sign: false,
        roll: None,
        splits: Splits::default(),
    };
    let c = 2_0000_0000;
    let e = base(
        Payouts::Enumerated(vec![("alice".into(), 2 * c), ("bob".into(), 0), ("draw".into(), c), ("dust".into(), 30)]),
        c,
        c,
    );
    assert_eq!(digest(&e, "alice", "bob", true), "744fea8e6873d1b0cd5fa514892d67d5977f00396a633f1b6e684e41264bde74", "enumerated");
    let s = base(Payouts::Enumerated(vec![("alice".into(), 0), ("bob".into(), c)]), c, 0);
    assert_eq!(digest(&s, "alice", "bob", false), "71fe1c23100e33fe5f40f5d6af97e50c95bfeb6194e836c26138a406b931151a", "single");
    let n = oracle::NumericOracle::new("golden", "zecusd", 12, 10);
    let mut v = base(
        Payouts::StableValue(StableValue {
            d_cents: 100_00,
            q_recipient: c,
            q_hedger: c,
            unit_cents: 10,
            nb_digits: 12,
            rounding: 800_0000,
            premium: 0,
        }),
        c,
        c,
    );
    v.event = Event::Numeric(vec![n.announcement().clone()]);
    assert_eq!(digest(&v, "alice", "bob", true), "3cc29e4f5cbefaaf3a97cecde0519ede4efc0d73bc87557d63bb8776fd09759d", "stable");
}
