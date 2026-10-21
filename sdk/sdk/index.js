// @zdlc/sdk for Node: a self-custodial zDLC node running in this process. The native
// addon holds the keys and the SQLite database and runs the contract loop; this class
// has the same calls as @zdlc/client's ZdlcClient, so an app can switch between talking
// to a zdlc-node and embedding one.
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import path from "node:path";

export { templates, zat, zec, ZdlcError } from "@zdlc/client";

const here = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const native = require(path.join(here, "native", `zdlc.${process.platform}-${process.arch}.node`));

export class Zdlc {
  #n;
  constructor(n) {
    this.#n = n;
  }

  /** Opens a node: { seed (32 bytes hex), rpc, desk, oracles, db, blockSecs?, autoShield?, recoveryRecords? }. */
  static async open(config) {
    return new Zdlc(await native.ZdlcNode.open(config));
  }

  /** What counterparties offer to. */
  get identity() {
    return this.#n.identity;
  }

  /** Fund this address with transparent ZEC to put up collateral from the treasury. */
  get treasury() {
    return this.#n.treasury;
  }

  info() {
    return this.#n.info();
  }
  balance() {
    return this.#n.balance();
  }
  contracts() {
    return this.#n.contracts();
  }
  async contract(id) {
    const c = await this.#n.contract(id);
    if (c === null) throw Object.assign(new Error(`no contract ${id}`), { status: 404 });
    return c;
  }
  async offer(counterparty, template) {
    return { id: await this.#n.offer(counterparty, template) };
  }
  async accept(id, funding = "treasury") {
    await this.#n.accept(id, funding);
    return { id };
  }
  async decline(id) {
    await this.#n.decline(id);
    return { id };
  }
  depositQr(id) {
    return this.#n.depositQr(id);
  }
  async withdraw(to) {
    return { txids: await this.#n.withdraw(to) };
  }
  recover(from = 1, gap = 20) {
    return this.#n.recover(from, gap);
  }
  events({ after = 0, limit = 100 } = {}) {
    return this.#n.events(after, limit);
  }

  /** Every event from `after` on, as they happen. Stops when `signal` aborts. */
  async *stream({ after = 0, signal } = {}) {
    while (!signal?.aborted) {
      const page = await this.events({ after });
      for (const e of page.events) yield e;
      after = page.next;
      if (page.events.length === 0) await new Promise((r) => setTimeout(r, 500));
    }
  }

  /** Resolves when contract `id` reaches `state`. */
  async waitFor(id, state, timeoutMs = 300_000) {
    const end = Date.now() + timeoutMs;
    for (;;) {
      const c = await this.#n.contract(id);
      if (c?.state === state) return c;
      if (c?.state === "failed") throw new Error(`contract ${id} failed: ${c.note}`);
      if (Date.now() > end) throw new Error(`contract ${id} did not reach ${state}`);
      await new Promise((r) => setTimeout(r, 500));
    }
  }

  /** Stops the node's loop; the database keeps everything. */
  close() {
    this.#n.close();
  }
}
