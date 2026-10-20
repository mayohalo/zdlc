//! The four things an app chooses: how peers talk, where contracts live, which oracles
//! attest, and how the kit reaches the chain. Each comes with one implementation
//! (`impls`): the desk mailbox, SQLite, `zdlc-oracled` over HTTP, and Zakura JSON-RPC.

use async_trait::async_trait;
use orchard::keys::FullViewingKey;
use serde_json::Value;
use zcash_protocol::{consensus::NetworkType, local_consensus::LocalNetwork};
use zcash_transparent::address::TransparentAddress;
use zdlc::{
    enclave::{AnnouncementMsg, AttestationMsg, EventSpec},
    notes::OwnedNote,
    tx::IronwoodSpend,
    wire::Envelope,
};

use crate::{Event, Record, Result};

/// Peer messaging. Messages are addressed by identity (a public key) and grouped in
/// threads (one per contract).
#[async_trait]
pub trait Transport: Send + Sync {
    fn name(&self) -> String;
    /// This node's identity: what a counterparty addresses offers to.
    fn identity(&self) -> String;
    async fn send(&self, to: &str, thread: &str, kind: &str, body: Value) -> Result<()>;
    /// New messages since the last call, verified (signed by their sender).
    async fn receive(&self) -> Result<Vec<Envelope>>;
}

/// Contracts, the event log and small values. Holds secrets (a deposit-funded side's
/// funding PCZT): keep it as private as the seed.
#[async_trait]
pub trait Storage: Send + Sync {
    fn name(&self) -> String;
    async fn put(&self, rec: &Record) -> Result<()>;
    async fn get(&self, id: &str) -> Result<Option<Record>>;
    async fn list(&self) -> Result<Vec<Record>>;
    /// Appends an event; returns its sequence number (from 1).
    async fn push_event(&self, ev: &Event) -> Result<u64>;
    async fn events_after(&self, seq: u64, limit: usize) -> Result<Vec<(u64, Event)>>;
    async fn get_kv(&self, key: &str) -> Result<Option<Value>>;
    async fn put_kv(&self, key: &str, v: &Value) -> Result<()>;
}

/// One oracle. A contract settles on the n-of-n agreement of every configured oracle.
#[async_trait]
pub trait Oracle: Send + Sync {
    fn name(&self) -> String;
    async fn pubkey(&self) -> Result<String>;
    async fn announce(&self, id: &str, spec: &EventSpec) -> Result<AnnouncementMsg>;
    async fn announcement(&self, id: &str) -> Result<AnnouncementMsg>;
    /// `None` while the event is not yet due or resolved.
    async fn attestation(&self, id: &str) -> Result<Option<AttestationMsg>>;
}

/// An unspent transparent output: (txid in internal order, index, zatoshis, height).
pub type Utxo = ([u8; 32], u32, u64, u32);

/// The chain.
#[async_trait]
pub trait Chain: Send + Sync {
    fn name(&self) -> String;
    /// Consensus parameters and address encoding, read from the node.
    async fn network(&self) -> Result<(LocalNetwork, NetworkType)>;
    async fn height(&self) -> Result<u32>;
    async fn send(&self, raw: &[u8]) -> Result<()>;
    async fn mined_height(&self, txid: &[u8; 32]) -> Result<Option<u32>>;
    async fn utxos(&self, addr: &TransparentAddress, net: NetworkType) -> Result<Vec<Utxo>>;
    async fn ironwood_anchor(&self, height: u32) -> Result<orchard::Anchor>;
    /// Unspent notes to `fvk` received in `from..=to`.
    async fn notes(&self, fvk: &FullViewingKey, from: u32, to: u32) -> Result<Vec<OwnedNote>>;
    /// Witnessed spends of `notes` against the root at `anchor_height`.
    async fn spends(&self, fvk: &FullViewingKey, notes: &[OwnedNote], anchor_height: u32) -> Result<(Vec<IronwoodSpend>, orchard::Anchor)>;
}
