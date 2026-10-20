# zdlc: Discreet Log Contracts on Zcash

zdlc is oracle-settled contracts on Zcash, entered and exited through Ironwood. Two parties lock ZEC in a transparent 2-of-2 for the life of one contract, pre-sign one payout per outcome and a refund, and normally close straight back to Ironwood. Every transaction is v6, built through pczt.

## Docs

| File | What it covers |
| --- | --- |
| [docs/design.md](docs/design.md) | The contract, what Zcash changes, privacy model, architecture tiers |
| [docs/core.md](docs/core.md) | rust-dlc reuse map, TEE oracle network, Ironwood payout spike |
| [docs/applications.md](docs/applications.md) | Shielded payments, private markets, cross-chain, Zcash-native extensions |
| [docs/plan.md](docs/plan.md) | Phased plan with gates, open questions, sources |

## Demo

A stable-value ZEC payment on the local regtest node: Alice is owed $100 and holds 2 ZEC at $50.00, Bob hedges with 2 ZEC, and an oracle attests the ZEC/USD price at maturity.

```sh
infra/zakura/scripts/up.sh
cargo run -p zdlc --release --bin zdlc-demo -- --price 60.00 --settle close   # cooperative close to Ironwood
cargo run -p zdlc --release --bin zdlc-demo -- --price 40.00 --settle cet     # Bob goes silent, Alice uses the CET
cargo run -p zdlc --release --bin zdlc-demo -- --settle refund                # the oracle never attests
```

Each run uses fresh keys and coins, so they can follow each other on one chain.

## Testnet

`zdlc-testnet` runs the same contract on Zcash testnet through any Zakura testnet JSON-RPC endpoint, given as `--node URL` or `ZDLC_TESTNET_RPC` (never stored in the repo). Collateral is 0.01 TAZ a side, capped at 0.05.

```sh
export ZDLC_TESTNET_RPC=<your testnet Zakura RPC URL>
cargo run -p zdlc --release --example testnet_wallet          # seed at ~/.zdlc/testnet-seed.hex; prints the address to fund
cargo run -p zdlc --release --bin zdlc-testnet -- status
cargo run -p zdlc --release --bin zdlc-testnet -- run --settle refund   # seed-only refund recovery
cargo run -p zdlc --release --bin zdlc-testnet -- run --settle close --price 60.00
cargo run -p zdlc --release --bin zdlc-testnet -- sweep         # return transparent payouts to the treasury
```

## Build and test

Rust 1.97.1 (pinned in `rust-toolchain.toml`), Docker for the regtest node.

```sh
cargo test -p zdlc --release                                         # unit + offline contract tests
infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh             # fresh Zakura regtest, NU7 at 200
cargo test -p zdlc --release --test regtest -- --ignored --nocapture # Phase 0 gate (needs a fresh chain)
cargo test -p zdlc --release --test phase1 -- --ignored --nocapture  # Phase 1 gate
cargo test -p zdlc --release --test phase2 -- --ignored --nocapture  # Phase 2 gate (needs a fresh chain)
cargo run -p zdlc --release --bin zdlc-oracle -- announce zecusd-1    # dev oracle
cargo run -p zdlc --release --bin zdlc-watch -- kit.json              # refund watcher
```

Results: [Phase 0](docs/spikes/phase0.md), [Phase 1](docs/phase1.md), [Phase 2](docs/phase2.md), [testnet](docs/testnet.md).
