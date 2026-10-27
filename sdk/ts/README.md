# @zdlc/client

Install from the [GitHub release](https://github.com/mayohalo/zdlc/releases) (not on npm yet):

```sh
npm install https://github.com/mayohalo/zdlc/releases/download/v0.1.0/zdlc-client-0.1.0.tgz
```

A typed TypeScript client for `zdlc-node`: Discreet Log Contracts on Zcash from any JavaScript runtime (Node 18+, browsers, Deno, Bun). The node holds the keys and runs the protocol; the client offers and accepts contracts from templates and follows them through events. It has no dependencies.

```ts
import { ZdlcClient, templates, zec } from "@zdlc/client";

const z = new ZdlcClient("http://127.0.0.1:7950", { token });

// Offer a client an escrow: they fund it, you are paid on "delivered".
const { id } = await z.offer(clientIdentity, templates.escrow({ amount: "1.5", question: "Milestone 1 delivered?" }));

// React to events as they happen (long-polls the node).
for await (const ev of z.stream()) {
  switch (ev.kind) {
    case "offered":
      if (ev.incoming) await z.accept(ev.id, "deposit"); // or "treasury"
      break;
    case "awaitingDeposit":
      showQr(ev.uri); // a ZIP 321 link any Zcash wallet can pay; z.depositQr(id) gives an SVG
      break;
    case "settled":
      console.log(ev.id, ev.how, ev.txid);
      break;
  }
}

console.log(zec((await z.balance()).shieldedZat), "ZEC shielded");
await z.withdraw(myUnifiedAddress);
```

| Call | What it does |
| --- | --- |
| `info()` | The node's identity (what counterparties offer to), network, tip, treasury address, oracles |
| `offer(counterparty, template)` | Offers a contract; returns its id |
| `accept(id, "treasury" \| "deposit")`, `decline(id)` | Answers an incoming offer |
| `contracts()`, `contract(id)`, `waitFor(id, state)` | Contract records |
| `events({ after, wait })`, `stream({ after, signal })` | The event log, paged or as an async iterator |
| `depositQr(id)` | The deposit link as SVG |
| `balance()`, `withdraw(unifiedAddress)` | Treasury, shielded balance, and moving it out |

`templates.stablePayment`, `templates.escrow` and `templates.bet` take amounts as decimal ZEC strings. `zat()` and `zec()` convert between ZEC and zatoshis. Errors are `ZdlcError` values carrying the node's message and HTTP status.

The types mirror the node's [OpenAPI spec](../../crates/zdlc-kit/openapi.json).

```sh
npm run build && npm test     # offline tests
npm run live                  # against two running nodes (see scripts/live.mjs)
```
