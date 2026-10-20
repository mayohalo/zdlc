# Phase 3: oracle network

Phase 3 built the 2-of-2 oracle network and ran it on regtest and Zcash testnet. A contract settles only when two independent oracles attest the same outcome, and the attestations alone (Bob silent) are enough for Alice to settle. The mainnet gate is ready to run but moves real funds, so it is left to the operator (see the end). Raw logs: [regtest](spikes/phase3-regtest.json), [testnet](spikes/phase3-testnet.json).

## What was built

| Plan item | Where | Status |
| --- | --- | --- |
| Two oracles as a 2-of-2 multi-oracle | `contract::Event::{Numeric, Enumerated}(Vec<…>)` | Every adaptor point is the sum of the oracles' points; a CET opens only with every oracle's scalar for the same outcome. One oracle alone, or two that disagree, opens nothing, and the contract refunds |
| Enclave oracle with a sealed nonce counter | `zdlc::enclave`, `zdlc-oracled` | Key, nonce seed, counter and event records in one sealed blob (ChaCha20-Poly1305 under a sealing key), written before any nonce or signature leaves. Each outcome is bound before signing; repeats re-sign identically. **Runs as two isolated processes with a file sealing key; no hardware enclave yet** |
| ZEC/USD feed | `zdlc::feeds` | Close of one fixed UTC minute on Coinbase, Kraken, Bitstamp and Binance (ZEC/USDT); lower median of those that answer, at least two |
| Shielded-payment attestations | `feeds::received`, `EventSpec::Payment` | The oracle holds the payee's Ironwood viewing key and attests "paid" if notes in a height range sum to at least the amount |
| dlcspecs compatibility | `tests/compat.rs` against kormir 1.1.2 | See below |
| Verifying client | `zdlc::oracle_client` | Each signature checked as BIP-340 by the announced key, under the announced nonce, on the tagged outcome, before use |

## Findings

- **zdlc used the wrong message format.** dlcspecs oracles (kormir, and through it DLC Dev Kit) sign the tagged hash `SHA256(t‖t‖outcome)` with `t = SHA256("DLC/oracle/attestation/v0")`. zdlc signed plain SHA-256, so a zdlc contract could not have settled on any standard oracle. zdlc now uses the tagged form for enumerated outcomes and for each "0"/"1" digit.
- **No public dlcspecs oracle answered.** suredbits, p2pderivatives, kormir.dlcdevkit.com and lava were down or gone, and signed price APIs such as Mycelia Signal are paid and pre-commit no nonces. The compatibility test therefore runs a maintained dlcspecs implementation (kormir) locally rather than a hosted oracle.
- **Two oracles agree on a closed minute.** Both oracles fetched the same minute independently and got identical closes from all four sources, so they attested the same value. Disagreement is still possible (a source failing for one oracle), and it ends in a refund, never in a wrong payout.
- **The pinned library has no mainnet NU7 height.** `zdlc-live` takes activation heights from the node (`params::from_node`), so the upgrade guard and branch selection follow what the node enforces.

## dlcspecs compatibility (kormir 1.1.2)

| Direction | Result |
| --- | --- |
| kormir → zdlc, numeric | A zdlc contract on a kormir BTC/USD announcement (17 digits) settled on kormir's attestation of $96,000: CET 14 of 17, payouts matching the curve |
| kormir → zdlc, enumerated | Settled on kormir's attestation of "alice" |
| zdlc → dlcspecs | A zdlc oracle attestation validates as a dlcspecs `OracleAttestation` against a dlcspecs announcement of the same event (`OracleAttestation::validate`) |
| Mixed 2-of-2 | One kormir oracle and one zdlc oracle settle one contract; either alone, or a disagreeing pair, opens nothing |

## Regtest run (two `zdlc-oracled` processes)

| Case | Result |
| --- | --- |
| ZEC/USD, 2-of-2, CET | Both oracles attested minute 1792497240 at $1,136.38 (Coinbase 1,135.66, Kraken 1,137.35, Bitstamp 1,136.63, Binance 1,136.38; lower median), value 1136. CET 56 of 98 (485 adaptor signatures each way) paid 1.76 / 2.24 ZEC as owed; mined at 213. One attestation alone opened nothing |
| Shielded payment, 2-of-2 | A loan-style contract: 0.5 ZEC to Bob's Ironwood address by height 220 returns Alice's collateral. Alice paid at 217; neither oracle attested before the deadline; both then saw 50,000,000 zat and attested "paid"; Alice's CET paid her the 4 ZEC pot at 221 |
| Sealed state | After a restart oracle A had the same key and nonce counter (15); a repeated attestation request returned identical signatures; the state file is not readable without the sealing key |

## Testnet run

Two `zdlc-oracled` services, each with its own sealed state, settled a stable-value contract on Zcash testnet (NU7) with no help from Bob.

| Step | Height | Result |
| --- | --- | --- |
| Both oracles announce ZEC/USD for minute 1792497720 (two minutes ahead) | 4,522,018 | 14 nonces each, keys `0f4f2ddd…` and `3046b16a…` |
| Setup on the two announcements | | Alice is owed $11.35: her 0.01 TAZ at the $1,135.89 spot; floor $567.50. 98 CETs, 481 adaptor signatures each way, 1.6 s |
| Recovery record, then funding | 4,522,025, 4,522,029 | |
| Both oracles attest, independently | | Both attested $1,138 from the same four closes (Coinbase 1,138.32, Kraken 1,138.70, Bitstamp 1,139.41, Binance 1,140.77) |
| Bob silent; Alice settles with the CET on the two attestations alone | 4,522,041 | CET #51: 0.0098 TAZ to Alice ($11.15 at $1,138, rounded down) and 0.0102 TAZ to Bob. [`612ba745…`](spikes/phase3-testnet.json) |

The run took 11 minutes, most of it testnet block time.

## The mainnet gate

The gate is a mainnet contract that settles on enclave attestations alone. It is ready to run: `zdlc-live` refuses a mainnet node without `--mainnet`, caps collateral at 0.01 ZEC a side (default 0.001), and takes upgrade heights from the node. It was not run here, because it moves real funds; the operator runs it:

```sh
export ZDLC_RPC=<mainnet Zakura RPC URL>
cargo run -p zdlc --release --example wallet -- mainnet    # fund the printed t1… address with about 0.005 ZEC
# two oracles, each ideally on its own machine
cargo run -p zdlc --release --bin zdlc-oracled -- --listen 127.0.0.1:7801 --state a.sealed --sealing-key a.key
cargo run -p zdlc --release --bin zdlc-oracled -- --listen 127.0.0.1:7802 --state b.sealed --sealing-key b.key
cargo run -p zdlc --release --bin zdlc-live -- run --mainnet --settle cet --oracles http://127.0.0.1:7801,http://127.0.0.1:7802
```

## Not done

- **Hardware enclaves.** The oracles are ordinary processes with a file sealing key. The next step is to build `zdlc-oracled` into an enclave image (for example AWS Nitro), take the sealing key from KMS bound to the image measurement, and publish the attestation document that binds each oracle key to its measurement. Until then the two oracles are trusted operators, not enclaves.
- **Disagreement handling.** Disagreement only refunds. rust-dlc's multi-oracle tries allow a bounded difference between oracles; zdlc does not use them yet.
- **Announcement signatures.** zdlc announcements are JSON without a dlcspecs announcement signature or TLV encoding. Wire formats come with the RFQ desk (Phase 4).
- **Viewing-key privacy.** Payment oracles learn every incoming payment to the viewing key they hold. A dedicated receiving address per contract limits what they see.
