# @zdlc/sdk

Install from the [GitHub release](https://github.com/mayohalo/zdlc/releases) (not on npm yet):

```sh
npm install https://github.com/mayohalo/zdlc/releases/download/v0.1.0/zdlc-sdk-0.1.0.tgz
```

Self-custodial zDLC for JavaScript, in two parts:

| Import | Runs | What it is |
| --- | --- | --- |
| `@zdlc/sdk` | Node.js (native addon) | A whole zDLC node inside your process: keys, SQLite database, the contract loop. The same calls as `@zdlc/client`, without a `zdlc-node` |
| `@zdlc/sdk/core` | Browsers, bundlers, Node (WebAssembly) | The stateless protocol core: build, check and sign every contract message and transaction. Your app keeps the messages and talks to its peer, oracles and chain |

## In Node: a node in your process

```ts
import { Zdlc, templates } from "@zdlc/sdk";

const z = await Zdlc.open({
  seed,                                   // 32 bytes of hex; every key derives from it
  rpc: "http://127.0.0.1:18252/",         // a Zakura JSON-RPC endpoint
  desk: "http://127.0.0.1:7900",
  oracles: ["http://127.0.0.1:7801", "http://127.0.0.1:7802"],
  db: "zdlc.db",
});
console.log("fund", z.treasury, "offer to", z.identity);

const { id } = await z.offer(peer, templates.escrow({ amount: "0.5", question: "Logo delivered?" }));
for await (const ev of z.stream()) {
  if (ev.kind === "settled") break;
}
```

`Zdlc` has the same methods as `ZdlcClient`: `offer`, `accept`, `decline`, `contracts`, `contract`, `events`, `stream`, `waitFor`, `balance`, `withdraw`, `recover` and `depositQr`. Moving from a remote node to an embedded one is one line. Settlement, funding by deposit link and recovery records are as in the [kit](../../crates/zdlc-kit).

## In the browser: the stateless core

```ts
import init, * as core from "@zdlc/sdk/core";
await init();

const ctx = { seed, index: 1, network: "testnet", inputs: myTreasuryCoins };
const terms = core.templateTerms(template, announcements, tip + 1);  // identical on both sides
const offer = core.offer(ctx, terms);                                 // → send to the peer
// peer: core.accept(peerCtx, offer, tip) → { accept, fundingTxid }
const { sign } = core.sign(ctx, offer, accept, tip);                  // → send to the peer
// peer: core.finish(peerCtx, offer, accept, sign) → { tx, txid }: broadcast
// at maturity, with the oracles' attestations:
core.cet(ctx, "offerer", offer, accept, sign, announcements, attestations, tip);  // or closeBuild / closeSign
```

| Group | Functions |
| --- | --- |
| Wallet | `walletInfo(ctx)`: identity, treasury (and its key), payout, deposit and close addresses |
| Templates | `templateFill`, `templateEvent`, `templateTerms`, `templateSummary` |
| Oracles | `verifyAttestation(announcement, attestation)`; `new Oracle(seed)` (announce and attest in memory: demos, tests, an in-browser arbiter) |
| Contract | `offer`, `accept`, `sign`, `finish`, `cet`, `refund`, `closeBuild`, `closeSign` |
| Payment links | `depositUri`, `legUri`, `parseUri`, `qrSvg` |

`ctx` is `{ seed, index, network, inputs }`:
- `network` is `"regtest"`, `"testnet"` or a node's `getblockchaininfo` result.
- `inputs` are this side's treasury coins as `{ txid, index, value, pubkey }`.

All messages are the same JSON as `zdlc-node`'s wire format, so a browser party can trade with a native node.

**Proofs.** Most steps prove nothing and are fast: setup, CETs, the refund, checking a close. The cooperative close proves its Ironwood outputs. The first close in a page builds the proving key: about 20 s single-threaded in Node's WebAssembly, then faster. Run it in a Web Worker.

## Building

```sh
../../scripts/build-node.sh   # the native addon → native/zdlc.<platform>-<arch>.node
../../scripts/build-wasm.sh   # the core → core/node and core/web (needs wasm-pack and a clang with a wasm32 target)
npm install && npm test       # offline: a whole contract between two wallets, the close proved in WebAssembly
```
