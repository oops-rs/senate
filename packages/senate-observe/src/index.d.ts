import type { EventFrame, ResetFrame, Snapshot } from "./wire.js";
export type * from "./wire.js";

export interface ObserveOptions {
  endpoint: string | URL;
  token: string;
  signal?: AbortSignal;
}

export interface UnknownFrame {
  schema_version: 1;
  cursor: string;
  at: string;
  kind: string;
  data: unknown;
  committee_id?: string;
  link?: string | null;
}

export interface Update {
  state: Snapshot;
  event?: EventFrame | UnknownFrame;
}

export class ObserveError extends Error {
  constructor(code: string, message: string, status?: number, retryAfterMs?: number);
  readonly code: string;
  readonly status: number;
  readonly retryAfterMs?: number;
}
export function endpointUrl(endpoint: string | URL): URL;
export function fetchSnapshot(options: ObserveOptions): Promise<Snapshot>;
export function parseEventStream(body: ReadableStream<Uint8Array>): AsyncGenerator<EventFrame | ResetFrame | UnknownFrame>;
export function applyFrame(state: Snapshot, frame: EventFrame | UnknownFrame): Snapshot;
export function observe(options: ObserveOptions): AsyncGenerator<Update>;
