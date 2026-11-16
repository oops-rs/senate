//! The developer-machine side of Senate: one daemon per machine joining
//! coding-agent sessions to committee logs.
//!
//! # A machine is a person running agents
//!
//! The bridge authenticates as a **person** (`bridge.toml`'s token) and seats
//! one junior senator per kind of coding agent it serves ([`agent`]): a
//! Claude Code session speaks as `<person>/claude`, a Codex session as
//! `<person>/codex`, unless `bridge.toml` renames them or a session pins its
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
//!   Codex; [`status`] renders what the daemon is doing.
//!
//! Everything written for a person or an agent to read is in the locale of
//! `bridge.toml` ([`senate_core::Locale`]); identifiers (addresses, kinds,
//! tool names, JSON fields) are the same in every language.
//!
//! Every path the bridge touches lives under [`paths::Paths`]
//! (`~/.senate`, or `$SENATE_HOME`).

pub mod agent;
pub mod approvals;
pub mod catch_up;
pub mod checkpoint;
pub mod client;
pub mod companion;
pub mod config;
pub mod daemon;
pub mod delivery;
pub mod format;
pub mod hook;
pub mod install;
pub mod ipc;
pub mod mcp;
pub mod paths;
pub mod routing;
pub mod session;
pub mod state;
pub mod status;
mod stream;

pub use config::BridgeConfig;
pub use daemon::{DaemonHandle, DaemonOptions, Timing};
pub use paths::Paths;
pub use session::{Session, SessionState};
