import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import test from "node:test";
import { applyFrame, endpointUrl, parseEventStream } from "../src/index.js";

const fixtures = new URL("../../../docs/observe/v1/fixtures/", import.meta.url);
const fixture = async (name) => JSON.parse(await readFile(new URL(name, fixtures), "utf8"));

function chunks(text) {
  const bytes = new TextEncoder().encode(text);
  return new ReadableStream({
    start(controller) {
      for (let at = 0; at < bytes.length; at += 3) controller.enqueue(bytes.slice(at, at + 3));
      controller.close();
    },
  });
}

test("SSE frames survive split UTF-8 and CRLF; reset has no cursor", async () => {
  const cursor = "BAAAAAAAAAAAAAAAAAAAAQ";
  const frame = { schema_version: 1, cursor, kind: "unknown.kind", data: { line: "🛠" } };
  const reset = await fixture("reset.sse.json");
  const text = `: ping\r\n\r\nid: ${cursor}\r\ndata: ${JSON.stringify(frame)}\r\n\r\n${reset}`;
  const received = [];
  for await (const event of parseEventStream(chunks(text))) received.push(event);
  assert.deepEqual(received[0], frame);
  assert.equal(received[1].kind, "stream.reset");
  assert.equal(received[1].cursor, undefined);
});

test("patch keys keep equal sequences in separate committees and remove exactly one seat", async () => {
  const state = await fixture("state.json");
  const [frame] = await Array.fromAsync(parseEventStream(chunks(await fixture("unseat.sse.json"))));
  const next = applyFrame(state, frame);
  assert.equal(next.senators.length, 1);
  assert.equal(next.senators[0].state, "away");
  assert.equal(next.threads[0].root_seq, "18446744073709551615");
  assert.equal(next.threads[0].root_seq, next.threads[1].root_seq);
  assert.notEqual(next.threads[0].committee_id, next.threads[1].committee_id);
  assert.deepEqual(applyFrame(next, frame), next);
});

test("endpoint validation keeps the token on loopback", () => {
  assert.equal(endpointUrl("http://127.0.0.1:8967").hostname, "127.0.0.1");
  for (const url of ["https://remote.example:8967", "http://127.0.0.1:8967/?token=x",
    "http://127.0.0.1:8967@shell.example:8967", "http://127.0.0.1:8967/other"]) {
    assert.throws(() => endpointUrl(url));
  }
});

test("fetch follower uses Last-Event-ID and reloads state on a rejected resume", async () => {
  const { createServer } = await import("node:http");
  const { once } = await import("node:events");
  const { observe } = await import("../src/index.js");
  const initial = await fixture("state.json");
  const patch = await fixture("unseat.sse.json");
  const cursors = [];
  let snapshots = 0;
  const server = createServer((request, response) => {
    assert.equal(request.headers.authorization, "Bearer local-fixture");
    if (request.url === "/v1/observe/state") {
      snapshots++;
      response.writeHead(200, { "content-type": "application/json" });
      response.end(JSON.stringify(initial));
    } else {
      assert.equal(request.url, "/v1/observe/events");
      cursors.push(request.headers["last-event-id"]);
      if (cursors.length === 1) {
        response.writeHead(200, { "content-type": "text/event-stream" });
        response.end(patch);
      } else {
        response.writeHead(409);
        response.end();
      }
    }
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const controller = new AbortController();
  const stream = observe({
    endpoint: `http://127.0.0.1:${server.address().port}`,
    token: "local-fixture",
    signal: controller.signal,
  });
  try {
    assert.equal((await stream.next()).value.state.senators.length, 2);
    const update = (await stream.next()).value;
    assert.equal(update.state.senators.length, 1);
    assert.equal((await stream.next()).value.state.senators.length, 2);
    assert.deepEqual(cursors, [initial.cursor, update.event.cursor]);
    assert.equal(snapshots, 2);
  } finally {
    controller.abort();
    await stream.return();
    server.closeAllConnections();
    server.close();
  }
});

test("followers wait for Retry-After before retrying state and events", async () => {
  const { createServer } = await import("node:http");
  const { once } = await import("node:events");
  const { observe } = await import("../src/index.js");
  const initial = await fixture("state.json");
  const patch = await fixture("unseat.sse.json");
  const attempts = { state: [], events: [] };
  const server = createServer((request, response) => {
    const route = request.url.endsWith("/state") ? "state" : "events";
    attempts[route].push(performance.now());
    if (attempts[route].length === 1) {
      response.writeHead(route === "state" ? 503 : 429, { "retry-after": "1" }).end();
    } else if (route === "state") {
      response.writeHead(200, { "content-type": "application/json" }).end(JSON.stringify(initial));
    } else {
      response.writeHead(200, { "content-type": "text/event-stream" }).end(patch);
    }
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const controller = new AbortController();
  const stream = observe({
    endpoint: `http://127.0.0.1:${server.address().port}`,
    token: "local-fixture",
    signal: controller.signal,
  });
  try {
    assert.equal((await stream.next()).value.state.senators.length, 2);
    assert.equal((await stream.next()).value.state.senators.length, 1);
    for (const attempt of Object.values(attempts)) assert.ok(attempt[1] - attempt[0] >= 990);
  } finally {
    controller.abort();
    await stream.return();
    server.closeAllConnections();
    server.close();
  }
});

test("shipped demo recordings do not advertise or retain app links", async () => {
  for (const filename of [
    "crates/senate-observe/src/demo.ndjson",
  ]) {
    const data = await readFile(new URL(`../../../${filename}`, import.meta.url), "utf8");
    assert.ok(!data.includes("senate://"), filename);
    assert.ok(!data.includes('"app_links"'), filename);
  }
});
