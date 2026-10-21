//! Oracle events as they travel: what an oracle is asked to attest, its announcement
//! and its attestation, and the checks a party runs before using them. Pure: no
//! network, so it also builds for WebAssembly.

use secp256k1_zkp::{schnorr, Message, Secp256k1, SecretKey, XOnlyPublicKey};
use serde::{Deserialize, Serialize};

use crate::{
    oracle::{self, Announcement, NumericAnnouncement, NumericAttestation},
    Error, Result,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum EventSpec {
    /// ZEC/USD close of the UTC minute starting at `minute`, in `unit_cents` units.
    ZecUsd {
        minute: i64,
        unit_cents: u64,
        nb_digits: usize,
    },
    /// "paid" if notes to `ivk` (64-byte raw Ironwood IVK, hex) in `from_height..=by_height`
    /// sum to at least `min_zat`, else "unpaid".
    Payment {
        ivk: String,
        min_zat: u64,
        from_height: u32,
        by_height: u32,
    },
    /// An outcome a human arbiter resolves (a milestone delivered or not). The oracle signs
    /// only after `resolve`, through the operator's admin endpoint.
    Arbiter {
        question: String,
        outcomes: Vec<String>,
    },
}

/// An announcement as the oracle publishes it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AnnouncementMsg {
    pub event_id: String,
    pub pubkey: String,
    pub nonces: Vec<String>,
    pub spec: EventSpec,
    /// Outcome strings: "0"/"1" per digit (numeric) or the outcome names (payment).
    pub outcomes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttestationMsg {
    pub event_id: String,
    pub pubkey: String,
    /// One per nonce: the digits, or the single outcome name.
    pub outcomes: Vec<String>,
    pub signatures: Vec<String>,
    pub detail: serde_json::Value,
}

pub fn parse_ivk(h: &str) -> Result<orchard::keys::IncomingViewingKey> {
    let b: [u8; 64] = hex::decode(h)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or(Error::Build("ivk must be 64 bytes of hex".into()))?;
    Option::from(orchard::keys::IncomingViewingKey::from_bytes(&b)).ok_or(Error::Build("invalid ivk".into()))
}

fn xonly(h: &str) -> Result<XOnlyPublicKey> {
    XOnlyPublicKey::from_slice(&hex::decode(h).map_err(|e| Error::Crypto(e.to_string()))?)
        .map_err(|e| Error::Crypto(e.to_string()))
}

pub fn to_numeric(a: &AnnouncementMsg) -> Result<NumericAnnouncement> {
    let EventSpec::ZecUsd { unit_cents, nb_digits, .. } = a.spec.clone() else {
        return Err(Error::Build("not a numeric event".into()));
    };
    if a.nonces.len() != nb_digits {
        return Err(Error::Crypto("nonce count".into()));
    }
    Ok(NumericAnnouncement {
        pubkey: xonly(&a.pubkey)?,
        nonces: a.nonces.iter().map(|n| xonly(n)).collect::<Result<_>>()?,
        event_id: a.event_id.clone(),
        nb_digits,
        unit_cents,
    })
}

pub fn to_enumerated(a: &AnnouncementMsg) -> Result<Announcement> {
    if a.nonces.len() != 1 {
        return Err(Error::Crypto("an enumerated event has one nonce".into()));
    }
    Ok(Announcement {
        pubkey: xonly(&a.pubkey)?,
        nonce: xonly(&a.nonces[0])?,
        event_id: a.event_id.clone(),
        outcomes: a.outcomes.clone(),
    })
}

/// Checks one signature and returns its scalar.
fn check(pubkey: &XOnlyPublicKey, nonce: &XOnlyPublicKey, outcome: &str, sig_hex: &str) -> Result<SecretKey> {
    let sig = schnorr::Signature::from_slice(&hex::decode(sig_hex).map_err(|e| Error::Crypto(e.to_string()))?)
        .map_err(|e| Error::Crypto(e.to_string()))?;
    let (r, s) = dlc::secp_utils::schnorrsig_decompose(&sig).map_err(|e| Error::Crypto(format!("{e:?}")))?;
    if r != *nonce {
        return Err(Error::Crypto("signature does not use the announced nonce".into()));
    }
    Secp256k1::verification_only()
        .verify_schnorr(&sig, &Message::from_digest(oracle::outcome_message(outcome)), pubkey)
        .map_err(|e| Error::Crypto(format!("attestation signature: {e}")))?;
    SecretKey::from_slice(s).map_err(|e| Error::Crypto(e.to_string()))
}

pub fn verify_numeric(a: &NumericAnnouncement, att: &AttestationMsg) -> Result<NumericAttestation> {
    if xonly(&att.pubkey)? != a.pubkey || att.outcomes.len() != a.nb_digits || att.signatures.len() != a.nb_digits {
        return Err(Error::Crypto("attestation does not match the announcement".into()));
    }
    let mut digits = vec![];
    let mut scalars = vec![];
    let mut signatures = vec![];
    for i in 0..a.nb_digits {
        let d: usize = match att.outcomes[i].as_str() {
            "0" => 0,
            "1" => 1,
            o => return Err(Error::Crypto(format!("digit {o}"))),
        };
        scalars.push(check(&a.pubkey, &a.nonces[i], &att.outcomes[i], &att.signatures[i])?);
        signatures.push(schnorr::Signature::from_slice(&hex::decode(&att.signatures[i]).unwrap()).unwrap());
        digits.push(d);
    }
    Ok(NumericAttestation {
        event_id: att.event_id.clone(),
        value: digits.iter().fold(0, |v, d| v * 2 + d),
        digits,
        signatures,
        scalars,
    })
}

pub fn verify_outcome(a: &Announcement, att: &AttestationMsg) -> Result<(String, SecretKey)> {
    if xonly(&att.pubkey)? != a.pubkey || att.outcomes.len() != 1 || !a.outcomes.contains(&att.outcomes[0]) {
        return Err(Error::Crypto("attestation does not match the announcement".into()));
    }
    let s = check(&a.pubkey, &a.nonce, &att.outcomes[0], &att.signatures[0])?;
    Ok((att.outcomes[0].clone(), s))
}

/// An oracle held in memory, for demos, tests and in-browser arbiters: announces and
/// attests like `zdlc-oracled` (same messages, same dlcspecs signatures), with nonces
/// derived from its seed and the event id. It refuses to attest two outcomes for one
/// event, which would reveal its key. Keep the seed secret; nothing is persisted.
pub struct MemoryOracle {
    seed: [u8; 32],
    secp: Secp256k1<secp256k1_zkp::All>,
    attested: std::collections::BTreeMap<String, Vec<String>>,
}

impl MemoryOracle {
    pub fn new(seed: [u8; 32]) -> Self {
        Self { seed, secp: Secp256k1::new(), attested: Default::default() }
    }

    fn secret(&self, what: &[u8]) -> SecretKey {
        use sha2::{Digest, Sha256};
        (0u8..=255)
            .find_map(|n| {
                let mut h = Sha256::new();
                h.update(b"zdlc/memory-oracle/v1");
                h.update(self.seed);
                h.update(what);
                h.update([n]);
                SecretKey::from_slice(&<[u8; 32]>::from(h.finalize())).ok()
            })
            .expect("a valid key")
    }

    fn keypair(&self) -> secp256k1_zkp::Keypair {
        secp256k1_zkp::Keypair::from_secret_key(&self.secp, &self.secret(b"key"))
    }

    pub fn pubkey(&self) -> String {
        hex::encode(self.keypair().x_only_public_key().0.serialize())
    }

    fn nonce(&self, event_id: &str, i: usize) -> SecretKey {
        self.secret(format!("nonce/{event_id}/{i}").as_bytes())
    }

    fn count(spec: &EventSpec) -> usize {
        match spec {
            EventSpec::ZecUsd { nb_digits, .. } => *nb_digits,
            _ => 1,
        }
    }

    pub fn announce(&self, event_id: &str, spec: &EventSpec) -> AnnouncementMsg {
        AnnouncementMsg {
            event_id: event_id.into(),
            pubkey: self.pubkey(),
            nonces: (0..Self::count(spec)).map(|i| hex::encode(self.nonce(event_id, i).x_only_public_key(&self.secp).0.serialize())).collect(),
            spec: spec.clone(),
            outcomes: match spec {
                EventSpec::ZecUsd { .. } => vec!["0".into(), "1".into()],
                EventSpec::Payment { .. } => vec!["paid".into(), "unpaid".into()],
                EventSpec::Arbiter { outcomes, .. } => outcomes.clone(),
            },
        }
    }

    /// Attests an outcome (enumerated events) or a value (ZEC/USD, in `unit_cents`).
    pub fn attest(&mut self, event_id: &str, spec: &EventSpec, outcome: &str) -> Result<AttestationMsg> {
        let outcomes: Vec<String> = match spec {
            EventSpec::ZecUsd { nb_digits, .. } => {
                let v: usize = outcome.parse().map_err(|_| Error::Build("a numeric event attests a number".into()))?;
                let v = v.min((1usize << nb_digits) - 1);
                (0..*nb_digits).rev().map(|i| ((v >> i) & 1).to_string()).collect()
            }
            _ => {
                if !self.announce(event_id, spec).outcomes.iter().any(|o| o == outcome) {
                    return Err(Error::Build(format!("{outcome} is not an outcome of {event_id}")));
                }
                vec![outcome.to_string()]
            }
        };
        if let Some(prev) = self.attested.get(event_id) {
            if *prev != outcomes {
                return Err(Error::Build(format!("{event_id} is already attested otherwise")));
            }
        }
        let kp = self.keypair();
        let signatures = outcomes
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let nonce = self.nonce(event_id, i).secret_bytes();
                let sig: schnorr::Signature = dlc::secp_utils::schnorrsig_sign_with_nonce(&self.secp, &Message::from_digest(oracle::outcome_message(o)), &kp, &nonce);
                hex::encode(sig.as_ref())
            })
            .collect();
        self.attested.insert(event_id.into(), outcomes.clone());
        Ok(AttestationMsg { event_id: event_id.into(), pubkey: self.pubkey(), outcomes, signatures, detail: serde_json::Value::Null })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_oracle_attestations_verify() {
        let mut o = MemoryOracle::new([3; 32]);
        let spec = EventSpec::Arbiter { question: "q".into(), outcomes: vec!["yes".into(), "no".into()] };
        let ann = o.announce("e1", &spec);
        let att = o.attest("e1", &spec, "yes").unwrap();
        assert_eq!(verify_outcome(&to_enumerated(&ann).unwrap(), &att).unwrap().0, "yes");
        assert!(o.attest("e1", &spec, "no").is_err(), "one outcome per event");
        let num = EventSpec::ZecUsd { minute: 60, unit_cents: 100, nb_digits: 14 };
        let ann = o.announce("p1", &num);
        let att = o.attest("p1", &num, "1234").unwrap();
        assert_eq!(verify_numeric(&to_numeric(&ann).unwrap(), &att).unwrap().value, 1234);
    }
}
