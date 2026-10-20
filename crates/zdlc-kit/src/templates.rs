//! Contract templates: plain inputs in, exact `Terms` out.
//!
//! The offerer sends the template with its offer. The accepter fetches the announcements
//! from its own oracles, derives the terms from the template again and requires them to
//! equal the offer's terms byte for byte: what it reviewed is what it signs.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zdlc::{
    contract::{Beneficiary, Event as CEvent, Payee, Payouts, Splits, Terms},
    enclave::{AnnouncementMsg, EventSpec},
    keys, oracle_client, paylink,
    payout::StableValue,
};

use crate::{Error, Result, Role};

/// One payee of the offerer's side (escrow).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PayeeSpec {
    pub label: String,
    /// Transparent address CETs pay.
    pub address: String,
    /// Unified address whose Ironwood receiver the cooperative close pays; none = the
    /// transparent address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shielded: Option<String>,
    /// Basis points of the side's payout; the payees' shares sum to 10,000.
    pub bps: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "template", rename_all = "snake_case")]
pub enum Template {
    /// The offerer is owed `usd_cents` in ZEC at the price minute; the accepter hedges
    /// with the same collateral and earns the premium.
    StablePayment {
        usd_cents: u64,
        collateral_zat: u64,
        #[serde(default)]
        premium_zat: u64,
        maturity_blocks: u32,
        refund_blocks: u32,
        /// Unix time of the ZEC/USD minute the oracles attest; the offerer fills it in.
        #[serde(default)]
        price_minute: Option<i64>,
    },
    /// The offerer (the contractor) is paid `amount_zat` when the arbiters resolve
    /// "delivered"; the accepter (the client) funds the escrow alone and gets it back on
    /// "not delivered" or by the refund.
    Escrow {
        amount_zat: u64,
        question: String,
        #[serde(default)]
        payees: Vec<PayeeSpec>,
        maturity_blocks: u32,
        refund_blocks: u32,
    },
    /// Both stake `stake_zat`; the winner takes both stakes.
    Bet {
        question: String,
        outcomes: Vec<String>,
        /// Outcomes the offerer wins; the accepter wins the rest.
        offerer_wins: Vec<String>,
        stake_zat: u64,
        maturity_blocks: u32,
        refund_blocks: u32,
    },
}

const DELIVERED: &str = "delivered";
const NOT_DELIVERED: &str = "not delivered";

fn bad(m: impl Into<String>) -> Error {
    Error::Template(m.into())
}

impl Template {
    pub fn validate(&self) -> Result<()> {
        let blocks = |m: u32, r: u32| if m == 0 || r <= m { Err(bad("need 0 < maturity_blocks < refund_blocks")) } else { Ok(()) };
        match self {
            Template::StablePayment { usd_cents, collateral_zat, maturity_blocks, refund_blocks, .. } => {
                if *usd_cents == 0 || *collateral_zat == 0 {
                    return Err(bad("usd_cents and collateral_zat must be positive"));
                }
                blocks(*maturity_blocks, *refund_blocks)
            }
            Template::Escrow { amount_zat, payees, maturity_blocks, refund_blocks, .. } => {
                if *amount_zat == 0 {
                    return Err(bad("amount_zat must be positive"));
                }
                if !payees.is_empty() && payees.iter().map(|p| p.bps as u64).sum::<u64>() != 10_000 {
                    return Err(bad("payee shares must sum to 10,000 bps"));
                }
                blocks(*maturity_blocks, *refund_blocks)
            }
            Template::Bet { outcomes, offerer_wins, stake_zat, maturity_blocks, refund_blocks, .. } => {
                if outcomes.len() < 2 || *stake_zat == 0 || offerer_wins.iter().any(|o| !outcomes.contains(o)) {
                    return Err(bad("a bet needs two or more outcomes, a stake, and winning outcomes among them"));
                }
                blocks(*maturity_blocks, *refund_blocks)
            }
        }
    }

    /// Fills what the offerer decides at offer time (the price minute).
    pub fn fill(&mut self, now_unix: i64) {
        if let Template::StablePayment { price_minute, maturity_blocks, .. } = self {
            if price_minute.is_none() {
                // About 75 s a block: the minute that closes around maturity.
                *price_minute = Some((now_unix + 75 * *maturity_blocks as i64) / 60 * 60);
            }
        }
    }

    /// What the oracles are asked to attest.
    pub fn event(&self) -> Result<EventSpec> {
        Ok(match self {
            Template::StablePayment { price_minute, .. } => EventSpec::ZecUsd {
                minute: price_minute.ok_or(bad("price_minute not filled"))?,
                unit_cents: 100,
                nb_digits: 14,
            },
            Template::Escrow { question, .. } => EventSpec::Arbiter {
                question: question.clone(),
                outcomes: vec![DELIVERED.into(), NOT_DELIVERED.into()],
            },
            Template::Bet { question, outcomes, .. } => EventSpec::Arbiter { question: question.clone(), outcomes: outcomes.clone() },
        })
    }

    fn heights(&self) -> (u32, u32) {
        match self {
            Template::StablePayment { maturity_blocks, refund_blocks, .. }
            | Template::Escrow { maturity_blocks, refund_blocks, .. }
            | Template::Bet { maturity_blocks, refund_blocks, .. } => (*maturity_blocks, *refund_blocks),
        }
    }

    /// The exact terms, from the oracles' announcements of `event()` and the sign height.
    pub fn terms(&self, anns: &[AnnouncementMsg], sign_height: u32) -> Result<Terms> {
        self.validate()?;
        let want = self.event()?;
        if anns.is_empty() || anns.iter().any(|a| a.spec != want) {
            return Err(bad("announcements are not of this contract's event"));
        }
        let (m, r) = self.heights();
        let tip = sign_height - 1;
        let base = |event, payouts, offer_collateral, accept_collateral, splits| Terms {
            event,
            payouts,
            offer_collateral,
            accept_collateral,
            maturity: tip + m,
            refund_height: tip + r,
            sign_height,
            dual_sign: false,
            roll: None,
            splits,
        };
        let enumerated = || -> Result<CEvent> {
            Ok(CEvent::Enumerated(anns.iter().map(oracle_client::to_enumerated).collect::<zdlc::Result<_>>()?))
        };
        Ok(match self {
            Template::StablePayment { usd_cents, collateral_zat, premium_zat, .. } => {
                let event = CEvent::Numeric(anns.iter().map(oracle_client::to_numeric).collect::<zdlc::Result<_>>()?);
                let curve = StableValue {
                    d_cents: *usd_cents,
                    q_recipient: *collateral_zat,
                    q_hedger: *collateral_zat,
                    unit_cents: 100,
                    nb_digits: 14,
                    rounding: (2 * collateral_zat / 100).max(1),
                    premium: *premium_zat,
                };
                base(event, Payouts::StableValue(curve), collateral_zat + premium_zat, *collateral_zat, Splits::default())
            }
            Template::Escrow { amount_zat, payees, .. } => {
                let payouts = Payouts::Enumerated(vec![(DELIVERED.into(), *amount_zat), (NOT_DELIVERED.into(), 0)]);
                base(enumerated()?, payouts, 0, *amount_zat, splits(payees)?)
            }
            Template::Bet { outcomes, offerer_wins, stake_zat, .. } => {
                let payouts = Payouts::Enumerated(
                    outcomes.iter().map(|o| (o.clone(), if offerer_wins.contains(o) { 2 * stake_zat } else { 0 })).collect(),
                );
                base(enumerated()?, payouts, *stake_zat, *stake_zat, Splits::default())
            }
        })
    }

    /// What each side puts in.
    pub fn collateral(&self, role: Role) -> u64 {
        match (self, role) {
            (Template::StablePayment { collateral_zat, premium_zat, .. }, Role::Offerer) => collateral_zat + premium_zat,
            (Template::StablePayment { collateral_zat, .. }, Role::Accepter) => *collateral_zat,
            (Template::Escrow { .. }, Role::Offerer) => 0,
            (Template::Escrow { amount_zat, .. }, Role::Accepter) => *amount_zat,
            (Template::Bet { stake_zat, .. }, _) => *stake_zat,
        }
    }

    /// A plain description for the side about to accept or offer.
    pub fn summary(&self, role: Role) -> Value {
        let mine = self.collateral(role);
        match self {
            Template::StablePayment { usd_cents, collateral_zat, premium_zat, .. } => json!({
                "template": "stable_payment",
                "you": if role == Role::Offerer { "recipient: owed the dollar amount in ZEC" } else { "hedger: pays the dollar amount, keeps the rest, earns the premium" },
                "usd": format!("{}.{:02}", usd_cents / 100, usd_cents % 100),
                "collateral_zec": paylink::zec(*collateral_zat),
                "premium_zec": paylink::zec(*premium_zat),
                "you_put_in_zec": paylink::zec(mine),
            }),
            Template::Escrow { amount_zat, question, payees, .. } => json!({
                "template": "escrow",
                "question": question,
                "you": if role == Role::Offerer { "contractor: paid on \"delivered\"" } else { "client: funds the escrow, refunded on \"not delivered\"" },
                "amount_zec": paylink::zec(*amount_zat),
                "payees": payees.iter().map(|p| json!({"label": p.label, "address": p.address, "bps": p.bps})).collect::<Vec<_>>(),
                "you_put_in_zec": paylink::zec(mine),
            }),
            Template::Bet { question, outcomes, offerer_wins, stake_zat, .. } => json!({
                "template": "bet",
                "question": question,
                "you_win_on": outcomes.iter().filter(|o| offerer_wins.contains(o) == (role == Role::Offerer)).collect::<Vec<_>>(),
                "stake_zec": paylink::zec(*stake_zat),
            }),
        }
    }
}

fn splits(payees: &[PayeeSpec]) -> Result<Splits> {
    let mut s = Splits::default();
    for (i, p) in payees.iter().enumerate() {
        let payout = keys::decode(&p.address).ok_or(bad(format!("{}: not a transparent address", p.address)))?.1;
        let close = match &p.shielded {
            Some(ua) => Some(paylink::decode_unified(ua).ok_or(bad(format!("{ua}: no Ironwood receiver")))?.1),
            None => None,
        };
        s.beneficiaries.push(Beneficiary { label: p.label.clone(), payout, close });
        s.offer.0.push((Payee::Beneficiary(i), p.bps));
    }
    Ok(s)
}
