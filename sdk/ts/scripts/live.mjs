// Drives two zdlc-nodes through @zdlc/client: Alice offers Bob a bet, Bob accepts from
// his treasury, the arbiters resolve, and both see it settle. Run by the zdlc-kit test
// `ts_client_drives_a_bet`, which starts the nodes and sets these variables.
import { ZdlcClient, templates } from "../dist/index.js";

const env = (k) => process.env[k] ?? (() => { throw new Error(`${k} not set`); })();
const alice = new ZdlcClient(env("ZDLC_A_URL"), { token: env("ZDLC_A_TOKEN") });
const bob = new ZdlcClient(env("ZDLC_B_URL"), { token: env("ZDLC_B_TOKEN") });
const oracles = env("ZDLC_ORACLES").split(",");
const admin = env("ZDLC_ADMIN");

const bobInfo = await bob.info();
const { id } = await alice.offer(bobInfo.identity, templates.bet({ question: "Will the TS client work?", outcomes: ["yes", "no"], offererWins: ["no"], stake: "0.1", maturityBlocks: 12, refundBlocks: 300 }));
console.log(`[ts] Alice offered bet ${id}`);

// Bob reacts to events, as an app would.
const after = (await bob.events()).next;
const ac = new AbortController();
const bobSaw = [];
const watching = (async () => {
  for await (const e of bob.stream({ after, signal: ac.signal })) {
    if (e.id !== id) continue;
    bobSaw.push(e.kind);
    if (e.kind === "offered" && e.incoming) await bob.accept(id, "treasury");
    if (e.kind === "settled") return e;
  }
})().catch((e) => { if (e.name !== "AbortError") throw e; });

await alice.waitFor(id, "funded", 180_000);
for (const o of oracles) {
  const r = await fetch(`${o}/events/zdlc-${id}/resolve`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ outcome: "yes", token: admin }) });
  if (!r.ok) throw new Error(`resolve: ${await r.text()}`);
}
const settled = await watching;
const a = await alice.waitFor(id, "settled", 180_000);
ac.abort();
console.log(`[ts] settled: Alice ${JSON.stringify(a.settlement)}, Bob saw ${bobSaw.join(" > ")}`);
if (!settled || settled.kind !== "settled") throw new Error("Bob did not see the settlement");
console.log(JSON.stringify({ id, alice: a.settlement, bob_events: bobSaw, bob_balance: await bob.balance() }));
