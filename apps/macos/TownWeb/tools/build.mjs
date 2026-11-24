// Builds the page into the app's bundle folder, Senate/Web/town/:
// town.js (Phaser and the town, one minified file), index.html, town.css,
// and the assets (map, sheets, licences). Everything the page needs is
// local: the app serves it from its own scheme, nothing comes from a CDN.
import { build } from "esbuild";
import { cpSync, mkdirSync, rmSync, statSync } from "node:fs";

const out = new URL("../../Senate/Web/town/", import.meta.url);
rmSync(out, { recursive: true, force: true });
mkdirSync(new URL("assets/", out), { recursive: true });
await build({
  entryPoints: [new URL("../src/main.ts", import.meta.url).pathname],
  bundle: true, minify: true, format: "iife", target: ["safari16"],
  outfile: new URL("town.js", out).pathname, legalComments: "eof",
  define: { CANVAS_RENDERER: "true", WEBGL_RENDERER: "true", WEBGL_DEBUG: "false", EXPERIMENTAL: "false",
            PLUGIN_3D: "false", PLUGIN_CAMERA3D: "false", PLUGIN_FBINSTANT: "false", FEATURE_SOUND: "false" },
});
for (const f of ["index.html", "town.css"]) cpSync(new URL(`../page/${f}`, import.meta.url), new URL(f, out));
for (const f of ["works.tmj", "ground.png", "NOTICE.md"])
  cpSync(new URL(`../assets/${f}`, import.meta.url), new URL(`assets/${f}`, out));
const kb = (f) => (statSync(new URL(f, out)).size / 1024).toFixed(0);
console.log(`Web/town: town.js ${kb("town.js")} KB, map ${kb("assets/works.tmj")} KB`);
