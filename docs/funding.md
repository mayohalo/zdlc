# Shielded funding by QR

A user funds a contract by scanning a QR code with any Zcash wallet, and chooses between two paths per contract:

- **Path A, pay from my zDLC balance:** pay a deposit receiver in your own zDLC wallet; the wallet funds the contract from that shielded note. Every payout and the refund are still signed before funding.
- **Path B, pay from any wallet:** pay the contract's own leg address, from any wallet, exchanges included. A leg is reclaimable by its payer alone from the refund height.

Milestones M0 to M5 are done and pass on Zakura regtest. M6 (testnet, paid by scanning the QR with a real wallet) is not run yet. Raw logs: [leg branches](spikes/funding-leg-branches.json), [joint Ironwood funding](spikes/funding-ironwood-joint.json), [path A escrow](spikes/funding-path-a-escrow.json), [path A desk trade](spikes/funding-path-a-desk.json), [path B by CET](spikes/funding-path-b-cet.json), [path B reclaim](spikes/funding-path-b-reclaim.json).

## Payment links

`zdlc::paylink` builds ZIP 321 links and renders them as terminal QR (two rows per character) and SVG.

| Path | Link |
| --- | --- |
| A | `zcash:<unified address>?amount=1.5002&label=zDLC%20escrow-1&memo=<base64url of "zDLC1:escrow-1">`. The unified address carries only an Orchard-typed receiver: Ironwood shares the Orchard receiver, so a wallet past NU6.3 pays it into Ironwood. The receiver is unique to the contract and identifies the deposit; the memo is only a label for the payer |
| B | `zcash:t2…?amount=1.000175&label=zDLC%20c2-alice`. No memo: ZIP 321 allows none to a transparent recipient, and the address is unique to the contract |

`paylink::parse` reads a link back (address, amount, memo), and `decode_unified` gets the Ironwood receiver out of a unified address the user scans for withdrawal.

## Path A: deposit to your zDLC wallet

| Piece | Where | What it does |
| --- | --- | --- |
| Deposit key | `Wallet::deposit_sk/fvk/address(i)` | One Ironwood receiver per contract, derived from the seed, separate from the close address |
| Amount | `contract::ironwood_deposit(p, terms, offer, accept, notes)` | The party's collateral and its share of the reserve and funding fee, plus one Ironwood action per note spent. Each extra note costs 5,000 zat, so a top-up link asks for the two-note amount less what arrived |
| Watcher | `watch::deposit` | Trial-decrypts blocks from the link's height with the deposit key: `Waiting`, `Underpaid { got, want }` (show a top-up link) or `Paid { got, over }` |
| Notes and witnesses | `zdlc::notes` | Finds notes and their nullifiers by trial decryption. A note's Merkle path starts from the `z_gettreestate` frontier of the block before it, appends the commitments after it up to the anchor height, and must reproduce the node's root there. No full tree is kept |
| Ironwood spends | `tx::Spec::ironwood_spends`, `tx::sign_ironwood` | Notes spent in `tx::build`, proved with the outputs, authorized with the spend authorizing key |
| Funding | `PartyInfo::ironwood`, `Party::ironwood` | The Ironwood party builds the funding transaction alone: the agreed transparent inputs and outputs, its notes, and its shielded change. It proves it and authorizes its spends. The redacted copy it sends carries no witnesses, keys, randomizers, note plaintexts, value trapdoors or binding key. The other side checks the exact transparent inputs and outputs, lock time, expiry, branch and that Ironwood only flows in, then computes the txid every CET spends |

**Who builds, and when.** CET adaptor signatures need the funding txid, and the funding transaction needs both funding keys. So:

- An Ironwood **accepter** builds the funding PCZT in `accept` and sends it with its adaptor signatures.
- An Ironwood **offerer** needs the accepter's party in advance. A desk maker's quote now carries it (`QuoteBody::party`), and `Party::offer_funded` sends the PCZT with the offer.

Either way, the binding key travels only at the last step:

- An accepter keeps it and combines its own copy with the offerer's signatures (pczt Combiner) in `finish`.
- An offerer sends it in its sign message, after it holds every adaptor signature and the refund signature.

The funding is still signed and broadcast last.

## Path B: pay the contract directly

```text
IF   <H> CHECKLOCKTIMEVERIFY DROP <owner> CHECKSIG    # the payer alone, from the refund height H
ELSE 2 <A> <B> 2 CHECKMULTISIG                         # both parties (the contract's funding keys)
ENDIF
```

| Piece | Where | What it does |
| --- | --- | --- |
| Leg | `zdlc::leg::Leg` | Script, P2SH address (114-byte redeem script), both scriptSigs, and an input size for fees (309 bytes) |
| Amounts | `contract::leg_amounts` | What each side's leg must hold: collateral plus its share of the reserve and funding fee |
| Watcher | `watch::leg` | `getaddressutxos` on the leg address, with the same `Waiting` / `Underpaid` / `Paid` states |
| Funding | legs as funding inputs | Every funding input is a leg keyed to this contract (A, B and H checked). The offerer signs each leg's 2-of-2 branch in `sign` (`SignMsg::leg_sigs`); the accepter signs and assembles in `finish` |
| Reclaim | `leg::reclaim` | The owner's branch, from H. A wrong key is refused |

**Where this differs from the doc.** Legs are swept into the contract's 2-of-2 by the funding transaction, which is built and signed exactly as before. The CETs therefore do not spend the legs directly. The cost is one extra transaction. In return:

- CETs, the close, the refund, the recovery record and the watcher are unchanged.
- The safety order is unchanged: every payout and the refund are signed before both parties sign the sweep.
- Over- and underpayment fall out naturally. An overpaid leg returns its excess as funding change; an underpaid one fails the setup until topped up.

**Leg spends outside pczt.** pczt's Signer, Spend Finalizer and the ZIP 317 fee estimate all refuse a redeem script that is not plain multisig. So zdlc does three things itself:

- It signs the input's sighash itself.
- It gives the fee rule the leg's input size.
- It assembles the transaction from the PCZT's effects with the scriptSigs it builds (`tx::extract_with_script_sigs`).

## Money out and recovery

| Piece | Where | What it does |
| --- | --- | --- |
| Auto-shield | `funds::auto_shield`, the taker agent after a CET in deposit mode | Moves whatever lands on contract `i`'s transparent payout address (CET, refund, leg reclaim) into its deposit receiver |
| Withdraw | `funds::withdraw`, `zdlc-trader withdraw --to UA` | Sends the shielded balance to a unified address the user scans or pastes |
| Balance | `funds::balance`, `zdlc-trader balance`, `zdlc-trader shield` | Shielded balance across contract indices; shield every payout |
| Leg record | `recovery::LegRecord` (version 2, 47 bytes), `leg_record_tx`, `scan_legs` | What the seed cannot give: the other party's funding key and H. A wallet restored from its seed finds it, rebuilds the leg and reclaims it from H. The funded contract's own record (version 1) is unchanged |

## Regtest runs

| Run | Result |
| --- | --- |
| **M0:** leg branches | The 2-of-2 branch was mined before H (block 222, 365 bytes). The owner's branch was refused at tip 225 ("locked until after block height 226") and mined at 227 |
| **M0:** joint funding | An outside wallet shielded funds, then paid Alice's deposit address shielded to shielded, and the text memo arrived intact. One v6 transaction spent Alice's note and Bob's transparent coin into a 1.5 ZEC 2-of-2 (block 220). Proving took 538 ms; the redacted PCZT was 6.2 KB and gave Bob the same txid. Bob signed and extracted it, and Alice's change came back to her deposit key |
| **M2:** path A escrow, one funder | The contractor offered (invoiced) and the client accepted. The client's link asked for 1.5002 ZEC. The payer sent 1 ZEC (`Underpaid`), and the top-up link asked for 0.50025 more, covering the second note's action. The client funded from both notes as it accepted (block 276: accept with proving 887 ms, 9.2 KB PCZT). "delivered" paid the contractor 1.5 ZEC by CET (block 277) |
| **M3:** path A desk trade, two funders | Separate processes for the desk, two oracles, the maker and the taker. The taker (`--fund deposit`) printed a deposit link for 0.100625 ZEC, and the test, as the payer's wallet, scanned and paid it. The taker built the funding from that note against the maker's quoted inputs. The maker checked and accepted (98 CETs), and the funding was mined at 249. Both oracles agreed on $1,210, and the contract closed to Ironwood at 267 |
| **M4 and M5:** path B by CET | Each leg needed 1.000175 ZEC. Alice's was paid from a shielded wallet and Bob's transparently. Both were swept into the 2-of-2 at 281. "yes" paid Alice by CET (282). The payout was auto-shielded (283, 1.9999 ZEC) and withdrawn to a scanned unified address (284, 1.9998 ZEC) |
| **M4 and M5:** path B reclaim | Alice's app posted her leg record (286) and she paid her leg; Bob never paid or signed. A wallet restored from Alice's seed found the record, rebuilt the leg, was refused at tip 295, and reclaimed 0.500025 ZEC at 297 (H = 296) |

Every earlier gate still passes on fresh chains (Phases 0 to 4, products, beneficiaries), and the default contracts' txids are unchanged (`tests/golden.rs`).

```sh
infra/zakura/scripts/down.sh; infra/zakura/scripts/up.sh
cargo test -p zdlc --release --test funding -- --ignored --nocapture --test-threads=1   # path_a_desk_trade needs internet
```

## Not done

- **M6, testnet:** one path A desk trade and one path B escrow on Zcash testnet, each paid by scanning the QR with a real wallet. It needs a person holding a wallet, and the testnet node.
- **Wallet support for the deposit link:** which wallets pay a unified address with only an Orchard-typed receiver from a ZIP 321 link (into Ironwood), and keep the memo. This is a manual check with real wallets.
- **Both parties funding from Ironwood:** refused for now. The second party first moves its deposit to a transparent address of its own.
- **Path B over the desk:** legs run in-process. The wire carries `leg_sigs`, but agents do not post leg links yet.
- **Confirmation depth:** demos use one block. Make it configurable before mainnet.
