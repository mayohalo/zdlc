# Phase 4: the RFQ desk and products

The desk is built and in use. Two independent agents (separate processes and seeds, meeting only through the desk, the oracles and the chain) traded through it on regtest and Zcash testnet:
- by public RFQ, settling by cooperative close and by CET;
- by private memo invitation.

Milestone escrow and payroll rolled on chain both settled on regtest. The gate, two counterparties other than us trading through the desk, needs outside traders; everything they would run is in place. Raw logs: [desk trades](spikes/phase4-regtest.json), [invitation](spikes/products-invite.json), [escrow](spikes/products-escrow.json), [payroll](spikes/products-payroll.json), [testnet](spikes/phase4-testnet.json).

## The desk

| Part | Where | What it does |
| --- | --- | --- |
| Quote board | `zdlc-desk` (`POST/GET /rfqs`, `/rfqs/{id}/quotes`) | Signed RFQs and signed firm quotes. With `--node` the desk checks that every quoted funding input is an unspent output of its key; expired quotes drop off |
| Firm, self-expiring quotes | `desk::QuoteBody` | A quote names the maker's funding UTXOs (reserved for it) and the height it expires at. The taker re-checks both; the maker refuses offers after expiry; the funding transaction expires 40 blocks after signing |
| Sealed mailbox | `POST /mailbox`, `GET /mailbox/{identity}` | Offer, accept, sign, funded, close and close-signed travel as envelopes. Each is signed by the sender's identity key and encrypted to the recipient's (secp256k1 ECDH, SHA-256 KDF, ChaCha20-Poly1305). The desk sees recipient, size and timing, never content or sender. Reading needs a fresh signature by the owner |
| Wire format | `zdlc::wire` | Offer, accept and sign as JSON; keys, signatures, adaptor signatures (162 B each) and the funding PCZT in hex |
| Memo invitations | `zdlc::invite` | A maker posts an unlisted quote and pays the taker's Ironwood inbox 10,000 zat. The encrypted memo (113 B) carries the quote id, terms, maker identity and desk. The taker finds it by trial decryption and trades without any public request |
| Agents | `zdlc-trader maker`, `zdlc-trader taker` | The maker quotes acceptable RFQs (its oracle pair, under its cap) at a premium. It checks every offer against its quote and that the announcements are its oracles' own, and co-signs closes only after re-reading the attestations itself. The taker picks the cheapest live quote, runs setup and settles by close or CET |

## Regtest runs

| Run | Result |
| --- | --- |
| Two trades by RFQ | The maker quoted 0.1 ZEC at a 0.0005 ZEC premium with one firm input. The taker offered, the maker accepted (98 CETs), and the taker verified 485 adaptor signatures and signed. The maker funded. Both oracles agreed on $1,148, and the close was co-signed and paid into Ironwood (block 239). The second trade settled by CET at $1,147 (block 264). Total 60 s, mining a block a second |
| Private trade by invitation | The taker was given no desk address and posted nothing. It found the maker's invitation (mined at 227) in its inbox, fetched the unlisted quote from the desk named in the memo, checked it against the memo, traded and closed to Ironwood (block 249) |
| Milestone escrow | The client funded 1.5 ZEC alone; the contractor brought nothing. Two arbiter oracles (`EventSpec::Arbiter`, resolved through an admin-token endpoint) both resolved "delivered". The contractor took the 1.5 ZEC with the CET (block 213). A conflicting second decision and a wrong token were refused |
| Payroll rolled on chain | Month 1 (1 ZEC a side, $1,000 owed at $1,000) attested $800: credits 1.24 / 0.76 ZEC. One transaction (block 218) spent month 1's 2-of-2 and opened month 2, each side signing the old output with its month-1 key. It paid the employee 0.2399 and the employer 0.2599 ZEC (P&L net of costs), and the employer topped up from a fresh coin. Month 2 settled at $1,000 by CET (block 221) |

## Testnet run

Two agents with their own seeds and treasuries (each funded with 0.03 TAZ) traded through a local desk on Zcash testnet (NU7), on the two Phase 3 oracles.

| Step | Height | Result |
| --- | --- | --- |
| Taker posts an RFQ for 0.01 TAZ; maker quotes 0.01 TAZ at a 0.00005 TAZ premium, one firm input, valid to 4,522,181 | | Quote verified by the desk against the chain before listing |
| Offer, accept, sign over the sealed mailbox | | Alice owed $11.52 (spot $1,152.93); 98 CETs, 530 adaptor signatures verified |
| Maker funds | 4,522,142 | |
| Both oracles attest ZEC/USD for minute 1792500600 | | $1,149, in agreement; owed 0.01 / 0.01005 TAZ |
| Taker builds the close; maker re-reads the attestations, checks and co-signs; taker signs and broadcasts | 4,522,155 | Paid into Ironwood. `cd366082…` |

Testnet stalled for a while during the run, and the agents simply waited.

## Changes underneath

- **Premium:** `StableValue::premium` is paid by the recipient inside its collateral and always goes to the hedger.
- **Single-funder contracts:** a party may bring no inputs and no collateral (escrow, commitment contracts).
- **Rolling:** `Terms::roll` with `Party::roll_key` spends the previous contract's funding output inside the new funding transaction.
- **Sign-height slack:** `SIGN_SLACK` lets a contract's sign height be up to 10 blocks old when a party checks the terms, since setup spans several messages.

## Not done

- **The gate itself:** both agents are ours. The gate is met when two outside parties run `zdlc-trader` against a public desk.
- **TLS and hosting:** the desk speaks plain HTTP. Put it behind a TLS terminator on a public host.
- **Rolling over the desk:** `wire` refuses rolled terms, so payroll rolls are negotiated in-process. Agent support comes next.
- **Agent outcome payments:** these use the shielded-payment event from Phase 3 (shown there as a repaid loan). They are not yet wrapped as a desk product.
- **Recovery records for agents:** the agents do not post them yet. `zdlc-live` does.
