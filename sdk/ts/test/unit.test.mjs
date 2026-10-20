// Offline tests: amounts, templates, and the client against a mocked fetch.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ZdlcClient, ZdlcError, templates, zat, zec } from "../dist/index.js";

test("amounts convert exactly", () => {
  assert.equal(zat("1.5"), 150_000_000);
  assert.equal(zat("0.00000001"), 1);
  assert.equal(zat("21"), 2_100_000_000);
  assert.throws(() => zat("1.000000001"));
  assert.equal(zec(150_000_000), "1.5");
  assert.equal(zec(1), "0.00000001");
  assert.equal(zec(0), "0");
});

test("templates match the node's wire shape", () => {
  assert.deepEqual(templates.escrow({ amount: "1.5", question: "Delivered?" }), {
    template: "escrow", amount_zat: 150_000_000, question: "Delivered?", payees: [], maturity_blocks: 20, refund_blocks: 2000,
  });
  assert.equal(templates.stablePayment({ usd: 100.25, collateral: "1" }).usd_cents, 10025);
  assert.deepEqual(templates.bet({ question: "q", outcomes: ["a", "b"], offererWins: ["a"], stake: "0.2" }).offerer_wins, ["a"]);
});

function mock(routes) {
  const calls = [];
  const f = async (url, init) => {
    calls.push({ url, init });
    const path = new URL(url).pathname;
    const [status, body] = routes[`${init.method} ${path}`] ?? [404, { error: "no route" }];
    return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
  };
  return { f, calls };
}

test("the client sends the token and the request body", async () => {
  const { f, calls } = mock({ "POST /v1/contracts": [200, { id: "abc" }] });
  const z = new ZdlcClient("http://node/", { token: "t0k", fetch: f });
  const { id } = await z.offer("02peer", templates.escrow({ amount: "1", question: "q" }));
  assert.equal(id, "abc");
  assert.equal(calls[0].url, "http://node/v1/contracts");
  assert.equal(calls[0].init.headers.authorization, "Bearer t0k");
  assert.equal(JSON.parse(calls[0].init.body).counterparty, "02peer");
});

test("errors carry the node's message and status", async () => {
  const { f } = mock({ "POST /v1/contracts/x/accept": [400, { error: "contract x is Funded, not incoming" }] });
  const z = new ZdlcClient("http://node", { token: "t", fetch: f });
  await assert.rejects(z.accept("x", "deposit"), (e) => e instanceof ZdlcError && e.status === 400 && /not incoming/.test(e.message));
});

test("stream pages through events", async () => {
  let n = 0;
  const f = async (url) => {
    const after = Number(new URL(url).searchParams.get("after"));
    n++;
    const events = after === 0 ? [{ seq: 1, kind: "offered", id: "a" }, { seq: 2, kind: "accepted", id: "a" }] : [{ seq: 3, kind: "settled", id: "a", how: "close" }];
    return new Response(JSON.stringify({ events, next: events.at(-1).seq }), { headers: { "content-type": "application/json" } });
  };
  const z = new ZdlcClient("http://node", { token: "t", fetch: f });
  const seen = [];
  for await (const e of z.stream()) {
    seen.push(e.kind);
    if (e.kind === "settled") break;
  }
  assert.deepEqual(seen, ["offered", "accepted", "settled"]);
  assert.equal(n, 2);
});
