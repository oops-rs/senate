# Senate for macOS

The Senate menu-bar app: a status item with the number of things waiting on
you, an inbox per agent and a merged one, Codex approvals, a small Codex
panel, the docket of a Senate one of your agents presides over, and the list
of Senates your machine serves. SwiftUI over the shared Rust core
[`senate-app-core`](../../crates/senate-app-core), macOS 14+. English and
Simplified Chinese, following the system language (English when it is
neither).

It is a **client of `senated --machine`**, the developer-machine daemon. The
core talks to it over `$SENATE_HOME/bridge.sock` (default
`~/.senate/bridge.sock`), the same newline-delimited JSON IPC the hooks and
the MCP server use, with the bridge's own types (`senate_bridge::ipc`). It
holds no state of its own and never talks to a Senate server: start the
daemon first (`senated --machine`, see the top-level README). When it is not
running the menu says *offline* and the core keeps reconnecting (0.5 s
doubling to 10 s).

## Build and run

Needs Xcode (Swift 6), [XcodeGen](https://github.com/yonaskolb/XcodeGen)
(`brew install xcodegen`) and the Rust toolchain with both macOS targets
(`rustup target add aarch64-apple-darwin x86_64-apple-darwin`). From
`apps/macos/`:

```sh
scripts/build-core.sh            # optional: the Xcode build runs it as its first phase
xcodegen generate                # Senate.xcodeproj from project.yml
xcodebuild -project Senate.xcodeproj -scheme Senate -configuration Debug \
    -derivedDataPath build CODE_SIGNING_ALLOWED=NO build
open build/Build/Products/Debug/Senate.app
```

or open `Senate.xcodeproj` in Xcode and run the *Senate* scheme. The target's
pre-build phase runs `scripts/build-core.sh` itself, so after the first
`xcodegen generate` a plain build picks up Rust changes. In Xcode the script
builds only the architectures being built (Debug: the active one); run by
hand it builds arm64 and x86_64 and `lipo`s them
(`scripts/build-core.sh --release` for the release profile).

The script keeps `MACOSX_DEPLOYMENT_TARGET` out of cargo's environment
(Xcode exports it): with rustc 1.97 on macOS 27 any value of it makes
release-mode proc-macro dylibs fail to load ("mis-aligned LINKEDIT string
pool"), and the broken dylibs stay cached. If a release build fails that way,
`cargo clean --release` first. The C in the dependencies gets the app's 14.0
through `CFLAGS_<target>` instead.

The bundle identifier is `dev.senate.app`. The app is signed ad hoc
(`CODE_SIGN_IDENTITY = -`), not with a team; with `CODE_SIGNING_ALLOWED=NO` it
is only linker-signed, which is enough to run it locally.

What is checked in: `project.yml`, the sources and string files in `Senate/`,
the script. What is generated and ignored: `Senate.xcodeproj` (by `xcodegen
generate`, after any change to `project.yml` or to the set of files),
`Senate/Info.plist` (by XcodeGen, from `project.yml`), `Generated/` (by
`build-core.sh`), `build/`.

To work on the app without a Senate server, run the core's fake daemon: it
serves the person `alice` with agents `alice/claude` and `alice/codex`, a
Senate under `alice/claude` (so a docket), canned threads, a Codex thread and
an approval every 20 s. It refuses to run unless `SENATE_HOME` names a scratch
directory, and the socket path must be short (about 100 bytes):

```sh
mkdir -p /tmp/sh && chmod 700 /tmp/sh
SENATE_HOME=/tmp/sh cargo run -p senate-app-core --example fake_daemon &
SENATE_HOME=/tmp/sh SENATE_APP_SHOW=1 \
    build/Build/Products/Debug/Senate.app/Contents/MacOS/Senate
```

| Variable | Effect |
| --- | --- |
| `SENATE_HOME` | the Senate home (`bridge.sock`, `senate.toml`); default `~/.senate` |
| `SENATE_APP_SHOW=1` | open the window at launch |
| `SENATE_APP_NOTIFICATIONS=0` | no notification permission prompt and no notifications |
| `RUST_LOG` | the core's log filter, to stderr (default `senate_app_core=info`) |

Launched from Finder or `open`, an app does not inherit the shell's
environment; run the binary inside the bundle (as above) or use `open --env`.

Tests: the core's are Rust (`cargo test -p senate-app-core`): state folding,
menu text in both languages, action to request mapping, the Terminal command,
mention splitting, and the driver and the FFI facade against a fake daemon on
a temp socket. They never touch `~/.senate`. The Swift side is views and a
store with no logic worth a test of its own.

## Architecture

```
SwiftUI views ──► CoreStore (@Observable, @MainActor) ──► AppCore (Rust, UniFFI)
       ▲                       ▲                               │ tokio runtime
       └── state ──────────────┴─ ListenerBridge ◄─ CoreListener ◄─ Driver ◄─ bridge.sock
```

- **`crates/senate-app-core`** (Rust, `lib` + `staticlib`) is the app:
  `model.rs` folds the daemon's `watch` stream into a model and maps actions
  to IPC requests; `link.rs` holds the socket, reconnects, and runs actions;
  `state.rs` flattens the model into the snapshot; `text.rs` writes its
  sentences; `ffi.rs` is the UniFFI facade. `AppCore` owns a tokio runtime on
  two background threads. `start(listener)` connects; after every change the
  core calls `CoreListener.onState(AppState)` with a full snapshot, and
  `onApprovalRequested` once per new approval.
- **The FFI surface** (generated into `Generated/senate_app_core.swift`):
  `AppCore(senateHome:language:)`, `setLanguage(language:)`,
  `start(listener:)`, `snapshot()`, `refresh()`, `reply(committee:thread:body:)`,
  `take(committee:thread:)`, `close(committee:thread:)`, `approve(id:decision:)`
  (the four are `async throws`: they run on the core's runtime and return when
  the daemon has answered), `codexTerminal(threadId:)` (returns the argv, cwd,
  shell line and AppleScript; Swift runs it, so the core never drives other
  apps), `senateHome()`, and the free function `splitMentions(text:)`. Errors
  are `CoreError` (`NotStarted`, `AlreadyStarted`, `Invalid`, `Failed`,
  `Setup`). UniFFI is pinned to `=0.32.2`; the bindings are generated by the
  same version from the built library's metadata (`--features bindgen --bin
  uniffi-bindgen`).
- **Swift** is thin: `CoreStore` keeps the latest `AppState` and what only the
  UI knows (the tab, the agent filter, reply drafts, rows in flight);
  `ListenerBridge` hops callbacks to the main queue in order; the views render
  the state and call the store. Linking: the bridging header includes
  `Generated/senate_app_coreFFI.h`; the target links
  `Generated/libsenate_app_core.a` plus `iconv`, CoreFoundation, Security and
  SystemConfiguration.

### Two languages

The split is by where the words come from.

- **Text built from data is written in Rust**, for the language the app passes
  to `AppCore(senateHome:language:)`: the menu lines and counts (`2 pending
  threads, 1 approval` / `2 个待处理线程，1 个审批`), stream states, the labels
  of states, kinds and triage verdicts, why an item awaits the Speaker, the
  offline reason, and the core's own errors. The app passes
  `Bundle.main.preferredLocalizations.first`, so the core and the labels agree
  on one language: `zh-Hans` or `en`, English when the system language is
  neither. Messages the daemon wrote (a refusal) are shown as it wrote them,
  in the language of `senate.toml`.
- **Fixed labels live in Swift**: `Senate/Resources/{en,zh-Hans}.lproj/
  Localizable.strings` (buttons, tabs, headings, hints, with `%@` / `%lld`
  slots, looked up with `L("key")`) and `InfoPlist.strings`. Both files carry
  every key.
- Senate, senator and Speaker are never translated, in either place. In
  running text an id is a bracketed mention (`[senator::alice/claude]`),
  written the same in both languages; `MentionText` sets it apart (monospaced,
  coloured by what it names) wherever text may hold one. What people wrote
  (a thread's title, a docket item's body) is shown as written.

## What it does

### Menu bar

The icon (filled when online) with the pending count, threads plus approvals,
next to it. The menu: `Senate: online as <person>` or `Senate: offline`;
`2 pending threads, 1 approval`; `Docket: 3 awaiting the Speaker` while one of
your agents presides; one line per stream (`alice/claude · refund-v2 · live ·
seq 120`); *Open Inbox* (⌘I), *Open Approvals* (⌘A), *Open Docket* (⌘D, while
presiding), *Quit Senate* (⌘Q). The app is menu-bar only (`LSUIElement`: no
Dock icon). The window starts closed (macOS 15+; on 14 SwiftUI opens it at
launch); closing it leaves the app running.

### Window

A header with the connection (`online as alice · http://…` or why it is
offline) and, with more than one Senate, one chip per Senate; the last error;
a Refresh button (⌘R); and the tabs.

**Inbox**: the threads waiting on you, for every committee the daemon has a
stream for. With more than one agent, a menu narrows it to one agent
(`alice/claude`, `alice/codex`; each with its count); a thread put to you
stays in every agent's view. Each row: title, committee, kind, state, triage
verdict and reason, who asked (`[speaker::bob/claude]` is the Speaker
answering for that senator), who it was addressed to, how long it has been in
its state, owner, takeover, escalation.

- *Reply* opens a box; *Send* (⌘↩) answers in the thread as you,
  `[senator::alice]` (`answer` with `as_person`). Drafts survive refreshes.
- *Take* takes the thread over (`take`): the daemon cancels queued deliveries
  of it to the agent.
- *Close* asks for confirmation, then archives the thread as you (`task` with
  state `archived`, `as_person`).

**Approvals**: Codex app-server approval requests the daemon relays: what it
wants to run or change, where, and why. *Allow*, *Allow for session*, *Deny*,
*Deny and stop* (cancel the turn). The first answer wins across the TUI, the
app and any chat platform: one answered elsewhere disappears; answering one
already answered shows the daemon's refusal. Each new request posts a macOS
notification; clicking it opens this tab.

**Codex**: the Codex threads registered with the daemon, in every Senate:
committee, the agent it speaks as, state (live / stale), thread id,
directory, branch, waiting approvals. *Open in Terminal* opens Terminal.app in
the thread's directory running `codex resume --remote <addr> <thread>`; *New
Codex in Terminal* runs `codex --remote <addr>`. `<addr>` is `[codex] listen`
from `senate.toml`, else `unix://` (the managed app-server daemon, as `senate
install` sets up); `[codex] bin` replaces `codex`. macOS asks once to let
Senate control Terminal.

**Docket** (only while one of your agents presides over a Senate): what awaits
that Speaker, per committee, oldest first. Each item says why it waits: a
message for one of your agents that the Speaker must route (*route*), a
message addressed to the Speaker (*answer*), or a question to agents none of
whom is connected (*cover*), plus the agents seated there with their latest
checkpoint. With more than one presided Senate, a menu picks one. **It is
read-only.** Routing, covering and referring to a person are done by the
presiding agent itself, with its own tools (`speaker_route`, `speaker_cover`,
`speaker_escalate`), in its session, never by a person in this window. The tab
exists to see what is waiting for a presiding session that is closed or stuck.

**Senates**: every Senate the daemon serves: its title (`Senate`, or `Senate
under alice/claude`), its address, who you are in it, who holds the Speaker's
seat, your agents there (runtime, sessions, whether presiding, what waits) and
each stream with its state and the seq delivered.

## IPC used

| Request | When |
| --- | --- |
| `watch` | held open for the app's life; `status`, `approval_requested`, `approval_resolved`, `inbox`, `lagged` drive everything |
| `inbox {committee, agent}` | for each (agent, committee) stream of the primary Senate: on every `status` (each sweep, 30 s), on an `inbox` event for it, after an action, on Refresh |
| `docket` (no session) | while an agent of yours presides: on every `status`, on any `inbox` event, on `lagged`, on Refresh |
| `approvals` | after `lagged`, on Refresh |
| `answer {thread, body, committee, as_person: true}` | Reply |
| `take {thread, committee}` | Take |
| `task {thread, status: {state: archived}, committee, as_person: true}` | Close |
| `approve {id, decision}` | the approval buttons |

A request that names no session (the app has none) is served by the
**primary Senate**, the one where your agents sit directly. So inboxes,
replies, take and close work for that Senate. A Senate one level down is known
by its status (streams, sessions, Codex threads) and its docket; the daemon
has no way yet to read a person's inbox there from a client without a session.

## Not in v1

- Claude Code approvals (they need Channels' permission relay).
- Acting on the docket (see above), asking, posting, search, the brief,
  checkpoints: the app is a menu and an inbox, not a second IDE. Use the agent.
- An inbox for a Senate below the primary one (see IPC above).
- Real lag numbers: `status` reports the seq delivered per stream but not the
  server's latest, so "behind" means the daemon's stream is not live.
- Starting the daemon from the app, login items, auto-update, an app icon,
  signing with a team identity and notarization.
- Swift UI tests; Windows and Linux (the core is portable, a shell for them is
  not written).
