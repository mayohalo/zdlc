//! The sealed-bid auction (`/auction`): a seller, four bidders, the server as auctioneer.
//! See `README.md` in this folder.

#![allow(unused_imports)]

use std::{collections::HashMap, io::Write, os::unix::fs::OpenOptionsExt, sync::Arc};

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use zdlc_kit::{impls::*, templates::Template, Funding, Zdlc};

use crate::{common::*, escrow::*, hedge::*};

pub(crate) const AUCTION_PAGE: &str = include_str!("page.html");

/// The auction demo: a seller node offers one contract per bidder, a bidders' node plays
/// the three house bidders and the visitor, and this server is the auctioneer: it holds
/// the sealed bids and has the two oracles rule each contract. One round at a time.
pub(crate) struct AuctionDemo {
    pub(crate) seller: Arc<Zdlc>,
    pub(crate) bidders: Arc<Zdlc>,
    pub(crate) round: std::sync::Mutex<Option<Round>>,
    /// Past clearing prices: what the public sees of each round.
    pub(crate) tape: std::sync::Mutex<Vec<Value>>,
    pub(crate) dir: String,
}

pub(crate) const LOT: &str = "One hour of GPU time";

pub(crate) const BIDDERS: [&str; 4] = ["Bidder A", "Bidder B", "Bidder C", "You"];

/// Every bidder locks the same deposit; prices run in ten steps up to it.
pub(crate) const AUCTION_DEPOSIT: u64 = 200_000;

pub(crate) fn auction_prices() -> Vec<u64> {
    (1..=10).map(|i| i * AUCTION_DEPOSIT / 10).collect()
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Round {
    pub(crate) id: String,
    pub(crate) started: u64,
    /// Each bidder's sealed bid (zat) and contract, in BIDDERS order.
    pub(crate) bids: Vec<u64>,
    pub(crate) contracts: Vec<Option<String>>,
    /// Set once cleared: the winner (index into BIDDERS) and the clearing price.
    pub(crate) winner: Option<usize>,
    pub(crate) price: Option<u64>,
    pub(crate) steps: Vec<Value>,
    pub(crate) error: Option<String>,
    pub(crate) done: bool,
}

impl AuctionDemo {
    pub(crate) fn save(&self) {
        let r = self.round.lock().unwrap().clone();
        let t = self.tape.lock().unwrap().clone();
        let _ = std::fs::write(format!("{}/auction.json", self.dir), serde_json::to_vec(&json!({"round": r, "tape": t})).unwrap_or_default());
    }

    pub(crate) fn step(&self, who: &str, what: impl Into<String>) {
        if let Some(r) = self.round.lock().unwrap().as_mut() {
            r.steps.push(json!({"t": now(), "who": who, "what": what.into()}));
        }
        self.save();
    }
}

pub(crate) async fn auction_page() -> Html<&'static str> {
    Html(AUCTION_PAGE)
}

pub(crate) async fn auction_info(Ready(a): Ready) -> Response {
    let d = &a.auction;
    let tip = a.contractor.tip().await.ok();
    // The explorer link is ours to show; everything else goes through the scrub.
    let mut v = json!({
        "network": a.cfg.label, "tip": tip, "blockSecs": a.block_secs().await.round(),
        "lot": LOT, "bidders": BIDDERS, "depositZat": AUCTION_DEPOSIT, "pricesZat": auction_prices(),
        "biddersZat": d.bidders.treasury_zat().await.ok(), "tape": d.tape.lock().unwrap().clone(),
    });
    scrub_json(&mut v);
    v["explorer"] = json!(a.cfg.explorer);
    Json(v).into_response()
}

/// The round and each contract as the bidders' node sees it.
pub(crate) async fn auction_state(Ready(a): Ready) -> Response {
    let d = &a.auction;
    let round = d.round.lock().unwrap().clone();
    let mut views = vec![];
    if let Some(r) = &round {
        for c in &r.contracts {
            let v = match c {
                Some(id) => d.bidders.contract(id).await.ok().flatten().map(|r| r.view()).unwrap_or_default(),
                None => Value::Null,
            };
            views.push(v);
        }
    }
    let tip = a.contractor.tip().await.ok();
    out(json!({"round": round, "contracts": views, "tip": tip, "tape": d.tape.lock().unwrap().clone()}))
}

#[derive(Deserialize)]
pub(crate) struct BidReq {
    /// The visitor's bid, in zat: one of the auction's prices.
    pub(crate) bid_zat: u64,
}

/// The visitor bids: a new round opens with three house bids beside it.
pub(crate) async fn auction_bid(Ready(a): Ready, Json(r): Json<BidReq>) -> Response {
    let prices = auction_prices();
    if !prices.contains(&r.bid_zat) {
        return bad("bid one of the auction's prices");
    }
    let d = a.auction.clone();
    {
        let mut g = d.round.lock().unwrap();
        if g.as_ref().is_some_and(|r| !r.done) {
            return err(StatusCode::CONFLICT, "a round is already running: watch it settle, then bid in the next");
        }
        // House bids: anywhere on the ladder, so the visitor sometimes wins.
        let mut house = [0u8; 3];
        rand10::fill(&mut house);
        let mut bids: Vec<u64> = house.iter().map(|b| prices[*b as usize % prices.len()]).collect();
        bids.push(r.bid_zat);
        let mut id = [0u8; 4];
        rand10::fill(&mut id);
        *g = Some(Round { id: hex::encode(id), started: now(), contracts: vec![None; BIDDERS.len()], bids, ..Default::default() });
    }
    d.save();
    let app = a.clone();
    tokio::spawn(async move {
        let r = run_round(&app).await;
        let d = &app.auction;
        if let Err(e) = r {
            if let Some(r) = d.round.lock().unwrap().as_mut() {
                r.error = Some(e);
            }
        }
        if let Some(r) = d.round.lock().unwrap().as_mut() {
            r.done = true;
        }
        d.save();
    });
    out(json!({"started": true}))
}

/// One round: offer each bidder's contract, wait until all are signed, clear the
/// auction (second price), have both oracles rule each contract, wait for the closes.
pub(crate) async fn run_round(a: &App) -> std::result::Result<(), String> {
    let d = &a.auction;
    let bids = d.round.lock().unwrap().as_ref().map(|r| r.bids.clone()).unwrap_or_default();
    d.step("auctioneer", format!("received {} sealed bids", bids.len()));
    let maturity = a.blocks(2 * a.cfg.block_secs, 2).await;
    let refund = a.blocks(8 * a.cfg.block_secs, maturity + 2).await;
    let lot = Template::Auction { item: LOT.into(), deposit_zat: AUCTION_DEPOSIT, prices_zat: auction_prices(), maturity_blocks: maturity, refund_blocks: refund };
    for (i, name) in BIDDERS.iter().enumerate() {
        let id = d.seller.offer(lot.clone(), &d.bidders.identity()).await.map_err(|e| e.to_string())?;
        if let Some(r) = d.round.lock().unwrap().as_mut() {
            r.contracts[i] = Some(id);
        }
        d.step("seller", format!("offered {name} a contract: a {} ZEC deposit", zdlc::paylink::zec(AUCTION_DEPOSIT)));
    }
    let ids: Vec<String> = d.round.lock().unwrap().as_ref().map(|r| r.contracts.iter().flatten().cloned().collect()).unwrap_or_default();
    // Every bidder must be locked in before the result exists.
    let deadline = now() + 180;
    loop {
        let mut signed = 0;
        for id in &ids {
            if let Some(r) = d.bidders.contract(id).await.ok().flatten() {
                match r.state {
                    zdlc_kit::State::Signed | zdlc_kit::State::Funded | zdlc_kit::State::Closing | zdlc_kit::State::Settled => signed += 1,
                    zdlc_kit::State::Failed | zdlc_kit::State::Declined => return Err(format!("a bidder's contract stopped: {}", r.note.unwrap_or_default())),
                    _ => {}
                }
            }
        }
        if signed == ids.len() {
            break;
        }
        if now() > deadline {
            return Err("the bidders' contracts were not all signed in time".into());
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    d.step("bidders", "all four deposits locked; every possible payout pre-signed");
    // Second price: the highest bid wins (the earliest on a tie) and pays the next.
    let mut order: Vec<usize> = (0..bids.len()).collect();
    order.sort_by(|x, y| bids[*y].cmp(&bids[*x]).then(x.cmp(y)));
    let (winner, price) = (order[0], bids[order[1]]);
    if let Some(r) = d.round.lock().unwrap().as_mut() {
        r.winner = Some(winner);
        r.price = Some(price);
    }
    d.step("auctioneer", format!("cleared at {} ZEC (second price)", zdlc::paylink::zec(price)));
    for (i, id) in ids.iter().enumerate() {
        let rec = d.seller.contract(id).await.ok().flatten().ok_or("a contract went missing")?;
        let outcome = if i == winner { zdlc::templates::won_at(price) } else { zdlc::templates::LOST.to_string() };
        for url in &a.oracles {
            let res = a.http.post(format!("{}/events/{}/resolve", url.trim_end_matches('/'), rec.event_id)).json(&json!({"outcome": outcome, "token": a.oracle_token})).send().await;
            match res {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => return Err(format!("an oracle refused: {}", r.text().await.unwrap_or_default())),
                Err(e) => return Err(format!("oracle: {}", e.without_url())),
            }
        }
    }
    d.step("oracles", "ruled every contract: one \u{201c}won at\u{201d}, three \u{201c}lost\u{201d}");
    {
        let mut t = d.tape.lock().unwrap();
        t.insert(0, json!({"t": now(), "lot": LOT, "price_zat": price, "bidders": bids.len()}));
        t.truncate(20);
    }
    let deadline = now() + 900;
    loop {
        let mut settled = 0;
        for id in &ids {
            if d.bidders.contract(id).await.ok().flatten().is_some_and(|r| r.state == zdlc_kit::State::Settled) {
                settled += 1;
            }
        }
        if settled == ids.len() {
            break;
        }
        if now() > deadline {
            return Err("not every contract settled in time".into());
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    d.step("chain", "all four contracts closed cooperatively into shielded notes");
    Ok(())
}

/// The auction's two nodes: the seller and the bidders. The bidders' node accepts every
/// auction offer from the treasury; the demo wallet keeps it funded.
pub(crate) async fn auction_demo(args: &[String], dir: &str, rpc: &str, cfg: &Cfg, bank: Arc<Zdlc>) -> Arc<AuctionDemo> {
    let (seller, bidders) = loop {
        match (node(args, dir, "seller", rpc, cfg, false, true).await, node(args, dir, "bidders", rpc, cfg, false, true).await) {
            (Ok(s), Ok(b)) => break (s, b),
            (Err(e), _) | (_, Err(e)) => eprintln!("zdlc-room: auction nodes waiting for the node: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    };
    bidders.set_policy(Arc::new(|rec: &zdlc_kit::Record| matches!(rec.template, Template::Auction { .. }).then_some(Funding::Treasury)));
    for z in [&seller, &bidders] {
        tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
    }
    spawn_sweeper(bidders.clone(), format!("{dir}/bidders-inbox.from"));
    let saved: Value = std::fs::read(format!("{dir}/auction.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut round: Option<Round> = serde_json::from_value(saved["round"].clone()).ok().flatten();
    if let Some(r) = round.as_mut() {
        if !r.done {
            r.done = true;
            r.error = Some("the server restarted during this round".into());
        }
    }
    let d = Arc::new(AuctionDemo {
        seller,
        bidders,
        round: std::sync::Mutex::new(round),
        tape: std::sync::Mutex::new(serde_json::from_value(saved["tape"].clone()).unwrap_or_default()),
        dir: dir.to_string(),
    });
    // Four deposits plus each one's fee cushion a round: top up when that runs low.
    {
        let (d, low, top) = (d.clone(), 4 * (AUCTION_DEPOSIT + zdlc_kit::CUSHION), 5_000_000u64);
        let path = format!("{dir}/topups.json");
        tokio::spawn(async move {
            loop {
                let mut last: HashMap<String, u64> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
                let z = &d.bidders;
                let recent = last.get(&z.identity()).is_some_and(|t| now() < t + 1800);
                if !recent && z.treasury_zat().await.is_ok_and(|v| v < low) {
                    match bank.pay_from_treasury(&z.inbox_address(), top).await {
                        Ok(txid) => {
                            println!("zdlc-room: topped up the auction bidders from the demo wallet: {txid}");
                            last.insert(z.identity(), now());
                            let _ = std::fs::write(&path, serde_json::to_vec(&last).unwrap_or_default());
                        }
                        Err(e) => eprintln!("zdlc-room: auction top-up: {e}"),
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            }
        });
    }
    d
}
