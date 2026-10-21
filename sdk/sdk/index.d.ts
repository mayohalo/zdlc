import type { Balance, Contract, Funding, Info, RecoveredRecord, State, Template, ZdlcEvent } from "@zdlc/client";
export { templates, zat, zec, ZdlcError } from "@zdlc/client";
export type { Balance, Contract, Funding, Info, RecoveredRecord, State, Template, ZdlcEvent } from "@zdlc/client";

export interface NodeConfig {
  /** 32 bytes of hex: every key of this node derives from it. Keep it secret. */
  seed: string;
  /** A Zakura JSON-RPC endpoint. */
  rpc: string;
  /** The desk (mailbox) URL. */
  desk: string;
  /** The oracles every contract settles on (all must agree). */
  oracles: string[];
  /** SQLite database path; ":memory:" keeps nothing. */
  db: string;
  /** Seconds a block takes (75 on Zcash). */
  blockSecs?: number;
  autoShield?: boolean;
  recoveryRecords?: boolean;
}

/** A self-custodial zDLC node in this process. Same calls as @zdlc/client's ZdlcClient. */
export declare class Zdlc {
  static open(config: NodeConfig): Promise<Zdlc>;
  readonly identity: string;
  readonly treasury: string;
  info(): Promise<Info>;
  balance(): Promise<Balance>;
  contracts(): Promise<Contract[]>;
  contract(id: string): Promise<Contract>;
  offer(counterparty: string, template: Template): Promise<{ id: string }>;
  accept(id: string, funding?: Funding): Promise<{ id: string }>;
  decline(id: string): Promise<{ id: string }>;
  depositQr(id: string): Promise<string>;
  withdraw(to: string): Promise<{ txids: string[] }>;
  recover(from?: number, gap?: number): Promise<{ records: RecoveredRecord[] }>;
  events(opts?: { after?: number; limit?: number }): Promise<{ events: ZdlcEvent[]; next: number }>;
  stream(opts?: { after?: number; signal?: AbortSignal }): AsyncGenerator<ZdlcEvent>;
  waitFor(id: string, state: State, timeoutMs?: number): Promise<Contract>;
  close(): void;
}
