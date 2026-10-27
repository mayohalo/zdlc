# Private market demos

Three functions of a private market, one contract underneath, each live on Zcash testnet at https://zdlc.cc:

| Demo | Market function | Try it | What stays private | What is public |
| --- | --- | --- | --- | --- |
| [Escrow room](escrow) | Settle a deal | `/escrow` | Who paid whom, the terms | The arbiters' ruling |
| [Hedge desk](hedge-desk) | Trade a price | `/hedge` | The counterparties, the offer itself | The quotes, the oracles' price |
| [Auction](auction) | Discover a price | `/auction` | The bidders, who won | The clearing price |

All three are one server, [`zdlc-room`](../crates/zdlc-kit/src/bin/zdlc-room), with a module and a page per demo:

```
crates/zdlc-kit/src/bin/zdlc-room/
  main.rs       startup, flags, routes
  common.rs     shared: app state, the ready check, the scrub of private endpoints, node builder, inbox sweeps, block timing
  escrow.rs     /escrow   (pages/escrow.html)
  hedge.rs      /hedge    (pages/hedge.html; the agents are in src/agent.rs)
  auction.rs    /auction  (pages/auction.html)
```

## Run them locally

```sh
just dev    # regtest, the desk, two oracles
just room   # http://127.0.0.1:8080/escrow, /hedge and /auction
```

The hedge desk's price oracles fetch ZEC/USD from four exchanges, so it needs internet access. Hosting on a server: [docs/room.md](../docs/room.md).

## What is a demo shortcut

Every party of every demo runs as a kit node inside this one server, so a visitor can play all of them; the demo wallet funds them, and the server tops them up. That is what makes a demo playable, and what a real app does differently: each user runs their own node or SDK and funds from their own shielded notes. To build one, start with [docs/first-app.md](../docs/first-app.md).
