// A made-up town that plays the same way every time: for a plain browser,
// snapshots and trying things without the app. Its names are invented.
import type { Mood, Runtime, Senator, TownEvent, TownState } from "./model.ts";

const s = (id: string, name: string, runtime: Runtime, mood: Mood, chair = false, doing?: string): Senator =>
  ({ id, name, runtime, mood, chair, sessionId: `demo-${id}`, doing });

export const quiet: TownState = {
  person: "alice",
  committees: [
    { name: "senate", senators: [
      s("claude-chair", "chair", "claude", "working", true, "Town on the web stack"),
      s("claude-town", "town", "claude", "working", false, "Phaser spike"),
      s("codex-flaky", "flaky", "codex", "idle"),
      s("claude-app", "app", "claude", "waiting", false, "cargo publish -p senated"),
    ], waiting: [{ committee: "senate", root: 2577, title: "Unknown committee in ask", asker: "app" }] },
    { name: "ledger", senators: [
      s("codex-ledger", "ledger-chair", "codex", "idle", true),
      s("claude-migrate", "migrate", "claude", "working", false, "Migrate the refund tables"),
      s("agy-schema", "schema", "agy", "paused"),
    ] },
    { name: "web", senators: [s("claude-web", "web-chair", "claude", "idle", true), s("agy-css", "css", "agy", "working", false, "Colour tokens")] },
    { name: "ios-duo", senators: [
      s("codex-ios", "ios-chair", "codex", "working", true, "Sheet presentation"),
      s("claude-modal", "modal", "claude", "idle"),
    ], waiting: [{ committee: "ios-duo", root: 412, title: "Ship the modal?", asker: "ios-chair" }] },
    { name: "docs", senators: [s("claude-docs", "docs", "claude", "idle", true)] },
  ],
  hall: { secretary: s("claude-secretary", "secretary", "claude", "working", false, "Working the docket"), docket: 1 },
};

const ref = (committee: string, root: number, title: string) => ({ committee, root, title });

/** Visits within a house, across town, to the hall, and from a guest. */
export const busy: TownEvent[] = [
  { kind: "message", from: "claude-chair", to: "claude-app", thread: ref("senate", 2585, "Town page"), line: "Can you take the Town page next?" },
  { kind: "message", from: "codex-ledger", to: "claude-web", thread: ref("ledger", 88, "Shared schema"), line: "The schema moved to v3" },
  { kind: "message", from: "agy-css", to: "codex-ios", thread: ref("web", 17, "Tokens"), line: "Are the colour tokens final for iOS too?" },
  { kind: "message", from: "claude-docs", to: "claude-secretary", thread: ref("docs", 9, "Release notes"), line: "Release notes are ready for the Speaker" },
  { kind: "message", from: "reviewer", to: "claude-migrate", thread: ref("ledger", 90, "Review"), line: "Can you check the refund policy?" },
  { kind: "answered", thread: ref("ledger", 77, "Migration order"), a: "claude-migrate", b: "codex-ledger" },
];

/** Thirty committees, sixty senators: the stress town. */
export function large(): TownState {
  const moods: Mood[] = ["working", "idle", "idle", "working", "paused", "waiting", "idle", "stopped"];
  const runtimes: Runtime[] = ["claude", "codex", "agy", "claude"];
  let n = 0;
  return {
    person: "alice",
    committees: Array.from({ length: 30 }, (_, c) => ({
      name: `committee-${c}`,
      senators: [0, 1].map((k) => { const i = n++; return s(`s${i}`, `senator-${i}`, runtimes[i % 4]!, moods[(i * 3 + c) % moods.length]!, k === 0); }),
      waiting: c % 7 === 0 ? [{ committee: `committee-${c}`, root: c + 1, title: "Question" }] : [],
    })),
    hall: {},
  };
}

export function largeEvents(): TownEvent[] {
  return Array.from({ length: 14 }, (_, i) => ({
    kind: "message", from: `s${i * 4}`, to: `s${i * 4 + 9}`, thread: ref(`committee-${i * 2}`, 100 + i, "Hello"), line: `Hello from senator ${i * 4}`,
  }));
}
