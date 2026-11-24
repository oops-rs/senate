import { describe, expect, test } from "vitest";
import { assign, CAPACITY, door, house, isRoad, lot, MAP_H, MAP_W, path, POST_OFFICE, px, seat, spiral, TILE, TOWN_HALL } from "../src/layout.ts";

const names = Array.from({ length: 30 }, (_, i) => `committee-${i}`);
const overlaps = (a: { x: number; y: number; w: number; h: number }, b: typeof a) =>
  a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;

describe("layout", () => {
  test("the same names give the same town whatever their order", () => {
    expect([...assign(names)].sort()).toEqual([...assign([...names].reverse())].sort());
    const cells = [...assign(names).values()].map((c) => `${c.x},${c.y}`);
    expect(new Set(cells).size).toBe(30);
    expect(cells).not.toContain(`${POST_OFFICE.x},${POST_OFFICE.y}`);
    expect(cells).not.toContain(`${TOWN_HALL.x},${TOWN_HALL.y}`);
  });
  test("a small town sits next to its centre", () => {
    const c = assign(["only"]).get("only")!;
    expect(Math.abs(c.x) + Math.abs(c.y)).toBe(1);
  });
  test("thirty committees fit; the spiral fills nearest first", () => {
    expect(CAPACITY).toBeGreaterThanOrEqual(30);
    const cells = spiral(24);
    expect(cells.slice(0, 7).every((c) => Math.max(Math.abs(c.x), Math.abs(c.y)) === 1)).toBe(true);
  });
  test("lots and houses fit the map and never overlap; doors and seats are off the road", () => {
    const cells = [...assign(names).values(), POST_OFFICE, TOWN_HALL];
    for (const c of cells) {
      const l = lot(c);
      expect(l.x).toBeGreaterThanOrEqual(0);
      expect(l.y + l.h).toBeLessThanOrEqual(MAP_H);
      expect(l.x + l.w).toBeLessThanOrEqual(MAP_W);
      const d = door(c);
      expect(isRoad(d.x, d.y)).toBe(false);
      for (let i = 0; i < 6; i++) {
        const p = seat(c, i);
        expect(isRoad(Math.floor(p.x / TILE), Math.floor(p.y / TILE) - 1)).toBe(false);
      }
    }
    for (let i = 0; i < cells.length; i++)
      for (let j = i + 1; j < cells.length; j++) expect(overlaps(house(cells[i]!), house(cells[j]!))).toBe(false);
  });
  test("a walk across town goes out one door, along roads only, in at the other", () => {
    const all = assign(names);
    const a = [...all.values()].find((c) => c.y === -2)!, b = [...all.values()].find((c) => c.y === 2)!;
    const from = seat(a, 0), to = seat(b, 0);
    const p = path(from, a, to, b);
    expect(p[0]).toEqual(from);
    expect(p[p.length - 1]).toEqual(to);
    expect(p).toContainEqual(px(door(a)));
    expect(p).toContainEqual(px(door(b)));
    // Between the doors every leg is straight and every point on it is road.
    const inner = p.slice(2, -2);
    for (let i = 1; i < inner.length; i++) {
      const u = inner[i - 1]!, v = inner[i]!;
      expect(u.x === v.x || u.y === v.y).toBe(true);
      for (let t = 0; t <= 1; t += 0.05) {
        const x = u.x + (v.x - u.x) * t, y = u.y + (v.y - u.y) * t;
        expect(isRoad(Math.floor(x / TILE), Math.floor(y / TILE))).toBe(true);
      }
    }
  });
  test("inside one lot a walk is straight", () => {
    const c = { x: 1, y: 0 };
    expect(path({ x: 0, y: 0 }, c, { x: 10, y: 10 }, c)).toHaveLength(2);
  });
});
