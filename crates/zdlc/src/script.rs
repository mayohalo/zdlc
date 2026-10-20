//! The contract's 2-of-2 redeem script and its P2SH address.

use secp256k1::PublicKey;
use zcash_script::{
    pattern::check_multisig,
    script::{Component, Evaluable, FromChain},
};
use zcash_transparent::{address::TransparentAddress, util::hash160};

use crate::{Error, Result};

/// `2 <a> <b> 2 CHECKMULTISIG`, keys in offer/accept order.
pub fn multisig_2of2(a: &PublicKey, b: &PublicKey) -> Result<FromChain> {
    let ops = check_multisig(2, &[&a.serialize(), &b.serialize()], false)
        .map_err(|e| Error::Build(format!("multisig script: {e:?}")))?;
    Ok(Component(ops).weaken())
}

pub fn p2sh(redeem: &FromChain) -> TransparentAddress {
    TransparentAddress::ScriptHash(hash160::hash(&redeem.to_bytes()))
}
