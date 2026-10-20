//! Talking to `zdlc-oracled` and checking what it returns. A party never uses an
//! attestation it has not verified: every signature must be a BIP-340 signature by the
//! announced key, under the announced nonce, on the dlcspecs-tagged outcome.

use secp256k1_zkp::{schnorr, Message, Secp256k1, SecretKey, XOnlyPublicKey};
use serde_json::{json, Value};

use crate::{
    enclave::{AnnouncementMsg, AttestationMsg, EventSpec},
    oracle::{self, Announcement, NumericAnnouncement, NumericAttestation},
    Error, Result,
};

pub struct OracleClient {
    pub base: String,
    http: reqwest::Client,
}

impl OracleClient {
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<(u16, Value)> {
        let r = req.send().await.map_err(|e| Error::Rpc(format!("oracle {}: {e}", self.base)))?;
        let status = r.status().as_u16();
        let v = r.json().await.map_err(|e| Error::Rpc(format!("oracle {}: {e}", self.base)))?;
        Ok((status, v))
    }

    pub async fn info(&self) -> Result<Value> {
        Ok(self.send(self.http.get(format!("{}/pubkey", self.base))).await?.1)
    }

    pub async fn announce(&self, event_id: &str, spec: &EventSpec) -> Result<AnnouncementMsg> {
        let (s, v) = self
            .send(self.http.post(format!("{}/events", self.base)).json(&json!({"event_id": event_id, "spec": spec})))
            .await?;
        if s != 200 {
            return Err(Error::Rpc(format!("announce: {v}")));
        }
        serde_json::from_value(v).map_err(|e| Error::Rpc(e.to_string()))
    }

    pub async fn announcement(&self, event_id: &str) -> Result<AnnouncementMsg> {
        let (s, v) = self.send(self.http.get(format!("{}/events/{event_id}", self.base))).await?;
        if s != 200 {
            return Err(Error::Rpc(format!("announcement: {v}")));
        }
        serde_json::from_value(v).map_err(|e| Error::Rpc(e.to_string()))
    }

    /// The attestation, or `None` while the event is not yet due.
    pub async fn attest(&self, event_id: &str) -> Result<Option<AttestationMsg>> {
        let (s, v) = self
            .send(self.http.post(format!("{}/events/{event_id}/attest", self.base)))
            .await?;
        match s {
            200 => serde_json::from_value(v).map(Some).map_err(|e| Error::Rpc(e.to_string())),
            202 => Ok(None),
            _ => Err(Error::Rpc(format!("attest: {v}"))),
        }
    }

    /// The arbiter's decision (operator only: needs the oracle's admin token).
    pub async fn resolve(&self, event_id: &str, outcome: &str, token: &str) -> Result<()> {
        let (s, v) = self
            .send(self.http.post(format!("{}/events/{event_id}/resolve", self.base)).json(&json!({"outcome": outcome, "token": token})))
            .await?;
        if s != 200 {
            return Err(Error::Rpc(format!("resolve: {v}")));
        }
        Ok(())
    }

    /// Polls until the event is attested.
    pub async fn wait_attestation(&self, event_id: &str) -> Result<AttestationMsg> {
        loop {
            if let Some(a) = self.attest(event_id).await? {
                return Ok(a);
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }
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
