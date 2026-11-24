import { MessageChannel } from "node:worker_threads";
import assert from "node:assert/strict";
import test from "node:test";
import { createSceneHost } from "../src/index.js";

test("scene uses one scoped channel and receives structured results", async () => {
  const pair = new MessageChannel();
  const host = createSceneHost(pair.port1);
  pair.port2.on("message", (message) => {
    const value = message.type === "ready" ? { version: 1, capabilities: ["storage"] } : undefined;
    const data = message.type === "card.write"
      ? { ok: false, code: "unsupported" } : { ok: true, value };
    pair.port2.postMessage({ host_version: 1, type: "result", request_id: message.request_id, data });
  });
  try {
    assert.deepEqual(await host.ready(), { version: 1, capabilities: ["storage"] });
    assert.equal(await host.storage.set("palette", 2), undefined);
    await assert.rejects(host.writeCard("town/v1", { palette: 1 }), /unsupported/);
  } finally {
    host.close();
    pair.port2.close();
  }
});
