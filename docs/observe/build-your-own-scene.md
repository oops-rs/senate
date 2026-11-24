# Build your own scene

Request an Observe grant from the person who runs the machine Senate. Use the
numeric loopback URL the CLI prints. The grant controls which committees and
data your scene can see. The bearer belongs in the client or shell.

```sh
senate observe token create --name my-scene --committees all \
  --origin http://127.0.0.1:8080
```

In a native game, issue `GET /v1/observe/state`, then open
`GET /v1/observe/events` with the snapshot cursor in `Last-Event-ID`. Set
`Authorization: Bearer <token>` on both requests. The clients handle SSE
reconnect and refresh the snapshot after a reset. A cursor belongs to one
observer grant. Log sequences belong to their committees. See the
[Observe protocol](../observe.md) for wire types and authority boundaries.

## Run without a live Senate

```sh
senate observe replay --demo --origin http://127.0.0.1:8080
```

Serve the checkout’s `packages/` directory on a loopback HTTP server at
port 8080. Open `/senate-scene-host/browser/`, paste the replay URL and token, and
select **Open scene**. The built-in scene renders the example committees.
Choose a scene folder to load your own `manifest.json` and HTML entry. Bundle
module imports, CSS assets and dynamically needed assets into the scene
before loading it. The reference shell inlines entry scripts, stylesheets
and supported image files from the selected folder. It exposes the shell's
storage and navigation capabilities and declines card writes.

A scene that uses the host contract can render the same bundle in an
independently implemented container. The bundle receives public state,
activity and environment messages. The container keeps its credential and
the SSE connection. When the person selects a row, the scene can ask the
container to open its supplied app link.

## Small standalone browser client

Use a loopback-served page with a form containing `endpoint` and `token`
inputs and a `main` element. Use `@senate/observe` directly if the page is
both the shell and the renderer.

```js
import { observe } from "@senate/observe";

const form = document.querySelector("form");
const main = document.querySelector("main");
let previous;

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  previous?.abort();
  const controller = new AbortController();
  previous = controller;
  const field = form.elements.namedItem("token");
  const token = field.value;
  field.value = "";
  const endpoint = form.elements.namedItem("endpoint").value;
  try {
    for await (const { state } of observe({ endpoint, token, signal: controller.signal })) {
      const rows = state.committees.map(({ name, link }) => {
        const row = document.createElement(link ? "a" : "p");
        row.textContent = name;
        if (link) row.href = link;
        return row;
      });
      main.replaceChildren(...rows);
    }
  } catch {
    if (!controller.signal.aborted) main.textContent = "Observe is unavailable.";
  }
});
```

A shell that opens custom navigation URLs should validate the shared
`senate://open/v1/` grammar and require a user click. Browser and OS
permissions still apply. Card values and message previews are untrusted
content. Use plain text for labels and validate a card against your scene's
schema before choosing assets or behavior.

## Record and replay an authorized view

```sh
senate observe record --endpoint http://127.0.0.1:12345 \
  --token-file ./scene-token --out session.ndjson
senate observe replay session.ndjson --origin http://127.0.0.1:8080
```

Put only the bearer in an owner-only token file, such as a file with mode
0600. Recording uses an owner-only output file and omits credentials, live cursors
and navigation links. Use `--include-lines` only for a recording you intend
to retain with approved message previews. Playback presents the same public
HTTP contract with a disposable token. Line-bearing recordings require
person confirmation and are currently refused.
