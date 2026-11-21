//! The developer-machine side of Senate: joining a person's coding-agent
//! sessions to committee logs, across every Senate the machine serves. It
//! runs inside `senated --machine` (`senated::machine`), and is the `senate`
//! command (`install`, `status`, `inbox`, `take`, `join`, `leave`, `pin`,
//! `unpin`, `where`, `list`, `free`, `init --local`, `service` (keep the
//! daemon running), and the MCP server and hooks Claude Code and Codex
//! run). A worktree's `.senate/` follows its branch ([`follow`]).
//!
//! # One Senate, one daemon
//!
//! A machine serves one Senate ([`config`]: `server_url` and `token`, or
//! the one `senated --machine` hosts). Its agents are flat names in it
//! ([`agent`]); a pinned path (`claude/reviewer`) is not a name and is
//! ignored. A seat on another committee or in another Senate is reached by
//! a qualified name (`ibot::codex`), which the server resolves.
//!
//! # Presiding
//!
//! The session of an agent that holds a Senate's Speaker's seat presides
//! ([`preside`]): it sits in that Senate and is its Speaker too, is pushed
//! what is put to the Speaker, and gets
//! the presiding tools (`speaker_docket`, `speaker_route`, `speaker_cover`,
//! `speaker_escalate`). No other session sees them.
//!
//! # A machine is a person running agents
//!
//! The bridge authenticates as a **person** (`senate.toml`'s token) and seats
//! one junior senator per kind of coding agent it serves ([`agent`]): a
//! Claude Code session speaks as `<person>/claude`, a Codex session as
//! `<person>/codex`, unless `senate.toml` renames them or a session pins its
//! own name. Each agent has its own stream, cursors, heartbeat, checkpoints
//! and inbox; a message for one agent never reaches another agent's session.
//!
//! # Shape
//!
//! ```text
//!  Senate server ──SSE per committee──▶ daemon ──inbox socket──▶ Claude Code session
//!        ▲                              │  ▲
//!        └──── HTTPS appends ───────────┘  └── bridge.sock (IPC) ◀── hooks, MCP shim, status
//! ```
//!
//! - [`daemon`] is the one long-running process. It holds one SSE stream per
//!   (agent, joined committee), records a durable `delivered` cursor for each
//!   in `state.db`, routes each directed event to exactly one session of its
//!   agent ([`routing`]), batches it ([`mod@format`]) and hands it to the
//!   [`delivery`] ladder.
//! - [`hook`] and [`mcp`] are thin clients of the daemon over the local
//!   [`ipc`] socket. Hooks register sessions and pick up queued items; the MCP
//!   server gives the agent `ask` / `answer` / `catch_up` / `join` and the
//!   rest.
//! - [`install`] wires the hooks and the MCP server into Claude Code and
//!   Codex; [`status`] renders what the daemon is doing; [`init`] writes the
//!   profile for using Senate alone on one machine.
//!
//! Everything written for a person or an agent to read is in the locale of
//! `senate.toml` ([`senate_core::Locale`]); identifiers (addresses, kinds,
//! tool names, JSON fields) are the same in every language.
//!
//! Every path the bridge touches lives under [`paths::Paths`]
//! (`~/.senate`, or `$SENATE_HOME`).

pub mod agent;
pub mod approvals;
pub mod binding;
pub mod catch_up;
pub mod checkpoint;
pub mod cli;
pub mod client;
pub mod companion;
pub mod config;
pub mod daemon;
pub mod delivery;
pub mod doctor;
pub mod follow;
pub mod format;
pub mod hook;
pub mod host;
mod host_approval;
pub mod init;
pub mod install;
pub mod ipc;
pub mod joinname;
pub mod mcp;
pub mod notify;
pub mod paths;
pub mod person;
pub mod preside;
pub mod routing;
pub mod service;
pub mod session;
pub mod sessions;
pub mod speak;
pub mod state;
pub mod status;
mod stream;
pub mod terminal;
pub mod testenv;
pub mod wsl;

pub use config::BridgeConfig;
pub use daemon::{DaemonHandle, DaemonOptions, Timing};
pub use paths::Paths;
pub use session::{Session, SessionState};
