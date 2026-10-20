//! The recovery record: seed-only recovery of a contract's refund.
//!
//! After setup and before the funding transaction goes out, a party pays its own
//! Ironwood address for the contract (`Wallet::close_address(i)`) a small note whose
//! encrypted memo carries everything needed to rebuild the refund: the funding outpoint
//! and value, both collaterals, both payout addresses, the heights, the counterparty's
//! funding key and its signature on the refund. Only the wallet's viewing key can read
//! it. A wallet restored from its seed scans the chain, finds the record, re-derives its
//! own keys and re-signs the exact refund, with no counterparty and no saved state.
//!
//! The record cannot ride in the funding transaction itself: the counterparty's refund
//! signature commits to the funding txid, which would commit to the record.

use secp256k1::{ecdsa, PublicKey};
use serde_json::json;
use zcash_note_encryption::try_note_decryption;
use zcash_primitives::transaction::Transaction;
use zcash_protocol::{consensus::BranchId, local_consensus::LocalNetwork};
use zcash_transparent::address::TransparentAddress;

use crate::{
    contract::{self, Contract},
    node::Node,
    script,
    tx::{self, Coin, Pay, Spec, Spend},
    wallet::Wallet,
    Error, Result,
};

pub const MAGIC: &[u8; 4] = b"ZDLC";
pub const VERSION: u8 = 1;
/// Value of the record note: enough to be an ordinary payment, small enough not to matter.
pub const RECORD_VALUE: u64 = 10_000;
const LEN: usize = 4 + 1 + 1 + 4 + 32 + 4 + 8 * 3 + 4 * 2 + 33 + 20 * 2 + 64;

#[derive(Clone, Debug, PartialEq)]
pub struct Record {
    /// The wallet's contract index: which keys to re-derive.
    pub index: u32,
    pub is_offerer: bool,
    pub funding_txid: [u8; 32],
    pub funding_index: u32,
    pub funding_value: u64,
    pub offer_collateral: u64,
    pub accept_collateral: u64,
    pub refund_height: u32,
    pub sign_height: u32,
    pub counterparty_pubkey: PublicKey,
    pub offer_payout: [u8; 20],
    pub accept_payout: [u8; 20],
    pub their_refund_sig: ecdsa::Signature,
}

fn pkh(a: &TransparentAddress) -> Result<[u8; 20]> {
    match a {
        TransparentAddress::PublicKeyHash(h) => Ok(*h),
        _ => Err(Error::Build("recovery needs P2PKH payout addresses".into())),
    }
}

impl Record {
    pub fn from_contract(c: &Contract, index: u32) -> Result<Self> {
        let them = if c.is_offerer { &c.accept } else { &c.offer };
        Ok(Self {
            index,
            is_offerer: c.is_offerer,
            funding_txid: c.txs.funding_txid,
            funding_index: c.txs.funding_coin.index,
            funding_value: c.txs.funding_coin.value,
            offer_collateral: c.terms.offer_collateral,
            accept_collateral: c.terms.accept_collateral,
            refund_height: c.terms.refund_height,
            sign_height: c.terms.sign_height,
            counterparty_pubkey: them.funding_pubkey,
            offer_payout: pkh(&c.offer.payout)?,
            accept_payout: pkh(&c.accept.payout)?,
            their_refund_sig: c.their_refund_sig(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(LEN);
        b.extend_from_slice(MAGIC);
        b.push(VERSION);
        b.push(self.is_offerer as u8);
        b.extend_from_slice(&self.index.to_le_bytes());
        b.extend_from_slice(&self.funding_txid);
        b.extend_from_slice(&self.funding_index.to_le_bytes());
        b.extend_from_slice(&self.funding_value.to_le_bytes());
        b.extend_from_slice(&self.offer_collateral.to_le_bytes());
        b.extend_from_slice(&self.accept_collateral.to_le_bytes());
        b.extend_from_slice(&self.refund_height.to_le_bytes());
        b.extend_from_slice(&self.sign_height.to_le_bytes());
        b.extend_from_slice(&self.counterparty_pubkey.serialize());
        b.extend_from_slice(&self.offer_payout);
        b.extend_from_slice(&self.accept_payout);
        b.extend_from_slice(&self.their_refund_sig.serialize_compact());
        debug_assert_eq!(b.len(), LEN);
        b
    }

    /// Parses a memo; `None` if it is not a zdlc record.
    pub fn decode(memo: &[u8]) -> Option<Self> {
        if memo.len() < LEN || &memo[..4] != MAGIC || memo[4] != VERSION {
            return None;
        }
        let mut at = 5;
        let mut take = |n: usize| {
            let s = &memo[at..at + n];
            at += n;
            s
        };
        let is_offerer = take(1)[0] == 1;
        let u32_ = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap());
        let u64_ = |s: &[u8]| u64::from_le_bytes(s.try_into().unwrap());
        let index = u32_(take(4));
        let funding_txid = take(32).try_into().ok()?;
        let funding_index = u32_(take(4));
        let funding_value = u64_(take(8));
        let offer_collateral = u64_(take(8));
        let accept_collateral = u64_(take(8));
        let refund_height = u32_(take(4));
        let sign_height = u32_(take(4));
        let counterparty_pubkey = PublicKey::from_slice(take(33)).ok()?;
        let offer_payout = take(20).try_into().ok()?;
        let accept_payout = take(20).try_into().ok()?;
        let their_refund_sig = ecdsa::Signature::from_compact(take(64)).ok()?;
        Some(Self {
            index,
            is_offerer,
            funding_txid,
            funding_index,
            funding_value,
            offer_collateral,
            accept_collateral,
            refund_height,
            sign_height,
            counterparty_pubkey,
            offer_payout,
            accept_payout,
            their_refund_sig,
        })
    }
}

/// The record transaction: `coin` (at `wallet.coin_address(i)`) pays `RECORD_VALUE` to
/// the contract's own Ironwood address with the record as memo; change back to the coin
/// address.
pub fn record_tx(
    p: &LocalNetwork,
    tip: u32,
    wallet: &Wallet,
    record: &Record,
    coin: Coin,
    anchor: orchard::Anchor,
) -> Result<tx::Final> {
    let i = record.index;
    let change = wallet.coin_address(i);
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 1 + contract::EXPIRY_DELTA,
        lock_height: None,
        inputs: vec![coin.clone()],
        outputs: vec![
            Pay::IronwoodMemo(wallet.close_address(i), RECORD_VALUE, record.encode()),
            Pay::Transparent(change, 1),
        ],
        ironwood_anchor: Some(anchor),
    };
    let fee = tx::fee(p, &spec)?;
    spec.outputs[1] = Pay::Transparent(
        change,
        coin.value
            .checked_sub(RECORD_VALUE + fee)
            .ok_or(Error::Build("record coin too small".into()))?,
    );
    tx::extract(tx::sign(tx::build(p, &spec)?, 0, &wallet.coin_key(i))?)
}

pub(crate) fn parse_tx(raw: &[u8]) -> Option<Transaction> {
    let branch = BranchId::try_from(u32::from_le_bytes(raw.get(8..12)?.try_into().ok()?)).ok()?;
    Transaction::read(raw, branch).ok()
}

/// Scans blocks `from..=to` for records addressed to the wallet's contract indices
/// `0..gap`, by trial decryption of every Ironwood output.
pub async fn scan(node: &Node, wallet: &Wallet, from: u32, to: u32, gap: u32) -> Result<Vec<Record>> {
    let ivks: Vec<_> = (0..gap).map(|i| (i, wallet.close_ivk(i))).collect();
    let mut found = vec![];
    for h in from..=to {
        let block = node.block(h).await?;
        for txid in block["tx"].as_array().cloned().unwrap_or_default() {
            let txid = txid.as_str().unwrap_or_default();
            let raw = node.call("getrawtransaction", json!([txid, 0])).await?;
            let raw = hex::decode(raw.as_str().unwrap_or_default())
                .map_err(|e| Error::Rpc(format!("tx hex: {e}")))?;
            let Some(t) = parse_tx(&raw) else { continue };
            let Some(bundle) = t.ironwood_bundle() else { continue };
            for a in bundle.actions() {
                let domain = orchard::note_encryption::IronwoodDomain::for_action(a);
                for (i, ivk) in &ivks {
                    if let Some((_, _, memo)) = try_note_decryption(&domain, ivk, a) {
                        if let Some(r) = Record::decode(&memo) {
                            if r.index == *i {
                                found.push(r);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(found)
}

/// Rebuilds and signs the refund from a record and the seed alone.
pub fn rebuild_refund(p: &LocalNetwork, wallet: &Wallet, r: &Record) -> Result<tx::Final> {
    let own = wallet.funding_key(r.index);
    let own_pk = crate::keys::pubkey(&own);
    let redeem = if r.is_offerer {
        script::multisig_2of2(&own_pk, &r.counterparty_pubkey)?
    } else {
        script::multisig_2of2(&r.counterparty_pubkey, &own_pk)?
    };
    let funding = Coin {
        txid: r.funding_txid,
        index: r.funding_index,
        value: r.funding_value,
        spend: Spend::P2sh(redeem),
    };
    let reserve = r.funding_value - r.offer_collateral - r.accept_collateral;
    let spec = contract::refund_spec(
        p,
        &funding,
        reserve,
        r.sign_height,
        r.refund_height,
        (TransparentAddress::PublicKeyHash(r.offer_payout), r.offer_collateral),
        (TransparentAddress::PublicKeyHash(r.accept_payout), r.accept_collateral),
    )?;
    let pczt = tx::append(tx::build(p, &spec)?, 0, r.their_refund_sig)?;
    tx::extract(tx::sign(pczt, 0, &own)?)
}
