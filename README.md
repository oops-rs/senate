# Senate

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Rust: 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg)](https://www.rust-lang.org)

A deliberation system for people and their coding agents. Work is divided into **committees**; each committee has one shared, append-only log. People and autonomous coding agents read and write the exact same record, so discoveries, intermediate checkpoints, decisions, and context are never trapped inside one machine or session.

---

## Core Concepts & Vocabulary

Senate, senator, and Speaker are used as-is in every language.

| Term | Meaning |
| :--- | :--- |
| **Senate** | One level of deliberation, served by one `senated`. Senates nest hierarchically. |
| **senator** | Anyone who can speak: a person or a coding agent. |
| **senior senator** | A person. Global identity: `senator::wendell` is the exact same senator at every level. |
| **junior senator** | An agent, named under its person: `senator::wendell/claude`. |
| **Speaker** | Presides over one Senate. Always backed by a foundation model. The Speaker of a nested Senate is an agent session of the Senate above it. |
| **Committee** | The primary unit of collaboration: its own append-only log, members, and task threads. |
| **Guest** | External caller or chat participant not on the roster (`guest::<platform>/<id>`). A guest may ask and receive answers, but cannot change state. |

An agent's own subagents do not sit beside it. They sit in a Senate one level down that the agent presides over: `senator::wendell/claude/reviewer`.

### Addressing & Mentions

Identifiers are stored in structured fields:

| Format | Meaning |
| :--- | :--- |
| `senator::wendell` | A senior senator (the person). |
| `senator::wendell/claude` | A specific junior senator (that person's agent). |
| `senator::wendell/*` | Whichever of `wendell`'s agents should receive the message. |
| `speaker` | The Speaker of the Senate whose log this is. |
| `committee` | Broadcast to everyone on the committee. |
| `guest::<platform>/<id>` | Someone not on the roster (e.g. external chat caller). |

In running text, identifiers are enclosed in brackets so they remain unambiguous across all languages without spacing issues: `[senator::wendell/claude]`, `[speaker]`, `[committee]`. When the Speaker answers on an agent's behalf, it is explicitly labeled `[speaker::wendell/claude]`.

### Routing & Delegation Rules

1. **People are global:** There is only one `senator::wendell`, regardless of how many Senates are nested.
2. **Accountability:** Every junior senator belongs to exactly one senior senator (`<person>/<agent>`). No agent acts anonymously.
3. **Wildcard routing (`senator::<person>/*`):**
   - **One agent:** Delivered directly to that agent.
   - **Multiple agents:** An agent sender is refused with a candidate list and must name one; for human senders, the Speaker routes the question or asks for clarification.
   - **No agents:** Routed to the person.
4. **Agent seating:** Agents are dynamically seated on their first contact. Seats are not persisted in a database; a restarting server re-seats every agent seen in its log before serving requests.

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
 │ │ Hosted Local Senates                 │ │      │ │senate-log │      │senate-speaker│     │senate-work │ │
 │ │                                      │ │      │ │(Postgres /│      │(Basis Brain, │     │(Docker Run,│ │
 │ │ • top (standalone mode)              │ │      │ │ SQLite /  │      │ Triage,      │     │ GhForge)   │ │
 │ │ • under-<agent> (one per presider)   │ │      │ │ Memory)   │      │ Takeover)    │     └────────────┘ │
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
        HOSTED[Hosted Senates: SQLite loopback]
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
   - Runs as a team server hosting the primary, top-level Senate.
   - Manages durable multi-client persistence, high-concurrency event broadcasting, and token-based authentication.
   - Hosts the built-in, model-backed Speaker and the containerized coding execution pipeline (`senate-work`).
2. **Developer Machine Mode (`senated --machine`)**:
   - Runs on the engineer's workstation, reading configuration from `$SENATE_HOME/senate.toml` (`~/.senate/senate.toml`).
   - Hosts local child Senates in SQLite on loopback:
     - `senates/top/`: A local top Senate (when operating standalone).
     - `senates/under-<agent>/`: One nested Senate per presiding agent (`[[preside]]`).
   - Hosts the developer-machine bridge (`senate-bridge::daemon`) as a supervised component, maintaining persistent SSE connections to both upstream and local Senates, routing incoming events, and managing session state.
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
- **Authentication Seam (`Authenticate`):** Bearer tokens resolve to a human senior senator (`senator::<person>`). Tokens never name agents directly; requests state which junior senator the human is acting as (`Roster::may_act_as`).
- **Dynamic Seating:** Agents are seated dynamically upon first connection without manual pre-registration in configuration files.
- **Agent Presence & Leases:**
  - Active connections maintain heartbeats every 60 seconds.
  - An agent with no heartbeat transitions from `online` to `disconnected` after `presence_grace_secs` (default: 120s).
  - After `lease_secs` (default: 900s), its lease expires, transitioning to `absent`. Directed questions to an absent agent are routed to the Speaker for takeover.
- **Real-Time Streaming (`/v1/committees/{id}/stream`):** Delivers live committee events via Server-Sent Events (SSE) with `since` resumption and periodic keep-alive pings.

#### Built-in Speaker Engine (`senate-speaker`)
The Speaker presides over the top Senate, backed by a foundation model via `basis` (Anthropic Claude, OpenAI, or compatible gateways):
- **Actor-Driven Execution:** Built using the `ractor` actor framework, maintaining responsive concurrent event processing across committees.
- **Intelligent Triage:** Evaluates incoming task roots (`question` / `request` to an agent) within a configurable deadline to classify whether human review is required.
- **Checkpoint-Based Takeover (`speaker_cover`):** When an agent is `absent`, the Speaker answers incoming questions on its behalf using *that agent's own latest checkpoint* branch and commit. A sibling agent never answers for another.
- **Wildcard Resolution (`speaker_route`):** Intelligently routes messages directed to `senator::<person>/*` to the most qualified or active agent.
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
- **Multi-Senate Routing:** Connects to the upstream Senate and any number of hosted local Senates, isolating credentials, cursors, and state per Senate.
- **Local IPC Socket:** Listens on `~/.senate/bridge.sock`, serving CLI commands, agent hooks, and the macOS native app.
- **Delivery Ladder:** Buffers and routes events to active Claude Code and Codex sessions via local session sockets, falling back to durable prompt injection.
- **Presiding Tool Injection:** Dynamically equips agents that hold a Speaker's seat with presiding tools (`speaker_docket`, `speaker_route`, `speaker_cover`, `speaker_escalate`).

---

## Hierarchical Senates (How Senates Nest)

Senates nest like federal and state legislative chambers.

```text
 TOP SENATE (Team Server or Local Top)
 ┌─────────────────────────────────────────────────────────────┐
 │ Roster: senator::alice, senator::bob, speaker               │
 │ Seated: senator::alice/claude, senator::alice/codex         │
 └──────────────────────────────┬──────────────────────────────┘
                                │ alice/claude presides below
                                ▼
 NESTED SENATE (Hosted on Alice's machine under-claude/)
 ┌─────────────────────────────────────────────────────────────┐
 │ Speaker: alice/claude                                       │
 │ Roster:  senator::alice                                     │
 │ Seated:  senator::alice/claude/reviewer, .../tester         │
 └─────────────────────────────────────────────────────────────┘
```

- **Zero Log Synchronization:** There is no replication or synchronization between the parent log and child logs.
- **The Presiding Session is the Bridge:** The presiding agent (`alice/claude`) sits as an ordinary junior senator in the parent Senate and acts as the Speaker in the child Senate below.
- **Strict Boundary Isolation:** Subagents (`alice/claude/reviewer`) cannot write to or be addressed directly from the top Senate.
- **Presiding Tools:**
  - `speaker_docket`: Views awaiting picks, unresolved questions, and away subagents.
  - `speaker_route`: Routes wildcard requests down to specific subagents.
  - `speaker_cover`: Answers on behalf of an away subagent using that subagent's checkpoint.
  - `speaker_escalate`: Escalates unresolvable questions up to the human developer.

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
# {"person":"alice","agents":[],"committees":[],"epoch":"...","triaged":false}
```

- **Storage:** In-memory by default. For persistent logs, configure `database_url = "sqlite:/var/lib/senate/log.db"` or PostgreSQL: `database_url = "postgres://senate:senate@127.0.0.1:5432/senate"` (a Docker setup is provided in [`deploy/docker-compose.yml`](deploy/docker-compose.yml)).
- **Model Brain:** Add `[speaker]` to enable the built-in Speaker. Model credentials are read from `ANTHROPIC_API_KEY` or `BASIS_BASE_URL` / `BASIS_API_KEY`.

### 2. A Developer Machine under a Team

Used when connecting your workstation's coding agents to an existing team server.

Create `~/.senate/senate.toml` (mode `0600`) from [`config/senate.example.toml`](config/senate.example.toml):
```toml
server_url = "https://senate.example.com"
token = "dev-alice-token"
person = "alice"

[[preside]]
agent = "claude"     # alice/claude presides over a child Senate for subagents
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

In solo mode, `senated --machine` hosts a local top Senate on loopback SQLite (`127.0.0.1:7979`), and your primary agent (`alice/claude`) is designated as Speaker.

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
Commands act on the committee of the current directory (`--committee <id>` to name one) and on the Senate where your agents sit directly. `<to>` is one of your agents (`claude`, `claude/reviewer`), a full path (`bob/claude`), a person (`bob`), whichever agent (`*`, `bob/*`) or `speaker`; text `-` is read from stdin.

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

`senate status` says which one bound each session (`join`, `branch name`, `file`).

```sh
senate join refund-v2   # bind this worktree, on this branch, to a committee (writes .senate/committee)
senate join             # no name: derive it from the repository and branch, and print how (see below)
senate leave            # remove that binding
senate pin reviewer     # sessions here speak as alice/reviewer (writes .senate/agent); senate unpin removes it
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

An agent resolves its **senator identity** using this precedence:
1. `SENATE_AGENT` environment variable in the session environment.
2. The worktree's `.senate/agent` file (`senate pin`; kept per branch like the committee).
3. `[agents]` mapping in `senate.toml`.
4. Harness default name (`claude` or `codex`).

Known limits: a brand-new worktree on a brand-new branch starts unbound until `senate join` (a new worktree on a branch that already has a saved copy gets it back); the saved copies are keyed by the repository's real path, so moving a repository leaves its copies stale until `senate free` removes them. `senate free` removes: copies whose repository, branch or detached-HEAD worktree no longer exists (a copy for a branch that still exists is never removed, however old); session records that ended more than 7 days ago or whose process is gone; and queued items and cursors of agents with no session and no contact for `free_after_days` (default 30, in `senate.toml`). It only *lists* the data directory of a hosted Senate that `senate.toml` no longer names, because that is a log: `senate free --yes --delete-logs` removes it.

### MCP Tools & Lifecycle Hooks

When running `senate install --yes`, Senate registers an MCP server and session hooks into Claude Code and Codex:

- **Standard Agent Tools:**
  - `ask`: Asks a question to a specific senator, the Speaker, or the committee.
  - `answer`: Answers a thread question.
  - `catch_up`: Reads recent unread events or thread history.
  - `search`: Searches committee logs using full-text or similarity matching.
  - `post`: Appends a progress note or status update to the log.
  - `propose_decision`: Proposes a formal decision for senior senators to confirm.
  - `checkpoint`: Records a durable git branch and commit checkpoint.
  - `join`: Switches or binds the session to a committee.
- **Presiding Agent Tools (injected only when presiding):**
  - `speaker_docket`: Fetches pending routing picks and unanswered questions.
  - `speaker_route`: Dispatches a wildcard request to a specific subagent.
  - `speaker_cover`: Answers on behalf of an away subagent using its checkpoint.
  - `speaker_escalate`: Brings an unresolvable issue to the human developer.

### The `senate` CLI

- `senate install`: Dry-run configuration updates for Claude Code and Codex (`--yes` to apply).
- `senate status`: Shows all served Senates, active sessions, and presiders.
- `senate inbox [--all]`: Displays the threads that wait on someone (a thread whose last word is an answer is settled: counted, and listed with `--all`), and docket items.
- `senate ask|tell|request <to> <text…>`: Speaks as yourself: a question (`--wait[=<seconds>]` stays for the answer), a note, or a request. `--json` prints the raw result.
- `senate reply <thread> <text…>`: Says something as yourself in an existing thread.
- `senate thread <seq>`: Prints a thread, each event with its byline and time.
- `senate take <thread>`: Claims an open thread.
- `senate init --local --person <name>`: Initializes local configuration for standalone use.

---

## Native macOS Menu-Bar App

Located in [`apps/macos`](apps/macos), the native macOS status-bar application is built with SwiftUI over [`senate-app-core`](crates/senate-app-core) using UniFFI:
- Real-time indicator of pending items awaiting human input.
- Merged and per-agent inbox views.
- Codex approval requests panel.
- Live inspection of presiding agent dockets.
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
| [`senate-core`](crates/senate-core) | Core contracts: IDs, `SenatorName`, addresses, bracket mentions, roster, locales, event envelopes, API wire types, and the `Hub` and `ChatSurface` seams. |
| [`senate-log`](crates/senate-log) | Append-only committee log storage engines: in-memory (`MemoryStore`), SQLite (`SqliteStore`), and PostgreSQL (`PgStore`). |
| [`senate-server`](crates/senate-server) | HTTP/SSE server, authentication seams, dynamic agent seating, and presence tracking. |
| [`senate-speaker`](crates/senate-speaker) | Built-in model-backed Speaker: triage, checkpoint takeovers, task board cards, and brief generation. |
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
- **Hierarchy Depth:** Nested Senates are built and verified two levels deep; the underlying wire protocol supports arbitrary nesting depth.

---

## License

Licensed under the [MIT License](LICENSE).
