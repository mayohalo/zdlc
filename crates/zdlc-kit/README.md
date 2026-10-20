# zdlc-kit

The zDLC developer kit: one `Zdlc` object runs contracts end to end. An app chooses four parts, describes a contract with a template, and reacts to events. The kit runs the rest:

- setup: offer, accept, sign;
- funding, from treasury coins or a shielded deposit link any wallet can pay;
- settlement: the cooperative close, the CET, the refund;
- moving payouts into the shielded balance.

| Part | Trait | Implementation |
| --- | --- | --- |
| How peers talk | `Transport` | `DeskTransport`: the desk's sealed mailbox |
| Where contracts live | `Storage` | `SqliteStorage` |
| Who attests | `Oracle` | `HttpOracle`: `zdlc-oracled` |
| The chain | `Chain` | `ZakuraRpc`: Zakura JSON-RPC |

## In Rust

```rust
use std::sync::Arc;
use zdlc_kit::{impls::*, templates::Template, Funding, Zdlc};

let z = Arc::new(
    Zdlc::builder()
        .seed(seed)
        .chain(Arc::new(ZakuraRpc::new(rpc_url)))
        .storage(Arc::new(SqliteStorage::open("zdlc.db")?))
        .oracle(Arc::new(HttpOracle::new("http://127.0.0.1:7801")))
        .oracle(Arc::new(HttpOracle::new("http://127.0.0.1:7802")))
        .transport(Arc::new(DeskTransport::new("http://127.0.0.1:7900", wallet.identity_key())))
        .build()
        .await?,
);
tokio::spawn(z.clone().run(std::time::Duration::from_secs(1)));

let id = z.offer(Template::Escrow { amount_zat: 150_000_000, question: "Milestone 1 delivered?".into(),
                                    payees: vec![], maturity_blocks: 20, refund_blocks: 2000 }, client).await?;
// The client's node: z.accept(&id, Funding::Deposit) → Event::AwaitingDeposit { uri, .. } → show the QR.
// Both follow z.events_after(seq, 100): offered, accepted, signed, funded, attested, settled, payoutShielded.
```

## Templates

| Template | The offerer | The accepter |
| --- | --- | --- |
| `stable_payment` | Is owed `usd_cents` in ZEC at the price minute | Hedges with the same collateral and earns `premium_zat` |
| `escrow` | Contractor: paid `amount_zat` on "delivered", optionally split among `payees` | Client: funds alone; refunded on "not delivered" or by the refund |
| `bet` | Wins both stakes on `offerer_wins` | Wins on the other outcomes |

The accepter derives the terms from the template and its own oracles' announcements, and requires them to equal the offer's terms byte for byte. What it reviews is what it signs.

## zdlc-node and zdlc-cli

`zdlc-node` serves the kit over REST (spec: [`openapi.json`](openapi.json), also at `/v1/openapi.json`) with a bearer token. `zdlc-cli` drives it.

```sh
zdlc-node --seed alice.seed --node $ZDLC_RPC --desk http://127.0.0.1:7900 --oracles http://127.0.0.1:7801,http://127.0.0.1:7802 --listen 127.0.0.1:7950
zdlc-cli --token-file alice.seed.token info                        # identity, treasury address to fund
zdlc-cli --token-file alice.seed.token offer escrow --to <bob identity> --amount 1.5 --question "Milestone 1 delivered?"
zdlc-cli --url http://127.0.0.1:7951 --token-file bob.seed.token accept <id> --funding deposit
zdlc-cli --url http://127.0.0.1:7951 --token-file bob.seed.token qr <id>     # the deposit link and QR
zdlc-cli --token-file alice.seed.token events --follow
zdlc-cli --token-file alice.seed.token withdraw --to <unified address>
```

The node can spend its treasury and commit collateral. Keep it on loopback, or behind TLS and authentication you trust.

From TypeScript: [`@zdlc/client`](../../sdk/ts).

## Tests

```sh
infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh
cargo build -p zdlc --release --bins && (cd sdk/ts && tsc -p .)
cargo test -p zdlc-kit --release --test kit -- --ignored --nocapture --test-threads=1
```
