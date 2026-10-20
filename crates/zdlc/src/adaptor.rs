//! ECDSA adaptor signatures on a v6 transparent sighash (secp256k1-zkp).
//!
//! A CET's counterparty signature is encrypted to the oracle's attestation point `S_m`;
//! the oracle's scalar `s_m` decrypts it into an ordinary ECDSA signature. Zcash and the
//! pczt Signer refuse high-S signatures; libsecp256k1-zkp's decrypt already returns
//! low-S (Phase 0 measured 0 of 64 high), and `decrypt` normalizes again regardless.

use secp256k1_zkp::{ecdsa, EcdsaAdaptorSignature, Message, PublicKey, Secp256k1, SecretKey};

use crate::{Error, Result};

pub use secp256k1_zkp::EcdsaAdaptorSignature as AdaptorSignature;

/// Bytes of a secp256k1 0.33 secret key as a secp256k1-zkp key.
pub fn zkp_secret(sk: &secp256k1::SecretKey) -> SecretKey {
    SecretKey::from_slice(&sk.to_secret_bytes()).expect("same curve, same range")
}

pub fn zkp_public(pk: &secp256k1::PublicKey) -> PublicKey {
    PublicKey::from_slice(&pk.serialize()).expect("same encoding")
}

/// Adaptor-signs `sighash` with `sk`, encrypted to `point`.
pub fn encrypt(
    sk: &secp256k1::SecretKey,
    sighash: [u8; 32],
    point: &PublicKey,
) -> EcdsaAdaptorSignature {
    let secp = Secp256k1::signing_only();
    let mut aux = [0u8; 32];
    rand10::fill(&mut aux);
    EcdsaAdaptorSignature::encrypt_with_aux_rand(
        &secp,
        &Message::from_digest(sighash),
        &zkp_secret(sk),
        point,
        &aux,
    )
}

/// Checks an adaptor signature before funding: it must decrypt to a valid signature by
/// `pubkey` on `sighash` once the scalar of `point` is known.
pub fn verify(
    adaptor: &EcdsaAdaptorSignature,
    sighash: [u8; 32],
    pubkey: &secp256k1::PublicKey,
    point: &PublicKey,
) -> Result<()> {
    adaptor
        .verify(
            &Secp256k1::verification_only(),
            &Message::from_digest(sighash),
            &zkp_public(pubkey),
            point,
        )
        .map_err(|e| Error::Crypto(format!("adaptor verify: {e}")))
}

pub struct Decrypted {
    /// Low-S, ready for pczt (secp256k1 0.33).
    pub signature: secp256k1::ecdsa::Signature,
    /// Whether decryption produced a high-S signature that had to be normalized.
    pub was_high_s: bool,
}

/// Decrypts with the oracle's attestation scalar and normalizes to low-S.
pub fn decrypt(adaptor: &EcdsaAdaptorSignature, s: &SecretKey) -> Result<Decrypted> {
    let raw = adaptor
        .decrypt(s)
        .map_err(|e| Error::Crypto(format!("adaptor decrypt: {e}")))?;
    let mut low = raw;
    low.normalize_s();
    let was_high_s = low != raw;
    Ok(Decrypted {
        signature: to_v033(&low)?,
        was_high_s,
    })
}

pub fn to_v033(sig: &ecdsa::Signature) -> Result<secp256k1::ecdsa::Signature> {
    secp256k1::ecdsa::Signature::from_compact(&sig.serialize_compact())
        .map_err(|e| Error::Crypto(format!("signature bytes: {e}")))
}

/// The same signature with `s` replaced by `n - s` (its high-S twin). For tests.
pub fn high_s_twin(sig: &secp256k1::ecdsa::Signature) -> [u8; 64] {
    const N: [u8; 32] = [
        0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
        0xFE, 0xBA, 0xAE, 0xDC, 0xE6, 0xAF, 0x48, 0xA0, 0x3B, 0xBF, 0xD2, 0x5E, 0x8C, 0xD0, 0x36,
        0x41, 0x41,
    ];
    let c = sig.serialize_compact();
    let mut out = c;
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let d = N[i] as i16 - c[32 + i] as i16 - borrow;
        out[32 + i] = d.rem_euclid(256) as u8;
        borrow = if d < 0 { 1 } else { 0 };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{keys, oracle};

    #[test]
    fn adaptor_round_trip_and_low_s() {
        let sk = keys::dev_key("adaptor-test");
        let pk = keys::pubkey(&sk);
        let mut high = 0;
        for i in 0..64 {
            let mut o = oracle::DevOracle::new("adaptor", &format!("e{i}"), &["a", "b"]);
            let point = oracle::attestation_point(o.announcement(), "a").unwrap();
            let sighash = [i as u8; 32];
            let ad = encrypt(&sk, sighash, &point);
            verify(&ad, sighash, &pk, &point).unwrap();
            let wrong = oracle::attestation_point(o.announcement(), "b").unwrap();
            assert!(verify(&ad, sighash, &pk, &wrong).is_err());
            let d = decrypt(&ad, &o.attest("a").unwrap()).unwrap();
            high += d.was_high_s as u32;
            secp256k1::ecdsa::verify(
                &d.signature,
                secp256k1::Message::from_digest(sighash),
                &pk,
            )
            .unwrap();
        }
        // libsecp256k1-zkp normalizes inside decrypt: none come out high-S.
        assert_eq!(high, 0, "high-S count {high}/64");
    }
}
