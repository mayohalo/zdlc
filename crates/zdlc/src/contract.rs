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

use orchard::{
    keys::{FullViewingKey, PreparedIncomingViewingKey, Scope},
    Address as IronwoodAddress,
};
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
/// How many blocks old a contract's sign height may be when a party checks the terms.
pub const SIGN_SLACK: u32 = 10;
/// Expiry delta for the funding and close transactions (not pre-signed).
pub const EXPIRY_DELTA: u32 = 40;

/// The oracle event a contract settles on. Several announcements make an n-of-n
/// multi-oracle event: every CET's adaptor point is the sum of the oracles' points, so a
/// CET opens only when every oracle attests the same outcome. Two oracles that disagree
/// unlock nothing, and the contract falls back to the refund.
#[derive(Clone, Debug)]
pub enum Event {
    Enumerated(Vec<Announcement>),
    Numeric(Vec<NumericAnnouncement>),
}

impl Event {
    pub fn oracles(&self) -> usize {
        match self {
            Event::Enumerated(a) => a.len(),
            Event::Numeric(a) => a.len(),
        }
    }
}

fn sum_points(points: &[Point]) -> Result<Point> {
    Point::combine_keys(&points.iter().collect::<Vec<_>>())
        .map_err(|e| Error::Crypto(format!("sum of oracle points: {e}")))
}

fn sum_scalars(s: &[secp256k1_zkp::SecretKey]) -> Result<secp256k1_zkp::SecretKey> {
    let (first, rest) = s.split_first().ok_or(Error::Crypto("no oracle scalars".into()))?;
    rest.iter()
        .try_fold(*first, |acc, x| acc.add_tweak(&secp256k1_zkp::Scalar::from(*x)))
        .map_err(|e| Error::Crypto(format!("sum of oracle scalars: {e}")))
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
    /// Allow a contract that spans one network upgrade: every CET is signed once per era
    /// (two adaptor signatures per unlock) and the refund for the era it confirms in.
    pub dual_sign: bool,
    /// Roll from a previous contract: its funding output, settled at `credit`, funds this
    /// one in the same transaction (see [`Roll`]).
    pub roll: Option<Roll>,
}

/// One transaction closes the previous contract and opens this one: the new funding spends
/// the old 2-of-2 output, each party's old payout (`credit`) counts toward its new
/// collateral, any surplus is paid to the party's payout address, and a shortfall comes
/// from the party's fresh inputs. Both parties sign the old output with their previous
/// funding keys (`Party::roll_key`).
#[derive(Clone, Debug)]
pub struct Roll {
    pub coin: Coin,
    /// (offer, accept) payouts of the old contract at its attested outcome.
    pub credit: (u64, u64),
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
    pub params: LocalNetwork,
    pub redeem: FromChain,
    pub funding: Pczt,
    pub funding_txid: [u8; 32],
    pub funding_coin: Coin,
    /// Part of the funding output set aside for the CET or refund fee.
    pub reserve: u64,
    /// (terms, PCZT, sighash, target height): one entry per CET per era.
    pub cets: Vec<(CetTerms, Pczt, [u8; 32], u32)>,
    pub refund: Pczt,
    pub refund_sighash: [u8; 32],
}

impl Terms {
    pub fn total(&self) -> u64 {
        self.offer_collateral + self.accept_collateral
    }

    /// Refuses collateral above `cap` on either side (a per-contract cap for live networks).
    pub fn check_cap(&self, cap: u64) -> Result<()> {
        if self.offer_collateral > cap || self.accept_collateral > cap {
            return Err(Error::Build(format!("collateral above the {cap} zat cap")));
        }
        Ok(())
    }

    /// The target heights CETs are signed for: the current era, plus the next one when a
    /// dual-signed contract spans its activation.
    pub fn eras(&self, p: &LocalNetwork) -> Vec<u32> {
        let mut eras = vec![self.sign_height];
        if self.dual_sign && params::spans_upgrade(p, self.sign_height, self.refund_height) {
            if let Some(act) = params::next_activation(p, self.sign_height) {
                eras.push(act);
            }
        }
        eras
    }

    /// The checks both sides run on the terms at `tip`, including the upgrade guard:
    /// maturity and refund must fall before the next activation height, or every
    /// pre-signed transaction would be invalid when the refund comes due.
    pub fn validate(&self, p: &LocalNetwork, tip: u32) -> Result<()> {
        let next = tip + 1;
        // Setup spans a few messages; a sign height up to SIGN_SLACK blocks old is fine
        // as long as it is in the same era as the next block.
        if self.sign_height + SIGN_SLACK < next
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
            let act = params::next_activation(p, next).unwrap_or(0);
            if !self.dual_sign {
                return Err(Error::Build(format!(
                    "contract spans a network upgrade: refund height {} is at or after the activation at {act}",
                    self.refund_height
                )));
            }
            if params::spans_upgrade(p, act, self.refund_height + 1) {
                return Err(Error::Build("contract spans two network upgrades".into()));
            }
        }
        self.cet_terms().map(|_| ())
    }

    pub fn cet_terms(&self) -> Result<Vec<CetTerms>> {
        let total = self.total();
        match (&self.event, &self.payouts) {
            (Event::Enumerated(anns), Payouts::Enumerated(list)) => list
                .iter()
                .map(|(o, off)| {
                    if *off > total {
                        return Err(Error::Build(format!("payout for {o} exceeds the pot")));
                    }
                    let points = anns
                        .iter()
                        .map(|a| oracle::attestation_point(a, o))
                        .collect::<Result<Vec<_>>>()?;
                    Ok(CetTerms {
                        offer: *off,
                        accept: total - off,
                        unlocks: vec![Unlock {
                            prefix: vec![],
                            outcome: Some(o.clone()),
                            point: sum_points(&points)?,
                        }],
                    })
                })
                .collect(),
            (Event::Numeric(anns), Payouts::StableValue(c)) => {
                if anns.is_empty()
                    || c.q_recipient + c.premium != self.offer_collateral
                    || c.q_hedger != self.accept_collateral
                    || anns
                        .iter()
                        .any(|a| c.nb_digits != a.nb_digits || c.unit_cents != a.unit_cents)
                {
                    return Err(Error::Build("curve does not match collateral or event".into()));
                }
                c.ranges()
                    .iter()
                    .map(|r| {
                        let unlocks = payout::prefixes(r, c.nb_digits)
                            .into_iter()
                            .map(|prefix| {
                                let points = anns
                                    .iter()
                                    .map(|a| oracle::prefix_point(a, &prefix))
                                    .collect::<Result<Vec<_>>>()?;
                                Ok(Unlock {
                                    point: sum_points(&points)?,
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

/// The height the refund signs for: the era of the first block it can be mined in.
pub fn refund_target(p: &LocalNetwork, sign_height: u32, refund_height: u32) -> u32 {
    if params::spans_upgrade(p, sign_height, refund_height + 1) {
        params::next_activation(p, sign_height).unwrap_or(sign_height)
    } else {
        sign_height
    }
}

/// The refund spec: each party's collateral back, locked to `refund_height`. Public so a
/// wallet restored from its seed can rebuild the exact transaction (`recovery`).
#[allow(clippy::too_many_arguments)]
pub fn refund_spec(
    p: &LocalNetwork,
    funding: &Coin,
    reserve: u64,
    sign_height: u32,
    refund_height: u32,
    offer: (TransparentAddress, u64),
    accept: (TransparentAddress, u64),
) -> Result<Spec> {
    payout_spec(
        p,
        funding,
        reserve,
        refund_target(p, sign_height, refund_height),
        Some(refund_height),
        [offer, accept],
    )
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
    let roll = terms.roll.as_ref();
    let inputs: Vec<Coin> = roll
        .map(|r| r.coin.clone())
        .into_iter()
        .chain(offer.inputs.iter().cloned())
        .chain(accept.inputs.iter().cloned())
        .collect();
    let (credit_o, credit_a) = roll.map(|r| r.credit).unwrap_or((0, 0));
    // The old output's remainder beyond both credits (its unused reserve) pays fees first.
    let old_extra = match roll {
        Some(r) => r.coin.value.checked_sub(credit_o + credit_a).ok_or(Error::Build("roll credits exceed the old output".into()))?,
        None => 0,
    };
    // A party funds if it brings inputs, a credit or collateral. Either party may fund
    // alone (an escrow client): a party with collateral must bring the value for it.
    // Each funder's leftover goes to its change address, or on a roll to its payout address.
    let funders: Vec<(&PartyInfo, u64, u64)> = [(offer, terms.offer_collateral, credit_o), (accept, terms.accept_collateral, credit_a)]
        .into_iter()
        .filter(|(info, collateral, credit)| !info.inputs.is_empty() || *collateral > 0 || *credit > 0)
        .collect();
    if funders.is_empty() {
        return Err(Error::Build("nobody funds the contract".into()));
    }
    let rest_to = |info: &PartyInfo| if roll.is_some() { info.payout } else { info.change };
    let mut spec = Spec {
        target_height: terms.sign_height,
        expiry_height: terms.sign_height + EXPIRY_DELTA,
        lock_height: None,
        inputs,
        outputs: std::iter::once(Pay::Transparent(p2sh, fund_value))
            .chain(funders.iter().map(|(info, _, _)| Pay::Transparent(rest_to(info), 1)))
            .collect(),
        ironwood_anchor: None,
    };
    let fee = tx::fee(p, &spec)?;
    let owed = (reserve + fee).checked_sub(old_extra).ok_or(Error::Build("old reserve exceeds the new costs".into()))?;
    let n = funders.len() as u64;
    let mut live = vec![Pay::Transparent(p2sh, fund_value)];
    for (k, (info, collateral, credit)) in funders.iter().enumerate() {
        let share = owed / n + if k == 0 { owed % n } else { 0 };
        let have: u64 = info.inputs.iter().map(|c| c.value).sum::<u64>() + credit;
        let rest = have
            .checked_sub(collateral + share)
            .ok_or(Error::Build("funding inputs too small".into()))?;
        if rest >= DUST {
            live.push(Pay::Transparent(rest_to(info), rest));
        } else if rest > 0 && roll.is_none() {
            return Err(Error::Build("change below dust; use other inputs".into()));
        }
    }
    if live.len() != spec.outputs.len() {
        // A funder's leftover was dust (a roll that nets to nothing): the fee absorbs it.
        spec.outputs = live;
        let refee = tx::fee(p, &spec)?;
        if refee > fee {
            return Err(Error::Build("fee changed after dropping dust".into()));
        }
    } else {
        spec.outputs = live;
    }
    let funding = tx::build(p, &spec)?;
    let funding_txid = tx::txid(&funding)?;
    let funding_coin = Coin {
        txid: funding_txid,
        index: 0,
        value: fund_value,
        spend: Spend::P2sh(redeem.clone()),
    };

    let mut cets = vec![];
    let all = terms.cet_terms()?;
    for era in terms.eras(p) {
        for ct in &all {
            let spec = payout_spec(
                p,
                &funding_coin,
                reserve,
                era,
                None,
                [(offer.payout, ct.offer), (accept.payout, ct.accept)],
            )?;
            let pczt = tx::build(p, &spec)?;
            let sighash = tx::sighash(&pczt, 0)?;
            cets.push((ct.clone(), pczt, sighash, era));
        }
    }
    let refund = tx::build(
        p,
        &refund_spec(
            p,
            &funding_coin,
            reserve,
            terms.sign_height,
            terms.refund_height,
            (offer.payout, terms.offer_collateral),
            (accept.payout, terms.accept_collateral),
        )?,
    )?;
    let refund_sighash = tx::sighash(&refund, 0)?;
    Ok(TxSet {
        params: *p,
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
        .map(|(ct, _, sighash, _)| {
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
    for ((ct, _, sighash, _), row) in txs.cets.iter().zip(sigs) {
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
    /// Viewing key of `info.close`: used to trial-decrypt this party's close output.
    pub close_fvk: FullViewingKey,
    /// This party's funding key in the contract being rolled from, if any.
    pub roll_key: Option<SecretKey>,
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
    close_ivk: PreparedIncomingViewingKey,
    their_adaptors: Vec<Vec<AdaptorSignature>>,
    their_refund_sig: ecdsa::Signature,
}

impl Party {
    /// Signs the rolled-over input (index 0) with the previous funding key; returns the
    /// index of the first ordinary input.
    fn sign_roll(&self, terms: &Terms, funding: &mut Pczt) -> Result<usize> {
        match &terms.roll {
            None => Ok(0),
            Some(_) => {
                let k = self.roll_key.ok_or(Error::Build("rolling needs the previous funding key".into()))?;
                *funding = tx::sign(funding.clone(), 0, &k)?;
                Ok(1)
            }
        }
    }

    fn close_ivk(&self) -> PreparedIncomingViewingKey {
        PreparedIncomingViewingKey::new(&self.close_fvk.to_ivk(Scope::External))
    }

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
        let base = self.sign_roll(&offer.terms, &mut funding)?;
        for (i, k) in self.input_keys.iter().enumerate() {
            funding = tx::sign(funding, base + i, k)?;
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
            close_ivk: self.close_ivk(),
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
        let base = self.sign_roll(&offer.terms, &mut funding)?;
        let first = base + offer.offer.inputs.len();
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
            close_ivk: self.close_ivk(),
            their_adaptors: sign.cet_adaptors.clone(),
            their_refund_sig: sign.refund_sig,
        };
        Ok((f, contract))
    }
}

impl Contract {
    /// The counterparty's signature on the refund: what the recovery record stores.
    pub fn their_refund_sig(&self) -> ecdsa::Signature {
        self.their_refund_sig
    }

    pub fn adaptor_count(&self) -> usize {
        self.their_adaptors.iter().map(Vec::len).sum()
    }

    /// The refund, valid once the tip reaches the refund height. Either side can send it.
    pub fn refund(&self) -> Result<tx::Final> {
        let pczt = tx::append(self.txs.refund.clone(), 0, self.their_refund_sig)?;
        tx::extract(tx::sign(pczt, 0, &self.funding_sk)?)
    }

    /// Whether CET entry `i` is valid in the block at `height` (its era's branch ID).
    fn in_era(&self, era: u32, height: u32) -> bool {
        params::branch_at(&self.txs.params, era) == params::branch_at(&self.txs.params, height)
    }

    /// The CET for an attested numeric value, for a block at `height`: decrypts the
    /// counterparty's adaptor signature for the matching prefix with the sum of every
    /// oracle's scalars. All oracles must have attested the same value. Returns the
    /// transaction and the CET index.
    pub fn cet_numeric(&self, atts: &[NumericAttestation], height: u32) -> Result<(tx::Final, usize)> {
        if atts.len() != self.terms.event.oracles() {
            return Err(Error::Build(format!(
                "{} attestations for a {}-oracle event",
                atts.len(),
                self.terms.event.oracles()
            )));
        }
        let att = &atts[0];
        if atts.iter().any(|a| a.digits != att.digits) {
            return Err(Error::Build(
                "oracles disagree: no CET opens; wait for the refund".into(),
            ));
        }
        for (i, (ct, pczt, _, era)) in self.txs.cets.iter().enumerate() {
            if !self.in_era(*era, height) {
                continue;
            }
            for (j, u) in ct.unlocks.iter().enumerate() {
                if !u.prefix.is_empty() && att.digits.starts_with(&u.prefix) {
                    let s = sum_scalars(
                        &atts
                            .iter()
                            .map(|a| oracle::prefix_scalar(a, u.prefix.len()))
                            .collect::<Result<Vec<_>>>()?,
                    )?;
                    let d = adaptor::decrypt(&self.their_adaptors[i][j], &s)?;
                    let pczt = tx::append(pczt.clone(), 0, d.signature)?;
                    return Ok((tx::extract(tx::sign(pczt, 0, &self.funding_sk)?)?, i));
                }
            }
        }
        Err(Error::Build("no CET covers the attested value".into()))
    }

    /// The CET for an outcome every oracle attested, for a block at `height`: `scalars`
    /// holds each oracle's attestation scalar for that outcome.
    pub fn cet_outcome(
        &self,
        outcome: &str,
        scalars: &[secp256k1_zkp::SecretKey],
        height: u32,
    ) -> Result<tx::Final> {
        if scalars.len() != self.terms.event.oracles() {
            return Err(Error::Build("one attestation per oracle".into()));
        }
        let s = sum_scalars(scalars)?;
        for (i, (ct, pczt, _, era)) in self.txs.cets.iter().enumerate() {
            if self.in_era(*era, height) && ct.unlocks[0].outcome.as_deref() == Some(outcome) {
                let d = adaptor::decrypt(&self.their_adaptors[i][0], &s)?;
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

    /// The other side's check of a close PCZT before signing. The close is built by one
    /// party, so the other checks what the transaction will actually do, not what the
    /// PCZT's metadata claims:
    /// - it spends exactly the funding outpoint, pays nothing transparent, has no lock
    ///   time and the agreed expiry;
    /// - every Ironwood output's note (recipient, value, rho, rseed) re-derives its `cmx`,
    ///   so the claimed recipient and value are what the chain commits to;
    /// - the outputs are exactly the agreed (recipient, value) pairs;
    /// - this party's own output trial-decrypts with its viewing key, so its wallet will
    ///   find the note.
    pub fn check_close(&self, pczt: &Pczt, spec: &Spec) -> Result<()> {
        let bad = |m: String| Error::Build(format!("close PCZT refused: {m}"));
        let mut want: Vec<([u8; 43], u64)> = spec
            .outputs
            .iter()
            .map(|o| match o {
                Pay::Ironwood(a, v) => Ok((a.to_raw_address_bytes(), *v)),
                _ => Err(bad("the agreed close pays something other than plain Ironwood".into())),
            })
            .collect::<Result<_>>()?;
        let effects = pczt
            .clone()
            .into_effects()
            .map_err(|e| bad(format!("effects: {e:?}")))?;
        if effects.lock_time() != 0 {
            return Err(bad(format!("lock time {}", effects.lock_time())));
        }
        if u32::from(effects.expiry_height()) != spec.expiry_height {
            return Err(bad(format!("expiry {}", u32::from(effects.expiry_height()))));
        }
        let me = if self.is_offerer { &self.offer } else { &self.accept };
        let mine = me.close.to_raw_address_bytes();
        let my_value = want.iter().find(|(a, _)| *a == mine).map(|(_, v)| *v);
        let funding = (self.txs.funding_txid, self.txs.funding_coin.index);
        let ivk = &self.close_ivk;
        let mut transparent: std::result::Result<(), String> = Err("not checked".into());
        let mut got = vec![];
        let mut my_note = None;
        let mut ironwood: std::result::Result<(), String> = Ok(());
        Verifier::new(pczt.clone())
            .with_transparent::<Infallible, _>(|t| {
                transparent = if t.inputs().len() != 1 {
                    Err(format!("{} transparent inputs", t.inputs().len()))
                } else if (*t.inputs()[0].prevout_txid().as_ref(), *t.inputs()[0].prevout_index())
                    != funding
                {
                    Err("input is not the funding output".into())
                } else if !t.outputs().is_empty() {
                    Err("transparent outputs".into())
                } else {
                    Ok(())
                };
                Ok(())
            })
            .map_err(|e| bad(format!("transparent: {e:?}")))?
            .with_ironwood::<Infallible, _>(|b| {
                let version = *b.bundle_version();
                for a in b.actions() {
                    let out = a.output();
                    if let Err(e) = out.verify_note_commitment(a.spend()) {
                        ironwood = Err(format!("output does not match its commitment: {e:?}"));
                        return Ok(());
                    }
                    let (r, v) = match (out.recipient(), out.value()) {
                        (Some(r), Some(v)) => (r.to_raw_address_bytes(), v.inner()),
                        _ => unreachable!("verify_note_commitment requires both"),
                    };
                    if v > 0 {
                        got.push((r, v));
                    }
                    if r == mine && v > 0 {
                        match a.decrypt_output_with_ivk(ivk, version) {
                            Ok(Some((note, _, _))) => my_note = Some(note.value().inner()),
                            _ => {
                                ironwood = Err("own output does not decrypt".into());
                                return Ok(());
                            }
                        }
                    }
                }
                Ok(())
            })
            .map_err(|e| bad(format!("ironwood: {e:?}")))?;
        transparent.map_err(bad)?;
        ironwood.map_err(bad)?;
        want.sort();
        got.sort();
        if want != got {
            return Err(bad("outputs differ from the agreed payouts".into()));
        }
        if my_value.is_some() && my_note != my_value {
            return Err(bad("own output did not decrypt to the agreed value".into()));
        }
        Ok(())
    }

    pub fn sign_close(&self, pczt: Pczt) -> Result<Pczt> {
        tx::sign(pczt, 0, &self.funding_sk)
    }
}
