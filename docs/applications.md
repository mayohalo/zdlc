# zdlc applications

## Shielded Payments

### Stable-value ZEC payments

The party who needs stability is whoever holds ZEC until a future date, so this is a deferred payment, not an instant one. An immediate payment at spot needs no DLC; an invoice due in 30 days, a monthly salary or a remittance the recipient wants to hold does.

**How it works.** The recipient's ZEC (q_r) and a hedger's ZEC (q_h) fund the contract on the ZEC/USD price. At maturity price p, the recipient receives ZEC worth the agreed USD amount D, capped by the collateral:

```latex
\text{recipient}(p) = \min\!\left(q_r + q_h,\ \frac{D}{p}\right) \qquad \text{hedger}(p) = q_r + q_h - \text{recipient}(p)
```

- **Protection range.** With q_h = q_r, the recipient is made whole down to a 50% price fall; below that it takes the whole pot. More hedger collateral widens the range.
- **The hedger's side.** It keeps all upside above D, so it is long ZEC at about 2x. A market maker on the RFQ desk prices this and charges a premium in the funding split.
- **Oracle event.** One numeric ZEC/USD attestation at maturity, digit-decomposed. Price in cents over 20 binary digits covers up to $10,485.76.
- **Rolling it.** Payroll rolls monthly: each month's close funds the next month's contract (on chain; see the CSV note under What Zcash changes).

### Milestone escrow

A client funds the contract alone, so funding is simple. Outcomes are enumerated: delivered or not delivered.

- **Oracles.** A TEE or zkTLS oracle reading GitHub (pull request merged into a named branch) or a carrier's tracking API. For subjective work, a human arbiter can act as the oracle and still never touch the funds.
- **Several milestones.** One contract per milestone. One contract over several events multiplies the CET count.
- **The refund is not neutral.** If the oracle goes silent, the refund returns the client's money, so oracle failure always favours the client. The contractor may ask for a split refund, or for the refund height to sit well after the delivery deadline.

### SLA-backed subscriptions

The customer prepays a month and the provider stakes alongside it. An uptime oracle attests availability in basis points (0 to 10,000), and the payout curve refunds the customer in proportion to downtime, capped by the provider's stake.

- **Shape.** One contract per billing period; on-chain cost is one funding and one close per month, at most a few tens of thousands of zatoshi.
- **Oracle.** An independent prober, ideally in a TEE, that measures from several regions and signs the monthly figure. The provider must not run it.

## Private Markets

### DLC RFQ desk

The desk is a matching board that never holds funds: a quote is a dlcspecs offer, and acceptance runs the offer, accept, sign flow directly between the two parties.

1. **Request.** A taker posts underlying, notional, expiry and payoff type (forward, call or put spread, binary).
2. **Quote.** Makers reply with full contract terms: payout curve, oracle announcement, each side's collateral.
3. **Accept.** The taker picks one; both run setup over the desk's transport (G8 over MLS, or Nostr).
4. **Fund.** The funding tx confirms and the desk drops out.

- **Fully collateralized.** There are no margin calls, so every payoff is bounded: spreads, capped forwards and binaries suit it; naked options do not.
- **What the desk learns.** Quotes and who accepted. Blinded requests are a later step.
- **Firm quotes.** A quote is only firm once the maker's funding inputs are reserved; otherwise it is an indication.

### Rolling ZEC/USD forwards

The original note said each roll settles cooperatively with no extra on-chain transactions. That needs a DLC channel, and dlcspecs channels rely on CSV, which Zcash lacks. Two workable versions:

| Version | Each roll | On-chain cost | Privacy | Status |
| --- | --- | --- | --- | --- |
| On-chain roll | One tx spends the old funding into the new one, netting P&L | About 10,000 to 20,000 zat per roll; six rolls a day at 4-hour periods | A visible chain of linked 2-of-2 outputs | Buildable now |
| Fixed-lifetime channel | New CETs off the same funding; old states revoked by penalty | None until close | One open, one close | Needs absolute-height revocation, in the manner of ztxo's height ladder; later |

Start with the on-chain roll. Shorter periods trade fees for less counterparty exposure per period, and Zcash's flat fee keeps that trade cheap.

### Peer-to-peer prediction contracts

Binary or enumerated events, with the oracle mirroring a public resolution: a sports result, an election call, or a Polymarket market's final resolution.

- **What stays hidden.** The outcome is public; who bet, how much and at what odds are not, as long as contracts close cooperatively.
- **Matching.** The RFQ desk or a G8 group. Liquidity is bilateral, so this suits a small number of larger bets better than an order book.

## Cross-Chain

### Borrow USDC against native ZEC

The borrower funds the contract alone with ZEC; the lender sends USDC on Solana. The hard part is linking the two chains, and the oracle does it by attesting Solana state.

| Outcome | Attested by | ZEC goes to |
| --- | --- | --- |
| Repaid | Oracle sees repayment in a Solana loan program | Borrower, all of it (or a cooperative close, skipping the oracle) |
| Liquidated | Oracle sees ZEC/USD cross the threshold before maturity | Lender: ZEC worth the debt plus a penalty; borrower: the rest |
| Matured unpaid | Oracle sees no repayment by maturity | Lender, as for liquidation |

- **Liquidation timing.** A DLC settles only when the oracle signs. A single barrier event ("first crossing below L before maturity") lets the oracle sign the moment it happens, instead of one event per period with its own CET set.
- **Gap risk.** Price can fall past the threshold faster than the attestation lands; size the loan-to-value with that delay in mind.
- **The refund.** If the oracle never signs, a refund to the borrower costs the lender its USDC, and a refund to the lender is confiscation. A split refund at a late height is the usual compromise; it must be agreed upfront.

### Settle Solana outcomes on Zcash without custody

Both players fund a contract; two TEE enclaves attest the result of a Solana program, such as a game outcome or a randomness draw. Treat the two enclaves as two independent oracles in a 2-of-2 multi-oracle contract: the adaptor point is S_1 + S_2, so no threshold Schnorr is needed, and dlc-trie already handles multi-oracle sets.

Best fit: head-to-head matches with meaningful stakes, played as one contract per match. Play starts once funding confirms, about 12 minutes at 10 blocks today and about 4 after NU7. Capital is locked per pair and both players must be online to sign every CET, so quick repeated games and pots with many players need a pooled design.

### Hyperliquid-referenced contracts

HyperDesk's feeds attest Hyperliquid prices; contracts settle in ZEC. The simplest form is a quanto: the payout in ZEC is a fixed multiple of the price move, capped by collateral. That needs no ZEC/USD feed at all, and the maker carries the currency risk. It gives shielded ZEC holders BTC, ETH or HYPE exposure without leaving Zcash.

## Wildcard

### Commitment contracts

A DLC needs a second signer, so the counterparty is a commitment service that stakes nothing and only co-signs. You fund alone; the success CET pays you, the failure CET pays the charity's address, and the refund pays you.

- **Oracle.** zkTLS over Strava or GitHub, or a TEE reading their APIs, attesting the goal at the deadline.
- **The service's power.** It can refuse to set up, nothing more; it never holds the stake.
- **The weak spot.** A refund to you means a silent oracle lets you off the hook, so the oracle must be one you cannot influence.

### Group-chat bets

Multi-party DLCs need every member to sign every CET, so group bets work best as pairwise contracts, or one bookmaker against each member. The G8 chat carries the offer, accept and sign messages over MLS; a bot posts the oracle's attestation and each bet's result into the thread.

### Fantasy Trader League, settled on Zcash

Fantasy leagues already play weekly head-to-head matchups, which is exactly a two-party contract. The oracle attests both traders' weekly profit and loss (or simply the winner), and each matchup settles as one contract; season prizes for N players need a pooled design, since a DLC has two parties.

## Zcash-native extensions

These use features Bitcoin lacks, and all of them work on tier A today.

| Extension | Mechanism | Unlocks |
| --- | --- | --- |
| Private-payment oracle | An oracle holding a viewing key attests that X ZEC reached a shielded address by height H | Escrow where delivery is a payment, refunds on non-payment, loans repaid on Zcash, agent outcome payments |
| Memo recovery record | The funding tx's change note carries terms, key index and the counterparty's refund signature in its encrypted memo | Seed-only recovery: collateral always returns at the refund height |
| Memo invitations | A small shielded payment whose memo carries a contract reference and negotiation bootstrap; the fee doubles as spam resistance | Private OTC offers with no public order board |
| Self-expiring quotes | The maker pre-signs its funding with a short nExpiryHeight | Firm RFQ quotes without locking inputs for long |
| Selective disclosure | A viewing key or payment disclosure shown to one party | Audits, insurance claims, reserve proofs for a DLC desk |

### Further applications

- **Parametric insurance.** The insurer is the counterparty; the CET pays the policyholder when rainfall, a flight delay or another measured value crosses a threshold. Weather suits a 2-of-3 multi-oracle set, and the terms never leave the two parties.
- **Agent outcome payments.** An agent locks payment against a deliverable; the oracle attests success or a quality score and the payout follows a curve. Pairs naturally with private x402 payments, using the private-payment oracle where the deliverable is itself a payment.
- **Transferable positions.** Research. Handing over your contract key does not transfer the right, because the seller keeps a copy. The workable form is novation: the counterparty co-signs a fresh CET set to the buyer.
- **Streaming DLCs.** Blocked like rolling forwards: updating a payable amount in place needs a channel, and channels need CSV or an absolute-height redesign.
