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

/// A numeric event: one nonce per binary digit, attested digit by digit (rust-dlc's
/// scheme: each digit's message is SHA-256 of the digit as a decimal string, signed with
/// `dlc::secp_utils::schnorrsig_sign_with_nonce`).
#[derive(Clone, Debug)]
pub struct NumericAnnouncement {
    pub pubkey: XOnlyPublicKey,
    pub nonces: Vec<XOnlyPublicKey>,
    pub event_id: String,
    pub nb_digits: usize,
    /// Price per unit of the attested value, in cents.
    pub unit_cents: u64,
}

#[derive(Clone, Debug)]
pub struct NumericAttestation {
    pub event_id: String,
    pub value: usize,
    pub digits: Vec<usize>,
    pub signatures: Vec<schnorr::Signature>,
    /// The signature scalars `s_i`: a CET prefix of length `n` decrypts with their sum
    /// over the first `n` digits.
    pub scalars: Vec<SecretKey>,
}

pub struct NumericOracle {
    secp: Secp256k1<secp256k1_zkp::All>,
    keypair: Keypair,
    nonces: Vec<[u8; 32]>,
    announcement: NumericAnnouncement,
    attested: bool,
}

pub fn digit_message(d: usize) -> Message {
    Message::from_digest(Sha256::digest(d.to_string().as_bytes()).into())
}

impl NumericOracle {
    /// A deterministic dev oracle for `event_id`, keyed by `label`.
    pub fn new(label: &str, event_id: &str, nb_digits: usize, unit_cents: u64) -> Self {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &zkp_key(&format!("oracle/{label}")));
        let nonces: Vec<[u8; 32]> = (0..nb_digits)
            .map(|i| zkp_key(&format!("oracle/{label}/nonce/{event_id}/{i}")).secret_bytes())
            .collect();
        let announcement = NumericAnnouncement {
            pubkey: keypair.x_only_public_key().0,
            nonces: nonces
                .iter()
                .map(|k| {
                    SecretKey::from_slice(k)
                        .expect("valid")
                        .x_only_public_key(&secp)
                        .0
                })
                .collect(),
            event_id: event_id.into(),
            nb_digits,
            unit_cents,
        };
        Self {
            secp,
            keypair,
            nonces,
            announcement,
            attested: false,
        }
    }

    pub fn announcement(&self) -> &NumericAnnouncement {
        &self.announcement
    }

    /// Attests `value`, clamped to the largest value the digits can carry. Once only.
    pub fn attest(&mut self, value: usize) -> Result<NumericAttestation> {
        if self.attested {
            return Err(Error::Crypto("event already attested".into()));
        }
        let n = self.announcement.nb_digits;
        let value = value.min((1 << n) - 1);
        let digits = dlc_trie::digit_decomposition::decompose_value(value, 2, n);
        let mut signatures = vec![];
        let mut scalars = vec![];
        for (i, d) in digits.iter().enumerate() {
            let msg = digit_message(*d);
            let sig = dlc::secp_utils::schnorrsig_sign_with_nonce(
                &self.secp,
                &msg,
                &self.keypair,
                &self.nonces[i],
            );
            self.secp
                .verify_schnorr(&sig, &msg, &self.announcement.pubkey)
                .map_err(|e| Error::Crypto(format!("digit {i}: {e}")))?;
            let (r, s) = dlc::secp_utils::schnorrsig_decompose(&sig)
                .map_err(|e| Error::Crypto(format!("decompose: {e:?}")))?;
            if r != self.announcement.nonces[i] {
                return Err(Error::Crypto(format!("digit {i}: nonce mismatch")));
            }
            scalars.push(SecretKey::from_slice(s).map_err(|e| Error::Crypto(e.to_string()))?);
            signatures.push(sig);
        }
        self.attested = true;
        Ok(NumericAttestation {
            event_id: self.announcement.event_id.clone(),
            value,
            digits,
            signatures,
            scalars,
        })
    }
}

/// The adaptor point a digit prefix unlocks: the sum of the oracle's signature points for
/// those digits (`dlc::get_adaptor_point_from_oracle_info`).
pub fn prefix_point(a: &NumericAnnouncement, prefix: &[usize]) -> Result<PublicKey> {
    if prefix.is_empty() || prefix.len() > a.nb_digits {
        return Err(Error::Crypto("prefix length".into()));
    }
    let info = dlc::OracleInfo {
        public_key: a.pubkey,
        nonces: a.nonces[..prefix.len()].to_vec(),
    };
    let msgs = prefix.iter().map(|d| digit_message(*d)).collect::<Vec<_>>();
    dlc::get_adaptor_point_from_oracle_info(&Secp256k1::verification_only(), &[info], &[msgs])
        .map_err(|e| Error::Crypto(format!("adaptor point: {e:?}")))
}

/// The scalar for a prefix of length `len` of an attestation's digits.
pub fn prefix_scalar(att: &NumericAttestation, len: usize) -> Result<SecretKey> {
    let mut it = att.scalars[..len].iter();
    let first = *it.next().ok_or(Error::Crypto("empty prefix".into()))?;
    it.try_fold(first, |acc, s| acc.add_tweak(&Scalar::from(*s)))
        .map_err(|e| Error::Crypto(format!("sum of scalars: {e}")))
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

    #[test]
    fn numeric_prefix_scalar_opens_prefix_point() {
        let secp = Secp256k1::new();
        let mut o = NumericOracle::new("t", "zecusd-1", 12, 10);
        let a = o.announcement().clone();
        let att = o.attest(600).unwrap();
        assert_eq!(att.digits.len(), 12);
        for len in [1, 5, 12] {
            let p = prefix_point(&a, &att.digits[..len]).unwrap();
            let s = prefix_scalar(&att, len).unwrap();
            assert_eq!(PublicKey::from_secret_key(&secp, &s), p);
        }
        let mut other = att.digits[..4].to_vec();
        other[3] ^= 1;
        let s = prefix_scalar(&att, 4).unwrap();
        assert_ne!(PublicKey::from_secret_key(&secp, &s), prefix_point(&a, &other).unwrap());
        assert!(o.attest(601).is_err());
    }
}
