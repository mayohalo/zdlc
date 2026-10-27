# Escrow room: settle a deal

A contractor invoices a client for 0.001 ZEC; two arbiters decide whether the work was delivered. The visitor plays the client, both arbiters and the contractor: they can rule, take a party offline, or try to cheat, and watch the network enforce the outcome.

Live at `/escrow` on the hosted site.

## Code

Everything for this demo is in this folder: [`mod.rs`](mod.rs), the server side (about 270 lines), and [`page.html`](page.html), the visitor's page. Read these first:

| Where | What it does |
| --- | --- |
| [`mod.rs`](mod.rs) `open` | the contractor offers `Template::Escrow` to the client's identity (`contractor.offer`) |
| `accept` | the client accepts, funded from the treasury or a deposit link (`client.accept(id, Funding::…)`) |
| `rule` | an arbiter rules: a POST to its oracle's `/events/{event_id}/resolve` |
| [`../common.rs`](../common.rs) `node` | how each party is built: `Zdlc::builder()` with `close_before_funded`, then `run` in the background |

There is no settlement code in the demo: once both arbiters have ruled, the kit's `run` loop on each node closes the contract. `show` only reads the record (`contract(id)`) for the page. `hold` and `attempt` are the misbehave buttons, demo-only.

- Page: [`page.html`](page.html) polls `GET /escrow/api/rooms/{id}` and draws it
- API: `POST /escrow/api/rooms` (`{"team": true}` splits the payout 50/30/20 between three payees), `GET /escrow/api/rooms/{id}`, `POST …/accept`, `…/{who}/hold`, `…/{who}/attempt`, `…/arbiters/{n}`, `GET …/qr.svg`
- The same flow without any demo machinery: [`examples/first_app.rs`](../../crates/zdlc-kit/examples/first_app.rs)

## How it works

| | |
| --- | --- |
| Template | `escrow`: the client locks the amount; the contractor is paid on "delivered", the client refunded on "not delivered" or by the time lock |
| Parties | `contractor` (offerer, locks nothing) and `client` (accepter), two kit nodes |
| Oracles | the two arbiters: `zdlc-oracled` arbiter events, resolved from the page through the server's admin token |
| Funding | the client's treasury ("Deposit from the demo wallet") or a deposit link paid from any wallet (QR) |

1. **Invoice.** `contractor.offer(Template::Escrow { .. }, client_identity)`.
2. **Accept and fund.** `client.accept(id, Funding::Treasury | Funding::Deposit)`. Both sides adaptor-sign the payout for each outcome and the refund before the 2-of-2 is funded.
3. **Rule.** Each arbiter signs "delivered" or "not delivered". Both must agree: their signatures add up to complete exactly one payout.
4. **Settle.** With both rulings, the contractor asks for the cooperative close (even before the funding is mined: `close_before_funded`), the client checks it pays the ruled outcome and co-signs, and it goes into shielded notes. If a party is offline, the other completes its pre-signed payout alone after `--close-wait`. If the arbiters disagree or stay silent, the refund settles it.

The buttons that misbehave use two kit calls built for this demo: `Zdlc::hold` (a node acts offline for one contract) and `Zdlc::attempt` (broadcast the refund early, or a payout completed with forged oracle signatures); the network refuses both.

## On testnet

| Run | Block |
| --- | --- |
| Rulings before funding: funding and close in one block | 4,547,229 |
| Team escrow, client offline: the contractor alone paid 0.0005 / 0.0003 / 0.0002 ZEC | 4,547,782 |
| Forged arbiter signatures | refused: `script verification failed` |

## In a real app

Each party runs its own node; the client funds from its shielded notes by deposit link; the arbiters are people or services both sides trust for the outcome (they can never take the funds). [docs/first-app.md](../../docs/first-app.md) builds this flow in about 100 lines.
