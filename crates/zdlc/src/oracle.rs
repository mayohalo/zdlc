//! A dev BIP-340 oracle: one key, one nonce per event, one attestation per event.
//!
//! The attestation point for outcome `m` is `S_m = R + e·P` with
//! `e = H_BIP340/challenge(R_x || P_x || m)`; attesting publishes `s_m = k + e·x`, the
//! BIP-340 signature scalar, and `s_m·G = S_m`. The message is SHA-256 of the outcome
//! string. This matches dlcspecs' math but not yet its message tagging (checked against a
//! live dlcspecs oracle in Phase 3).

use secp256k1_zkp::{
    schnorr, Keypair, Message, Parity, PublicKey, Scalar, Secp256k1, SecretKey, XOnlyPublicKey,
};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// What the oracle publishes before the event.
#[derive(Clone, Debug)]
pub struct Announcement {
    pub pubkey: XOnlyPublicKey,
    pub nonce: XOnlyPublicKey,
    pub event_id: String,
    pub outcomes: Vec<String>,
}

pub struct DevOracle {
    secp: Secp256k1<secp256k1_zkp::All>,
    x: SecretKey,
    k: SecretKey,
    announcement: Announcement,
    attested: bool,
}

fn even(secp: &Secp256k1<secp256k1_zkp::All>, sk: SecretKey) -> SecretKey {
    let (_, parity) = sk.x_only_public_key(secp);
    if parity == Parity::Odd {
        sk.negate()
    } else {
        sk
    }
}

fn tagged(tag: &str, parts: &[&[u8]]) -> [u8; 32] {
    let t = Sha256::digest(tag.as_bytes());
    let mut h = Sha256::new();
    h.update(t);
    h.update(t);
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn outcome_message(outcome: &str) -> [u8; 32] {
    Sha256::digest(outcome.as_bytes()).into()
}

fn challenge(a: &Announcement, outcome: &str) -> Result<Scalar> {
    let e = tagged(
        "BIP0340/challenge",
        &[
            &a.nonce.serialize(),
            &a.pubkey.serialize(),
            &outcome_message(outcome),
        ],
    );
    Scalar::from_be_bytes(e).map_err(|_| Error::Crypto("challenge out of range".into()))
}

/// `S_m = R + e·P` for an announced outcome: the point each CET's adaptor signature is
/// encrypted to. Anyone can compute it from the announcement.
pub fn attestation_point(a: &Announcement, outcome: &str) -> Result<PublicKey> {
    let secp = Secp256k1::verification_only();
    let e = challenge(a, outcome)?;
    let p = a.pubkey.public_key(Parity::Even);
    let r = a.nonce.public_key(Parity::Even);
    let ep = p
        .mul_tweak(&secp, &e)
        .map_err(|e| Error::Crypto(format!("e·P: {e}")))?;
    r.combine(&ep)
        .map_err(|e| Error::Crypto(format!("R + e·P: {e}")))
}

impl DevOracle {
    /// A deterministic dev oracle for `event_id`, keyed by `label`.
    pub fn new(label: &str, event_id: &str, outcomes: &[&str]) -> Self {
        let secp = Secp256k1::new();
        let x = even(&secp, zkp_key(&format!("oracle/{label}")));
        let k = even(&secp, zkp_key(&format!("oracle/{label}/nonce/{event_id}")));
        let announcement = Announcement {
            pubkey: x.x_only_public_key(&secp).0,
            nonce: k.x_only_public_key(&secp).0,
            event_id: event_id.into(),
            outcomes: outcomes.iter().map(|o| o.to_string()).collect(),
        };
        Self {
            secp,
            x,
            k,
            announcement,
            attested: false,
        }
    }

    pub fn announcement(&self) -> &Announcement {
        &self.announcement
    }

    /// Publishes `s_m`. Refuses a second attestation: two signatures under one nonce
    /// leak the oracle key.
    pub fn attest(&mut self, outcome: &str) -> Result<SecretKey> {
        if self.attested {
            return Err(Error::Crypto("event already attested".into()));
        }
        if !self.announcement.outcomes.iter().any(|o| o == outcome) {
            return Err(Error::Crypto(format!("unknown outcome {outcome}")));
        }
        let e = challenge(&self.announcement, outcome)?;
        let s = self
            .x
            .mul_tweak(&e)
            .and_then(|ex| ex.add_tweak(&Scalar::from(self.k)))
            .map_err(|e| Error::Crypto(format!("s = k + e·x: {e}")))?;
        // It is a valid BIP-340 signature on the outcome message.
        let mut sig = [0u8; 64];
        sig[..32].copy_from_slice(&self.announcement.nonce.serialize());
        sig[32..].copy_from_slice(&s.secret_bytes());
        let sig = schnorr::Signature::from_slice(&sig)
            .map_err(|e| Error::Crypto(format!("schnorr sig: {e}")))?;
        self.secp
            .verify_schnorr(
                &sig,
                &Message::from_digest(outcome_message(outcome)),
                &self.announcement.pubkey,
            )
            .map_err(|e| Error::Crypto(format!("attestation is not a BIP-340 signature: {e}")))?;
        let _ = Keypair::from_secret_key(&self.secp, &self.x);
        self.attested = true;
        Ok(s)
    }
}

/// A secp256k1-zkp (0.29) secret key derived from a label.
pub fn zkp_key(label: &str) -> SecretKey {
    let mut h: [u8; 32] = Sha256::digest(format!("zdlc-dev/{label}").as_bytes()).into();
    loop {
        if let Ok(sk) = SecretKey::from_slice(&h) {
            return sk;
        }
        h = Sha256::digest(h).into();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attestation_scalar_opens_its_point() {
        let secp = Secp256k1::new();
        let mut o = DevOracle::new("t", "coin-1", &["heads", "tails"]);
        let a = o.announcement().clone();
        let s_heads = attestation_point(&a, "heads").unwrap();
        let s_tails = attestation_point(&a, "tails").unwrap();
        assert_ne!(s_heads, s_tails);
        let s = o.attest("heads").unwrap();
        assert_eq!(PublicKey::from_secret_key(&secp, &s), s_heads);
        assert!(o.attest("tails").is_err(), "second attestation refused");
    }
}
