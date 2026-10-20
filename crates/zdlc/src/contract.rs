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

/// Outputs below this are merged into another output, as relay rejects them as dust.
pub const DUST: u64 = 54;
/// Most beneficiaries a contract may name: a 16-output CET pays 80,000 zat in fees.
pub const MAX_BENEFICIARIES: usize = 16;
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
    /// Who each side's payout goes to. The default pays each side to its own party.
    pub splits: Splits,
}

/// Who an output pays: one of the two signers, or a beneficiary named in the terms.
/// Outputs are ordered by payee (offer, accept, then beneficiaries by index), so both
/// signers build identical transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Payee {
    Offer,
    Accept,
    Beneficiary(usize),
}

/// A payee who never signs: it only gave addresses.
#[derive(Clone, Debug)]
pub struct Beneficiary {
    pub label: String,
    /// CETs pay here.
    pub payout: TransparentAddress,
    /// The cooperative close pays here; `None` pays `payout` transparently.
    pub close: Option<IronwoodAddress>,
}

/// How one side's payout divides among payees, in basis points summing to 10,000. Each
/// share rounds down; the remainder and any share below dust go to the first payee.
/// Empty: everything to the side's own party.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Split(pub Vec<(Payee, u32)>);

/// The payees of every outcome. Splits apply to CETs and the cooperative close; the
/// refund ignores them and returns each funder's collateral, since no outcome happened.
#[derive(Clone, Debug, Default)]
pub struct Splits {
    pub beneficiaries: Vec<Beneficiary>,
    pub offer: Split,
    pub accept: Split,
    /// Enumerated contracts only: an outcome's own output list, replacing both splits
    /// for that outcome (fixed fees, say). Amounts sum to the pot.
    pub outcomes: Vec<(String, Vec<(Payee, u64)>)>,
}

impl Split {
    fn expand(&self, own: Payee, amount: u64) -> Vec<(Payee, u64)> {
        if self.0.is_empty() {
            return vec![(own, amount)];
        }
        let mut out: Vec<(Payee, u64)> = self
            .0
            .iter()
            .map(|(p, bps)| (*p, (amount as u128 * *bps as u128 / 10_000) as u64))
            .collect();
        let paid: u64 = out.iter().map(|(_, v)| v).sum();
        out[0].1 += amount - paid;
        for i in 1..out.len() {
            if out[i].1 < DUST {
                out[0].1 += out[i].1;
                out[i].1 = 0;
            }
        }
        out
    }
}

impl Splits {
    pub fn is_default(&self) -> bool {
        self.beneficiaries.is_empty()
            && self.offer.0.is_empty()
            && self.accept.0.is_empty()
            && self.outcomes.is_empty()
    }

    /// The outputs paying (offer, accept), or the outcome's own list: merged per payee,
    /// ordered by payee, without zero or dust outputs. A side whose whole payout is dust
    /// goes to the other side's first payee.
    pub fn outputs(&self, offer: u64, accept: u64, outcome: Option<&str>) -> Vec<(Payee, u64)> {
        if let Some((_, list)) = outcome.and_then(|o| self.outcomes.iter().find(|(n, _)| n == o)) {
            return merge(list.iter().copied());
        }
        let mut o = self.offer.expand(Payee::Offer, offer);
        let mut a = self.accept.expand(Payee::Accept, accept);
        if o[0].1 < DUST {
            a[0].1 += o[0].1;
            o[0].1 = 0;
        } else if a[0].1 < DUST {
            o[0].1 += a[0].1;
            a[0].1 = 0;
        }
        merge(o.into_iter().chain(a))
    }

    fn check(&self, total: u64, outcomes: &[String]) -> Result<()> {
        let bad = |m: &str| Err(Error::Build(format!("splits: {m}")));
        if self.beneficiaries.len() > MAX_BENEFICIARIES {
            return bad(&format!("more than {MAX_BENEFICIARIES} beneficiaries"));
        }
        let known = |p: &Payee| match p {
            Payee::Beneficiary(i) => *i < self.beneficiaries.len(),
            _ => true,
        };
        for s in [&self.offer, &self.accept] {
            if !s.0.is_empty() && s.0.iter().map(|(_, b)| *b as u64).sum::<u64>() != 10_000 {
                return bad("a side's shares do not sum to 10,000 bps");
            }
            if s.0.iter().any(|(p, b)| *b == 0 || !known(p)) {
                return bad("a share is zero or names an unknown beneficiary");
            }
        }
        for (o, list) in &self.outcomes {
            if !outcomes.contains(o) {
                return bad(&format!("override for unknown outcome {o}"));
            }
            if list.iter().map(|(_, v)| v).sum::<u64>() != total {
                return bad(&format!("outputs for {o} do not sum to the pot"));
            }
            if list.iter().any(|(p, v)| !known(p) || (*v > 0 && *v < DUST)) {
                return bad(&format!("outputs for {o} name an unknown payee or pay dust"));
            }
        }
        Ok(())
    }

    /// (transparent, Ironwood) addresses of a payee.
    pub fn address(
        &self,
        payee: Payee,
        offer: &PartyInfo,
        accept: &PartyInfo,
    ) -> (TransparentAddress, Option<IronwoodAddress>) {
        match payee {
            Payee::Offer => (offer.payout, Some(offer.close)),
            Payee::Accept => (accept.payout, Some(accept.close)),
            Payee::Beneficiary(i) => (self.beneficiaries[i].payout, self.beneficiaries[i].close),
        }
    }
}

fn merge(outs: impl Iterator<Item = (Payee, u64)>) -> Vec<(Payee, u64)> {
    let mut m = std::collections::BTreeMap::new();
    for (p, v) in outs {
        *m.entry(p).or_insert(0u64) += v;
    }
    m.into_iter().filter(|(_, v)| *v > 0).collect()
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
    /// What the CET pays: `offer` and `accept` through the splits.
    pub outputs: Vec<(Payee, u64)>,
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
    /// The party funds from its own Ironwood notes (path A of funding by QR) and brings
    /// no transparent inputs. It builds the funding transaction; the other side checks it.
    pub ironwood: bool,
}

/// An Ironwood funder's notes: spent into the funding output, change back to `change`.
#[derive(Clone)]
pub struct IronwoodFunds {
    pub sk: orchard::keys::SpendingKey,
    pub spends: Vec<tx::IronwoodSpend>,
    pub anchor: orchard::Anchor,
    pub change: IronwoodAddress,
}

#[derive(Clone)]
pub struct Offer {
    pub terms: Terms,
    pub offer: PartyInfo,
    /// An Ironwood-funding offerer's funding PCZT: proved, its spends authorized, its
    /// secrets and binding key redacted. It fixes the funding txid before anyone signs.
    pub funding: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct Accept {
    pub accept: PartyInfo,
    /// Per CET, per unlock.
    pub cet_adaptors: Vec<Vec<AdaptorSignature>>,
    pub refund_sig: ecdsa::Signature,
    /// An Ironwood-funding accepter's funding PCZT, redacted as in [`Offer::funding`].
    pub funding: Option<Vec<u8>>,
}

#[derive(Clone)]
pub struct SignMsg {
    pub cet_adaptors: Vec<Vec<AdaptorSignature>>,
    pub refund_sig: ecdsa::Signature,
    /// The funding PCZT with the offerer's inputs signed (and, for an Ironwood-funding
    /// offerer, its binding key, so the accepter can extract the transaction).
    pub funding_pczt: Vec<u8>,
    /// The offerer's signatures on funding inputs that are legs, by input index.
    pub leg_sigs: Vec<(usize, ecdsa::Signature)>,
}

pub struct TxSet {
    /// The funding transaction's inputs, in order.
    funding_inputs: Vec<Coin>,
    pub params: LocalNetwork,
    pub redeem: FromChain,
    pub funding: Pczt,
    pub funding_txid: [u8; 32],
    pub funding_coin: Coin,
    /// Part of the funding output set aside for the CET or refund fee.
    pub reserve: u64,
    /// The Ironwood funder's own unredacted funding PCZT (accepter side), kept for
    /// `finish`.
    own_funding: Option<Pczt>,
    /// (terms, PCZT, sighash, target height): one entry per CET per era.
    pub cets: Vec<(CetTerms, Pczt, [u8; 32], u32)>,
    pub refund: Pczt,
    pub refund_sighash: [u8; 32],
}

impl TxSet {
    /// The Ironwood funder's own unredacted funding PCZT, if this side built one. It
    /// holds note witnesses and the binding key: store it as secretly as the seed.
    pub fn own_funding(&self) -> Option<&Pczt> {
        self.own_funding.as_ref()
    }

    /// Puts back a stored [`TxSet::own_funding`] after a restart.
    pub fn with_own_funding(mut self, full: Pczt) -> Result<Self> {
        if tx::txid(&full)? != self.funding_txid {
            return Err(Error::Build("stored funding PCZT is not this contract's".into()));
        }
        self.own_funding = Some(full);
        Ok(self)
    }

    fn has_legs(&self) -> bool {
        self.funding_inputs.iter().any(|c| matches!(c.spend, Spend::Leg(_)))
    }

    fn funding_inputs(&self) -> usize {
        self.funding_inputs.len()
    }

    fn funding_inputs_coins(&self) -> &[Coin] {
        &self.funding_inputs
    }
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
        let outcomes = match &self.payouts {
            Payouts::Enumerated(l) => l.iter().map(|(o, _)| o.clone()).collect(),
            Payouts::StableValue(_) => vec![],
        };
        self.splits.check(self.total(), &outcomes)?;
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
                        outputs: self.splits.outputs(*off, total - off, Some(o)),
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
                            outputs: self.splits.outputs(r.recipient, total - r.recipient, None),
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

/// A spec spending the funding coin to `to` (already merged and free of dust). The fee
/// comes out of the reserve and any reserve left over goes to the largest output (the
/// later one on a tie).
fn payout_spec(
    p: &LocalNetwork,
    funding: &Coin,
    reserve: u64,
    sign_height: u32,
    lock_height: Option<u32>,
    to: &[(TransparentAddress, u64)],
) -> Result<Spec> {
    let mut spec = Spec {
        target_height: sign_height,
        expiry_height: 0,
        lock_height,
        inputs: vec![funding.clone()],
        outputs: to.iter().map(|(a, v)| Pay::Transparent(*a, *v)).collect(),
        ironwood_anchor: None,
        ironwood_spends: vec![],
    };
    let fee = tx::fee(p, &spec)?;
    let left = reserve
        .checked_sub(fee)
        .ok_or(Error::Build(format!("fee {fee} exceeds reserve {reserve}")))?;
    let big = (0..to.len()).max_by_key(|&i| to[i].1).unwrap_or(0);
    spec.outputs[big] = Pay::Transparent(to[big].0, to[big].1 + left);
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
    let to: Vec<(TransparentAddress, u64)> = Splits::default()
        .outputs(offer.1, accept.1, None)
        .into_iter()
        .map(|(who, v)| (if who == Payee::Offer { offer.0 } else { accept.0 }, v))
        .collect();
    payout_spec(
        p,
        funding,
        reserve,
        refund_target(p, sign_height, refund_height),
        Some(refund_height),
        &to,
    )
}

/// Derives the whole transaction set from the terms and both parties' info.
pub fn build_txs(
    p: &LocalNetwork,
    terms: &Terms,
    offer: &PartyInfo,
    accept: &PartyInfo,
) -> Result<TxSet> {
    build_txs_with(p, terms, offer, accept, None)
}

fn pczt_bytes(pczt: &Pczt) -> Result<Vec<u8>> {
    pczt.clone().serialize().map_err(|e| Error::Pczt(format!("serialize: {e:?}")))
}

fn parse_pczt(bytes: &[u8]) -> Result<Pczt> {
    Pczt::parse(bytes).map_err(|e| Error::Pczt(format!("parse: {e:?}")))
}

/// Checks a funding PCZT built by an Ironwood funder against the transparent spec both
/// sides derive: exactly the agreed inputs and transparent outputs, lock time, expiry
/// and branch, and Ironwood only flowing in. Whatever else it spends or pays is the
/// builder's own notes and change.
fn check_funding(p: &LocalNetwork, pczt: &Pczt, spec: &Spec) -> Result<()> {
    let bad = |m: &str| Error::Build(format!("funding PCZT refused: {m}"));
    let fx = pczt.clone().into_effects().map_err(|e| Error::Pczt(format!("effects: {e:?}")))?;
    if fx.lock_time() != 0 || u32::from(fx.expiry_height()) != spec.expiry_height {
        return Err(bad("lock time or expiry"));
    }
    if u32::from(fx.consensus_branch_id()) != params::branch_at(p, spec.target_height) {
        return Err(bad("branch"));
    }
    if fx.sapling_bundle().is_some() || fx.orchard_bundle().is_some() {
        return Err(bad("Sapling or Orchard parts"));
    }
    match fx.ironwood_bundle() {
        Some(b) if i64::from(*b.value_balance()) > 0 => {}
        _ => return Err(bad("no Ironwood value flowing in")),
    }
    let t = fx.transparent_bundle().ok_or(bad("no transparent part"))?;
    let ins: Vec<([u8; 32], u32)> = t.vin.iter().map(|i| (*i.prevout().hash(), i.prevout().n())).collect();
    let want_ins: Vec<([u8; 32], u32)> = spec.inputs.iter().map(|c| (c.txid, c.index)).collect();
    if ins != want_ins {
        return Err(bad("inputs differ"));
    }
    let outs: Vec<(Option<TransparentAddress>, u64)> =
        t.vout.iter().map(|o| (o.recipient_address(), o.value().into_u64())).collect();
    let want: Vec<(Option<TransparentAddress>, u64)> = spec
        .outputs
        .iter()
        .map(|o| match o {
            Pay::Transparent(a, v) => (Some(*a), *v),
            _ => (None, 0),
        })
        .collect();
    if outs != want {
        return Err(bad("transparent outputs differ"));
    }
    Ok(())
}

/// The Ironwood funder's side: the agreed transparent spec plus its notes and change,
/// proved and with its spends authorized. Returns the full PCZT (keep it) .
fn build_ironwood_funding(p: &LocalNetwork, spec: &Spec, funds: &IronwoodFunds) -> Result<Pczt> {
    let notes: u64 = funds.spends.iter().map(|s| s.note.value().inner()).sum();
    let ins: u64 = spec.inputs.iter().map(|c| c.value).sum();
    let outs: u64 = spec.outputs.iter().map(Pay::value).sum();
    let mut full = spec.clone();
    full.ironwood_spends = funds.spends.clone();
    full.ironwood_anchor = Some(funds.anchor);
    full.outputs.push(Pay::Ironwood(funds.change, 0));
    let fee = tx::fee(p, &full)?;
    let change = (notes + ins)
        .checked_sub(outs + fee)
        .ok_or(Error::Build(format!("notes ({notes} zat) do not cover the contract, share and fee")))?;
    *full.outputs.last_mut().unwrap() = Pay::Ironwood(funds.change, change);
    let (pczt, n) = tx::sign_ironwood(tx::build(p, &full)?, &orchard::keys::SpendAuthorizingKey::from(&funds.sk))?;
    if n != funds.spends.len() {
        return Err(Error::Build(format!("authorized {n} of {} spends", funds.spends.len())));
    }
    check_funding(p, &pczt, spec)?;
    Ok(pczt)
}

/// What each side's legs must hold (offer, accept): its collateral and share of the
/// reserve and funding fee. `offer.inputs` and `accept.inputs` are the legs (one coin
/// each, any value); a side that funds nothing needs 0. The amounts leg links ask for.
pub fn leg_amounts(p: &LocalNetwork, terms: &Terms, offer: &PartyInfo, accept: &PartyInfo) -> Result<(u64, u64)> {
    let big = terms.total() + 1_0000_0000;
    let probe = |info: &PartyInfo| {
        let mut i = info.clone();
        for c in &mut i.inputs {
            c.value = big;
        }
        i
    };
    let (o, a) = (probe(offer), probe(accept));
    let plan = plan(p, terms, &o, &a)?;
    let need = |info: &PartyInfo| -> u64 {
        if info.inputs.is_empty() {
            return 0;
        }
        let back: u64 = plan
            .spec
            .outputs
            .iter()
            .filter_map(|x| match x {
                Pay::Transparent(t, v) if *t == info.change => Some(*v),
                _ => None,
            })
            .sum();
        big * info.inputs.len() as u64 - back
    };
    Ok((need(&o), need(&a)))
}

/// Derives the transaction set; `funding` is the Ironwood funder's PCZT when one party
/// funds from Ironwood (built by that party, checked here), and `None` otherwise.
pub fn build_txs_with(
    p: &LocalNetwork,
    terms: &Terms,
    offer: &PartyInfo,
    accept: &PartyInfo,
    funding: Option<&Pczt>,
) -> Result<TxSet> {
    let (txs, _) = derive_txs(p, terms, offer, accept, Funding::Given(funding))?;
    Ok(txs)
}

enum Funding<'a> {
    /// Build it (no Ironwood funder) or check the given one.
    Given(Option<&'a Pczt>),
    /// Build it as the Ironwood funder.
    Mine(&'a IronwoodFunds),
}

/// Returns the transaction set and, for the Ironwood funder, its full funding PCZT.
/// The funding transaction's transparent part, as both sides derive it.
struct Plan {
    redeem: FromChain,
    reserve: u64,
    fund_value: u64,
    spec: Spec,
    /// How many parties fund from Ironwood (0 or 1).
    iron: usize,
    all: Vec<CetTerms>,
}

fn plan(p: &LocalNetwork, terms: &Terms, offer: &PartyInfo, accept: &PartyInfo) -> Result<Plan> {
    let redeem = script::multisig_2of2(&offer.funding_pubkey, &accept.funding_pubkey)?;
    let total = terms.total();
    let probe = Coin {
        txid: [0; 32],
        index: 0,
        value: total + 1_000_000,
        spend: Spend::P2sh(redeem.clone()),
    };
    // The reserve covers the largest CET: the one paying the most outputs.
    let all = terms.cet_terms()?;
    let widest = all.iter().map(|ct| ct.outputs.len()).max().unwrap_or(0).max(2);
    let mut reserve = tx::fee(
        p,
        &Spec {
            target_height: terms.sign_height,
            expiry_height: 0,
            lock_height: None,
            inputs: vec![probe],
            outputs: vec![Pay::Transparent(offer.payout, 1); widest],
            ironwood_anchor: None,
            ironwood_spends: vec![],
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
        .filter(|(info, collateral, credit)| !info.inputs.is_empty() || *collateral > 0 || *credit > 0 || info.ironwood)
        .collect();
    let iron = [offer, accept].iter().filter(|i| i.ironwood).count();
    if iron > 1 {
        return Err(Error::Build("both parties fund from Ironwood: one must first move to transparent".into()));
    }
    if iron == 1 && (roll.is_some() || [offer, accept].iter().any(|i| i.ironwood && !i.inputs.is_empty())) {
        return Err(Error::Build("an Ironwood funder brings notes only, and does not roll".into()));
    }
    // Legs: every funding input is one, keyed to this contract's 2-of-2 and refund height.
    let legs = inputs.iter().filter(|c| matches!(c.spend, Spend::Leg(_))).count();
    if legs > 0 {
        if legs != inputs.len() || iron > 0 {
            return Err(Error::Build("a contract funded by legs is funded by legs only".into()));
        }
        for c in &inputs {
            if let Spend::Leg(l) = &c.spend {
                if l.a != offer.funding_pubkey || l.b != accept.funding_pubkey || l.reclaim_height != terms.refund_height {
                    return Err(Error::Build("a leg is not keyed to this contract".into()));
                }
            }
        }
    }
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
        ironwood_spends: vec![],
    };
    let fee = tx::fee(p, &spec)?;
    let owed = (reserve + fee).checked_sub(old_extra).ok_or(Error::Build("old reserve exceeds the new costs".into()))?;
    let n = funders.len() as u64;
    let mut live = vec![Pay::Transparent(p2sh, fund_value)];
    for (k, (info, collateral, credit)) in funders.iter().enumerate() {
        let share = owed / n + if k == 0 { owed % n } else { 0 };
        if info.ironwood {
            // Brings exactly its collateral and share from Ironwood, plus whatever its
            // own Ironwood actions add to the fee; its change stays shielded.
            continue;
        }
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
    let iron_change = (iron > 0) as usize;
    if live.len() + iron_change != spec.outputs.len() {
        // A funder's leftover was dust (a roll that nets to nothing): the fee absorbs it.
        spec.outputs = live;
        let refee = tx::fee(p, &spec)?;
        if refee > fee {
            return Err(Error::Build("fee changed after dropping dust".into()));
        }
    } else {
        spec.outputs = live;
    }
    Ok(Plan { redeem, reserve, fund_value, spec, iron, all })
}

/// What an Ironwood funder's notes must hold for this contract when it spends `notes`
/// of them: its collateral and share of the reserve and fee, plus the Ironwood actions
/// (one per note; the first also carries the change). The amount a deposit link asks
/// for; a top-up link asks for the two-note amount less what arrived.
pub fn ironwood_deposit(p: &LocalNetwork, terms: &Terms, offer: &PartyInfo, accept: &PartyInfo, notes: usize) -> Result<u64> {
    let plan = plan(p, terms, offer, accept)?;
    let ins: u64 = plan.spec.inputs.iter().map(|c| c.value).sum();
    let outs: u64 = plan.spec.outputs.iter().map(Pay::value).sum();
    let mut full = plan.spec.clone();
    // Ironwood actions = max(spends, outputs): `notes` placeholder outputs count the same.
    full.outputs.extend(std::iter::repeat_n(Pay::Ironwood(offer.close, 0), notes.max(1)));
    full.ironwood_anchor = Some(orchard::Anchor::empty_tree());
    Ok(outs + tx::fee(p, &full)? - ins)
}

fn derive_txs(
    p: &LocalNetwork,
    terms: &Terms,
    offer: &PartyInfo,
    accept: &PartyInfo,
    given: Funding<'_>,
) -> Result<(TxSet, Option<Pczt>)> {
    let Plan { redeem, reserve, fund_value, spec, iron, all } = plan(p, terms, offer, accept)?;
    let (funding, full) = match given {
        Funding::Given(None) if iron == 0 => (tx::build(p, &spec)?, None),
        Funding::Given(None) => return Err(Error::Build("needs the Ironwood funder's funding PCZT".into())),
        Funding::Given(Some(_)) if iron == 0 => return Err(Error::Build("a funding PCZT without an Ironwood funder".into())),
        Funding::Given(Some(pczt)) => {
            check_funding(p, pczt, &spec)?;
            (pczt.clone(), None)
        }
        Funding::Mine(funds) => {
            let full = build_ironwood_funding(p, &spec, funds)?;
            (tx::redact_ironwood(full.clone(), false), Some(full))
        }
    };
    let funding_txid = tx::txid(&funding)?;
    let spec_inputs = spec.inputs.clone();
    let funding_coin = Coin {
        txid: funding_txid,
        index: 0,
        value: fund_value,
        spend: Spend::P2sh(redeem.clone()),
    };

    let mut cets = vec![];
    for era in terms.eras(p) {
        for ct in &all {
            let to: Vec<(TransparentAddress, u64)> = ct
                .outputs
                .iter()
                .map(|(who, v)| (terms.splits.address(*who, offer, accept).0, *v))
                .collect();
            let spec = payout_spec(p, &funding_coin, reserve, era, None, &to)?;
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
    Ok((
        TxSet {
            params: *p,
            redeem,
            funding,
            funding_txid,
            funding_coin,
            reserve,
            funding_inputs: spec_inputs,
            own_funding: full.clone(),
            cets,
            refund,
            refund_sighash,
        },
        full,
    ))
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
    /// Notes to fund from, when `info.ironwood`.
    pub ironwood: Option<IronwoodFunds>,
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
            funding: None,
        }
    }

    /// An Ironwood-funding offerer's offer: it needs the accepter's info in advance (a
    /// desk maker's quote carries it) to build the funding transaction, which fixes the
    /// txid every CET spends. Keep the returned PCZT for [`Party::sign_funded`].
    pub fn offer_funded(&self, p: &LocalNetwork, terms: Terms, accept: &PartyInfo) -> Result<(Offer, Pczt)> {
        let funds = self.ironwood.as_ref().ok_or(Error::Build("no Ironwood funds".into()))?;
        let (txs, full) = derive_txs(p, &terms, &self.info, accept, Funding::Mine(funds))?;
        let offer = Offer { terms, offer: self.info.clone(), funding: Some(pczt_bytes(&txs.funding)?) };
        Ok((offer, full.expect("built as the funder")))
    }

    /// Accepter: checks the terms, derives the transactions, adaptor-signs every CET and
    /// signs the refund. An Ironwood-funding accepter builds the funding transaction here
    /// and sends it, redacted, with its signatures.
    pub fn accept(&self, p: &LocalNetwork, tip: u32, offer: &Offer) -> Result<(Accept, TxSet)> {
        offer.terms.validate(p, tip)?;
        let txs = match (&self.ironwood, &offer.funding) {
            (Some(_), Some(_)) => return Err(Error::Build("both parties fund from Ironwood".into())),
            (Some(funds), None) => derive_txs(p, &offer.terms, &offer.offer, &self.info, Funding::Mine(funds))?.0,
            (None, Some(b)) => build_txs_with(p, &offer.terms, &offer.offer, &self.info, Some(&parse_pczt(b)?))?,
            (None, None) => build_txs(p, &offer.terms, &offer.offer, &self.info)?,
        };
        let accept = Accept {
            accept: self.info.clone(),
            cet_adaptors: adaptors(&txs, &self.funding_sk),
            refund_sig: ecdsa::sign(Message::from_digest(txs.refund_sighash), &self.funding_sk),
            funding: match self.ironwood {
                Some(_) => Some(pczt_bytes(&txs.funding)?),
                None => None,
            },
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
        self.sign_funded(p, tip, offer, accept, None)
    }

    /// [`Party::sign`], for an offerer that funds from Ironwood with `own` (from
    /// [`Party::offer_funded`]): its message carries the binding key, so the accepter
    /// can extract the funding transaction once it has signed its own inputs.
    pub fn sign_funded(
        &self,
        p: &LocalNetwork,
        tip: u32,
        offer: &Offer,
        accept: &Accept,
        own: Option<&Pczt>,
    ) -> Result<(SignMsg, Contract)> {
        offer.terms.validate(p, tip)?;
        let txs = match (own, &accept.funding) {
            (Some(_), Some(_)) => return Err(Error::Build("both parties fund from Ironwood".into())),
            (Some(full), None) => build_txs_with(p, &offer.terms, &self.info, &accept.accept, Some(&tx::redact_ironwood(full.clone(), false)))?,
            (None, Some(b)) => build_txs_with(p, &offer.terms, &self.info, &accept.accept, Some(&parse_pczt(b)?))?,
            (None, None) => build_txs(p, &offer.terms, &self.info, &accept.accept)?,
        };
        check_adaptors(&txs, &accept.cet_adaptors, &accept.accept.funding_pubkey)?;
        check_sig(&accept.refund_sig, txs.refund_sighash, &accept.accept.funding_pubkey)?;
        let mut leg_sigs = vec![];
        let funding = if let Some(full) = own {
            if tx::txid(full)? != txs.funding_txid {
                return Err(Error::Build("own funding PCZT is not this contract's".into()));
            }
            tx::redact_ironwood(full.clone(), true)
        } else if txs.has_legs() {
            for i in 0..txs.funding_inputs() {
                leg_sigs.push((i, crate::leg::sign(&txs.funding, i, &self.funding_sk)?));
            }
            txs.funding.clone()
        } else {
            let mut funding = txs.funding.clone();
            let base = self.sign_roll(&offer.terms, &mut funding)?;
            for (i, k) in self.input_keys.iter().enumerate() {
                funding = tx::sign(funding, base + i, k)?;
            }
            funding
        };
        let msg = SignMsg {
            cet_adaptors: adaptors(&txs, &self.funding_sk),
            refund_sig: ecdsa::sign(Message::from_digest(txs.refund_sighash), &self.funding_sk),
            funding_pczt: pczt_bytes(&funding)?,
            leg_sigs,
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
        let mut funding = parse_pczt(&sign.funding_pczt)?;
        if tx::txid(&funding)? != txs.funding_txid {
            return Err(Error::Build("funding PCZT is not the agreed transaction".into()));
        }
        if let Some(full) = &txs.own_funding {
            // Our proofs, spend authorizations and binding key, with their signatures.
            funding = pczt::roles::combiner::Combiner::new(vec![full.clone(), funding])
                .combine()
                .map_err(|e| Error::Pczt(format!("combine: {e:?}")))?;
        }
        let f = if txs.has_legs() {
            let mut script_sigs = vec![];
            for (i, c) in txs.funding_inputs_coins().iter().enumerate() {
                let Spend::Leg(leg) = &c.spend else { unreachable!("checked in derive_txs") };
                let theirs = sign
                    .leg_sigs
                    .iter()
                    .find(|(j, _)| *j == i)
                    .map(|(_, s)| *s)
                    .ok_or(Error::Build(format!("no offerer signature on leg input {i}")))?;
                check_sig(&theirs, tx::sighash(&funding, i)?, &offer.offer.funding_pubkey)?;
                let mine = crate::leg::sign(&funding, i, &self.funding_sk)?;
                script_sigs.push(leg.joint_script_sig(&theirs, &mine));
            }
            tx::extract_with_script_sigs(funding, script_sigs)?
        } else {
            let base = self.sign_roll(&offer.terms, &mut funding)?;
            let first = base + offer.offer.inputs.len();
            for (i, k) in self.input_keys.iter().enumerate() {
                funding = tx::sign(funding, first + i, k)?;
            }
            tx::extract(funding)?
        };
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

impl Party {
    /// Rebuilds this side's contract from the setup messages (after a restart): derives
    /// the transactions again and checks the counterparty's signatures, without the
    /// tip check that guards a fresh setup.
    pub fn restore(&self, p: &LocalNetwork, is_offerer: bool, offer: &Offer, accept: &Accept, sign: &SignMsg) -> Result<Contract> {
        let given = match (&offer.funding, &accept.funding) {
            (Some(b), None) | (None, Some(b)) => Some(parse_pczt(b)?),
            (None, None) => None,
            (Some(_), Some(_)) => return Err(Error::Build("both parties fund from Ironwood".into())),
        };
        let txs = build_txs_with(p, &offer.terms, &offer.offer, &accept.accept, given.as_ref())?;
        let (them, adaptors, refund_sig) = if is_offerer {
            (&accept.accept, &accept.cet_adaptors, accept.refund_sig)
        } else {
            (&offer.offer, &sign.cet_adaptors, sign.refund_sig)
        };
        check_adaptors(&txs, adaptors, &them.funding_pubkey)?;
        check_sig(&refund_sig, txs.refund_sighash, &them.funding_pubkey)?;
        Ok(Contract {
            is_offerer,
            terms: offer.terms.clone(),
            offer: offer.offer.clone(),
            accept: accept.accept.clone(),
            txs,
            funding_sk: self.funding_sk,
            close_ivk: self.close_ivk(),
            their_adaptors: adaptors.clone(),
            their_refund_sig: refund_sig,
        })
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
        let outs = self.terms.splits.outputs(payout.0, payout.1, None);
        self.close_spec_to(p, tip, &outs, anchor)
    }

    /// The close of an enumerated contract at `outcome`: pays what its CET pays.
    pub fn close_outcome(
        &self,
        p: &LocalNetwork,
        tip: u32,
        outcome: &str,
        anchor: orchard::Anchor,
    ) -> Result<Spec> {
        let ct = self
            .txs
            .cets
            .iter()
            .find(|(ct, ..)| ct.unlocks.iter().any(|u| u.outcome.as_deref() == Some(outcome)))
            .ok_or(Error::Build(format!("no CET for outcome {outcome}")))?;
        self.close_spec_to(p, tip, &ct.0.outputs.clone(), anchor)
    }

    /// The close paying `outs`: Ironwood to every payee with a close address, transparent
    /// to the rest. Payees share the fee equally; the reserve pays it first.
    pub fn close_spec_to(
        &self,
        p: &LocalNetwork,
        tip: u32,
        outs: &[(Payee, u64)],
        anchor: orchard::Anchor,
    ) -> Result<Spec> {
        let value = self.txs.funding_coin.value;
        let pay = |who: Payee, v: u64| match self.terms.splits.address(who, &self.offer, &self.accept) {
            (_, Some(z)) => Pay::Ironwood(z, v),
            (t, None) => Pay::Transparent(t, v),
        };
        let live: Vec<(Payee, u64)> = outs.iter().filter(|(_, v)| *v >= DUST).copied().collect();
        let mut spec = Spec {
            target_height: tip + 1,
            expiry_height: tip + 1 + EXPIRY_DELTA,
            lock_height: None,
            inputs: vec![self.txs.funding_coin.clone()],
            outputs: live.iter().map(|(w, v)| pay(*w, *v)).collect(),
            ironwood_anchor: Some(anchor),
            ironwood_spends: vec![],
        };
        let fee = tx::fee(p, &spec)?;
        // The reserve and any dust go into the fee first; the rest is split.
        let paid: u64 = live.iter().map(|(_, v)| v).sum();
        let extra = value - paid;
        let owed = fee.saturating_sub(extra);
        let n = live.len() as u64;
        let mut amounts = vec![];
        for (k, (_, v)) in live.iter().enumerate() {
            let share = owed / n + if (k as u64) < owed % n { 1 } else { 0 };
            amounts.push(
                v.checked_sub(share)
                    .ok_or(Error::Build("payout below its fee share".into()))?,
            );
        }
        let sum: u64 = amounts.iter().sum();
        if let Some(last) = amounts.last_mut() {
            *last += value - fee - sum;
        }
        spec.outputs = live.iter().zip(amounts).map(|((w, _), v)| pay(*w, v)).collect();
        Ok(spec)
    }

    /// The other side's check of a close PCZT before signing. The close is built by one
    /// party, so the other checks what the transaction will actually do, not what the
    /// PCZT's metadata claims:
    /// - it spends exactly the funding outpoint, has no lock time and the agreed expiry;
    /// - every Ironwood output's note (recipient, value, rho, rseed) re-derives its `cmx`,
    ///   so the claimed recipient and value are what the chain commits to;
    /// - the outputs, Ironwood and transparent, are exactly the agreed (recipient, value)
    ///   pairs, every beneficiary included;
    /// - this party's own output trial-decrypts with its viewing key, so its wallet will
    ///   find the note.
    pub fn check_close(&self, pczt: &Pczt, spec: &Spec) -> Result<()> {
        let bad = |m: String| Error::Build(format!("close PCZT refused: {m}"));
        let mut want: Vec<(Out, u64)> = spec
            .outputs
            .iter()
            .map(|o| match o {
                Pay::Ironwood(a, v) => Ok((Out::Ironwood(a.to_raw_address_bytes()), *v)),
                Pay::Transparent(a, v) => Ok((Out::Transparent(*a), *v)),
                _ => Err(bad("the agreed close pays a memo".into())),
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
        let my_value = want.iter().find(|(a, _)| *a == Out::Ironwood(mine)).map(|(_, v)| *v);
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
                } else {
                    for o in t.outputs() {
                        match TransparentAddress::from_script_pubkey(o.script_pubkey()) {
                            Some(a) => got.push((Out::Transparent(a), o.value().into_u64())),
                            None => {
                                transparent = Err("a transparent output to a non-standard script".into());
                                return Ok(());
                            }
                        }
                    }
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
                        got.push((Out::Ironwood(r), v));
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

/// A close output's recipient, for comparing what was agreed with what the PCZT pays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Out {
    Transparent(TransparentAddress),
    Ironwood([u8; 43]),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(i: usize) -> Payee {
        Payee::Beneficiary(i)
    }

    #[test]
    fn default_split_pays_each_side_to_itself() {
        let s = Splits::default();
        assert_eq!(s.outputs(7, 3_000, None), vec![(Payee::Accept, 3_007)], "dust side goes across");
        assert_eq!(s.outputs(500, 0, None), vec![(Payee::Offer, 500)]);
        assert_eq!(s.outputs(500, 600, None), vec![(Payee::Offer, 500), (Payee::Accept, 600)]);
    }

    #[test]
    fn shares_round_down_and_the_remainder_goes_first() {
        let s = Splits {
            accept: Split(vec![(b(0), 5_000), (b(1), 3_000), (b(2), 2_000)]),
            ..Default::default()
        };
        assert_eq!(
            s.outputs(0, 1_5000_0000, None),
            vec![(b(0), 7500_0000), (b(1), 4500_0000), (b(2), 3000_0000)]
        );
        // 1,001 zat: 500.5 / 300.3 / 200.2 round down to 500 / 300 / 200; 1 to the first.
        assert_eq!(s.outputs(0, 1_001, None), vec![(b(0), 501), (b(1), 300), (b(2), 200)]);
    }

    #[test]
    fn dust_shares_stay_on_their_side() {
        let s = Splits {
            offer: Split(vec![(Payee::Offer, 9_900), (b(1), 100)]),
            accept: Split(vec![(b(0), 9_000), (Payee::Accept, 1_000)]),
            ..Default::default()
        };
        // 100 bps of 5,000 is 50 zat: dust, merged into the offer side's first payee.
        assert_eq!(
            s.outputs(5_000, 10_000, None),
            vec![(Payee::Offer, 5_000), (Payee::Accept, 1_000), (b(0), 9_000)]
        );
        // A side's own payee may appear on both sides: its outputs merge.
        let t = Splits { accept: Split(vec![(Payee::Offer, 5_000), (b(0), 5_000)]), ..Default::default() };
        assert_eq!(t.outputs(1_000, 2_000, None), vec![(Payee::Offer, 2_000), (b(0), 1_000)]);
    }

    #[test]
    fn overrides_replace_the_split_for_their_outcome() {
        let s = Splits {
            accept: Split(vec![(b(0), 10_000)]),
            outcomes: vec![("paid".into(), vec![(b(0), 600), (Payee::Offer, 400)])],
            ..Default::default()
        };
        assert_eq!(s.outputs(0, 1_000, Some("paid")), vec![(Payee::Offer, 400), (b(0), 600)]);
        assert_eq!(s.outputs(0, 1_000, Some("other")), vec![(b(0), 1_000)]);
    }

    #[test]
    fn splits_are_checked() {
        let ben = Beneficiary { label: "x".into(), payout: TransparentAddress::PublicKeyHash([1; 20]), close: None };
        let with = |offer: Vec<(Payee, u32)>, outcomes| Splits {
            beneficiaries: vec![ben.clone()],
            offer: Split(offer),
            accept: Split::default(),
            outcomes,
        };
        let names = ["a".to_string()];
        assert!(with(vec![(b(0), 10_000)], vec![]).check(100, &names).is_ok());
        assert!(with(vec![(b(0), 9_999)], vec![]).check(100, &names).is_err(), "bps sum");
        assert!(with(vec![(b(1), 10_000)], vec![]).check(100, &names).is_err(), "unknown beneficiary");
        assert!(with(vec![(b(0), 10_000), (Payee::Offer, 0)], vec![]).check(100, &names).is_err(), "zero share");
        assert!(with(vec![], vec![("a".into(), vec![(b(0), 99)])]).check(100, &names).is_err(), "override sum");
        assert!(with(vec![], vec![("a".into(), vec![(b(0), 60), (Payee::Offer, 40)])]).check(100, &names).is_err(), "override dust");
        assert!(with(vec![], vec![("z".into(), vec![(b(0), 100)])]).check(100, &names).is_err(), "unknown outcome");
        let many = Splits { beneficiaries: vec![ben; MAX_BENEFICIARIES + 1], ..Default::default() };
        assert!(many.check(100, &names).is_err(), "too many");
    }
}
