//! The first implementation of each trait.

use std::sync::Mutex;

use async_trait::async_trait;
use orchard::keys::FullViewingKey;
use rusqlite::{params, Connection, OptionalExtension};
use secp256k1::SecretKey;
use serde_json::Value;
use zcash_protocol::{consensus::NetworkType, local_consensus::LocalNetwork};
use zcash_transparent::address::TransparentAddress;
use zdlc::{
    desk::DeskClient,
    enclave::{AnnouncementMsg, AttestationMsg, EventSpec},
    node::Node,
    notes::{self, OwnedNote},
    oracle_client::OracleClient,
    params,
    tx::IronwoodSpend,
    txid_hex,
    wire::{self, Envelope},
};

use crate::{
    traits::{Chain, Oracle, Storage, Transport, Utxo},
    Error, Event, Record, Result,
};

// ---- Transport: the desk mailbox ------------------------------------------------------

/// Envelopes signed by the sender and sealed to the recipient, stored and forwarded by
/// a `zdlc-desk`. The desk sees recipients, sizes and timing, never contents.
pub struct DeskTransport {
    desk: DeskClient,
    key: SecretKey,
    seq: Mutex<u64>,
}

impl DeskTransport {
    /// `key` is the node's identity key (`Wallet::identity_key`).
    pub fn new(url: &str, key: SecretKey) -> Self {
        Self { desk: DeskClient::new(url), key, seq: Mutex::new(0) }
    }

    /// Resume reading after mailbox position `seq`.
    pub fn resume(self, seq: u64) -> Self {
        *self.seq.lock().unwrap() = seq;
        self
    }

    pub fn position(&self) -> u64 {
        *self.seq.lock().unwrap()
    }
}

#[async_trait]
impl Transport for DeskTransport {
    fn name(&self) -> String {
        format!("desk {}", self.desk.base)
    }

    fn identity(&self) -> String {
        wire::identity(&self.key)
    }

    async fn send(&self, to: &str, thread: &str, kind: &str, body: Value) -> Result<()> {
        let env = Envelope::new(&self.key, to, thread, kind, body);
        Ok(self.desk.send(&env.seal()?).await?)
    }

    async fn receive(&self) -> Result<Vec<Envelope>> {
        let since = self.position();
        let boxes = self.desk.read(&self.key, since).await?;
        let mut out = vec![];
        let mut next = since;
        for (seq, b) in boxes {
            next = next.max(seq + 1);
            if let Ok(e) = b.open(&self.key) {
                out.push(e);
            }
        }
        *self.seq.lock().unwrap() = next;
        Ok(out)
    }
}

// ---- Storage: SQLite ------------------------------------------------------------------

/// Contracts and events as JSON rows in one SQLite file.
pub struct SqliteStorage {
    db: Mutex<Connection>,
    path: String,
}

impl SqliteStorage {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).map_err(db_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS contracts (id TEXT PRIMARY KEY, json TEXT NOT NULL, updated INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY AUTOINCREMENT, json TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, json TEXT NOT NULL);",
        )
        .map_err(db_err)?;
        Ok(Self { db: Mutex::new(conn), path: path.into() })
    }

    /// An in-memory database, for tests.
    pub fn memory() -> Result<Self> {
        Self::open(":memory:")
    }
}

fn db(e: rusqlite::Error) -> Error {
    Error::Storage(e.to_string())
}

fn js<T: serde::Serialize>(v: &T) -> Result<String> {
    serde_json::to_string(v).map_err(|e| Error::Storage(e.to_string()))
}

fn unjs<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| Error::Storage(e.to_string()))
}

#[async_trait]
impl Storage for SqliteStorage {
    fn name(&self) -> String {
        format!("sqlite {}", self.path)
    }

    async fn put(&self, rec: &Record) -> Result<()> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        self.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO contracts (id, json, updated) VALUES (?1, ?2, ?3)
                 ON CONFLICT(id) DO UPDATE SET json = excluded.json, updated = excluded.updated",
                params![rec.id, js(rec)?, now],
            )
            .map_err(db_err)?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Record>> {
        let s: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT json FROM contracts WHERE id = ?1", params![id], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        s.map(|s| unjs(&s)).transpose()
    }

    async fn list(&self) -> Result<Vec<Record>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT json FROM contracts ORDER BY updated DESC").map_err(db_err)?;
        let rows: Vec<String> = q.query_map([], |r| r.get(0)).map_err(db_err)?.collect::<std::result::Result<_, _>>().map_err(db_err)?;
        rows.iter().map(|s| unjs(s)).collect()
    }

    async fn push_event(&self, ev: &Event) -> Result<u64> {
        let db = self.db.lock().unwrap();
        db.execute("INSERT INTO events (json) VALUES (?1)", params![js(ev)?]).map_err(db_err)?;
        Ok(db.last_insert_rowid() as u64)
    }

    async fn events_after(&self, seq: u64, limit: usize) -> Result<Vec<(u64, Event)>> {
        let db = self.db.lock().unwrap();
        let mut q = db.prepare("SELECT seq, json FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2").map_err(db_err)?;
        let rows: Vec<(i64, String)> = q
            .query_map(params![seq as i64, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(db_err)?
            .collect::<std::result::Result<_, _>>()
            .map_err(db_err)?;
        rows.into_iter().map(|(n, s)| Ok((n as u64, unjs(&s)?))).collect()
    }

    async fn get_kv(&self, key: &str) -> Result<Option<Value>> {
        let s: Option<String> = self
            .db
            .lock()
            .unwrap()
            .query_row("SELECT json FROM kv WHERE key = ?1", params![key], |r| r.get(0))
            .optional()
            .map_err(db_err)?;
        s.map(|s| unjs(&s)).transpose()
    }

    async fn put_kv(&self, key: &str, v: &Value) -> Result<()> {
        self.db
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO kv (key, json) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET json = excluded.json",
                params![key, js(v)?],
            )
            .map_err(db_err)?;
        Ok(())
    }
}

fn db_err(e: rusqlite::Error) -> Error {
    db(e)
}

// ---- Oracle: zdlc-oracled over HTTP ---------------------------------------------------

pub struct HttpOracle(OracleClient);

impl HttpOracle {
    pub fn new(url: &str) -> Self {
        Self(OracleClient::new(url))
    }
}

#[async_trait]
impl Oracle for HttpOracle {
    fn name(&self) -> String {
        self.0.base.clone()
    }

    async fn pubkey(&self) -> Result<String> {
        Ok(self.0.info().await?["pubkey"].as_str().unwrap_or_default().to_string())
    }

    async fn announce(&self, id: &str, spec: &EventSpec) -> Result<AnnouncementMsg> {
        Ok(self.0.announce(id, spec).await?)
    }

    async fn announcement(&self, id: &str) -> Result<AnnouncementMsg> {
        Ok(self.0.announcement(id).await?)
    }

    async fn attestation(&self, id: &str) -> Result<Option<AttestationMsg>> {
        Ok(self.0.attest(id).await?)
    }
}

// ---- Chain: Zakura JSON-RPC -----------------------------------------------------------

pub struct ZakuraRpc(pub Node);

impl ZakuraRpc {
    pub fn new(url: &str) -> Self {
        Self(Node::new(url))
    }
}

#[async_trait]
impl Chain for ZakuraRpc {
    fn name(&self) -> String {
        "zakura".into()
    }

    async fn network(&self) -> Result<(LocalNetwork, NetworkType)> {
        let info = self.0.blockchain_info().await?;
        let net = match info["chain"].as_str() {
            Some("main") => NetworkType::Main,
            Some("test") => NetworkType::Test,
            _ => NetworkType::Regtest,
        };
        let p = params::from_node(&info["upgrades"]).ok_or(Error::Chain("node lacks NU6.3".into()))?;
        Ok((p, net))
    }

    async fn height(&self) -> Result<u32> {
        Ok(self.0.height().await?)
    }

    async fn send(&self, raw: &[u8]) -> Result<()> {
        self.0.send(raw).await?;
        Ok(())
    }

    async fn mined_height(&self, txid: &[u8; 32]) -> Result<Option<u32>> {
        Ok(self.0.mined_height(&txid_hex(txid)).await.ok().flatten())
    }

    async fn utxos(&self, addr: &TransparentAddress, net: NetworkType) -> Result<Vec<Utxo>> {
        Ok(self.0.utxos(&zdlc::keys::encode(addr, net)).await?)
    }

    async fn ironwood_anchor(&self, height: u32) -> Result<orchard::Anchor> {
        Ok(self.0.ironwood_anchor(height).await?)
    }

    async fn notes(&self, fvk: &FullViewingKey, from: u32, to: u32) -> Result<Vec<OwnedNote>> {
        if to < from {
            return Ok(vec![]);
        }
        Ok(notes::unspent(&self.0, fvk, from, to).await?)
    }

    async fn spends(&self, fvk: &FullViewingKey, n: &[OwnedNote], anchor_height: u32) -> Result<(Vec<IronwoodSpend>, orchard::Anchor)> {
        Ok(notes::spends(&self.0, fvk, n, anchor_height).await?)
    }
}
