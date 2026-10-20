# Multi-beneficiary payouts

A contract still has two signers, but each outcome can now pay any number of beneficiaries (up to 16). Beneficiaries never sign; they only give addresses. The two signers agree the split at setup, and every CET and the cooperative close pay it. The refund ignores the split and returns each funder's collateral, since no outcome happened.

Milestones M1 to M3 are done and pass on regtest. M4 (testnet) is not run yet. Raw logs: [team escrow](spikes/beneficiaries-regtest.json), [split desk trade](spikes/phase4-split-regtest.json).

## The model

```rust
pub enum Payee { Offer, Accept, Beneficiary(usize) }

pub struct Beneficiary {
    pub label: String,
    pub payout: TransparentAddress,       // CETs pay here
    pub close: Option<orchard::Address>,  // the close pays here; None: `payout`, transparently
}

pub struct Split(pub Vec<(Payee, u32)>);  // basis points summing to 10,000; empty: all to the side's own party

pub struct Splits {                       // Terms::splits
    pub beneficiaries: Vec<Beneficiary>,
    pub offer: Split,
    pub accept: Split,
    pub outcomes: Vec<(String, Vec<(Payee, u64)>)>,  // enumerated only: an outcome's own outputs
}
```

The contract still computes one payout per side, exactly as before. The split then divides each side's amount:

| Rule | Detail |
| --- | --- |
| Rounding | Each share rounds down. The remainder goes to the side's first payee, so both signers compute identical outputs |
| Dust | A share under 54 zat is added to the first payee of the same side. Only a side whose whole payout is dust moves to the other side, as before |
| Merging and order | Outputs to the same payee merge. Outputs are ordered by payee: offer, accept, then beneficiaries by index |
| Fee reserve | Sized for the CET with the most outputs: ZIP 317 charges 5,000 zat per output past two. A smaller CET's unused reserve goes to its largest output (the later one on a tie) |
| Per-outcome overrides | `Splits::outcomes` gives an enumerated outcome a fixed output list, such as fixed fees with the rest to the client. Its amounts sum to the pot, and none is dust |
| Payees | A split names any payee, including the other side's party or a party's own address. `Payee` replaces the doc's plain beneficiary index, so "60% to me, 40% to a partner" needs no extra entry |
| Checks | `Terms::validate` refuses: more than 16 beneficiaries, shares not summing to 10,000, a zero share, an unknown beneficiary, or an override that is off the pot, pays dust or names an unknown outcome |

**Default contracts are unchanged byte for byte.** `tests/golden.rs` pins the funding txid, every CET sighash and the refund sighash of three default contracts (enumerated with a dust outcome, single-funder, stable value) to the values built at `0ceda8c`.

## Settlement

- **CETs** pay one transparent output per payee.
- **The cooperative close** pays Ironwood to every payee with a close address and transparently to the rest. Payees share the fee equally; the reserve pays it first.
- **`Contract::close_outcome`** builds the close of an enumerated contract at an outcome, paying what its CET pays.
- **`check_close`** compares the full output list, Ironwood and transparent: each note re-derives its commitment, each transparent script decodes to the agreed address, and the amounts match. Each signer trial-decrypts only its own output, since it holds no other viewing keys.
- **The refund and the recovery record** are unchanged. The refund needs no beneficiary data, and beneficiaries hold no keys to recover.

## Wire and agents

| Where | Change |
| --- | --- |
| `WireTerms::splits` | Beneficiaries (label, transparent address, optional raw Ironwood receiver), both splits and overrides. Payees are `offer`, `accept` or `b<index>`. The field is left out when it holds the default, and a message without it decodes as the default, so older agents interoperate |
| `zdlc-trader taker --split` | `self:6000,tmADDR+IRONWOOD_HEX:4000` splits the taker's side by basis points. `self` is the taker; `+IRONWOOD_HEX` is where the close pays that payee, shielded |
| Maker | Prices only its own side and never needs the taker's beneficiaries. It refuses an offer that splits the maker's side or carries overrides |
| Desk | Unchanged: the split travels only in the sealed offer, so the desk never sees it |

## Regtest runs

| Run | Result |
| --- | --- |
| Team escrow by CET (`tests/beneficiaries.rs`) | The client funded 1.5 ZEC alone (block 255), and the team lead signed with no funds. "delivered" was attested and the lead completed the CET (block 256): 0.75, 0.45 and 0.30 ZEC to Ana, Ben and Chen. The three-output reserve (15,000 zat) covered the fee exactly. Each payout was found at its address |
| Team escrow by cooperative close | Funded at 258 and closed at 259 to three Ironwood receivers: 0.74996666, 0.44996667 and 0.29996667 ZEC. The fee was shared equally, and the pool grew by exactly the outputs. Each beneficiary trial-decrypted its own note. A close moving 0.1 ZEC from Ben to Ana was refused before anyone signed |
| Split desk trade (`tests/phase4.rs`, third trade) | Separate maker and taker processes traded through the desk. The taker ran with `--split self:6000,<partner>:4000`, and the maker co-signed the close (block 344). The partner, who only gave addresses, found 0.03996667 ZEC (40% of the 0.1 ZEC side, less its fee share) with its viewing key |

Offline tests cover rounding, dust, merging, overrides, the validity checks, a mixed close (one beneficiary without a shielded address), a refused redirect of a transparent payee, and the wire round trip.

## Not done

- **M4, testnet:** one team escrow and one split desk trade on Zcash testnet.
- **Fee fairness:** the larger reserve is still shared equally by the funders. A side that adds beneficiaries could pay for its own outputs instead.
- **RFQ splits:** an RFQ cannot name the taker's split yet. It is not needed, since the maker never prices it.
- **`zdlc-live` and the demo** run default contracts only.
