# zdlc: Discreet Log Contracts on Zcash

zdlc is oracle-settled contracts on Zcash, entered and exited through Ironwood. Two parties lock ZEC in a transparent 2-of-2 for the life of one contract, pre-sign one payout per outcome and a refund, and normally close straight back to Ironwood. Every transaction is v6, built through pczt.

## Docs

| File | What it covers |
| --- | --- |
| [docs/design.md](docs/design.md) | The contract, what Zcash changes, privacy model, architecture tiers |
| [docs/core.md](docs/core.md) | rust-dlc reuse map, TEE oracle network, Ironwood payout spike |
| [docs/applications.md](docs/applications.md) | Shielded payments, private markets, cross-chain, Zcash-native extensions |
| [docs/plan.md](docs/plan.md) | Phased plan with gates, open questions, sources |

## Build and test

Rust 1.97.1 (pinned in `rust-toolchain.toml`), Docker for the regtest node.

```sh
cargo test -p zdlc --release                                         # unit + offline contract tests
infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh             # fresh Zakura regtest, NU7 at 200
cargo test -p zdlc --release --test regtest -- --ignored --nocapture # Phase 0 gate
cargo test -p zdlc --release --test phase1 -- --ignored --nocapture  # Phase 1 gate (fresh chain)
cargo run -p zdlc --release --bin zdlc-oracle -- announce zecusd-1    # dev oracle
```

Results: [Phase 0](docs/spikes/phase0.md), [Phase 1](docs/phase1.md).
