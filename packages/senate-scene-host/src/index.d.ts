import type { EventFrame, Snapshot, UnknownFrame } from "@senate/observe";

export const HOST_VERSION: 1;
export type Status = "ready" | "access_needed" | "data_unavailable" | "version_mismatch";
export interface Environment {
  color_scheme: "light" | "dark";
  reduced_motion: boolean;
  locale: string;
  time_zone: string;
  visible: boolean;
  viewport: { width: number; height: number };
  safe_area: { top: number; right: number; bottom: number; left: number };
}

export type HostEvent = { host_version: 1 } & (
  | { type: "status"; data: { state: Status } }
  | { type: "observe.snapshot"; data: { snapshot: Snapshot } }
  | { type: "observe.event"; data: { frame: EventFrame | UnknownFrame } }
  | { type: "environment"; data: Environment }
);

export interface SceneHost {
  ready(): Promise<{ version: 1; capabilities: string[] }>;
  open(url: string): Promise<void>;
  writeCard(schema: string, value: Record<string, unknown> | null): Promise<void>;
  storage: {
    get(key: string): Promise<unknown>;
    set(key: string, value: unknown): Promise<void>;
    delete(key: string): Promise<void>;
  };
  subscribe(listener: (event: HostEvent) => void): () => void;
  close(): void;
}

export function createSceneHost(port: MessagePort): SceneHost;
export function fromParent(parent?: Window): Promise<SceneHost>;
