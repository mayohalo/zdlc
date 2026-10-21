# Developer kit: K1, K2, K3 and the rest of item 5

The first two milestones of the [developer kit plan](https://claude.ai/code/artifact/75411bc2-1e3b-4805-b91e-699af3568800) are built and pass on Zakura regtest. Two `zdlc-node`s, driven only by `zdlc-cli` or by `@zdlc/client`, settled four contracts: an escrow funded from a deposit link, a bet settled by CET while the loser's node was down, a stable payment priced from the live ZEC/USD feed, and a bet driven by the TypeScript client. Raw logs: [CLI](spikes/kit-cli.json), [stable payment](spikes/kit-stable-payment.json), [TypeScript](spikes/kit-ts.json).

## What exists

| Piece | Where | What it does |
| --- | --- | --- |
| `Zdlc` + `Builder` | `crates/zdlc-kit` | One object per node: `offer`, `accept`, `decline`, `contracts`, `events_after`, `balance`, `withdraw`, and `run` (the loop) |
| Four traits | `traits.rs`, `impls.rs` | `Transport` (`DeskTransport`), `Storage` (`SqliteStorage`), `Oracle` (`HttpOracle`), `Chain` (`ZakuraRpc`) |
| State machine | `lib.rs` | `offered → incoming → (awaiting_deposit) → accepted → signed → funded → closing → settled`, persisted after every step |
| Templates | `templates.rs` | `stable_payment`, `hedge`, `payroll`, `pay_on_payment`, `escrow` (with payee splits), `bet`. The accepter re-derives the terms from the template and its own oracles and requires byte-equality with the offer |
| Payroll series | `Template::next_period`, `Zdlc::decide` | Each period is its own stable payment. When one settles, the employee's node offers the next, and the employer's node accepts it the way it accepted the first |
| Pay on payment | `Template::PayOnPayment` | The lender's node fills `repay_to` (its contract close address) and that address's viewing key. The borrower's node checks the key really is `repay_to`'s before accepting, since the oracles watch that key |
| Auto-accept policy | `Zdlc::set_policy` | A closure that accepts or leaves incoming offers: an agent's pricing, an app's allowlist |
| Recovery records | `post_record`, `Zdlc::recover`, `POST /v1/recover`, `zdlc-cli recover` | Each side posts its record (a memo to its own close address, paid from the treasury) once signed, retrying until a treasury coin is free. A node restored from the seed alone, with an empty database, finds its records and broadcasts every refund that is due |
| Dev stack | `scripts/dev.sh`, `scripts/demo.sh`, `justfile`, `zdlc-dev` | `just dev` (or `scripts/dev.sh up`) starts regtest, a desk, two arbiter oracles, a miner and two funded nodes. `just demo escrow\|stable\|bet` runs a contract end to end. `zdlc-dev pay URI` plays the user's phone wallet |
| Kit agents | `agent.rs`, `zdlc-agent maker\|taker` | Desk agents rebuilt on the kit. The desk is only the quote board; the maker's policy accepts offers that match its standing quotes |
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

More runs (item 5):

| Run | Result |
| --- | --- |
| Payroll, two periods (`tests/kit.rs`, live price) | Period 1 settled by close at $1,259 (block 259). The employee's node offered period 2 itself, and the employer's node accepted it unasked. Period 2 settled by close at $1,258 (block 318) |
| Pay on payment | Bob locked 0.5 ZEC. The dev wallet repaid 0.3 ZEC to Alice's `repay_to` from a shielded wallet. Both oracles saw it by viewing key and attested "paid", and the collateral came back by close (block 363) |
| Seed-only recovery | A bet the arbiters never resolved. Both nodes were stopped after funding. A node with Alice's seed and an empty database found her record and broadcast the refund once due; the next scan shows it settled |
| Kit agents | A maker and a taker process traded a 0.1 ZEC hedge through the desk's quote board; closed cooperatively (block 237) |
| Dev stack | `scripts/dev.sh up` then `scripts/demo.sh escrow`, `bet` and `stable` all settled by close |

Raw logs: [templates](spikes/kit-templates.json), [recovery](spikes/kit-recovery.json), [agents](spikes/kit-agents.json), [embedded nodes](spikes/kit-sdk.json).

## K3: `@zdlc/sdk` (Node and WebAssembly; React Native skipped)

| Piece | Where | What it does |
| --- | --- | --- |
| Native addon | `crates/zdlc-napi` (napi-rs), `sdk/sdk/index.js` | The whole kit inside a Node process: `Zdlc.open({ seed, rpc, desk, oracles, db })`, with the same calls as `@zdlc/client` |
| WebAssembly core | `crates/zdlc-wasm` (wasm-bindgen, built by wasm-pack), `@zdlc/sdk/core` | The stateless protocol for browsers and Node: wallet addresses, templates, attestation checks, offer / accept / sign / finish, CET, refund, cooperative close (proved in WebAssembly), payment links and QR, an in-memory oracle |
| A pure core crate | `zdlc` with `--no-default-features` | The node, oracle, desk and agent modules sit behind a `net` feature. Without it the crate is the protocol alone and builds for wasm32 (proving single-threaded: `multicore` only comes with `net`) |
| `zdlc::events`, `zdlc::templates` | core | Oracle messages and checks, `MemoryOracle`, and the templates moved out of the kit so the WebAssembly build has them |

| Run | Result |
| --- | --- |
| Core in Node (`sdk/sdk/test/core.test.mjs`) | Two wallets ran a bet offline with an in-memory oracle. Both sides agreed on the funding transaction, either side completed the CET, the refund was locked at its height, and the oracle refused a second outcome. The cooperative close was proved in WebAssembly: 19.3 s single-threaded, including the one-time proving key build. The browser build loads and runs. The module is 7.9 MB |
| Two embedded nodes (`node_sdk_runs_two_nodes`) | Two `@zdlc/sdk` nodes in one Node process. Alice invoiced Bob a 0.5 ZEC escrow, and Bob funded it from a deposit link the dev wallet paid. The arbiters resolved, and both nodes settled by cooperative close (block 240) |

```sh
scripts/build-node.sh && scripts/build-wasm.sh && (cd sdk/sdk && npm install && npm test)
cargo test -p zdlc-kit --release --test kit node_sdk -- --ignored --nocapture
```

## Not done yet

- **gRPC.**
- **Agent parity:** `zdlc-agent` covers the RFQ maker and taker. Memo invitations, deposit-funded takers and taker splits still run on the legacy `zdlc-trader`.
- **Firm quotes:** kit makers quote without naming inputs. Their node reserves coins when it accepts, so a quote can fail if the treasury is spent meanwhile.
- **Recovery after withdrawal:** a record lives in a note at the close address. `withdraw` sweeps it, so recover before withdrawing.
- **Hedge on chain:** the hedge template shares the stable-payment path and has offline tests only.
- **K3 leftovers:**
    - React Native (skipped).
    - Browser proving runs on one thread; a threaded build needs cross-origin isolation.
    - The stateless core funds from treasury coins only: no deposit or leg funding yet.
    - The addon is built for the host platform only, with no prebuilt binaries.
- **K4 and K5:** the hosted testnet, docs, and a stable 1.0.
