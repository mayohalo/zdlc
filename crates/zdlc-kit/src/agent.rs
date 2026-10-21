//! Desk agents on the kit: a maker that quotes requests for a ZEC/USD hedge and accepts
//! the offers that match its quotes, and a taker that asks for quotes, offers on the
//! cheapest and follows the contract to settlement. The desk is only the quote board;
//! everything after the quote is the kit (offer, accept, funding, close, CET, refund).

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{json, Value};
use zdlc::desk::{self, DeskClient, QuoteBody, RfqBody, Signed};

use crate::{templates::Template, Event, Funding, Result, State, Zdlc};

/// A quote the maker stands behind: whoever asked may offer on these terms.
#[derive(Clone, Debug)]
struct Standing {
    collateral: u64,
    premium: u64,
    maturity_blocks: u32,
    refund_blocks: u32,
    valid_until: u32,
}

pub struct MakerArgs {
    pub premium_bps: u64,
    pub cap_zat: u64,
    pub ttl_blocks: u32,
    /// Stop after this many contracts settle (0: run forever).
    pub contracts: usize,
}

/// Quotes every acceptable request and accepts the matching offers from treasury.
pub async fn run_maker(z: Arc<Zdlc>, desk: &DeskClient, id_key: &secp256k1::SecretKey, oracle_keys: Vec<String>, a: &MakerArgs) -> Result<Vec<Value>> {
    let standing: Arc<Mutex<HashMap<String, Standing>>> = Arc::default();
    let s2 = standing.clone();
    z.set_policy(Arc::new(move |rec| {
        let Template::StablePayment { collateral_zat, premium_zat, maturity_blocks, refund_blocks, .. } = &rec.template else { return None };
        let mut book = s2.lock().unwrap();
        let q = book.get(&rec.counterparty)?;
        let ok = *collateral_zat == q.collateral && *premium_zat >= q.premium && *maturity_blocks == q.maturity_blocks && *refund_blocks == q.refund_blocks;
        if ok {
            book.remove(&rec.counterparty);
            Some(Funding::Treasury)
        } else {
            None
        }
    }));
    let mut seen = std::collections::BTreeSet::new();
    let mut log = vec![];
    let mut cursor = 0;
    let mut settled = 0;
    loop {
        let tip = z.info().await?["tip"].as_u64().unwrap_or_default() as u32;
        for (rfq_id, rfq) in desk.rfqs().await? {
            if !seen.insert(rfq_id.clone()) {
                continue;
            }
            let ok = rfq.verify(desk::RFQ_TAG).is_ok()
                && rfq.body.oracle_keys == oracle_keys
                && rfq.body.q_recipient <= a.cap_zat
                && rfq.body.nb_digits == 14
                && rfq.body.unit_cents == 100;
            if !ok {
                log.push(json!({"step": "rfq declined", "rfq": rfq_id}));
                continue;
            }
            let premium = rfq.body.q_recipient * a.premium_bps / 10_000;
            let quote = QuoteBody {
                rfq_id: rfq_id.clone(),
                q_hedger: rfq.body.q_recipient,
                premium,
                rounding: (2 * rfq.body.q_recipient / 100).max(1),
                inputs: vec![],
                valid_until: tip + a.ttl_blocks,
                party: None,
            };
            let qid = desk.post_quote(&Signed::new(desk::QUOTE_TAG, quote.clone(), id_key), false).await?;
            standing.lock().unwrap().insert(
                rfq.from.clone(),
                Standing { collateral: rfq.body.q_recipient, premium, maturity_blocks: rfq.body.maturity_blocks, refund_blocks: rfq.body.refund_blocks, valid_until: quote.valid_until },
            );
            println!("[maker] quoted {qid}: premium {premium} zat for {} zat", rfq.body.q_recipient);
            log.push(json!({"step": "quoted", "rfq": rfq_id, "quote": qid, "premium": premium}));
        }
        standing.lock().unwrap().retain(|_, q| q.valid_until > tip);
        for (seq, ev) in z.events_after(cursor, 100).await? {
            cursor = seq;
            println!("[maker] {}", serde_json::to_string(&ev).unwrap_or_default());
            log.push(json!({"step": "event", "event": ev}));
            if matches!(ev, Event::Settled { .. }) {
                settled += 1;
            }
        }
        if a.contracts > 0 && settled >= a.contracts {
            return Ok(log);
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}

pub struct TakerArgs {
    pub collateral_zat: u64,
    pub maturity_blocks: u32,
    pub refund_blocks: u32,
    /// Minutes from now of the ZEC/USD minute (negative: an already closed minute).
    pub minute_offset: i64,
    pub quote_wait_s: u64,
}

/// Asks the desk for a hedge on `collateral_zat`, offers on the cheapest quote and
/// returns once the contract settles.
pub async fn run_taker(z: Arc<Zdlc>, desk: &DeskClient, id_key: &secp256k1::SecretKey, oracle_keys: Vec<String>, a: &TakerArgs) -> Result<Vec<Value>> {
    let mut log = vec![];
    let mut nonce = [0u8; 8];
    rand10::fill(&mut nonce);
    let rfq = RfqBody {
        q_recipient: a.collateral_zat,
        maturity_blocks: a.maturity_blocks,
        refund_blocks: a.refund_blocks,
        oracle_keys,
        unit_cents: 100,
        nb_digits: 14,
        nonce: hex::encode(nonce),
    };
    let rfq_id = desk.post_rfq(&Signed::new(desk::RFQ_TAG, rfq, id_key)).await?;
    println!("[taker] rfq {rfq_id}");
    log.push(json!({"step": "rfq posted", "rfq": rfq_id}));
    tokio::time::sleep(Duration::from_secs(a.quote_wait_s)).await;
    let tip = z.info().await?["tip"].as_u64().unwrap_or_default() as u32;
    let best = desk
        .quotes(&rfq_id)
        .await?
        .into_iter()
        .filter(|(_, q)| q.verify(desk::QUOTE_TAG).is_ok() && q.body.q_hedger == a.collateral_zat && q.body.valid_until > tip + 2)
        .min_by_key(|(_, q)| q.body.premium)
        .ok_or(crate::Error::Usage("no usable quote".into()))?;
    let (qid, quote) = best;
    // Owed today's dollar value of the collateral: a pure hedge.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let (spot, _) = zdlc::feeds::zecusd_minute((now / 60 - 2) * 60).await?;
    let template = Template::StablePayment {
        usd_cents: (spot as u128 * a.collateral_zat as u128 / 100_000_000) as u64,
        collateral_zat: a.collateral_zat,
        premium_zat: quote.body.premium,
        maturity_blocks: a.maturity_blocks,
        refund_blocks: a.refund_blocks,
        price_minute: Some((now / 60 + a.minute_offset) * 60),
    };
    let id = z.offer(template, &quote.from).await?;
    println!("[taker] offered {id} on quote {qid} (premium {} zat, spot {spot} cents)", quote.body.premium);
    log.push(json!({"step": "offered", "quote": qid, "contract": id, "premium": quote.body.premium, "spot_cents": spot}));
    let mut cursor = 0;
    loop {
        for (seq, ev) in z.events_after(cursor, 100).await? {
            cursor = seq;
            println!("[taker] {}", serde_json::to_string(&ev).unwrap_or_default());
            log.push(json!({"step": "event", "event": ev}));
        }
        if let Some(rec) = z.contract(&id).await? {
            match rec.state {
                State::Settled => return Ok(log),
                State::Failed | State::Declined => return Err(crate::Error::Usage(format!("contract {id} {:?}: {:?}", rec.state, rec.note))),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(1000)).await;
    }
}
