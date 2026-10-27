//! The hedge desk (`/hedge`): a house maker agent quoting on the RFQ desk and a taker
//! the visitor starts. See `demos/hedge-desk/README.md`.

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

use crate::{auction::*, common::*, escrow::*};

pub(crate) const DESK_PAGE: &str = include_str!("pages/hedge.html");

/// The desk demo: a house maker agent always quoting on the desk, and a taker the
/// visitor drives. One trade runs at a time, so every visitor watches the same one.
pub(crate) struct DeskDemo {
    pub(crate) maker: Arc<Zdlc>,
    pub(crate) taker: Arc<Zdlc>,
    pub(crate) taker_key: secp256k1::SecretKey,
    pub(crate) desk: zdlc::desk::DeskClient,
    pub(crate) oracle_keys: Vec<String>,
    pub(crate) trade: std::sync::Mutex<Option<Trade>>,
    pub(crate) file: String,
    pub(crate) args: DeskArgs,
    /// The last ZEC/USD spot fetched for the page's example, in cents, and when.
    pub(crate) spot: std::sync::Mutex<(u64, u64)>,
}

pub(crate) struct DeskArgs {
    pub(crate) collateral_zat: u64,
    pub(crate) premium_bps: u64,
    pub(crate) maturity: u32,
    pub(crate) refund: u32,
    pub(crate) minute_offset: i64,
}

/// The current (or last) trade, as the taker reports it. Saved to `trade.json`.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Trade {
    pub(crate) started: u64,
    pub(crate) steps: Vec<Value>,
    pub(crate) contract: Option<String>,
    pub(crate) rfq: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) done: bool,
}

impl DeskDemo {
    pub(crate) fn save(&self, t: &Trade) {
        if let Ok(b) = serde_json::to_vec(t) {
            let _ = std::fs::write(&self.file, b);
        }
    }

    pub(crate) fn record(&self, step: Value) {
        let mut g = self.trade.lock().unwrap();
        if let Some(t) = g.as_mut() {
            if step["step"] == "rfq posted" {
                t.rfq = step["rfq"].as_str().map(String::from);
            }
            if step["step"] == "offered" {
                t.contract = step["contract"].as_str().map(String::from);
            }
            t.steps.push(json!({"t": now(), "step": step}));
            self.save(t);
        }
    }
}

pub(crate) async fn desk_page() -> Html<&'static str> {
    Html(DESK_PAGE)
}

pub(crate) async fn desk_info(Ready(a): Ready) -> Response {
    let d = &a.desk;
    let tip = match d.taker.tip().await {
        Ok(t) => t,
        Err(e) => return bad(e),
    };
    Json(json!({
        "network": a.cfg.label, "tip": tip, "blockSecs": a.block_secs().await.round(), "explorer": a.cfg.explorer,
        "collateralZat": d.args.collateral_zat, "premiumBps": d.args.premium_bps, "refundBlocks": d.args.refund,
        "maker": d.maker.identity(), "taker": d.taker.identity(), "oracles": d.oracle_keys.len(),
        "makerZat": d.maker.treasury_zat().await.ok(), "takerZat": d.taker.treasury_zat().await.ok(),
        "makerFund": d.maker.inbox_address(), "takerFund": d.taker.inbox_address(),
        "spotCents": desk_spot(d).await,
    }))
    .into_response()
}

/// A recent ZEC/USD price for the page's worked example: the median of the exchanges
/// for a closed minute, fetched at most every two minutes.
pub(crate) async fn desk_spot(d: &DeskDemo) -> Option<u64> {
    let (at, cents) = *d.spot.lock().unwrap();
    if now() < at + 120 && cents > 0 {
        return Some(cents);
    }
    let minute = (now() as i64 / 60 - 2) * 60;
    let (cents, _) = zdlc::feeds::zecusd_minute(minute).await.ok()?;
    *d.spot.lock().unwrap() = (now(), cents);
    Some(cents)
}

/// The trade, both sides' view of its contract, and the desk's quotes on its request.
pub(crate) async fn desk_state(Ready(a): Ready) -> Response {
    let d = &a.desk;
    let trade = d.trade.lock().unwrap().clone();
    let Some(t) = trade else { return out(json!({"trade": null})) };
    let (mut taker, mut maker) = (Value::Null, Value::Null);
    if let Some(id) = &t.contract {
        taker = d.taker.contract(id).await.ok().flatten().map(|r| r.view()).unwrap_or_default();
        maker = d.maker.contract(id).await.ok().flatten().map(|r| r.view()).unwrap_or_default();
    }
    let tip = d.taker.tip().await.ok();
    out(json!({"trade": t, "taker": taker, "maker": maker, "tip": tip}))
}

/// The visitor's taker asks the desk for a hedge.
pub(crate) async fn desk_trade(Ready(a): Ready) -> Response {
    let d = a.desk.clone();
    let maturity = a.blocks(d.args.maturity as i64 * a.cfg.block_secs, 2).await;
    let refund = a.blocks(d.args.refund as i64 * a.cfg.block_secs, maturity + 2).await;
    {
        let mut g = d.trade.lock().unwrap();
        if g.as_ref().is_some_and(|t| !t.done) {
            return err(StatusCode::CONFLICT, "a trade is already running: watch it settle, then start the next");
        }
        let t = Trade { started: now(), ..Default::default() };
        d.save(&t);
        *g = Some(t);
    }
    tokio::spawn(async move {
        let args = zdlc_kit::agent::TakerArgs {
            collateral_zat: d.args.collateral_zat,
            maturity_blocks: maturity,
            refund_blocks: refund,
            minute_offset: d.args.minute_offset,
            quote_wait_s: 5,
            // 5% payout rounding: about half the CETs of the default 2% to sign.
            rounding_bps: Some(500),
        };
        let d2 = d.clone();
        let r = zdlc_kit::agent::run_taker_with(d.taker.clone(), &d.desk, &d.taker_key, d.oracle_keys.clone(), &args, &move |step| d2.record(step)).await;
        let mut g = d.trade.lock().unwrap();
        if let Some(t) = g.as_mut() {
            t.done = true;
            t.error = r.err().map(|e| e.to_string());
            d.save(t);
        }
    });
    out(json!({"started": true}))
}

/// The desk demo's two agents, each a kit node with its own seed and funding inbox. The
/// maker quotes and accepts on its own, forever; the taker waits for the visitor.
/// They post no recovery records: each would prove a shielded memo (seconds of CPU) at
/// every signing, and their seeds live on this server anyway.
pub(crate) async fn desk_demo(args: &[String], dir: &str, rpc: &str, cfg: &Cfg, oracles: &[String], bank: Arc<Zdlc>) -> Arc<DeskDemo> {
    let (maker, taker) = loop {
        match (node(args, dir, "maker", rpc, cfg, false, true).await, node(args, dir, "taker", rpc, cfg, false, true).await) {
            (Ok(m), Ok(t)) => break (m, t),
            (Err(e), _) | (_, Err(e)) => eprintln!("zdlc-room: desk agents waiting for the node: {e}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    };
    for z in [&maker, &taker] {
        tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
    }
    let mut oracle_keys = vec![];
    for o in oracles {
        let k = loop {
            match zdlc::oracle_client::OracleClient::new(o).info().await {
                Ok(i) => break i["pubkey"].as_str().unwrap_or_default().to_string(),
                Err(e) => eprintln!("zdlc-room: oracle: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        };
        oracle_keys.push(k);
    }
    let desk_url = arg(args, "--desk", "");
    let num = |name: &str, default: &str| arg(args, name, default).parse::<i64>().unwrap_or_else(|_| die(name));
    let d = Arc::new(DeskDemo {
        taker_key: zdlc::wallet::Wallet::from_seed(secret(&format!("{dir}/taker.seed"))).identity_key(),
        desk: zdlc::desk::DeskClient::new(&desk_url),
        trade: std::sync::Mutex::new(std::fs::read(format!("{dir}/trade.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())),
        file: format!("{dir}/trade.json"),
        args: DeskArgs {
            collateral_zat: zat(&arg(args, "--desk-collateral", "0.001")),
            premium_bps: num("--desk-premium-bps", "50") as u64,
            maturity: num("--desk-maturity", "2") as u32,
            refund: num("--desk-refund", "8") as u32,
            // The minute in progress: it closes while the funding is being mined, so the
            // price is attested by the time the contract is funded.
            minute_offset: num("--desk-minute-offset", "0"),
        },
        oracle_keys,
        maker,
        taker,
        spot: std::sync::Mutex::new((0, 0)),
    });
    // A trade cut short by a restart is over: its contract still settles on its own.
    if let Some(t) = d.trade.lock().unwrap().as_mut() {
        if !t.done {
            t.done = true;
            t.error = Some("the server restarted during this trade".into());
            d.save(t);
        }
    }
    for (z, name) in [(&d.maker, "maker"), (&d.taker, "taker")] {
        spawn_sweeper(z.clone(), format!("{dir}/{name}-inbox.from"));
    }
    // The demo wallet (`bank`, the escrow client's treasury) keeps both agents funded: when
    // one runs low, it pays the agent's inbox, which the sweeper turns into coins.
    {
        let (d, low, top) = (d.clone(), 4 * d.args.collateral_zat + 1_000_000, 2_000_000u64);
        // When each agent was last topped up: a top-up takes a block to arrive and a
        // minute to be swept, so a restart must not send a second one meanwhile.
        let path = format!("{dir}/topups.json");
        tokio::spawn(async move {
            loop {
                let mut last: HashMap<String, u64> = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
                for z in [&d.maker, &d.taker] {
                    let recent = last.get(&z.identity()).is_some_and(|t| now() < t + 1800);
                    if !recent && z.treasury_zat().await.is_ok_and(|v| v < low) {
                        match bank.pay_from_treasury(&z.inbox_address(), top).await {
                            Ok(txid) => {
                                println!("zdlc-room: topped up a desk agent from the demo wallet: {txid}");
                                last.insert(z.identity(), now());
                                let _ = std::fs::write(&path, serde_json::to_vec(&last).unwrap_or_default());
                            }
                            Err(e) => eprintln!("zdlc-room: desk top-up: {e}"),
                        }
                    }
                }
                // Long enough for a top-up to confirm and be swept before the next check.
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            }
        });
    }
    // The house maker quotes every request on the desk and accepts the matching offers.
    {
        let (z, desk, key, keys, bps) = (d.maker.clone(), zdlc::desk::DeskClient::new(&desk_url), zdlc::wallet::Wallet::from_seed(secret(&format!("{dir}/maker.seed"))).identity_key(), d.oracle_keys.clone(), d.args.premium_bps);
        tokio::spawn(async move {
            let a = zdlc_kit::agent::MakerArgs { premium_bps: bps, cap_zat: 10_000_000, ttl_blocks: 20, contracts: 0 };
            loop {
                if let Err(e) = zdlc_kit::agent::run_maker(z.clone(), &desk, &key, keys.clone(), &a).await {
                    eprintln!("zdlc-room: maker: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            }
        });
    }
    d
}
