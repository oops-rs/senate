// The smallest scene: says hello over the native binding of the scene host
// contract, shows the status and the committees it is sent, and asks the app
// to open a committee's link when one is clicked. No markup from strings.
(() => {
  "use strict";
  const handler = window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.scene;
  const pending = new Map();
  const received = [];
  let next = 0;
  let committees = [];
  const $ = (id) => document.getElementById(id);

  function request(type, data) {
    const id = String(++next);
    return new Promise((resolve) => {
      pending.set(id, resolve);
      handler.postMessage({ host_version: 1, type, request_id: id, data });
    });
  }

  function render() {
    const list = $("committees");
    list.replaceChildren(...committees.map((c) => {
      const item = document.createElement("li");
      const button = document.createElement("button");
      button.textContent = c.name;
      button.disabled = !c.link;
      button.addEventListener("click", () => { if (c.link) request("open", { url: c.link }); });
      item.append(button);
      return item;
    }));
  }

  Object.defineProperty(window, "__senateScene", {
    value: (message) => {
      if (!message || message.host_version !== 1) return;
      received.push(message.type);
      if (message.type === "result") {
        const resolve = pending.get(message.request_id);
        pending.delete(message.request_id);
        if (resolve) resolve(message.data);
      } else if (message.type === "status") {
        $("status").textContent = message.data.state;
      } else if (message.type === "observe.snapshot") {
        const s = message.data.snapshot;
        $("senate").textContent = s.senate ? s.senate.name : "";
        committees = (s.committees || []).filter((c) => !c.archived);
        render();
      } else if (message.type === "observe.event") {
        const f = message.data.frame;
        const up = f.kind === "state.patch" && f.data && f.data.upsert && f.data.upsert.committees;
        if (up) {
          for (const row of up) committees = committees.filter((c) => c.id !== row.id).concat([row]);
          render();
        }
      }
    },
  });

  // For tests: what arrived, and a way to send any request.
  Object.defineProperty(window, "fixture", {
    value: Object.freeze({
      received: () => received.slice(),
      request,
      text: () => [$("status").textContent, $("senate").textContent,
        ...Array.from(document.querySelectorAll("li")).map((li) => li.textContent)].join("|"),
    }),
  });

  if (handler) request("ready", { supported_host_versions: [1] });
})();
