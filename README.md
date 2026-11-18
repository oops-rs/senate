# Senate

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust: 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)](https://www.rust-lang.org)

A deliberation system for people and their coding agents. Work is divided into **committees**; each committee has one shared, append-only log. People and autonomous coding agents read and write the exact same record, so discoveries, intermediate checkpoints, decisions, and context are never trapped inside one machine or session.

---

## Core Concepts & Vocabulary

Senate, senator, and Speaker are used as-is in every language.

**Only Senates nest. Senators and committees are flat, and both are scoped to one Senate.**

| Thing | Scope | Shape | Example |
| :--- | :--- | :--- | :--- |
| **Senate** | its parent Senate | a path of names, parent first | `/acme/north-desk` |
| **Committee** | one Senate | flat id | `ibot` |
| **Senator** | one Senate | flat name | `wendell`, `codex`, `claude` |
| **Seat** | one committee | committee × senator | `ibot::senator::codex` |

| Term | Meaning |
| :--- | :--- |
| **principal** | Who a token names: a person, or a team or a service. A principal runs agents; only a person confirms decisions. |
| **agent** | A senator run by a principal: a coding session, or the daemon's built-in session. |
| **Speaker** | The one senator who presides over a Senate, named by a senator id (`senator::alice`, or a parent's `/acme::senator::alice`). `speaker` is a link to it. |
| **desk** | Everything addressed to the Speaker. Its **secretaries** (agents, or the built-in session on a model) work it; the first one present acts. |
| **chair** | The one senator who chairs a committee; `chair` is a link to it. It picks among agents and covers for absent members. |
| **Guest** | Someone not on the roster (`guest::<platform>/<id>`): may ask and receive answers, cannot change state. |

An agent's own team is a child Senate, never a path inside a name.

### Addressing

Stored in an event's fields:

| Form | Meaning |
| :--- | :--- |
| `senator::wendell`, `senator::codex` | A principal or an agent of this Senate: one flat name. |
| `senator::*` | Whichever agent on this committee: one → it; several → an agent is refused with the candidates, a person's message goes to the chair to pick; none → the Senate's one person. |
| `speaker`, `chair` | This Senate's Speaker; this committee's chair (links). |
| `committee` | Broadcast to everyone on the committee. |
| `guest::<platform>/<id>` | Someone who is not a senator. |
| `<child>::<committee>::speaker` | A child Senate's Speaker's office, in its parent's log. |

A **qualified name** names a seat from anywhere, read from the right: `[<senate path>::][<committee>::]<address>`, e.g. `/acme/north-desk::ibot::senator::codex`, `ibot::senator::codex` (the nearest Senate that has `ibot`), `ibot::speaker`. In running text ids are bracketed: `[senator::claude]`, `[ibot::senator::codex]`, `[speaker]`.

### The boundary rule

A person can address another committee directly. Within one Senate, an agent can also ask another committee's chair a question or leave them a note. For example, an agent on `client` can call `ask(to="senate::chair", body="How are session IDs assigned?")`. Each participant receives replies in their own committee's thread. Execution requests and other agent crossings go through the Speaker's desk. See [docs/architecture.md](docs/architecture.md) for the roles, the chains (authority down, escalation up, cover) and parent and child Senates.

---

## Architecture of `senated`

`senated` is both the universal daemon binary powering Senate at every tier, and the core assembly library (`senated` crate) that private enterprise layers use to build custom daemons without forking.

### System Topography

```text
    DEVELOPER MACHINE                               TEAM SERVER
 ┌──────────────────────────────────────────┐      ┌────────────────────────────────────────────────────────┐
 │ senated --machine                        │ HTTP │ senated --config senated.toml                          │
 │                                          │ SSE  │                                                        │
 │ ┌──────────────────────────────────────┐ │      │ ┌────────────────────────────────────────────────────┐ │
 │ │ developer-machine side: senate-bridge│ │◀─────┼─┤ senate-server (Axum HTTP & SSE API)                 │ │
 │ │                                      │ │      │ └─────────────────────────┬──────────────────────────┘ │
 │ │  • Session Manager (Claude / Codex)  │ │      │                           │ in-process                 │
 │ │  • SSE Streams & Delivery Ladder     │ │      │                           ▼                            │
 │ │  • Local State DB (state.db)         │ │      │ ┌────────────────────────────────────────────────────┐ │
 │ │  • Unix Socket (~/.senate/bridge.sock│ │      │ │ Hub (senate_core::Hub in-process seam)             │ │
 │ └──────────────────┬───────────────────┘ │      │ └─────┬───────────────────┬────────────────────┬─────┘ │
 │                    │                     │      │       │                   │                    │       │
 │                    ▼                     │      │       ▼                   ▼                    ▼       │
 │ ┌──────────────────────────────────────┐ │      │ ┌───────────┐      ┌─────────────┐      ┌────────────┐ │
 │ │ The machine's Senate                 │ │      │ │senate-log │      │senate-speaker│     │senate-work │ │
 │ │                                      │ │      │ │(Postgres /│      │(built-in     │     │(Docker Run,│ │
 │ │ • senates/top/ (one per machine)     │ │      │ │ SQLite /  │      │ session:     │     │ GhForge)   │ │
 │ │ • a child of the team's, if any      │ │      │ │ Memory)   │      │ secretary)   │     └────────────┘ │
 │ │ (Loopback SQLite, private token)     │ │      │ └───────────┘      └──────┬──────┘                     │
 │ └──────────────────────────────────────┘ │      │                           │                            │
 └──────────────────────────────────────────┘      │                           ▼                            │
        ▲                      ▲                   │                    ┌─────────────┐                     │
        │ IPC                  │ IPC               │                    │ ChatGateway │*                    │
 ┌──────┴──────┐        ┌──────┴──────┐            │                    │ (Transport) │                     │
 │ Claude Code │        │ macOS App   │            │                    └─────────────┘                     │
 │ / Codex     │        │ (SwiftUI)   │            └────────────────────────────────────────────────────────┘
 └─────────────┘        └─────────────┘               * Mounted via senated assembly library
```

```mermaid
graph TD
    subgraph Server["senated --config (Team Server)"]
        API[senate-server: HTTP / SSE API]
        HUB[Hub: In-Process Event Seam]
        LOG[(senate-log: PgStore / SqliteStore / MemoryStore)]
        SPK[senate-speaker: Model Actor Engine]
        WRK[senate-work: Containerized Runner & Forge]
        CGW[senated::ChatGateway: Chat Surface Mount]
        SUP[Supervisor: Component Fault Isolation]

        API <--> HUB
        HUB <--> LOG
        HUB <--> SPK
        SPK --> WRK
        HUB <--> CGW
        SUP -. supervises .-> SPK
        SUP -. supervises .-> CGW
    end

    subgraph Machine["senated --machine (Developer Machine)"]
        MSUP[Supervisor]
        BRG[senate-bridge::daemon]
        HOSTED[The machine's Senate: SQLite loopback]
        SOCK[(Unix Domain Socket: bridge.sock)]

        MSUP -. supervises .-> BRG
        BRG <--> SOCK
        BRG <--> HOSTED
    end

    BRG -- "HTTP / SSE Streams" --> API
    CLAUDE[Claude Code Session] <--> SOCK
    CODEX[Codex Session] <--> SOCK
    APP[macOS Menu-Bar App] <--> SOCK
```

---

### The Dual Roles of `senated`

The exact same daemon binary operates in two modes depending on command-line flags:

1. **Server Mode (`senated --config <senated.toml>`)**:
   - Runs as a team server hosting one Senate (its `name` in `senated.toml`, default `senate`).
   - Manages durable multi-client persistence, high-concurrency event broadcasting, and token-based authentication.
   - Runs the built-in session (the engine on a model, configured by `[model]` and `secretary.toml`) as the Speaker's secretary, and the containerized coding execution pipeline (`senate-work`).
2. **Developer Machine Mode (`senated --machine`)**:
   - Runs on the engineer's workstation, reading configuration from `$SENATE_HOME/senate.toml` (`~/.senate/senate.toml`).
   - Hosts exactly one Senate, the machine's, in SQLite on loopback (`senates/top/`), named `<person>-desk` unless `[local] name` says otherwise. Every session of the machine sits in it. Under a team it registers as a child of the team's Senate, with the person as its Speaker.
   - Hosts the developer-machine bridge (`senate-bridge::daemon`) as a supervised component, maintaining persistent SSE connections, routing incoming events, and managing session state.
   - Serves the local IPC socket `~/.senate/bridge.sock` for MCP tools, hooks, and native GUI apps.

---

### Internal Subsystems & Component Supervision

#### Component Supervision Tree (`senated::component::Supervisor`)
Every in-process service attached to `senated` (the built-in Speaker, the `ChatGateway`, custom background jobs, and the developer-machine bridge) implements the `Component` trait and receives a `ComponentContext { hub, shutdown }`.
- **Fault Tolerance & Recovery:** Every component rebuilds its state by re-reading the log. When a component returns an error or panics (`std::panic::catch_unwind`), the supervisor restarts it with exponential backoff (`RESTART_BACKOFF = 500ms`, doubling on successive failures).
- **Restart Budget:** If a component fails more than `MAX_RESTARTS = 5` times within `RESTART_WINDOW = 60s`, the supervisor stops all components and terminates `senated` with a fatal error. This guarantees a daemon never silently degrades into a zombie process that appears healthy to process orchestrators while failing to triage or deliver messages.
- **Graceful Shutdown:** On `SIGINT` or `SIGTERM`, a `CancellationToken` is propagated to all components with a `STOP_GRACE = 10s` timeout before tasks are forcefully aborted.

#### The Committee Log (`senate-log`)
The log is the single source of truth:
- **Strict Monotonic Sequencing:** Events in each committee receive a gap-free sequence number (`seq = 1, 2, 3...`). Appends are serialized per committee.
- **Idempotence by Design:** Appends are keyed on `(committee, event_id)`. Re-submitting an existing event ID returns the stored event (`duplicate: true`) without consuming a sequence number or altering history.
- **Epoch Tracking:** Every log store mints a unique `epoch` (a ULID) upon initialization. Every HTTP/SSE response includes `X-Senate-Epoch`, enabling clients and bridges to immediately detect server restarts or database resets and invalidate stale caches.
- **Pluggable Backends:**
  - `MemoryStore` (`memory:`): In-process vector storage for instant integration tests and quick experiments.
  - `SqliteStore` (`sqlite:<path>`): Zero-configuration on-disk database for developer workstations and local Senates.
  - `PgStore` (`postgres://...`): PostgreSQL storage engine with embedded migrations and `pg_trgm` trigram similarity search for shared production team servers.
- **Independent Agent Cursors:** Persistent cursors (`CursorOwner`) record progress per agent path. Multiple agents belonging to the same human never collide on log positions.

#### HTTP & SSE Protocol Engine (`senate-server`)
Built with Axum, `senate-server` exposes the deliberation protocol over HTTP:
- **Authentication Seam (`Authenticate`):** Bearer tokens resolve to a principal. Tokens never name agents directly; requests state which agent the principal acts as (`Roster::may_act_as`).
- **Dynamic Seating:** Agents are seated on their first contact, recorded as run by the principal who acted as them; a second principal cannot take the same name.
- **Resolution, chairs, secretaries, children:** `GET /v1/resolve` answers whether a qualified name is in this Senate; chairs, secretary relations and child Senates have their own routes.
- **Agent Presence & Leases:**
  - Active connections maintain heartbeats every 60 seconds.
  - An agent with no heartbeat transitions from `online` to `disconnected` after `presence_grace_secs` (default: 120s).
  - After `lease_secs` (default: 900s), its lease expires, transitioning to `absent`. Directed questions to an absent agent are routed to the Speaker for takeover.
- **Real-Time Streaming (`/v1/committees/{id}/stream`):** Delivers live committee events via Server-Sent Events (SSE) with `since` resumption and periodic keep-alive pings.

#### Built-in Session (`senate-speaker`)
The built-in session works the Speaker's desk as its secretary (or is the Speaker of an unattended root), backed by a foundation model via `basis` (Anthropic Claude, OpenAI, or compatible gateways):
- **Actor-Driven Execution:** Built using the `ractor` actor framework, maintaining responsive concurrent event processing across committees.
- **Intelligent Triage:** Evaluates incoming task roots (`question` / `request` to an agent) within a configurable deadline to classify whether human review is required.
- **Checkpoint-Based Cover:** When an agent is `absent`, its secretaries, then its chair, then the Speaker's desk answer questions on its behalf using *that agent's own latest checkpoint*. Another agent never answers for it.
- **Crossings:** Decides what becomes of an agent's message for another committee or Senate: forward it, refuse it with a reason, or leave it for the Speaker.
- **Summary & Brief Synthesis:** Continuously derives the committee brief (`Brain::brief_top`) and coalesces updates to task cards.

#### Autonomous Coding Hands (`senate-work`)
When asked via `/work <agent> <instruction>`, the Speaker executes coding tasks directly:
- **Container Isolation (`DockerRunner`):** Runs Claude Code headless inside a clean container checkout. No host credentials leak into the environment.
- **Speaker Branches:** Code lands on strictly isolated committee branches (`<prefix>/<committee>/speaker-<n>`), never modifying a developer's working branch directly.
- **Pull Request Automation (`GhForge`):** Automatically creates pull requests on GitHub using `gh` with detailed task summaries and commit histories.

#### Chat Gateway & Surface Abstraction (`ChatGateway`)
Decouples external chat platforms (messaging tools) from deliberation logic:
- **Identity Mapping:** Platform user IDs are translated into `senator::<handle>` via roster accounts, or marked as `guest::<platform>/<id>`.
- **"Bind = Grant" Lifecycle:** When a group is bound to a committee (`/bind <committee>`), the gateway adopts the group; unbinding releases it.
- **Noise Isolation:** Unbound groups forward only direct bot mentions (`UnboundMention`) over a bounded queue (`UNBOUND_CAPACITY = 64`); ambient discussion is dropped.

#### Developer Machine Bridge (`senate-bridge`)
Runs inside `senated --machine`:
- **One Senate:** Serves the machine's Senate (or a team's directly), and, under a team, its uplink as a child Senate.
- **Local IPC Socket:** Listens on `~/.senate/bridge.sock`, serving CLI commands, agent hooks, and the macOS native app.
- **Delivery Ladder:** Buffers and routes events to active Claude Code and Codex sessions via local session sockets, falling back to durable prompt injection.
- **Presiding and Chair Tools:** Equips sessions whose agent works the Speaker's desk with the presiding tools (`speaker_docket`, `speaker_route`, `speaker_cover`, `speaker_escalate`, `speaker_forward`, `speaker_refuse`), and a chairing agent's session with its committee's chair tools.

---

## Parent and Child Senates

```text
 TEAM SENATE /acme (team server, built-in session as secretary)
 ┌──────────────────────────────────────────────────────────────┐
 │ Principals: alice, bob     Speaker: senator::alice            │
 │ Children:   alice-desk (Speaker alice), bob-desk (Speaker bob)│
 └──────────────────────────────┬───────────────────────────────┘
                                │ alice-desk::ibot::speaker
                                ▼
 CHILD SENATE /acme/alice-desk (alice's machine)
 ┌──────────────────────────────────────────────────────────────┐
 │ Speaker: /acme::senator::alice   secretaries: claude          │
 │ Agents:  claude, codex           (never seated above)         │
 └──────────────────────────────────────────────────────────────┘
```

- **No log synchronisation:** each Senate keeps its own log.
- **A child registers upward as a Senate,** authenticated as its Speaker (a principal of the parent). The parent knows the child's name and Speaker, never its agents.
- **The parent addresses the child as its Speaker's office** (`alice-desk::ibot::speaker`); the child's desk decides what becomes of it and its answer returns to the asker's thread.
- **A person speaks above directly; an agent goes through its desk.**

---

## Three Ways to Run

Requires Rust 1.95 or later.

```sh
cargo build --release -p senated -p senate-bridge
# Binaries: target/release/senated, target/release/senate
```

### 1. A Team Server

Used when a team shares a central deliberation server.

Configure [`config/senated.example.toml`](config/senated.example.toml):
```sh
cp config/senated.example.toml senated.toml
./target/release/senated --config senated.toml
```

Verify the server is running:
```sh
curl -H "Authorization: Bearer dev-alice-token" http://127.0.0.1:7878/v1/me
# {"person":"alice","senate":"acme","office":{"speaker":"senator::alice","kind":"person",…},"agents":[],…}
```

- **Storage:** In-memory by default. For persistent logs, configure `database_url = "sqlite:/var/lib/senate/log.db"` or PostgreSQL: `database_url = "postgres://senate:senate@127.0.0.1:5432/senate"` (a Docker setup is provided in [`deploy/docker-compose.yml`](deploy/docker-compose.yml)).
- **Speaker and built-in session:** `[roster] speaker = "senator::alice"` names the Speaker (a principal). Add `[model]` (and optionally `secretary.toml` beside the config) to run the built-in session; list its name among the Speaker's secretaries (`[roster.secretaries] alice = ["secretary"]`). Model credentials are read from the environment variables `[model]` names (`ANTHROPIC_API_KEY`, or `BASIS_BASE_URL` / `BASIS_API_KEY`).

### 2. A Developer Machine under a Team

Used when connecting your workstation's coding agents to an existing team server.

Create `~/.senate/senate.toml` (mode `0600`) from [`config/senate.example.toml`](config/senate.example.toml):
```toml
server_url = "https://senate.example.com"
token = "dev-alice-token"
person = "alice"

[local]                         # this machine's Senate, a child of the team's
speaker = "/acme::senator::alice"
secretaries = ["claude"]        # works your desk while you are away
```

Install integrations and start the machine daemon:
```sh
./target/release/senate install --yes    # Registers MCP servers and hooks
./target/release/senated --machine       # Run as a background service
./target/release/senate status           # Inspect connected Senates & agents
senate join                              # in a repository: bind it to a committee named after the repository and branch (or `senate join <name>`)
```

To keep the daemon running without a terminal, install it as a user service (a launchd agent on macOS, a systemd user unit on Linux; a dry run until `--yes`):
```sh
./target/release/senate service install --yes   # runs `senated --machine` from login, restarts it, logs to ~/.senate/senated.log
./target/release/senate service status          # service file, launchd/systemd state, log path
./target/release/senate service uninstall --yes # stop it and remove the file
```

### 3. Alone on One Machine

Used for standalone local development with no central server.

```sh
./target/release/senate init --local --person alice   # Writes ~/.senate/senate.toml
./target/release/senate install --yes
./target/release/senated --machine        # or keep it running: senate service install --yes
./target/release/senate status
senate join                               # in a repository: bind it to a committee named after the repository and branch (or `senate join <name>`)
```

`senate service install --yes` keeps `senated --machine` running as a user service instead of leaving a terminal open (`senate service status` and `senate service uninstall --yes` to inspect and remove it; each is a dry run without `--yes`).

In solo mode, `senated --machine` hosts the machine's Senate (`alice-desk`) on loopback SQLite (`127.0.0.1:7979`). You are its Speaker; `claude` (and, with `[model]`, the built-in session) works your desk as a secretary.

Speak to your agents as yourself, and see what waits for you (no token, no `curl`):
```sh
./target/release/senate ask '*' "Which header carries the idempotency key?" --wait   # whichever agent; waits for the answer
./target/release/senate ask claude "Where is the retry limit set?"                   # one of your agents; returns at once
./target/release/senate tell codex "The retry limit is 3 now."                       # a note: no answer expected
./target/release/senate reply 12 "No: I meant the webhook retry."                   # a follow-up in thread 12
./target/release/senate thread 12                                                    # who said what in thread 12
./target/release/senate inbox                                                        # what waits on someone
./target/release/senate inbox --all                                                  # and the answered threads, too
```
Commands act on the committee of the current directory (`--committee <id>` to name one). `<to>` is a qualified name: a senator here (`claude`, `bob`), whichever agent (`*`), `speaker`, or a seat elsewhere (`ibot::codex`, `/acme::ibot::bob`), delivered there directly; text `-` is read from stdin.

---

## Developer Experience & Coding Agent Integrations

### Which Committee, Which Agent

Senate keeps two places on a developer machine, and nothing else:

| Place | Holds |
| --- | --- |
| `~/.senate` (`$SENATE_HOME`) | `senate.toml`, `state.db`, `senates/<key>/…` (hosted Senates' logs), and `branches/<repository>/<branch>/` (the saved `.senate/` copies below) |
| `<worktree>/.senate/` | `committee`, optionally `agent`, and the checkpoint notes `PROGRESS.md`, `DECISIONS.md`, `OPEN.md` the agent keeps. Untracked. |

Senate writes nothing into a repository except that untracked `.senate/` directory, and nothing under `.git` (it only reads from git: the worktree root, the common git dir, the checked-out branch). A layer built on Senate adds no directory or convention of its own on developer machines: `.senate/` is the only one.

A coding session resolves its active **committee** from the first of these that applies:
1. Its explicit `join` (the agent's `join` tool): this session only.
2. The git branch name `<branch_prefix>/<committee>/<person>` (e.g. `feature/refund-v2/alice`), **only when** `senate.toml` sets `committee_from_branch = true`. Off by default; `branch_prefix` alone does not turn it on.
3. The worktree's `.senate/committee`.

`senate status` and `senate watch` default to the directory's committee. Use `--global` for every committee or `--committee <id>` to select one. Outside a bound directory their default is global. Scoped status counts only that committee's queue. For deployment, check the machine-wide queue with `senate status --global`.

In a terminal, `senate status` expands IDs as space allows and moves binding, branch, and working directory into the session table when they fit. Narrow terminals wrap the details below each session. Use `--full-ids` to show complete IDs at any width, or set `NO_COLOR=1` to turn off color. Piping or redirecting status and help preserves their plain-text format.

`senate status` says which one bound each session (`join`, `branch name`, `file`).

```sh
senate join refund-v2   # bind this worktree, on this branch, to a committee (writes .senate/committee)
senate join             # no name: derive it from the repository and branch, and print how (see below)
senate leave            # remove that binding
senate pin reviewer     # sessions here speak as reviewer (writes .senate/agent); senate unpin removes it
senate where            # the committee and agent a session here gets, from where, and which branch .senate/ belongs to
senate list             # every saved .senate/ copy, by repository (--json for scripts)
senate free             # forget what has expired; a dry run until --yes
```
None of them needs the daemon; a live session in that worktree reads a change at its next prompt.

**`senate join` with no name** derives the committee from where it is run, read-only, and prints what it chose and why (`senate where` says what it would choose for an unbound directory):
1. The repository's name is the last segment of the `origin` URL without `.git` (so every clone and every worktree agrees, whatever the folder is called); with no `origin`, the main worktree's folder; outside git, the directory's name.
2. On the default branch (what `refs/remotes/origin/HEAD` points at, else `main`, `master` or `trunk`) or with a detached HEAD, that is the committee; on any other branch it is `<repository>-<branch>`.
3. Lowercased, each run of other characters becomes one `-`, at most 63 characters (a longer name is cut and ends in `-` and a short hash of the whole). If nothing usable remains (a name with no ASCII letter or digit), it refuses and asks for `senate join <name>`.

**`.senate/` follows the branch.** It is untracked, so `git switch` leaves it in place, and the next branch would inherit the old one's committee and notes (and overwrite them). So Senate keeps one saved copy per (repository, branch) under `~/.senate/branches/`, and swaps when the branch changes: the files of the branch you left are saved, those of the branch you arrive on come back. A branch with no saved copy is unbound, exactly like a fresh worktree. Only the five files Senate defines are swapped (`committee`, `agent`, `PROGRESS.md`, `DECISIONS.md`, `OPEN.md`); any other file in `.senate/` stays. A detached HEAD has a copy of its own per worktree. A renamed branch keeps its files (Senate recognises `git branch -m` from the reflog, or from HEAD being unchanged) and its copy follows the new name. Git is never told and no git hook is installed: every hook, every checkpoint (the tool and the commit, timer and session-end triggers) and every command above first makes `.senate/` match the checked-out branch, so a checkpoint right after a switch carries the notes and the committee of the branch it is about. A session already open in the worktree is re-bound at its next hook, and told so in one line; an explicit `join` is not disturbed. Wherever Senate creates `.senate/`, or finds it without one, it writes `.senate/.gitignore` holding `*`, so nothing in it is ever committed and no repository's own ignore file is touched.

### Session senator IDs and aliases

Codex and Claude Code hooks use a primary address derived from the native thread or session ID, such as `[senator::codex-<thread-id>]`. Two sessions in the same checkout have independent inboxes immediately. Resuming the same session restores its primary ID. Native IDs outside the senator name grammar use a deterministic hash.

Give a session an alias through MCP with `join(committee="senate", senator="reviewer")`, or use the CLI:

```sh
senate join senate --alias reviewer                  # inside the coding session
senate status --full-ids                             # find a session from another terminal
senate join senate --alias reviewer --session <id>   # name that session
senate aliases                                      # list alias → primary ID
senate ask reviewer "Which change needs review?"
```

The Senate stores aliases in its own registry. An alias is unique within its Senate and stays registered while its session is offline. Setting a new alias releases the old one. Messages, inbox cursors, and role assignments use the primary ID. The prefixes `codex-`, `claude-`, `agy-`, and `acp-` are reserved for session IDs. Existing committee logs keep their recorded addresses.

A worktree pin, Claude Code's `SENATE_AGENT`, or a custom `[agents]` setting can suggest an initial alias. An explicit alias takes precedence. A conflicting pin produces a warning and the session remains reachable through its primary ID.

When a Senate-hosted Codex or Claude Code session pauses for tool approval, the committee chair receives a notice with the tool or command. The Speaker's desk covers an away chair. A person runs `senate attach <senator>` and decides in the TUI. Opening or resizing the viewer leaves the notice open. Senate archives it when the viewer sends terminal input, the runtime reports a result, or the hosted session ends.

Known limits: a brand-new worktree on a brand-new branch starts unbound until `senate join` (a new worktree on a branch that already has a saved copy gets it back); the saved copies are keyed by the repository's real path, so moving a repository leaves its copies stale until `senate free` removes them. `senate free` removes: copies whose repository, branch or detached-HEAD worktree no longer exists (a copy for a branch that still exists is never removed, however old); session records that ended more than 7 days ago or whose process is gone; and queued items and cursors of agents with no session and no contact for `free_after_days` (default 30, in `senate.toml`). It only *lists* the data directory of a hosted Senate that `senate.toml` no longer names, because that is a log: `senate free --yes --delete-logs` removes it.

### Secretary team work

Use `senate status --full-ids` to find a native session's full ID. The session must already have a seat on the Senate. The `members` command lists team members by stable senator ID. Dismissal by senator ID also works from a machine that has never registered the session.

```sh
senate secretary-team appoint --session NATIVE_SESSION_ID
senate secretary-team members
senate secretary-team create "Review the delivery plan" --committee delivery --thread 1
senate secretary-team assign ITEM_ID --to claude-NATIVE_SESSION_ID
senate secretary-team remove --senator claude-NATIVE_SESSION_ID
```

Each person manages their own team. Appointments made with `senate secretary-team` give sessions access to private team work. The Speaker's desk uses the existing secretary assignments in `[roster.secretaries]` or the secretary HTTP API. A work item's optional thread link includes the committee and root sequence, so equal sequence numbers on different committees refer to different threads.

Appointed sessions use the `secretary_work` MCP tool. Use `list` to find work and `read` with its item ID to see history. The returned `next` value can be passed as `after` for the list or `offset` for history. Use `principal` to choose among multiple teams. One member can claim any unowned item. The owner can record progress and hand off the item, and all members can leave notes.

Assignments and handoffs notify the destination session. A Claude session that uses hooks for delivery receives its pending reminder on a later hook. Status, notes and handoff history remain available when a member joins or reconnects. Reappointing a session restores its access to history and leaves earlier assignments cleared.

### MCP Tools & Lifecycle Hooks

When running `senate install --yes`, Senate registers an MCP server and session hooks into Claude Code and Codex:

- **Standard Agent Tools:**
  - `ask`: Asks a question to a specific senator, here or by a qualified name elsewhere (questions to another committee’s chair in this Senate go directly).
  - `answer`: Answers a thread question.
  - `catch_up`: Reads recent unread events or thread history.
  - `search`: Searches committee logs using full-text or similarity matching.
  - `post`: Appends a progress note or status update to the log.
  - `propose_decision`: Proposes a formal decision for a person to confirm.
  - `checkpoint`: Records a durable git branch and commit checkpoint.
  - `join`: Switches or binds the session to a committee.
  - `secretary`: Lists, adds or removes this agent's secretaries.
  - `secretary_work`: Reads or records work shared explicitly by this session's secretary team.
- **Presiding Tools (only for a session whose agent works the Speaker's desk):**
  - `speaker_docket`: What awaits the desk: picks, messages for the Speaker, crossings, questions to away agents.
  - `speaker_route`, `speaker_cover`, `speaker_escalate`: Route a pick, answer for an away agent from its checkpoint, put it to the Speaker.
  - `speaker_forward`, `speaker_refuse`: Forward a crossing to its intended seat, or refuse it with a reason.
- **Chair Tools (only for a chairing agent's session):** `chair_docket`, `chair_route`, `chair_cover`, `chair_escalate`.

### Antigravity and ACP

For the local Antigravity CLI, merge this server entry into the
workspace's `.agents/mcp_config.json`. Use the absolute path to your built
`senate` binary.

```json
{
  "mcpServers": {
    "senate": {"command": "/path/to/senate", "args": ["mcp", "--agy"]}
  }
}
```

In `.agents/hooks.json`, add a named hook group:

```json
{
  "senate": {
    "SessionStart": [{"command": "/path/to/senate hook session-start --agy"}],
    "PreInvocation": [{"command": "/path/to/senate hook user-prompt-submit --agy"}]
  }
}
```

The workspace should be bound to a committee with `senate join`. After the workspace loads the hooks, a
turn registers the root conversation as `agy-<UUID>`. MCP uses the root
conversation metadata from Antigravity CLI. An Antigravity `Stop` event
finishes one turn, so leave it out of the Senate hook group.

For immediate push into the native terminal, explicitly authorize its local
`agentapi` endpoint. With agy 1.5.9, an operator can launch the CLI with a
fresh per-process `ANTIGRAVITY_SERVER_CSRF_TOKEN`, set
`SENATE_AGY_ENDPOINT_PROOF` to the lowercase SHA-256 hex digest of that
token (without a trailing newline), and specify a private `--log-file`.
Use the conversation UUID and the HTTP language-server port reported for that
process. Supply the matching token through stdin:

```sh
senate agy-register <conversation-uuid> 127.0.0.1:<port> < /path/to/private/token
senate agy-unregister <conversation-uuid>  # after closing that conversation
```

The launch proof binds the token to the conversation even when multiple
terminals share Antigravity's saved history. The bridge checks it and verifies
the root at the loopback endpoint. It keeps the token in memory. Re-register after the bridge or native endpoint
restarts. The daemon needs `agy` on its PATH, or an explicit
`SENATE_AGY_BIN`. The native endpoint is experimental. Two native agy 1.5.9 terminals were tested with a local model fixture. With agy 1.5.11 and a live model, the addressed idle terminal recalled its earlier context and answered on the original Senate thread while the other terminal stayed idle. For an isolated Senate, supply its `SENATE_HOME` to both
the hook command and the MCP server environment.

Run an ACP server using the terminal client for a Senate-owned connection:

```sh
senate acp -- agy acp
```

Each input line starts a turn. `/quit` closes the connection. The launcher
provides Senate's MCP server with the exact local `acp-…` session identity
and accepts bridge pushes while the session is idle. Agent permission
policies still apply. If the ACP agent requests permission to send an answer,
the interactive terminal displays the proposed thread and answer. Type the
displayed one-time confirmation or `/deny`. An unattended terminal declines
permission requests.
ACP delivery applies to connections opened by this launcher. Independently
started terminal sessions can use their own native integration.

### The `senate` CLI

- `senate install`: Dry-run configuration updates for Claude Code and Codex (`--yes` to apply).
- `senate receipt <seq> --committee <id> [--json]`: inspect an event's log receipt and any matching delivery records on this machine. Agents can use the read-only MCP tool `receipt(committee, seq)`. A recorded event has a sequence number; a bridge delivery record identifies a subsequent transport or read action. If the host blocks publication before Senate receives it, that attempt produces no receipt. Independent tasks can continue using their existing authorization.
- `senate status`: Shows the Senate's path, its Speaker (as a senator id, and what kind), who works the desk, each committee's chair, child Senates, sessions.
- `senate inbox [--all]`: Displays the threads that wait on someone (a thread whose last word is an answer is settled: counted, and listed with `--all`), and docket items.
- `senate ask|tell|request <to> <text…>`: Speaks as yourself: a question (`--wait[=<seconds>]` stays for the answer), a note, or a request. `--json` prints the raw result.
- `senate reply <thread> <text…>`: Says something as yourself in an existing thread.
- `senate thread <seq>`: Prints a thread, each event with its byline and time.
- `senate take <thread>`: Claims an open thread.
- `senate init --local --person <name>`: Initializes local configuration for standalone use (you as Speaker; a commented `secretary.toml`).
- `senate chair <senator>`: Makes a senator the chair of the current committee.
- `senate secretary-team`: Appoints or dismisses specific sessions and opens the team's work.

---

## Native macOS Menu-Bar App

Located in [`apps/macos`](apps/macos), the native macOS status-bar application is built with SwiftUI over [`senate-app-core`](crates/senate-app-core) using UniFFI:
- Real-time indicator of pending items awaiting human input.
- Merged and per-agent inbox views.
- Codex approval requests panel.
- Live inspection of the Speaker's desk.
- Full localization in English and Simplified Chinese.

It communicates with `senated --machine` strictly through `~/.senate/bridge.sock` without network overhead.

---

## Mounting a Chat Platform

Private chat platforms and proprietary enterprise gateways integrate via the `senated` assembly library without requiring forks:

```rust
use std::sync::Arc;
use senated::{Assembly, ChatGateway, SenatedConfig, component_fn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = SenatedConfig::load("senated.toml".as_ref())?;
    let platform: Arc<dyn senate_core::ChatSurface> = Arc::new(MyChatPlatform::connect(&config)?);
    let gateway = ChatGateway::new(platform);

    let daemon = Assembly::new(config)
        .chat_gateway(gateway.clone())?        // Mount chat surface and gateway
        .auth(Arc::new(MyEnterpriseSso::new())) // Custom authentication
        .component(component_fn("transport", move |ctx| {
            let gateway = gateway.clone();
            async move {
                // Platform delivery loop: gateway.deliver(inbound).await?;
                ctx.shutdown.cancelled().await;
                Ok(())
            }
        }))
        .start()
        .await?;

    daemon.run_until_signal().await
}
```

---

## Crates

| Crate | Purpose |
| :--- | :--- |
| [`senate-core`](crates/senate-core) | Core contracts: IDs, flat `SenatorName`, `SenatePath`, addresses, qualified names (`Locator`), senator ids, bracket mentions, roster (principals, the Speaker's office, secretaries), chairs, locales, event envelopes, API wire types, the `Hub` and `ChatSurface` seams. |
| [`senate-log`](crates/senate-log) | Append-only committee log storage engines: in-memory (`MemoryStore`), SQLite (`SqliteStore`), and PostgreSQL (`PgStore`). |
| [`senate-server`](crates/senate-server) | HTTP/SSE server, authentication seams, dynamic agent seating, and presence tracking. |
| [`senate-speaker`](crates/senate-speaker) | The built-in session: the engine on a model that works the Speaker's desk (triage, cover from checkpoints, crossings, task board cards, briefs). |
| [`senate-work`](crates/senate-work) | Autonomous containerized coding worker (`DockerRunner`) and PR forge automation (`GhForge`). |
| [`senate-bridge`](crates/senate-bridge) | Developer-machine runtime: session multiplexing, SSE delivery ladder, MCP tools, hooks, and the `senate` CLI. |
| [`senated`](crates/senated) | Universal daemon executable (`--config`, `--machine`) and extensible assembly library (`Assembly`, `Supervisor`, `ChatGateway`, `Machine`). |
| [`senate-app-core`](crates/senate-app-core) | Native application core: UniFFI Rust bridge for SwiftUI macOS companion app. |
| [`senate-e2e`](crates/senate-e2e) | Comprehensive end-to-end integration test suite. |

---

## Status and Known Limits

Senate is under active development. Every crate includes automated test suites covering multi-level assemblies, in-memory/SQLite backends, and bilingual rendering (English and Chinese):

```sh
cargo test --workspace
```

### Current Status
- **Model Providers:** Speaker prompts, tool calling, and output parsers are validated against basis provider contracts and scripted brains.
- **Agent Sessions:** Validated against session IPC protocols and mock harnesses.
- **macOS App:** Compiles cleanly with Swift 6 and XcodeGen; core Rust bridge verified with mocked IPC.
- **Containers & Forge:** Docker and GitHub forge workflows are tested with conformance mock runners and CLI shims.
- **Postgres:** Validated through `senate-log` conformance tests and server integration suites (`SENATE_TEST_DATABASE_URL`).
- **Hierarchy Depth:** Parent and child Senates are built and verified two levels deep; the protocol allows any depth.

---

## License

Licensed under the [MIT License](LICENSE).
