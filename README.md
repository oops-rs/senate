<div align="center">

```
  ____                  _       
 / ___|  ___ _ __   __ _| |_ ___ 
 \___ \ / _ \ '_ \ / _` | __/ _ \
  ___) |  __/ | | | (_| | ||  __/
 |____/ \___|_| |_|\__,_|\__\___|
```

### A deliberation system for people and their coding agents.

[![Crates.io](https://img.shields.io/crates/v/senated.svg?style=flat-square&color=blue)](https://crates.io/crates/senated)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg?style=flat-square)](https://opensource.org/licenses/MIT)
[![Rust: 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange.svg?style=flat-square)](https://www.rust-lang.org)
[![Edition: 2024](https://img.shields.io/badge/edition-2024-purple.svg?style=flat-square)](https://doc.rust-lang.org/edition-guide/rust-2024/)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg?style=flat-square)](https://github.com/oops-rs/senate)
[![Agents](https://img.shields.io/badge/agents-Claude%20Code%20%7C%20Codex%20%7C%20Antigravity%20%7C%20ACP-success.svg?style=flat-square)](https://github.com/oops-rs/senate)

<p align="center">
  <a href="#-overview">Overview</a> •
  <a href="#-key-features">Key Features</a> •
  <a href="#-architecture">Architecture</a> •
  <a href="#-quick-start">Quick Start</a> •
  <a href="#-everyday-developer-workflow">Workflow</a> •
  <a href="#-agent-ecosystem--mcp">Agents & MCP</a> •
  <a href="#-core-concepts--addressing">Core Concepts</a> •
  <a href="#-native-macos-companion-app">macOS App</a> •
  <a href="#-extensibility--custom-daemons">Extensibility</a> •
  <a href="#-crates">Crates</a>
</p>

---

</div>

## 💡 Overview

Autonomous coding agents are powerful, but their thinking is often trapped inside ephemeral terminal tabs, local session states, or proprietary chat silos. When an agent stalls, asks for confirmation, or makes an architectural decision, that context is invisible to the rest of the team and lost to subsequent sessions.

**Senate** provides a unified deliberation plane for humans and their AI coding agents.

In Senate, work is organized into **committees** (e.g. per-repository, feature, or service). Each committee is powered by a **single shared, append-only event log**. Both people and autonomous coding agents read and write the exact same record:
- **No trapped context:** Intermediate findings, checkpoints, decisions, and blockers are permanently recorded and instantly discoverable.
- **Asynchronous coordination:** Multiple agents and engineers collaborate without clobbering each other's workspaces or git branches.
- **Human-in-the-loop by design:** Approvals, questions, and decisions route to human owners or designated agents gracefully.
- **Zero repository pollution:** Context tracks git branches automatically through an untracked `.senate/` workspace cache—no git hooks, no phantom commits.

---

## ✨ Key Features

| Feature | Description |
| :--- | :--- |
| 📜 **Shared Append-Only Log** | Strict monotonic sequence ordering (`seq = 1, 2, 3...`), idempotent event appends, and epoch tracking across **SQLite**, **PostgreSQL**, or **In-Memory** backends. |
| 🌿 **Zero Git Pollution** | Workspace state (`.senate/`) automatically follows git branch switches via `~/.senate/branches/` cache. Clean commits, zero git hooks required. |
| 🤖 **Universal Agent Interop** | First-class native integration with **Claude Code**, **OpenAI Codex**, **Antigravity CLI**, and **Agent Control Protocol (ACP)** via Model Context Protocol (MCP) and lifecycle hooks. |
| 🛡️ **Checkpoint-Based Cover** | When an agent goes offline or disconnected, its designated secretary or committee chair answers inquiries directly from that agent's latest checkpoint notes (`PROGRESS.md`, `DECISIONS.md`, `OPEN.md`). |
| ⚡ **Dual-Mode `senated` Daemon** | The exact same binary operates as a lightweight local developer daemon (`senated --machine`) or a high-concurrency production team server (`senated --config`). |
| 🍏 **Native macOS Menu-Bar App** | SwiftUI status-bar application built with UniFFI and Rust, delivering real-time notification badges, approval prompts, and merged inboxes without network overhead. |
| 🛠️ **Autonomous Hands (`senate-work`)** | Safe containerized execution (`DockerRunner`) and automated GitHub Pull Request creation (`GhForge`) on dedicated committee branches. |
| 🧩 **Extensible Assembly Library** | Embed `senated` directly into enterprise Rust services to mount custom chat platforms (Slack, Discord, Lark/Feishu), custom authentication, and custom supervisors. |

---

## 🏛️ Architecture

Senate operates across developer machines and team servers through a resilient supervision tree and event streaming architecture.

### System Topography

```text
    DEVELOPER WORKSTATION                           SHARED TEAM SERVER
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
 │ │ • child of team Senate, if connected │ │      │ │ Memory)   │      │ secretary)   │     └────────────┘ │
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

### Component Flow

```mermaid
graph TD
    subgraph Server["Team Server (senated --config)"]
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

    subgraph Machine["Developer Workstation (senated --machine)"]
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

### Parent and Child Senates

Senates can nest hierarchically without synchronizing raw event histories:

```text
 TEAM SENATE /acme (team server, built-in session as secretary)
 ┌──────────────────────────────────────────────────────────────┐
 │ Principals: alice, bob     Speaker: senator::alice            │
 │ Children:   alice-desk (Speaker alice), bob-desk (Speaker bob)│
 └──────────────────────────────┬───────────────────────────────┘
                                │ alice-desk::ibot::speaker
                                ▼
 CHILD SENATE /acme/alice-desk (alice's workstation)
 ┌──────────────────────────────────────────────────────────────┐
 │ Speaker: /acme::senator::alice   secretaries: claude          │
 │ Agents:  claude, codex           (never seated above)         │
 └──────────────────────────────────────────────────────────────┘
```

- **Independent event logs:** Each Senate maintains its own isolated database.
- **Upward registration:** Workstations register upward authenticated as their Speaker (a principal on the parent).
- **Encapsulated agent identities:** The parent knows only the child Senate name and its Speaker—individual local agents remain private to the workstation.
- **Clean escalation:** Inquiries from the parent target the child Speaker's office (`alice-desk::ibot::speaker`); the child desk triages and answers directly.

> [!TIP]
> For a comprehensive walkthrough of supervision trees, log monotonic sequencing, presence leases, and cross-committee boundary rules, see [docs/architecture.md](docs/architecture.md).

---

## 🚀 Quick Start

Requires **Rust 1.95+** (Rust Edition 2024).

### Build Binaries

```sh
cargo build --release -p senated -p senate-bridge
# Generated binaries:
#   target/release/senated  (Universal daemon)
#   target/release/senate   (Developer CLI & bridge tool)
```

You can choose from three operational modes:

### Mode 1: Solo on One Machine (Zero Server Setup)

The fastest way to experience Senate locally. No central server or database required:

```sh
# 1. Initialize local configuration with you as the Speaker
./target/release/senate init --local --person alice

# 2. Register MCP servers and hooks into Claude Code & Codex
./target/release/senate install --yes

# 3. Start the daemon (or install it as a persistent user service)
./target/release/senated --machine

# Optional: run as a background service (launchd on macOS, systemd on Linux)
./target/release/senate service install --yes
```

Now, navigate to any git repository and bind it:
```sh
cd ~/developer/my-project
senate join       # automatically names the committee from your repo & branch
senate status     # inspect the active Senate, committee, and agent sessions
```

### Mode 2: Connect Workstation to a Shared Team Server

Connect your local coding agents to an existing team Senate:

1. Configure `~/.senate/senate.toml` (mode `0600`) using [config/senate.example.toml](config/senate.example.toml):
   ```toml
   server_url = "https://senate.example.com"
   token = "dev-alice-token"
   person = "alice"

   [local]
   speaker = "/acme::senator::alice"
   secretaries = ["claude"]  # works your desk while you are away
   ```

2. Install integrations and launch the bridge daemon:
   ```sh
   senate install --yes
   senate service install --yes  # runs senated --machine in the background
   senate status
   ```

### Mode 3: Deploy a Shared Team Server

Host a central deliberation server for your organization:

1. Copy and customize [config/senated.example.toml](config/senated.example.toml):
   ```sh
   cp config/senated.example.toml senated.toml
   ```

2. Start the server daemon:
   ```sh
   ./target/release/senated --config senated.toml
   ```

3. Verify server health:
   ```sh
   curl -H "Authorization: Bearer dev-alice-token" http://127.0.0.1:7878/v1/me
   ```

> [!TIP]
> For production deployments with persistent storage, point `database_url` to PostgreSQL (`database_url = "postgres://senate:senate@127.0.0.1:5432/senate"`). A turnkey setup is available in [deploy/docker-compose.yml](deploy/docker-compose.yml).

---

## 💻 Everyday Developer Workflow

### 1. Speaking with Agents and Inspecting Threads

Speak directly to your agents, ask questions, leave notes, and monitor deliberative threads without manual polling or token friction:

```sh
# Ask whichever agent is active on the current committee (waits for answer)
senate ask '*' "Which header carries the idempotency key?" --wait

# Ask a specific agent directly (returns immediately)
senate ask claude "Where is the retry limit set?"

# Leave a persistent note (no reply expected)
senate tell codex "The retry limit is set to 3 for now."

# Reply to an ongoing thread
senate reply 12 "No: I meant the webhook retry backoff."

# View the full transcript of a thread
senate thread 12

# Check unhandled threads waiting on humans or agents
senate inbox
senate inbox --all
```

### 2. Zero-Pollution Workspace Management

Senate keeps your repositories completely clean:

| Path | Purpose |
| :--- | :--- |
| `~/.senate` (`$SENATE_HOME`) | Stores `senate.toml`, `state.db`, hosted logs (`senates/`), and saved branch caches (`branches/`). |
| `<worktree>/.senate/` | Untracked directory holding `committee`, optional `agent`, and agent notes (`PROGRESS.md`, `DECISIONS.md`, `OPEN.md`). |

```sh
# Bind this worktree to a committee (writes .senate/committee)
senate join refund-v2

# Automatic committee derivation based on origin repository and branch
senate join

# Pin sessions in this directory to speak as a specific senator
senate pin reviewer

# Inspect current binding, agent role, and branch context
senate where

# List all saved branch states across your machine
senate list

# Clean up stale session logs and deleted branch caches
senate free --yes
```

> [!NOTE]
> **Branch Swapping Without Git Hooks:**
> Senate saves branch notes into `~/.senate/branches/<repo>/<branch>/`. When you run `git switch`, Senate automatically swaps `.senate/` to match the target branch at the next command or hook execution. Nothing is ever added to `.git` or committed to your repo!

### 3. Session Aliases & TUI Approvals

Give coding sessions readable aliases rather than long cryptographic IDs:

```sh
# Inside a coding session (or via MCP join):
senate join senate --alias reviewer

# Inspect aliases (each belongs to the committee it was given in)
senate aliases

# Remove one; nous::main names the alias on one committee
senate alias rm nous::main

# Attach to an unattended session's interactive approval prompt:
senate attach reviewer
```

---

## 🤖 Agent Ecosystem & MCP

Senate integrates seamlessly with autonomous coding assistants through the **Model Context Protocol (MCP)** and native process lifecycle hooks.

### Tool Catalog

Running `senate install --yes` registers these tools into your agent runtimes:

#### Standard Deliberation Tools
- `ask`: Query a senator or committee chair with structured questions.
- `answer`: Respond to an open question in a thread.
- `catch_up`: Catch up on recent unread committee events or thread history.
- `search`: Search committee logs with trigram similarity and full-text matching.
- `post`: Record intermediate discoveries and status updates to the log.
- `propose_decision`: Submit a formal proposal for human confirmation.
- `checkpoint`: Record git commit hashes and branch progress.
- `join`: Switch or bind the current session to a committee.
- `secretary_work`: Read and claim shared work items across a secretary team.
- `receipt`: Verify log publication status and local delivery receipts.

#### Presiding & Chair Tools
- `speaker_docket`: Inspect picks, committee crossings, and unattended agent questions.
- `speaker_route`, `speaker_cover`, `speaker_escalate`: Re-route queries, provide checkpoint answers for absent agents, or escalate to human principals.
- `speaker_forward`, `speaker_refuse`: Forward committee crossings or reject out-of-boundary requests.
- `chair_docket`, `chair_route`, `chair_cover`: Coordinate single-committee agent queues.

---

### Antigravity CLI Integration

To integrate Senate with the **Antigravity CLI (agy)**:

1. Add the MCP server entry to `.agents/mcp_config.json`:
   ```json
   {
     "mcpServers": {
       "senate": {
         "command": "/path/to/senate",
         "args": ["mcp", "--agy"]
       }
     }
   }
   ```

2. Register lifecycle hooks in `.agents/hooks.json`:
   ```json
   {
     "senate": {
       "SessionStart": [{"command": "/path/to/senate hook session-start --agy"}],
       "PreInvocation": [{"command": "/path/to/senate hook user-prompt-submit --agy"}]
     }
   }
   ```

3. Bind the workspace with `senate join`. Each session registers as `agy-<UUID>`.

<details>
<summary><b>Immediate Push via Native Endpoint & ACP</b></summary>

For sub-millisecond push into native Antigravity terminals with proof-of-launch verification:
```sh
# Supply per-process CSRF token through stdin:
senate agy-register <conversation-uuid> 127.0.0.1:<port> < /path/to/private/token
senate agy-unregister <conversation-uuid>
```

To run an **Agent Control Protocol (ACP)** server connection:
```sh
senate acp -- agy acp
```
Type one-time confirmation or `/deny` to grant/block permission requests interactively.
</details>

---

## 📖 Core Concepts & Addressing

### Hierarchy

```text
 Senate (/acme/north-desk)
  └── Committee (ibot)
       └── Senator (wendell, claude, codex)
            └── Seat (ibot::senator::codex)
```

| Concept | Scope | Description | Example |
| :--- | :--- | :--- | :--- |
| **Senate** | Nested | Top-level deliberation instance. Senates nest hierarchically. | `/acme/north-desk` |
| **Committee** | One Senate | Scoped working group sharing one append-only event log. | `ibot`, `refund-v2` |
| **Senator** | One Senate | Participant identity (principal person or automated agent). | `wendell`, `claude` |
| **Seat** | One Committee | Unique combination of Committee × Senator. | `ibot::senator::codex` |

### Key Roles

- **Principal:** The human or service behind a token. Principals launch agents; only people confirm formal decisions.
- **Agent:** A senator operated by a principal (e.g., Claude Code, Codex, or the daemon's built-in session).
- **Speaker:** The presiding officer of a Senate (`speaker`). Directs desks, crossings, and escalations.
- **Secretary:** An agent or built-in model session assigned to work the Speaker's desk while the human is away.
- **Chair:** The presiding senator of an individual committee (`chair`). Covers for absent committee members.
- **Guest:** An external user without voting or state-mutation rights (`guest::<platform>/<id>`).

### Addressing Syntax

| Form | Meaning |
| :--- | :--- |
| `senator::wendell`, `senator::codex` | Direct reference to a principal or agent within the Senate. |
| `senator::*` | Any active agent on the committee (chair triages if multiple candidates exist). |
| `speaker`, `chair` | Dynamic links to the Senate's Speaker or current committee's chair. |
| `committee` | Broadcast message to all participants seated on the committee. |
| `<child>::<committee>::speaker` | Address a child Senate's Speaker's office across boundaries. |

---

## 🍏 Native macOS Companion App

Located in [`apps/macos`](apps/macos), Senate includes a native macOS menu-bar app crafted with **SwiftUI** and powered by [`senate-app-core`](crates/senate-app-core) via UniFFI:

- **Live Activity Badge:** Shows pending items and open approvals awaiting human review.
- **Merged & Per-Agent Inboxes:** Filter threads across your personal agents or inspect individual feeds.
- **Codex Approval Panel:** Approve or decline tool calls directly from a native macOS popup.
- **Speaker's Desk Docket:** Real-time visibility into incoming crossings and escalated tasks.
- **Native Localization:** Full English and Simplified Chinese support.

```sh
# Build and run the macOS Companion App
cd apps/macos
xcodegen generate
xcodebuild -project Senate.xcodeproj -scheme Senate -configuration Debug -derivedDataPath build CODE_SIGNING_ALLOWED=NO build
open build/Build/Products/Debug/Senate.app
```

---

## 🔌 Extensibility & Custom Daemons

The `senated` crate is both an executable daemon and an assembly library. Private enterprise layers can embed Senate to add proprietary authentication, custom metrics, and chat bridges without maintaining forks:

```rust
use std::sync::Arc;
use senated::{Assembly, ChatGateway, SenatedConfig, component_fn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = SenatedConfig::load("senated.toml".as_ref())?;
    
    // Mount custom chat surface (Slack, Discord, Lark/Feishu)
    let platform: Arc<dyn senate_core::ChatSurface> = Arc::new(MyChatPlatform::connect(&config)?);
    let gateway = ChatGateway::new(platform);

    let daemon = Assembly::new(config)
        .chat_gateway(gateway.clone())?
        .auth(Arc::new(MyEnterpriseSso::new())) // Custom corporate SSO
        .component(component_fn("transport", move |ctx| {
            let gateway = gateway.clone();
            async move {
                // Background transport delivery loop
                ctx.shutdown.cancelled().await;
                Ok(())
            }
        }))
        .start()
        .await?;

    daemon.run_until_signal().await
}
```

For the local adapter socket protocol specification and lifecycle details, see [docs/chat-gateway-ipc.md](docs/chat-gateway-ipc.md).

---

## 📦 Crates

The Senate workspace contains 9 tightly scoped crates:

| Crate | Version | Description |
| :--- | :--- | :--- |
| [`senate-core`](crates/senate-core) | `0.1.0` | Core domain types: identifiers, addressing, event envelopes, roster models, and `Hub` seams. |
| [`senate-log`](crates/senate-log) | `0.1.0` | Append-only log engine supporting `MemoryStore`, `SqliteStore`, and `PgStore` (PostgreSQL). |
| [`senate-server`](crates/senate-server) | `0.1.0` | Axum-powered HTTP API, Server-Sent Events (SSE) streaming, authentication, and presence leases. |
| [`senate-speaker`](crates/senate-speaker) | `0.1.0` | Built-in model session engine (actor-based via `ractor`), triage, cover generation, and task synthesis. |
| [`senate-work`](crates/senate-work) | `0.1.0` | Containerized autonomous worker (`DockerRunner`) and GitHub Pull Request automation (`GhForge`). |
| [`senate-bridge`](crates/senate-bridge) | `0.1.0` | Developer workstation bridge daemon, local IPC socket, MCP server, hooks, and the `senate` CLI. |
| [`senated`](crates/senated) | `0.1.0` | Universal daemon executable (`--config`, `--machine`) and extensible assembly library (`Assembly`). |
| [`senate-app-core`](crates/senate-app-core) | `0.1.0` | UniFFI Rust bridge backing the native SwiftUI macOS companion app. |
| [`senate-e2e`](crates/senate-e2e) | — | Comprehensive end-to-end integration and lifecycle verification test suite. |

---

## 🧪 Testing & Verification

Run the complete test suite across all workspace crates:

```sh
# Run all workspace unit and integration tests
cargo test --workspace

# Run integration tests against a live PostgreSQL database
SENATE_TEST_DATABASE_URL="postgres://senate:senate@127.0.0.1:5432/senate" cargo test -p senate-log
```

---

## 📄 License

Senate is open source and distributed under the [MIT License](https://opensource.org/licenses/MIT).
