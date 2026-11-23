//! The shared core of the native Senate apps: a client of the developer
//! machine's daemon (`senated --machine`) over `bridge.sock`, exposed to
//! Swift (and later other languages) with UniFFI.
//!
//! - [`model`]: what the app knows, folded from the daemon's `watch` stream;
//!   the window's actions mapped to IPC requests.
//! - [`link`]: the socket, and the [`link::Driver`] that keeps the model
//!   current and runs actions.
//! - [`state`]: the flat snapshot the app renders, with its sentences
//!   written in the app's language ([`text`]).
//! - [`reads`]: what the app reads on demand and shows as it comes back
//!   (a thread), outside the snapshot.
//! - [`ffi`]: the UI-agnostic facade the apps call: [`AppCore`] owns a
//!   tokio runtime and a driver, and pushes [`AppState`] snapshots to a
//!   foreign [`CoreListener`].
//!
//! # What the app is, and is not
//!
//! The person's side of a machine: who they are in each Senate the daemon
//! serves, their agents, what waits on them (an inbox per agent and a merged
//! one, Codex approvals, the Codex panel). When one of their sessions
//! presides over a Senate as its Speaker, the app also lists that Senate's
//! docket, **read-only**: routing, covering and referring are the presiding
//! agent's own tools (`speaker_route`, `speaker_cover`, `speaker_escalate`),
//! run by that agent in its session, never by a person clicking in the app.
//!
//! # Languages
//!
//! Text produced here is written for the [`senate_core::Locale`] the app
//! passes in; fixed labels live in the app. See [`text`].
//!
//! None of it needs a window; the macOS app (`apps/macos`) renders what
//! [`CoreListener`] receives.

// UniFFI's scaffolding is `extern "C"` glue, which the workspace's
// `unsafe_code = "deny"` refuses; the hand-written code has none.
#![allow(unsafe_code)]

pub mod ffi;
pub mod link;
pub mod model;
pub mod names;
#[cfg(target_os = "macos")]
pub mod oslog;
pub mod reads;
pub mod state;
pub mod text;

pub use ffi::{
    AppCore, ApprovalDecision, CoreError, CoreListener, MentionKind, TerminalCommand, TextPart,
    split_mentions,
};
pub use reads::{ThreadEvent, ThreadView};
pub use state::{
    AgentInfo, AppState, ApprovalRequest, ChairLine, ChildLine, ChildSeatLine, CodexThreadInfo,
    CommitteeLine, Counts, DocketCommittee, DocketEntry, DocketSenate, HostedSession, InboxThread,
    MenuState, RosterCommittee, RosterMember, SeatLine, SenateInfo,
};

uniffi::setup_scaffolding!();
