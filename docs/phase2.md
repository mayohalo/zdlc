# Phase 2: safety and recovery

The Phase 2 gate passed on 2026-10-20 on Zakura 1.6.0 regtest, and again on Zcash testnet ([testnet.md](testnet.md)). Alice's wallet was restored from its 32-byte seed alone. It found her recovery record on chain, rebuilt the exact refund and confirmed it at the refund height, while Bob did nothing. Mainnet, the last Phase 2 item, has not started (see the end). Raw log: [spikes/phase2-regtest.json](spikes/phase2-regtest.json).

```sh
infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh
cargo test -p zdlc --release --test phase2 -- --ignored --nocapture
```

## Gate run

| Step | Height | Result |
| --- | --- | --- |
| Alice posts her recovery record: 10,000 zat to her own Ironwood address, the record in the encrypted memo | 202 | 215-byte record in a 512-byte memo |
| Funding broadcast, after the record | 203 | |
| Alice's state and keys are dropped; a wallet is rebuilt from the seed | | |
| Trial-decrypting 14 blocks finds the record | | 28 ms |
| Refund rebuilt from the seed and record, sent at tip 212 | 212 | Rejected: "locked until after block height 213" |
| Same refund sent at tip 213 | 214 | Confirmed: 2 ZEC to Alice's seed-derived payout address. Its txid equals the original refund's |

## Other Phase 2 results

| Case | Result |
| --- | --- |
| **Dual signing.** Funded at 112 under NU6.3 with refund height 215, so it spans NU7 at 200 | Without dual signing the terms are refused: "refund height 215 is at or after the activation at 200". With it: 96 CETs (48 per era), 450 adaptor signatures each way, refund signed for NU7. After activation the NU6.3 CET is refused ("incorrect consensus branch id") and the NU7 CET confirms in block 200 |
| **Refund racing a late CET.** The oracle attests after the refund height, so both are valid | Whichever reaches the mempool first wins: the refund went in, and the CET was refused because its input was already spent. There is no RBF, so the refund height must sit well after maturity plus the oracle's delay |
| **Dust.** An outcome paying Bob 30 zat | The CET drops the output and pays Alice the whole pot; it confirmed. A plain 30 zat output is refused as "transaction is non-standard" |
| **Free option.** Bob holds a fully signed funding transaction and waits | Its expiry (sign height + 40) ends the option: sent at the expiry height, it was refused ("must not be mined at a block … greater than its expiry"). Alice can also end it at once by spending her funding input; Bob's funding is then refused ("could not find transparent input UTXO") |
| **Counterparty stops before signing** | Until the offerer sends its signatures, the accepter has signed nothing that spends its coins; covered by the protocol order and not tested on chain |
| **Watcher.** Both parties abandon a contract after funding | `zdlc-watch` (or `watch::Kit`) holds only the signed refund. The test reloaded the kit from disk before every pass; it broadcast the refund at height 178, which confirmed at 179 |
| **Wrong branch ID, high-S** | Covered by Phase 0's chain tests and the offline tests: refused by the node and by the pczt Signer respectively |

## Design

| Piece | Where | Note |
| --- | --- | --- |
| Seed wallet | `zdlc::wallet` | Contract `i`'s funding, payout and coin keys and its Ironwood close key all derive from one seed (SHA-256 with domain tags; BIP-32/ZIP-32 paths later) |
| Recovery record | `zdlc::recovery` | Holds the funding outpoint and value, both collaterals, both payout key hashes, the refund and sign heights, the counterparty's funding key and its refund signature. Posted before funding. It cannot ride in the funding transaction, because the counterparty's refund signature commits to the funding txid |
| Seed-only restore | `recovery::scan`, `recovery::rebuild_refund` | Trial-decrypts Ironwood outputs for indices `0..gap` from a birthday height, then re-derives the keys and rebuilds the refund with `contract::refund_spec`, the same code that built it at setup |
| Watcher | `zdlc::watch`, `zdlc-watch` | A JSON kit holding the signed refund. It scans blocks for a spend of the funding outpoint, broadcasts the refund when due and survives restarts |
| Dual signing | `Terms::dual_sign`, `Terms::eras` | One CET set per era, and the refund signed for the era of the first block it can enter. Contracts spanning two upgrades are refused |

## Costs

- **Recovery record:** one extra transaction per party per contract: a transparent input paying 10,000 zat to Ironwood, plus its ZIP 317 fee.
- **Privacy of the record:** it links that party's coin to a shielded payment, not to the contract.
- **Dual signing:** it doubles the CETs and adaptor signatures, from 225 to 450 each way here.

## Not done

- **Testnet:** done. The refund, close and CET all ran on Zcash testnet with a 0.05 TAZ cap; see [testnet.md](testnet.md).
- **Mainnet with a per-contract cap:** not started. It moves real funds, so it waits for an explicit go-ahead and a cap.
- **Record ordering.** Each party must see its own record confirmed before the funding goes out. The offerer's signatures are already out by then, so it cannot gate the accepter's broadcast. The offerer should post its record before sending its signatures; the protocol does not enforce this yet.
- **Recovery beyond the refund.** Recovery finds the refund only. A restored wallet cannot complete CETs or a close; that would need the counterparty's adaptor signatures, which are too large for a memo.
