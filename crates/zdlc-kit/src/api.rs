//! The REST API `zdlc-node` serves: everything a [`Zdlc`] does, for any language.
//! Every route but the spec needs `Authorization: Bearer <token>`. The OpenAPI spec is
//! `openapi.json` beside this crate, served at `/v1/openapi.json`; `@zdlc/client` is
//! written against it.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{templates::Template, Funding, Zdlc};

pub const OPENAPI: &str = include_str!("../openapi.json");

#[derive(Clone)]
struct App {
    z: Arc<Zdlc>,
    token: Arc<String>,
}

fn err(code: StatusCode, m: impl ToString) -> Response {
    (code, Json(json!({"error": m.to_string()}))).into_response()
}

#[allow(clippy::result_large_err)] // a ready response, returned once per request
fn authed(app: &App, h: &HeaderMap) -> Result<(), Response> {
    let want = format!("Bearer {}", app.token);
    match h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        Some(v) if v == want => Ok(()),
        _ => Err(err(StatusCode::UNAUTHORIZED, "missing or wrong bearer token")),
    }
}

fn ok(v: Value) -> Response {
    Json(v).into_response()
}

fn bad(e: impl ToString) -> Response {
    err(StatusCode::BAD_REQUEST, e)
}

pub fn router(z: Arc<Zdlc>, token: String) -> Router {
    Router::new()
        .route("/v1/openapi.json", get(|| async { ([(header::CONTENT_TYPE, "application/json")], OPENAPI) }))
        .route("/v1/info", get(info))
        .route("/v1/balance", get(balance))
        .route("/v1/contracts", get(list).post(offer))
        .route("/v1/contracts/{id}", get(show))
        .route("/v1/contracts/{id}/accept", post(accept))
        .route("/v1/contracts/{id}/decline", post(decline))
        .route("/v1/contracts/{id}/qr.svg", get(qr))
        .route("/v1/events", get(events))
        .route("/v1/withdraw", post(withdraw))
        .route("/v1/recover", post(recover))
        .with_state(App { z, token: Arc::new(token) })
}

async fn info(State(a): State<App>, h: HeaderMap) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.info().await {
        Ok(v) => ok(v),
        Err(e) => bad(e),
    }
}

async fn balance(State(a): State<App>, h: HeaderMap) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.balance().await {
        Ok(b) => ok(json!(b)),
        Err(e) => bad(e),
    }
}

async fn list(State(a): State<App>, h: HeaderMap) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.contracts().await {
        Ok(v) => ok(json!(v.iter().map(|r| r.view()).collect::<Vec<_>>())),
        Err(e) => bad(e),
    }
}

async fn show(State(a): State<App>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.contract(&id).await {
        Ok(Some(r)) => ok(r.view()),
        Ok(None) => err(StatusCode::NOT_FOUND, format!("no contract {id}")),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct OfferReq {
    counterparty: String,
    template: Template,
}

async fn offer(State(a): State<App>, h: HeaderMap, Json(req): Json<OfferReq>) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.offer(req.template, &req.counterparty).await {
        Ok(id) => ok(json!({"id": id})),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct AcceptReq {
    funding: Funding,
}

async fn accept(State(a): State<App>, h: HeaderMap, Path(id): Path<String>, Json(req): Json<AcceptReq>) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.accept(&id, req.funding).await {
        Ok(()) => ok(json!({"id": id})),
        Err(e) => bad(e),
    }
}

async fn decline(State(a): State<App>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.decline(&id).await {
        Ok(()) => ok(json!({"id": id})),
        Err(e) => bad(e),
    }
}

async fn qr(State(a): State<App>, h: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    let rec = match a.z.contract(&id).await {
        Ok(Some(r)) => r,
        Ok(None) => return err(StatusCode::NOT_FOUND, format!("no contract {id}")),
        Err(e) => return bad(e),
    };
    let Some(d) = rec.deposit else { return err(StatusCode::NOT_FOUND, "no deposit link") };
    match zdlc::paylink::qr_svg(&d.uri) {
        Ok(svg) => ([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response(),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct EventsQ {
    #[serde(default)]
    after: u64,
    /// Long-poll: wait up to this many seconds for an event.
    #[serde(default)]
    wait: u64,
    #[serde(default)]
    limit: Option<usize>,
}

async fn events(State(a): State<App>, h: HeaderMap, Query(q): Query<EventsQ>) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(q.wait.min(60));
    loop {
        match a.z.events_after(q.after, q.limit.unwrap_or(100).min(1000)).await {
            Ok(evs) if !evs.is_empty() || std::time::Instant::now() >= deadline => {
                let next = evs.last().map(|e| e.0).unwrap_or(q.after);
                let list: Vec<Value> = evs
                    .into_iter()
                    .map(|(seq, e)| {
                        let mut v = json!(e);
                        v["seq"] = json!(seq);
                        v
                    })
                    .collect();
                return ok(json!({"events": list, "next": next}));
            }
            Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(250)).await,
            Err(e) => return bad(e),
        }
    }
}

#[derive(Deserialize)]
struct WithdrawReq {
    to: String,
}

async fn withdraw(State(a): State<App>, h: HeaderMap, Json(req): Json<WithdrawReq>) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.withdraw(&req.to).await {
        Ok(txids) => ok(json!({"txids": txids})),
        Err(e) => bad(e),
    }
}

#[derive(Deserialize)]
struct RecoverReq {
    #[serde(default)]
    from: u32,
    #[serde(default = "gap")]
    gap: u32,
}

fn gap() -> u32 {
    20
}

async fn recover(State(a): State<App>, h: HeaderMap, Json(req): Json<RecoverReq>) -> Response {
    if let Err(r) = authed(&a, &h) {
        return r;
    }
    match a.z.recover(req.from, req.gap).await {
        Ok(v) => ok(json!({"records": v})),
        Err(e) => bad(e),
    }
}
