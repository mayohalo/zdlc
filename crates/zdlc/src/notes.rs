//! Ironwood notes the wallet can spend: found by trial decryption, witnessed from the
//! node's tree state.
//!
//! No full tree is kept. A note's witness starts from the commitment tree frontier of
//! the block before it (`z_gettreestate`), adds the commitments of its own block up to
//! and including it, then every commitment after it up to the anchor height. The
//! result's root must equal the node's root at that height.

use incrementalmerkletree::witness::IncrementalWitness;
use orchard::{
    keys::{FullViewingKey, PreparedIncomingViewingKey, Scope},
    note::{ExtractedNoteCommitment, Nullifier},
    note_encryption::IronwoodDomain,
    tree::{MerkleHashOrchard, MerklePath},
    Note,
};
use serde_json::json;
use zcash_note_encryption::try_note_decryption;
use zcash_primitives::merkle_tree::read_commitment_tree;

use crate::{node::Node, recovery::parse_tx, tx::IronwoodSpend, Error, Result};

/// A received note.
#[derive(Clone, Debug)]
pub struct OwnedNote {
    pub note: Note,
    pub height: u32,
    /// Internal byte order.
    pub txid: [u8; 32],
    pub action: usize,
    pub cmx: [u8; 32],
    pub memo: [u8; 512],
}

impl OwnedNote {
    pub fn value(&self) -> u64 {
        self.note.value().inner()
    }
}

/// Every Ironwood action of a block, in tree order: (txid, action index, cmx, nullifier).
type Act = orchard::Action<orchard::primitives::redpallas::Signature<orchard::primitives::redpallas::SpendAuth>>;

async fn block_actions(node: &Node, h: u32) -> Result<Vec<([u8; 32], usize, Act)>> {
    let block = node.block(h).await?;
    let mut out = vec![];
    for txid in block["tx"].as_array().cloned().unwrap_or_default() {
        let raw = node.call("getrawtransaction", json!([txid.as_str().unwrap_or_default(), 0])).await?;
        let raw = hex::decode(raw.as_str().unwrap_or_default()).map_err(|e| Error::Rpc(e.to_string()))?;
        let Some(t) = parse_tx(&raw) else { continue };
        let id = *t.txid().as_ref();
        if let Some(b) = t.ironwood_bundle() {
            for (i, a) in b.actions().iter().enumerate() {
                out.push((id, i, a.clone()));
            }
        }
    }
    Ok(out)
}

/// Notes to `fvk` (external and internal scope) received in blocks `from..=to`, and
/// whether each was spent there. Returns the unspent ones.
pub async fn unspent(node: &Node, fvk: &FullViewingKey, from: u32, to: u32) -> Result<Vec<OwnedNote>> {
    let ivks: Vec<PreparedIncomingViewingKey> = [Scope::External, Scope::Internal]
        .iter()
        .map(|s| PreparedIncomingViewingKey::new(&fvk.to_ivk(*s)))
        .collect();
    let mut found: Vec<(OwnedNote, Nullifier)> = vec![];
    let mut spent: Vec<Nullifier> = vec![];
    for h in from..=to {
        for (txid, i, a) in block_actions(node, h).await? {
            spent.push(*a.nullifier());
            let domain = IronwoodDomain::for_action(&a);
            for ivk in &ivks {
                if let Some((note, _, memo)) = try_note_decryption(&domain, ivk, &a) {
                    let nf = note.nullifier(fvk);
                    found.push((
                        OwnedNote { note, height: h, txid, action: i, cmx: a.cmx().to_bytes(), memo },
                        nf,
                    ));
                    break;
                }
            }
        }
    }
    Ok(found.into_iter().filter(|(_, nf)| !spent.contains(nf)).map(|(n, _)| n).collect())
}

fn tree_state(v: &serde_json::Value) -> Result<incrementalmerkletree::frontier::CommitmentTree<MerkleHashOrchard, 32>> {
    let hex_state = v["ironwood"]["commitments"]["finalState"].as_str().unwrap_or("000000");
    let bytes = hex::decode(hex_state).map_err(|e| Error::Rpc(format!("finalState: {e}")))?;
    read_commitment_tree::<MerkleHashOrchard, _, 32>(&bytes[..]).map_err(|e| Error::Rpc(format!("finalState: {e}")))
}

fn leaf(cmx: &[u8; 32]) -> Result<MerkleHashOrchard> {
    Option::from(ExtractedNoteCommitment::from_bytes(cmx))
        .map(|c| MerkleHashOrchard::from_cmx(&c))
        .ok_or(Error::Build("cmx".into()))
}

/// The note's Merkle path to the tree root at `anchor_height`, and that root.
pub async fn witness(node: &Node, n: &OwnedNote, anchor_height: u32) -> Result<(MerklePath, orchard::Anchor)> {
    let before = node.call("z_gettreestate", json!([(n.height - 1).to_string()])).await?;
    let mut tree = tree_state(&before)?;
    let mut w: Option<IncrementalWitness<MerkleHashOrchard, 32>> = None;
    for h in n.height..=anchor_height {
        for (txid, i, a) in block_actions(node, h).await? {
            let l = leaf(&a.cmx().to_bytes())?;
            match &mut w {
                Some(w) => w.append(l).map_err(|_| Error::Build("tree full".into()))?,
                None => {
                    tree.append(l).map_err(|_| Error::Build("tree full".into()))?;
                    if (txid, i) == (n.txid, n.action) {
                        w = IncrementalWitness::from_tree(tree.clone());
                    }
                }
            }
        }
    }
    let w = w.ok_or(Error::Build("note not found in its block".into()))?;
    let anchor = node.ironwood_anchor(anchor_height).await?;
    let root = orchard::Anchor::from(w.root());
    if root != anchor {
        return Err(Error::Build(format!("witness root differs from the node's at {anchor_height}")));
    }
    let path = w.path().ok_or(Error::Build("witness path".into()))?;
    Ok((MerklePath::from(path), anchor))
}

/// Spends of `notes` (all to `fvk`) against one anchor at `anchor_height`.
pub async fn spends(
    node: &Node,
    fvk: &FullViewingKey,
    notes: &[OwnedNote],
    anchor_height: u32,
) -> Result<(Vec<IronwoodSpend>, orchard::Anchor)> {
    let mut out = vec![];
    let mut anchor = None;
    for n in notes {
        let (path, a) = witness(node, n, anchor_height).await?;
        anchor = Some(a);
        out.push(IronwoodSpend { fvk: fvk.clone(), note: n.note, path });
    }
    Ok((out, anchor.ok_or(Error::Build("no notes".into()))?))
}
