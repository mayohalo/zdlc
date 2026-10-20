# Phase 0: spike results

The Phase 0 gate passed on 2026-10-20 on Zakura 1.6.0 regtest. A refund was rejected one block before its lock height and confirmed right after it. An adaptor-completed CET confirmed. Raw log: [phase0-regtest.json](phase0-regtest.json).

```sh
infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh      # fresh chain, NU7 at 200
cargo test -p zdlc --release                                  # unit + offline
cargo test -p zdlc --release --test regtest -- --ignored --nocapture
```

## Gate run (one chain, heights as mined)

| Step | Height | Result |
| --- | --- | --- |
| Coinbase (P2PKH) spent straight into the 2-of-2 funding output | 111 | Accepted: Zakura regtest allows unshielded coinbase spends by default |
| Refund, lock height 116, sent at tip 115 | 115 | Rejected: "transaction is locked until after block height 116" |
| Same refund sent at tip 116 | 117 | Accepted, mined at 117 = lock + 1 |
| CET for "heads", Bob's adaptor signature decrypted with the oracle scalar | 119 | Confirmed; outputs 139,993,000 and 59,997,000 zat (70/30 of 2 ZEC less a 10,000 zat fee) |
| Cooperative close: P2SH in, two Ironwood outputs, NU6.3 | 121 | Confirmed; Ironwood pool 0 → 199,980,000 zat |
| CET signed for NU7 (77190ad9), sent before activation | 122 | Rejected: "transaction uses an incorrect consensus branch id" |
| Same contract's CET signed for NU6.3 (37a5165b), sent at tip 199 | 199 | Rejected: same error |
| CET signed for NU7, sent at tip 199 | 200 | Confirmed in block 200, the first NU7 block |
| Cooperative close under NU7 | 202 | Confirmed; Ironwood pool 199,980,000 → 399,960,000 zat |

## Answers

| Spike | Answer |
| --- | --- |
| Builder | pczt 0.10.0-pre.1 builds, signs and finalizes v6 P2SH 2-of-2 spends. The Builder makes the parts. zdlc sets `nExpiryHeight = 0` through `with_expiry_height`. For the refund, it rebuilds the transparent bundle with `Input::parse`, giving each input sequence `0xFFFFFFFE` and a required height lock time: the pczt Updater has no setter for either field. Sizes: CET 353–354 B, refund 353 B. |
| Adaptor path | `Signer::transparent_sighash` → `secp256k1-zkp` `encrypt_with_aux_rand` → `verify` at setup → `decrypt` with `s_m` → `append_transparent_signature`. `append_transparent_signature` verifies every signature. It returns `InvalidExternalSignature` for a key outside the script, a wrong message, a high-S twin, and the other outcome's adaptor decrypted with this scalar. libsecp256k1-zkp's decrypt already returns low-S: 0 of 64 trials came out high-S, plus 3 on-chain. zdlc normalizes anyway. |
| Ironwood | Yes: a v6 transaction with only transparent inputs can create Ironwood outputs, under both NU6.3 and NU7. A two-output close is 9,425 B, with about 0.9–2.6 s to build and prove (release, Apple Silicon). The anchor was the empty-tree root. |
| NU7 | A v6 CET is valid only under the branch it was signed for, in both directions, so one CET set covers one era. Signing for NU7 before activation works offline; the node accepts it once the next block is NU7. |
| Dependencies | rust-dlc v0.8.0 = commit `8e6a75fbc9685e6eafa348edd45a793fcb63fa4d` (also HEAD on 2026-10-20). `dlc-trie` 0.8.0 pulls in `dlc` 0.8.0, `bitcoin` 0.32 and `miniscript` 12. It builds beside pczt: `secp256k1` 0.29 (via `secp256k1-zkp` 0.11) and 0.33 (pczt) coexist, and keys and signatures cross as bytes. Toolchain: the locked `aes` 0.9.3 needs rustc 1.89, so the repo pins 1.97.1. |

## Facts measured

- **Fee:** a CET with one P2SH input and two P2PKH outputs pays 10,000 zat under ZIP 317 (2 logical actions at 5,000). The design note's 20,000 assumed the 10,000-per-action ceiling.
- **Lock time:** consensus checks `nLockTime < next block height`, so a refund locked at H enters the mempool at tip H and is mined at H + 1.
- **Coinbase on regtest:** `should_allow_unshielded_coinbase_spends` is not a config field in Zakura 1.6.0. Regtest already allows unshielded coinbase spends, and testnet and mainnet keep the shielding rule.

## Not covered

- Shielded CETs, held for weeks against the anchor rule, and proof cost at 100+ CETs: these are on the research track.
- Two-party Ironwood funding through one pczt.
- Message tagging compatible with a live dlcspecs oracle (Phase 3).
