//! Legs: a payment address unique to one contract and one funding party, payable from
//! any wallet (path B of shielded funding by QR).
//!
//! ```text
//! IF   <H> CHECKLOCKTIMEVERIFY DROP <owner> CHECKSIG    # the owner alone, from height H
//! ELSE 2 <A> <B> 2 CHECKMULTISIG                         # both parties, any time
//! ENDIF
//! ```
//!
//! `A` and `B` are the contract's funding keys and `H` its refund height, so a leg the
//! contract never spends goes back to its owner exactly when the refund would. The
//! funding transaction spends every leg by the 2-of-2 branch, after both parties hold
//! every CET and the refund, so everything downstream of funding is unchanged.
//!
//! The redeem script is not one pczt's Signer or Spend Finalizer recognise: zdlc signs
//! the input's sighash itself and assembles the scriptSig.

use secp256k1::{ecdsa, Message, PublicKey, SecretKey};
use zcash_script::script::{Code, FromChain};
use zcash_transparent::address::TransparentAddress;

use crate::{
    script,
    tx::{self, Coin, Pay, Spec, Spend},
    Error, Result,
};

const OP_0: u8 = 0x00;
const OP_1: u8 = 0x51;
const OP_2: u8 = 0x52;
const OP_IF: u8 = 0x63;
const OP_ELSE: u8 = 0x67;
const OP_ENDIF: u8 = 0x68;
const OP_DROP: u8 = 0x75;
const OP_CHECKSIG: u8 = 0xac;
const OP_CHECKMULTISIG: u8 = 0xae;
const OP_CHECKLOCKTIMEVERIFY: u8 = 0xb1;
const SIGHASH_ALL: u8 = 0x01;
/// A DER signature with its sighash byte, at most.
const MAX_SIG: usize = 73;

#[derive(Clone, Debug, PartialEq)]
pub struct Leg {
    /// Who may reclaim the leg alone from `reclaim_height`.
    pub owner: PublicKey,
    /// The contract's funding keys, offer then accept.
    pub a: PublicKey,
    pub b: PublicKey,
    /// The contract's refund height.
    pub reclaim_height: u32,
}

fn push(out: &mut Vec<u8>, data: &[u8]) {
    match data.len() {
        n if n < 0x4c => out.push(n as u8),
        n if n <= 0xff => out.extend([0x4c, n as u8]),
        n => out.extend([0x4d, n as u8, (n >> 8) as u8]),
    }
    out.extend(data);
}

/// Minimal CScriptNum encoding of a positive height.
fn script_num(mut n: u32) -> Vec<u8> {
    let mut v = vec![];
    while n > 0 {
        v.push((n & 0xff) as u8);
        n >>= 8;
    }
    if v.last().is_some_and(|b| b & 0x80 != 0) {
        v.push(0);
    }
    v
}

fn der(sig: &ecdsa::Signature) -> Vec<u8> {
    let mut v = sig.serialize_der().to_vec();
    v.push(SIGHASH_ALL);
    v
}

impl Leg {
    pub fn redeem_bytes(&self) -> Vec<u8> {
        assert!(self.reclaim_height > 16, "a height of at least 17");
        let mut s = vec![OP_IF];
        push(&mut s, &script_num(self.reclaim_height));
        s.extend([OP_CHECKLOCKTIMEVERIFY, OP_DROP]);
        push(&mut s, &self.owner.serialize());
        s.extend([OP_CHECKSIG, OP_ELSE, OP_2]);
        push(&mut s, &self.a.serialize());
        push(&mut s, &self.b.serialize());
        s.extend([OP_2, OP_CHECKMULTISIG, OP_ENDIF]);
        s
    }

    pub fn redeem(&self) -> FromChain {
        FromChain::parse(&Code(self.redeem_bytes())).expect("well-formed script")
    }

    pub fn address(&self) -> TransparentAddress {
        script::p2sh(&self.redeem())
    }

    /// scriptSig of the 2-of-2 branch: `0 <sig a> <sig b> 0 <redeem>`.
    pub fn joint_script_sig(&self, sig_a: &ecdsa::Signature, sig_b: &ecdsa::Signature) -> Vec<u8> {
        let mut s = vec![OP_0];
        push(&mut s, &der(sig_a));
        push(&mut s, &der(sig_b));
        s.push(OP_0);
        push(&mut s, &self.redeem_bytes());
        s
    }

    /// scriptSig of the owner's branch: `<sig> 1 <redeem>`.
    pub fn owner_script_sig(&self, sig: &ecdsa::Signature) -> Vec<u8> {
        let mut s = vec![];
        push(&mut s, &der(sig));
        s.push(OP_1);
        push(&mut s, &self.redeem_bytes());
        s
    }

    /// The serialized size of an input spending the leg (the larger branch), for ZIP 317.
    pub fn input_size(&self) -> usize {
        let script_sig = 1 + 2 * (1 + MAX_SIG) + 1 + 2 + self.redeem_bytes().len();
        let compact = if script_sig < 0xfd { 1 } else { 3 };
        36 + compact + script_sig + 4
    }

    pub fn coin(&self, txid: [u8; 32], index: u32, value: u64) -> Coin {
        Coin { txid, index, value, spend: Spend::Leg(self.clone()) }
    }
}

/// Signs leg input `index` of `pczt` with `sk` (the owner's key, or a funding key).
pub fn sign(pczt: &pczt::Pczt, index: usize, sk: &SecretKey) -> Result<ecdsa::Signature> {
    Ok(ecdsa::sign(Message::from_digest(tx::sighash(pczt, index)?), sk))
}

/// The owner's reclaim from `reclaim_height`: every coin of the leg to `to`, less the fee.
pub fn reclaim(
    p: &zcash_protocol::local_consensus::LocalNetwork,
    tip: u32,
    leg: &Leg,
    coins: &[Coin],
    to: TransparentAddress,
    owner: &SecretKey,
) -> Result<tx::Final> {
    if PublicKey::from_secret_key(owner) != leg.owner {
        return Err(Error::Build("not this leg's owner key".into()));
    }
    let target = tip.max(leg.reclaim_height) + 1;
    let total: u64 = coins.iter().map(|c| c.value).sum();
    let mut spec = Spec {
        target_height: target,
        expiry_height: 0,
        lock_height: Some(leg.reclaim_height),
        inputs: coins.to_vec(),
        outputs: vec![Pay::Transparent(to, total)],
        ironwood_anchor: None,
        ironwood_spends: vec![],
    };
    let fee = tx::fee(p, &spec)?;
    spec.outputs = vec![Pay::Transparent(to, total.checked_sub(fee).ok_or(Error::Build("leg below the fee".into()))?)];
    let pczt = tx::build(p, &spec)?;
    let mut sigs = vec![];
    for i in 0..coins.len() {
        sigs.push((i, leg.owner_script_sig(&sign(&pczt, i, owner)?)));
    }
    tx::extract_with_script_sigs(pczt, sigs.into_iter().map(|(_, s)| s).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_shape() {
        let k = |b: u8| PublicKey::from_secret_key(&SecretKey::from_secret_bytes([b; 32]).unwrap());
        let leg = Leg { owner: k(1), a: k(2), b: k(3), reclaim_height: 300 };
        let r = leg.redeem_bytes();
        assert_eq!(r.len(), 1 + 3 + 2 + 34 + 3 + 34 + 34 + 3);
        assert_eq!(&r[..4], &[OP_IF, 2, 0x2c, 0x01]);
        assert_eq!(script_num(128), vec![0x80, 0x00]);
        assert_eq!(script_num(4_522_181), vec![0xc5, 0x00, 0x45]);
        assert!(matches!(leg.address(), TransparentAddress::ScriptHash(_)));
        assert!(leg.input_size() > 250 && leg.input_size() < 320);
    }
}
