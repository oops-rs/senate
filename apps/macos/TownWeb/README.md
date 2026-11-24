# Town (web)

The Town page as a web game: [Phaser 4](https://phaser.io) (MIT) in the app's
web view, a [Tiled](https://www.mapeditor.org) map, CC0 tiles and sprites,
TypeScript bundled locally with esbuild. The same page runs in a plain
browser with a scripted town.

```sh
npm install          # Phaser 4, TypeScript, esbuild, vitest, tile-extruder
npm run map          # assets/town.tmj (+ the extruded tileset) from src/layout.ts
npm test             # vitest: model, layout, director
npm run build        # ../Senate/Web/town/: index.html, town.js, town.css, assets/
python3 -m http.server -d ../Senate/Web/town 8765   # then open http://127.0.0.1:8765/
```

Browser query flags: `dark=1|0`, `reduce-motion`, `large` (30 committees, 60
senators), `once` (play the visits once), `now` (start them at once).

In the app: `SENATE_APP_TOWN_WEB=1` shows this page in place of the
SpriteKit map (`Senate/Town/TownWebPage.swift`); the SpriteKit Town keeps
shipping until this one replaces it.

## Where things live

| | |
| --- | --- |
| `src/model.ts` | `TownState`, `TownEvent`, the state diff, activity entries → events, ids, the FNV hash (same as the Swift town's) |
| `src/layout.ts` | the lot grid, the spiral, committee → lot by name hash, houses, doors, seats, paths along the roads |
| `src/director.ts` | events → actions: at most 6 walkers, one visit each, a queue of 12, moods held while out, guests |
| `src/scene.ts` | the Phaser scene: the Tiled map, houses stamped on lots, senators, letters through the post office, cues, hover/click, pan/zoom, sleep when still |
| `src/main.ts` | the page: the app bridge (`window.town`, `webkit.messageHandlers.town`), the hover card, the scripted town in a browser |
| `tools/make-map.mjs` | writes `assets/town.tmj` from `src/layout.ts` (open it in Tiled) |
| `assets/NOTICE.md` | every asset, its source and licence |

Swift only feeds the page: the town's state (`TownState(store:)`, as JSON)
whenever the store changes, and the committees' activity entries as they
arrive (`TownActivityFeed`); the page diffs, maps and directs. The page
sends back `ready` and `open` (session, thread, committee, hall), which
Swift checks against what it sent before acting.

## Why Phaser (and not PixiJS or Godot)

| | Phaser 3 | PixiJS 8 (+ @pixi/tilemap, pixi-viewport) | Godot 4 (web export / libgodot) |
| --- | --- | --- | --- |
| What it is | a 2D game framework (Phaser 4: WebGL first, Canvas fallback): scenes, cameras, tweens, input, tilemaps, particles, loader | a 2D renderer; the rest from plugins or by hand | a full engine and editor |
| Community | 40.8k GitHub stars, ~770k npm downloads a week; large example and plugin set | 48.9k stars, ~2.0M downloads a week (much of it non-game) | 120.7k stars; web is a secondary target |
| Tiled / isometric | Tiled JSON loaded natively: orthogonal, isometric, staggered, hexagonal layers and object layers | no Tiled loader in core; @pixi/tilemap (~12k downloads a week) renders tiles, parsing and iso are yours | its own TileMap editor (iso included); Tiled via an import plugin |
| Bundle | town.js here: 1,385 KB minified, 378 KB gzip with Phaser 4.2.1 (1,209 / 338 with 3.90) | ~600–700 KB minified for pixi.js alone, plus the plugins and the game code we would write | ~35–40 MB wasm + pack uncompressed; threads need cross-origin isolation (SharedArrayBuffer), a single-threaded export since 4.3 |
| In WKWebView | one script and a canvas (WebGL, canvas fallback), served from our own scheme under a strict CSP; works unchanged in a browser and on iOS | the same | a large wasm module and worker setup to serve from a custom scheme; libgodot embedding is a native engine in the app, not a web page |
| Accessibility | canvas: none built in; we keep the native List view and DOM hover cards | the same; Pixi has an accessibility plugin (DOM shadows for interactive objects) | AccessKit screen-reader support on desktop; the web export has none |
| Licence | MIT | MIT (plugins MIT) | MIT |

Phaser is the pick: it has the most of what the Town needs out of the box
(Tiled maps with isometric support for draft B, cameras, tweens, input,
particles), the largest game-specific community and examples, one
dependency instead of a renderer plus plugins, and it embeds like any web
page. Pixi is a fine renderer but we would rebuild the game layer that
Phaser already has. Godot is the strongest engine but its web build is two
orders of magnitude bigger, needs threads/isolation or a degraded build,
and moves the town into an editor and language the rest of the app does
not use.

The page is on Phaser 4.2.1 (moved from 3.90): the town's code needed no
change (types, scenes, tilemaps, tweens, input, blend modes, generated
textures all carried over); the bundle grew by 176 KB minified (40 KB gzip);
the esbuild defines for the Canvas renderer and plugins are gone (4 reads
only `WEBGL_RENDERER`/`WEBGL_DEBUG`).
