# Hedge desk: trade a price

Two software agents trade a ZEC/USD hedge with nobody in the loop after the visitor's click. The taker wants today's dollar value of 0.001 ZEC; a house maker takes the other side of the price risk for a 0.5% premium.

Live at `/hedge` on the hosted site.

## How it works

| | |
| --- | --- |
| Template | `stable_payment` with 5% payout rounding (40 payout transactions instead of 98): the taker is owed a dollar amount in ZEC at a price minute |
| Parties | `taker` (offerer: the visitor's agent) and `maker` (accepter: the house agent), two kit nodes |
| Oracles | two `zdlc-oracled` price oracles: the median of four exchanges' ZEC/USD close for the minute |
| Market | the desk's RFQ board: signed requests and quotes in public, the offer itself in a sealed mailbox box |

1. **Request.** The taker posts a signed request for quotes (`agent::run_taker_with`).
2. **Quote.** The maker's loop (`agent::run_maker`) quotes every acceptable request and records a standing quote for that taker.
3. **Offer.** The taker offers on the cheapest quote, sealed to the maker through the desk's mailbox.
4. **Accept.** The maker's policy accepts offers that match its standing quote, funded from its treasury; both sign every payout and the refund.
5. **Price.** The price minute is the one in progress at the click; the oracles attest it while the funding is mined.
6. **Settle.** The taker asks for the cooperative close as soon as the price is attested (`close_before_funded`); the payout splits at that price into shielded notes.

## The code

- Server: [`crates/zdlc-kit/src/bin/zdlc-room/hedge.rs`](../../crates/zdlc-kit/src/bin/zdlc-room/hedge.rs)
- The agents: [`crates/zdlc-kit/src/agent.rs`](../../crates/zdlc-kit/src/agent.rs) (also the standalone `zdlc-agent maker | taker`)
- Page: [`pages/hedge.html`](../../crates/zdlc-kit/src/bin/zdlc-room/pages/hedge.html)
- API: `GET /hedge/api/info`, `GET /hedge/api/state`, `POST /hedge/api/trade`. One trade at a time; every visitor watches the same one.

## On testnet

| Run | Block |
| --- | --- |
| ZEC/USD attested at $1,265; funding and close in one block | 4,547,436 |
| An earlier hedge between two agents, closed cooperatively | 4,522,155 |

A trade is about two minutes when testnet includes transactions promptly: setup in under 10 s, the price within a minute, then one block for funding and close together.

## In a real app

The maker is a market maker's own agent with its own node and pricing; the taker is a user's wallet or app. Run them with `zdlc-agent`, or embed `agent::run_maker` and `agent::run_taker` in your service.
