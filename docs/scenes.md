# Scenes in the macOS app

The Senate app is a container for scenes. A scene is a web bundle that
satisfies the [scene host contract](observe.md#scene-host-contract). The app
includes no scene. A person installs the scenes they want. Each scene is
treated as untrusted code from somewhere else.

## Installing

Open **Scenes** in the sidebar, choose **Add Scene…**, and pick a bundle
folder or a `.senatescene` file (that folder's files in an uncompressed zip,
as `senate scene pack` writes it). Before anything is
copied, the app checks the bundle completely and shows the person the scene's
name, version, size and fingerprint, what it observes, whether it asks for
this Senate's links, and which cards it reads and writes. Nothing in the
bundle runs before the person confirms.

When the person confirms, the app copies only the checked files into
`~/Library/Application Support/dev.senate.app/Scenes/<id>/bundle/`. The
Senate home belongs to the daemon, so scenes are not stored there. `<id>` is
a random 128-bit identity created at installation. The scene's origin,
storage and Observe grant are named by this identity, never by the manifest's
`name`. If the same bundle is installed twice, the app creates two scenes
that share nothing. Each installed scene appears as a sidebar row. With no
scenes installed, a single **Scenes** row opens the place that explains them.

**Update…** on an installed scene checks a new bundle, shows what changes in
its requests (capabilities, cards and links), and replaces the bundle under
the same identity and storage. If its request for this Senate's links
changes, the old grant is revoked and the person allows the scene again.
**Remove** revokes the scene's grants, deletes its Keychain items for every
Senate, and deletes its files and storage. When the app reloads the list, it
checks every installed scene against the fingerprint recorded at
installation. A changed file makes that scene unreadable, so it won't load.

`SENATE_APP_SCENE_DEV=<folder>` loads a bundle from a build folder in Debug
builds only. The same checks run on every load. The identity comes from the
folder path, so storage and the grant survive app restarts.

## Bundle rules

| Rule | Limit |
|---|---|
| Manifest | `manifest.json` at the root, up to 16 KiB, strict JSON with no duplicate keys, exactly the contract's fields with their types and bounds ([`manifest.rs`](../crates/senate-scene/src/manifest.rs)), `observe.major` 1, `min_host_version` not above 1 |
| Names | Every path segment matches `[A-Za-z0-9_][A-Za-z0-9._-]*` (ASCII, no dot files, no `.`/`..`), at most 16 deep. `.DS_Store` and a root `__MACOSX` are skipped |
| Files | Regular files only. No symbolic links, no hard links (link count 1), no devices, FIFOs or sockets |
| Types | `html js mjs css json ndjson txt md png jpg jpeg gif webp woff2`, plus `LICENSE NOTICE COPYING AUTHORS`. No SVG, WebAssembly or media |
| Sizes | 8 MiB per file, 64 MiB and 2,000 files in total |
| Archives | Entries are stored, not compressed, so an archive can't unpack into more than its own size. The central directory is read before anything is unpacked. Links, absolute or escaping names, encryption, zip64, compression, disallowed types and sizes over the limits are refused. Each file is then taken from its local header, which must match the directory and its CRC-32, into an empty staging folder of the app's own. The result is checked like a folder |

One implementation of these rules, in [`crates/senate-scene`](../crates/senate-scene),
serves both the app and the `senate` CLI. Scene authors check a build with
the same rules the app applies, and package it reproducibly:

```sh
senate scene inspect dist/                    # what the app would show, or why it refuses
senate scene inspect town-1.2.0.senatescene   # the same for an archive
senate scene pack dist/ -o town-1.2.0.senatescene
```

`pack` checks the folder, then writes exactly its files, sorted, stored,
with fixed times and modes. The same bundle always produces the same bytes,
and the archive's fingerprint is the bundle's. Zips made by other tools are
usually compressed, so the app refuses them.

## What an installed scene can do

- Run its own scripts and styles from its own bundle, in a `WKWebView` with
  non-persistent website data and its own origin (`senate-scene://<id>`).
- Receive Observe snapshots and frames for **its own grant** and nothing
  else. The person creates that grant with one click on the scene's page. It
  is metadata scope with no message text, covers every committee now and
  later, expires in 30 days, and includes this Senate's direct links only
  when the manifest names `topology`. The shell checks the required
  capabilities against what Observe offers and sends nothing when they don't
  match.
- Read its environment: colour scheme, reduced motion, locale, time zone,
  viewport, visibility, window focus, and power source or Low Power Mode.
- Keep up to 64 KiB of JSON (16 KiB per value) per Senate, in its own file
  under its own folder.
- Ask the app to open a `senate://open/v1/…` link. The app's single router
  validates the link and only navigates. The app accepts up to 5 links per
  second. A link counts as a person's gesture only if the app itself saw a
  click or key press in the page within the last second.
- Ask to change **the person's own profile card** under a schema its manifest
  declares in `cards.writes`. The app shows the exact value and asks every
  time. It keeps the rest of the card, enforces Observe's card limits, and
  writes through the app's person path. After a refusal, it refuses further
  requests without asking for a minute. This works only when the scene's
  grant shows the person's row (topology). Without it, the rest of the card
  is unknown, so the app refuses.

## What it cannot do

- **Reach the network or the disk.** The content rule list blocks `http(s)`,
  `ws(s)`, `file`, `ftp` and `blob`. If the list fails to compile, the page
  doesn't load. The CSP is `default-src 'none'`, and scripts, styles, fonts
  and fetches come only from the scene scheme. Images can also come from
  `data:`. The scheme handler belongs to one web view, serves only that
  scene's host, and checks each path again. It opens every component with
  `O_NOFOLLOW`, serves only regular unlinked files of allowed types within
  the size limit, and accepts no escapes, queries, fragments, ports or user
  information.
- **Run code it didn't ship.** The CSP has no `unsafe-inline`, `unsafe-eval`
  or `wasm-unsafe-eval`. Trusted Types has no policy, so markup can't come
  from strings. Workers, frames, objects, plugins, forms and manifests are
  all `'none'`.
- **Leave its page.** The app cancels every navigation except the entry page
  in the main frame, refuses new windows, denies camera and microphone
  requests, offers no file panel, and disables fullscreen. The page has no
  context menu.
- **See credentials or app state.** The bearer, Observe endpoint, session
  IDs and the app's own data never enter the channel. Data goes into the
  page as call arguments, never as script.
- **Reach another scene.** Origins, scheme handlers, message channels,
  storage files, Keychain items and grant tags are separate for each scene.
- **Speak out of turn.** The app reads messages only from the main frame of
  its own document. Before the scene says `ready`, after navigation and after
  a crash, the app answers nothing. Each message must match the contract's
  exact shape, stay under 64 KiB, and fit the rate limit (a burst of 40, then
  20 per second). Unknown requests get the result `unsupported`.
- **Write anything but its declared part of the person's own card**, and only
  after the person confirms each change.

## What doesn't hold

- **WebKit is the boundary.** A WebKit or WebContent sandbox exploit is
  outside what the app can contain.
- **The clipboard and drag and drop are exits.** After a real click or key
  press, a scene can write to the pasteboard (`navigator.clipboard.writeText`,
  `copy`), and a person can drag text or images out of the page to another
  app. The app doesn't block either.
- **A confirmed card is an exit.** A card change the person confirms can
  carry anything the scene chose, including metadata it observed, to
  everyone who can see the person's profile. The confirmation shows the
  exact value.
- **Scenes aren't signed.** No publisher identity or signature is checked.
  The manifest's name is self-declared and can imitate something else. The
  app shows the identity and fingerprint, but provenance is the person's
  choice of file.
- **Observe scope is coarse.** The one-click grant covers every committee. A
  scene can't ask for less, and the app offers no narrower choice. For a
  narrower grant, use `senate observe token create` with a browser or native
  client.
- **Revocation needs the daemon.** If a scene is removed while the daemon is
  unreachable, the app still deletes its Keychain items and files, but the
  daemon keeps the grant until it expires (at most 30 days). The grant is
  listed under the scene's tag in `senate observe token list`.
- **A hidden scene isn't stopped.** When a scene is off screen, the app stops
  its Observe subscription and sets `visible:false`. The page itself stays
  alive while installed and throttled by WebKit. The app doesn't cap a
  scene's CPU or memory.
- **Same-user processes are out of scope.** Another process running as the
  same user can change files in Application Support or read the Keychain
  items. Fingerprint checks catch changed bundles only when the list reloads.
  `SENATE_APP_SCENE_DEV` serves the folder as it is.
- **The gesture heuristic is coarse.** Any click inside the page within a
  second counts as a gesture for the next `open`, whatever the scene drew
  under it. A link can only navigate the app and bring its window forward.
