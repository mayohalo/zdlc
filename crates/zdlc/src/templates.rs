//! Contract templates: plain inputs in, exact `Terms` out.
//!
//! The offerer sends the template with its offer. The accepter fetches the announcements
//! from its own oracles, derives the terms from the template again and requires them to
//! equal the offer's terms byte for byte: what it reviewed is what it signs.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    contract::{Beneficiary, Event as CEvent, Payee, Payouts, Splits, Terms},
    events::{self as oracle_client, AnnouncementMsg, EventSpec},
    keys, paylink,
    payout::StableValue,
    Error, Result,
};

/// Which side of a contract this node plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Offerer,
    Accepter,
}

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
        /// Payouts rounded to this share of the collateral, in basis points (default 200,
        /// 2%). Wider rounding, fewer CETs to sign: a demo uses 500.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rounding_bps: Option<u64>,
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
    /// One bidder's contract in a sealed-bid auction. The offerer (the seller) is paid the
    /// clearing price when the auctioneer rules this bidder "won at" a price; the accepter
    /// (the bidder) locks `deposit_zat` and gets the rest back, all of it on "lost" or by
    /// the refund. Every bidder locks the same deposit, so the chain cannot tell their
    /// bids apart. `prices_zat` are the possible clearing prices, rising.
    Auction {
        item: String,
        deposit_zat: u64,
        prices_zat: Vec<u64>,
        maturity_blocks: u32,
        refund_blocks: u32,
    },
    /// The offerer protects `amount_zat` of ZEC at `price_cents` (USD per ZEC): it is
    /// owed that dollar value in ZEC at the price minute. The accepter hedges with the
    /// same amount and earns the premium.
    Hedge {
        amount_zat: u64,
        price_cents: u64,
        #[serde(default)]
        premium_zat: u64,
        maturity_blocks: u32,
        refund_blocks: u32,
        #[serde(default)]
        price_minute: Option<i64>,
    },
    /// A salary in dollars, paid in ZEC each period: one stable payment per period, the
    /// next offered when the last settles. The offerer (employee) is owed `usd_cents`; the
    /// accepter (employer) puts up `collateral_zat` a period, as does the employee.
    Payroll {
        usd_cents: u64,
        collateral_zat: u64,
        #[serde(default)]
        premium_zat: u64,
        periods: u32,
        period_blocks: u32,
        /// Blocks after a period ends before its refund.
        refund_margin_blocks: u32,
        /// 1-based; the offerer fills the series fields.
        #[serde(default = "one")]
        period: u32,
        #[serde(default)]
        series_id: Option<String>,
        #[serde(default)]
        price_minute: Option<i64>,
    },
    /// Collateral released on proof of payment: the accepter (borrower) locks
    /// `collateral_zat`; if `amount_zat` reaches the offerer's (lender's) shielded
    /// address `repay_to` by the deadline, the borrower gets it back, else the lender
    /// takes it. The oracles watch `repay_to` through its viewing key.
    PayOnPayment {
        amount_zat: u64,
        collateral_zat: u64,
        by_blocks: u32,
        refund_blocks: u32,
        /// The lender's unified address; the offerer fills it with its viewing key.
        #[serde(default)]
        repay_to: Option<String>,
        /// Raw Ironwood incoming viewing key of `repay_to` (64 bytes, hex).
        #[serde(default)]
        ivk: Option<String>,
        #[serde(default)]
        from_height: Option<u32>,
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

fn one() -> u32 {
    1
}

const DELIVERED: &str = "delivered";
/// An auction bidder's ruling when it did not win.
pub const LOST: &str = "lost";

/// An auction bidder's ruling when it won at `price_zat`.
pub fn won_at(price_zat: u64) -> String {
    format!("won at {} ZEC", paylink::zec(price_zat))
}
const NOT_DELIVERED: &str = "not delivered";

fn bad(m: impl Into<String>) -> Error {
    Error::Build(format!("template: {}", m.into()))
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
            Template::Auction { deposit_zat, prices_zat, maturity_blocks, refund_blocks, .. } => {
                if *deposit_zat == 0 || prices_zat.is_empty() || prices_zat.len() > 32 {
                    return Err(bad("an auction needs a deposit and 1 to 32 prices"));
                }
                if prices_zat.iter().any(|p| *p == 0 || p > deposit_zat) || prices_zat.windows(2).any(|w| w[0] >= w[1]) {
                    return Err(bad("auction prices must rise, each above zero and within the deposit"));
                }
                blocks(*maturity_blocks, *refund_blocks)
            }
            Template::Hedge { amount_zat, price_cents, maturity_blocks, refund_blocks, .. } => {
                if *amount_zat == 0 || *price_cents == 0 {
                    return Err(bad("amount_zat and price_cents must be positive"));
                }
                blocks(*maturity_blocks, *refund_blocks)
            }
            Template::Payroll { usd_cents, collateral_zat, periods, period, period_blocks, refund_margin_blocks, .. } => {
                if *usd_cents == 0 || *collateral_zat == 0 || *periods == 0 || *period == 0 || period > periods {
                    return Err(bad("payroll needs a salary, collateral, and 1 <= period <= periods"));
                }
                blocks(*period_blocks, period_blocks + refund_margin_blocks)
            }
            Template::PayOnPayment { amount_zat, collateral_zat, by_blocks, refund_blocks, repay_to, ivk, .. } => {
                if *amount_zat == 0 || *collateral_zat == 0 {
                    return Err(bad("amount_zat and collateral_zat must be positive"));
                }
                if let (Some(ua), Some(k)) = (repay_to, ivk) {
                    // The oracles watch `ivk`: it must be the key of the address the borrower pays.
                    let (_, addr) = paylink::decode_unified(ua).ok_or(bad("repay_to: no Ironwood receiver"))?;
                    let ivk = crate::events::parse_ivk(k).map_err(|e| bad(e.to_string()))?;
                    if ivk.address_at(0u32) != addr {
                        return Err(bad("the viewing key is not repay_to's"));
                    }
                }
                blocks(*by_blocks, *refund_blocks)
            }
            Template::Bet { outcomes, offerer_wins, stake_zat, maturity_blocks, refund_blocks, .. } => {
                if outcomes.len() < 2 || *stake_zat == 0 || offerer_wins.iter().any(|o| !outcomes.contains(o)) {
                    return Err(bad("a bet needs two or more outcomes, a stake, and winning outcomes among them"));
                }
                blocks(*maturity_blocks, *refund_blocks)
            }
        }
    }

    /// Fills what the offerer decides at offer time: the price minute (the minute that
    /// closes around maturity, at `block_secs` a block) and the payment window.
    pub fn fill(&mut self, now_unix: i64, tip: u32, block_secs: i64) {
        let minute = |blocks: u32| (now_unix + block_secs * blocks as i64) / 60 * 60;
        match self {
            Template::StablePayment { price_minute, maturity_blocks, .. } | Template::Hedge { price_minute, maturity_blocks, .. } => {
                price_minute.get_or_insert(minute(*maturity_blocks));
            }
            Template::Payroll { price_minute, period_blocks, .. } => {
                price_minute.get_or_insert(minute(*period_blocks));
            }
            Template::PayOnPayment { from_height, .. } => {
                from_height.get_or_insert(tip + 1);
            }
            _ => {}
        }
    }

    /// The same payroll's next period, to be offered when this one settles.
    pub fn next_period(&self) -> Option<Template> {
        match self {
            Template::Payroll { period, periods, .. } if period < periods => {
                let mut t = self.clone();
                if let Template::Payroll { period, price_minute, .. } = &mut t {
                    *period += 1;
                    *price_minute = None;
                }
                Some(t)
            }
            _ => None,
        }
    }

    /// The stable-value curve of the price-settled templates: (owed cents, recipient's
    /// collateral, hedger's collateral, premium).
    fn rounding_bps(&self) -> u64 {
        match self {
            Template::StablePayment { rounding_bps: Some(b), .. } => *b,
            _ => 200,
        }
    }

    fn curve(&self) -> Option<(u64, u64, u64, u64)> {
        match self {
            Template::StablePayment { usd_cents, collateral_zat, premium_zat, .. } | Template::Payroll { usd_cents, collateral_zat, premium_zat, .. } => {
                Some((*usd_cents, *collateral_zat, *collateral_zat, *premium_zat))
            }
            Template::Hedge { amount_zat, price_cents, premium_zat, .. } => {
                Some(((*amount_zat as u128 * *price_cents as u128 / 100_000_000) as u64, *amount_zat, *amount_zat, *premium_zat))
            }
            _ => None,
        }
    }

    /// What the oracles are asked to attest.
    pub fn event(&self) -> Result<EventSpec> {
        Ok(match self {
            Template::StablePayment { price_minute, .. } | Template::Hedge { price_minute, .. } | Template::Payroll { price_minute, .. } => EventSpec::ZecUsd {
                minute: price_minute.ok_or(bad("price_minute not filled"))?,
                unit_cents: 100,
                nb_digits: 14,
            },
            Template::PayOnPayment { amount_zat, by_blocks, ivk, from_height, .. } => {
                let from = from_height.ok_or(bad("from_height not filled"))?;
                EventSpec::Payment {
                    ivk: ivk.clone().ok_or(bad("ivk not filled"))?,
                    min_zat: *amount_zat,
                    from_height: from,
                    // The window closes `by_blocks` after the contract is offered.
                    by_height: from - 1 + by_blocks,
                }
            }
            Template::Escrow { question, .. } => EventSpec::Arbiter {
                question: question.clone(),
                outcomes: vec![DELIVERED.into(), NOT_DELIVERED.into()],
            },
            Template::Bet { question, outcomes, .. } => EventSpec::Arbiter { question: question.clone(), outcomes: outcomes.clone() },
            Template::Auction { item, prices_zat, .. } => EventSpec::Arbiter {
                question: item.clone(),
                outcomes: std::iter::once(LOST.to_string()).chain(prices_zat.iter().map(|p| won_at(*p))).collect(),
            },
        })
    }

    fn heights(&self) -> (u32, u32) {
        match self {
            Template::StablePayment { maturity_blocks, refund_blocks, .. }
            | Template::Hedge { maturity_blocks, refund_blocks, .. }
            | Template::Escrow { maturity_blocks, refund_blocks, .. }
            | Template::Bet { maturity_blocks, refund_blocks, .. }
            | Template::Auction { maturity_blocks, refund_blocks, .. } => (*maturity_blocks, *refund_blocks),
            Template::Payroll { period_blocks, refund_margin_blocks, .. } => (*period_blocks, period_blocks + refund_margin_blocks),
            Template::PayOnPayment { by_blocks, refund_blocks, .. } => (by_blocks + 1, *refund_blocks),
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
            Ok(CEvent::Enumerated(anns.iter().map(oracle_client::to_enumerated).collect::<Result<_>>()?))
        };
        Ok(match self {
            Template::StablePayment { .. } | Template::Hedge { .. } | Template::Payroll { .. } => {
                let (d_cents, q_recipient, q_hedger, premium) = self.curve().expect("a price-settled template");
                let event = CEvent::Numeric(anns.iter().map(oracle_client::to_numeric).collect::<Result<_>>()?);
                let curve = StableValue {
                    d_cents,
                    q_recipient,
                    q_hedger,
                    unit_cents: 100,
                    nb_digits: 14,
                    rounding: (q_recipient * self.rounding_bps() / 10_000).max(1),
                    premium,
                };
                base(event, Payouts::StableValue(curve), q_recipient + premium, q_hedger, Splits::default())
            }
            Template::PayOnPayment { collateral_zat, .. } => {
                // Paid: the borrower's collateral goes back; unpaid: to the lender.
                let payouts = Payouts::Enumerated(vec![("paid".into(), 0), ("unpaid".into(), *collateral_zat)]);
                base(enumerated()?, payouts, 0, *collateral_zat, Splits::default())
            }
            Template::Escrow { amount_zat, payees, .. } => {
                let payouts = Payouts::Enumerated(vec![(DELIVERED.into(), *amount_zat), (NOT_DELIVERED.into(), 0)]);
                base(enumerated()?, payouts, 0, *amount_zat, splits(payees)?)
            }
            Template::Auction { deposit_zat, prices_zat, .. } => {
                // The seller's payout per ruling: nothing on "lost", the price on "won at".
                let payouts = Payouts::Enumerated(std::iter::once((LOST.to_string(), 0)).chain(prices_zat.iter().map(|p| (won_at(*p), *p))).collect());
                base(enumerated()?, payouts, 0, *deposit_zat, Splits::default())
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
            (Template::StablePayment { .. } | Template::Hedge { .. } | Template::Payroll { .. }, role) => {
                let (_, qr, qh, premium) = self.curve().expect("a price-settled template");
                if role == Role::Offerer { qr + premium } else { qh }
            }
            (Template::PayOnPayment { .. }, Role::Offerer) => 0,
            (Template::PayOnPayment { collateral_zat, .. }, Role::Accepter) => *collateral_zat,
            (Template::Escrow { .. }, Role::Offerer) => 0,
            (Template::Escrow { amount_zat, .. }, Role::Accepter) => *amount_zat,
            (Template::Bet { stake_zat, .. }, _) => *stake_zat,
            (Template::Auction { .. }, Role::Offerer) => 0,
            (Template::Auction { deposit_zat, .. }, Role::Accepter) => *deposit_zat,
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
            Template::Hedge { amount_zat, price_cents, premium_zat, .. } => json!({
                "template": "hedge",
                "you": if role == Role::Offerer { "protected: owed the dollar value of the ZEC" } else { "hedger: pays the dollar value, keeps the rest, earns the premium" },
                "protects_zec": paylink::zec(*amount_zat),
                "at_usd_per_zec": format!("{}.{:02}", price_cents / 100, price_cents % 100),
                "premium_zec": paylink::zec(*premium_zat),
                "you_put_in_zec": paylink::zec(mine),
            }),
            Template::Payroll { usd_cents, collateral_zat, periods, period, period_blocks, .. } => json!({
                "template": "payroll",
                "you": if role == Role::Offerer { "employee: owed the salary in ZEC each period" } else { "employer: pays the salary, keeps the rest" },
                "usd_per_period": format!("{}.{:02}", usd_cents / 100, usd_cents % 100),
                "period": format!("{period} of {periods}"),
                "period_blocks": period_blocks,
                "collateral_zec": paylink::zec(*collateral_zat),
                "you_put_in_zec": paylink::zec(mine),
            }),
            Template::PayOnPayment { amount_zat, collateral_zat, by_blocks, repay_to, .. } => json!({
                "template": "pay_on_payment",
                "you": if role == Role::Offerer { "lender: takes the collateral if not repaid" } else { "borrower: collateral back once you repay" },
                "repay_zec": paylink::zec(*amount_zat),
                "repay_to": repay_to,
                "within_blocks": by_blocks,
                "collateral_zec": paylink::zec(*collateral_zat),
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
            Template::Auction { item, deposit_zat, prices_zat, .. } => json!({
                "template": "auction",
                "item": item,
                "you": if role == Role::Offerer { "seller: paid the clearing price if this bidder wins" } else { "bidder: locks the deposit, pays the clearing price only if it wins" },
                "deposit_zec": paylink::zec(*deposit_zat),
                "prices_zec": prices_zat.iter().map(|p| paylink::zec(*p)).collect::<Vec<_>>(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use orchard::keys::{FullViewingKey, Scope, SpendingKey};
    use zcash_protocol::consensus::NetworkType;

    fn fvk(b: u8) -> FullViewingKey {
        let sk = (0u8..=255).find_map(|i| Option::<SpendingKey>::from(SpendingKey::from_bytes([b ^ i; 32]))).unwrap();
        FullViewingKey::from(&sk)
    }

    #[test]
    fn hedge_owes_the_dollar_value_of_the_zec() {
        let h = Template::Hedge { amount_zat: 2_0000_0000, price_cents: 1_250_00, premium_zat: 1_000, maturity_blocks: 6, refund_blocks: 60, price_minute: None };
        h.validate().unwrap();
        assert_eq!(h.curve(), Some((2_500_00, 2_0000_0000, 2_0000_0000, 1_000)));
        assert_eq!(h.collateral(Role::Offerer), 2_0000_1000);
        assert_eq!(h.collateral(Role::Accepter), 2_0000_0000);
    }

    #[test]
    fn payroll_runs_its_periods() {
        let mut p = Template::Payroll { usd_cents: 1_000_00, collateral_zat: 1_0000_0000, premium_zat: 0, periods: 2, period_blocks: 30, refund_margin_blocks: 60, period: 1, series_id: Some("s".into()), price_minute: Some(60) };
        p.validate().unwrap();
        let next = p.next_period().unwrap();
        assert!(matches!(next, Template::Payroll { period: 2, price_minute: None, ref series_id, .. } if series_id.as_deref() == Some("s")));
        assert!(next.next_period().is_none(), "two periods only");
        p.fill(1_000, 10, 75);
        assert!(matches!(p, Template::Payroll { price_minute: Some(60), .. }), "a filled minute stays");
    }

    #[test]
    fn a_loan_must_watch_the_address_it_asks_to_be_repaid() {
        let (lender, other) = (fvk(1), fvk(2));
        let ua = paylink::unified_address(&lender.address_at(0u32, Scope::External), NetworkType::Test);
        let ivk = |f: &FullViewingKey| Some(hex::encode(f.to_ivk(Scope::External).to_bytes()));
        let loan = |k| Template::PayOnPayment { amount_zat: 1, collateral_zat: 2, by_blocks: 10, refund_blocks: 100, repay_to: Some(ua.clone()), ivk: k, from_height: Some(5) };
        loan(ivk(&lender)).validate().unwrap();
        assert!(loan(ivk(&other)).validate().is_err(), "a key that is not repay_to's");
        assert!(matches!(loan(ivk(&lender)).event().unwrap(), EventSpec::Payment { by_height: 14, from_height: 5, min_zat: 1, .. }));
    }

    #[test]
    fn bad_templates_are_refused() {
        assert!(Template::Bet { question: "q".into(), outcomes: vec!["a".into()], offerer_wins: vec![], stake_zat: 1, maturity_blocks: 1, refund_blocks: 2 }.validate().is_err());
        assert!(Template::Escrow { amount_zat: 1, question: "q".into(), payees: vec![], maturity_blocks: 5, refund_blocks: 5 }.validate().is_err());
    }

    #[test]
    fn auction_pays_the_seller_the_clearing_price_only() {
        let lot = |prices: Vec<u64>| Template::Auction { item: "lot".into(), deposit_zat: 1000, prices_zat: prices, maturity_blocks: 5, refund_blocks: 50 };
        assert!(lot(vec![]).validate().is_err(), "no prices");
        assert!(lot(vec![500, 400]).validate().is_err(), "falling prices");
        assert!(lot(vec![500, 1500]).validate().is_err(), "a price above the deposit");
        let ok = lot(vec![250, 500, 1000]);
        ok.validate().unwrap();
        let EventSpec::Arbiter { outcomes, .. } = ok.event().unwrap() else { panic!("an arbiter event") };
        assert_eq!(outcomes, vec![LOST.to_string(), won_at(250), won_at(500), won_at(1000)]);
        assert_eq!((ok.collateral(Role::Offerer), ok.collateral(Role::Accepter)), (0, 1000), "only the bidder locks");
    }
}
