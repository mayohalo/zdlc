//! v6 transactions through the pczt roles.
//!
//! `zcash_primitives`' Builder makes the parts (`build_for_pczt`); zdlc then sets what
//! the Builder cannot: a height lock time with a non-final input sequence (the refund),
//! and `nExpiryHeight = 0` (every pre-signed transaction). The pczt Creator, IoFinalizer,
//! Prover (Ironwood outputs only), Signer, SpendFinalizer and TxExtractor do the rest.
//!
//! Signatures for the 2-of-2 input come either from a key (`Signer::sign_transparent`)
//! or from outside (`Signer::append_transparent_signature`): the decrypted adaptor
//! signature on a CET.
//!
//! Two additions for shielded funding: Ironwood spends (a note and its Merkle path,
//! proved with the outputs and authorized by `sign_ironwood`), and leg inputs, whose
//! redeem script neither pczt's Signer nor its Spend Finalizer recognise. A leg's fee
//! size comes from `leg::Leg::input_size`, and a transaction spending legs is assembled
//! by `extract_with_script_sigs`.

use std::{collections::BTreeMap, convert::Infallible, sync::OnceLock};

use orchard::circuit::{OrchardCircuitVersion, ProvingKey, VerifyingKey};
use pczt::{
    roles::{
        creator::Creator, io_finalizer::IoFinalizer, prover::Prover, signer::Signer,
        spend_finalizer::SpendFinalizer, tx_extractor::TransactionExtractor,
    },
    Pczt,
};
use secp256k1::{ecdsa, PublicKey, SecretKey};
use zcash_primitives::transaction::{
    builder::{BuildConfig, Builder, BundlePadding, PcztResult},
    fees::{transparent::InputSize, zip317},
    txid::{to_txid, TxIdDigester},
};
use zcash_protocol::{
    consensus::BlockHeight, local_consensus::LocalNetwork, memo::MemoBytes, value::Zatoshis,
};
use zcash_script::script::{Evaluable, FromChain};
use zcash_transparent::{
    address::TransparentAddress,
    bundle::{OutPoint, TxOut},
    pczt as tpczt,
};

use crate::{Error, Result};

/// `nSequence` that keeps `nLockTime` enforced.
pub const SEQUENCE_LOCKTIME: u32 = 0xFFFF_FFFE;

pub fn proving_key() -> &'static ProvingKey {
    static PK: OnceLock<ProvingKey> = OnceLock::new();
    PK.get_or_init(|| ProvingKey::build(OrchardCircuitVersion::PostNu6_3))
}

pub fn verifying_key() -> &'static VerifyingKey {
    static VK: OnceLock<VerifyingKey> = OnceLock::new();
    VK.get_or_init(|| VerifyingKey::build(OrchardCircuitVersion::PostNu6_3))
}

/// A transparent coin to spend.
#[derive(Clone, Debug)]
pub struct Coin {
    /// Internal byte order.
    pub txid: [u8; 32],
    pub index: u32,
    pub value: u64,
    pub spend: Spend,
}

#[derive(Clone, Debug)]
pub enum Spend {
    P2pkh(PublicKey),
    P2sh(FromChain),
    /// A leg (`leg`): P2SH with a reclaim branch.
    Leg(crate::leg::Leg),
}

impl Coin {
    pub fn address(&self) -> TransparentAddress {
        match &self.spend {
            Spend::P2pkh(pk) => TransparentAddress::from_pubkey(pk),
            Spend::P2sh(r) => crate::script::p2sh(r),
            Spend::Leg(l) => l.address(),
        }
    }
}

/// An Ironwood note to spend, with its path to `Spec::ironwood_anchor`.
#[derive(Clone, Debug)]
pub struct IronwoodSpend {
    pub fvk: orchard::keys::FullViewingKey,
    pub note: orchard::Note,
    pub path: orchard::tree::MerklePath,
}

#[derive(Clone, Debug)]
pub enum Pay {
    Transparent(TransparentAddress, u64),
    Ironwood(orchard::Address, u64),
    /// An Ironwood output with a memo (at most 512 bytes), encrypted to the recipient.
    IronwoodMemo(orchard::Address, u64, Vec<u8>),
}

impl Pay {
    pub fn value(&self) -> u64 {
        match self {
            Pay::Transparent(_, v) | Pay::Ironwood(_, v) | Pay::IronwoodMemo(_, v, _) => *v,
        }
    }

    fn is_ironwood(&self) -> bool {
        !matches!(self, Pay::Transparent(..))
    }
}

#[derive(Clone, Debug)]
pub struct Spec {
    /// Sets the branch ID the transaction signs under (`BranchId::for_height`).
    pub target_height: u32,
    /// 0 = never expires: required on every pre-signed transaction.
    pub expiry_height: u32,
    /// A height lock: `nLockTime = h` and every input's sequence non-final.
    pub lock_height: Option<u32>,
    pub inputs: Vec<Coin>,
    pub outputs: Vec<Pay>,
    /// Required when an output pays or a spend spends Ironwood.
    pub ironwood_anchor: Option<orchard::Anchor>,
    pub ironwood_spends: Vec<IronwoodSpend>,
}

/// ZIP 317, with leg inputs at their known size (the standard rule refuses P2SH inputs
/// whose redeem script it does not recognise).
struct FeeRule(Vec<(OutPoint, usize)>);

impl zcash_primitives::transaction::fees::FeeRule for FeeRule {
    type Error = zip317::FeeError;

    fn fee_required<P: zcash_protocol::consensus::Parameters>(
        &self,
        params: &P,
        target_height: BlockHeight,
        transparent_input_sizes: impl IntoIterator<Item = InputSize>,
        transparent_output_sizes: impl IntoIterator<Item = usize>,
        sapling_input_count: usize,
        sapling_output_count: usize,
        orchard_action_count: usize,
        ironwood_action_count: usize,
    ) -> std::result::Result<Zatoshis, Self::Error> {
        let sizes: Vec<InputSize> = transparent_input_sizes
            .into_iter()
            .map(|s| match s {
                InputSize::Unknown(o) => match self.0.iter().find(|(p, _)| *p == o) {
                    Some((_, n)) => InputSize::Known(*n),
                    None => InputSize::Unknown(o),
                },
                k => k,
            })
            .collect();
        zip317::FeeRule::standard().fee_required(
            params,
            target_height,
            sizes,
            transparent_output_sizes,
            sapling_input_count,
            sapling_output_count,
            orchard_action_count,
            ironwood_action_count,
        )
    }
}

fn fee_rule(spec: &Spec) -> FeeRule {
    FeeRule(
        spec.inputs
            .iter()
            .filter_map(|c| match &c.spend {
                Spend::Leg(l) => Some((OutPoint::new(c.txid, c.index), l.input_size())),
                _ => None,
            })
            .collect(),
    )
}

fn builder(params: &LocalNetwork, spec: &Spec) -> Result<Builder<LocalNetwork, ()>> {
    let config = BuildConfig::Standard {
        sapling_anchor: None,
        orchard_anchor: None,
        ironwood_anchor: spec.ironwood_anchor,
        orchard_padding: BundlePadding::UNPADDED,
        ironwood_padding: BundlePadding::UNPADDED,
    };
    let mut b = Builder::new(
        *params,
        BlockHeight::from_u32(spec.target_height),
        config,
    )
    .with_expiry_height(BlockHeight::from_u32(spec.expiry_height));
    for c in &spec.inputs {
        let utxo = OutPoint::new(c.txid, c.index);
        let coin = TxOut::new(zat(c.value)?, c.address().script().into());
        match &c.spend {
            Spend::P2pkh(pk) => b.add_transparent_p2pkh_input(*pk, utxo, coin),
            Spend::P2sh(r) => b.add_transparent_p2sh_input(r.clone(), utxo, coin),
            Spend::Leg(l) => b.add_transparent_p2sh_input(l.redeem(), utxo, coin),
        }
        .map_err(|e| Error::Build(format!("input: {e:?}")))?;
    }
    for sp in &spec.ironwood_spends {
        b.add_ironwood_spend::<Infallible>(sp.fvk.clone(), sp.note, sp.path.clone())
            .map_err(|e| Error::Build(format!("ironwood spend: {e:?}")))?;
    }
    for o in &spec.outputs {
        match o {
            Pay::Transparent(a, v) => b
                .add_transparent_output(a, zat(*v)?)
                .map_err(|e| Error::Build(format!("output: {e:?}")))?,
            Pay::Ironwood(a, v) => b
                .add_ironwood_output::<Infallible>(None, *a, zat(*v)?, MemoBytes::empty())
                .map_err(|e| Error::Build(format!("ironwood output: {e:?}")))?,
            Pay::IronwoodMemo(a, v, memo) => {
                let memo = MemoBytes::from_bytes(memo)
                    .map_err(|e| Error::Build(format!("memo: {e:?}")))?;
                b.add_ironwood_output::<Infallible>(None, *a, zat(*v)?, memo)
                    .map_err(|e| Error::Build(format!("ironwood output: {e:?}")))?
            }
        }
    }
    Ok(b)
}

fn zat(v: u64) -> Result<Zatoshis> {
    Zatoshis::from_u64(v).map_err(|_| Error::Build(format!("value {v}")))
}

/// The ZIP 317 fee for `spec`'s inputs and outputs.
pub fn fee(params: &LocalNetwork, spec: &Spec) -> Result<u64> {
    let b = builder(params, spec)?;
    Ok(u64::from(
        b.get_fee(&fee_rule(spec))
            .map_err(|e| Error::Build(format!("fee: {e:?}")))?,
    ))
}

/// Builds the unsigned PCZT (Creator + IoFinalizer, and Prover if it pays Ironwood).
/// Outputs must already balance: inputs = outputs + `fee(spec)`.
pub fn build(params: &LocalNetwork, spec: &Spec) -> Result<Pczt> {
    let need = fee(params, spec)?;
    let ins: u64 = spec.inputs.iter().map(|c| c.value).sum::<u64>()
        + spec.ironwood_spends.iter().map(|s| s.note.value().inner()).sum::<u64>();
    let outs: u64 = spec.outputs.iter().map(Pay::value).sum();
    if ins != outs + need {
        return Err(Error::Build(format!(
            "unbalanced: in {ins} != out {outs} + fee {need}"
        )));
    }
    let PcztResult { mut pczt_parts, .. } = builder(params, spec)?
        .build_for_pczt(rand10::rng(), &fee_rule(spec))
        .map_err(|e| Error::Build(format!("build_for_pczt: {e:?}")))?;
    if let Some(h) = spec.lock_height {
        pczt_parts.lock_time = h;
        let t = pczt_parts
            .transparent
            .take()
            .ok_or(Error::Build("lock height without transparent inputs".into()))?;
        pczt_parts.transparent = Some(relock(t, h)?);
    }
    let pczt = Creator::build_from_parts(pczt_parts)
        .ok_or(Error::Build("Creator::build_from_parts".into()))?;
    let pczt = IoFinalizer::new(pczt)
        .finalize_io(rand10::rng())
        .map_err(|e| Error::Pczt(format!("IoFinalizer: {e:?}")))?;
    if spec.outputs.iter().any(Pay::is_ironwood) || !spec.ironwood_spends.is_empty() {
        return Ok(Prover::new(pczt)
            .create_ironwood_proof(rand10::rng(), proving_key())
            .map_err(|e| Error::Pczt(format!("Prover: {e:?}")))?
            .finish());
    }
    Ok(pczt)
}

/// Rebuilds the transparent bundle with every input's sequence non-final and a required
/// height lock time: the pczt Updater exposes neither field.
fn relock(t: tpczt::Bundle, h: u32) -> Result<tpczt::Bundle> {
    let bad = |what: &str| Error::Build(format!("relock: unexpected {what}"));
    let mut inputs = vec![];
    for i in t.inputs() {
        if !i.partial_signatures().is_empty() || i.script_sig().is_some() {
            return Err(bad("signature"));
        }
        if !i.bip32_derivation().is_empty() || !i.proprietary().is_empty() {
            return Err(bad("metadata"));
        }
        inputs.push(
            tpczt::Input::parse(
                *i.prevout_txid().as_ref(),
                *i.prevout_index(),
                Some(SEQUENCE_LOCKTIME),
                None,
                Some(h),
                None,
                u64::from(*i.value()),
                i.script_pubkey().to_bytes(),
                i.redeem_script().as_ref().map(|r| r.to_bytes()),
                BTreeMap::new(),
                i.sighash_type().encode(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
            )
            .map_err(|e| Error::Build(format!("relock input: {e:?}")))?,
        );
    }
    let mut outputs = vec![];
    for o in t.outputs() {
        outputs.push(
            tpczt::Output::parse(
                u64::from(*o.value()),
                o.script_pubkey().to_bytes(),
                o.redeem_script().as_ref().map(|r| r.to_bytes()),
                BTreeMap::new(),
                o.user_address().clone(),
                BTreeMap::new(),
            )
            .map_err(|e| Error::Build(format!("relock output: {e:?}")))?,
        );
    }
    tpczt::Bundle::parse(inputs, outputs).map_err(|e| Error::Build(format!("relock: {e:?}")))
}

/// The txid of an unsigned PCZT. v5/v6 txids do not commit to signatures, so CETs and
/// the refund can spend the funding output before anyone signs it.
pub fn txid(pczt: &Pczt) -> Result<[u8; 32]> {
    let data = pczt
        .clone()
        .into_effects()
        .map_err(|e| Error::Pczt(format!("effects: {e:?}")))?;
    let digests = data.digest(TxIdDigester);
    Ok(*to_txid(data.version(), data.consensus_branch_id(), &digests).as_ref())
}

/// The v6 sighash of transparent input `index` (SIGHASH_ALL): what the adaptor signs.
pub fn sighash(pczt: &Pczt, index: usize) -> Result<[u8; 32]> {
    Signer::new(pczt.clone())
        .map_err(|e| Error::Pczt(format!("Signer: {e:?}")))?
        .transparent_sighash(index)
        .map_err(|e| Error::Pczt(format!("transparent_sighash: {e:?}")))
}

pub fn sign(pczt: Pczt, index: usize, sk: &SecretKey) -> Result<Pczt> {
    let mut s = Signer::new(pczt).map_err(|e| Error::Pczt(format!("Signer: {e:?}")))?;
    s.sign_transparent(index, sk)
        .map_err(|e| Error::Pczt(format!("sign_transparent: {e:?}")))?;
    Ok(s.finish())
}

/// Appends a signature made elsewhere (a decrypted adaptor signature).
pub fn append(pczt: Pczt, index: usize, sig: ecdsa::Signature) -> Result<Pczt> {
    let mut s = Signer::new(pczt).map_err(|e| Error::Pczt(format!("Signer: {e:?}")))?;
    s.append_transparent_signature(index, sig)
        .map_err(|e| Error::Pczt(format!("append_transparent_signature: {e:?}")))?;
    Ok(s.finish())
}

/// Authorizes every Ironwood spend `ask` can sign; returns how many it signed.
pub fn sign_ironwood(pczt: Pczt, ask: &orchard::keys::SpendAuthorizingKey) -> Result<(Pczt, usize)> {
    let actions = pczt.ironwood().actions().len();
    let mut s = Signer::new(pczt).map_err(|e| Error::Pczt(format!("Signer: {e:?}")))?;
    let mut n = 0;
    for i in 0..actions {
        // Dummy spends, and spends of other keys, refuse this key.
        if s.sign_ironwood(rand10::rng(), i, ask).is_ok() {
            n += 1;
        }
    }
    Ok((s.finish(), n))
}

/// What a counterparty may see of a PCZT that spends this party's Ironwood notes: no
/// witnesses, keys, randomizers, note plaintexts (spends or change) or value
/// commitment trapdoors. `keep_bsk` keeps the binding signing key, which lets whoever
/// holds the PCZT extract the transaction once its transparent inputs are signed.
pub fn redact_ironwood(pczt: Pczt, keep_bsk: bool) -> Pczt {
    pczt::roles::redactor::Redactor::new(pczt)
        .redact_ironwood_with(|mut r| {
            r.redact_actions(|mut a| {
                a.clear_spend_witness();
                a.clear_spend_fvk();
                a.clear_spend_recipient();
                a.clear_spend_value();
                a.clear_spend_rho();
                a.clear_spend_rseed();
                a.clear_spend_alpha();
                a.clear_spend_dummy_sk();
                a.clear_spend_zip32_derivation();
                a.clear_output_recipient();
                a.clear_output_value();
                a.clear_output_rseed();
                a.clear_output_ock();
                a.clear_output_zip32_derivation();
                a.clear_output_user_address();
                a.clear_rcv();
            });
            if !keep_bsk {
                r.clear_bsk();
            }
        })
        .finish()
}

/// Assembles a transparent-only transaction from its PCZT and one scriptSig per input,
/// for inputs pczt cannot finalize (legs). The caller builds every scriptSig.
pub fn extract_with_script_sigs(pczt: Pczt, script_sigs: Vec<Vec<u8>>) -> Result<Final> {
    use zcash_primitives::transaction::{Authorized, TransactionData};
    use zcash_transparent::{address::Script, bundle as tb};
    let fx = pczt.into_effects().map_err(|e| Error::Pczt(format!("effects: {e:?}")))?;
    if fx.sapling_bundle().is_some() || fx.orchard_bundle().is_some() || fx.ironwood_bundle().is_some() {
        return Err(Error::Build("hand assembly is for transparent-only transactions".into()));
    }
    let t = fx.transparent_bundle().ok_or(Error::Build("no transparent bundle".into()))?;
    if t.vin.len() != script_sigs.len() {
        return Err(Error::Build("one scriptSig per input".into()));
    }
    let bundle = tb::Bundle::<tb::Authorized> {
        vin: t
            .vin
            .iter()
            .zip(script_sigs)
            .map(|(i, s)| tb::TxIn::from_parts(i.prevout().clone(), Script(zcash_script::script::Code(s)), i.sequence()))
            .collect(),
        vout: t.vout.clone(),
        authorization: tb::Authorized,
    };
    let tx = TransactionData::<Authorized>::from_parts_v6(
        fx.consensus_branch_id(),
        fx.lock_time(),
        fx.expiry_height(),
        Some(bundle),
        None,
        None,
        None,
    )
    .freeze()
    .map_err(|e| Error::Pczt(format!("freeze: {e}")))?;
    let mut raw = vec![];
    tx.write(&mut raw).map_err(|e| Error::Pczt(e.to_string()))?;
    Ok(Final {
        txid: *tx.txid().as_ref(),
        lock_time: tx.lock_time(),
        expiry_height: u32::from(tx.expiry_height()),
        branch_id: u32::from(tx.consensus_branch_id()),
        raw,
    })
}

/// P2PKH scriptSig: `<sig> <pubkey>`.
pub fn p2pkh_script_sig(sig: &ecdsa::Signature, pk: &PublicKey) -> Vec<u8> {
    let mut d = sig.serialize_der().to_vec();
    d.push(0x01);
    let mut s = vec![d.len() as u8];
    s.extend(d);
    s.push(33);
    s.extend(pk.serialize());
    s
}

/// A signed v6 transaction.
pub struct Final {
    pub raw: Vec<u8>,
    /// Internal byte order.
    pub txid: [u8; 32],
    pub lock_time: u32,
    pub expiry_height: u32,
    pub branch_id: u32,
}

pub fn extract(pczt: Pczt) -> Result<Final> {
    let pczt = SpendFinalizer::new(pczt)
        .finalize_spends()
        .map_err(|e| Error::Pczt(format!("SpendFinalizer: {e:?}")))?;
    let tx = TransactionExtractor::new(pczt)
        .with_orchard(verifying_key())
        .extract(rand10::rng())
        .map_err(|e| Error::Pczt(format!("TxExtractor: {e:?}")))?;
    let mut raw = vec![];
    tx.write(&mut raw).map_err(|e| Error::Pczt(e.to_string()))?;
    Ok(Final {
        txid: *tx.txid().as_ref(),
        lock_time: tx.lock_time(),
        expiry_height: u32::from(tx.expiry_height()),
        branch_id: u32::from(tx.consensus_branch_id()),
        raw,
    })
}
