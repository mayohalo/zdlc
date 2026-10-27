# Build your first zDLC app

A zDLC app offers contracts, accepts them, and follows them to settlement. The kit does the protocol: signing every payout before money moves, funding, watching the chain, the cooperative close, and the fallbacks (a pre-signed payout if the other side disappears, the refund if the oracles never answer). Your app chooses a template, decides what to accept, and shows the events.

## What you need

| Piece | What it is | Locally |
| --- | --- | --- |
| A Zakura node | The chain, over JSON-RPC | `just dev` starts a regtest one |
| The desk | The mailbox the parties' nodes exchange contract messages through (sealed: the desk cannot read them) | started by `just dev` |
| Oracles | Two `zdlc-oracled`: the ZEC/USD price, or an arbiter's ruling | started by `just dev` |
| A node per party | `zdlc-node` (REST), or the kit / SDK inside your app | your code |

## Pick a way in

| You are building | Use | Docs |
| --- | --- | --- |
| A Rust service, agent or bot | `zdlc-kit`: `Zdlc::builder()`, `offer`, `accept`, `run` | [crates/zdlc-kit](../crates/zdlc-kit) |
| A web or mobile app, any language | one `zdlc-node` per user, called over REST; `@zdlc/client` for TypeScript | [sdk/ts](../sdk/ts) |
| A Node.js backend without a separate node | `@zdlc/sdk` (the kit as a native addon) | [sdk/sdk](../sdk/sdk) |
| A self-custodial browser app | `@zdlc/sdk/core` (the protocol in WebAssembly; your app carries the messages) | [sdk/sdk](../sdk/sdk) |

## Pick a template

| Template | One side locks | Settles on | Demo |
| --- | --- | --- | --- |
| `escrow` | the client (the amount) | an arbiter's ruling; up to 16 payees | [/escrow](../demos/escrow) |
| `stable_payment` | both | the ZEC/USD price at a minute | [/hedge](../demos/hedge-desk) |
| `auction` | each bidder (the same deposit) | the auctioneer's ruling: "won at" a price or "lost" | [/auction](../demos/auction) |
| `bet` | both | an arbiter's ruling among any outcomes | |
| `payroll` | the employer | the price, period after period | |
| `pay_on_payment` | the borrower | whether a shielded repayment arrived (viewing key) | |

## The flow, in Rust

[`crates/zdlc-kit/examples/first_app.rs`](../crates/zdlc-kit/examples/first_app.rs) is the whole thing, about 100 lines with comments. Against the dev stack:

```sh
just dev
cargo run -p zdlc-kit --release --example first_app
```

The steps:

```rust
// 1. Each party is a node: a seed, the chain, a database, the desk, the oracles.
let z = Zdlc::builder()
    .seed(seed)
    .chain(Arc::new(ZakuraRpc::new(RPC)))
    .storage(Arc::new(SqliteStorage::open("contractor.db")?))
    .transport(Arc::new(DeskTransport::new(DESK, wallet.identity_key())))
    .oracle(Arc::new(HttpOracle::new(ORACLE_1)))
    .oracle(Arc::new(HttpOracle::new(ORACLE_2)))
    .build()
    .await?;
tokio::spawn(z.clone().run(Duration::from_secs(1)));   // the protocol loop

// 2. The client decides what it accepts (or shows offers to its user).
client.set_policy(Arc::new(|rec| matches!(rec.template, Template::Escrow { .. }).then_some(Funding::Treasury)));

// 3. The contractor offers; the counterparty is its identity.
let id = contractor.offer(Template::Escrow { amount_zat: 1_000_000, question: "Logo delivered?".into(),
    payees: vec![], maturity_blocks: 10, refund_blocks: 300 }, &client.identity()).await?;

// 4. Follow it: z.contract(&id) for the record, z.events_after(seq, n) for the log.
// 5. The arbiters rule (on the dev stack, the oracles' admin endpoint plays them).
// 6. Settled: the cooperative close, into shielded notes.
```

## The same flow over REST, in TypeScript

`just dev` also starts two `zdlc-node`s, Alice on :7950 and Bob on :7951, each funded, with tokens in `.dev/`:

```ts
import { ZdlcClient, templates } from "@zdlc/client";
import { readFileSync } from "node:fs";

const token = (who: string) => readFileSync(`.dev/${who}.seed.token`, "utf8").trim();
const alice = new ZdlcClient("http://127.0.0.1:7950", { token: token("alice") });   // the contractor
const bob = new ZdlcClient("http://127.0.0.1:7951", { token: token("bob") });       // the client

const { identity } = await bob.info();
const { id } = await alice.offer(identity, templates.escrow({ amount: "0.01", question: "Logo delivered?" }));

await bob.waitFor(id, "incoming");
await bob.accept(id, "treasury");          // or "deposit": then show bob.depositQr(id) to any wallet
await bob.waitFor(id, "funded");

// The arbiters rule (dev stack: the oracles' admin endpoint).
const admin = readFileSync(".dev/admin", "utf8").trim();
const { eventId } = await alice.contract(id);
for (const o of ["http://127.0.0.1:7801", "http://127.0.0.1:7802"])
  await fetch(`${o}/events/${eventId}/resolve`, { method: "POST", headers: { "content-type": "application/json" },
    body: JSON.stringify({ outcome: "delivered", token: admin }) });

for await (const ev of alice.stream()) if (ev.kind === "settled" && ev.id === id) { console.log(ev.how, ev.txid); break; }
```

## From a demo to a real app

The [demos](../demos) run every party in one server so a visitor can play them all. A real app changes four things:

1. **Each user runs their own node or SDK.** Their keys never leave their device or server.
2. **Funding from shielded notes.** Accept with `Funding::Deposit` and show the deposit link as a QR; the user pays it from any Zcash wallet, so nothing on chain points back to them.
3. **Real oracles.** Price contracts need no one: the oracles attest the ZEC/USD median of four exchanges at the minute. Ruled contracts need an arbiter you trust for the outcome (they can never take the funds), ideally two that must agree.
4. **Windows sized for the network.** On testnet, blocks and inclusion vary; size maturity and refund in time and convert to blocks (the demos measure the recent block interval). Turn on `close_before_funded` to settle in one block when the outcome is known before funding.

zDLC is not audited yet: use testnet.
