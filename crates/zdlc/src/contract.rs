//! A two-party contract: offer, accept, sign; the transaction set both parties derive from
//! those messages; settlement by cooperative close, CET or refund.
//!
//! Both sides build every transaction themselves from the messages and compare: the
//! funding txid, each CET sighash and the refund sighash must match before anyone signs
//! funding inputs. The funding transaction is signed last, by the offerer and then the
//! accepter, so neither can lose collateral before holding every CET and the refund.
//!
//! Messages are Rust values here; the funding PCZT crosses as bytes (`Pczt::serialize`).
//! A wire encoding comes with the RFQ desk (Phase 4).

use std::convert::Infallible;

use orchard::Address as IronwoodAddress;
use pczt::{roles::verifier::Verifier, Pczt};
use secp256k1::{ecdsa, Message, PublicKey, SecretKey};
use secp256k1_zkp::PublicKey as Point;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_script::script::FromChain;
use zcash_transparent::address::TransparentAddress;

use crate::{
    adaptor::{self, AdaptorSignature},
    oracle::{self, Announcement, NumericAnnouncement, NumericAttestation},
    params,
    payout::{self, StableValue},
    script,
    tx::{self, Coin, Pay, Spec, Spend},
    Error, Result,
};

/// Outputs below this are dropped (to the other side), as relay rejects them as dust.
pub const DUST: u64 = 54;
/// Expiry delta for the funding and close transactions (not pre-signed).
pub const EXPIRY_DELTA: u32 = 40;

#[derive(Clone, Debug)]
pub enum Event {
    Enumerated(Announcement),
    Numeric(NumericAnnouncement),
}

#[derive(Clone, Debug)]
pub enum Payouts {
    /// The offerer's payout per outcome, in zatoshis.
    Enumerated(Vec<(String, u64)>),
    /// A stable-value curve; the offerer is the recipient.
    StableValue(StableValue),
}

#[derive(Clone, Debug)]
pub struct Terms {
    pub event: Event,
    pub payouts: Payouts,
    pub offer_collateral: u64,
    pub accept_collateral: u64,
    /// Height by which the oracle attests.
    pub maturity: u32,
    pub refund_height: u32,
    /// Target height of every transaction built at setup: fixes their branch ID.
    pub sign_height: u32,
}

/// What unlocks a CET: a digit prefix (numeric) or an outcome (enumerated), and the
/// adaptor point its signatures are encrypted to.
#[derive(Clone, Debug)]
pub struct Unlock {
    pub prefix: Vec<usize>,
    pub outcome: Option<String>,
    pub point: Point,
}

#[derive(Clone, Debug)]
pub struct CetTerms {
    pub offer: u64,
    pub accept: u64,
    pub unlocks: Vec<Unlock>,
}

#[derive(Clone, Debug)]
pub struct PartyInfo {
    pub funding_pubkey: PublicKey,
    /// P2PKH coins this party contributes to funding.
    pub inputs: Vec<Coin>,
    pub change: TransparentAddress,
    /// Where CETs and the refund pay this party.
    pub payout: TransparentAddress,
    /// Where the cooperative close pays this party.
    pub close: IronwoodAddress,
}

#[derive(Clone)]
pub struct Offer {
    pub terms: Terms,
    pub offer: PartyInfo,
}

#[derive(Clone)]
pub struct Accept {
    pub accept: PartyInfo,
    /// Per CET, per unlock.
    pub cet_adaptors: Vec<Vec<AdaptorSignature>>,
    pub refund_sig: ecdsa::Signature,
}

#[derive(Clone)]
pub struct SignMsg {
    pub cet_adaptors: Vec<Vec<AdaptorSignature>>,
    pub refund_sig: ecdsa::Signature,
    /// The funding PCZT with the offerer's inputs signed.
    pub funding_pczt: Vec<u8>,
}

pub struct TxSet {
    pub redeem: FromChain,
    pub funding: Pczt,
    pub funding_txid: [u8; 32],
    pub funding_coin: Coin,
    /// Part of the funding output set aside for the CET or refund fee.
    pub reserve: u64,
    pub cets: Vec<(CetTerms, Pczt, [u8; 32])>,
    pub refund: Pczt,
    pub refund_sighash: [u8; 32],
}

impl Terms {
    pub fn total(&self) -> u64 {
        self.offer_collateral + self.accept_collateral
    }

    /// The checks both sides run on the terms at `tip`, including the upgrade guard:
    /// maturity and refund must fall before the next activation height, or every
    /// pre-signed transaction would be invalid when the refund comes due.
    pub fn validate(&self, p: &LocalNetwork, tip: u32) -> Result<()> {
        let next = tip + 1;
        if self.sign_height < next
            || params::branch_at(p, self.sign_height) != params::branch_at(p, next)
        {
            return Err(Error::Build(format!(
                "sign height {} is not in the current era (next block {next})",
                self.sign_height
            )));
        }
        if self.maturity < next || self.refund_height <= self.maturity {
            return Err(Error::Build("need tip < maturity < refund height".into()));
        }
        if params::spans_upgrade(p, next, self.refund_height) {
            return Err(Error::Build(format!(
                "contract spans a network upgrade: refund height {} is at or after the activation at {}",
                self.refund_height,
                params::next_activation(p, next).unwrap_or(0)
            )));
        }
        self.cet_terms().map(|_| ())
    }

    pub fn cet_terms(&self) -> Result<Vec<CetTerms>> {
        let total = self.total();
        match (&self.event, &self.payouts) {
            (Event::Enumerated(a), Payouts::Enumerated(list)) => list
                .iter()
                .map(|(o, off)| {
                    if *off > total {
                        return Err(Error::Build(format!("payout for {o} exceeds the pot")));
                    }
                    Ok(CetTerms {
                        offer: *off,
                        accept: total - off,
                        unlocks: vec![Unlock {
                            prefix: vec![],
                            outcome: Some(o.clone()),
                            point: oracle::attestation_point(a, o)?,
                        }],
                    })
                })
                .collect(),
            (Event::Numeric(a), Payouts::StableValue(c)) => {
                if c.q_recipient != self.offer_collateral
                    || c.q_hedger != self.accept_collateral
                    || c.nb_digits != a.nb_digits
                    || c.unit_cents != a.unit_cents
                {
                    return Err(Error::Build("curve does not match collateral or event".into()));
                }
                c.ranges()
                    .iter()
                    .map(|r| {
                        let unlocks = payout::prefixes(r, c.nb_digits)
                            .into_iter()
                            .map(|prefix| {
                                Ok(Unlock {
                                    point: oracle::prefix_point(a, &prefix)?,
                                    prefix,
                                    outcome: None,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        Ok(CetTerms {
                            offer: r.recipient,
                            accept: total - r.recipient,
                            unlocks,
                        })
                    })
                    .collect()
            }
            _ => Err(Error::Build("event and payouts disagree".into())),
        }
    }

    /// (offer, accept) payout for an attested numeric value.
    pub fn payout_at(&self, value: usize) -> Result<(u64, u64)> {
        match &self.payouts {
            Payouts::StableValue(c) => {
                let o = c.at(value);
                Ok((o, self.total() - o))
            }
            _ => Err(Error::Build("not a numeric contract".into())),
        }
    }
}

/// A spec spending the funding coin to the two payouts. Dust goes to the other side; the
/// fee comes out of the reserve and any reserve left over goes to the larger output.
fn payout_spec(
    p: &LocalNetwork,
    funding: &Coin,
    reserve: u64,
    sign_height: u32,
    lock_height: Option<u32>,
    to: [(TransparentAddress, u64); 2],
) -> Result<Spec> {
    let mut amounts = to;
    for i in 0..2 {
        if amounts[i].1 < DUST {
            amounts[1 - i].1 += amounts[i].1;
            amounts[i].1 = 0;
        }
    }
    let live: Vec<(TransparentAddress, u64)> =
        amounts.iter().filter(|(_, v)| *v > 0).copied().collect();
    let mut spec = Spec {
        target_height: sign_height,
        expiry_height: 0,
        lock_height,
        inputs: vec![funding.clone()],
        outputs: live.iter().map(|(a, v)| Pay::Transparent(*a, *v)).collect(),
        ironwood_anchor: None,
    };
    let fee = tx::fee(p, &spec)?;
    let left = reserve
        .checked_sub(fee)
        .ok_or(Error::Build(format!("fee {fee} exceeds reserve {reserve}")))?;
    let big = (0..live.len()).max_by_key(|&i| live[i].1).unwrap_or(0);
    spec.outputs[big] = Pay::Transparent(live[big].0, live[big].1 + left);
    Ok(spec)
}

/// Derives the whole transaction set from the terms and both parties' info.
pub fn build_txs(
    p: &LocalNetwork,
    terms: &Terms,
    offer: &PartyInfo,
    accept: &PartyInfo,
) -> Result<TxSet> {
    let redeem = script::multisig_2of2(&offer.funding_pubkey, &accept.funding_pubkey)?;
    let total = terms.total();
    let probe = Coin {
        txid: [0; 32],
        index: 0,
        value: total + 1_000_000,
        spend: Spend::P2sh(redeem.clone()),
    };
    let mut reserve = tx::fee(
        p,
        &Spec {
            target_height: terms.sign_height,
            expiry_height: 0,
            lock_height: None,
            inputs: vec![probe],
            outputs: vec![
                Pay::Transparent(offer.payout, 1),
                Pay::Transparent(accept.payout, 1),
            ],
            ironwood_anchor: None,
        },
    )?;
    reserve += reserve % 2;

    let fund_value = total + reserve;
    let p2sh = script::p2sh(&redeem);
    let inputs: Vec<Coin> = offer.inputs.iter().chain(&accept.inputs).cloned().collect();
    let mut spec = Spec {
        target_height: terms.sign_height,
        expiry_height: terms.sign_height + EXPIRY_DELTA,
        lock_height: None,
        inputs,
        outputs: vec![
            Pay::Transparent(p2sh, fund_value),
            Pay::Transparent(offer.change, 1),
            Pay::Transparent(accept.change, 1),
        ],
        ironwood_anchor: None,
    };
    let fee = tx::fee(p, &spec)?;
    let change = |info: &PartyInfo, collateral: u64, fee_share: u64| -> Result<u64> {
        let have: u64 = info.inputs.iter().map(|c| c.value).sum();
        let c = have
            .checked_sub(collateral + reserve / 2 + fee_share)
            .ok_or(Error::Build("funding inputs too small".into()))?;
        if c < DUST {
            return Err(Error::Build("change below dust; use other inputs".into()));
        }
        Ok(c)
    };
    spec.outputs[1] = Pay::Transparent(
        offer.change,
        change(offer, terms.offer_collateral, fee - fee / 2)?,
    );
    spec.outputs[2] = Pay::Transparent(accept.change, change(accept, terms.accept_collateral, fee / 2)?);
    let funding = tx::build(p, &spec)?;
    let funding_txid = tx::txid(&funding)?;
    let funding_coin = Coin {
        txid: funding_txid,
        index: 0,
        value: fund_value,
        spend: Spend::P2sh(redeem.clone()),
    };

    let mut cets = vec![];
    for ct in terms.cet_terms()? {
        let spec = payout_spec(
            p,
            &funding_coin,
            reserve,
            terms.sign_height,
            None,
            [(offer.payout, ct.offer), (accept.payout, ct.accept)],
        )?;
        let pczt = tx::build(p, &spec)?;
        let sighash = tx::sighash(&pczt, 0)?;
        cets.push((ct, pczt, sighash));
    }
    let refund_spec = payout_spec(
        p,
        &funding_coin,
        reserve,
        terms.sign_height,
        Some(terms.refund_height),
        [
            (offer.payout, terms.offer_collateral),
            (accept.payout, terms.accept_collateral),
        ],
    )?;
    let refund = tx::build(p, &refund_spec)?;
    let refund_sighash = tx::sighash(&refund, 0)?;
    Ok(TxSet {
        redeem,
        funding,
        funding_txid,
        funding_coin,
        reserve,
        cets,
        refund,
        refund_sighash,
    })
}

fn adaptors(txs: &TxSet, sk: &SecretKey) -> Vec<Vec<AdaptorSignature>> {
    txs.cets
        .iter()
        .map(|(ct, _, sighash)| {
            ct.unlocks
                .iter()
                .map(|u| adaptor::encrypt(sk, *sighash, &u.point))
                .collect()
        })
        .collect()
}

fn check_adaptors(txs: &TxSet, sigs: &[Vec<AdaptorSignature>], pk: &PublicKey) -> Result<usize> {
    if sigs.len() != txs.cets.len() {
        return Err(Error::Crypto("adaptor signature count".into()));
    }
    let mut n = 0;
    for ((ct, _, sighash), row) in txs.cets.iter().zip(sigs) {
        if row.len() != ct.unlocks.len() {
            return Err(Error::Crypto("adaptor signature count".into()));
        }
        for (u, s) in ct.unlocks.iter().zip(row) {
            adaptor::verify(s, *sighash, pk, &u.point)?;
            n += 1;
        }
    }
    Ok(n)
}

fn check_sig(sig: &ecdsa::Signature, sighash: [u8; 32], pk: &PublicKey) -> Result<()> {
    ecdsa::verify(sig, Message::from_digest(sighash), pk)
        .map_err(|e| Error::Crypto(format!("refund signature: {e}")))
}

/// A party's secrets and public info.
pub struct Party {
    pub funding_sk: SecretKey,
    /// Keys for `info.inputs`, in order.
    pub input_keys: Vec<SecretKey>,
    pub info: PartyInfo,
}

/// One side's view after setup: enough to settle alone.
pub struct Contract {
    pub is_offerer: bool,
    pub terms: Terms,
    pub offer: PartyInfo,
    pub accept: PartyInfo,
    pub txs: TxSet,
    funding_sk: SecretKey,
    their_adaptors: Vec<Vec<AdaptorSignature>>,
    their_refund_sig: ecdsa::Signature,
}

impl Party {
    pub fn offer(&self, terms: Terms) -> Offer {
        Offer {
            terms,
            offer: self.info.clone(),
        }
    }

    /// Accepter: checks the terms, derives the transactions, adaptor-signs every CET and
    /// signs the refund.
    pub fn accept(&self, p: &LocalNetwork, tip: u32, offer: &Offer) -> Result<(Accept, TxSet)> {
        offer.terms.validate(p, tip)?;
        let txs = build_txs(p, &offer.terms, &offer.offer, &self.info)?;
        let accept = Accept {
            accept: self.info.clone(),
            cet_adaptors: adaptors(&txs, &self.funding_sk),
            refund_sig: ecdsa::sign(Message::from_digest(txs.refund_sighash), &self.funding_sk),
        };
        Ok((accept, txs))
    }

    /// Offerer: derives the same transactions, verifies every accepter signature, returns
    /// its own and its signed funding inputs.
    pub fn sign(
        &self,
        p: &LocalNetwork,
        tip: u32,
        offer: &Offer,
        accept: &Accept,
    ) -> Result<(SignMsg, Contract)> {
        offer.terms.validate(p, tip)?;
        let txs = build_txs(p, &offer.terms, &self.info, &accept.accept)?;
        check_adaptors(&txs, &accept.cet_adaptors, &accept.accept.funding_pubkey)?;
        check_sig(&accept.refund_sig, txs.refund_sighash, &accept.accept.funding_pubkey)?;
        let mut funding = txs.funding.clone();
        for (i, k) in self.input_keys.iter().enumerate() {
            funding = tx::sign(funding, i, k)?;
        }
        let msg = SignMsg {
            cet_adaptors: adaptors(&txs, &self.funding_sk),
            refund_sig: ecdsa::sign(Message::from_digest(txs.refund_sighash), &self.funding_sk),
            funding_pczt: funding
                .serialize()
                .map_err(|e| Error::Pczt(format!("serialize: {e:?}")))?,
        };
        let contract = Contract {
            is_offerer: true,
            terms: offer.terms.clone(),
            offer: self.info.clone(),
            accept: accept.accept.clone(),
            txs,
            funding_sk: self.funding_sk,
            their_adaptors: accept.cet_adaptors.clone(),
            their_refund_sig: accept.refund_sig,
        };
        Ok((msg, contract))
    }

    /// Accepter: verifies the offerer's signatures and funding PCZT, signs its own inputs
    /// and returns the funding transaction to broadcast.
    pub fn finish(
        &self,
        offer: &Offer,
        txs: TxSet,
        sign: &SignMsg,
    ) -> Result<(tx::Final, Contract)> {
        check_adaptors(&txs, &sign.cet_adaptors, &offer.offer.funding_pubkey)?;
        check_sig(&sign.refund_sig, txs.refund_sighash, &offer.offer.funding_pubkey)?;
        let mut funding = Pczt::parse(&sign.funding_pczt)
            .map_err(|e| Error::Pczt(format!("parse funding: {e:?}")))?;
        if tx::txid(&funding)? != txs.funding_txid {
            return Err(Error::Build("funding PCZT is not the agreed transaction".into()));
        }
        let first = offer.offer.inputs.len();
        for (i, k) in self.input_keys.iter().enumerate() {
            funding = tx::sign(funding, first + i, k)?;
        }
        let f = tx::extract(funding)?;
        let contract = Contract {
            is_offerer: false,
            terms: offer.terms.clone(),
            offer: offer.offer.clone(),
            accept: self.info.clone(),
            txs,
            funding_sk: self.funding_sk,
            their_adaptors: sign.cet_adaptors.clone(),
            their_refund_sig: sign.refund_sig,
        };
        Ok((f, contract))
    }
}

impl Contract {
    pub fn adaptor_count(&self) -> usize {
        self.their_adaptors.iter().map(Vec::len).sum()
    }

    /// The refund, valid once the tip reaches the refund height. Either side can send it.
    pub fn refund(&self) -> Result<tx::Final> {
        let pczt = tx::append(self.txs.refund.clone(), 0, self.their_refund_sig)?;
        tx::extract(tx::sign(pczt, 0, &self.funding_sk)?)
    }

    /// The CET for an attested numeric value: decrypts the counterparty's adaptor
    /// signature for the matching prefix with the oracle's scalars. Returns the
    /// transaction and the CET index.
    pub fn cet_numeric(&self, att: &NumericAttestation) -> Result<(tx::Final, usize)> {
        for (i, (ct, pczt, _)) in self.txs.cets.iter().enumerate() {
            for (j, u) in ct.unlocks.iter().enumerate() {
                if !u.prefix.is_empty() && att.digits.starts_with(&u.prefix) {
                    let s = oracle::prefix_scalar(att, u.prefix.len())?;
                    let d = adaptor::decrypt(&self.their_adaptors[i][j], &s)?;
                    let pczt = tx::append(pczt.clone(), 0, d.signature)?;
                    return Ok((tx::extract(tx::sign(pczt, 0, &self.funding_sk)?)?, i));
                }
            }
        }
        Err(Error::Build("no CET covers the attested value".into()))
    }

    /// The CET for an attested outcome of an enumerated event.
    pub fn cet_outcome(&self, outcome: &str, s: &secp256k1_zkp::SecretKey) -> Result<tx::Final> {
        for (i, (ct, pczt, _)) in self.txs.cets.iter().enumerate() {
            if ct.unlocks[0].outcome.as_deref() == Some(outcome) {
                let d = adaptor::decrypt(&self.their_adaptors[i][0], s)?;
                let pczt = tx::append(pczt.clone(), 0, d.signature)?;
                return tx::extract(tx::sign(pczt, 0, &self.funding_sk)?);
            }
        }
        Err(Error::Build(format!("no CET for {outcome}")))
    }

    /// The cooperative close: the funding output straight to both parties' Ironwood
    /// addresses, the fee split evenly (all of it from the only payee if one side gets
    /// nothing). Both sides derive the same amounts.
    pub fn close_spec(
        &self,
        p: &LocalNetwork,
        tip: u32,
        payout: (u64, u64),
        anchor: orchard::Anchor,
    ) -> Result<Spec> {
        let value = self.txs.funding_coin.value;
        let to = [(self.offer.close, payout.0), (self.accept.close, payout.1)];
        let live: Vec<(IronwoodAddress, u64)> =
            to.iter().filter(|(_, v)| *v >= DUST).copied().collect();
        let mut spec = Spec {
            target_height: tip + 1,
            expiry_height: tip + 1 + EXPIRY_DELTA,
            lock_height: None,
            inputs: vec![self.txs.funding_coin.clone()],
            outputs: live.iter().map(|(a, v)| Pay::Ironwood(*a, *v)).collect(),
            ironwood_anchor: Some(anchor),
        };
        let fee = tx::fee(p, &spec)?;
        // The reserve and any dust go into the fee first; the rest is split.
        let paid: u64 = live.iter().map(|(_, v)| v).sum();
        let extra = value - paid;
        let owed = fee.saturating_sub(extra);
        let n = live.len() as u64;
        let mut outs = vec![];
        for (k, (a, v)) in live.iter().enumerate() {
            let share = owed / n + if (k as u64) < owed % n { 1 } else { 0 };
            outs.push(Pay::Ironwood(
                *a,
                v.checked_sub(share)
                    .ok_or(Error::Build("payout below its fee share".into()))?,
            ));
        }
        let sum: u64 = outs.iter().map(Pay::value).sum();
        if let Some(Pay::Ironwood(a, v)) = outs.last().cloned() {
            *outs.last_mut().unwrap() = Pay::Ironwood(a, v + (value - fee - sum));
        }
        spec.outputs = outs;
        Ok(spec)
    }

    /// The other side's check of a close PCZT before signing: it spends only the funding
    /// output and pays exactly the agreed Ironwood outputs.
    pub fn check_close(&self, pczt: &Pczt, spec: &Spec) -> Result<()> {
        let mut want: Vec<([u8; 43], u64)> = spec
            .outputs
            .iter()
            .map(|o| match o {
                Pay::Ironwood(a, v) => Ok((a.to_raw_address_bytes(), *v)),
                Pay::Transparent(..) => Err(Error::Build("close pays transparent".into())),
            })
            .collect::<Result<_>>()?;
        let mut got = vec![];
        let mut t_ok = false;
        let funding = self.txs.funding_txid;
        Verifier::new(pczt.clone())
            .with_transparent::<Infallible, _>(|t| {
                t_ok = t.inputs().len() == 1
                    && *t.inputs()[0].prevout_txid().as_ref() == funding
                    && t.outputs().is_empty();
                Ok(())
            })
            .map_err(|e| Error::Pczt(format!("verify transparent: {e:?}")))?
            .with_ironwood::<Infallible, _>(|b| {
                for a in b.actions() {
                    if let (Some(r), Some(v)) = (a.output().recipient(), a.output().value()) {
                        if v.inner() > 0 {
                            got.push((r.to_raw_address_bytes(), v.inner()));
                        }
                    }
                }
                Ok(())
            })
            .map_err(|e| Error::Pczt(format!("verify ironwood: {e:?}")))?;
        want.sort();
        got.sort();
        if !t_ok || want != got {
            return Err(Error::Build("close PCZT does not pay the agreed outputs".into()));
        }
        Ok(())
    }

    pub fn sign_close(&self, pczt: Pczt) -> Result<Pczt> {
        tx::sign(pczt, 0, &self.funding_sk)
    }
}
