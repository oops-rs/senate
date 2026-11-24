// Writes assets/town.tmj, a Tiled map (open it in Tiled to look or edit):
// grass, the road grid, trees on the lots' corners, and object layers
// naming every lot (gx, gy) and the civic lots. The geometry comes from
// src/layout.ts, so the map and the town's layout always agree.
// Node strips the TypeScript types when importing.
import { execFileSync } from "node:child_process";
import { writeFileSync } from "node:fs";
import { isRoad, lot, MAP_H, MAP_W, POST_OFFICE, RINGS, TILE, TOWN_HALL } from "../src/layout.ts";
import { seeded } from "../src/model.ts";

const gid = (i) => i + 1; // tiny-town's tiles, firstgid 1
const rnd = seeded("town-map");
const ground = [], roads = [], decor = [];
for (let y = 0; y < MAP_H; y++) {
  for (let x = 0; x < MAP_W; x++) {
    const r = rnd();
    ground.push(gid(r < 0.012 ? 2 : r < 0.12 ? 1 : 0));
    roads.push(isRoad(x, y) ? gid(rnd() < 0.15 ? 41 : 25) : 0);
    decor.push(0);
  }
}
const put = (x, y, tile) => { if (x >= 0 && y >= 0 && x < MAP_W && y < MAP_H && !isRoad(x, y)) decor[y * MAP_W + x] = gid(tile); };

const lots = [];
let id = 1;
for (let gy = -RINGS; gy <= RINGS; gy++) {
  for (let gx = -RINGS; gx <= RINGS; gx++) {
    const l = lot({ x: gx, y: gy });
    const civic = (gx === POST_OFFICE.x && gy === POST_OFFICE.y) ? "post-office" : (gx === TOWN_HALL.x && gy === TOWN_HALL.y) ? "town-hall" : "";
    lots.push({
      id: id++, name: civic || `lot ${gx},${gy}`, type: civic ? "civic" : "lot",
      x: l.x * TILE, y: l.y * TILE, width: l.w * TILE, height: l.h * TILE, rotation: 0, visible: true,
      properties: [{ name: "gx", type: "int", value: gx }, { name: "gy", type: "int", value: gy }],
    });
    // A tree or a bush in the garden column right of the house.
    const trees = [28, 27, 5, 16, 4];
    put(l.x + 12, l.y + 1, trees[Math.floor(rnd() * trees.length)]);
    if (rnd() < 0.7) put(l.x + 12, l.y + 4, rnd() < 0.5 ? 5 : trees[Math.floor(rnd() * trees.length)]);
    if (rnd() < 0.4) put(l.x + 2, l.y + 8, 5);
  }
}

const layer = (lid, name, data) => ({ id: lid, name, type: "tilelayer", width: MAP_W, height: MAP_H, x: 0, y: 0, opacity: 1, visible: true, data });
const map = {
  type: "map", version: "1.10", tiledversion: "1.10.2", orientation: "orthogonal", renderorder: "right-down",
  width: MAP_W, height: MAP_H, tilewidth: TILE, tileheight: TILE, infinite: false,
  nextlayerid: 5, nextobjectid: id,
  tilesets: [{ firstgid: 1, name: "tiny-town", image: "tiny-town-extruded.png", imagewidth: 12 * 18, imageheight: 11 * 18,
               tilewidth: TILE, tileheight: TILE, columns: 12, tilecount: 132, margin: 1, spacing: 2 }],
  layers: [
    layer(1, "ground", ground),
    layer(2, "roads", roads),
    layer(3, "decor", decor),
    { id: 4, name: "lots", type: "objectgroup", draworder: "topdown", x: 0, y: 0, opacity: 1, visible: true, objects: lots },
  ],
};
writeFileSync(new URL("../assets/town.tmj", import.meta.url), JSON.stringify(map));
// The tiles, each grown by a pixel of itself all round, so a zoom never
// samples a neighbour (no seams between tiles).
execFileSync("npx", ["tile-extruder", "-w", "16", "-h", "16", "-i", "assets/tiny-town.png", "-o", "assets/tiny-town-extruded.png"],
  { cwd: new URL("..", import.meta.url).pathname, stdio: "inherit" });
console.log(`assets/town.tmj: ${MAP_W}×${MAP_H} tiles, ${lots.length} lots`);
