// The Town page: the Works in a page of its own. In the app it gets the
// town's state and the committees' activity from Swift and sends back what
// to open; in a plain browser (or the app's demo) it plays the scripted town.
import Phaser from "phaser";
import type { Daylight } from "./art.ts";
import { activityEvents, firstWords, type ActivityEntry, type TownEvent, type TownState } from "./model.ts";
import * as script from "./script.ts";
import { Works, type Target } from "./works.ts";

interface Native { postMessage(body: unknown): void }
const native: Native | undefined = (window as unknown as { webkit?: { messageHandlers?: { town?: Native } } }).webkit?.messageHandlers?.town;

const scene = new Works();
const card = document.getElementById("card")!;
const params = new URLSearchParams(location.search);
const dpr = Math.min(window.devicePixelRatio || 1, 3);
scene.reduceMotion = params.has("reduce-motion") || window.matchMedia("(prefers-reduced-motion: reduce)").matches;
scene.dark = params.get("dark") === "1" || (params.get("dark") !== "0" && window.matchMedia("(prefers-color-scheme: dark)").matches);
const time = params.get("time");
if (time === "dawn" || time === "day" || time === "dusk" || time === "night") scene.fixedTime = time as Daylight;
document.documentElement.dataset.theme = scene.dark ? "dark" : "light";

const ago = (s: number) => s < 3600 ? `${Math.round(s / 60)} min` : s < 86_400 ? `${Math.round(s / 3600)} h` : s < 14 * 86_400 ? `${Math.round(s / 86_400)} days` : `${Math.round(s / (7 * 86_400))} weeks`;

/** What a hover card says: the who and what, never drawn on the town. */
function describe(t: Target): string[] {
  switch (t.kind) {
    case "senator": {
      const s = t.senator;
      return [`${s.name}${s.chair ? " · chair" : ""}`, `${s.runtime} · ${s.mood}${t.committee ? ` · ${t.committee}` : " · town hall"}${s.watched ? " · someone is watching" : ""}`, ...(s.doing ? [s.doing] : [])];
    }
    case "elder": return [t.elder.name, `${t.elder.runtime} · last worked ${ago(t.elder.idle)} ago${t.elder.committee ? ` · ${t.elder.committee}` : ""}`, "opens its conversation in History"];
    case "workshop": return [`${t.name} workshop`, `${t.senators} senator${t.senators === 1 ? "" : "s"}${t.waiting ? ` · ${t.waiting} waiting on you` : ""}`];
    case "hall": return ["Sorting hall · senated --machine", "every message passes through bridge.sock", `${t.passed} sorted since you opened the town`];
    case "archive": return ["Archive · the Senate server's logs", "each message stamped and filed by seq", ...t.filed.filter(([, n]) => n > 0).slice(0, 5).map(([c, n]) => `${c}: ${n} filed here`)];
    case "townhall": return ["Town hall · the Speaker's desk", `docket ${t.docket}${t.secretary ? ` · ${t.secretary.name} ${t.secretary.mood}` : ""}`];
    case "memory": return ["Memory hall · History", `${t.sessions} ended sessions, each readable`];
    case "capsule": return [`${t.from} → ${t.to ?? t.thread.committee}`, `“${firstWords(t.line, 12, 80)}”`,
      `via the sorting hall, filed at ${t.thread.committee} #${t.thread.root}`, ...(t.held ? [`held: ${t.held}`] : [])];
    case "signal": return [`${t.committee}'s signal`, t.held ? `red: ${t.held}` : "green: deliveries go straight in"];
    case "gate": return [`Approval gate · ${t.approval.person ?? "a person"} must sign`, t.approval.ask, `for ${t.approval.senator}`];
    case "worker": return [`${({ bash: "builder", edit: "scribe", read: "librarian", grep: "searcher", mcp: "courier", web: "explorer" } as const)[t.tool]} · ${t.tool}`, `for ${t.senator}`, ...(t.target ? [t.target] : [])];
    case "monument": return t.next
      ? [`${t.project.name} · next release`, `${t.project.unreleased?.merges ?? 0} merged so far`, ...(t.project.unreleased?.plaques ?? [])]
      : [`${t.project.name} ${t.release!.name}${t.release!.date ? ` · ${t.release!.date}` : ""}`, `${t.release!.merges} merged`, ...t.release!.plaques];
    case "link": return [t.link === "parent" ? `${t.name} · the parent Senate` : t.link === "child" ? `${t.name} · a child Senate` : `${t.name} · a paired machine`,
      `${t.link === "parent" ? "the uplink" : t.link === "child" ? "the road" : "the rail"} is ${t.up ? "up" : "down"}`];
    case "waiting": return [`Waiting on ${t.person ?? "you"}`, t.thread.title, ...(t.thread.asker ? [`asked by ${t.thread.asker}`] : [])];
    case "gatehouse": return ["The town gate", "explorers leave here for the web"];
  }
}

function openAction(t: Target): { kind: string; id: string } | undefined {
  switch (t.kind) {
    case "senator": return t.senator.sessionId ? { kind: "session", id: t.senator.sessionId } : t.committee ? { kind: "committee", id: t.committee } : undefined;
    case "elder": return t.elder.sessionId ? { kind: "past", id: t.elder.sessionId } : undefined;
    case "workshop": return { kind: "committee", id: t.name };
    case "townhall": return { kind: "hall", id: "" };
    case "memory": return { kind: "history", id: "" };
    case "gate": return { kind: "approvals", id: "" };
    case "capsule": case "waiting": return { kind: "thread", id: `${t.thread.committee}:${t.thread.root}` };
    default: return undefined;
  }
}

scene.hooks = {
  hover(target, x, y) {
    if (!target) { card.hidden = true; return; }
    card.replaceChildren(...describe(target).map((line, i) => {
      const el = document.createElement(i === 0 ? "strong" : "span");
      el.textContent = line;
      return el;
    }));
    card.hidden = false;
    const r = card.getBoundingClientRect();
    card.style.left = `${Math.min(x / dpr + 14, innerWidth - r.width - 8)}px`;
    card.style.top = `${Math.min(y / dpr + 18, innerHeight - r.height - 8)}px`;
  },
  open(target) {
    const body = openAction(target);
    if (!body) return;
    if (native) native.postMessage({ action: "open", ...body });
    else toast(`open ${body.kind} ${body.id}`.trim());
  },
};

function toast(text: string) {
  const el = document.getElementById("toast")!;
  el.textContent = text;
  el.hidden = false;
  setTimeout(() => { el.hidden = true; }, 1600);
}

// A canvas backed at the screen's pixel density, shown at the page's size.
const game = new Phaser.Game({
  type: Phaser.AUTO,
  parent: "town",
  backgroundColor: "#bfe0a6",
  banner: false,
  audio: { noAudio: true },
  disableContextMenu: true,
  antialias: true,
  loader: { imageLoadType: "HTMLImageElement" },
  scale: { mode: Phaser.Scale.NONE, width: innerWidth * dpr, height: innerHeight * dpr, zoom: 1 / dpr },
  scene,
});
window.addEventListener("resize", () => {
  game.scale.resize(innerWidth * dpr, innerHeight * dpr);
  game.scale.setZoom(1 / dpr);
});

// The loop sleeps when the town is still; any input wakes it.
for (const type of ["pointermove", "pointerdown", "wheel", "keydown"]) {
  window.addEventListener(type, () => scene.touch(), { passive: true });
}

let playing: number[] = [];
function play(timeline: [number, TownEvent[]][], loop: boolean) {
  for (const t of playing) clearTimeout(t);
  playing = [];
  let at = 0;
  for (const [delay, events] of timeline) {
    at += delay;
    playing.push(window.setTimeout(() => scene.act(events), at));
  }
  if (loop) playing.push(window.setTimeout(() => { scene.setState(big ? script.large() : script.quiet); play(timeline, loop); }, at + 9_000));
}
const big = params.has("large") || params.get("demo") === "large";

/** The page's API, called by the app with plain data. */
const api = {
  setState(state: TownState, entries: ActivityEntry[] = []) { scene.setState(state, activityEvents(entries)); },
  activity(entries: ActivityEntry[]) { scene.act(activityEvents(entries)); },
  events(events: TownEvent[]) { scene.act(events); },
  setOptions(o: { reduceMotion?: boolean; dark?: boolean; visible?: boolean; time?: Daylight }) {
    if (o.reduceMotion !== undefined) scene.reduceMotion = o.reduceMotion;
    if (o.time) scene.fixedTime = o.time;
    if (o.dark !== undefined) { scene.setDark(o.dark); document.documentElement.dataset.theme = o.dark ? "dark" : "light"; }
    if (o.visible === false) { game.loop.sleep(); game.pause(); }
    if (o.visible === true) { game.resume(); scene.touch(); }
  },
  /** The scripted town (the app's demo): "small" or "large". */
  demo(kind: string) {
    const isBig = kind === "large";
    scene.setState(isBig ? script.large() : script.quiet);
    play(isBig ? script.largeBusy() : script.busy, true);
  },
  fit() { scene.fit(); },
  zoom(f: number) { scene.zoomAt(f, (innerWidth / 2) * dpr, (innerHeight / 2) * dpr); },
  stats() { return scene.stats(); },
  probe(click = false) { return scene.probe(click); },
};
(window as unknown as { town: typeof api }).town = api;

if (params.has("stats")) setInterval(() => console.log("town-stats " + JSON.stringify(scene.stats())), 1000);

if (native) {
  native.postMessage({ action: "ready" });
} else {
  // A plain browser: the scripted town.
  scene.setState(big ? script.large() : script.quiet);
  const timeline = big ? script.largeBusy() : script.busy;
  setTimeout(() => play(timeline, !params.has("once")), params.has("now") ? 200 : 1200);
}
