//! A chain watcher for one contract. It holds no keys: only the fully signed refund,
//! which pays both parties their collateral and can be broadcast by anyone. It scans each
//! new block for a spend of the funding output (a close, a CET or the refund) and, if the
//! output is still unspent once the refund is valid, broadcasts the refund. The kit is a
//! JSON file, so the watcher survives restarts and can run on another machine.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{contract::Contract, node::Node, txid_hex, Error, Result};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Status {
    Watching,
    /// The refund was broadcast and awaits a block.
    RefundSent { txid: String },
    /// The funding output was spent.
    Settled { txid: String, height: u32, by_refund: bool },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Kit {
    pub label: String,
    /// Display-order hex.
    pub funding_txid: String,
    pub funding_index: u32,
    pub refund_height: u32,
    pub refund_txid: String,
    pub refund_hex: String,
    /// Last block scanned.
    pub scanned_to: u32,
    pub status: Status,
}

impl Kit {
    /// A kit for `c`, scanning from `from` (the block before funding is broadcast).
    pub fn new(label: &str, c: &Contract, from: u32) -> Result<Self> {
        let refund = c.refund()?;
        Ok(Self {
            label: label.into(),
            funding_txid: txid_hex(&c.txs.funding_txid),
            funding_index: c.txs.funding_coin.index,
            refund_height: c.terms.refund_height,
            refund_txid: txid_hex(&refund.txid),
            refund_hex: hex::encode(refund.raw),
            scanned_to: from,
            status: Status::Watching,
        })
    }

    pub fn load(path: &str) -> Result<Self> {
        let b = std::fs::read(path).map_err(|e| Error::Build(format!("kit {path}: {e}")))?;
        serde_json::from_slice(&b).map_err(|e| Error::Build(format!("kit {path}: {e}")))
    }

    pub fn save(&self, path: &str) -> Result<()> {
        let tmp = format!("{path}.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).unwrap())
            .and_then(|_| std::fs::rename(&tmp, path))
            .map_err(|e| Error::Build(format!("kit {path}: {e}")))
    }

    /// One pass: scan new blocks, then broadcast the refund if it is due and nothing has
    /// spent the funding output. Returns the status after the pass.
    pub async fn step(&mut self, node: &Node) -> Result<Status> {
        if matches!(self.status, Status::Settled { .. }) {
            return Ok(self.status.clone());
        }
        let tip = node.height().await?;
        for h in self.scanned_to + 1..=tip {
            let block = node.call("getblock", json!([h.to_string(), 2])).await?;
            for t in block["tx"].as_array().cloned().unwrap_or_default() {
                let spends = t["vin"].as_array().is_some_and(|vin| {
                    vin.iter().any(|i| {
                        i["txid"] == self.funding_txid.as_str()
                            && i["vout"].as_u64() == Some(self.funding_index as u64)
                    })
                });
                if spends {
                    let txid = t["txid"].as_str().unwrap_or_default().to_string();
                    let by_refund = txid == self.refund_txid;
                    self.status = Status::Settled { txid, height: h, by_refund };
                    self.scanned_to = h;
                    return Ok(self.status.clone());
                }
            }
            self.scanned_to = h;
        }
        if self.status == Status::Watching && tip >= self.refund_height {
            let raw = hex::decode(&self.refund_hex).map_err(|e| Error::Build(e.to_string()))?;
            let txid = node.send(&raw).await?;
            self.status = Status::RefundSent { txid };
        }
        Ok(self.status.clone())
    }
}

// ---- Payment links ------------------------------------------------------------------

/// What a payment link (deposit receiver or leg) has received.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Payment {
    /// Nothing confirmed yet.
    Waiting,
    /// Confirmed, but short: show a top-up link for `want - got` to the same address.
    Underpaid { got: u64, want: u64 },
    /// Enough. `over` stays with the payer: shielded balance (deposit) or funding change
    /// (leg).
    Paid { got: u64, over: u64 },
}

fn payment(got: u64, want: u64) -> Payment {
    match got {
        0 => Payment::Waiting,
        g if g < want => Payment::Underpaid { got: g, want },
        g => Payment::Paid { got: g, over: g - want },
    }
}

/// A deposit receiver (path A): notes to `fvk` confirmed `confirmations` deep, from the
/// height the link was made.
pub async fn deposit(
    node: &Node,
    fvk: &orchard::keys::FullViewingKey,
    from: u32,
    want: u64,
    confirmations: u32,
) -> Result<(Payment, Vec<crate::notes::OwnedNote>)> {
    let tip = node.height().await?;
    let deep = (tip + 1).saturating_sub(confirmations.max(1));
    if deep < from {
        return Ok((Payment::Waiting, vec![]));
    }
    let notes = crate::notes::unspent(node, fvk, from, deep).await?;
    Ok((payment(notes.iter().map(|n| n.value()).sum(), want), notes))
}

/// A leg (path B): its unspent coins confirmed `confirmations` deep.
pub async fn leg(
    node: &Node,
    net: zcash_protocol::consensus::NetworkType,
    leg: &crate::leg::Leg,
    want: u64,
    confirmations: u32,
) -> Result<(Payment, Vec<crate::tx::Coin>)> {
    let tip = node.height().await?;
    let coins: Vec<crate::tx::Coin> = node
        .utxos(&crate::keys::encode(&leg.address(), net))
        .await?
        .into_iter()
        .filter(|(_, _, _, h)| *h > 0 && tip + 1 - h >= confirmations.max(1))
        .map(|(txid, i, v, _)| leg.coin(txid, i, v))
        .collect();
    Ok((payment(coins.iter().map(|c| c.value).sum(), want), coins))
}
