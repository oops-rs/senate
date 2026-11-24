// The Town, drawn with Phaser: the Tiled map (grass, roads, trees, lots),
// a house built from tiles on each committee's lot, its senators in the
// yard, and the visits the Director makes of events. No text is drawn on
// the town: what things are shows in a card on hover (and on focus, in
// the app's List), and a click opens it.
import Phaser from "phaser";
import { Director, type Action } from "./director.ts";
import {
  assign, door, house, lot, MAP_H, MAP_W, path, length, POST_OFFICE, px, seat, TILE, TOWN_HALL,
  type Cell, type Point,
} from "./layout.ts";
import { diff, homeOf, senatorsOf, seeded, threadKey, type Senator, type ThreadRef, type TownEvent, type TownState } from "./model.ts";

/** What the pointer is over or a click lands on. */
export type Target =
  | { kind: "senator"; senator: Senator; committee?: string }
  | { kind: "committee"; name: string; senators: number; waiting: number }
  | { kind: "hall"; docket: number; secretary?: Senator }
  | { kind: "post-office" }
  | { kind: "envelope"; from: string; to?: string; thread: ThreadRef; line: string }
  | { kind: "waiting"; thread: ThreadRef; person?: string };

export interface Hooks {
  hover(target: Target | undefined, x: number, y: number): void;
  open(target: Target): void;
}

/** Frames in the people sheet (tiny-dungeon), by runtime. */
const FACE: Record<string, number> = { claude: 98, codex: 97, agy: 112, other: 85, elder: 84, person: 99, guest: 100 };

/** House tiles (tiny-town indices), row by row, 8 × 5. */
const WOOD = [
  [52, 53, 53, 53, 53, 53, 53, 54],
  [64, 65, 65, 65, 65, 65, 65, 66],
  [72, 73, 84, 73, 73, 84, 73, 75],
  [72, 73, 73, 73, 73, 73, 73, 75],
  [72, 84, 73, 73, 86, 73, 84, 75],
];
const STONE = [
  [48, 49, 49, 49, 49, 49, 49, 50],
  [60, 61, 61, 61, 61, 61, 61, 62],
  [76, 77, 88, 77, 77, 88, 77, 79],
  [76, 77, 77, 77, 77, 77, 77, 79],
  [76, 88, 77, 77, 90, 77, 88, 79],
];

const SPEED = 90; // px a second, walking

export class TownScene extends Phaser.Scene {
  hooks!: Hooks;
  reduceMotion = false;
  dark = false;

  private map!: Phaser.Tilemaps.Tilemap;
  private buildings!: Phaser.Tilemaps.TilemapLayer;
  private state: TownState = { committees: [] };
  private lots = new Map<string, Cell>();
  private people = new Map<string, Phaser.GameObjects.Sprite>();
  private cues = new Map<string, Phaser.GameObjects.GameObject>();
  private zones: Phaser.GameObjects.Zone[] = [];
  private glows: Phaser.GameObjects.GameObject[] = [];
  private night!: Phaser.GameObjects.Rectangle;
  private director = new Director();
  private fitted = false;
  /** The person has panned or zoomed: resizing no longer refits. */
  private moved = false;
  /** Walks, fades and pans going on: the loop stays awake while any runs. */
  private busy = 0;
  private awakeUntil = 0;
  private dragging = false;
  private pending?: { state: TownState; events: TownEvent[] };

  constructor() {
    super("town");
  }

  preload() {
    this.load.setPath("assets");
    this.load.tilemapTiledJSON("town", "town.tmj");
    this.load.image("tiny-town", "tiny-town-extruded.png");
    this.load.spritesheet("people", "tiny-dungeon.png", { frameWidth: 16, frameHeight: 16 });
  }

  create() {
    this.map = this.make.tilemap({ key: "town" });
    const tiles = this.map.addTilesetImage("tiny-town", "tiny-town", 16, 16, 1, 2)!;
    for (const name of ["ground", "roads", "decor"]) this.map.createLayer(name, tiles, 0, 0);
    this.buildings = this.map.createBlankLayer("buildings", tiles)!;
    this.buildings.setDepth(1);

    // The envelope: a small white letter, drawn once.
    const g = this.add.graphics();
    g.fillStyle(0x2b2420).fillRect(0, 0, 12, 9);
    g.fillStyle(0xffffff).fillRect(1, 1, 10, 7);
    g.lineStyle(1, 0x2b2420).beginPath().moveTo(1, 1).lineTo(6, 5).lineTo(11, 1).strokePath();
    g.generateTexture("envelope", 12, 9).destroy();
    const s = this.add.graphics();
    s.fillStyle(0xffd27a, 1).fillCircle(16, 16, 16);
    s.generateTexture("glow", 32, 32).destroy();
    const d = this.add.graphics();
    d.fillStyle(0xf2a93b).fillTriangle(5, 0, 10, 5, 5, 10).fillTriangle(5, 0, 0, 5, 5, 10);
    d.generateTexture("diamond", 10, 10).destroy();

    this.civic(POST_OFFICE, "post-office");
    this.civic(TOWN_HALL, "hall");

    this.night = this.add.rectangle(0, 0, 10, 10, 0x0b1630, 0.5).setOrigin(0).setScrollFactor(0).setDepth(9_000);
    this.night.setBlendMode(Phaser.BlendModes.MULTIPLY);
    this.setDark(this.dark);

    const cam = this.cameras.main;
    cam.setBounds(0, 0, MAP_W * TILE, MAP_H * TILE);
    cam.setRoundPixels(true);
    this.scale.on("resize", () => this.onResize());
    this.onResize();
    this.input.on("pointerdown", () => { this.dragging = false; this.touch(); });
    this.input.on("pointermove", (p: Phaser.Input.Pointer) => {
      this.touch();
      if (!p.isDown) return;
      const dx = p.x - p.prevPosition.x, dy = p.y - p.prevPosition.y;
      if (Math.abs(dx) + Math.abs(dy) > 0) {
        this.dragging ||= Math.hypot(p.x - p.downX, p.y - p.downY) > 4;
        cam.scrollX -= dx / cam.zoom;
        cam.scrollY -= dy / cam.zoom;
        this.moved = true;
        this.hooks.hover(undefined, 0, 0);
      }
    });
    // The wheel: two-finger scroll pans; ⌘/ctrl (a pinch arrives as ctrl) zooms at the pointer.
    this.input.on("wheel", (p: Phaser.Input.Pointer, _o: unknown, dx: number, dy: number) => {
      this.touch();
      const e = p.event as WheelEvent;
      if (e.ctrlKey || e.metaKey) this.zoomAt(Math.exp(-dy * 0.01), p.x, p.y);
      else { cam.scrollX += dx / cam.zoom; cam.scrollY += dy / cam.zoom; }
      this.moved = true;
      this.hooks.hover(undefined, 0, 0);
    });
    if (this.pending) {
      const { state, events } = this.pending;
      this.pending = undefined;
      this.setState(state, events);
    }
  }

  private updates = 0;

  override update(time: number) {
    this.updates++;
    // Sleep when nothing moves: no walk or fade running, no recent input.
    if (this.busy === 0 && !this.dragging && performance.now() > this.awakeUntil && this.tweens.getTweens().length === 0) {
      this.sleeps++;
      this.game.loop.sleep();
    }
  }

  /**
   * Something happened: draw frames again until things are still. Input
   * keeps the loop up half a second (hover, drag, wheel); a change of state
   * needs only a few frames to be drawn.
   */
  touch(hold = 500) {
    this.awakeUntil = Math.max(this.awakeUntil, performance.now() + hold);
    if (!this.game.loop.running) this.game.loop.wake();
  }

  // MARK: The page's API

  setState(state: TownState, extra: TownEvent[] = []) {
    if (!this.map) { this.pending = { state, events: extra }; return; }
    this.touch(50);
    const events = [...diff(this.state.committees.length || this.people.size ? this.state : undefined, state), ...extra];
    const before = new Set(this.state.committees.map((c) => c.name));
    const relayout = before.size !== state.committees.length || state.committees.some((c) => !before.has(c.name));
    const departing = new Set(events.flatMap((e) => (e.kind === "left" ? [e.senator] : [])));
    this.state = state;
    if (relayout) this.lots = assign(state.committees.map((c) => c.name));
    this.drawHouses();
    this.placePeople(departing);
    const present = new Set(this.people.keys());
    this.perform(this.director.accept(events, present, this.reduceMotion));
    if (!this.fitted) this.fit();
  }

  /** Events from the committees' activity (already mapped). */
  act(events: TownEvent[]) {
    if (!this.map) return;
    this.touch(50);
    this.perform(this.director.accept(events, new Set(this.people.keys()), this.reduceMotion));
  }

  setDark(dark: boolean) {
    this.dark = dark;
    if (!this.night) return;
    this.night.setVisible(dark);
    for (const l of this.glows) (l as Phaser.GameObjects.Image).setVisible(dark);
    this.touch();
  }

  fit() {
    const cam = this.cameras.main;
    const cells = [...this.lots.values(), POST_OFFICE, TOWN_HALL];
    const xs = cells.map((c) => lot(c).x), ys = cells.map((c) => lot(c).y);
    const x0 = (Math.min(...xs) - 1) * TILE, y0 = (Math.min(...ys) - 2) * TILE;
    const x1 = (Math.max(...xs) + 21) * TILE, y1 = (Math.max(...ys) + 17) * TILE;
    const zoom = Math.min(cam.width / (x1 - x0), cam.height / (y1 - y0));
    // Whole or half steps keep the pixel art crisp and the tiles seamless.
    cam.setZoom(Phaser.Math.Clamp(Math.floor(zoom * 2) / 2 || 0.5, 0.5, 4));
    this.placeNight();
    cam.centerOn((x0 + x1) / 2, (y0 + y1) / 2);
    this.fitted = true;
    this.touch();
  }

  zoomAt(factor: number, sx: number, sy: number) {
    const cam = this.cameras.main;
    const before = cam.getWorldPoint(sx, sy);
    cam.setZoom(Phaser.Math.Clamp(cam.zoom * factor, 0.5, 6));
    this.placeNight();
    const after = cam.getWorldPoint(sx, sy);
    cam.scrollX += before.x - after.x;
    cam.scrollY += before.y - after.y;
    this.touch();
  }

  private resizes = 0;
  private sleeps = 0;
  private lastSize = "";

  /**
   * The night: a fixed rectangle over the view. The camera's zoom scales
   * even fixed objects about the view's centre, so it is sized and placed
   * for the zoom each time the zoom or the view changes.
   */
  private placeNight() {
    const cam = this.cameras.main;
    const w = cam.width / cam.zoom, h = cam.height / cam.zoom;
    this.night?.setSize(w, h).setPosition((cam.width - w) / 2, (cam.height - h) / 2);
  }

  private onResize() {
    const { width, height } = this.scale.gameSize;
    this.resizes++;
    const size = `${Math.round(width)}x${Math.round(height)}`;
    if (size === this.lastSize) return;
    this.lastSize = size;
    this.cameras.main.setSize(width, height);
    this.placeNight();
    if (!this.moved && this.lots.size) this.fit();
    this.touch();
  }

  // MARK: Building

  private stamp(cell: Cell, rows: number[][]) {
    const h = house(cell);
    rows.forEach((row, y) => row.forEach((t, x) => this.buildings.putTileAt(t + 1, h.x + x, h.y + y)));
  }

  private zone(cell: Cell, target: () => Target) {
    const h = house(cell);
    const z = this.add.zone(h.x * TILE, h.y * TILE, h.w * TILE, h.h * TILE).setOrigin(0).setInteractive({ useHandCursor: true });
    z.on("pointerover", (p: Phaser.Input.Pointer) => this.hooks.hover(target(), p.x, p.y));
    z.on("pointerout", () => this.hooks.hover(undefined, 0, 0));
    z.on("pointerup", () => { if (!this.dragging) this.hooks.open(target()); });
    this.zones.push(z);
    return z;
  }

  private civic(cell: Cell, kind: "post-office" | "hall") {
    this.stamp(cell, STONE);
    const z = this.zone(cell, () =>
      kind === "hall" ? { kind: "hall", docket: this.state.hall?.docket ?? 0, secretary: this.state.hall?.secretary } : { kind: "post-office" });
    z.setData("civic", true);
    // The post office's lamp is always on: every message passes here.
    if (kind === "post-office") this.light(cell, 2, true);
  }

  /** A warm light in a window (shown at night): someone works inside. */
  private light(cell: Cell, column: number, always = false) {
    const h = house(cell);
    const l = this.add.image((h.x + column + 0.5) * TILE, (h.y + 2.5) * TILE, "glow").setBlendMode(Phaser.BlendModes.ADD)
      .setAlpha(0.55).setScale(0.6).setDepth(9_001).setVisible(this.dark);
    if (!always) l.setData("house", true);
    this.glows.push(l);
  }

  private drawHouses() {
    // Clear the committee lots (not the civic ones) and their zones and lights.
    this.buildings.forEachTile((t) => {
      if (t.index < 0) return;
      const onCivic = [POST_OFFICE, TOWN_HALL].some((c) => {
        const h = house(c);
        return t.x >= h.x && t.x < h.x + h.w && t.y >= h.y && t.y < h.y + h.h;
      });
      if (!onCivic) this.buildings.removeTileAt(t.x, t.y);
    });
    this.zones = this.zones.filter((z) => { if (z.getData("civic")) return true; z.destroy(); return false; });
    this.glows = this.glows.filter((l) => { if (!l.getData("house")) return true; l.destroy(); return false; });
    for (const c of this.cues.values()) c.destroy();
    this.cues.clear();
    for (const committee of this.state.committees) {
      const cell = this.lots.get(committee.name);
      if (!cell) continue;
      this.stamp(cell, WOOD);
      this.zone(cell, () => ({ kind: "committee", name: committee.name, senators: committee.senators.length, waiting: committee.waiting?.length ?? 0 }));
      // Lights on where someone works.
      committee.senators.filter((s) => s.mood === "working").slice(0, 3).forEach((_, i) => this.light(cell, [2, 5, 1][i]!));
      // Someone waits on the person: a visitor at the door, holding a letter.
      const first = committee.waiting?.[0];
      if (first) {
        const d = px(door(cell));
        const v = this.add.sprite(d.x + 22, d.y + 22, "people", FACE.person).setOrigin(0.5, 1).setDepth(d.y + 22)
          .setInteractive({ useHandCursor: true });
        const target: Target = { kind: "waiting", thread: first, person: this.state.person };
        v.on("pointerover", (p: Phaser.Input.Pointer) => this.hooks.hover(target, p.x, p.y));
        v.on("pointerout", () => this.hooks.hover(undefined, 0, 0));
        v.on("pointerup", () => { if (!this.dragging) this.hooks.open(target); });
        const letter = this.add.image(d.x + 29, d.y + 14, "envelope").setScale(0.6).setDepth(d.y + 23);
        this.cues.set(`visitor:${committee.name}`, v);
        this.cues.set(`letter:${committee.name}`, letter);
      }
    }
  }

  private home(id: string): { at: Point; cell: Cell } | undefined {
    const secretary = this.state.hall?.secretary;
    if (secretary?.id === id && !homeOf(this.state, id)) return { at: seat(TOWN_HALL, 2), cell: TOWN_HALL };
    for (const c of this.state.committees) {
      const i = c.senators.findIndex((s) => s.id === id);
      const cell = this.lots.get(c.name);
      if (i >= 0 && cell) return { at: seat(cell, i), cell };
    }
    return undefined;
  }

  private placePeople(departing: Set<string>) {
    const all = senatorsOf(this.state);
    for (const [id, sprite] of this.people) {
      if (!all.has(id) && !departing.has(id)) { sprite.destroy(); this.people.delete(id); }
    }
    for (const [id, s] of all) {
      const home = this.home(id);
      if (!home) continue;
      let sprite = this.people.get(id);
      if (!sprite) {
        sprite = this.add.sprite(home.at.x, home.at.y, "people", FACE[s.runtime] ?? FACE.other).setOrigin(0.5, 1)
          .setInteractive({ useHandCursor: true });
        sprite.on("pointerover", (p: Phaser.Input.Pointer) => this.hooks.hover(this.senatorTarget(id), p.x, p.y));
        sprite.on("pointerout", () => this.hooks.hover(undefined, 0, 0));
        sprite.on("pointerup", () => { const t = this.senatorTarget(id); if (t && !this.dragging) this.hooks.open(t); });
        this.people.set(id, sprite);
      }
      sprite.setData("home", home);
      if (!this.director.out.has(id)) this.settle(id);
    }
  }

  private senatorTarget(id: string): Target | undefined {
    const s = senatorsOf(this.state).get(id);
    return s && { kind: "senator", senator: s, committee: homeOf(this.state, id) };
  }

  /** Put a senator in its place for its mood: wordless cues only. */
  private settle(id: string) {
    const sprite = this.people.get(id);
    const s = senatorsOf(this.state).get(id);
    const home = sprite?.getData("home") as { at: Point } | undefined;
    if (!sprite || !s || !home) return;
    this.tweens.killTweensOf(sprite);
    sprite.setPosition(home.at.x, home.at.y).setDepth(home.at.y).setAngle(0).setAlpha(1).setVisible(s.mood !== "stopped");
    const cueKey = `mood:${id}`;
    this.cues.get(cueKey)?.destroy();
    this.cues.delete(cueKey);
    if (s.mood === "paused") sprite.setAngle(-90).setAlpha(0.65); // lying down, asleep
    if (s.mood === "waiting") {
      const cue = this.add.image(home.at.x, home.at.y - 21, "diamond").setDepth(home.at.y + 1);
      this.cues.set(cueKey, cue);
    }
    if (s.mood === "working") sprite.setFlipX(false); // facing the house
  }

  // MARK: Acting

  private perform(actions: Action[]) {
    for (const a of actions) {
      switch (a.kind) {
        case "mood": if (!this.director.out.has(a.senator)) this.settle(a.senator); break;
        case "visit": this.visit(a); break;
        case "arrive": this.comeOrGo(a.senator, true, a.walk); break;
        case "depart": this.comeOrGo(a.senator, false, a.walk); break;
        case "settled": this.settled([a.a, a.b]); break;
      }
    }
  }

  /** Walk a sprite along points, a little hop for each step; then done. */
  private walk(sprite: Phaser.GameObjects.Sprite, pts: Point[], done: () => void) {
    this.busy++;
    // Never a long trudge: a walk takes at most about four seconds.
    const speed = Math.max(SPEED, length(pts) / 4);
    const legs: Phaser.Types.Tweens.TweenBuilderConfig[] = [];
    for (let i = 1; i < pts.length; i++) {
      const a = pts[i - 1]!, b = pts[i]!;
      const d = Math.hypot(b.x - a.x, b.y - a.y);
      if (d < 0.5) continue;
      legs.push({
        targets: sprite, x: b.x, y: b.y, duration: (d / speed) * 1000,
        onStart: () => sprite.setFlipX(b.x < a.x),
        onUpdate: () => sprite.setDepth(sprite.y),
      } as Phaser.Types.Tweens.TweenBuilderConfig);
    }
    const bob = this.tweens.add({ targets: sprite, scaleY: 0.9, duration: 140, yoyo: true, repeat: -1 });
    this.tweens.chain({
      tweens: legs.length ? legs : [{ targets: sprite, alpha: sprite.alpha, duration: 1 }],
      onComplete: () => { bob.stop(); sprite.setScale(1); this.busy--; done(); },
    } as unknown as Phaser.Types.Tweens.TweenChainBuilderConfig);
  }

  private visit(a: Extract<Action, { kind: "visit" }>) {
    const sender = this.people.get(a.from);
    const home = sender?.getData("home") as { at: Point; cell: Cell } | undefined;
    const office = px(door(POST_OFFICE));
    const target = this.destination(a.to, a.thread);
    if (!a.walk) {
      // Reduce Motion: the letter fades in by the sender, then by the receiver.
      const from = home?.at ?? office;
      this.letter(a, from.x, from.y - 22, 1200, () => this.letter(a, target.at.x, target.at.y - 22, 1400, () => {}));
      return;
    }
    const goHome = () => this.perform(this.director.cameHome(a.from));
    // The letter goes from the post office to the receiver: every message passes the daemon.
    const send = () => {
      const env = this.letterSprite(a, office.x, office.y);
      const route = path(office, POST_OFFICE, target.at, target.cell);
      this.busy++;
      this.tweens.chain({
        tweens: this.legs(env, route, Math.max(SPEED * 2.2, length(route) / 2.5)),
        onComplete: () => this.tweens.add({ targets: env, alpha: 0, duration: 400, onComplete: () => { env.destroy(); this.busy--; } }),
      } as unknown as Phaser.Types.Tweens.TweenChainBuilderConfig);
    };
    if (!sender || !home) {
      // A guest from elsewhere: in from the west road, to the post office, and away.
      const g = this.add.sprite(0, office.y, "people", FACE.guest).setOrigin(0.5, 1);
      const edge = { x: TILE, y: (lot(POST_OFFICE).y + 15) * TILE };
      const route = [edge, { x: office.x, y: edge.y }, office];
      g.setPosition(edge.x, edge.y);
      this.walk(g, route, () => { send(); this.walk(g, [...route].reverse(), () => { g.destroy(); goHome(); }); });
      return;
    }
    const there = path(home.at, home.cell, office, POST_OFFICE);
    this.walk(sender, there, () => {
      send();
      this.walk(sender, [...there].reverse(), () => { this.settle(a.from); goHome(); });
    });
  }

  private legs(obj: Phaser.GameObjects.Image, pts: Point[], speed: number) {
    const legs: Phaser.Types.Tweens.TweenBuilderConfig[] = [];
    for (let i = 1; i < pts.length; i++) {
      const a = pts[i - 1]!, b = pts[i]!;
      const d = Math.hypot(b.x - a.x, b.y - a.y);
      if (d >= 0.5) legs.push({ targets: obj, x: b.x, y: b.y - 18, duration: (d / speed) * 1000 } as Phaser.Types.Tweens.TweenBuilderConfig);
    }
    return legs.length ? legs : [{ targets: obj, alpha: 1, duration: 1 } as Phaser.Types.Tweens.TweenBuilderConfig];
  }

  private letterSprite(a: Extract<Action, { kind: "visit" }>, x: number, y: number) {
    const env = this.add.image(x, y - 18, "envelope").setDepth(8_000).setInteractive({ useHandCursor: true });
    const target: Target = { kind: "envelope", from: a.from, to: a.to, thread: a.thread, line: a.line };
    env.on("pointerover", (p: Phaser.Input.Pointer) => this.hooks.hover(target, p.x, p.y));
    env.on("pointerout", () => this.hooks.hover(undefined, 0, 0));
    env.on("pointerup", () => { if (!this.dragging) this.hooks.open(target); });
    return env;
  }

  private letter(a: Extract<Action, { kind: "visit" }>, x: number, y: number, hold: number, done: () => void) {
    const env = this.letterSprite(a, x, y + 18).setAlpha(0);
    this.busy++;
    this.tweens.chain({
      tweens: [{ targets: env, alpha: 1, duration: 300 }, { targets: env, alpha: 1, duration: hold }, { targets: env, alpha: 0, duration: 300 }],
      onComplete: () => { env.destroy(); this.busy--; done(); },
    } as unknown as Phaser.Types.Tweens.TweenChainBuilderConfig);
  }

  /** Where a letter goes: the receiver, else the thread's house, else the hall. */
  private destination(to: string | undefined, thread: ThreadRef): { at: Point; cell: Cell } {
    const home = to ? (this.people.get(to)?.getData("home") as { at: Point; cell: Cell } | undefined) : undefined;
    if (home) return home;
    const cell = this.lots.get(thread.committee) ?? TOWN_HALL;
    return { at: px(door(cell)), cell };
  }

  private comeOrGo(id: string, arriving: boolean, walk: boolean) {
    const sprite = this.people.get(id);
    const home = sprite?.getData("home") as { at: Point; cell: Cell } | undefined;
    const finish = () => {
      if (!arriving && !senatorsOf(this.state).has(id)) { sprite?.destroy(); this.people.delete(id); }
      else this.settle(id);
      if (walk) this.perform(this.director.cameHome(id));
    };
    if (!sprite || !home) return finish();
    if (!walk) {
      sprite.setVisible(true).setAlpha(arriving ? 0 : 1);
      this.busy++;
      this.tweens.add({ targets: sprite, alpha: arriving ? 1 : 0, duration: 500, onComplete: () => { this.busy--; finish(); } });
      return;
    }
    const d = px(door(home.cell));
    const y = (lot(home.cell).y + 15) * TILE;
    const route = [{ x: TILE, y }, { x: d.x, y }, d, home.at];
    if (!arriving) route.reverse();
    sprite.setVisible(true).setAlpha(1).setAngle(0).setPosition(route[0]!.x, route[0]!.y);
    this.cues.get(`mood:${id}`)?.destroy();
    this.walk(sprite, route, finish);
  }

  /** A settled thread: a small green spark over each of the two. */
  private settled(ids: (string | undefined)[]) {
    for (const id of ids) {
      const s = id ? this.people.get(id) : undefined;
      if (!s || !s.visible) continue;
      const spark = this.add.image(s.x, s.y - 24, "glow").setTint(0x6bd68d).setScale(0.25).setAlpha(0).setDepth(8_500);
      this.busy++;
      this.tweens.chain({
        tweens: [{ targets: spark, alpha: 0.9, scale: 0.4, y: s.y - 30, duration: 250 }, { targets: spark, alpha: 0, duration: 900, delay: 600 }],
        onComplete: () => { spark.destroy(); this.busy--; },
      } as unknown as Phaser.Types.Tweens.TweenChainBuilderConfig);
    }
  }

  // MARK: Probing (snapshots and tests)

  /** Hover (or click) the first working senator the way the pointer would. */
  probe(click: boolean): boolean {
    const id = [...senatorsOf(this.state).values()].find((s) => s.mood === "working" && this.people.has(s.id))?.id;
    const sprite = id ? this.people.get(id) : undefined;
    const target = id ? this.senatorTarget(id) : undefined;
    if (!sprite || !target) return false;
    const cam = this.cameras.main;
    const x = (sprite.x - cam.worldView.x) * cam.zoom, y = (sprite.y - 8 - cam.worldView.y) * cam.zoom;
    if (click) this.hooks.open(target); else this.hooks.hover(target, x, y);
    this.touch();
    return true;
  }

  // MARK: Measurement

  stats() {
    return { fps: Math.round(this.game.loop.actualFps), sleeping: !this.game.loop.running, people: this.people.size, busy: this.busy,
      resizes: this.resizes, sleeps: this.sleeps, updates: this.updates, hidden: document.hidden, awake: Math.max(0, Math.round(this.awakeUntil - performance.now())) };
  }

  /** Keys of the threads on screen now: what a click may name. */
  threads(): string[] {
    return this.state.committees.flatMap((c) => (c.waiting ?? []).map(threadKey));
  }

  /** A deterministic jitter for decoration. */
  static rnd = seeded("town");
}

export { length };
