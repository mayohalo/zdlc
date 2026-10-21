//! The zDLC kit as a Node-API addon: a self-custodial zDLC node running inside a Node
//! process (keys, SQLite database, the contract loop), with the same operations as
//! `zdlc-node`'s REST API. `@zdlc/sdk` wraps it in a typed class.

use std::sync::Arc;

use napi::{bindgen_prelude::*, tokio::task::JoinHandle};
use napi_derive::napi;
use serde_json::{json, Value};
use zdlc_kit::{impls::*, templates::Template, Funding, Zdlc};

fn js(e: impl std::fmt::Display) -> Error {
    Error::from_reason(e.to_string())
}

#[napi(object)]
pub struct NodeConfig {
    /// 32 bytes of hex: every key of this node derives from it.
    pub seed: String,
    /// A Zakura JSON-RPC endpoint.
    pub rpc: String,
    /// The desk (mailbox) URL.
    pub desk: String,
    /// The oracles every contract settles on (n-of-n).
    pub oracles: Vec<String>,
    /// SQLite database path (":memory:" for none).
    pub db: String,
    pub block_secs: Option<i64>,
    pub auto_shield: Option<bool>,
    pub recovery_records: Option<bool>,
}

#[napi]
pub struct ZdlcNode {
    z: Arc<Zdlc>,
    run: Option<JoinHandle<()>>,
}

fn funding(f: &str) -> Result<Funding> {
    match f {
        "treasury" => Ok(Funding::Treasury),
        "deposit" => Ok(Funding::Deposit),
        _ => Err(js("funding: \"treasury\" or \"deposit\"")),
    }
}

#[napi]
impl ZdlcNode {
    /// Opens the node and starts its loop (a step a second).
    #[napi(factory)]
    pub async fn open(cfg: NodeConfig) -> Result<ZdlcNode> {
        let seed: [u8; 32] = hex::decode(&cfg.seed).ok().and_then(|b| b.try_into().ok()).ok_or(js("seed: 32 bytes of hex"))?;
        let wallet = zdlc::wallet::Wallet::from_seed(seed);
        let mut b = Zdlc::builder()
            .seed(seed)
            .chain(Arc::new(ZakuraRpc::new(&cfg.rpc)))
            .storage(Arc::new(SqliteStorage::open(&cfg.db).map_err(js)?))
            .transport(Arc::new(DeskTransport::new(&cfg.desk, wallet.identity_key())))
            .block_secs(cfg.block_secs.unwrap_or(75))
            .auto_shield(cfg.auto_shield.unwrap_or(true))
            .recovery_records(cfg.recovery_records.unwrap_or(true));
        for o in &cfg.oracles {
            b = b.oracle(Arc::new(HttpOracle::new(o)));
        }
        let z = Arc::new(b.build().await.map_err(js)?);
        let run = napi::tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
        Ok(ZdlcNode { z, run: Some(run) })
    }

    #[napi(getter)]
    pub fn identity(&self) -> String {
        self.z.identity()
    }

    #[napi(getter)]
    pub fn treasury(&self) -> String {
        self.z.treasury_address()
    }

    #[napi]
    pub async fn info(&self) -> Result<Value> {
        self.z.info().await.map_err(js)
    }

    /// Offers `template` to `counterparty`; returns the contract id.
    #[napi]
    pub async fn offer(&self, counterparty: String, template: Value) -> Result<String> {
        let t: Template = serde_json::from_value(template).map_err(js)?;
        self.z.offer(t, &counterparty).await.map_err(js)
    }

    #[napi]
    pub async fn accept(&self, id: String, how: String) -> Result<()> {
        self.z.accept(&id, funding(&how)?).await.map_err(js)
    }

    #[napi]
    pub async fn decline(&self, id: String) -> Result<()> {
        self.z.decline(&id).await.map_err(js)
    }

    #[napi]
    pub async fn contracts(&self) -> Result<Value> {
        Ok(json!(self.z.contracts().await.map_err(js)?.iter().map(|r| r.view()).collect::<Vec<_>>()))
    }

    /// The contract, or null.
    #[napi]
    pub async fn contract(&self, id: String) -> Result<Value> {
        Ok(self.z.contract(&id).await.map_err(js)?.map(|r| r.view()).unwrap_or(Value::Null))
    }

    /// Events after `after`: `{ events, next }`, each event with its `seq`.
    #[napi]
    pub async fn events(&self, after: i64, limit: u32) -> Result<Value> {
        let evs = self.z.events_after(after.max(0) as u64, limit.clamp(1, 1000) as usize).await.map_err(js)?;
        let next = evs.last().map(|e| e.0 as i64).unwrap_or(after);
        let list: Vec<Value> = evs
            .into_iter()
            .map(|(seq, e)| {
                let mut v = json!(e);
                v["seq"] = json!(seq);
                v
            })
            .collect();
        Ok(json!({"events": list, "next": next}))
    }

    #[napi]
    pub async fn balance(&self) -> Result<Value> {
        Ok(json!(self.z.balance().await.map_err(js)?))
    }

    #[napi]
    pub async fn withdraw(&self, to: String) -> Result<Vec<String>> {
        self.z.withdraw(&to).await.map_err(js)
    }

    #[napi]
    pub async fn recover(&self, from: u32, gap: u32) -> Result<Value> {
        Ok(json!({"records": self.z.recover(from, gap).await.map_err(js)?}))
    }

    /// The deposit link of a contract as an SVG QR code.
    #[napi]
    pub async fn deposit_qr(&self, id: String) -> Result<String> {
        let rec = self.z.contract(&id).await.map_err(js)?.ok_or(js(format!("no contract {id}")))?;
        let d = rec.deposit.ok_or(js("no deposit link"))?;
        zdlc::paylink::qr_svg(&d.uri).map_err(js)
    }

    /// Stops the loop. The database keeps everything; open again to resume.
    #[napi]
    pub fn close(&mut self) {
        if let Some(r) = self.run.take() {
            r.abort();
        }
    }
}
