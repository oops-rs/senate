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

test("environment hints are forwarded and remain optional", async () => {
  const pair = new MessageChannel();
  const host = createSceneHost(pair.port1);
  const environment = {
    color_scheme: "dark", reduced_motion: false, locale: "en", time_zone: "UTC",
    visible: true, viewport: { width: 320, height: 240 },
    safe_area: { top: 0, right: 0, bottom: 0, left: 0 },
  };
  try {
    const received = new Promise((resolve) => {
      const messages = [];
      host.subscribe(({ data }) => {
        messages.push(data);
        if (messages.length === 2) resolve(messages);
      });
    });
    for (const data of [environment, { ...environment, focused: false, power: "low_power" }]) {
      pair.port2.postMessage({ host_version: 1, type: "environment", data });
    }
    assert.deepEqual(await received, [environment, { ...environment, focused: false, power: "low_power" }]);
  } finally {
    host.close();
    pair.port2.close();
  }
});
