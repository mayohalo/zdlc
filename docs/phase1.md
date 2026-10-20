# Phase 1: stable-value ZEC payments

The Phase 1 gate passed on 2026-10-20 on Zakura 1.6.0 regtest, in the NU7 era. One stable-value contract shape settled at three prices and refunded once, and its cooperative close landed in Ironwood. Raw log: [spikes/phase1-regtest.json](spikes/phase1-regtest.json).

```sh
infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh
cargo test -p zdlc --release                                         # unit, offline, protocol
cargo test -p zdlc --release --test phase1 -- --ignored --nocapture  # the gate
```

## The contract

Alice is owed $100 in 30 days and holds 2 ZEC at $50.00. Bob, a hedger, puts up 2 ZEC against it. At maturity, Alice receives ZEC worth $100, capped by the 4 ZEC pot, and Bob takes the rest. Alice is made whole down to $25.00, and below that she takes the whole pot.

| Setting | Value |
| --- | --- |
| Oracle event | ZEC/USD in units of $0.10, 12 binary digits (up to $409.50), one nonce per digit |
| Payout rounding | 0.08 ZEC (2% of the pot) |
| CETs | 48, one per distinct payout |
| Adaptor signatures | 238 each way, one per digit prefix covering a payout range |
| Setup time | accept 184 ms, sign 206 ms, finish 26 ms (release, Apple Silicon) |
| Funding PCZT on the wire | 429 B |

## Gate run

| Contract | Price | Settled by | Alice / Bob (zat) | Height |
| --- | --- | --- | --- | --- |
| p60 | $60.00 | Cooperative close, P2SH → two Ironwood outputs | 167,995,000 / 231,995,000 after a 20,000 zat fee split evenly; Ironwood pool +399,990,000 | 215 |
| p40 | $40.00 | CET, completed by Alice alone | 248,000,000 / 152,000,000 | 220 |
| p20 | $20.00, below the floor | CET with a single output | 400,000,000 / 0 | 225 |
| silent | none | Refund, lock height 234 | 200,000,000 / 200,000,000; rejected at tip 233 ("locked until after block height 234") | 235 |

Every contract was funded by a two-party transaction in which each side signed only its own input, offerer first. All transactions signed under NU7's branch ID (`77190ad9`).

## How it is built

| Plan item | Where | Note |
| --- | --- | --- |
| 1. Dev oracle | `zdlc::oracle::NumericOracle`, `zdlc-oracle` CLI | rust-dlc's per-digit scheme: message = SHA-256 of the digit, signed with `dlc::secp_utils::schnorrsig_sign_with_nonce`. A prefix's point is the sum of digit points, and its key is the sum of digit scalars. The CLI records attested events and refuses a second attestation. |
| 2. Offer, accept, sign | `zdlc::contract` | Both sides derive every transaction from the messages. The funding txid comes from the unsigned PCZT (`into_effects` + `to_txid`), so CETs and the refund are signed before funding. The funding PCZT crosses as bytes. Enumerated contracts run the same flow (`tests/protocol.rs`). |
| 3. Numeric curve | `zdlc::payout` | `recipient(p) = min(pot, D / p)`, rounded; equal payouts grouped into ranges and covered by `dlc_trie::digit_decomposition::group_by_ignoring_digits`. Every outcome is covered exactly once (unit test). |
| 4. Outputs | `contract::build_txs`, `Contract::close_spec` | CETs and the refund pay transparent P2PKH. Their fee comes from a reserve in the funding output, so payouts are exact. The close pays Ironwood in one v6 transaction. The other side checks the close PCZT's outputs before signing and refuses any other amounts. |
| 5. Upgrade guard | `Terms::validate` | A contract whose refund height reaches the next activation is refused, for example "refund height 210 is at or after the activation at 200". |
| 6. Demo | `tests/phase1.rs` | The hedger funds from transparent on regtest. With shielded funds each party deshields first (three transactions; the deshields are linkable to the 2-of-2). That path is described, not built. |

## Demo

`zdlc-demo` runs the same contract on the regtest node with a chosen price and settlement path (`--settle close|cet|refund`) and narrates each step; see the README.

## Fixes after the Phase 0–1 review

| Issue | Fix | Test |
| --- | --- | --- |
| The close check trusted PCZT metadata: a builder could claim the agreed recipient and value while the note commitment paid someone else | `check_close` re-derives every Ironwood output's `cmx` from its note (`verify_note_commitment`) and trial-decrypts the checker's own output with its viewing key | `close_check_binds_metadata_to_the_note` forges the recipient metadata of a close through the PCZT encoding; it is refused with `InvalidExtractedNoteCommitment` |
| The close check ignored output index, expiry and lock time | Checks the funding outpoint (txid and index), no transparent outputs, lock time 0 and the agreed expiry | Same test: a close with another expiry is refused |
| Oracle nonces derived from the event: losing the CLI state then attesting again would leak the key | Nonces are random per announcement and stored by `zdlc-oracle announce`; `attest` refuses unannounced events and marks the event attested before printing signatures | `numeric_prefix_scalar_opens_prefix_point` checks fresh nonces; the CLI refuses an unannounced event and a second attestation |

Still open from the review: payouts round to the nearest grid point, so the recipient can receive slightly more than D (1.68 ZEC, $100.80, at $60.00); the close uses the empty-tree anchor; only regtest parameters exist.

## What this does not show yet

- **Trust:** one dev oracle signs everything, and parties trust it until Phase 3's two-enclave oracle.
- **Wire format:** messages are Rust values passed in-process; the wire encoding comes with the RFQ desk.
- **Watcher:** nothing watches the chain for a party who goes offline (Phase 2).
- **Message tagging:** the per-digit messages match rust-dlc's math. Compatibility with a live dlcspecs oracle's tagging is Phase 3's test.
