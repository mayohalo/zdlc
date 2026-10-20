//! The RFQ desk's data: requests, firm quotes, and the signatures that bind them to
//! identities. Shared by `zdlc-desk` (the server) and `zdlc-trader` (the agents).
//!
//! The desk never holds funds or keys. It learns who asked, who quoted and who reads
//! which mailbox, as the design assumes; setup messages pass through it sealed.

use secp256k1::{ecdsa, Message, PublicKey, SecretKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{wire::{self, WireCoin}, Error, Result};

/// A request for a stable-value hedge: the taker (recipient) holds `q_recipient` and
/// wants it worth a fixed dollar amount at maturity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RfqBody {
    pub q_recipient: u64,
    /// Blocks from setup to the oracle event and to the refund.
    pub maturity_blocks: u32,
    pub refund_blocks: u32,
    /// The oracle pair both sides will settle on, by public key.
    pub oracle_keys: Vec<String>,
    pub unit_cents: u64,
    pub nb_digits: usize,
    /// Unique per request.
    pub nonce: String,
}

/// A firm quote: terms, plus the maker's funding inputs and the height it expires at.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct QuoteBody {
    pub rfq_id: String,
    pub q_hedger: u64,
    pub premium: u64,
    pub rounding: u64,
    /// The maker's funding inputs, reserved for this quote until `valid_until`.
    pub inputs: Vec<WireCoin>,
    pub valid_until: u32,
    /// The maker's whole side of the contract (keys, inputs, addresses), fixed by the
    /// quote: a taker funding from Ironwood builds the funding transaction from it
    /// before offering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub party: Option<wire::WireParty>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Signed<T> {
    pub body: T,
    pub from: String,
    pub sig: String,
}

fn digest<T: Serialize>(tag: &str, body: &T, from: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(tag.as_bytes());
    h.update([0]);
    h.update(from.as_bytes());
    h.update([0]);
    h.update(serde_json::to_vec(body).expect("serializable"));
    h.finalize().into()
}

impl<T: Serialize> Signed<T> {
    pub fn new(tag: &str, body: T, sk: &SecretKey) -> Self {
        let from = wire::identity(sk);
        let sig = ecdsa::sign(Message::from_digest(digest(tag, &body, &from)), sk);
        Self { body, from, sig: hex::encode(sig.serialize_compact()) }
    }

    pub fn verify(&self, tag: &str) -> Result<()> {
        let pk = PublicKey::from_slice(&hex::decode(&self.from).map_err(|e| Error::Crypto(e.to_string()))?)
            .map_err(|e| Error::Crypto(e.to_string()))?;
        let sig = ecdsa::Signature::from_compact(&hex::decode(&self.sig).map_err(|e| Error::Crypto(e.to_string()))?)
            .map_err(|e| Error::Crypto(e.to_string()))?;
        ecdsa::verify(&sig, Message::from_digest(digest(tag, &self.body, &self.from)), &pk)
            .map_err(|_| Error::Crypto(format!("{tag} signature")))
    }
}

pub const RFQ_TAG: &str = "zdlc/rfq/v1";
pub const QUOTE_TAG: &str = "zdlc/quote/v1";
pub const READ_TAG: &str = "zdlc/mailbox-read/v1";

/// The signed query a mailbox owner sends to read its boxes from `since`.
pub fn read_auth(sk: &SecretKey, since: u64, ts: i64) -> (String, String) {
    let from = wire::identity(sk);
    let body = (since, ts);
    let sig = ecdsa::sign(Message::from_digest(digest(READ_TAG, &body, &from)), sk);
    (from, hex::encode(sig.serialize_compact()))
}

pub fn check_read_auth(owner: &str, since: u64, ts: i64, sig: &str) -> Result<()> {
    Signed { body: (since, ts), from: owner.to_string(), sig: sig.to_string() }.verify(READ_TAG)
}

/// Content-derived id for a signed post.
pub fn id_of<T: Serialize>(s: &Signed<T>) -> String {
    let h: [u8; 32] = Sha256::digest(serde_json::to_vec(&s.body).unwrap()).into();
    hex::encode(&h[..12])
}

/// The client side of the desk's HTTP API.
pub struct DeskClient {
    pub base: String,
    http: reqwest::Client,
}

impl DeskClient {
    pub fn new(base: impl Into<String>) -> Self {
        Self { base: base.into().trim_end_matches('/').to_string(), http: reqwest::Client::new() }
    }

    async fn json(&self, r: reqwest::RequestBuilder) -> Result<serde_json::Value> {
        let r = r.send().await.map_err(|e| Error::Rpc(format!("desk: {e}")))?;
        let ok = r.status().is_success();
        let v: serde_json::Value = r.json().await.map_err(|e| Error::Rpc(format!("desk: {e}")))?;
        if !ok {
            return Err(Error::Rpc(format!("desk: {v}")));
        }
        Ok(v)
    }

    pub async fn post_rfq(&self, s: &Signed<RfqBody>) -> Result<String> {
        let v = self.json(self.http.post(format!("{}/rfqs", self.base)).json(s)).await?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    pub async fn rfqs(&self) -> Result<Vec<(String, Signed<RfqBody>)>> {
        let v = self.json(self.http.get(format!("{}/rfqs", self.base))).await?;
        serde_json::from_value(v).map_err(|e| Error::Rpc(e.to_string()))
    }

    pub async fn post_quote(&self, s: &Signed<QuoteBody>, private: bool) -> Result<String> {
        let path = if private { "quotes/private".to_string() } else { format!("rfqs/{}/quotes", s.body.rfq_id) };
        let v = self.json(self.http.post(format!("{}/{path}", self.base)).json(s)).await?;
        Ok(v["id"].as_str().unwrap_or_default().to_string())
    }

    pub async fn quotes(&self, rfq: &str) -> Result<Vec<(String, Signed<QuoteBody>)>> {
        let v = self.json(self.http.get(format!("{}/rfqs/{rfq}/quotes", self.base))).await?;
        serde_json::from_value(v).map_err(|e| Error::Rpc(e.to_string()))
    }

    pub async fn quote(&self, id: &str) -> Result<Signed<QuoteBody>> {
        let v = self.json(self.http.get(format!("{}/quotes/{id}", self.base))).await?;
        serde_json::from_value(v).map_err(|e| Error::Rpc(e.to_string()))
    }

    pub async fn send(&self, b: &wire::SealedBox) -> Result<()> {
        self.json(self.http.post(format!("{}/mailbox", self.base)).json(b)).await.map(|_| ())
    }

    /// Boxes for `sk`'s identity from sequence `since`: (seq, box).
    pub async fn read(&self, sk: &SecretKey, since: u64) -> Result<Vec<(u64, wire::SealedBox)>> {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let (who, sig) = read_auth(sk, since, ts);
        let v = self
            .json(self.http.get(format!("{}/mailbox/{who}?since={since}&ts={ts}&sig={sig}", self.base)))
            .await?;
        serde_json::from_value(v).map_err(|e| Error::Rpc(e.to_string()))
    }
}
