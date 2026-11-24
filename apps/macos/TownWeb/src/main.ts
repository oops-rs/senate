// The Town page: a Phaser game in a page of its own. In the app it gets the
// town's state and the committees' activity from Swift and sends back what
// to open; in a plain browser it plays the scripted town.
import Phaser from "phaser";
import { activityEvents, firstWords, type ActivityEntry, type TownEvent, type TownState } from "./model.ts";
import { TownScene, type Target } from "./scene.ts";
import * as script from "./script.ts";

interface Native { postMessage(body: unknown): void }
const native: Native | undefined = (window as unknown as { webkit?: { messageHandlers?: { town?: Native } } }).webkit?.messageHandlers?.town;

const scene = new TownScene();
const card = document.getElementById("card")!;
const params = new URLSearchParams(location.search);
const motion = window.matchMedia("(prefers-reduced-motion: reduce)");
const darkQuery = window.matchMedia("(prefers-color-scheme: dark)");
scene.reduceMotion = params.has("reduce-motion") || motion.matches;
scene.dark = params.get("dark") === "1" || (params.get("dark") !== "0" && darkQuery.matches);
document.documentElement.dataset.theme = scene.dark ? "dark" : "light";

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
    card.style.left = `${Math.min(x + 14, innerWidth - r.width - 8)}px`;
    card.style.top = `${Math.min(y + 18, innerHeight - r.height - 8)}px`;
  },
  open(target) {
    const body = openAction(target);
    if (!body) return;
    if (native) native.postMessage({ action: "open", ...body });
    else toast(`open ${body.kind} ${body.id}`);
  },
};

/** What a hover card says: the who and what, never drawn on the town. */
function describe(t: Target): string[] {
  switch (t.kind) {
    case "senator": {
      const s = t.senator;
      return [`${s.name}${s.chair ? " · chair" : ""}`, `${s.runtime} · ${s.mood}${t.committee ? ` · ${t.committee}` : " · town hall"}`, ...(s.doing ? [s.doing] : [])];
    }
    case "committee": return [t.name, `${t.senators} senator${t.senators === 1 ? "" : "s"}${t.waiting ? ` · ${t.waiting} waiting on you` : ""}`];
    case "hall": return ["Town hall · the Speaker's desk", `docket ${t.docket}${t.secretary ? ` · ${t.secretary.name} ${t.secretary.mood}` : ""}`];
    case "post-office": return ["Post office · senated --machine", "every message passes through bridge.sock"];
    case "envelope": return [`${t.from} → ${t.to ?? t.thread.committee}`, `“${firstWords(t.line, 12, 80)}”`, `${t.thread.committee} #${t.thread.root}`];
    case "waiting": return [`Waiting on ${t.person ?? "you"}`, t.thread.title, ...(t.thread.asker ? [`asked by ${t.thread.asker}`] : [])];
  }
}

function openAction(t: Target): { kind: string; id: string } | undefined {
  switch (t.kind) {
    case "senator": return t.senator.sessionId ? { kind: "session", id: t.senator.sessionId } : t.committee ? { kind: "committee", id: t.committee } : undefined;
    case "committee": return { kind: "committee", id: t.name };
    case "hall": return { kind: "hall", id: "" };
    case "envelope": case "waiting": return { kind: "thread", id: `${t.thread.committee}:${t.thread.root}` };
    case "post-office": return undefined;
  }
}

function toast(text: string) {
  const el = document.getElementById("toast")!;
  el.textContent = text;
  el.hidden = false;
  setTimeout(() => { el.hidden = true; }, 1600);
}

const game = new Phaser.Game({
  type: Phaser.AUTO,
  parent: "town",
  pixelArt: true,
  backgroundColor: "#7fb069",
  banner: false,
  audio: { noAudio: true },
  disableContextMenu: true,
  // Images straight from the app's own scheme: no blob: URLs.
  loader: { imageLoadType: "HTMLImageElement" },
  scale: { mode: Phaser.Scale.RESIZE, width: innerWidth, height: innerHeight },
  scene,
});

// The loop sleeps when the town is still; any input wakes it.
for (const type of ["pointermove", "pointerdown", "wheel", "keydown"]) {
  window.addEventListener(type, () => scene.touch(), { passive: true });
}

/** The page's API, called by the app with plain data. */
const api = {
  setState(state: TownState, entries: ActivityEntry[] = []) {
    scene.setState(state, activityEvents(entries));
  },
  activity(entries: ActivityEntry[]) { scene.act(activityEvents(entries)); },
  events(events: TownEvent[]) { scene.act(events); },
  setOptions(o: { reduceMotion?: boolean; dark?: boolean; visible?: boolean }) {
    if (o.reduceMotion !== undefined) scene.reduceMotion = o.reduceMotion;
    if (o.dark !== undefined) { scene.setDark(o.dark); document.documentElement.dataset.theme = o.dark ? "dark" : "light"; }
    if (o.visible === false) { game.loop.sleep(); game.pause(); }
    if (o.visible === true) { game.resume(); scene.touch(); }
  },
  fit() { scene.fit(); },
  zoom(f: number) { scene.zoomAt(f, innerWidth / 2, innerHeight / 2); },
  stats() { return scene.stats(); },
  /** Hover or click the first working senator (snapshots). */
  probe(click = false) { return scene.probe(click); },
};
(window as unknown as { town: typeof api }).town = api;

if (params.has("stats")) setInterval(() => console.log("town-stats " + JSON.stringify({ ...scene.stats(), tweens: scene.tweens.getTweens().length })), 1000);

if (native) {
  native.postMessage({ action: "ready" });
} else {
  // A plain browser: the scripted town, busy every few seconds.
  const big = params.has("large");
  api.setState(big ? script.large() : script.quiet);
  const play = () => api.events(big ? script.largeEvents() : script.busy);
  setTimeout(play, params.has("now") ? 300 : 1500);
  if (!params.has("once")) setInterval(play, 14_000);
}
