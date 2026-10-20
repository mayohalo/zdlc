# Zcash testnet runs

On 2026-10-20 the stable-value contract ran three times on Zcash testnet and settled every way it can: seed-only refund, cooperative close to Ironwood, and CET. All transactions signed under NU7 (branch `77190ad9`) and were broadcast through a pruned Zakura 1.6.0 testnet node's JSON-RPC. Raw logs: [refund](spikes/testnet-refund.json), [close](spikes/testnet-close.json), [CET](spikes/testnet-cet.json).

## Setup

| Item | Value |
| --- | --- |
| Wallet | One 32-byte seed outside the repo (`examples/wallet.rs`); index 0 is the treasury, each run takes two fresh indices for Alice and Bob |
| Funding | 0.125 TAZ from the Valar testnet faucet to the treasury's transparent address |
| Contract | Alice is owed $0.50 and holds 0.01 TAZ at $50.00; Bob hedges with 0.01 TAZ; floor $25.00 |
| Collateral cap | 0.05 TAZ a side (`params::TESTNET_CAP`), checked before setup |
| Curve | 12 binary digits in $0.10 units, payouts rounded down to 0.0002 TAZ (1% of the pot): 95 CETs, 371 adaptor signatures each way, setup about 0.7 s |
| Ironwood anchor | The node's current Ironwood root from `z_gettreestate`, for the recovery records and the close |

## Results

| Run | Path | Heights | Result |
| --- | --- | --- | --- |
| refund | Oracle silent; Alice keeps only her seed; Bob does nothing | Record 4,521,971, funding 4,521,976, refund height 4,521,977 | A wallet rebuilt from the seed scanned 11 blocks, found its record and rebuilt the identical refund. Sent at tip 4,521,976 it was rejected ("locked until after block height 4521977"); it was mined at 4,521,979 and paid 0.01 TAZ to Alice's seed-derived address. [`13caad64…`](spikes/testnet-refund.json) |
| close | Oracle attests $60.00 | Funding 4,521,978, close 4,521,980 | Owed 0.0082 / 0.0118 TAZ (Alice's $0.50 is 0.00833, rounded down). Bob checked the close (note commitments, his own output decrypting), both signed, and the close paid Ironwood 0.00815 and 0.01175 TAZ after a 10,000 zat fee split evenly. [`0f662f85…`](spikes/testnet-close.json) |
| cet | Oracle attests $40.00; Bob goes silent | Funding 4,521,980, CET 4,521,988 | Alice decrypted Bob's adaptor signature for the matching price prefix (CET #38) and settled alone: 0.0124 / 0.0076 TAZ. [`e21742c7…`](spikes/testnet-cet.json) |

Each run took 3.5 to 6 minutes of wall time, mostly spent waiting for testnet blocks. `zdlc-testnet sweep` then returned 0.053 TAZ from 13 leftover transparent coins and payouts to the treasury in one transaction.

## Costs

Of the 0.125 TAZ, 0.1024 is back in the treasury. About 0.0203 sits in Ironwood notes the seed controls: the two close outputs (0.0199) and three recovery-record notes (0.0001 each). zdlc cannot spend Ironwood notes yet, but a shielded wallet with the seed-derived keys can. Fees came to roughly 0.0024 TAZ across 13 transactions: 3 splits, 3 records, 3 fundings, 1 refund, 1 close, 1 CET and 1 sweep.

## What testnet changed in the code

- `params::testnet()`: the pinned zcash_protocol's testnet heights (NU6.3 at 4,134,000, NU7 at 4,465,026). The runner refuses a node whose next-block branch ID differs from zdlc's.
- `Node::utxos` (`getaddressutxos`), `wait_height` and `wait_mined`: testnet has no `generate`.
- `Node::ironwood_anchor`: a current anchor from `z_gettreestate` replaces the empty-tree root (open issue 5 of the Phase 0–1 review).
- `Terms::check_cap`: a per-contract collateral cap.
- `zdlc-testnet status | run --settle close|cet|refund | sweep` (since renamed `zdlc-live`, which also runs mainnet and two-oracle events). The RPC endpoint comes from `--node` or an environment variable and is never stored.

## Limits

- **Recovery scan:** the node is pruned, so a restored wallet must scan from a recent birthday.
- **One wallet plays both sides:** Alice and Bob are separate indices of one wallet. A run between two machines needs the wire format (Phase 4).
- **Mainnet:** not started; it waits for an explicit go-ahead and a mainnet cap.
