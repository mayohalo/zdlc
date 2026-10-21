/**
 * @zdlc/client: a typed client for zdlc-node, the zDLC node's REST API.
 *
 * zDLC runs Discreet Log Contracts on Zcash: two parties lock ZEC in a 2-of-2, pre-sign
 * one payout per oracle outcome, and normally close straight back to shielded
 * addresses. The node holds the keys and runs the protocol; this client offers and
 * accepts contracts from templates and follows them through events.
 *
 * ```ts
 * import { ZdlcClient, templates, zat } from "@zdlc/client";
 *
 * const z = new ZdlcClient("http://127.0.0.1:7950", { token });
 * const { id } = await z.offer(counterparty, templates.escrow({ amount: "1.5", question: "Milestone 1 delivered?" }));
 * for await (const ev of z.stream()) {
 *   if (ev.kind === "awaitingDeposit") renderQr(ev.uri);
 *   if (ev.kind === "settled") console.log(ev.id, ev.how);
 * }
 * ```
 *
 * Amounts are zatoshis (1 ZEC = 100,000,000) as numbers; `zat("1.5")` converts.
 */

// ---- Types (mirroring openapi.json) -------------------------------------------------------

export type Funding = "treasury" | "deposit";
export type Role = "offerer" | "accepter";
export type State =
  | "offered"
  | "incoming"
  | "awaiting_deposit"
  | "refreshing"
  | "accepted"
  | "signed"
  | "funded"
  | "closing"
  | "settled"
  | "declined"
  | "failed";

export interface Payee {
  label: string;
  /** Transparent address the CET pays. */
  address: string;
  /** Unified address whose Ironwood receiver a cooperative close pays. */
  shielded?: string;
  /** Basis points of the side's payout; all payees sum to 10,000. */
  bps: number;
}

export interface StablePaymentTemplate {
  template: "stable_payment";
  usd_cents: number;
  collateral_zat: number;
  premium_zat?: number;
  maturity_blocks: number;
  refund_blocks: number;
  /** Unix time of the ZEC/USD minute the oracles attest; the node fills it in when absent. */
  price_minute?: number | null;
}

export interface EscrowTemplate {
  template: "escrow";
  amount_zat: number;
  question: string;
  payees?: Payee[];
  maturity_blocks: number;
  refund_blocks: number;
}

export interface BetTemplate {
  template: "bet";
  question: string;
  outcomes: string[];
  offerer_wins: string[];
  stake_zat: number;
  maturity_blocks: number;
  refund_blocks: number;
}

export interface HedgeTemplate {
  template: "hedge";
  amount_zat: number;
  /** USD per ZEC, in cents. */
  price_cents: number;
  premium_zat?: number;
  maturity_blocks: number;
  refund_blocks: number;
  price_minute?: number | null;
}

export interface PayrollTemplate {
  template: "payroll";
  usd_cents: number;
  collateral_zat: number;
  premium_zat?: number;
  periods: number;
  period_blocks: number;
  refund_margin_blocks: number;
  /** 1-based; the node fills the series fields. */
  period?: number;
  series_id?: string | null;
  price_minute?: number | null;
}

export interface PayOnPaymentTemplate {
  template: "pay_on_payment";
  amount_zat: number;
  collateral_zat: number;
  by_blocks: number;
  refund_blocks: number;
  /** The node fills the lender's address and viewing key. */
  repay_to?: string | null;
  ivk?: string | null;
  from_height?: number | null;
}

export type Template = StablePaymentTemplate | HedgeTemplate | PayrollTemplate | PayOnPaymentTemplate | EscrowTemplate | BetTemplate;

export interface Settlement {
  how: "close" | "cet" | "refund" | "counterparty";
  txid: string | null;
  height: number | null;
}

export interface Contract {
  id: string;
  role: Role;
  state: State;
  counterparty: string;
  template: Template;
  summary: Record<string, unknown>;
  eventId: string;
  funding: Funding | null;
  deposit: { uri: string; zat: number; from: number } | null;
  fundingTxid: string | null;
  maturity: number | null;
  refundHeight: number | null;
  attested: string | null;
  settlement: Settlement | null;
  shieldedPayout: string | null;
  recordTxid: string | null;
  note: string | null;
}

export interface RecoveredRecord {
  index: number;
  fundingTxid: string;
  refundHeight: number;
  collateral: number;
  /** "settled", "refund due at H", "refund sent TXID" or "refund refused: …". */
  status: string;
}

export interface Info {
  identity: string;
  network: string;
  tip: number;
  treasury: string;
  oracles: { url: string; pubkey: string }[];
  transport?: string;
  storage?: string;
  chain?: string;
}

export interface Balance {
  treasuryZat: number;
  shieldedZat: number;
  shieldedNotes: number;
  inContractsZat: number;
}

type Ev<K extends string, F> = { seq: number; kind: K; id: string } & F;

export type ZdlcEvent =
  | Ev<"offered", { incoming: boolean; counterparty: string; summary: Record<string, unknown> }>
  | Ev<"awaitingDeposit", { uri: string; zat: number }>
  | Ev<"accepted", {}>
  | Ev<"signed", {}>
  | Ev<"funded", { txid: string; height: number }>
  | Ev<"attested", { outcome: string }>
  | Ev<"settled", { how: Settlement["how"]; txid: string | null; height: number | null }>
  | Ev<"payoutShielded", { txid: string }>
  | Ev<"warning", { message: string }>
  | Ev<"declined", {}>
  | Ev<"failed", { reason: string }>;

export class ZdlcError extends Error {
  constructor(
    message: string,
    readonly status: number,
  ) {
    super(message);
    this.name = "ZdlcError";
  }
}

// ---- Amounts and templates ------------------------------------------------------------------

/** Decimal ZEC ("1.5") to zatoshis. */
export function zat(zec: string | number): number {
  const s = typeof zec === "number" ? zec.toFixed(8) : zec.trim();
  const m = /^(\d+)(?:\.(\d{0,8}))?$/.exec(s);
  if (!m) throw new Error(`not a ZEC amount: ${zec}`);
  return Number(m[1]) * 100_000_000 + Number((m[2] ?? "").padEnd(8, "0"));
}

/** Zatoshis to decimal ZEC, without trailing zeros. */
export function zec(zatoshis: number): string {
  const whole = Math.floor(zatoshis / 100_000_000);
  const frac = String(zatoshis % 100_000_000).padStart(8, "0").replace(/0+$/, "");
  return frac ? `${whole}.${frac}` : String(whole);
}

/** Friendly builders: amounts in decimal ZEC, sensible block defaults. */
export const templates = {
  escrow(o: { amount: string; question: string; payees?: Payee[]; maturityBlocks?: number; refundBlocks?: number }): EscrowTemplate {
    return {
      template: "escrow",
      amount_zat: zat(o.amount),
      question: o.question,
      payees: o.payees ?? [],
      maturity_blocks: o.maturityBlocks ?? 20,
      refund_blocks: o.refundBlocks ?? 2000,
    };
  },
  stablePayment(o: { usd: number; collateral: string; premium?: string; maturityBlocks?: number; refundBlocks?: number; priceMinute?: number }): StablePaymentTemplate {
    return {
      template: "stable_payment",
      usd_cents: Math.round(o.usd * 100),
      collateral_zat: zat(o.collateral),
      premium_zat: zat(o.premium ?? "0"),
      maturity_blocks: o.maturityBlocks ?? 6,
      refund_blocks: o.refundBlocks ?? 60,
      price_minute: o.priceMinute ?? null,
    };
  },
  hedge(o: { amount: string; priceUsd: number; premium?: string; maturityBlocks?: number; refundBlocks?: number; priceMinute?: number }): HedgeTemplate {
    return {
      template: "hedge",
      amount_zat: zat(o.amount),
      price_cents: Math.round(o.priceUsd * 100),
      premium_zat: zat(o.premium ?? "0"),
      maturity_blocks: o.maturityBlocks ?? 6,
      refund_blocks: o.refundBlocks ?? 60,
      price_minute: o.priceMinute ?? null,
    };
  },
  payroll(o: { usd: number; collateral: string; periods: number; periodBlocks: number; refundMarginBlocks?: number; premium?: string }): PayrollTemplate {
    return {
      template: "payroll",
      usd_cents: Math.round(o.usd * 100),
      collateral_zat: zat(o.collateral),
      premium_zat: zat(o.premium ?? "0"),
      periods: o.periods,
      period_blocks: o.periodBlocks,
      refund_margin_blocks: o.refundMarginBlocks ?? 40,
    };
  },
  payOnPayment(o: { amount: string; collateral: string; withinBlocks: number; refundBlocks?: number }): PayOnPaymentTemplate {
    return {
      template: "pay_on_payment",
      amount_zat: zat(o.amount),
      collateral_zat: zat(o.collateral),
      by_blocks: o.withinBlocks,
      refund_blocks: o.refundBlocks ?? 2000,
    };
  },
  bet(o: { question: string; outcomes: string[]; offererWins: string[]; stake: string; maturityBlocks?: number; refundBlocks?: number }): BetTemplate {
    return {
      template: "bet",
      question: o.question,
      outcomes: o.outcomes,
      offerer_wins: o.offererWins,
      stake_zat: zat(o.stake),
      maturity_blocks: o.maturityBlocks ?? 20,
      refund_blocks: o.refundBlocks ?? 2000,
    };
  },
};

// ---- The client -------------------------------------------------------------------------------

export interface ClientOptions {
  /** The node's bearer token (its token file). */
  token: string;
  /** A fetch implementation; defaults to the global one (Node 18+, browsers). */
  fetch?: typeof fetch;
}

export class ZdlcClient {
  private readonly base: string;
  private readonly token: string;
  private readonly f: typeof fetch;

  constructor(baseUrl: string, opts: ClientOptions) {
    this.base = baseUrl.replace(/\/+$/, "");
    this.token = opts.token;
    this.f = opts.fetch ?? globalThis.fetch.bind(globalThis);
  }

  private async req<T>(method: string, path: string, body?: unknown, signal?: AbortSignal): Promise<T> {
    const r = await this.f(`${this.base}${path}`, {
      method,
      headers: { authorization: `Bearer ${this.token}`, ...(body === undefined ? {} : { "content-type": "application/json" }) },
      body: body === undefined ? undefined : JSON.stringify(body),
      signal,
    });
    const text = await r.text();
    if (!r.ok) {
      let msg = text;
      try {
        msg = (JSON.parse(text) as { error?: string }).error ?? text;
      } catch {}
      throw new ZdlcError(msg, r.status);
    }
    return (r.headers.get("content-type") ?? "").includes("application/json") ? (JSON.parse(text) as T) : (text as T);
  }

  /** This node: its identity (what counterparties offer to), network, tip, treasury. */
  info(): Promise<Info> {
    return this.req("GET", "/v1/info");
  }

  balance(): Promise<Balance> {
    return this.req("GET", "/v1/balance");
  }

  contracts(): Promise<Contract[]> {
    return this.req("GET", "/v1/contracts");
  }

  contract(id: string): Promise<Contract> {
    return this.req("GET", `/v1/contracts/${encodeURIComponent(id)}`);
  }

  /** Offers a contract to `counterparty` (its identity). */
  offer(counterparty: string, template: Template): Promise<{ id: string }> {
    return this.req("POST", "/v1/contracts", { counterparty, template });
  }

  /** Accepts an incoming offer. With "deposit", watch for `awaitingDeposit` and show its link. */
  accept(id: string, funding: Funding = "treasury"): Promise<{ id: string }> {
    return this.req("POST", `/v1/contracts/${encodeURIComponent(id)}/accept`, { funding });
  }

  decline(id: string): Promise<{ id: string }> {
    return this.req("POST", `/v1/contracts/${encodeURIComponent(id)}/decline`, {});
  }

  /** The deposit link as an SVG QR code. */
  depositQr(id: string): Promise<string> {
    return this.req("GET", `/v1/contracts/${encodeURIComponent(id)}/qr.svg`);
  }

  /** Sends the whole shielded balance to a unified address. */
  withdraw(to: string): Promise<{ txids: string[] }> {
    return this.req("POST", "/v1/withdraw", { to });
  }

  /** Seed-only recovery: finds this wallet's recovery records and broadcasts every refund that is due. */
  recover(from = 1, gap = 20): Promise<{ records: RecoveredRecord[] }> {
    return this.req("POST", "/v1/recover", { from, gap });
  }

  /** One page of events after `after`; `wait` long-polls up to that many seconds. */
  events(opts: { after?: number; wait?: number; limit?: number; signal?: AbortSignal } = {}): Promise<{ events: ZdlcEvent[]; next: number }> {
    const q = new URLSearchParams({ after: String(opts.after ?? 0), wait: String(opts.wait ?? 0), limit: String(opts.limit ?? 100) });
    return this.req("GET", `/v1/events?${q}`, undefined, opts.signal);
  }

  /** Every event from `after` on, as they happen. Stops when `signal` aborts. */
  async *stream(opts: { after?: number; signal?: AbortSignal } = {}): AsyncGenerator<ZdlcEvent> {
    let after = opts.after ?? 0;
    while (!opts.signal?.aborted) {
      const page = await this.events({ after, wait: 30, signal: opts.signal });
      for (const e of page.events) yield e;
      after = page.next;
    }
  }

  /** Resolves when contract `id` reaches `state`; rejects if it fails or `timeoutMs` passes. */
  async waitFor(id: string, state: State, timeoutMs = 300_000): Promise<Contract> {
    const end = Date.now() + timeoutMs;
    for (;;) {
      try {
        const c = await this.contract(id);
        if (c.state === state) return c;
        if (c.state === "failed") throw new ZdlcError(`contract ${id} failed: ${c.note}`, 409);
      } catch (e) {
        if (!(e instanceof ZdlcError) || e.status !== 404) throw e;
      }
      if (Date.now() > end) throw new ZdlcError(`contract ${id} did not reach ${state}`, 408);
      await new Promise((r) => setTimeout(r, 500));
    }
  }
}
