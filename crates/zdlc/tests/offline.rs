//! Phase 0 offline: the whole contract through the pczt roles, no node.

mod common;

use common::*;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::consensus::BranchId;
use zdlc::{
    adaptor, keys, oracle,
    params::{self, BRANCH_NU6_3, BRANCH_NU7, NU7_AT},
    tx::{self, SEQUENCE_LOCKTIME},
};

const FUNDING: u64 = 2_0000_0000;

fn parse(raw: &[u8], branch: u32) -> Transaction {
    Transaction::read(raw, BranchId::try_from(branch).unwrap()).unwrap()
}

#[test]
fn refund_carries_lock_time_and_non_final_sequence() {
    let p = params::regtest(Some(NU7_AT));
    let parties = Parties::new("offline-refund");
    let coin = parties.funding_coin([1; 32], 0, FUNDING);
    let a = keys::p2pkh(&keys::pubkey(&parties.a));
    let b = keys::p2pkh(&keys::pubkey(&parties.b));
    let spec = split(&p, 120, 0, Some(150), vec![coin], &[(a, 500), (b, 500)]);
    let f = tx::extract(cosign(tx::build(&p, &spec).unwrap(), &parties)).unwrap();
    assert_eq!(f.lock_time, 150);
    assert_eq!(f.expiry_height, 0);
    assert_eq!(f.branch_id, BRANCH_NU6_3);
    let t = parse(&f.raw, f.branch_id);
    let vin = &t.transparent_bundle().unwrap().vin;
    assert_eq!(vin[0].sequence(), SEQUENCE_LOCKTIME);
    println!("refund: {} bytes, nLockTime {}, nSequence {:#x}", f.raw.len(), f.lock_time, vin[0].sequence());
}

#[test]
fn adaptor_cet_completes_under_both_branches() {
    let p = params::regtest(Some(NU7_AT));
    let parties = Parties::new("offline-cet");
    let coin = parties.funding_coin([2; 32], 0, FUNDING);
    let a = keys::p2pkh(&keys::pubkey(&parties.a));
    let b = keys::p2pkh(&keys::pubkey(&parties.b));
    for (target, branch) in [(150, BRANCH_NU6_3), (NU7_AT, BRANCH_NU7)] {
        let mut o = oracle::DevOracle::new("offline", &format!("coin-{target}"), &["heads", "tails"]);
        let ann = o.announcement().clone();
        let specs = vec![
            ("heads".into(), split(&p, target, 0, None, vec![coin.clone()], &[(a, 700), (b, 300)])),
            ("tails".into(), split(&p, target, 0, None, vec![coin.clone()], &[(a, 300), (b, 700)])),
        ];
        let cets = presign_cets(&parties, &ann, specs, &p);
        let s = o.attest("heads").unwrap();
        let (f, high) = complete_cet(&cets[0], &parties, &s);
        assert_eq!(f.branch_id, branch);
        assert_eq!(f.expiry_height, 0);
        println!("CET heads @target {target}: branch {:08x}, {} bytes, decrypted high-S: {high}", f.branch_id, f.raw.len());
        // The tails CET cannot be completed with the heads scalar.
        let d = adaptor::decrypt(&cets[1].bob_adaptor, &s).unwrap();
        assert!(tx::append(cets[1].pczt.clone(), 0, d.signature).is_err(), "wrong-outcome signature refused");
    }
}

/// What pczt's `append_transparent_signature` checks.
#[test]
fn append_checks_signature_and_low_s() {
    let p = params::regtest(Some(NU7_AT));
    let parties = Parties::new("offline-append");
    let coin = parties.funding_coin([3; 32], 0, FUNDING);
    let a = keys::p2pkh(&keys::pubkey(&parties.a));
    let spec = split(&p, 120, 0, None, vec![coin], &[(a, 1000)]);
    let pczt = tx::build(&p, &spec).unwrap();
    let sighash = tx::sighash(&pczt, 0).unwrap();
    let msg = secp256k1::Message::from_digest(sighash);
    let good = secp256k1::ecdsa::sign(msg, &parties.b);

    // A signature by a key outside the redeem script.
    let stranger = secp256k1::ecdsa::sign(msg, &keys::dev_key("stranger"));
    let r_stranger = tx::append(pczt.clone(), 0, stranger);
    // A signature on another message by a script key.
    let other = secp256k1::ecdsa::sign(secp256k1::Message::from_digest([9; 32]), &parties.b);
    let r_other = tx::append(pczt.clone(), 0, other);
    // The high-S twin of a valid signature.
    let twin = secp256k1::ecdsa::Signature::from_compact(&adaptor::high_s_twin(&good)).unwrap();
    let r_high = tx::append(pczt.clone(), 0, twin);
    let r_good = tx::append(pczt.clone(), 0, good);
    println!("append stranger: {:?}", r_stranger.as_ref().err());
    println!("append wrong msg: {:?}", r_other.as_ref().err());
    println!("append high-S twin: {:?}", r_high.as_ref().err());
    assert!(r_stranger.is_err());
    assert!(r_other.is_err());
    assert!(r_high.is_err(), "high-S is refused, so normalization is required");
    assert!(r_good.is_ok());
}

#[test]
fn cooperative_close_pays_ironwood() {
    let p = params::regtest(Some(NU7_AT));
    let parties = Parties::new("offline-close");
    let coin = parties.funding_coin([4; 32], 0, FUNDING);
    for target in [150, NU7_AT] {
        let (spec, _) = close_to_ironwood(&p, target, target + 20, vec![coin.clone()], orchard::Anchor::empty_tree());
        let t0 = std::time::Instant::now();
        let pczt = tx::build(&p, &spec).unwrap();
        let built = t0.elapsed();
        let f = tx::extract(cosign(pczt, &parties)).unwrap();
        let t = parse(&f.raw, f.branch_id);
        assert!(t.transparent_bundle().unwrap().vout.is_empty());
        let actions = t.ironwood_bundle().map(|b| b.actions().len()).unwrap_or(0);
        assert!(actions >= 2);
        println!("close @target {target}: branch {:08x}, {} Ironwood actions, {} bytes, build+prove {:?}", f.branch_id, actions, f.raw.len(), built);
    }
}
