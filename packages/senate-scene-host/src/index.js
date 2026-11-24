export const HOST_VERSION = 1;
const hostEvents = new Set(["status", "observe.snapshot", "observe.event", "environment"]);

export function createSceneHost(port) {
  const listeners = new Set();
  const pending = new Map();
  let requestId = 0;
  let closed = false;
  const receive = ({ data: message }) => {
    if (message?.host_version !== HOST_VERSION) return;
    if (message.type === "result") {
      const request = pending.get(message.request_id);
      if (!request) return;
      pending.delete(message.request_id);
      clearTimeout(request.timeout);
      if (message.data?.ok === true) request.resolve(message.data.value);
      else request.reject(new Error(message.data?.code || "unavailable"));
    } else if (hostEvents.has(message.type)) {
      for (const listener of listeners) listener(message);
    }
  };
  port.addEventListener("message", receive);
  port.start?.();

  function request(type, data) {
    if (closed || pending.size >= 16) return Promise.reject(new Error("unavailable"));
    const id = String(++requestId);
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        pending.delete(id);
        reject(new Error("unavailable"));
      }, 30_000);
      pending.set(id, { resolve, reject, timeout });
      try {
        port.postMessage({ host_version: HOST_VERSION, type, request_id: id, data });
      } catch (error) {
        clearTimeout(timeout);
        pending.delete(id);
        reject(error);
      }
    });
  }

  return {
    ready: () => request("ready", { supported_host_versions: [HOST_VERSION] }),
    open: (url) => request("open", { url }),
    storage: {
      get: (key) => request("storage.get", { key }),
      set: (key, value) => request("storage.set", { key, value }),
      delete: (key) => request("storage.delete", { key }),
    },
    writeCard: (schema, value) => request("card.write", { schema, value }),
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    close() {
      closed = true;
      port.removeEventListener("message", receive);
      port.close?.();
      listeners.clear();
      for (const request of pending.values()) {
        clearTimeout(request.timeout);
        request.reject(new Error("unavailable"));
      }
      pending.clear();
    },
  };
}

// Browser scenes bind only the transferred port in the parent's handshake.
// Native wrappers supply a port-compatible adapter to createSceneHost.
export function fromParent(parent = window.parent) {
  return new Promise((resolve) => {
    function connect(event) {
      if (event.source !== parent || event.data?.host_version !== HOST_VERSION ||
          event.data?.type !== "scene.connect" || event.ports.length !== 1) return;
      window.removeEventListener("message", connect);
      resolve(createSceneHost(event.ports[0]));
    }
    window.addEventListener("message", connect);
  });
}
