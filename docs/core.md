# zdlc core and tooling

## Core and Tooling

### zdlc library: a reuse map from rust-dlc

Most of zdlc already exists in rust-dlc; only the transaction layer is Bitcoin-specific. Its manager builds on `secp256k1_zkp::EcdsaAdaptorSignature` and `dlc-trie` for numeric outcomes ([dlc-manager source](https://docs.rs/crate/dlc-manager/latest/source/src/contract/numerical_descriptor.rs)).

| Crate | What it does | For Zcash |
| --- | --- | --- |
| [dlc-trie](https://docs.rs/dlc-trie/latest/dlc_trie/) | Digit decomposition, prefix compression, t-of-n multi-oracle tries | As is; chain-agnostic |
| secp256k1-zkp adaptor module | ECDSA adaptor sign, verify, decrypt, recover | As is, plus low-S normalization |
| payout curve (in dlc-manager) | Piecewise payout functions, rounding intervals | As is |
| dlc-messages | Offer, accept, sign, oracle announcement and attestation formats | Keep; add network, branch ID and refund-era fields |
| dlc | Funding, CET and refund construction, sighash | Replace with pczt (v6): P2SH 2-of-2 in place of P2WSH, ZIP 317 fees, refund lock time, nExpiryHeight 0 |
| dlc-manager | Contract state machine, storage, chain monitoring | Port; swap the Bitcoin chain source for Zakura's JSON-RPC |

The [ddk-trie](https://docs.rs/crate/ddk-trie/latest) crate from DLC Dev Kit is an alternative base with parallel signing. ztxo-core encodes only v5, so it is a reference for the upgrade guard and fee code, not a dependency.

### TEE oracle network

Each of two TEE enclaves is its own BIP-340 oracle, and contracts require both (2-of-2 multi-oracle). One compromised enclave can then only withhold, which leads to the refund, never sign a false outcome.

- **Nonce safety.** An oracle that signs two outcomes with one R leaks its key. Each enclave keeps a sealed, monotonic nonce counter and refuses to sign an event twice.
- **Events.** ZEC/USD and other prices; Solana program state; Hyperliquid prices via HyperDesk; and "this Ironwood payment arrived", read with a viewing key by the scanner. The last one makes contracts conditional on a shielded payment.
- **Bitcoin oracle compatibility.** The attestation scheme is chain-agnostic, so existing dlcspecs oracles should work as-is. Most of them attest BTC/USD, though, so they help with BTC-referenced contracts settled in ZEC; a ZEC/USD feed needs this network.

### Ironwood payout check (spike)

A short spike on Zakura regtest that answers whether shielded payouts are viable, and how expensive.

- [ ] Can a v6 tx with only transparent inputs create Ironwood outputs? ZIP 258 bars Orchard inflows; Ironwood's rule is unconfirmed.
- [ ] Does a fully built and proven CET with nExpiryHeight 0 stay valid for weeks? Check the anchor rule for its dummy spends.
- [ ] Seconds and bytes per shielded CET on target hardware, then for 100, 500 and 2,000 CETs.
- [ ] Can one party build both outputs and the other verify its own note, ciphertext included, from the plaintext?
- [ ] Is a v6 CET signed under NU6.3's branch ID (37a5165b) rejected once NU7 activates, and the same CET signed under NU7's (77190ad9) accepted?
- [ ] Write up the answers, with the decision on shielded CETs.
