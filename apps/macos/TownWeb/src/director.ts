// Turns events into the scene's actions, keeping the town calm: at most
// maxWalkers senators out at once, one visit per senator at a time, a
// short queue behind them (the oldest dropped when it overflows), moods
// held while a senator is out. Pure: the scene says when a walker is home.
import type { Mood, ThreadRef, TownEvent } from "./model.ts";

export type Action =
  | { kind: "visit"; from: string; to?: string; thread: ThreadRef; line: string; walk: boolean }
  | { kind: "mood"; senator: string; mood: Mood }
  | { kind: "arrive"; senator: string; walk: boolean }
  | { kind: "depart"; senator: string; walk: boolean }
  | { kind: "settled"; thread: ThreadRef; a?: string; b?: string };

export class Director {
  out = new Set<string>();
  queue: Action[] = [];
  held = new Map<string, Mood>();

  constructor(public maxWalkers = 6, public maxQueue = 12) {}

  /** `present`: who lives in town; a message from anyone else is a guest's visit. */
  accept(events: TownEvent[], present: Set<string>, reduceMotion: boolean): Action[] {
    const now: Action[] = [];
    for (const e of events) {
      switch (e.kind) {
        case "message": {
          const to = e.to !== undefined && present.has(e.to) ? e.to : undefined;
          const visit: Action = { kind: "visit", from: e.from, to, thread: e.thread, line: e.line, walk: !reduceMotion };
          if (reduceMotion) now.push(visit);
          else this.queue.push(visit);
          break;
        }
        case "mood":
          if (this.out.has(e.senator)) this.held.set(e.senator, e.mood);
          else now.push({ kind: "mood", senator: e.senator, mood: e.mood });
          break;
        case "left":
          this.queue = this.queue.filter((a) => !(a.kind === "visit" && a.from === e.senator));
          this.held.delete(e.senator);
          if (present.has(e.senator) && !this.out.has(e.senator))
            now.push({ kind: "depart", senator: e.senator, walk: this.letOut(e.senator, reduceMotion) });
          break;
        case "arrived":
          if (present.has(e.senator) && !this.out.has(e.senator))
            now.push({ kind: "arrive", senator: e.senator, walk: this.letOut(e.senator, reduceMotion) });
          break;
        case "answered": {
          const a = e.a !== undefined && present.has(e.a) ? e.a : undefined;
          const b = e.b !== undefined && present.has(e.b) ? e.b : undefined;
          if (a !== undefined || b !== undefined) now.push({ kind: "settled", thread: e.thread, a, b });
          break;
        }
      }
    }
    const started = this.dispatch();
    if (this.queue.length > this.maxQueue) this.queue.splice(0, this.queue.length - this.maxQueue);
    return [...now, ...started];
  }

  /** A walker is home: its held mood, and whoever can go next. */
  cameHome(senator: string): Action[] {
    this.out.delete(senator);
    const now: Action[] = [];
    const mood = this.held.get(senator);
    if (mood) {
      this.held.delete(senator);
      now.push({ kind: "mood", senator, mood });
    }
    return [...now, ...this.dispatch()];
  }

  reset() {
    this.out.clear();
    this.queue = [];
    this.held.clear();
  }

  private letOut(senator: string, reduceMotion: boolean): boolean {
    if (reduceMotion || this.out.size >= this.maxWalkers) return false;
    this.out.add(senator);
    return true;
  }

  private dispatch(): Action[] {
    const started: Action[] = [];
    const waiting: Action[] = [];
    for (const a of this.queue) {
      if (a.kind !== "visit") continue;
      if (this.out.size < this.maxWalkers && !this.out.has(a.from)) {
        this.out.add(a.from);
        started.push(a);
      } else waiting.push(a);
    }
    this.queue = waiting;
    return started;
  }
}
