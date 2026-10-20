//! Deterministic dev keys and transparent addresses (regtest only).

use secp256k1::{PublicKey, SecretKey};
use sha2::{Digest, Sha256};
use zcash_address::{ConversionError, ToAddress, TryFromAddress, ZcashAddress};
use zcash_protocol::consensus::NetworkType;
use zcash_transparent::address::TransparentAddress;

/// A secp256k1 key derived from a label. For regtest and tests only.
pub fn dev_key(label: &str) -> SecretKey {
    let mut h: [u8; 32] = Sha256::digest(format!("zdlc-dev/{label}").as_bytes()).into();
    loop {
        if let Ok(sk) = SecretKey::from_secret_bytes(h) {
            return sk;
        }
        h = Sha256::digest(h).into();
    }
}

pub fn pubkey(sk: &SecretKey) -> PublicKey {
    PublicKey::from_secret_key(sk)
}

pub fn p2pkh(pk: &PublicKey) -> TransparentAddress {
    TransparentAddress::from_pubkey(pk)
}

/// Encodes a transparent address for a network (`t1…`, or `tm…` on testnet and regtest).
pub fn encode(addr: &TransparentAddress, net: NetworkType) -> String {
    match addr {
        TransparentAddress::PublicKeyHash(h) => ZcashAddress::from_transparent_p2pkh(net, *h).encode(),
        TransparentAddress::ScriptHash(h) => ZcashAddress::from_transparent_p2sh(net, *h).encode(),
    }
}

struct Decoded(NetworkType, TransparentAddress);

impl TryFromAddress for Decoded {
    type Error = ();

    fn try_from_transparent_p2pkh(net: NetworkType, data: [u8; 20]) -> Result<Self, ConversionError<()>> {
        Ok(Decoded(net, TransparentAddress::PublicKeyHash(data)))
    }

    fn try_from_transparent_p2sh(net: NetworkType, data: [u8; 20]) -> Result<Self, ConversionError<()>> {
        Ok(Decoded(net, TransparentAddress::ScriptHash(data)))
    }
}

/// Decodes a transparent address (`t1…`, `t3…`, `tm…`, `t2…`) and its network.
pub fn decode(s: &str) -> Option<(NetworkType, TransparentAddress)> {
    let d: Decoded = ZcashAddress::try_from_encoded(s).ok()?.convert().ok()?;
    Some((d.0, d.1))
}

/// Encodes a transparent address for regtest (`tm…` / `t2…`).
pub fn encode_regtest(addr: &TransparentAddress) -> String {
    encode(addr, NetworkType::Regtest)
}

/// The regtest miner key: `infra/zakura/regtest.toml` pays coinbase to its P2PKH address.
pub fn miner() -> SecretKey {
    dev_key("miner")
}
