//! Minimal JSON-RPC client for a Zakura node. Errors never carry the node's URL: they
//! can reach an app's users (a contract's note), and the endpoint is private.

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
            .map_err(|e| Error::Rpc(format!("{method}: {}", e.without_url())))?
            .json()
            .await
            .map_err(|e| Error::Rpc(format!("{method}: {}", e.without_url())))?;
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

    /// Unspent transparent outputs of an address: (txid internal order, index, zat, height).
    pub async fn utxos(&self, address: &str) -> Result<Vec<([u8; 32], u32, u64, u32)>> {
        let v = self
            .call("getaddressutxos", json!([{"addresses": [address]}]))
            .await?;
        let mut out = vec![];
        for u in v.as_array().cloned().unwrap_or_default() {
            out.push((
                crate::txid_from_hex(u["txid"].as_str().unwrap_or_default())?,
                u["outputIndex"].as_u64().unwrap_or_default() as u32,
                u["satoshis"].as_u64().unwrap_or_default(),
                u["height"].as_u64().unwrap_or_default() as u32,
            ));
        }
        Ok(out)
    }

    /// Waits (polling) until the tip reaches `h`. For networks without `generate`.
    pub async fn wait_height(&self, h: u32) -> Result<u32> {
        loop {
            let tip = self.height().await?;
            if tip >= h {
                return Ok(tip);
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    /// Waits until a transaction is mined; returns its height.
    pub async fn wait_mined(&self, txid_display: &str) -> Result<u32> {
        loop {
            if let Ok(Some(h)) = self.mined_height(txid_display).await {
                return Ok(h);
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        }
    }

    /// The Ironwood note commitment tree root at `height`: a current anchor for outputs.
    pub async fn ironwood_anchor(&self, height: u32) -> Result<orchard::Anchor> {
        let v = self.call("z_gettreestate", json!([height.to_string()])).await?;
        let hex_root = v["ironwood"]["commitments"]["finalRoot"]
            .as_str()
            .ok_or(Error::Rpc("z_gettreestate: no ironwood root".into()))?;
        let mut b: [u8; 32] = hex::decode(hex_root)
            .map_err(|e| Error::Rpc(format!("ironwood root: {e}")))?
            .try_into()
            .map_err(|_| Error::Rpc("ironwood root length".into()))?;
        if let Some(a) = Option::<orchard::Anchor>::from(orchard::Anchor::from_bytes(b)) {
            return Ok(a);
        }
        b.reverse();
        Option::<orchard::Anchor>::from(orchard::Anchor::from_bytes(b))
            .ok_or(Error::Rpc("ironwood root is not a field element".into()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn errors_do_not_carry_the_url() {
        // Nothing listens on port 1: the request fails before any response.
        let e = Node::new("http://127.0.0.1:1/").call("getblockcount", json!([])).await.unwrap_err().to_string();
        assert!(e.starts_with("rpc: getblockcount: ") || e.contains("getblockcount: "), "{e}");
        assert!(!e.contains("127.0.0.1") && !e.contains("http://"), "the URL leaked: {e}");
    }
}
