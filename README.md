# zdlc: Discreet Log Contracts on Zcash

zdlc is oracle-settled contracts on Zcash, entered and exited through Ironwood. Two parties lock ZEC in a transparent 2-of-2 for the life of one contract, pre-sign one payout per outcome and a refund, and normally close straight back to Ironwood. Every transaction is v6, built through pczt.

## Start here

- **Try the private market demos**, live on Zcash testnet: [escrow room](demos/escrow) (settle a deal), [hedge desk](demos/hedge-desk) (trade a price), [auction](demos/auction) (discover a price). How they are built and run: [demos/](demos).
- **Build an app:** [docs/first-app.md](docs/first-app.md) picks a way in (Rust kit, REST with `@zdlc/client`, Node or browser SDK) and a template, and walks through an escrow in about 100 lines ([`examples/first_app.rs`](crates/zdlc-kit/examples/first_app.rs)).
- **Install:** the packages and binaries are on the [releases page](https://github.com/mayohalo/zdlc/releases): `npm install https://github.com/mayohalo/zdlc/releases/download/v0.1.0/zdlc-client-0.1.0.tgz`, or `zdlc-kit = { git = "https://github.com/mayohalo/zdlc", tag = "v0.1.0" }` in Cargo.toml. See [docs/first-app.md](docs/first-app.md#install).
- **Run everything locally:** `just dev` (regtest, the desk, two oracles, two nodes), then `just room` for the three demos at http://127.0.0.1:8080/escrow, /hedge and /auction.

## Docs

| File | What it covers |
| --- | --- |
| [docs/design.md](docs/design.md) | The contract, what Zcash changes, privacy model, architecture tiers |
| [docs/core.md](docs/core.md) | rust-dlc reuse map, TEE oracle network, Ironwood payout spike |
| [docs/applications.md](docs/applications.md) | Shielded payments, private markets, cross-chain, Zcash-native extensions |
| [docs/plan.md](docs/plan.md) | Phased plan with gates, open questions, sources |
| [docs/beneficiaries.md](docs/beneficiaries.md) | Multi-beneficiary payouts: splits, per-outcome overrides, multi-output CETs and closes |
| [site/index.html](site/index.html) | The landing page: open it in a browser |
| [docs/kit.md](docs/kit.md) | Developer kit: the `Zdlc` object, templates, `zdlc-node` REST API, `zdlc-cli`, `@zdlc/client` |
| [docs/first-app.md](docs/first-app.md) | Build your first zDLC app: ways in, templates, the flow in Rust and TypeScript, from demo to real app |
| [demos/](demos) | The three private market demos: one README each, the code layout, running them |
| [docs/room.md](docs/room.md) | Hosting the demos: the escrow room and auction in detail, the kit calls behind them, deploying to a server |
| [docs/funding.md](docs/funding.md) | Shielded funding by QR: deposit links (path A), legs paid from any wallet (path B), auto-shield and withdraw |

## Demo

A stable-value ZEC payment on the local regtest node: Alice is owed $100 and holds 2 ZEC at $50.00, Bob hedges with 2 ZEC, and an oracle attests the ZEC/USD price at maturity.

```sh
infra/zakura/scripts/up.sh
cargo run -p zdlc --release --bin zdlc-demo -- --price 60.00 --settle close   # cooperative close to Ironwood
cargo run -p zdlc --release --bin zdlc-demo -- --price 40.00 --settle cet     # Bob goes silent, Alice uses the CET
cargo run -p zdlc --release --bin zdlc-demo -- --settle refund                # the oracle never attests
```

Each run uses fresh keys and coins, so they can follow each other on one chain.

## Live networks and oracles

`zdlc-live` runs the contract on Zcash testnet or mainnet through any Zakura JSON-RPC endpoint, given as `--node URL` or `ZDLC_RPC` (never stored in the repo). The network and its upgrade heights come from the node; mainnet needs `--mainnet`. Collateral is capped at 0.05 TAZ (testnet) and 0.01 ZEC (mainnet) a side.

`zdlc-oracled` is one oracle of a 2-of-2 pair, built to run in a TEE: it attests ZEC/USD (the median close of one minute across four exchanges) and shielded payments (by viewing key), with its keys and nonce counter in sealed state.

```sh
export ZDLC_RPC=<your Zakura RPC URL>
cargo run -p zdlc --release --example wallet -- testnet          # seed at ~/.zdlc/testnet-seed.hex; prints the address to fund
cargo run -p zdlc --release --bin zdlc-oracled -- --listen 127.0.0.1:7801 --state a.sealed --sealing-key a.key --node $ZDLC_RPC
cargo run -p zdlc --release --bin zdlc-oracled -- --listen 127.0.0.1:7802 --state b.sealed --sealing-key b.key --node $ZDLC_RPC
cargo run -p zdlc --release --bin zdlc-live -- status
cargo run -p zdlc --release --bin zdlc-live -- run --settle cet --oracles http://127.0.0.1:7801,http://127.0.0.1:7802
cargo run -p zdlc --release --bin zdlc-live -- run --settle refund    # seed-only refund recovery
cargo run -p zdlc --release --bin zdlc-live -- sweep                 # return transparent payouts to the treasury
```

## The RFQ desk

`zdlc-desk` is a quote board and a sealed mailbox (put it behind TLS). `zdlc-trader` runs a maker (hedger) or a taker (recipient), each with its own seed; they meet only through the desk, the oracles and the chain.

```sh
cargo run -p zdlc --release --bin zdlc-desk -- --listen 127.0.0.1:7900 --state desk.json --node $ZDLC_RPC
cargo run -p zdlc --release --bin zdlc-trader -- address --seed maker.seed      # fund the printed treasury
cargo run -p zdlc --release --bin zdlc-trader -- maker --seed maker.seed --desk http://127.0.0.1:7900 --oracles $ORACLES --premium-bps 50
cargo run -p zdlc --release --bin zdlc-trader -- taker --seed taker.seed --desk http://127.0.0.1:7900 --oracles $ORACLES --collateral 1000000 --settle close
# private quote by memo invitation: no public RFQ
cargo run -p zdlc --release --bin zdlc-trader -- maker ... --invite <taker inbox hex> --invite-collateral 1000000
cargo run -p zdlc --release --bin zdlc-trader -- taker ... --invitations-from <height>
# the taker's side paid 60 % to itself, 40 % to a partner (Ironwood on the close)
cargo run -p zdlc --release --bin zdlc-trader -- taker ... --split self:6000,<tm address>+<raw Ironwood hex>:4000
```

## Developer kit

Build apps on zDLC without touching the protocol ([docs/kit.md](docs/kit.md)):

- **Rust:** one `Zdlc` object ([crates/zdlc-kit](crates/zdlc-kit)).
- **Any language:** `zdlc-node`, a REST node ([OpenAPI spec](crates/zdlc-kit/openapi.json)), driven by `zdlc-cli`.
- **TypeScript:** [`@zdlc/client`](sdk/ts).
- **Self-custodial JavaScript:** [`@zdlc/sdk`](sdk/sdk). It runs a full node inside Node.js, and its WebAssembly core runs the stateless protocol in browsers.

Templates: stable payment, escrow, bet.

```sh
cargo run -p zdlc-kit --release --bin zdlc-node -- --seed alice.seed --node $ZDLC_RPC --desk http://127.0.0.1:7900 --oracles $ORACLES
cargo run -p zdlc-kit --release --bin zdlc-cli -- --token-file alice.seed.token offer escrow --to <identity> --amount 1.5 --question "Delivered?"
```

## Funding by QR

A user funds a contract from any Zcash wallet by scanning a ZIP 321 link (see [docs/funding.md](docs/funding.md)):

- **Path A:** pay a deposit receiver in your own zDLC wallet, which funds the contract from that shielded note after every payout is signed.
- **Path B:** pay the contract's leg address. The legs are swept into the 2-of-2 once every payout is signed, and each payer can reclaim its own leg from the refund height.

```sh
# the taker funds its side from a shielded deposit: it prints a link and QR, any wallet pays it
cargo run -p zdlc --release --bin zdlc-trader -- taker ... --fund deposit
cargo run -p zdlc --release --bin zdlc-trader -- balance --seed taker.seed
cargo run -p zdlc --release --bin zdlc-trader -- shield --seed taker.seed            # CET payouts into the shielded balance
cargo run -p zdlc --release --bin zdlc-trader -- withdraw --seed taker.seed --to <unified address>
```

A contract can pay up to 16 beneficiaries who never sign: each side's payout splits by basis points, and enumerated outcomes can carry fixed output lists (see [docs/beneficiaries.md](docs/beneficiaries.md)).

## Build and test

Rust 1.97.1 (pinned in `rust-toolchain.toml`), Docker for the regtest node.

```sh
cargo test -p zdlc --release                                         # unit + offline contract tests
infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh             # fresh Zakura regtest, NU7 at 200
cargo test -p zdlc --release --test regtest -- --ignored --nocapture # Phase 0 gate (needs a fresh chain)
cargo test -p zdlc --release --test phase1 -- --ignored --nocapture  # Phase 1 gate
cargo test -p zdlc --release --test phase2 -- --ignored --nocapture  # Phase 2 gate (needs a fresh chain)
cargo test -p zdlc --release --test phase3 -- --ignored --nocapture  # two oracle daemons, 2-of-2 (needs internet)
cargo test -p zdlc --release --test phase4 -- --ignored --nocapture  # maker and taker processes trade through the desk
cargo test -p zdlc --release --test products -- --ignored --nocapture --test-threads=1  # invitation, escrow, payroll roll
cargo test -p zdlc --release --test beneficiaries -- --ignored --nocapture  # team escrow: 3 payees by CET and by close
cargo test -p zdlc --release --test funding -- --ignored --nocapture --test-threads=1  # funding by QR: deposits, legs, desk trade
cargo test -p zdlc-kit --release --test kit -- --ignored --nocapture --test-threads=1   # zdlc-cli, @zdlc/client, @zdlc/sdk, templates, agents
scripts/build-node.sh && scripts/build-wasm.sh && (cd sdk/sdk && npm install && npm test)  # @zdlc/sdk: native addon and WebAssembly core
cargo run -p zdlc --release --bin zdlc-oracle -- announce zecusd-1    # dev oracle
cargo run -p zdlc --release --bin zdlc-watch -- kit.json              # refund watcher
```

Results: [Phase 0](docs/spikes/phase0.md), [Phase 1](docs/phase1.md), [Phase 2](docs/phase2.md), [testnet](docs/testnet.md), [Phase 3](docs/phase3.md), [Phase 4](docs/phase4.md), [beneficiaries](docs/beneficiaries.md), [funding by QR](docs/funding.md), [developer kit](docs/kit.md).
