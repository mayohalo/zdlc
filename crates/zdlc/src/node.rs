//! Minimal JSON-RPC client for a Zakura node.

use serde_json::{json, Value};

use crate::{Error, Result};

pub struct Node {
    url: String,
    http: reqwest::Client,
}

impl Node {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let v: Value = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Rpc(format!("{method}: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Rpc(format!("{method}: {e}")))?;
        if !v["error"].is_null() {
            return Err(Error::Rpc(format!("{method}: {}", v["error"])));
        }
        Ok(v["result"].clone())
    }

    pub async fn blockchain_info(&self) -> Result<Value> {
        self.call("getblockchaininfo", json!([])).await
    }

    pub async fn height(&self) -> Result<u32> {
        let v = self.call("getblockcount", json!([])).await?;
        v.as_u64()
            .map(|h| h as u32)
            .ok_or(Error::Rpc("getblockcount".into()))
    }

    pub async fn generate(&self, n: u32) -> Result<()> {
        self.call("generate", json!([n])).await.map(|_| ())
    }

    /// Mines until the tip is `h`.
    pub async fn mine_to(&self, h: u32) -> Result<()> {
        let now = self.height().await?;
        if h > now {
            self.generate(h - now).await?;
        }
        Ok(())
    }

    pub async fn send(&self, raw: &[u8]) -> Result<String> {
        let v = self
            .call("sendrawtransaction", json!([hex::encode(raw)]))
            .await?;
        v.as_str()
            .map(str::to_owned)
            .ok_or(Error::Rpc("sendrawtransaction".into()))
    }

    pub async fn tx(&self, txid_display: &str) -> Result<Value> {
        self.call("getrawtransaction", json!([txid_display, 1]))
            .await
    }

    pub async fn block(&self, height: u32) -> Result<Value> {
        self.call("getblock", json!([height.to_string(), 1])).await
    }

    /// Mined height of a transaction, if it is in a block.
    pub async fn mined_height(&self, txid_display: &str) -> Result<Option<u32>> {
        let v = self.tx(txid_display).await?;
        Ok(v["height"].as_u64().map(|h| h as u32))
    }

    /// Ironwood pool value, in zatoshis, from `getblockchaininfo`.
    pub async fn pool_zat(&self, id: &str) -> Result<i64> {
        let info = self.blockchain_info().await?;
        info["valuePools"]
            .as_array()
            .and_then(|a| a.iter().find(|p| p["id"] == id))
            .and_then(|p| p["chainValueZat"].as_i64())
            .ok_or(Error::Rpc(format!("valuePools {id}")))
    }
}
