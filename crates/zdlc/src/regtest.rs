//! Regtest helpers shared by the gate tests and `zdlc-demo`: dev parties funded from the
//! miner's coinbase on `infra/zakura`.

use orchard::keys::{FullViewingKey, Scope, SpendingKey};
use serde_json::json;
use zcash_protocol::local_consensus::LocalNetwork;

use crate::{
    contract::{Party, PartyInfo},
    keys,
    node::Node,
    params::{self, NU7_AT},
    tx::{self, Coin, Pay, Spec, Spend},
    txid_from_hex, txid_hex, Error, Result,
};

pub const COINBASE_MATURITY: u32 = 100;

pub struct Regtest {
    pub node: Node,
    pub p: LocalNetwork,
}

impl Regtest {
    /// Connects and checks the chain is the zdlc regtest (NU7 at `NU7_AT`).
    pub async fn connect(url: &str) -> Result<Self> {
        let node = Node::new(url);
        let info = node.blockchain_info().await?;
        let nu7 = &info["upgrades"][format!("{:08x}", params::BRANCH_NU7)]["activationheight"];
        if nu7 != NU7_AT {
            return Err(Error::Rpc(format!(
                "not the zdlc regtest (NU7 at {nu7}, want {NU7_AT}): infra/zakura/scripts/up.sh"
            )));
        }
        Ok(Self {
            node,
            p: params::regtest(Some(NU7_AT)),
        })
    }

    pub async fn tip(&self) -> Result<u32> {
        self.node.height().await
    }

    pub async fn mine(&self, n: u32) -> Result<()> {
        self.node.generate(n).await
    }

    /// Sends, mines one block, returns the height it was mined at.
    pub async fn send_confirm(&self, f: &tx::Final) -> Result<u32> {
        self.node.send(&f.raw).await?;
        self.mine(1).await?;
        self.node
            .mined_height(&txid_hex(&f.txid))
            .await?
            .ok_or(Error::Rpc("not mined".into()))
    }

    pub async fn outputs(&self, f: &tx::Final) -> Result<Vec<u64>> {
        let v = self.node.tx(&txid_hex(&f.txid)).await?;
        Ok(v["vout"]
            .as_array()
            .map(|a| a.iter().filter_map(|o| o["valueZat"].as_u64()).collect())
            .unwrap_or_default())
    }

    /// Pays the most recently matured coinbase to fresh P2PKH coins of `amount` for two
    /// dev parties, mines it, and returns the parties (keys derived from `tag`).
    pub async fn faucet(&self, tag: &str, amount: u64) -> Result<(Party, Party)> {
        let tip = self.tip().await?;
        if tip < COINBASE_MATURITY {
            self.node.mine_to(COINBASE_MATURITY).await?;
        }
        let tip = self.tip().await?;
        let h = tip + 1 - COINBASE_MATURITY;
        let b = self.node.call("getblock", json!([h.to_string(), 2])).await?;
        let cb = &b["tx"][0];
        let value = cb["vout"][0]["valueZat"]
            .as_u64()
            .ok_or(Error::Rpc("coinbase value".into()))?;
        let miner = keys::miner();
        let to_miner = keys::p2pkh(&keys::pubkey(&miner));
        let (a, b) = (dev_party(tag, "alice", 0xA1), dev_party(tag, "bob", 0xB0));
        let mut spec = Spec {
            target_height: tip + 1,
            expiry_height: tip + 20,
            lock_height: None,
            inputs: vec![Coin {
                txid: txid_from_hex(cb["txid"].as_str().unwrap_or_default())?,
                index: 0,
                value,
                spend: Spend::P2pkh(keys::pubkey(&miner)),
            }],
            outputs: vec![
                Pay::Transparent(a.change, amount),
                Pay::Transparent(b.change, amount),
                Pay::Transparent(to_miner, 1),
            ],
            ironwood_anchor: None,
        };
        let fee = tx::fee(&self.p, &spec)?;
        spec.outputs[2] = Pay::Transparent(
            to_miner,
            value
                .checked_sub(2 * amount + fee)
                .ok_or(Error::Build("coinbase too small".into()))?,
        );
        let f = tx::extract(tx::sign(tx::build(&self.p, &spec)?, 0, &miner)?)?;
        self.send_confirm(&f).await?;
        Ok((a.into_party(f.txid, 0, amount), b.into_party(f.txid, 1, amount)))
    }
}

/// A dev party's keys before it holds coins.
pub struct DevParty {
    funding_sk: secp256k1::SecretKey,
    input_sk: secp256k1::SecretKey,
    close_fvk: FullViewingKey,
    pub change: zcash_transparent::address::TransparentAddress,
    payout: zcash_transparent::address::TransparentAddress,
}

pub fn dev_party(tag: &str, who: &str, close_seed: u8) -> DevParty {
    let funding_sk = keys::dev_key(&format!("{tag}/{who}/funding"));
    let input_sk = keys::dev_key(&format!("{tag}/{who}/input"));
    let mut seed = [close_seed; 32];
    seed[..tag.len().min(31)].copy_from_slice(&tag.as_bytes()[..tag.len().min(31)]);
    let close_sk = (0u8..=255)
        .find_map(|i| {
            seed[31] = i;
            Option::<SpendingKey>::from(SpendingKey::from_bytes(seed))
        })
        .expect("a valid spending key");
    DevParty {
        funding_sk,
        input_sk,
        close_fvk: FullViewingKey::from(&close_sk),
        change: keys::p2pkh(&keys::pubkey(&input_sk)),
        payout: keys::p2pkh(&keys::pubkey(&keys::dev_key(&format!("{tag}/{who}/payout")))),
    }
}

impl DevParty {
    pub fn into_party(self, txid: [u8; 32], index: u32, value: u64) -> Party {
        let close = self.close_fvk.address_at(0u32, Scope::External);
        Party {
            funding_sk: self.funding_sk,
            input_keys: vec![self.input_sk],
            close_fvk: self.close_fvk,
            info: PartyInfo {
                funding_pubkey: keys::pubkey(&self.funding_sk),
                inputs: vec![Coin {
                    txid,
                    index,
                    value,
                    spend: Spend::P2pkh(keys::pubkey(&self.input_sk)),
                }],
                change: self.change,
                payout: self.payout,
                close,
            },
        }
    }
}
