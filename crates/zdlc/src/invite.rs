//! Memo invitations: a maker sends a private quote to one taker as a small shielded
//! payment to the taker's Ironwood inbox (`Wallet::close_address(0)`). The encrypted memo
//! carries the quote's id and terms, the maker's identity and the desk to meet at. The
//! quote itself sits on the desk unlisted (`POST /quotes/private`), so there is no public
//! RFQ or order board; the payment's value and fee make spam cost something.

use serde_json::json;
use zcash_note_encryption::try_note_decryption;
use zcash_protocol::local_consensus::LocalNetwork;

use crate::{
    node::Node,
    tx::{self, Coin, Pay, Spec},
    wallet::Wallet,
    Error, Result,
};

pub const MAGIC: &[u8; 8] = b"ZDLCINV1";
/// Value of the invitation note.
pub const INVITE_VALUE: u64 = 10_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Invitation {
    pub quote_id: String,
    pub maker: String,
    pub desk: String,
    pub q_recipient: u64,
    pub q_hedger: u64,
    pub premium: u64,
    pub maturity_blocks: u32,
    pub refund_blocks: u32,
    pub valid_until: u32,
}

impl Invitation {
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut b = MAGIC.to_vec();
        let qid = hex::decode(&self.quote_id).map_err(|e| Error::Build(e.to_string()))?;
        let maker = hex::decode(&self.maker).map_err(|e| Error::Build(e.to_string()))?;
        if qid.len() != 12 || maker.len() != 33 || self.desk.len() > 400 {
            return Err(Error::Build("invitation fields".into()));
        }
        b.extend(qid);
        b.extend(maker);
        for v in [self.q_recipient, self.q_hedger, self.premium] {
            b.extend(v.to_le_bytes());
        }
        for v in [self.maturity_blocks, self.refund_blocks, self.valid_until] {
            b.extend(v.to_le_bytes());
        }
        b.extend((self.desk.len() as u16).to_le_bytes());
        b.extend(self.desk.as_bytes());
        Ok(b)
    }

    pub fn decode(m: &[u8]) -> Option<Self> {
        if m.len() < 8 + 12 + 33 + 24 + 12 + 2 || &m[..8] != MAGIC {
            return None;
        }
        let mut at = 8;
        let mut take = |n: usize| {
            let s = &m[at..at + n];
            at += n;
            s
        };
        let quote_id = hex::encode(take(12));
        let maker = hex::encode(take(33));
        let u64_ = |s: &[u8]| u64::from_le_bytes(s.try_into().unwrap());
        let u32_ = |s: &[u8]| u32::from_le_bytes(s.try_into().unwrap());
        let (q_recipient, q_hedger, premium) = (u64_(take(8)), u64_(take(8)), u64_(take(8)));
        let (maturity_blocks, refund_blocks, valid_until) = (u32_(take(4)), u32_(take(4)), u32_(take(4)));
        let len = take(2);
        let len = len[0] as usize | (len[1] as usize) << 8;
        let desk = String::from_utf8(m.get(at..at + len)?.to_vec()).ok()?;
        Some(Self { quote_id, maker, desk, q_recipient, q_hedger, premium, maturity_blocks, refund_blocks, valid_until })
    }
}

/// Pays the taker's inbox `INVITE_VALUE` with the invitation as memo, from the maker's
/// treasury `coin`; change back to the treasury.
pub fn invitation_tx(
    p: &LocalNetwork,
    tip: u32,
    maker: &Wallet,
    coin: Coin,
    inbox: orchard::Address,
    inv: &Invitation,
    anchor: orchard::Anchor,
) -> Result<tx::Final> {
    let change = maker.coin_address(0);
    let mut spec = Spec {
        target_height: tip + 1,
        expiry_height: tip + 41,
        lock_height: None,
        inputs: vec![coin.clone()],
        outputs: vec![Pay::IronwoodMemo(inbox, INVITE_VALUE, inv.encode()?), Pay::Transparent(change, 1)],
        ironwood_anchor: Some(anchor),
        ironwood_spends: vec![],
    };
    let fee = tx::fee(p, &spec)?;
    spec.outputs[1] = Pay::Transparent(
        change,
        coin.value.checked_sub(INVITE_VALUE + fee).ok_or(Error::Build("coin too small".into()))?,
    );
    tx::extract(tx::sign(tx::build(p, &spec)?, 0, &maker.coin_key(0))?)
}

/// Invitations to `wallet`'s inbox in blocks `from..=to`.
pub async fn scan(node: &Node, wallet: &Wallet, from: u32, to: u32) -> Result<Vec<Invitation>> {
    let ivk = wallet.close_ivk(0);
    let mut out = vec![];
    for h in from..=to {
        let block = node.block(h).await?;
        for txid in block["tx"].as_array().cloned().unwrap_or_default() {
            let raw = node.call("getrawtransaction", json!([txid.as_str().unwrap_or_default(), 0])).await?;
            let raw = hex::decode(raw.as_str().unwrap_or_default()).map_err(|e| Error::Rpc(e.to_string()))?;
            let Some(t) = crate::recovery::parse_tx(&raw) else { continue };
            let Some(bundle) = t.ironwood_bundle() else { continue };
            for a in bundle.actions() {
                let domain = orchard::note_encryption::IronwoodDomain::for_action(a);
                if let Some((_, _, memo)) = try_note_decryption(&domain, &ivk, a) {
                    if let Some(i) = Invitation::decode(&memo) {
                        out.push(i);
                    }
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invitation_round_trip() {
        let i = Invitation {
            quote_id: "00112233445566778899aabb".into(),
            maker: format!("02{}", "ab".repeat(32)),
            desk: "https://desk.example".into(),
            q_recipient: 1_000_000,
            q_hedger: 1_000_000,
            premium: 5_000,
            maturity_blocks: 6,
            refund_blocks: 60,
            valid_until: 4_600_000,
        };
        let m = i.encode().unwrap();
        assert!(m.len() <= 512);
        assert_eq!(Invitation::decode(&m).unwrap(), i);
    }
}
