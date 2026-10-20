# Developer kit: K1 and K2

The first two milestones of the [developer kit plan](https://claude.ai/code/artifact/75411bc2-1e3b-4805-b91e-699af3568800) are built and pass on Zakura regtest. Two `zdlc-node`s, driven only by `zdlc-cli` or by `@zdlc/client`, settled four contracts: an escrow funded from a deposit link, a bet settled by CET while the loser's node was down, a stable payment priced from the live ZEC/USD feed, and a bet driven by the TypeScript client. Raw logs: [CLI](spikes/kit-cli.json), [stable payment](spikes/kit-stable-payment.json), [TypeScript](spikes/kit-ts.json).

## What exists

| Piece | Where | What it does |
| --- | --- | --- |
| `Zdlc` + `Builder` | `crates/zdlc-kit` | One object per node: `offer`, `accept`, `decline`, `contracts`, `events_after`, `balance`, `withdraw`, and `run` (the loop) |
| Four traits | `traits.rs`, `impls.rs` | `Transport` (`DeskTransport`), `Storage` (`SqliteStorage`), `Oracle` (`HttpOracle`), `Chain` (`ZakuraRpc`) |
| State machine | `lib.rs` | `offered → incoming → (awaiting_deposit) → accepted → signed → funded → closing → settled`, persisted after every step |
| Templates | `templates.rs` | `stable_payment`, `escrow` (with payee splits), `bet`. The accepter re-derives the terms from the template and its own oracles and requires byte-equality with the offer |
| Settlement policy | `lib.rs` | The offerer asks for the cooperative close at maturity. Either side settles by CET after 6 blocks and 60 seconds without a close. Either side broadcasts the refund at its height. A settled contract records which transaction spent it: its own close, the attested CET or the refund. CET and refund payouts are moved into the shielded balance |
| Restart | `Party::restore`, `TxSet::with_own_funding` | A restarted node rebuilds live contracts from the stored setup messages. A deposit-funded side keeps its proved funding PCZT, so it can finish after a restart |
| Stale offers | `refresh` message | When a deposit takes longer than the sign-height slack, the accepter asks for fresh heights. The offerer re-offers and the accepter re-accepts with the same notes |
| `zdlc-node` | `src/bin/zdlc-node.rs`, `api.rs` | REST with a bearer token: info, balance, contracts, offer, accept, decline, deposit QR (SVG), long-polled events, withdraw. Spec in `openapi.json` |
| `zdlc-cli` | `src/bin/zdlc-cli.rs` | `offer escrow / stable-payment / bet / json`, `accept --funding treasury\|deposit`, `qr`, `events --follow`, `balance`, `withdraw` |
| `@zdlc/client` | `sdk/ts` | Typed client with no dependencies; `stream()` is an async iterator over events; template builders in decimal ZEC |

## Regtest runs

| Run | Result |
| --- | --- |
| Escrow by deposit, through the CLI | Alice invoiced Bob for 1.5 ZEC. Bob accepted with `--funding deposit`, and an outside wallet paid his link (1.5002 ZEC). Bob's node funded from that note. Both arbiters resolved "delivered", and both nodes recorded the same cooperative close (block 233) |
| Bet from treasuries, loser offline | Bob's node was stopped after funding. Alice settled by CET alone (block 307) and auto-shielded her payout. Bob's restarted node identified Alice's CET as the spend |
| Stable payment, live price | Alice was owed $50 against 0.1 ZEC a side. Both oracles attested the four-exchange median ($1,216), and the contract closed cooperatively (block 237) |
| `@zdlc/client` | Alice offered a bet. Bob's app accepted from its own event stream and saw offered, accepted, signed, funded, attested, settled. Bob won by cooperative close (block 231) |

Two bugs found by these runs are fixed:
- A close request that arrived before the accepter saw the funding mined was dropped.
- On a fast chain the CET fallback outran a slow first price attestation. It now also waits 60 seconds.

```sh
cargo build -p zdlc --release --bins && (cd sdk/ts && tsc -p . && node --test test/unit.test.mjs)
cargo test -p zdlc-kit --release --test kit -- --ignored --nocapture --test-threads=1   # one fresh chain per test is safest
```

## Not done yet

- **From the plan:** the hedge, payroll and pay-on-payment templates; gRPC; rewriting `zdlc-trader` on the kit.
- **Recovery records:** the kit does not post them yet. The node's database and the seed are the backup; `zdlc-live` and the core library already post them.
- **K3 to K5:** `@zdlc/sdk` (UniFFI: WebAssembly, N-API, React Native), the hosted testnet and `just dev`, docs and a stable 1.0.
