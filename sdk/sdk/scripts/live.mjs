// Two self-custodial nodes in one Node process (@zdlc/sdk): Alice invoices Bob an escrow,
// Bob funds it from a deposit link (paid by the dev wallet, as a phone would), the
// arbiters resolve "delivered", and both settle. Run by the zdlc-kit test
// `node_sdk_runs_two_nodes`, which starts the chain, desk and oracles and sets these.
import { execFileSync } from "node:child_process";
import { Zdlc, templates, zec } from "../index.js";

const env = (k) => process.env[k] ?? (() => { throw new Error(`${k} not set`); })();
const common = { rpc: env("ZDLC_RPC"), desk: env("ZDLC_DESK"), oracles: env("ZDLC_ORACLES").split(","), blockSecs: 1 };
const alice = await Zdlc.open({ ...common, seed: env("ZDLC_A_SEED"), db: `${env("ZDLC_DIR")}/sdk-alice.db` });
const bob = await Zdlc.open({ ...common, seed: env("ZDLC_B_SEED"), db: `${env("ZDLC_DIR")}/sdk-bob.db` });
console.log(`[sdk] alice ${alice.identity.slice(0, 12)}…, bob ${bob.identity.slice(0, 12)}…`);

const { id } = await alice.offer(bob.identity, templates.escrow({ amount: "0.5", question: "Logo delivered?", maturityBlocks: 12, refundBlocks: 300 }));
await bob.waitFor(id, "incoming", 60_000);
await bob.accept(id, "deposit");
const waiting = await bob.waitFor(id, "awaiting_deposit", 60_000);
console.log(`[sdk] Bob's deposit link: ${waiting.deposit.uri}`);
const qr = await bob.depositQr(id);
console.log(`[sdk] ${execFileSync(env("ZDLC_DEV_BIN"), ["pay", "--node", common.rpc, waiting.deposit.uri]).toString().trim()}`);
const funded = await alice.waitFor(id, "funded", 180_000);
for (const o of common.oracles) {
  const r = await fetch(`${o}/events/zdlc-${id}/resolve`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ outcome: "delivered", token: env("ZDLC_ADMIN") }) });
  if (!r.ok) throw new Error(`resolve: ${await r.text()}`);
}
const a = await alice.waitFor(id, "settled", 240_000);
const b = await bob.waitFor(id, "settled", 60_000);
const kinds = [];
for await (const e of alice.stream()) {
  kinds.push(e.kind);
  if (e.kind === "settled") break;
}
const bal = await alice.balance();
console.log(`[sdk] settled: ${JSON.stringify(a.settlement)}; Alice's events ${kinds.join(" > ")}; shielded ${zec(bal.shieldedZat)} ZEC`);
alice.close();
bob.close();
console.log(JSON.stringify({ id, funding_txid: funded.fundingTxid, alice: a.settlement, bob: b.settlement, alice_events: kinds, qr_svg_bytes: qr.length, alice_balance: bal }));
process.exit(0);
