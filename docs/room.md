# The escrow room

An interactive demo: a web page where a visitor plays every side of one escrow. `zdlc-room` runs the two parties, a contractor and a client, as two kit nodes in one process and serves the page. The visitor:

- accepts the invoice as the client and funds it, from the demo wallet or by scanning the deposit link with their own wallet;
- rules as each of the two arbiters: "delivered" or "not delivered", or stays silent;
- takes either party offline;
- tries to cheat: the contractor pays itself with forged arbiter signatures, or the client takes the refund early.

Every transaction is real, and every refusal comes from the network, not the app.

| What the visitor does | What happens | Regtest run |
| --- | --- | --- |
| Both arbiters: "delivered" | The contractor asks for the cooperative close, the client's node checks it pays exactly the attested outcome and co-signs; shielded outputs | Close, block 812 |
| Client offline, then "delivered" | The close request is dropped. After 6 blocks and 60 s the contractor completes the pre-signed payout with the arbiters' signatures and settles alone. The client, back online, recognises the settlement | CET, block 839 |
| Arbiters disagree | No payout can be completed; the refund settles it at its height | Refund, block 553 |
| Contractor: "pay myself, forging the arbiters" | The payout is assembled by hand around a signature decrypted with made-up scalars. The node refuses it: `could not verify a transparent script: script verification failed` | Refused |
| Client: "take my money back now" | The node refuses the refund: `transaction is locked until after block height H` | Refused |
| Client pays the deposit link | The client's node funds the escrow from the shielded note once it is mined | Close, block 687 |

## Run it locally

```sh
just dev    # the regtest stack
just room   # http://127.0.0.1:8080/ with a funded demo wallet
```

## Kit changes behind it

- **`Zdlc::hold(id, held)`** (`POST /v1/contracts/{id}/hold`): a held contract's messages are dropped and the loop skips it, as if the node were offline for it.
- **`Zdlc::attempt(id, "refund" | "forge")`** (`POST /v1/contracts/{id}/attempt`): broadcasts the refund now, or this side's best payout completed with made-up oracle signatures (`Contract::cet_unchecked`, assembled by hand so no local check stops it), and returns what the chain said.
- **Stale maturity:** an accepter that decides after the offer's maturity height (or waits that long for a deposit) now asks for fresh heights instead of failing. The offerer re-offers in the same case.

## On testnet

`zdlc-room` needs a Zakura testnet node, the desk, two `zdlc-oracled` arbiters started with `--admin-token FILE`, and that token file:

```sh
zdlc-room --node $ZDLC_RPC --desk $DESK --oracles $ORACLE_1,$ORACLE_2 --oracle-token-file admin.token \
          --dir /var/lib/zdlc-room --listen 127.0.0.1:8080 --label testnet \
          --amount 0.01 --maturity 3 --refund 24 --block-secs 75 --max-rooms 20 \
          --explorer 'https://<a testnet explorer>/tx/{txid}'
```

- **Demo wallet:** the client's treasury, printed at start. Fund it with several separate coins (one room holds one or more until it settles).
- **Timing at 75 s blocks:** the happy path takes about 5–10 minutes from accept to close. A party offline adds 6 blocks (about 8 minutes). The refund paths wait for `--refund` blocks after the invoice (24 is about 30 minutes). A room's address stays valid, so a visitor can come back.
- **Exposure:** only the page and its `/api` are public, behind a TLS reverse proxy. They can only run the demo escrow at `--amount` between the two demo nodes, at most `--max-rooms` at once. The node RPC, the desk and the oracles' admin token stay on the server.
- **Custody:** both demo parties' keys live on the server, so the demo is custodial by construction. A real user runs their own node or the SDK.
