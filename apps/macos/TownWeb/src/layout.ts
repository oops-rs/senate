// Where everything stands, from the committees' names alone. The town is a
// grid of lots around a civic centre: the post office (the machine daemon,
// which every message passes through) in the middle, the town hall above
// it. Each committee gets a lot along a spiral, in the order of a stable
// hash of its name, so the same names give the same town on every launch.
//
// Units are tiles (16 px) unless a name says px. y grows downwards.
import { hash } from "./model.ts";

export const TILE = 16;
/**
 * A lot, snug: the avenue (2 columns) on its left, the house (8 × 5) with a
 * column of garden either side, a yard of two rows of senators, the street
 * (2 rows) below.
 */
export const LOT_W = 14;
export const LOT_H = 11;
/** Lots each way from the centre: (2R+1)² cells. */
export const RINGS = 3;
/** Grass around the outermost lots. */
export const MARGIN = 3;
export const MAP_W = (2 * RINGS + 1) * LOT_W + 2 * MARGIN + 4;
export const MAP_H = (2 * RINGS + 1) * LOT_H + 2 * MARGIN + 2;

export interface Cell { x: number; y: number }
export interface Rect { x: number; y: number; w: number; h: number }
export interface Point { x: number; y: number }

export const POST_OFFICE: Cell = { x: 0, y: 0 };
export const TOWN_HALL: Cell = { x: 0, y: -1 };
const RESERVED = [POST_OFFICE, TOWN_HALL];
const same = (a: Cell, b: Cell) => a.x === b.x && a.y === b.y;

/** A lot's rectangle, in tiles. */
export function lot(cell: Cell): Rect {
  return { x: MARGIN + (cell.x + RINGS) * LOT_W, y: MARGIN + 2 + (cell.y + RINGS) * LOT_H, w: LOT_W, h: LOT_H };
}

/** The house on a lot: 8 × 5 tiles at its top, between avenue and garden. */
export function house(cell: Cell): Rect {
  const l = lot(cell);
  return { x: l.x + 3, y: l.y + 1, w: 8, h: 5 };
}

/** The door: the middle of the house's bottom row, in tiles. */
export function door(cell: Cell): Point {
  const h = house(cell);
  return { x: h.x + Math.floor(h.w / 2), y: h.y + h.h - 1 };
}

/** The street below a lot: its two bottom rows. Its centre line, in px. */
export const streetY = (cell: Cell) => (lot(cell).y + LOT_H - 1) * TILE;
/** The avenue on a lot's left: its two left columns. Its centre line, in px. */
export const avenueX = (cell: Cell) => (lot(cell).x + 1) * TILE;

/** Where the `i`th senator of a house stands in its yard (seven a row, two rows), in px. */
export function seat(cell: Cell, i: number): Point {
  const l = lot(cell);
  const row = Math.floor(i / 7) % 2, col = i % 7;
  return { x: (l.x + 3 + col * 1.4 + 0.5) * TILE, y: (l.y + 7 + row * 1.5) * TILE };
}

/** The rectangle the occupied lots cover, in px: what "fit" frames. */
export function occupied(cells: Cell[]): Rect {
  const ls = cells.map(lot);
  const x0 = Math.min(...ls.map((l) => l.x)), y0 = Math.min(...ls.map((l) => l.y)) - 1;
  const x1 = Math.max(...ls.map((l) => l.x + l.w)) + 2, y1 = Math.max(...ls.map((l) => l.y + l.h));
  return { x: x0 * TILE, y: y0 * TILE, w: (x1 - x0) * TILE, h: (y1 - y0) * TILE };
}

export const px = (p: Point): Point => ({ x: (p.x + 0.5) * TILE, y: (p.y + 0.5) * TILE });

/**
 * The cells of a square spiral around the centre, the civic lots left
 * out: ring 1 first, the nearest first within a ring (sides before
 * corners), then top to bottom, left to right.
 */
export function spiral(count: number): Cell[] {
  const cells: Cell[] = [];
  for (let ring = 1; ring <= RINGS && cells.length < count; ring++) {
    const ringCells: Cell[] = [];
    for (let y = -ring; y <= ring; y++)
      for (let x = -ring; x <= ring; x++)
        if (Math.max(Math.abs(x), Math.abs(y)) === ring) ringCells.push({ x, y });
    ringCells.sort((a, b) => Math.abs(a.x) + Math.abs(a.y) - (Math.abs(b.x) + Math.abs(b.y)) || a.y - b.y || a.x - b.x);
    for (const c of ringCells) if (!RESERVED.some((r) => same(r, c))) cells.push(c);
  }
  return cells.slice(0, count);
}

/** How many committees the town has lots for. */
export const CAPACITY = (2 * RINGS + 1) ** 2 - RESERVED.length;

/** Each committee's lot, in the order of the stable hash of its name. */
export function assign(names: string[]): Map<string, Cell> {
  const ordered = [...new Set(names)].sort((a, b) => {
    const ha = hash(a), hb = hash(b);
    return ha < hb ? -1 : ha > hb ? 1 : a < b ? -1 : a > b ? 1 : 0;
  });
  const cells = spiral(Math.min(ordered.length, CAPACITY));
  const out = new Map<string, Cell>();
  ordered.forEach((name, i) => { const c = cells[i]; if (c) out.set(name, c); });
  return out;
}

/**
 * The way between two lots along the streets, in px: out of the door, down
 * to the street below, along it to the avenue, up or down the avenue to
 * the other street, along it and in at the other door. Inside one lot,
 * straight.
 */
export function path(from: Point, a: Cell, to: Point, b: Cell): Point[] {
  if (same(a, b)) return [from, to];
  const da = px(door(a)), db = px(door(b));
  const ya = streetY(a), yb = streetY(b);
  const pts: Point[] = [from, da, { x: da.x, y: ya }];
  if (ya !== yb) {
    const av = avenueX(b.x >= a.x ? { x: a.x + 1, y: a.y } : a);
    pts.push({ x: av, y: ya }, { x: av, y: yb });
  }
  pts.push({ x: db.x, y: yb }, db, to);
  return dedupe(pts);
}

export function length(pts: Point[]): number {
  let d = 0;
  for (let i = 1; i < pts.length; i++) d += Math.hypot(pts[i]!.x - pts[i - 1]!.x, pts[i]!.y - pts[i - 1]!.y);
  return d;
}

function dedupe(pts: Point[]): Point[] {
  const out: Point[] = [];
  for (const p of pts) {
    const last = out[out.length - 1];
    if (!last || Math.hypot(last.x - p.x, last.y - p.y) > 0.5) out.push(p);
  }
  return out;
}

/**
 * Whether a tile is road: each lot's two left columns (the avenue) and two
 * bottom rows (the street), two columns after the last lot and two rows
 * above the first, so every lot is ringed by road.
 */
export function isRoad(tx: number, ty: number): boolean {
  const x0 = lot({ x: -RINGS, y: -RINGS }).x, y0 = lot({ x: -RINGS, y: -RINGS }).y;
  const x1 = x0 + (2 * RINGS + 1) * LOT_W + 2, y1 = y0 + (2 * RINGS + 1) * LOT_H;
  if (tx < x0 || tx >= x1 || ty < y0 - 2 || ty >= y1) return false;
  if (ty < y0 || tx >= x1 - 2) return true;
  const lx = (tx - x0) % LOT_W, ly = (ty - y0) % LOT_H;
  return lx < 2 || ly >= LOT_H - 2;
}
