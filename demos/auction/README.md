# Auction: discover a price

A sealed-bid, second-price auction for one lot. Bids stay sealed; only the clearing price goes public, and every contract settles into shielded notes alike, so the chain cannot tell the winner from the losers.

Live at `/auction` on the hosted site.

## How it works

| | |
| --- | --- |
| Template | `auction`: one contract per bidder with the seller. The bidder locks the deposit (the same for everyone, 0.002 ZEC); the seller is paid the clearing price on "won at" it, nothing on "lost" |
| Parties | `seller` (offerer) and `bidders` (accepter for all four bidders: three house bidders and the visitor), two kit nodes |
| Oracles | two arbiter oracles; the server, as auctioneer, has them rule each contract |
| Prices | ten steps up to the deposit, so each contract pre-signs eleven payouts: "won at" each price, plus "lost" |

1. **Bid.** The visitor picks a price; three house bids are drawn at random. The bids go to the auctioneer only.
2. **Lock.** The seller offers each bidder a contract; the bidders' node accepts all four from its treasury, each with the same deposit.
3. **Clear.** Once all four are signed, the auctioneer opens the bids: the highest wins and pays the second-highest.
4. **Rule.** Both oracles rule each contract: "won at P" for the winner, "lost" for the others.
5. **Settle.** All four close cooperatively into shielded notes, often in the same block as their funding. The clearing price goes on the public tape.

## The code

- Server: [`crates/zdlc-kit/src/bin/zdlc-room/auction.rs`](../../crates/zdlc-kit/src/bin/zdlc-room/auction.rs) (`run_round` is the whole round)
- Template: `Template::Auction` in [`crates/zdlc/src/templates.rs`](../../crates/zdlc/src/templates.rs)
- Page: [`pages/auction.html`](../../crates/zdlc-kit/src/bin/zdlc-room/pages/auction.html)
- API: `GET /auction/api/info`, `GET /auction/api/state`, `POST /auction/api/bid {"bid_zat": …}`

## On testnet

| Run | Block |
| --- | --- |
| Bids 0.001, 0.0002, 0.0018, 0.0014 ZEC; cleared at 0.0014; four shielded closes | 4,547,603 |

## What it does not hide yet

- **The auctioneer sees the bids.** It can never touch the funds, but it is trusted for bid secrecy and for clearing honestly. Next: commit-reveal, where bidders post hashes of their bids and reveal after the deadline, so anyone can recompute the clearing price; the bids then become public but stay anonymous.
- **In the demo, all four deposits come from one wallet**, so the chain can link them to each other (not to a bid or the win). Real bidders each fund from their own shielded notes with a deposit link.
