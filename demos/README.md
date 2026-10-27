# Private market demos

Three functions of a private market, one contract underneath, each live on Zcash testnet at https://zdlc.cc:

| Demo | Market function | Try it | What stays private | What is public |
| --- | --- | --- | --- | --- |
| [Escrow room](escrow) | Settle a deal | `/escrow` | Who paid whom, the terms | The arbiters' ruling |
| [Hedge desk](hedge) | Trade a price | `/hedge` | The counterparties, the offer itself | The quotes, the oracles' price |
| [Auction](auction) | Discover a price | `/auction` | The bidders, who won | The clearing price |

## The code is here

This folder is the server for all three demos, `zdlc-room`: a Rust crate (`zdlc-demos`) with one folder per demo. Each folder holds that demo's code, its page and its README:

```
demos/
  Cargo.toml        the crate; builds the zdlc-room binary
  main.rs           startup, flags, routes for all three
  common.rs         shared: app state, the ready check, the scrub of private endpoints, node builder, inbox sweeps, block timing
  escrow/           /escrow   mod.rs (server), page.html (the visitor's page), README.md
  hedge/            /hedge    mod.rs, page.html, README.md  (the agents are in crates/zdlc-kit/src/agent.rs)
  auction/          /auction  mod.rs, page.html, README.md  (the template is in crates/zdlc/src/templates.rs)
```

Every demo file has the same shape: a state struct for what is on screen, HTTP handlers for the buttons, and one async task that drives the contracts through the kit (`offer`, `accept`, the oracles ruling, `Settled`). The page is plain HTML and JavaScript with no build step; it polls the demo's `api/state` and draws it.

What is a demo shortcut, not protocol: every party runs in this one process, the demo wallet funds them, and the server tops them up (in `common.rs` and each demo's `*_demo` setup). The protocol calls are the ones listed in each demo's README.

## Run them locally

```sh
just dev    # regtest, the desk, two oracles
just room   # http://127.0.0.1:8080/escrow, /hedge and /auction
cargo build --release -p zdlc-demos   # builds the server alone: target/release/zdlc-room
```

The hedge desk's price oracles fetch ZEC/USD from four exchanges, so it needs internet access. Hosting on a server: [docs/room.md](../docs/room.md).

## What is a demo shortcut

Every party of every demo runs as a kit node inside this one server, so a visitor can play all of them; the demo wallet funds them, and the server tops them up. That is what makes a demo playable, and what a real app does differently: each user runs their own node or SDK and funds from their own shielded notes. To build one, start with [docs/first-app.md](../docs/first-app.md).
