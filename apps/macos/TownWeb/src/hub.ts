// The Works: a hub-and-ring town. The sorting hall (the machine daemon,
// which every message passes) sits at the centre with the archive (the
// server's logs) and the town hall (the Speaker's desk) beside it; each
// committee's workshop sits on a ring around it, joined to the hall by its
// own tube. Rings fill from the inside out; the order on them is the stable
// hash of the committees' names, so the same names make the same town.
// The memory hall, the monument quarter, the districts beyond, the rail
// and the town gate stand just outside the outermost ring, so a small town
// stays compact and a big one grows outwards.
//
// World units are px at zoom 1, the hub at (0, 0), y down. Pure: no Phaser.
import { hash } from "./model.ts";

export interface Pt { x: number; y: number }

/** A workshop: where it stands, which ring, the direction from the hub. */
export interface Slot { at: Pt; ring: number; angle: number }

export const WORKSHOP = { w: 200, h: 136 };
/** The archive and the town hall are drawn at this scale, in the civic wedge. */
export const CIVIC_SCALE = 0.75;
export const HALL_R = 118;
const FIRST_RING = 560;
const RING_STEP = 370;
const SPACING = 245;

export interface Plan {
  slots: Map<string, Slot>;
  /** The radius past the last ring's workshops. */
  outer: number;
  civic: {
    hall: Pt; archive: Pt; townHall: Pt;
    memory: Pt; promenade: Pt[]; monuments: Pt;
    gate: Pt; parent: Pt; children: Pt[]; rail: number;
  };
}

/**
 * The civic wedge: no workshop stands north of the hall within this half
 * angle, so no tube crosses the archive, the town hall or the uplink.
 */
export const WEDGE = (35 * Math.PI) / 180;

/** How many workshops a ring holds. */
export function capacity(ring: number): number {
  return Math.floor(((2 * Math.PI - 2 * WEDGE) * (FIRST_RING + ring * RING_STEP)) / SPACING);
}

export function ordered(names: string[]): string[] {
  return [...new Set(names)].sort((a, b) => {
    const ha = hash(a), hb = hash(b);
    return ha < hb ? -1 : ha > hb ? 1 : a < b ? -1 : a > b ? 1 : 0;
  });
}

export function plan(names: string[]): Plan {
  const all = ordered(names);
  const slots = new Map<string, Slot>();
  let i = 0, ring = 0;
  while (i < all.length) {
    const n = Math.min(capacity(ring), all.length - i);
    const r = FIRST_RING + ring * RING_STEP;
    // Spread over the ring outside the civic wedge; odd rings half a step
    // round, so their tubes pass between the inner workshops.
    const span = 2 * Math.PI - 2 * WEDGE;
    const offset = ring % 2 === 1 ? 0.25 : 0;
    for (let k = 0; k < n; k++) {
      const angle = -Math.PI / 2 + WEDGE + ((k + 0.5 + offset) * span) / Math.max(n, 1);
      slots.set(all[i + k]!, { at: { x: Math.cos(angle) * r, y: Math.sin(angle) * r }, ring, angle });
    }
    i += n;
    ring++;
  }
  const lastRing = Math.max(0, ring - 1);
  const outer = FIRST_RING + lastRing * RING_STEP + WORKSHOP.w * 0.75;
  const edge = Math.max(outer, 640);
  const memory = { x: -edge - 150, y: -edge * 0.5 };
  return {
    slots,
    outer: edge,
    civic: {
      hall: { x: 0, y: 0 },
      archive: { x: 100, y: -395 },
      townHall: { x: -100, y: -395 },
      memory,
      promenade: [
        { x: memory.x - 150, y: memory.y + 150 }, { x: memory.x + 170, y: memory.y + 150 },
        { x: memory.x + 170, y: memory.y + 330 }, { x: memory.x - 150, y: memory.y + 330 },
      ],
      monuments: { x: -edge - 130, y: edge * 0.42 },
      gate: { x: -edge - 420, y: 40 },
      parent: { x: 0, y: -edge - 170 },
      children: [{ x: -300, y: edge + 120 }, { x: 300, y: edge + 120 }],
      rail: edge + 150,
    },
  };
}

/** Where a workshop's tube leaves it: the point of its outline facing the hub. */
export function port(slot: Slot): Pt {
  const ux = -Math.cos(slot.angle), uy = -Math.sin(slot.angle);
  // The rectangle's edge along (ux, uy) from its centre.
  const t = Math.min(WORKSHOP.w / 2 / Math.max(Math.abs(ux), 1e-6), WORKSHOP.h / 2 / Math.max(Math.abs(uy), 1e-6));
  return { x: slot.at.x + ux * t, y: slot.at.y + uy * t };
}

/** A workshop's tube to the hall, as points for a smooth curve: from its port to the hall's rim. */
export function tube(slot: Slot): Pt[] {
  const p = port(slot);
  const rim = { x: Math.cos(slot.angle) * HALL_R, y: Math.sin(slot.angle) * HALL_R };
  // A gentle bend, the same way for every tube: the town turns like a wheel.
  const mid = { x: (p.x + rim.x) / 2, y: (p.y + rim.y) / 2 };
  const len = Math.hypot(p.x - rim.x, p.y - rim.y);
  const bend = Math.min(60, len * 0.12);
  const ctrl = { x: mid.x - Math.sin(slot.angle) * bend, y: mid.y + Math.cos(slot.angle) * bend };
  return [p, ctrl, rim];
}

/** A point on a quadratic tube at t in [0, 1]. */
export function along(t3: Pt[], t: number): Pt {
  const [a, c, b] = t3 as [Pt, Pt, Pt];
  const u = 1 - t;
  return { x: u * u * a.x + 2 * u * t * c.x + t * t * b.x, y: u * u * a.y + 2 * u * t * c.y + t * t * b.y };
}

/** Where a workshop's signal stands: near its end of the tube. */
export const signalAt = (slot: Slot) => along(tube(slot), 0.18);

/** The box everything stands in: what "fit" frames. */
export function bounds(p: Plan): { x: number; y: number; w: number; h: number } {
  const e = p.outer;
  const x0 = p.civic.gate.x - 70, x1 = p.civic.rail + 70;
  const y0 = p.civic.parent.y - 80, y1 = p.civic.children[0]!.y + 70;
  return { x: x0, y: Math.min(y0, -e - 200), w: x1 - x0, h: Math.max(y1, e + 200) - Math.min(y0, -e - 200) };
}
