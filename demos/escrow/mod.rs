//! The escrow room (`/escrow`): a contractor and a client, the visitor playing both and
//! both arbiters. See `README.md` in this folder.

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

use crate::{auction::*, common::*, hedge::*};

pub(crate) const PAGE: &str = include_str!("page.html");

/// What the page did in a room, beyond what the nodes record. Saved to `rooms.json`.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Room {
    /// Each arbiter's ruling, once given.
    pub(crate) arbiters: Vec<Option<String>>,
    pub(crate) log: Vec<Value>,
}

impl App {
    pub(crate) fn side(&self, who: &str) -> Option<&Arc<Zdlc>> {
        match who {
            "contractor" => Some(&self.contractor),
            "client" => Some(&self.client),
            _ => None,
        }
    }

    pub(crate) async fn log(&self, id: &str, who: &str, what: impl Into<Value>) {
        let mut rooms = self.rooms.lock().await;
        if let Some(r) = rooms.get_mut(id) {
            r.log.push(json!({"t": now(), "who": who, "what": what.into()}));
        }
        self.save(&rooms);
    }

    pub(crate) fn save(&self, rooms: &HashMap<String, Room>) {
        let tmp = format!("{}.tmp", self.rooms_file);
        let ok = serde_json::to_vec(rooms).ok().and_then(|b| std::fs::write(&tmp, b).ok()).and_then(|_| std::fs::rename(&tmp, &self.rooms_file).ok());
        if ok.is_none() {
            eprintln!("zdlc-room: could not save {}", self.rooms_file);
        }
    }

    /// The room must be one this server opened (the contractor's offer).
    pub(crate) async fn known(&self, id: &str) -> Result<(), Response> {
        match self.contractor.contract(id).await {
            Ok(Some(_)) => {
                self.rooms.lock().await.entry(id.to_string()).or_insert_with(|| Room { arbiters: vec![None; self.oracles.len()], log: vec![] });
                Ok(())
            }
            Ok(None) => Err(err(StatusCode::NOT_FOUND, format!("no room {id}"))),
            Err(e) => Err(bad(e)),
        }
    }
}

pub(crate) async fn page() -> Html<&'static str> {
    Html(PAGE)
}

pub(crate) async fn info(Ready(a): Ready) -> Response {
    let tip = match a.contractor.tip().await {
        Ok(t) => t,
        Err(e) => return bad(e),
    };
    let bal = a.client.treasury_zat().await.ok();
    Json(json!({
        "network": a.cfg.label, "tip": tip, "amountZat": a.cfg.amount_zat, "maturityBlocks": a.cfg.maturity,
        "refundBlocks": a.blocks(a.cfg.refund as i64 * a.cfg.block_secs, 4).await, "closeWaitBlocks": a.cfg.close_wait.0, "closeWaitSecs": a.cfg.close_wait.1, "blockSecs": a.block_secs().await.round(), "explorer": a.cfg.explorer, "arbiters": a.oracles.len(),
        "demoWalletZat": bal, "fundAddress": a.client.inbox_address(),
    }))
    .into_response()
}

#[derive(Deserialize)]
pub(crate) struct OpenReq {
    #[serde(default)]
    pub(crate) question: Option<String>,
    /// A team escrow: the contractor's payout split 50/30/20 between three payees (this
    /// server's agent wallets, transparent so a CET shows the split). Not on the page.
    #[serde(default)]
    pub(crate) team: bool,
}

/// The contractor invoices the client: a new escrow, a new room.
pub(crate) async fn open(Ready(a): Ready, Json(r): Json<OpenReq>) -> Response {
    let open = match a.contractor.contracts().await {
        Ok(v) => v.iter().filter(|c| !matches!(c.state, zdlc_kit::State::Settled | zdlc_kit::State::Declined | zdlc_kit::State::Failed)).count(),
        Err(e) => return bad(e),
    };
    if open >= a.cfg.max_rooms {
        return err(StatusCode::TOO_MANY_REQUESTS, format!("{open} rooms are open; try again when one settles"));
    }
    let question: String = r.question.unwrap_or_else(|| "Milestone 1 delivered?".into()).chars().take(80).collect();
    // The windows are meant in time (the flags count blocks at --block-secs); size them
    // in blocks at the rate the chain is running now.
    let maturity = a.blocks(a.cfg.maturity as i64 * a.cfg.block_secs, 2).await;
    let refund = a.blocks(a.cfg.refund as i64 * a.cfg.block_secs, maturity + 2).await;
    let payees = if r.team {
        vec![
            zdlc::templates::PayeeSpec { label: "Ana".into(), address: a.desk.maker.treasury_address(), shielded: None, bps: 5000 },
            zdlc::templates::PayeeSpec { label: "Ben".into(), address: a.desk.taker.treasury_address(), shielded: None, bps: 3000 },
            zdlc::templates::PayeeSpec { label: "Chen".into(), address: a.auction.bidders.treasury_address(), shielded: None, bps: 2000 },
        ]
    } else {
        vec![]
    };
    let t = Template::Escrow { amount_zat: a.cfg.amount_zat, question: question.clone(), payees, maturity_blocks: maturity, refund_blocks: refund };
    match a.contractor.offer(t, &a.client.identity()).await {
        Ok(id) => {
            a.rooms.lock().await.insert(id.clone(), Room { arbiters: vec![None; a.oracles.len()], log: vec![] });
            a.log(&id, "contractor", format!("invoiced the client: \u{201c}{question}\u{201d}")).await;
            Json(json!({"id": id})).into_response()
        }
        Err(e) => bad(e),
    }
}

pub(crate) async fn show(Ready(a): Ready, Path(id): Path<String>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let tip = a.contractor.tip().await.ok();
    let contractor = a.contractor.contract(&id).await.ok().flatten().map(|r| r.view());
    let client = a.client.contract(&id).await.ok().flatten().map(|r| r.view());
    let rooms = a.rooms.lock().await;
    let room = rooms.get(&id);
    out(json!({
        "id": id, "tip": tip, "contractor": contractor, "client": client,
        "arbiters": room.map(|r| r.arbiters.clone()).unwrap_or_default(),
        "log": room.map(|r| r.log.clone()).unwrap_or_default(),
    }))
}

#[derive(Deserialize)]
pub(crate) struct AcceptReq {
    pub(crate) funding: String,
}

pub(crate) async fn accept(Ready(a): Ready, Path(id): Path<String>, Json(r): Json<AcceptReq>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let funding = match r.funding.as_str() {
        "treasury" => Funding::Treasury,
        "deposit" => Funding::Deposit,
        f => return bad(format!("funding {f}: treasury or deposit")),
    };
    match a.client.accept(&id, funding).await {
        Ok(()) => {
            a.log(&id, "client", if r.funding == "deposit" { "accepted; deposits from their own wallet (QR code)" } else { "accepted; deposited from the demo wallet" }).await;
            Json(json!({"id": id})).into_response()
        }
        Err(e) => bad(e),
    }
}

pub(crate) async fn qr(Ready(a): Ready, Path(id): Path<String>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let Some(d) = a.client.contract(&id).await.ok().flatten().and_then(|r| r.deposit) else {
        return err(StatusCode::NOT_FOUND, "no deposit link");
    };
    match zdlc::paylink::qr_svg(&d.uri) {
        Ok(svg) => ([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct HoldReq {
    pub(crate) held: bool,
}

pub(crate) async fn hold(Ready(a): Ready, Path((id, who)): Path<(String, String)>, Json(r): Json<HoldReq>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let Some(z) = a.side(&who) else { return bad(format!("no party {who}")) };
    match z.hold(&id, r.held).await {
        Ok(()) => {
            a.log(&id, &who, if r.held { "went offline" } else { "came back online" }).await;
            Json(json!({"held": r.held})).into_response()
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct AttemptReq {
    pub(crate) action: String,
}

pub(crate) async fn attempt(Ready(a): Ready, Path((id, who)): Path<(String, String)>, Json(r): Json<AttemptReq>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    let Some(z) = a.side(&who) else { return bad(format!("no party {who}")) };
    match z.attempt(&id, &r.action).await {
        Ok(v) => {
            let mut v = v;
            if let Some(e) = v["error"].as_str() {
                v["error"] = json!(reason(e));
            }
            let what = match (r.action.as_str(), v["accepted"].as_bool() == Some(true)) {
                ("refund", true) => "sent the refund: accepted (the refund height has passed)".to_string(),
                ("refund", false) => format!("tried to take the refund early: refused ({})", v["error"].as_str().unwrap_or("")),
                (_, true) => "tried to pay itself with forged attestations: accepted (this should never happen)".to_string(),
                (_, false) => format!("tried to pay itself with forged attestations: refused ({})", v["error"].as_str().unwrap_or("")),
            };
            a.log(&id, &who, json!({"text": what, "txid": v["txid"], "accepted": v["accepted"]})).await;
            out(v)
        }
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
pub(crate) struct RuleReq {
    pub(crate) outcome: String,
}

/// Arbiter `n` resolves the room's event.
pub(crate) async fn rule(Ready(a): Ready, Path((id, n)): Path<(String, usize)>, Json(r): Json<RuleReq>) -> Response {
    if let Err(r) = a.known(&id).await {
        return r;
    }
    if !matches!(r.outcome.as_str(), "delivered" | "not delivered") {
        return bad("outcome: delivered or not delivered");
    }
    let Some(url) = a.oracles.get(n) else { return bad(format!("no arbiter {n}")) };
    let Some(rec) = a.contractor.contract(&id).await.ok().flatten() else { return bad("no room") };
    if a.rooms.lock().await.get(&id).and_then(|room| room.arbiters.get(n).cloned().flatten()).is_some() {
        return bad("this arbiter has already ruled");
    }
    let res = a
        .http
        .post(format!("{}/events/{}/resolve", url.trim_end_matches('/'), rec.event_id))
        .json(&json!({"outcome": r.outcome, "token": a.oracle_token}))
        .send()
        .await;
    match res {
        Ok(resp) if resp.status().is_success() => {
            if let Some(room) = a.rooms.lock().await.get_mut(&id) {
                room.arbiters[n] = Some(r.outcome.clone());
            }
            // Saved with the log line below.
            a.log(&id, &format!("arbiter {}", n + 1), format!("ruled \u{201c}{}\u{201d}", r.outcome)).await;
            Json(json!({"arbiter": n, "outcome": r.outcome})).into_response()
        }
        Ok(resp) => {
            let t = resp.text().await.unwrap_or_default();
            bad(format!("the oracle refused: {t}"))
        }
        Err(e) => bad(format!("oracle: {}", e.without_url())),
    }
}
