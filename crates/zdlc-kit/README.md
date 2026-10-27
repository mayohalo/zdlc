# zdlc-kit

Depend on it from the repo, pinned to a release (not on crates.io yet):

```toml
[dependencies]
zdlc-kit = { git = "https://github.com/mayohalo/zdlc", tag = "v0.1.0" }
```

The binaries (`zdlc-node`, `zdlc-cli`, `zdlc-agent`, `zdlc-room`) are built for Linux x86_64 and macOS Apple Silicon on the [releases page](https://github.com/mayohalo/zdlc/releases).

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
| `hedge` | Protects `amount_zat` at `price_cents` a ZEC | Hedges, earns the premium |
| `payroll` | Employee: owed `usd_cents` a period for `periods` periods | Employer; later periods are accepted automatically |
| `pay_on_payment` | Lender: takes the collateral unless repaid to its `repay_to` | Borrower: collateral back once it repays |
| `bet` | Wins both stakes on `offerer_wins` | Wins on the other outcomes |

The accepter derives the terms from the template and its own oracles' announcements, and requires them to equal the offer's terms byte for byte. What it reviews is what it signs.

## Local stack

```sh
just dev            # or scripts/dev.sh up: regtest, desk, two oracles, miner, alice :7950 and bob :7951 with 5 ZEC each
just demo escrow    # escrow | stable | bet, end to end
source .dev/env; za contracts; zb info; zpay <ZIP 321 link>; zresolve <id> delivered
just down --chain
```

`zdlc-agent maker|taker` are desk agents on the kit. `zdlc-dev` funds and mines regtest, and pays links as a phone wallet would.

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
