// The stateless WebAssembly core in Node: two wallets run a whole contract offline with
// an in-memory oracle (offer, accept, sign, finish, CET, refund, cooperative close).
import { test } from "node:test";
import assert from "node:assert/strict";
import * as core from "../core/node/zdlc_core.js";

const seed = (b) => b.repeat(32);
const ctx = (s, inputs = []) => ({ seed: s, index: 1, network: "regtest", inputs });
const coin = (txid, info) => ({ txid, index: 0, value: 3_0000_0000, pubkey: info.treasuryPubkey });

function setup() {
  const [sa, sb] = [seed("a1"), seed("b2")];
  const [ia, ib] = [core.walletInfo(ctx(sa)), core.walletInfo(ctx(sb))];
  const a = ctx(sa, [coin("11".repeat(32), ia)]);
  const b = ctx(sb, [coin("22".repeat(32), ib)]);
  const oracle = new core.Oracle(seed("0c"));
  const template = { template: "bet", question: "Rain on Friday?", outcomes: ["yes", "no"], offerer_wins: ["yes"], stake_zat: 1_0000_0000, maturity_blocks: 10, refund_blocks: 40 };
  const spec = core.templateEvent(template);
  const ann = oracle.announce("bet-1", spec);
  const tip = 300;
  const terms = core.templateTerms(template, [ann], tip + 1);
  const offer = core.offer(a, terms);
  const { accept, fundingTxid } = core.accept(b, offer, tip);
  const { sign } = core.sign(a, offer, accept, tip);
  const funding = core.finish(b, offer, accept, sign);
  return { a, b, ia, ib, oracle, spec, ann, tip, offer, accept, sign, funding, fundingTxid, template };
}

test("wallet addresses", () => {
  const i = core.walletInfo(ctx(seed("a1")));
  assert.match(i.treasury, /^tm/);
  assert.match(i.depositAddress, /^uregtest1/);
  assert.equal(i.identity.length, 66);
});

test("offer, accept, sign, finish: both sides agree on the funding transaction", () => {
  const s = setup();
  assert.equal(s.funding.txid, s.fundingTxid);
  assert.ok(s.funding.tx.length > 400);
  // The accepter re-derives the terms from the template and its own view of the oracle.
  assert.deepEqual(core.templateTerms(s.template, [s.ann], s.offer.terms.sign_height), s.offer.terms);
});

test("attested outcome: either side completes the CET; the refund is locked", () => {
  const s = setup();
  const att = s.oracle.attest("bet-1", s.spec, "yes");
  assert.deepEqual(core.verifyAttestation(s.ann, att), { outcome: "yes" });
  const byA = core.cet(s.a, "offerer", s.offer, s.accept, s.sign, [s.ann], [att], s.tip + 11);
  const byB = core.cet(s.b, "accepter", s.offer, s.accept, s.sign, [s.ann], [att], s.tip + 11);
  assert.equal(byA.txid, byB.txid);
  const r = core.refund(s.a, "offerer", s.offer, s.accept, s.sign);
  assert.equal(r.lockTime, s.tip + 40);
  assert.throws(() => s.oracle.attest("bet-1", s.spec, "no"), /already attested/);
});

test("payment links", () => {
  const i = core.walletInfo(ctx(seed("a1")));
  const uri = core.depositUri(i.depositAddress, 150_020_000, "abc");
  assert.deepEqual(core.parseUri(uri), { address: i.depositAddress, zat: 150_020_000, memo: "zDLC1:abc" });
  assert.match(core.qrSvg(uri), /^<\?xml/);
});

test("cooperative close, proved in WebAssembly", { timeout: 600_000 }, () => {
  const s = setup();
  const att = s.oracle.attest("bet-1", s.spec, "no");
  const anchor = "ae2935f1dfd8a24aed7c70df7de3a668eb7a49b1319880dde2bbd9031ae5d82f"; // the empty tree's root
  const t0 = Date.now();
  const { pczt } = core.closeBuild(s.a, "offerer", s.offer, s.accept, s.sign, [s.ann], [att], s.tip + 11, anchor);
  const proveMs = Date.now() - t0;
  const co = core.closeSign(s.b, "accepter", s.offer, s.accept, s.sign, [s.ann], [att], pczt, s.tip + 11, anchor);
  const tx = core.closeSign(s.a, "offerer", s.offer, s.accept, s.sign, [s.ann], [att], co.pczt, s.tip + 11, anchor);
  assert.ok(tx.tx.length > 1000);
  console.log(`  close: proving (with proving key build) ${proveMs} ms, ${tx.tx.length / 2} bytes`);
});

test("the browser build loads and runs", async () => {
  const { readFile } = await import("node:fs/promises");
  const web = await import("../core/web/zdlc_core.js");
  web.initSync({ module: await readFile(new URL("../core/web/zdlc_core_bg.wasm", import.meta.url)) });
  assert.equal(web.version(), core.version());
  assert.match(web.walletInfo({ seed: "a1".repeat(32), index: 1, network: "testnet" }).treasury, /^tm/);
});
