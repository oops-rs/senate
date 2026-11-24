import { observe } from "../../senate-observe/src/index.js";
import { HOST_VERSION } from "../src/index.js";

const form = document.querySelector("form");
const mount = document.querySelector("main");
const status = document.querySelector("output");
const navigation = document.querySelector("#navigation");
const encoder = new TextEncoder();
const sceneCsp = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:; font-src data:; media-src data:; connect-src 'none'; base-uri 'none'; form-action 'none'; object-src 'none'; frame-src 'none'";
let current;

function folderPath(value) {
  if (!value || value.length > 256 || value.split("/").some((part) =>
    !part || part === "." || part === ".." || !/^[a-zA-Z0-9._ -]+$/.test(part))) {
    throw new Error("Invalid path in scene bundle.");
  }
  return value;
}

function textArray(value) {
  return Array.isArray(value) && value.length <= 64 &&
    value.every((item) => typeof item === "string" && item.length > 0 && item.length <= 128);
}

async function bundle(files) {
  if (files.length === 0) {
    const [page, sdk] = await Promise.all([
      fetch(new URL("./example.html", import.meta.url)).then((response) => response.text()),
      fetch(new URL("../src/index.js", import.meta.url)).then((response) => response.text()),
    ]);
    return { id: "example-v1", required: [], html: page.replace("/* SCENE_SDK */", () => sdk) };
  }
  if (files.length > 256 || files.reduce((size, file) => size + file.size, 0) > 16 * 1024 * 1024) {
    throw new Error("Scene folder exceeds the shell's limit.");
  }
  const assets = new Map(files.map((file) => [folderPath(file.webkitRelativePath.split("/").slice(1).join("/")), file]));
  const manifestFile = assets.get("manifest.json");
  if (!manifestFile || manifestFile.size > 16 * 1024) throw new Error("Missing bounded scene manifest.");
  const manifest = JSON.parse(await manifestFile.text());
  if (typeof manifest.name !== "string" || typeof manifest.version !== "string" ||
      manifest.observe?.major !== 1 || !textArray(manifest.observe.required) ||
      !textArray(manifest.observe.optional) || !textArray(manifest.cards?.reads) ||
      !textArray(manifest.cards?.writes) || manifest.min_host_version !== HOST_VERSION) {
    throw new Error("Scene manifest is incompatible with this shell.");
  }
  const entry = folderPath(manifest.entry);
  const source = assets.get(entry);
  if (!source) throw new Error("Scene entry was not found in the folder.");
  const base = new URL(entry, "https://scene.invalid/");
  function asset(reference) {
    const url = new URL(reference, base);
    if (url.origin !== base.origin || url.search || url.hash) throw new Error("Scene assets must be local files.");
    const file = assets.get(folderPath(decodeURIComponent(url.pathname.slice(1))));
    if (!file) throw new Error("Referenced scene asset is missing.");
    return file;
  }
  const page = new DOMParser().parseFromString(await source.text(), "text/html");
  for (const script of page.querySelectorAll("script[src]")) {
    script.textContent = await asset(script.getAttribute("src")).text();
    script.removeAttribute("src");
  }
  for (const link of page.querySelectorAll("link[rel=stylesheet]")) {
    const style = page.createElement("style");
    style.textContent = await asset(link.getAttribute("href")).text();
    link.replaceWith(style);
  }
  for (const item of page.querySelectorAll("img[src]")) {
    const reference = item.getAttribute("src");
    if (reference.startsWith("data:")) continue;
    const file = asset(reference);
    if (!/^image\/(?:png|jpeg|webp|gif|avif)$/.test(file.type)) throw new Error("Unsupported image asset.");
    const data = await new Promise((resolve, reject) => {
      const reader = new FileReader();
      reader.onload = () => resolve(reader.result);
      reader.onerror = reject;
      reader.readAsDataURL(file);
    });
    item.setAttribute("src", data);
  }
  const digest = async (file) => [...new Uint8Array(await crypto.subtle.digest("SHA-256", await file.arrayBuffer()))];
  const identities = [];
  for (const key of [...assets.keys()].sort()) identities.push([key, await digest(assets.get(key))]);
  const hash = await crypto.subtle.digest("SHA-256", encoder.encode(JSON.stringify(identities)));
  const id = [...new Uint8Array(hash)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
  return { id, required: manifest.observe.required, html: `<!doctype html>${page.documentElement.outerHTML}` };
}

function environment(frame) {
  return {
    color_scheme: matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light",
    reduced_motion: matchMedia("(prefers-reduced-motion: reduce)").matches,
    locale: navigator.language,
    time_zone: Intl.DateTimeFormat().resolvedOptions().timeZone,
    visible: !document.hidden,
    viewport: { width: frame.clientWidth, height: frame.clientHeight },
    safe_area: { top: 0, right: 0, bottom: 0, left: 0 },
  };
}

function storage(namespace, type, data) {
  if (typeof data?.key !== "string" || !/^[A-Za-z0-9._~-]{1,64}$/.test(data.key)) throw "invalid_request";
  const previous = JSON.parse(localStorage.getItem(namespace) ?? "{}");
  if (type === "storage.get") return Object.hasOwn(previous, data.key) ? previous[data.key] : null;
  const next = Object.assign(Object.create(null), previous);
  if (type === "storage.delete") delete next[data.key];
  else {
    const value = JSON.stringify(data.value);
    if (typeof value !== "string" || encoder.encode(value).byteLength > 16 * 1024) throw "quota";
    next[data.key] = JSON.parse(value);
  }
  const serialized = JSON.stringify(next);
  if (encoder.encode(serialized).byteLength > 64 * 1024) throw "quota";
  localStorage.setItem(namespace, serialized);
}

function showNavigation(raw, state, activityLinks) {
  if (typeof raw !== "string" || raw.length > 2048 || !raw.startsWith("senate://open/v1/")) throw "invalid_request";
  const links = [state?.senate, state?.senate?.person, ...(state?.committees ?? []), ...(state?.senators ?? []),
    ...(state?.threads ?? []), ...(state?.desk_items ?? []), ...(state?.approvals ?? []),
    ...(state?.retired ?? []), ...(state?.projects ?? [])];
  if (!links.some((item) => item?.link === raw) && !Object.values(state?.navigation ?? {}).includes(raw) && !activityLinks.has(raw)) {
    throw "invalid_request";
  }
  navigation.href = raw;
  navigation.hidden = false;
}

function launch(content, options) {
  const controller = new AbortController();
  const frame = document.createElement("iframe");
  frame.title = "Scene";
  frame.setAttribute("sandbox", "allow-scripts");
  frame.referrerPolicy = "no-referrer";
  const isolated = new DOMParser().parseFromString(content.html, "text/html");
  const policy = isolated.createElement("meta");
  policy.httpEquiv = "Content-Security-Policy";
  policy.content = sceneCsp;
  isolated.head.prepend(policy);
  frame.srcdoc = `<!doctype html>${isolated.documentElement.outerHTML}`;
  const channel = new MessageChannel();
  const port = channel.port1;
  let loaded = false;
  let ready = false;
  let state;
  let requests = [];
  const handled = new Set();
  const activityLinks = new Set();
  const send = (type, data, request_id) => port.postMessage({ host_version: HOST_VERSION, type, data, request_id });
  const report = (state) => {
    status.value = state.replaceAll("_", " ");
    send("status", { state });
  };
  const env = () => { if (ready) send("environment", environment(frame)); };
  const small = matchMedia("(prefers-reduced-motion: reduce)");
  const dark = matchMedia("(prefers-color-scheme: dark)");
  const resize = new ResizeObserver(env);
  resize.observe(frame);
  document.addEventListener("visibilitychange", env);
  small.addEventListener("change", env);
  dark.addEventListener("change", env);
  function close() {
    controller.abort();
    port.close();
    resize.disconnect();
    navigation.hidden = true;
    navigation.removeAttribute("href");
    document.removeEventListener("visibilitychange", env);
    small.removeEventListener("change", env);
    dark.removeEventListener("change", env);
  }
  frame.addEventListener("load", () => {
    if (loaded) { close(); return; }
    loaded = true;
    frame.contentWindow.postMessage({ host_version: HOST_VERSION, type: "scene.connect" }, "*", [channel.port2]);
  });
  port.addEventListener("message", async ({ data: request }) => {
    const id = request?.request_id;
    if (request?.host_version !== HOST_VERSION || typeof id !== "string" || !/^[A-Za-z0-9_-]{1,64}$/.test(id) ||
        handled.has(id) || handled.size >= 4096) return;
    handled.add(id);
    try {
      const now = performance.now();
      requests = requests.filter((at) => now - at < 60_000);
      if (requests.length >= 60) throw "quota";
      requests.push(now);
      let value;
      if (request.type === "ready" && !ready) {
        if (!request.data?.supported_host_versions?.includes(HOST_VERSION)) throw "unsupported";
        ready = true;
        value = { version: HOST_VERSION, capabilities: ["open", "storage"] };
      } else {
        if (!ready) throw "unavailable";
        switch (request.type) {
          case "open": showNavigation(request.data?.url, state, activityLinks); break;
          case "storage.get":
          case "storage.set":
          case "storage.delete":
            value = storage(`senate-scene:${state?.senate?.id ?? "offline"}:${content.id}`, request.type, request.data);
            break;
          case "card.write": throw "unsupported";
          default: throw "invalid_request";
        }
      }
      send("result", { ok: true, value }, id);
      if (request.type === "ready") {
        report("data_unavailable");
        env();
        for await (const update of observe({ ...options, signal: controller.signal })) {
          state = update.state;
          if (!content.required.every((capability) => state.capabilities.includes(capability))) {
            report("version_mismatch");
            controller.abort();
            break;
          }
          if (update.event) {
            if (update.event.link) {
              activityLinks.add(update.event.link);
              if (activityLinks.size > 256) activityLinks.delete(activityLinks.values().next().value);
            }
            send("observe.event", { frame: update.event });
          } else {
            activityLinks.clear();
            report("ready");
            send("observe.snapshot", { snapshot: state });
          }
        }
      }
    } catch (error) {
      if (typeof error === "string") send("result", { ok: false, code: error }, id);
      else if (!controller.signal.aborted && request.type === "ready") {
        report(error.status === 401 || error.status === 403 ? "access_needed" :
          error.code === "version_mismatch" ? "version_mismatch" : "data_unavailable");
      } else send("result", { ok: false, code: "unavailable" }, id);
    }
  });
  port.start();
  mount.replaceChildren(frame);
  return close;
}

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  current?.();
  const tokenField = form.elements.namedItem("token");
  const token = tokenField.value;
  tokenField.value = "";
  try {
    const content = await bundle([...form.elements.namedItem("bundle").files]);
    current = launch(content, { endpoint: form.elements.namedItem("endpoint").value, token });
  } catch (error) {
    status.value = error.message;
  }
});
