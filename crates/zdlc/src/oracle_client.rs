//! Talking to `zdlc-oracled` and checking what it returns. A party never uses an
//! attestation it has not verified: every signature must be a BIP-340 signature by the
//! announced key, under the announced nonce, on the dlcspecs-tagged outcome.

use serde_json::{json, Value};

pub use crate::events::{to_enumerated, to_numeric, verify_numeric, verify_outcome};
use crate::{
    enclave::{AnnouncementMsg, AttestationMsg, EventSpec},
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

