//! The developer-machine side of Senate: joining a person's coding-agent
//! sessions to committee logs, across every Senate the machine serves. It
//! runs inside `senated --machine` (`senated::machine`), and is the `senate`
//! command (`install`, `status`, `inbox`, `take`, `join`, `leave`, `pin`,
//! `unpin`, `where`, `list`, `free`, `init --local`, `service` (keep the
//! daemon running), and the MCP server and hooks Claude Code and Codex
//! run). A worktree's `.senate/` follows its branch ([`follow`]).
//!
//! # Several Senates, one daemon
//!
//! A machine may serve the Senate above and any number of Senates one level
//! down, each presided over by one of the person's agents ([`config`]:
//! `[[senates]]`, filled in by `senated --machine` for the ones it hosts).
//! The daemon keeps one half per Senate (server, token, state database,
//! streams, cursors, heartbeats, queue, sessions) and nothing leaks between
//! them; a front on the local socket sends each request to the Senate it
//! concerns. An agent's path relative to the person selects its Senate
//! ([`agent`]): `claude` sits above, `claude/reviewer` below.
//!
//! # Presiding
//!
//! The session of an agent that holds a Senate's Speaker's seat presides
//! ([`preside`]): it is attached to that Senate as its Speaker as well as
//! to the one it sits in, is pushed what is put to the Speaker, and gets
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
pub mod client;
pub mod companion;
pub mod config;
pub mod daemon;
pub mod delivery;
pub mod follow;
pub mod format;
pub mod hook;
pub mod init;
pub mod install;
pub mod ipc;
pub mod mcp;
pub mod paths;
pub mod preside;
pub mod routing;
pub mod service;
pub mod session;
pub mod speak;
pub mod state;
pub mod status;
mod stream;

pub use config::BridgeConfig;
pub use daemon::{DaemonHandle, DaemonOptions, Timing};
pub use paths::Paths;
pub use session::{Session, SessionState};
