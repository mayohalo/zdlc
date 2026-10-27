//! zdlc-room: the three private market demos in one server, each a module with a page.
//!
//! - `escrow` (`/escrow`): the visitor plays a client, a contractor and both arbiters.
//! - `hedge` (`/hedge`): two agents trade a ZEC/USD hedge over the RFQ desk.
//! - `auction` (`/auction`): a sealed-bid auction; the price public, the bidders not.
//! - `common`: what they share. The demos' READMEs are in `demos/` at the repo root.
//!
//! Every party of every demo runs as a kit node in this process, so a visitor can play
//! them all; a real app gives each party its own node or SDK.
//!
//! ```sh
//! zdlc-room --node $ZDLC_RPC --desk http://127.0.0.1:7900 --oracles http://127.0.0.1:7801,http://127.0.0.1:7802 \
//!           --oracle-token-file admin.token [--dir room] [--listen 127.0.0.1:8080] [--amount 0.01] \
//!           [--maturity 2] [--refund 4] [--close-wait 1] [--close-wait-secs 30] [--block-secs 75] \
//!           [--max-rooms 20] [--label testnet] [--explorer 'https://…/tx/{txid}']
//! ```
//!
//! A room lasts about 5 minutes on testnet (75 s blocks): the refund unlocks 4 blocks
//! after the invoice. The contract settles as soon as both arbiters have ruled; a side
//! whose counterparty is offline settles alone `--close-wait` blocks after the rulings.
//! The maturity (2 blocks) only bounds how long the invoice stays valid.
//!
//! The page and its API are public and need no token: they can only run the demo
//! escrow, at `--amount`, between the two demo nodes, with at most `--max-rooms` open at
//! once. The oracles' admin token stays on the server; the page resolves only the events
//! of the rooms it opened.

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

mod auction;
mod common;
mod escrow;
mod hedge;

#[allow(unused_imports)]
use crate::{auction::*, common::*, escrow::*, hedge::*};

// More workers than cores: proving a close or signing a hedge's CETs is CPU work done
// inside a task, and with one worker per core (one, on a small server) it would stall
// every other task meanwhile: the other agent, the event stream, the pages.
#[tokio::main(flavor = "multi_thread", worker_threads = 6)]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rpc = match arg(&args, "--node", "").as_str() {
        "" => std::env::var("ZDLC_RPC").unwrap_or_else(|_| die("--node URL or ZDLC_RPC")),
        u => u.to_string(),
    };
    let dir = arg(&args, "--dir", "room");
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| die(e));
    let oracles: Vec<String> = arg(&args, "--oracles", "").split(',').filter(|s| !s.is_empty()).map(String::from).collect();
    if oracles.is_empty() {
        die("--oracles URL,URL");
    }
    let token_file = arg(&args, "--oracle-token-file", "");
    let oracle_token = std::fs::read_to_string(&token_file).unwrap_or_else(|e| die(format!("--oracle-token-file: {e}"))).trim().to_string();
    let block_secs: i64 = arg(&args, "--block-secs", "75").parse().unwrap_or_else(|_| die("--block-secs"));
    let amount_zat = zat(&arg(&args, "--amount", "0.01"));
    let cfg = Cfg {
        amount_zat,
        maturity: arg(&args, "--maturity", "2").parse().unwrap_or_else(|_| die("--maturity BLOCKS")),
        refund: arg(&args, "--refund", "4").parse().unwrap_or_else(|_| die("--refund BLOCKS")),
        close_wait: (
            arg(&args, "--close-wait", "1").parse().unwrap_or_else(|_| die("--close-wait BLOCKS")),
            arg(&args, "--close-wait-secs", "30").parse().unwrap_or_else(|_| die("--close-wait-secs SECS")),
        ),
        max_rooms: arg(&args, "--max-rooms", "20").parse().unwrap_or_else(|_| die("--max-rooms")),
        block_secs,
        explorer: arg(&args, "--explorer", ""),
        label: arg(&args, "--label", "testnet"),
    };
    let listen = arg(&args, "--listen", "127.0.0.1:8080");
    let slot: Shared = Arc::new(Slot(tokio::sync::OnceCell::new()));
    {
        let slot = slot.clone();
        tokio::spawn(async move {
            // Both nodes need the chain to start; retry until it answers.
            let (contractor, client) = loop {
                match (node(&args, &dir, "contractor", &rpc, &cfg, false, true).await, node(&args, &dir, "client", &rpc, &cfg, true, true).await) {
                    (Ok(k), Ok(c)) => break (k, c),
                    (Err(e), _) | (_, Err(e)) => eprintln!("zdlc-room: waiting for the node: {e}"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            };
            for z in [&contractor, &client] {
                tokio::spawn(z.clone().run(std::time::Duration::from_millis(1000)));
            }
            // The client's node accepts nothing by itself: the visitor decides.
            println!("zdlc-room: contractor {} client {}", contractor.identity(), client.identity());
            println!("zdlc-room: fund the demo wallet at {} (shielded; swept into coins every minute)", client.inbox_address());
            spawn_sweeper(client.clone(), format!("{dir}/inbox.from"));
            let desk = desk_demo(&args, &dir, &rpc, &cfg, &oracles, client.clone()).await;
            let auction = auction_demo(&args, &dir, &rpc, &cfg, client.clone()).await;
            let rooms_file = format!("{dir}/rooms.json");
            let rooms: HashMap<String, Room> = std::fs::read(&rooms_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
            let app = Arc::new(App { contractor, client, oracles, oracle_token, http: reqwest::Client::new(), rooms: Mutex::new(rooms), rooms_file, cfg, desk, auction, node: zdlc::node::Node::new(rpc.clone()), block_time: std::sync::Mutex::new((0, 0.0)) });
            let _ = slot.0.set(app);
        });
    }
    let r = Router::new()
        // Everything under /escrow and /hedge, so a site can serve its own pages beside
        // them. The old /room and /desk addresses redirect, so shared links keep working.
        .route("/", get(|| async { axum::response::Redirect::temporary("/escrow") }))
        .route("/room", get(|| async { axum::response::Redirect::permanent("/escrow") }))
        .route("/room/r/{id}", get(|Path(id): Path<String>| async move { axum::response::Redirect::permanent(&format!("/escrow/r/{id}")) }))
        .route("/desk", get(|| async { axum::response::Redirect::permanent("/hedge") }))
        .route("/escrow", get(page))
        .route("/escrow/r/{id}", get(page))
        .route("/escrow/api/info", get(info))
        .route("/escrow/api/rooms", post(open))
        .route("/escrow/api/rooms/{id}", get(show))
        .route("/escrow/api/rooms/{id}/accept", post(accept))
        .route("/escrow/api/rooms/{id}/qr.svg", get(qr))
        .route("/escrow/api/rooms/{id}/{who}/hold", post(hold))
        .route("/escrow/api/rooms/{id}/{who}/attempt", post(attempt))
        .route("/escrow/api/rooms/{id}/arbiters/{n}", post(rule))
        .route("/hedge", get(desk_page))
        .route("/hedge/api/info", get(desk_info))
        .route("/hedge/api/state", get(desk_state))
        .route("/hedge/api/trade", post(desk_trade))
        .route("/auction", get(auction_page))
        .route("/auction/api/info", get(auction_info))
        .route("/auction/api/state", get(auction_state))
        .route("/auction/api/bid", post(auction_bid))
        .with_state(slot);
    println!("zdlc-room: http://{listen}/escrow and http://{listen}/hedge");
    let l = tokio::net::TcpListener::bind(&listen).await.unwrap_or_else(|e| die(e));
    axum::serve(l, r).await.unwrap_or_else(|e| die(e));
}
