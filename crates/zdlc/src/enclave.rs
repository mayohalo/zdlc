//! The oracle core meant to run inside a TEE enclave.
//!
//! All secrets (the signing key and the nonce seed) and the event records live in one
//! sealed state blob: encrypted and authenticated under a sealing key, and written to
//! disk before any announcement or signature leaves the process.
//!
//! - **Nonce counter.** Nonces derive from the seed and a monotonic counter. Every
//!   announcement reserves fresh counter values and persists the counter first, so a
//!   nonce is never used for two events, even across restarts.
//! - **One outcome per event.** The attested outcome is recorded and persisted before
//!   signing. A later request re-signs the same outcome, which yields identical
//!   signatures; a different outcome is refused. A crash or a replayed request therefore
//!   cannot produce two signatures under one nonce.
//!
//! [`Sealer`] abstracts the sealing key. `DevSealer` keeps it in a file (ChaCha20-Poly1305),
//! the stand-in used on this machine. Inside an enclave, the sealer is the platform's
//! sealing key (for example, a KMS key released only to the enclave's attested
//! measurement), and the attestation document binds the oracle's public key to that
//! measurement.

use std::collections::BTreeMap;

use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, KeyInit, Nonce};
use secp256k1_zkp::{schnorr, Keypair, Message, Secp256k1, SecretKey, XOnlyPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use crate::events::{parse_ivk, AnnouncementMsg, AttestationMsg, EventSpec};
use crate::{feeds, node::Node, oracle, Error, Result};

pub trait Sealer: Send + Sync {
    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>>;
    fn unseal(&self, sealed: &[u8]) -> Result<Vec<u8>>;
    fn describe(&self) -> String;
}

/// A sealing key in a file: the development stand-in for an enclave's sealing key.
pub struct DevSealer {
    key: [u8; 32],
}

impl DevSealer {
    /// Loads the key, creating a random one (mode 600) if the file does not exist.
    pub fn open(path: &str) -> Result<Self> {
        if let Ok(h) = std::fs::read_to_string(path) {
            let key = hex::decode(h.trim())
                .ok()
                .and_then(|b| b.try_into().ok())
                .ok_or(Error::Build("sealing key must be 32 bytes of hex".into()))?;
            return Ok(Self { key });
        }
        let mut key = [0u8; 32];
        rand10::fill(&mut key);
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| Error::Build(format!("sealing key {path}: {e}")))?;
        writeln!(f, "{}", hex::encode(key)).map_err(|e| Error::Build(e.to_string()))?;
        Ok(Self { key })
    }
}

impl Sealer for DevSealer {
    fn seal(&self, plain: &[u8]) -> Result<Vec<u8>> {
        let mut n = [0u8; 12];
        rand10::fill(&mut n);
        let ct = ChaCha20Poly1305::new((&self.key).into())
            .encrypt(Nonce::from_slice(&n), plain)
            .map_err(|_| Error::Crypto("seal".into()))?;
        Ok([n.to_vec(), ct].concat())
    }

    fn unseal(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        if sealed.len() < 12 {
            return Err(Error::Crypto("sealed state too short".into()));
        }
        ChaCha20Poly1305::new((&self.key).into())
            .decrypt(Nonce::from_slice(&sealed[..12]), &sealed[12..])
            .map_err(|_| Error::Crypto("sealed state fails authentication".into()))
    }

    fn describe(&self) -> String {
        "dev (file key, ChaCha20-Poly1305)".into()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct EventRec {
    spec: EventSpec,
    first_nonce: u64,
    /// The outcome this event is bound to: digits of the value, or an outcome name.
    attested: Option<Vec<String>>,
    /// The attested value (numeric) and the evidence behind it.
    detail: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
struct State {
    signing_key: [u8; 32],
    nonce_seed: [u8; 32],
    next_nonce: u64,
    events: BTreeMap<String, EventRec>,
}

pub struct Enclave {
    path: String,
    sealer: Box<dyn Sealer>,
    state: State,
    secp: Secp256k1<secp256k1_zkp::All>,
}

const PAYMENT_OUTCOMES: [&str; 2] = ["paid", "unpaid"];

fn random32() -> [u8; 32] {
    let mut b = [0u8; 32];
    rand10::fill(&mut b);
    b
}

impl Enclave {
    /// Opens the sealed state at `path`, or creates a fresh oracle (new random key and
    /// nonce seed) if there is none.
    pub fn open(path: &str, sealer: Box<dyn Sealer>) -> Result<Self> {
        let state = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&sealer.unseal(&b)?)
                .map_err(|e| Error::Build(format!("state: {e}")))?,
            Err(_) => {
                let mut sk = random32();
                while SecretKey::from_slice(&sk).is_err() {
                    sk = random32();
                }
                State {
                    signing_key: sk,
                    nonce_seed: random32(),
                    next_nonce: 0,
                    events: BTreeMap::new(),
                }
            }
        };
        let e = Self {
            path: path.into(),
            sealer,
            state,
            secp: Secp256k1::new(),
        };
        e.save()?;
        Ok(e)
    }

    fn save(&self) -> Result<()> {
        let plain = serde_json::to_vec(&self.state).map_err(|e| Error::Build(e.to_string()))?;
        let tmp = format!("{}.tmp", self.path);
        std::fs::write(&tmp, self.sealer.seal(&plain)?)
            .and_then(|_| std::fs::rename(&tmp, &self.path))
            .map_err(|e| Error::Build(format!("save state: {e}")))
    }

    pub fn sealer(&self) -> String {
        self.sealer.describe()
    }

    fn keypair(&self) -> Keypair {
        Keypair::from_secret_key(&self.secp, &SecretKey::from_slice(&self.state.signing_key).expect("valid"))
    }

    pub fn pubkey(&self) -> XOnlyPublicKey {
        self.keypair().x_only_public_key().0
    }

    pub fn next_nonce(&self) -> u64 {
        self.state.next_nonce
    }

    fn nonce_secret(&self, index: u64) -> [u8; 32] {
        let mut attempt = 0u8;
        loop {
            let mut h = Sha256::new();
            h.update(b"zdlc/oracle-nonce/v1");
            h.update(self.state.nonce_seed);
            h.update(index.to_be_bytes());
            h.update([attempt]);
            let b: [u8; 32] = h.finalize().into();
            if SecretKey::from_slice(&b).is_ok() {
                return b;
            }
            attempt += 1;
        }
    }

    fn nonce_count(spec: &EventSpec) -> usize {
        match spec {
            EventSpec::ZecUsd { nb_digits, .. } => *nb_digits,
            EventSpec::Payment { .. } | EventSpec::Arbiter { .. } => 1,
        }
    }

    fn announcement_of(&self, id: &str, rec: &EventRec) -> AnnouncementMsg {
        let n = Self::nonce_count(&rec.spec);
        AnnouncementMsg {
            event_id: id.into(),
            pubkey: hex::encode(self.pubkey().serialize()),
            nonces: (0..n as u64)
                .map(|i| {
                    let k = SecretKey::from_slice(&self.nonce_secret(rec.first_nonce + i)).expect("valid");
                    hex::encode(k.x_only_public_key(&self.secp).0.serialize())
                })
                .collect(),
            spec: rec.spec.clone(),
            outcomes: match &rec.spec {
                EventSpec::ZecUsd { .. } => vec!["0".into(), "1".into()],
                EventSpec::Payment { .. } => PAYMENT_OUTCOMES.iter().map(|s| s.to_string()).collect(),
                EventSpec::Arbiter { outcomes, .. } => outcomes.clone(),
            },
        }
    }

    /// Announces an event (idempotent for the same id and spec). The nonce counter is
    /// advanced and sealed to disk before the nonces are returned.
    pub fn announce(&mut self, id: &str, spec: EventSpec) -> Result<AnnouncementMsg> {
        if let Some(rec) = self.state.events.get(id) {
            if rec.spec != spec {
                return Err(Error::Build(format!("event {id} already announced with other terms")));
            }
            return Ok(self.announcement_of(id, rec));
        }
        match &spec {
            EventSpec::ZecUsd { minute, nb_digits, unit_cents } => {
                if minute % 60 != 0 || *nb_digits == 0 || *nb_digits > 32 || *unit_cents == 0 {
                    return Err(Error::Build("bad ZEC/USD event".into()));
                }
            }
            EventSpec::Payment { ivk, from_height, by_height, .. } => {
                parse_ivk(ivk)?;
                if by_height < from_height {
                    return Err(Error::Build("by_height < from_height".into()));
                }
            }
            EventSpec::Arbiter { outcomes, .. } => {
                if outcomes.len() < 2 {
                    return Err(Error::Build("an arbiter event needs two outcomes or more".into()));
                }
            }
        }
        let rec = EventRec {
            first_nonce: self.state.next_nonce,
            spec,
            attested: None,
            detail: None,
        };
        self.state.next_nonce += Self::nonce_count(&rec.spec) as u64;
        self.state.events.insert(id.into(), rec.clone());
        self.save()?;
        Ok(self.announcement_of(id, &rec))
    }

    pub fn announcement(&self, id: &str) -> Option<AnnouncementMsg> {
        self.state.events.get(id).map(|r| self.announcement_of(id, r))
    }

    /// Attests an event once its data is final. `now` is unix seconds; `node` is needed
    /// for payment events. Returns `Ok(None)` if the event is not yet due. A service that
    /// shares the enclave should use [`Enclave::pending`], [`Enclave::observe`] and
    /// [`Enclave::bind_and_sign`] instead, so the network fetch runs without the lock.
    pub async fn attest(&mut self, id: &str, now: i64, node: Option<&Node>) -> Result<Option<AttestationMsg>> {
        match self.pending(id, now)? {
            Pending::NotDue => Ok(None),
            Pending::Attested => self.bind_and_sign(id, None).map(Some),
            Pending::Observe(spec) => match Self::observe(&spec, now, node).await? {
                Some(o) => self.bind_and_sign(id, Some(o)).map(Some),
                None => Ok(None),
            },
        }
    }

    /// What attesting `id` needs: nothing (already bound), nothing yet (not due), or an
    /// observation of the world. Quick; reads the state only.
    pub fn pending(&self, id: &str, now: i64) -> Result<Pending> {
        let rec = self.state.events.get(id).ok_or(Error::Build(format!("event {id} was never announced")))?;
        if rec.attested.is_some() {
            return Ok(Pending::Attested);
        }
        Ok(match &rec.spec {
            // Wait for the minute to close and the exchanges to publish it.
            EventSpec::ZecUsd { minute, .. } if now < minute + 60 + 15 => Pending::NotDue,
            // Not resolved yet.
            EventSpec::Arbiter { .. } => Pending::NotDue,
            spec => Pending::Observe(spec.clone()),
        })
    }

    /// Observes what an event attests: the exchanges' ZEC/USD close, or the payments a
    /// viewing key received. Network only, no enclave state, so it runs without the
    /// lock. `None` if the data is not final yet (a payment window still open).
    pub async fn observe(spec: &EventSpec, _now: i64, node: Option<&Node>) -> Result<Option<(Vec<String>, serde_json::Value)>> {
        Ok(Some(match spec {
            EventSpec::ZecUsd { minute, unit_cents, nb_digits } => {
                let (price, quotes) = feeds::zecusd_minute(*minute).await?;
                let max = (1u64 << nb_digits) - 1;
                let value = (price / unit_cents).min(max) as usize;
                let digits = dlc_trie::digit_decomposition::decompose_value(value, 2, *nb_digits);
                let detail = serde_json::json!({
                    "price_cents": price, "value": value,
                    "quotes": quotes.iter().map(|q| serde_json::json!({"source": q.source, "close_cents": q.close_cents})).collect::<Vec<_>>(),
                });
                (digits.iter().map(|d| d.to_string()).collect::<Vec<_>>(), detail)
            }
            EventSpec::Payment { ivk, min_zat, from_height, by_height } => {
                let node = node.ok_or(Error::Build("payment events need --node".into()))?;
                if node.height().await? < *by_height {
                    return Ok(None);
                }
                let got = feeds::received(node, &parse_ivk(ivk)?, *from_height, *by_height).await?;
                let outcome = if got >= *min_zat { "paid" } else { "unpaid" };
                (vec![outcome.to_string()], serde_json::json!({"received_zat": got}))
            }
            EventSpec::Arbiter { .. } => return Ok(None),
        }))
    }

    /// Binds the event to `observed` unless it is already bound (the first observation
    /// wins, so two concurrent fetches cannot produce two outcomes), then signs the bound
    /// outcome. The binding is sealed before any signature exists.
    pub fn bind_and_sign(&mut self, id: &str, observed: Option<(Vec<String>, serde_json::Value)>) -> Result<AttestationMsg> {
        let rec = self.state.events.get(id).cloned().ok_or(Error::Build(format!("event {id} was never announced")))?;
        let (outcomes, detail) = match (&rec.attested, observed) {
            (Some(o), _) => (o.clone(), rec.detail.clone().unwrap_or_default()),
            (None, Some((outcomes, detail))) => {
                let r = self.state.events.get_mut(id).expect("present");
                r.attested = Some(outcomes.clone());
                r.detail = Some(detail.clone());
                self.save()?;
                (outcomes, detail)
            }
            (None, None) => return Err(Error::Build(format!("event {id} has no outcome to sign"))),
        };
        let kp = self.keypair();
        let mut signatures = vec![];
        for (i, o) in outcomes.iter().enumerate() {
            let msg = Message::from_digest(oracle::outcome_message(o));
            let nonce = self.nonce_secret(rec.first_nonce + i as u64);
            let sig: schnorr::Signature = dlc::secp_utils::schnorrsig_sign_with_nonce(&self.secp, &msg, &kp, &nonce);
            signatures.push(hex::encode(sig.as_ref()));
        }
        Ok(AttestationMsg {
            event_id: id.into(),
            pubkey: hex::encode(self.pubkey().serialize()),
            outcomes,
            signatures,
            detail,
        })
    }
}

/// What [`Enclave::pending`] says an attestation needs.
pub enum Pending {
    Attested,
    NotDue,
    Observe(EventSpec),
}

impl Enclave {
    /// The arbiter's decision for an `Arbiter` event: binds the event to `outcome`
    /// (sealed before anything is signed). A second, different decision is refused.
    pub fn resolve(&mut self, id: &str, outcome: &str) -> Result<()> {
        let rec = self.state.events.get_mut(id).ok_or(Error::Build(format!("event {id} was never announced")))?;
        let EventSpec::Arbiter { outcomes, .. } = &rec.spec else {
            return Err(Error::Build("only arbiter events are resolved by hand".into()));
        };
        if !outcomes.iter().any(|o| o == outcome) {
            return Err(Error::Build(format!("{outcome} is not an outcome of {id}")));
        }
        match &rec.attested {
            Some(o) if o != &vec![outcome.to_string()] => return Err(Error::Build("already resolved otherwise".into())),
            Some(_) => return Ok(()),
            None => {}
        }
        rec.attested = Some(vec![outcome.to_string()]);
        rec.detail = Some(serde_json::json!({"resolved_by": "arbiter"}));
        self.save()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_observation_binds_the_event() {
        let dir = std::env::temp_dir().join(format!("zdlc-enclave-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = |f: &str| dir.join(f).to_string_lossy().into_owned();
        let sealer = DevSealer::open(&path("k")).unwrap();
        let mut e = Enclave::open(&path("s"), Box::new(sealer)).unwrap();
        let minute = 1_800_000_000 - 1_800_000_000 % 60;
        e.announce("p", EventSpec::ZecUsd { minute, unit_cents: 100, nb_digits: 4 }).unwrap();
        assert!(matches!(e.pending("p", minute).unwrap(), Pending::NotDue), "the minute has not closed");
        assert!(matches!(e.pending("p", minute + 75).unwrap(), Pending::Observe(_)));
        // Two fetches raced; the first to bind wins and the second is ignored.
        let first = e.bind_and_sign("p", Some((vec!["0".into(), "1".into(), "0".into(), "1".into()], serde_json::json!({"value": 5})))).unwrap();
        let second = e.bind_and_sign("p", Some((vec!["1".into(), "1".into(), "1".into(), "1".into()], serde_json::json!({"value": 15})))).unwrap();
        assert_eq!(first.outcomes, second.outcomes);
        assert_eq!(first.signatures, second.signatures);
        assert!(matches!(e.pending("p", minute + 75).unwrap(), Pending::Attested));
        std::fs::remove_dir_all(&dir).ok();
    }
}
