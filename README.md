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
| 🧩 **Extensible Assembly Library** | Embed `senated` directly into enterprise Rust services to mount custom chat platforms (Slack, Discord, …), custom authentication, and custom supervisors. |

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
 │ │  • Session Manager (Claude/Codex/agy)│ │      │                           │ in-process                 │
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
                                │ alice-desk::ledger::speaker
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
- A parent chat can address a child desk (`alice-desk::ledger::speaker`) or a named seat. The child marks a relay as “your user, relayed by the Speaker” only for the receiving agent’s own verified chat sender. A different sender’s request still goes to the agent’s person first. Simulated chat carries no user authority.
- When a child agent escalates or directs a question to a person in the relayed task, the parent Speaker shows it in the originating chat thread. A reply from that person returns as an answer in the child task. Hosted permission prompts still use their existing approval controls.

> [!TIP]
> For a comprehensive walkthrough of supervision trees, log monotonic sequencing, presence leases, and cross-committee boundary rules, see [docs/architecture.md](docs/architecture.md).

---

## 🚀 Quick Start

Requires **Rust 1.95+** (Rust Edition 2024).

### Install

```sh
cargo install senate senated
# Installs into ~/.cargo/bin:
#   senate   (developer CLI & bridge tool)
#   senated  (daemon: your machine's, or a team server)
```

`senate setup service install` copies the `senated` it finds next to `senate`
(and that `senate`) into `~/.senate/bin` and runs the copy, so install both the
same way. An upgrade overwrites the installed binaries in place, which macOS
refuses to start a service from (`OS_REASON_CODESIGNING`). The copy is
untouched, so the service keeps running the old version until you run
`senate setup service install --yes` again. `senate setup doctor` says when the
copy is behind. (`senate-bridge` installs the same `senate` binary
as the `senate` package; install one of them, not both.)

### Shell Completion

Add the line for your shell to its startup file. Setup prints the
registration script, so the installed CLI supplies the current commands and
flags each time you complete. Names are offered when the daemon answers
within 150 ms.

```sh
# zsh (~/.zshrc)
autoload -Uz compinit && compinit && source <(senate setup completions zsh)
# bash (~/.bashrc)
source <(senate setup completions bash)
# fish (~/.config/fish/config.fish)
senate setup completions fish | source
```

`senate setup install` prints this guidance and `senate setup doctor` looks for
the registration in your shell's startup files.

### Build from Source (Contributors)

```sh
cargo build --release -p senated -p senate-bridge
# Generated binaries:
#   target/release/senated
#   target/release/senate
```

Put `target/release` first on your `PATH` (or use the full paths) to run the
commands below against your build.

You can choose from three operational modes:

### Mode 1: Solo on One Machine (Zero Server Setup)

The fastest way to experience Senate locally. No central server or database required:

```sh
# 1. Initialize local configuration with you as the Speaker
senate setup init --local --person alice

# 2. Register MCP servers and hooks into Claude Code, Codex & agy
senate setup install --yes

# 3. Start the daemon (or install it as a persistent user service)
senated --machine

# Optional: run as a background service (launchd on macOS, systemd on Linux)
senate setup service install --yes
```

For Codex, `senate setup install --yes` records per-hook trust in
`config.toml` for the four lifecycle hooks that match Senate's installation,
including the executable path, event and timeout. Setup reports when it adds
trust. Codex reviews changes to a hook’s effective declaration and treats an
explicit default such as `async = false` as equivalent to omitting it.
If hook review blocks a hosted session, Senate alerts the chair so a person
can attach.

Now, navigate to any git repository and bind it:
```sh
cd ~/developer/my-project
senate join       # automatically names the committee from your repo & branch
senate status     # inspect the active Senate, committee, and agent sessions
senate spawn codex --alias review --attach # start Codex and open its terminal
```

### Mode 2: Connect Workstation to a Shared Team Server

Connect your local coding agents to an existing team Senate:

1. Configure `~/.senate/senate.toml` (mode `0600`) using [config/senate.example.toml](config/senate.example.toml):
   ```toml
   server_url = "https://senate.example.com"
   token = "dev-alice-token"
   person = "alice"

   # Optional: cap Senate-hosted sessions (spawn/restore) on this machine.
   # Absent: no limit; 0 disables them.
   # hosted_session_limit = 4

   [local]
   speaker = "/acme::senator::alice"
   secretaries = ["claude"]  # works your desk while you are away
   ```

2. Install integrations and launch the bridge daemon:
   ```sh
   senate setup install --yes
   senate setup service install --yes  # runs senated --machine in the background
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
   senated --config senated.toml
   ```

3. Verify server health:
   ```sh
   curl -H "Authorization: Bearer dev-alice-token" http://127.0.0.1:7878/v1/me
   ```

> [!TIP]
> For production deployments with persistent storage, point `database_url` to PostgreSQL (`database_url = "postgres://senate:senate@127.0.0.1:5432/senate"`). A turnkey setup is available in [deploy/docker-compose.yml](deploy/docker-compose.yml).

### Windows (WSL)

On Windows, Senate runs inside WSL 2 and works there as it does on Linux.
That includes hosted sessions (`senate spawn`, `attach`, `session watch`).
Native Windows is not supported yet; [docs/windows-plan.md](docs/windows-plan.md)
describes what it would take.

**Everything inside the distro.** Install the build tools, Rust, Senate,
Claude Code and Codex inside WSL (an Ubuntu distro is assumed below):

```sh
sudo apt-get install -y build-essential pkg-config git curl
curl -sSf https://sh.rustup.rs | sh
cargo install senate senated
curl -fsSL https://claude.ai/install.sh | bash          # Claude Code, Linux build
# Codex needs a Linux Node, for example through nvm:
nvm install --lts && npm install -g @openai/codex
```

- **Use the Linux agents, not the Windows ones.** WSL appends the Windows
  `PATH`, so `claude`, `codex`, `npm` or `node` can resolve to Windows
  programs under `/mnt/c` (npm wrappers that run Windows Node). Check with
  `command -v claude codex node`: each must be under `/home` or `/usr`. nvm
  sets itself up in `~/.bashrc`, which login shells do not read, so also add
  it to `~/.profile`. In WSL, `senate setup` leaves `/mnt/<drive>`
  directories out when it looks for programs and when it writes the
  service's `PATH`. `senate setup install` refuses when the only `claude`
  is a Windows one, and `setup service install` flags a configured agent
  under `/mnt/`.
- **Keep repositories on the WSL filesystem** (`~/src/...`), not under
  `/mnt/c`. Access across the Windows boundary is slow, and file watching
  and locking behave differently there.
- **Editors.** Run Claude Code and Codex in a WSL terminal (Windows Terminal
  → Ubuntu). Or open the folder with VS Code's WSL remote (`code .` from
  WSL, or "WSL: Open Folder in WSL"): its terminals and extensions then run
  inside the distro and use the Linux `claude`, `codex` and `senate`.
- **Logins carry over as files.** To reuse a Windows setup, copy
  `~/.claude/settings.json` and `~/.codex/{auth.json,config.toml}` from
  `/mnt/c/Users/<you>/` into the distro and `chmod 600` them. Drop the
  Windows-only parts: `projects`, `hooks` and `[windows]` in Codex's config.
  Then run `senate setup install --yes`.

- **Codex says "Reconnecting… waiting for network" while `curl` works.**
  Codex resolves host names itself, not through the system's resolver, and
  it gives up on a name when the DNS server answers the IPv6 (`AAAA`) query
  with REFUSED instead of an empty answer. `curl` and Node ignore that
  refusal and use the IPv4 address. Some company DNS servers do refuse
  `AAAA`. To check, query the server in `/etc/resolv.conf` for `AAAA`
  (`dig AAAA <gateway host>`) and look for `status: REFUSED`. The real fix
  is on the DNS server: it should answer NOERROR with no records. Until
  then, pin the gateway's IPv4 address in the distro's `/etc/hosts`, and
  stop WSL from rewriting that file at boot:
  ```sh
  printf '[network]\ngenerateHosts=false\n' | sudo tee -a /etc/wsl.conf
  echo "$(getent ahostsv4 <gateway host> | awk 'NR==1{print $1}') <gateway host>" | sudo tee -a /etc/hosts
  ```
  The pin goes stale if the gateway's address changes. Windows Codex is not
  affected, since it uses Windows' resolver.

- **Hosted Codex needs Codex's app-server daemon** (`codex app-server daemon
  start`, as `senate setup install` says). It is a plain background
  process, not a systemd unit, so it stops with the distro. Start it again
  after the distro restarts, or have Codex manage it with
  `codex app-server daemon bootstrap`.

Then set up Senate as in Mode 1. `senate setup init` and
`senate setup service install` notice WSL, and print the steps below.

**Keeping the daemon alive.** A systemd user unit runs the daemon. WSL,
though, stops a distro once no Windows process is attached to it, and the
daemon goes with it.

1. Inside the distro, turn on systemd and lingering, and install the
   service:
   ```sh
   printf '[boot]\nsystemd=true\n' | sudo tee -a /etc/wsl.conf   # then `wsl --shutdown` once, from Windows
   sudo loginctl enable-linger "$USER"
   senate setup service install --yes
   ```
   With lingering on, the daemon starts whenever the distro boots, before
   anyone opens a terminal. A restart of the daemon keeps hosted sessions
   running (`KillMode=process`).
2. On Windows, keep the distro running from logon with a per-user
   scheduled task that holds one `wsl.exe` open. Save the task below as
   `senate-wsl-keepalive.xml`, with your `DOMAIN\user` and distro name in
   it, then register it from PowerShell:
   `Register-ScheduledTask -TaskName 'Senate WSL keepalive' -Xml (Get-Content -Raw .\senate-wsl-keepalive.xml)`.
   `conhost --headless` keeps a console window from opening at logon.
   ```xml
   <?xml version="1.0" encoding="UTF-16"?>
   <Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
     <RegistrationInfo>
       <Description>Keeps the Ubuntu WSL distro, and the Senate daemon in it, running while this user is logged on.</Description>
     </RegistrationInfo>
     <Triggers>
       <LogonTrigger>
         <Enabled>true</Enabled>
         <UserId>DOMAIN\user</UserId>
       </LogonTrigger>
     </Triggers>
     <Principals>
       <Principal id="Author">
         <UserId>DOMAIN\user</UserId>
         <LogonType>InteractiveToken</LogonType>
         <RunLevel>LeastPrivilege</RunLevel>
       </Principal>
     </Principals>
     <Settings>
       <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
       <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
       <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
       <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
       <RestartOnFailure>
         <Interval>PT1M</Interval>
         <Count>999</Count>
       </RestartOnFailure>
       <IdleSettings>
         <StopOnIdleEnd>false</StopOnIdleEnd>
         <RestartOnIdle>false</RestartOnIdle>
       </IdleSettings>
       <Enabled>true</Enabled>
       <Hidden>true</Hidden>
     </Settings>
     <Actions Context="Author">
       <Exec>
         <Command>C:\Windows\System32\conhost.exe</Command>
         <Arguments>--headless C:\Windows\System32\wsl.exe -d Ubuntu --exec sleep infinity</Arguments>
       </Exec>
     </Actions>
   </Task>
   ```
   Stopping the task (`schtasks /End`) ends only `conhost`; the `wsl.exe`
   under it keeps running. To really stop the keepalive, run
   `wsl -d Ubuntu --exec pkill -fx "sleep infinity"`, or disable the task
   and log off.
3. **Docker Desktop.** With its WSL integration on for the distro, Docker
   Desktop keeps its own `wsl.exe` processes attached, so the distro stays
   up and the keepalive is not needed.

`vmIdleTimeout` in `.wslconfig` does not replace the keepalive. It only
delays shutting down the VM after the distros have stopped. Logging off
Windows ends your processes, `wsl.exe` included, so the daemon runs only
while you are logged on. At the next logon the task starts the distro, and
lingering starts the daemon.

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

Each command works on the committee this directory is bound to, or the one
named with `--committee`. Outside a bound worktree, `ask`, `tell` and
`request` to an alias that exists on exactly one committee go to that
committee, and say so on stderr (`on committee refund (where rev is an
alias)`); the commands they suggest then carry `--committee`. Otherwise the
error lists the committees to choose from.

On a terminal, `senate thread` and `senate inbox --follow` lay message bodies
out as Markdown, wrapped to the window (in colour unless `NO_COLOR` is set or
`TERM=dumb`), and print times in the local zone, naming it once
(`local times (UTC+01:00)`). Piped, they print the text exactly as written,
with every time in UTC.

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

# Offer sessions started here the alias reviewer (and give it to this one)
senate join --alias reviewer

# Inspect current binding, agent role, and branch context
senate where

# List all saved branch states across your machine
senate where --all

# Clean up stale session logs and deleted branch caches
senate setup clean --yes
```

> [!NOTE]
> **Branch Swapping Without Git Hooks:**
> Senate saves branch notes into `~/.senate/branches/<repo>/<branch>/`. When you run `git switch`, Senate automatically swaps `.senate/` to match the target branch at the next command or hook execution. Nothing is ever added to `.git` or committed to your repo!

> [!IMPORTANT]
> **Branch prefix default changed:** branches Senate reads and writes are named `<branch_prefix>/<committee>/<person>` (and `<branch_prefix>/<committee>/speaker-<n>` for work runs). The default `branch_prefix` is now `senate`; it used to be `feature`. If your branches follow `feature/<committee>/<person>`, set `branch_prefix = "feature"` in `senate.toml` and under `[speaker.work]` in `senated.toml` to keep the old names.

### 3. Session Aliases & TUI Approvals

Use a short role alias for each coding session. Hosted sessions without an
explicit alias get a name such as `claude1` or `codex2`. Older sessions without
an alias display the runtime and the first eight hex digits of their ID.
When an alias names different senators, Senate displays it as `committee::alias`.
Use `-v` to display complete IDs, which commands also accept.

```sh
# Inside a coding session (or via MCP join): name this session reviewer
senate senator alias reviewer

# Rename an existing senator by its current alias or full senator ID
senate alias reviewer docs
senate alias <senator-id> docs --committee refund-v2

# List active senators by committee (add --all for inactive senators)
senate senator list

# Remove one; nous::main names the alias on one committee
senate senator unalias nous::main

# Say what a session does on its committee
senate senator duty "reviews the server crate"

# Attach to an unattended session's interactive approval prompt
# (an alias on several committees lists them; name one as committee::alias):
senate attach reviewer
```

### 4. Hosted Sessions

Senate can run coding agents in terminals it owns, so they keep working when no
one has them open:

```sh
senate committees                      # chairs, active members and pending threads
senate status --all                     # include inactive entries
senate spawn claude --alias reviewer    # seat a member on this committee
senate spawn codex --chair --alias lead --attach # appoint the chair and open its terminal
senate spawn claude --dir ../ledger     # on the committee bound in ../ledger
senate spawn claude --secretary --alias desk # add a Speaker’s desk secretary
senate attach reviewer                 # restore or resume as needed (Ctrl-] detaches)
senate session list                    # active hosted sessions on this machine
senate session list --all               # include stopped and paused sessions
senate session watch                   # read-only grid of terminals without an interactive viewer
senate session watch --all             # include attached, paused and stopped sessions
senate session approve reviewer        # answer its permission prompt (--always, --deny)
senate session approve --once          # the one prompt waiting, whoever's it is
senate session stop reviewer           # end its terminal...
senate session restore reviewer        # ...and reopen it later (add --attach to enter)
senate session pause --all             # checkpoint, then stop each one...
senate session resume --all            # ...and bring them back where they were
senate session replace reviewer        # a fresh session takes the seat
```

`senate spawn` seats the session on `--committee` when given, else on the
committee bound in `--dir`, else on this directory's; when `--dir` binds
none, it says on stderr which committee it used.

A senator is active while present online or while its hosted terminal is
running. `senate committees` shows every committee, even without active
members. The pending column counts the open threads in your inbox for that
committee. Only the Speaker’s office can appoint a chair or desk secretary
through `senate spawn`.

`senate session pause` sends each session a Senate note asking it to finish its
current step and write a checkpoint. It waits up to 90 s for the checkpoint
(`--timeout` changes that), or until the session has been quiet for 10 s after
writing it. Then it stops the terminal and records the session as paused. A
paused seat shows as `paused` in `senate session list --all`, `senate status --all`, the
Speaker's `/status --all` and the app. Its presence stays `absent`, so the usual away
rules apply, and messages for it wait in the queue. A session someone typed
into in the last 5 minutes is skipped unless you pass `--force`. `--dry-run`
lists what would be paused. `senate session resume` restores each paused
session into the same conversation and points it at its last checkpoint. A
session you stopped on purpose is never resumed. In `senate.toml`,
`pause_on_shutdown = true` pauses every session when the service stops (90 s
at most), and `resume_on_start = true` resumes them when it starts again. Both
are off by default.

`senate session replace` starts a new session of the same runtime in the same
directory and committee. Once it is connected, it hands over the old seat's
alias, duty, place among the desk workers, the chair (if the old seat held it)
and its open questions and requests, then stops and unseats the old session.
It reports each step. If one fails, running the command again resumes from that
step.

`senate session approve` answers the permission prompt waiting in a hosted
terminal without attaching, as you would there: allow once (the default),
`--always` (and stop asking for the same thing this session) or `--deny`.
Without a name it answers the one session with a prompt waiting, and only
when the answer is given (`--once`, `--always` or `--deny`); it prints which
session and what the prompt asks about before answering, and lists them when
several wait.
Codex is answered through its app-server; for Claude Code the host checks that
its screen shows the permission dialog and types the chosen option. A choice
Senate only saw on the screen is not answered this way: attach to it. Each
answer is noted in the committee log. The macOS app's Allow / Always / Deny
buttons on a waiting prompt do the same. The daemon refuses the request from
inside a coding session's process tree, so an agent cannot approve its own
prompts or another's. That check walks the caller's ancestors and is defence
in depth: a process that detached from its agent escapes it.

### Where Each Command Lives

The top level is the daily loop: `status` (a bare `senate` runs it), `inbox`
(`--follow` to watch), `thread`, `ask`, `tell`, `request`, `reply`, `take`,
`join`, `leave`, `where`, `profiles`, `spawn` and `attach`. Everything else is in a group:

| Group | Commands |
| :--- | :--- |
| `senate session` | `list` · `watch` · `approve` · `stop` · `restore` · `pause` · `resume` · `replace` · `prune` |
| `senate senator` | `list` · `alias` · `unalias` · `seat` · `unseat` · `duty` |
| `senate secretary` | `list` · `appoint` · `dismiss` · `work list\|show\|create\|assign` (the secretary team) · `desk` · `desk set` (the Speaker's desk workers, in order) |
| `senate chat` | `send` · `thread` (simulated chat; see [Testing](#-testing--verification)) |
| `senate setup` | `init` · `install` · `completions <zsh\|bash\|fish>` · `service install\|uninstall\|status` · `doctor` · `clean` |

`spawn`, `session restore`, and `session replace` accept `--attach` (`-a`).
Without a name, `attach` offers running, paused, and stopped hosted sessions.
`attach --read-only` opens a running terminal for watching only. It never
opens a stopped terminal.

The names before this grouping (`sessions`, `watch`, `pin`, `install`, …) still
work for one more minor release and print the command that replaces them.

### Launch profiles

Save launch settings and a note about when to use them in the machine's
`senate.toml` (by default, `~/.senate/senate.toml`).

```toml
[profiles.review]
runtime = "codex"
model = "gpt-6.2-astra"
effort = "high"
note = "Use for independent diff review."
```

Run `senate profiles` to see the configured names, settings, and notes. Its
output is one tab-separated line per profile; with none it prints `No launch
profiles configured.` and, on stderr, an example to copy and the file to put
it in.
`senate setup doctor` reports profile configuration errors by name. Restart
the machine daemon after editing a profile so new launches pick up the change.

```bash
senate spawn --profile review --committee refund-v2
senate spawn codex --profile review --alias second-review --effort xhigh
```

`model` and `effort` are optional and use the runtime's defaults when omitted.
The CLI's explicit `--model` and `--effort` override profile values. A runtime
given alongside a profile must match it. Model aliases resolve through the
runtime's usual lookup at launch. If you omit `--alias`, Senate uses the profile
name and applies the usual roster alias rules.

In MCP, call `launch_profiles` to read the available profiles and their notes,
then pass the chosen name in the `profile` field of `spawn`. The same chair and
Speaker's office permissions apply to launches with a profile.

### Troubleshooting

When something does not arrive or a session sits idle, start with:

```bash
senate setup doctor          # check this machine; each problem with its fix
senate setup doctor --json   # the same, for scripts: {checks: [...], summary}
```

Each line is `ok`, `warn` or `fail`, what was found, and the command that
fixes it where one does. It exits non-zero when a check fails and changes
nothing. What it checks, and what usually fixes it:

| Check | Fails or warns when | Fix |
| :--- | :--- | :--- |
| `service.file`, `service.running` | the service file is not what `setup service install` writes now (an old `senated` path, an old `PATH`), or the service is not running (with the manager's last exit reason, e.g. `OS_REASON_CODESIGNING`) | `senate setup service install --yes` |
| `service.binary` | the service's copy in `~/.senate/bin` is older than the `senated` installed | `senate setup service install --yes` |
| `service.program.*` | the daemon cannot find `claude`, `codex` or `agy` on the service's `PATH`, a recorded path is gone, or it is a Windows program | `senate setup service install --yes` |
| `install.*` | Claude Code, Codex or agy is not registered the way `setup install` would register it now (another `senate` path, hooks without `--runtime`) | `senate setup install --yes` |
| `install.codex.approval`, `install.agy.grant` | Codex lacks `default_tools_approval_mode = "approve"`, or agy lacks `mcp(senate/*)`: a hosted session blocks on its first Senate tool call | `senate setup install --yes` |
| `leftovers.*` | another bridge (e.g. `acme-bridge`) or an old Senate still has hooks or an MCP server registered | `claude mcp remove --scope user <name>`, `codex mcp remove <name>`, or remove the hook from the file named |
| `daemon`, `daemon.version` | the daemon is down, or runs another version than this `senate` | `senate setup service install --yes` (restarts it) |
| `codex.app_server` | the Codex app-server does not answer: hosted Codex sessions cannot start or restore | `codex app-server daemon bootstrap` |
| `sessions.git.*` | a hosted Codex session's worktree keeps its git data (the common `.git`) outside its directory, where Codex's `workspace-write` sandbox cannot write: every git write asks first (Claude Code's sandbox already writes a worktree's shared `.git`) | add the path to `[sandbox_workspace_write] writable_roots`, as shown |
| `sessions.*` | hosted terminals run with no session (`senate session prune`), or a session waits on a permission prompt, or on a choice its screen has shown unchanged for 15 seconds that no hook or app-server reported (`senate attach <name>`) | as shown |
| `uplink.*` | the Senate above does not answer, refused the registration, or runs a newer minor release | `cargo install senate senated` for a newer parent |
| `senate.*` | the Senate served does not answer, the daemon's log epoch differs from the server's, or a cursor is ahead of the log | `senate setup service install --yes` (a restart resets them) |
| `bindings.*` | saved worktree bindings expired (`senate setup clean --yes`), or name a committee with no messages yet | as shown |
| `wsl.*` | systemd is off, linger is off, or the service's `PATH` has `/mnt/<drive>` directories | as shown |
| `model` (server doctor) | a tier fails its self-check, the measured triage deadline is too short, or no model is configured | check the named tier’s endpoint, key and tool support, or set the suggested `triage_deadline_secs`. With no model, fixed syntax still works. |

A server has its own: `senated doctor --config senated.toml [--json]` probes the Speaker’s configured models (use `--no-model` to skip live provider calls) and checks
the configuration, the log (read only: never created or migrated), whether
the server answers on its `listen` address, and with `[chat_gateway]`
whether the socket is served, whether a platform adapter is attached, and
whether any bound group is stuck: one the platform refused to join, or one
with no real message to join it by (the server writes `chat_gateway.json`
in its `data_dir` on every attach and detach and whenever that changes).

---

## 🤖 Agent Ecosystem & MCP

Senate integrates seamlessly with autonomous coding assistants through the **Model Context Protocol (MCP)** and native process lifecycle hooks.

### Tool Catalog

Running `senate setup install --yes` registers these tools into your agent runtimes:

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

`senate setup install` sets up the **Antigravity CLI (agy)** when `agy` is on
your `PATH` (`--agy` forces it, `--no-agy` skips it). Like the other
runtimes, the dry run shows each change and `--yes` writes them into agy's
own global files under `~/.gemini` (`--gemini-dir` for another):

| File | What Senate adds |
| :--- | :--- |
| `config/mcp_config.json` | The `senate` MCP server: `senate mcp --runtime agy` |
| `config/hooks.json` | A named hook `senate`: `SessionStart`, `PreInvocation` (before each model call) and `Stop` (end of a turn) run `senate hook … --runtime agy` |
| `antigravity-cli/settings.json` | `mcp(senate/*)` in `permissions.allow`, so Senate's own tools run without asking |

An agy session joins as `[senator::agy-<conversation-id>]`. It takes what
waits for it at its hooks: the brief and its messages arrive as a user
message before its next model call, and a turn that ends while a message is
waiting goes on to read it. agy offers no channel Senate can push into, so a
message that lands while an agy session you started yourself sits idle waits
for its next prompt (`senate status` shows its delivery as `at hooks`). A
hosted agy (below) does not wait: when something arrives for it while it is
idle, its host types Senate's own line `[senate] new messages` and Enter into
its terminal, and that prompt's hook hands the messages over. It types that
line once per idle spell, never anything a peer wrote, and not while someone
has typed in the terminal in the last 10 seconds (it waits for them; a
read-only watcher does not count). Nor while a line typed in its prompt is
not yet submitted: the nudge never adds to it, clears it or sends it.
Pressing Enter sends that prompt, whose hook hands the messages over too;
clearing the line (Ctrl-U, Ctrl-C) lets the nudge come.

Host one like any other runtime:

```sh
senate spawn agy --alias triage --prompt 'Triage the failing tests'
senate attach triage
senate session restore triage      # resumes the same agy conversation
```

A hosted agy starts from Senate's fixed seed prompt, which tells it to read
its Senate inbox; `--prompt` arrives as an ordinary Senate request.
Senate resolves `--model` against `agy models` for agy and the app-server's
`model/list` for Codex. An exact ID wins. A family name such as `astra` selects
its newest numeric version. If several families match, choose a listed ID.
Restore keeps the recorded choice unless you supply a new `--model`.
Claude Code handles its own aliases. agy accepts `--effort` from `low` to `max`. `[agy] bin` in
`senate.toml` picks the executable. A hosted agy starts with session-only
settings (`--settings`, never written to agy's own files) that make its spawn
directory a trusted workspace, so it does not stop at agy's trust question.
agy takes that list in place of its global `trustedWorkspaces`, so inside a
hosted agy only its spawn directory is trusted and other workspaces ask as
usual. agy matches trust paths exactly, by real path and case-sensitively
(`/private/tmp/x`, not `/tmp/x`), so Senate gives it the real path.

To run an agent over the **Agent Client Protocol (ACP)** instead, with
Senate as the client (agy's own ACP server, for one):
```sh
senate acp -- agy acp
```
Type one-time confirmation or `/deny` to grant/block permission requests interactively.

---

## 📖 Core Concepts & Addressing

### Hierarchy

```text
 Senate (/acme/north-desk)
  └── Committee (ledger)
       └── Senator (wendell, claude, codex)
            └── Seat (ledger::senator::codex)
```

| Concept | Scope | Description | Example |
| :--- | :--- | :--- | :--- |
| **Senate** | Nested | Top-level deliberation instance. Senates nest hierarchically. | `/acme/north-desk` |
| **Committee** | One Senate | Scoped working group sharing one append-only event log. | `ledger`, `refund-v2` |
| **Senator** | One Senate | Participant identity (principal person or automated agent). | `wendell`, `claude` |
| **Seat** | One Committee | Unique combination of Committee × Senator. | `ledger::senator::codex` |

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
    
    // Mount custom chat surface (Slack, Discord, …)
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
| [`senate-core`](crates/senate-core) | `0.10.0` | Core domain types: identifiers, addressing, event envelopes, roster models, and `Hub` seams. |
| [`senate-log`](crates/senate-log) | `0.10.0` | Append-only log engine supporting `MemoryStore`, `SqliteStore`, and `PgStore` (PostgreSQL). |
| [`senate-server`](crates/senate-server) | `0.10.0` | Axum-powered HTTP API, Server-Sent Events (SSE) streaming, authentication, and presence leases. |
| [`senate-speaker`](crates/senate-speaker) | `0.10.0` | Built-in model session engine (actor-based via `ractor`), triage, cover generation, and task synthesis. |
| [`senate-work`](crates/senate-work) | `0.10.0` | Containerized autonomous worker (`DockerRunner`) and GitHub Pull Request automation (`GhForge`). |
| [`senate-bridge`](crates/senate-bridge) | `0.10.0` | Developer workstation bridge daemon, local IPC socket, MCP server, hooks, and the `senate` CLI. |
| [`senated`](crates/senated) | `0.10.0` | Universal daemon executable (`--config`, `--machine`) and extensible assembly library (`Assembly`). |
| [`senate-app-core`](crates/senate-app-core) | `0.10.0` | UniFFI Rust bridge backing the native SwiftUI macOS companion app. |
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

### Simulated Chat

A chat flow can be a script instead of clicks in the chat platform. `senate
chat send` injects a message into a group bound to a committee, as if you had
written it there. It enters at the chat gateway, where the platform's messages
enter, so the Speaker interprets it, relays it and answers it exactly as it
would a real message. `senate chat thread` reads the conversation back,
including what the Speaker posted.

```sh
# Ask through the Speaker; prints the message's ref (sim:…)
ref=$(senate chat send --committee refund-v2 "ask bob's agent how long refunds take")

# Wait for the Speaker's acknowledgement, then for the next thing it posts
senate chat thread "$ref" --wait
senate chat thread "$ref" --wait --since 2 --json | jq -r '.entries[-1].text'

# Answer in the same thread (a confirmation, a correction)
senate chat send --committee refund-v2 --in "$ref" "yes, record that"

# Script two people agreeing, to see the Speaker propose a decision
ref=$(senate chat send --committee refund-v2 --ambient --as alice "cap refund retries at three?")
senate chat send --committee refund-v2 --ambient --as bob --in "$ref" "agreed, three"
senate chat thread "$ref" --wait   # "Record this as a decision?"
```

- **Log only by default.** Whatever the Speaker posts for a simulated message
  is recorded in the conversation and never reaches the platform. That
  covers replies under it, its relay's task card in the Agent group, and
  everything under those. With `--live`, the message is posted into the real
  group, marked *(simulated by you)*, and the answers go there as well. The
  group must actually be bound.
- **Marked.** The `chat` event is from the person you sent it as, as a real
  message from them would be. Its `source` is marked `simulated`, with
  `simulated_by` naming you. The Speaker carries a chat's source onto the
  relay, task, decision or proposal it makes from it, so those are marked
  too. Agents, `senate thread` and `senate chat thread` see *(simulated by
  you)*, and the brief and the "today" card leave simulated work out.
  Simulated discussion is checked for agreement on its own, never mixed with
  real messages: a proposal from it stays in the conversation, and its topic
  never blocks a real one. What happens in the log is
  real: a simulated `/bind` binds, and a simulated decision is recorded.
- **Who may.** Any person (their own token) may simulate any person on the
  roster, themselves included (`--as alice`). The message always shows who
  simulated it. The Speaker's office may too (`--office --as alice`). The
  server refuses a caller that is an agent (the command run inside a coding
  session). It never simulates a message from a guest or an unknown name: a
  person with no account on the platform is refused.
- `--group` names the bound group (default: the committee's only people
  group). `--ambient` sends plain chat that does not mention the Speaker.
  Conversations are held in the daemon's memory until `senated` restarts.

End-to-end tests drive the same path in-process: see
[`crates/senate-e2e/tests/chat_sim.rs`](crates/senate-e2e/tests/chat_sim.rs).

---

## 📄 License

Senate is open source and distributed under the [MIT License](https://opensource.org/licenses/MIT).
