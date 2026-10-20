//! Money in and out of the zDLC wallet around a contract: auto-shielding transparent
//! payouts (CETs, refunds, leg reclaims) into Ironwood, and sending shielded balance on
//! (withdraw, or a deposit paid from another wallet).

use orchard::keys::{FullViewingKey, SpendAuthorizingKey, SpendingKey};
use secp256k1::SecretKey;
use zcash_protocol::local_consensus::LocalNetwork;

use crate::{
    node::Node,
    notes::{self, OwnedNote},
    tx::{self, Coin, Pay, Spec},
    Error, Result,
};

/// The zDLC wallet's shielded balance across contract indices `0..gap`: notes to each
/// deposit key, from height `from`.
pub async fn balance(node: &Node, wallet: &crate::wallet::Wallet, from: u32, gap: u32) -> Result<Vec<(u32, Vec<OwnedNote>)>> {
    let tip = node.height().await?;
    let mut out = vec![];
    for i in 0..gap {
        let n = notes::unspent(node, &wallet.deposit_fvk(i), from, tip).await?;
        if !n.is_empty() {
            out.push((i, n));
        }
    }
    Ok(out)
}

/// Withdraws everything held by contract `i`'s deposit key to a unified address the
/// user scans or pastes (its Ironwood receiver).
pub async fn withdraw(node: &Node, p: &LocalNetwork, wallet: &crate::wallet::Wallet, i: u32, notes: &[OwnedNote], to_ua: &str) -> Result<tx::Final> {
    let (_, to) = crate::paylink::decode_unified(to_ua).ok_or(Error::Build("not a unified address with an Ironwood receiver".into()))?;
    send_shielded(node, p, &wallet.deposit_sk(i), notes, to, None, wallet.deposit_address(i), None).await
}

/// Auto-shield: moves a payout that landed on contract `i`'s transparent payout address
/// (a CET, the refund or a leg reclaim) into the zDLC wallet, at its deposit address.
pub async fn auto_shield(node: &Node, p: &LocalNetwork, net: zcash_protocol::consensus::NetworkType, wallet: &crate::wallet::Wallet, i: u32) -> Result<Option<tx::Final>> {
    let key = wallet.payout_key(i);
    let coins: Vec<(Coin, SecretKey)> = node
        .utxos(&crate::keys::encode(&wallet.payout_address(i), net))
        .await?
        .into_iter()
        .filter(|u| u.3 > 0)
        .map(|(txid, index, value, _)| (Coin { txid, index, value, spend: tx::Spend::P2pkh(crate::keys::pubkey(&key)) }, key))
        .collect();
    if coins.is_empty() {
        return Ok(None);
    }
    let tip = node.height().await?;
    let anchor = node.ironwood_anchor(tip).await?;
    shield(p, tip, &coins, wallet.deposit_address(i), anchor).map(Some)
}

/// Moves transparent P2PKH coins (each with its key) to an Ironwood address, less the fee.
pub fn shield(p: &LocalNetwork, tip: u32, coins: &[(Coin, SecretKey)], to: orchard::Address, anchor: orchard::Anchor) -> Result<tx::Final> {
    let total: u64 = coins.iter().map(|(c, _)| c.value).sum();
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 1 + crate::contract::EXPIRY_DELTA,
        lock_height: None,
        inputs: coins.iter().map(|(c, _)| c.clone()).collect(),
        outputs: vec![Pay::Ironwood(to, total)],
        ironwood_anchor: Some(anchor),
        ironwood_spends: vec![],
    };
    let fee = tx::fee(p, &spec)?;
    spec.outputs = vec![Pay::Ironwood(to, total.checked_sub(fee).ok_or(Error::Build("coins below the fee".into()))?)];
    let mut pczt = tx::build(p, &spec)?;
    for (i, (_, k)) in coins.iter().enumerate() {
        pczt = tx::sign(pczt, i, k)?;
    }
    tx::extract(pczt)
}

/// Where a payment from notes goes.
pub enum To {
    Ironwood(orchard::Address, Option<Vec<u8>>),
    /// A transparent address: a leg, or a payout.
    Transparent(zcash_transparent::address::TransparentAddress),
}

/// Pays `amount` from `notes` (all to `sk`) to `to`, with change to `change`. With
/// `amount = None`, sends everything less the fee.
#[allow(clippy::too_many_arguments)]
pub async fn send_shielded(
    node: &Node,
    p: &LocalNetwork,
    sk: &SpendingKey,
    notes: &[OwnedNote],
    to: orchard::Address,
    amount: Option<u64>,
    change: orchard::Address,
    memo: Option<Vec<u8>>,
) -> Result<tx::Final> {
    pay_from_notes(node, p, sk, notes, To::Ironwood(to, memo), amount, change).await
}

/// [`send_shielded`] to any recipient.
pub async fn pay_from_notes(
    node: &Node,
    p: &LocalNetwork,
    sk: &SpendingKey,
    notes: &[OwnedNote],
    to: To,
    amount: Option<u64>,
    change: orchard::Address,
) -> Result<tx::Final> {
    let fvk = FullViewingKey::from(sk);
    let tip = node.height().await?;
    let (spends, anchor) = notes::spends(node, &fvk, notes, tip).await?;
    let have: u64 = notes.iter().map(OwnedNote::value).sum();
    let pay = |v| match &to {
        To::Ironwood(a, Some(m)) => Pay::IronwoodMemo(*a, v, m.clone()),
        To::Ironwood(a, None) => Pay::Ironwood(*a, v),
        To::Transparent(t) => Pay::Transparent(*t, v),
    };
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 1 + crate::contract::EXPIRY_DELTA,
        lock_height: None,
        inputs: vec![],
        outputs: match amount {
            None => vec![pay(1)],
            Some(_) => vec![pay(1), Pay::Ironwood(change, 1)],
        },
        ironwood_anchor: Some(anchor),
        ironwood_spends: spends,
    };
    let fee = tx::fee(p, &spec)?;
    let short = || Error::Build(format!("notes hold {have} zat, short of the amount and fee {fee}"));
    spec.outputs = match amount {
        None => vec![pay(have.checked_sub(fee).ok_or_else(short)?)],
        Some(v) => vec![pay(v), Pay::Ironwood(change, have.checked_sub(v + fee).ok_or_else(short)?)],
    };
    let (pczt, n) = tx::sign_ironwood(tx::build(p, &spec)?, &SpendAuthorizingKey::from(sk))?;
    if n != notes.len() {
        return Err(Error::Build(format!("signed {n} of {} spends", notes.len())));
    }
    tx::extract(pczt)
}
