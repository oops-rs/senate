//! Local IPC between the daemon and its thin clients: the hooks, the MCP
//! shim, and `status`. This contract belongs to the bridge.
//!
//! # Transport
//!
//! A Unix domain socket at `$SENATE_HOME/bridge.sock` (default
//! `~/.senate/bridge.sock`), mode 0600 inside a 0700 directory, so only the
//! user can connect.
//!
//! # Framing
//!
//! Newline-delimited JSON, UTF-8, at most [`MAX_LINE`] bytes per line. The
//! client writes one request object and reads exactly one response line
//! before writing the next. A connection may carry any number of requests;
//! either side may close between them. A `session_drop` upload is the
//! exception: after the daemon accepts its JSON header, the client sends
//! the stated number of raw file bytes on that connection, then reads the
//! final JSON reply.
//!
//! # Requests
//!
//! An object tagged by `op` ([`Request`]):
//!
//! | `op` | Fields | `result` |
//! | --- | --- | --- |
//! | `ping` | — | [`Pong`] |
//! | `status` | — | [`StatusReply`] |
//! | `hook` | [`HookReport`] | [`HookReply`] |
//! | `spawn` | `session?`, `runtime`, `committee`, `alias?`, `cwd`, `chair?`, `secretary?` | [`SpawnReply`] |
//! | `restore` | `session?`, `senator` | [`SpawnReply`] |
//! | `attach` | `senator` (an alias may be `committee::alias`) | Local terminal socket path |
//! | `session_drop` | `session_id`, `name`, `size` | Ready, then the staged local path |
//! | `aliases` | — | [`KnownAliases`](senate_core::alias::KnownAliases) |
//! | `set_alias` | `senator`, `alias`, `committee?` | [`SenatorName`] |
//! | `roster` | — | `Vec<`[`RosterCommittee`]`>` |
//! | `remove_alias` | `primary`, `alias`, `committee?` | `{}` |
//! | `session_end` | `session_id`, `reason?` | `{}` |
//! | `ask` | `session`, `to` (addresses), `body`, `refs?`, `id?`, `request?` | [`Posted`] |
//! | `answer` | `session`, `thread` (root seq), `body`, `id?` | [`Posted`] |
//! | `inbox` | `session`, `committee?`, `agent?`, `all?` | [`InboxReply`] |
//! | `say` | `session`, `committee?`, `kind`, `to`, `body`, `thread?`, `id?` | [`Posted`] |
//! | `thread` | `session`, `committee?`, `thread`, `mark_seen?` | [`ThreadReply`] |
//! | `seen` | `committee`, `marks` (thread, seq) | `{}` |
//! | `catch_up` | `session`, `limit?` | [`CatchUpReply`] |
//! | `join_as` | `session`, `committee`, `senator` | [`JoinReply`] |
//! | `join` | `session`, `committee` | [`JoinReply`] |
//! | `set_committee_seat` | `committee`, `senator`, `action`, `id`, `acted_by?` | [`CommitteeSeatInfo`](senate_core::api::CommitteeSeatInfo) |
//! | `search` | `session`, `query`, `limit?`, `committee?` | [`SearchReply`] |
//! | `post` | `session`, `body`, `proposal?`, `committee?`, `id?` | [`Posted`] |
//! | `brief` | `session`, `committee?` | [`Brief`](senate_core::Brief) |
//! | `bound` | `session` | [`BoundReply`] |
//! | `checkpoint` | `session`, `summary?`, `progress?`, `decisions?`, `open?`, `push?`, `id?` | [`CheckpointReply`] |
//!
//! | `presides` | `session`, `runtime?`, `agent_env?`, `agent_file?` | [`PresidesReply`] |
//! | `docket` | `session`, `committee?` | [`DocketReply`] |
//! | `route` | `session`, `committee`, `seq`, `to`, `id?` | [`Posted`] |
//! | `cover` | `session`, `committee`, `thread`, `body`, `agent?`, `id?` | [`Posted`] |
//! | `hold` | `session`, `committee`, `seq`, `id?` | [`Posted`] |
//! | `refer` | `session`, `committee`, `seq`, `reason`, `id?` | [`Posted`] |
//! | `forward` | `session`, `committee`, `seq`, `id?` | [`Forwarded`] |
//! | `refuse` | `session`, `committee`, `seq`, `reason`, `id?` | [`Posted`] |
//! | `set_chair` | `session`, `committee?`, `senator` | [`ChairInfo`](senate_core::api::ChairInfo) |
//! | `chair_docket` | `session` | [`ChairDocketReply`] |
//! | `chair_route` | `session`, `seq`, `to`, `id?` | [`Posted`] |
//! | `chair_cover` | `session`, `thread`, `body`, `agent?`, `id?` | [`Posted`] |
//! | `chair_escalate` | `session`, `seq`, `reason`, `id?` | [`Posted`] |
//! | `secretary_cover` | `session`, `thread`, `body`, `agent?`, `id?` | [`Posted`] |
//! | `send` | `session`, `kind`, `to?`, `at?`, `body`, `refs?`, `id?` | [`SendReply`] |
//! | `secretaries` | `session`, `action`, `for?`, `secretary?` | [`SecretariesReply`] |
//! | `roles` | — | [`RolesReply`] |
//!
//! | `approvals` | — | [`ApprovalsReply`] |
//! | `approve` | `id`, `decision` | [`ApproveReply`] |
//! | `watch` | `app?` | `{"watching": true}`, then a stream of [`WatchEvent`] lines |
//!
//! # Presiding
//!
//! `presides` says whether the calling session's agent holds the Speaker's
//! seat of a Senate this machine serves (the session need not be
//! registered yet: the agent is then worked out from `runtime`,
//! `agent_env` and `agent_file` as a hook would). The MCP shim lists the
//! presiding tools only when it does.
//!
//! `docket`, `route`, `cover` and `refer` are the presiding session's: each
//! acts on the Senate its agent presides over, writing as `speaker` with
//! the person's token there, and is refused (`not_presiding`) for any other
//! session. `docket` lists what awaits the Speaker ([`senate_core::docket`])
//! on `committee`, or on every committee of that Senate. `route` relays an
//! item to one agent seated there (one of the candidates of a pick);
//! `cover` answers a question to an away agent on that agent's behalf from
//! its own latest checkpoint (`agent` chooses among the agents the question
//! was addressed to, never another); `refer` puts an item to the person.
//! Without a session, `docket` is a person's read of every Senate on this
//! machine one of their agents presides over: what waits for a Speaker that
//! may be away.
//!
//! # The Speaker's office
//!
//! A session presides when its agent works the Speaker's desk
//! ([`senate_core::Office::desk`]: the Speaker itself when it is an agent,
//! then the Speaker's secretaries, in order). Everything it writes as
//! `speaker` carries `meta.acted_by` = its agent. `forward` passes a desk
//! item that carries an intended recipient (`meta.intended`, an agent's
//! message for another committee) on: a new root at that seat, `from:
//! speaker`, with `meta.origin` naming the asker and its thread, and a note
//! in the asker's thread with `meta.forwarded_to`. `refuse` answers the
//! asker in its thread with the reason instead. `set_chair` (with a
//! presiding session, or without a session for the person) makes a senator
//! the chair of a committee.
//!
//! # A committee's chair
//!
//! A session whose agent chairs its bound committee gets `chair_docket`
//! (what awaits the chair there, `GET .../docket?role=chair`),
//! `chair_route`, `chair_cover` and `chair_escalate` (one step up: the
//! Speaker's desk); any other session is refused with
//! [`codes::NOT_CHAIRING`]. Events addressed to `chair` are pushed to the
//! chair's session (an agent) or announced to the person (a principal).
//!
//! # Qualified names and secretaries
//!
//! `send` is an agent writing a question, request or note: to addresses on
//! its own committee, or to a qualified name (`at`). A name on another
//! committee is written on the session's own committee to `speaker` with
//! `meta.intended` = the name, completed (the boundary rule: the Speaker's
//! desk gets it); [`SendReply`] says where it went. `secretaries` lists,
//! adds or removes a senator's secretaries (`for`: the session's agent, or
//! without a session the person). `roles` is what `senate status` shows of
//! the Speaker's office, the chairs and the child Senates.
//!
//! `escalate` with no `to` goes one step up: a member to its committee's
//! chair, the chair (or a committee with no chair) to the Speaker's desk, a
//! desk worker to the Speaker; [`EscalateReply`] says where.
//!
//! `say` and `thread` are the person's own (`senate ask`, `tell`, `request`,
//! `reply`, `thread`): `say` appends a `question`, `note` or `request` as
//! `senator::<person>`, never as an agent. `to` may hold an agent, a person,
//! `speaker`, or `senator::*`, which the server resolves among the agents on
//! the committee, or puts to the Speaker to pick where a senator presides;
//! with several agents and nobody to pick it is refused with
//! [`codes::AMBIGUOUS_RECIPIENT`]. `at` instead of `to` is a qualified name
//! (`ledger::senator::codex`): the daemon resolves it through the server and
//! posts on the committee it names. With
//! `thread` (any seq in it) the event is written in that thread, and an
//! empty `to` addresses whoever took part in it other than the person. A
//! person's `committee` is required without a session; which Senate the
//! request goes to follows the other sessionless requests (the primary one:
//! where the person's agents sit directly). `thread` reads one thread's events,
//! and those of the thread a Speaker relayed it into (`meta.relayed_from`).
//!
//! `approvals` / `approve` / `watch` are for a desktop app.
//! `approvals` lists the Codex approval requests waiting
//! ([`crate::approvals`]); `approve` answers one (`decision`: `allow`,
//! `allow_for_session`, `deny`, `cancel`), and fails when it was answered
//! elsewhere first. `watch` turns the connection into a one-way stream:
//! after its reply, every line is a [`WatchEvent`], starting with a
//! `status` and an `approval_requested` per pending approval; the client
//! sends nothing more and closes the connection to stop. The MCP server of
//! a session watches too, for `roles` alone: a session's tools follow what
//! its agent is ([`crate::mcp::SenateMcp::follow_roles`]).
//!
//! `inbox`, `answer`, `take`, `post` and `status` (the `task` op) also take
//! an optional `committee`: with it they need no registered session (a
//! desktop app has none). Without a session, which of this bridge's agents
//! acts is `agent` (a name, e.g. `codex`) when given, else the only agent
//! this bridge holds a stream for on that committee; with several it is
//! refused. `answer` and `task` take `as_person: true` to speak as
//! `senator::<person>` rather than as an agent.
//!
//! # Whose
//!
//! A request from a session acts as that session's agent
//! ([`Session::agent`]). Everything an agent reads back (catch-up, inbox,
//! checkpoints) is that agent's, never its person's other agents'.
//!
//! `checkpoint` writes the files it is given into
//! `<git root>/.senate/`, reads all three back, and appends a `checkpoint`
//! event for the session's worktree ([`crate::checkpoint`]); with `push`
//! it first runs `git push origin HEAD:<branch>` and the checkpoint's
//! trigger is `handoff`.
//!
//! `session` is a [`SessionSelector`]: `{"session_id"?: "...", "claude_pid"?: 123}`.
//! The daemon picks the non-ended session with that id; failing that, the
//! most recently active non-ended session of that Claude pid (the MCP shim
//! outlives `/clear`, which starts a new session id in the same process),
//! matched against the pid the session's hooks reported (`claude_pid`) and
//! then against the pids above its hooks (`ancestor_pids`, reported when
//! Claude does not export `CLAUDE_PID` or the hook runs under a hosted TUI).
//! A Codex session is selected by its thread id alone: the MCP shim takes it from each tool call's
//! `_meta.threadId`, and Codex hooks report no pids (every thread of an
//! app-server shares the same ones).
//!
//! `ask` accepts an agent (`senator::codex`) or whichever agent on the
//! committee should get it (`senator::*`); the Speaker, the committee, a
//! person and guests are refused. When the server resolves `senator::*`
//! and several agents could get it, the reply is the
//! error [`codes::AMBIGUOUS_RECIPIENT`] with the agents in
//! [`IpcError::candidates`]: the asker names one and asks again.
//!
//! `committee` on `search` / `post` / `brief` names the committee to act on;
//! without it the session's bound committee is used, and with it no
//! registered session is needed. `post` appends an undirected `note`; with
//! `proposal: true` the note carries `meta.proposal = "decision"`, a decision
//! proposal a person confirms in the committee's chat.
//!
//! `id` on `ask` / `answer` / `post` is the event's ULID ([`EventId`]), minted by the
//! client. A client that times out may resend the same request with the same
//! `id`; the server stores the event once and the daemon answers the retry
//! with `duplicate: true`. Without `id` the daemon mints one per request.
//!
//! # Responses
//!
//! `{"ok":true,"result":<value>}` or
//! `{"ok":false,"error":{"code":"…","message":"…"}}`, codes in
//! [`codes`]. `message` is written for the agent or the user to read.
//!
//! # Compatibility
//!
//! Fields are only added; unknown fields are ignored; [`PROTOCOL_VERSION`]
//! changes only on a break.
//!
//! ```text
//! → {"op":"ping"}
//! ← {"ok":true,"result":{"protocol":1,"pid":4242,"person":"alice","locale":"en","version":"0.1.0"}}
//! → {"op":"catch_up","session":{"session_id":"d7ac…"},"limit":20}
//! ← {"ok":true,"result":{"committee":"refund-v2","items":[…],"has_more":false,…}}
//! ```

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate_core::alias::AliasName;
use senate_core::{
    Address, CommitteeId, Event, EventId, Kind, Locale, Name, Refs, SenatorName, Seq,
    StatusPayload, ThreadSummary,
};

use crate::approvals::{Approval, Decision, ResolvedBy};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use crate::catch_up::CatchUpItem;
use crate::hook::HookKind;
pub use crate::host_approval::ClaudeApprovalSignal;
use crate::session::{Runtime, Session};
use crate::state::QueueCounts;

pub const PROTOCOL_VERSION: u32 = 1;
/// Longest accepted line, in bytes.
pub const MAX_LINE: usize = 1 << 20;
/// Separate from the JSON line budget: an authorized SessionDrop transfer
/// sends exactly this many bytes at most after the daemon says it is ready.
pub const MAX_SESSION_DROP_BYTES: u32 = 20 * 1024 * 1024;
pub(crate) const SESSION_DROP_TIMEOUT: Duration = Duration::from_secs(15);

/// Error codes.
pub mod codes {
    pub const LEGACY_HOST: &str = "legacy_host";
    /// The request was malformed or its arguments invalid.
    pub const BAD_REQUEST: &str = "bad_request";
    /// No registered session matches the selector.
    pub const NO_SESSION: &str = "no_session";
    /// The session is not bound to any committee.
    pub const NOT_JOINED: &str = "not_joined";
    /// A presiding op from a session whose agent does not work the
    /// Speaker's desk.
    pub const NOT_PRESIDING: &str = "not_presiding";
    /// A chair op from a session whose agent does not chair its committee.
    pub const NOT_CHAIRING: &str = "not_chairing";
    /// A request only a person may make came from inside a coding
    /// session's process tree.
    pub const NOT_A_PERSON: &str = "not_a_person";
    /// The Senate server refused or could not be reached.
    pub const SERVER: &str = "server";
    /// `senator::*` could reach several agents on the committee;
    /// [`IpcError::candidates`](super::IpcError::candidates) lists them.
    pub const AMBIGUOUS_RECIPIENT: &str = senate_core::api::code::AMBIGUOUS_RECIPIENT;
    pub const INTERNAL: &str = "internal";
    /// A hosted spawn may have created a session, but has no confirmed receipt.
    pub const SPAWN_OUTCOME_UNKNOWN: &str = "spawn_outcome_unknown";
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_pid: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
}

/// Which seat now owns a repaired terminal and what work stayed behind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebindHostReply {
    pub retired: SenatorName,
    pub current: SenatorName,
    pub session_id: String,
    pub undelivered: usize,
    pub threads: Vec<RebindThread>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeAuthorityDetails {
    pub approval: EventId,
    pub pid: u32,
    pub executable: PathBuf,
    pub signing_team: Option<String>,
    pub cwd: Option<PathBuf>,
    pub hosted: bool,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RebindThread {
    pub committee: CommitteeId,
    pub seq: Seq,
}

/// What a SessionStart / UserPromptSubmit hook saw.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookReport {
    pub event: HookEvent,
    pub session_id: String,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_path: Option<String>,
    /// SessionStart's `source`: `startup`, `resume`, `clear`, `compact`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_root: Option<PathBuf>,
    /// Contents of `.senate/committee`, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committee_file: Option<String>,
    /// A Claude inbox socket or a Senate-owned ACP listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// Claude inbox token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_pid: Option<u32>,
    /// The hook's parent, grandparent, ... (nearest first), reported when
    /// `CLAUDE_PID` is absent or the hook runs under a hosted TUI. Used for
    /// MCP lookup and to verify the process reporting a new native ID.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ancestor_pids: Vec<u32>,
    /// Absent from an older hook, which is always Claude Code's.
    #[serde(default)]
    pub runtime: Runtime,
    /// `SENATE_AGENT` as the hook saw it ([`crate::agent`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_env: Option<String>,
    /// Contents of `.senate/agent`, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_file: Option<String>,
    /// The Senate terminal host an agy hook runs under
    /// ([`crate::host::HOSTED_AGY_ENV`]). agy names its conversation only
    /// once it starts, so its first hook tells the host which one it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<EventId>,
}

impl std::fmt::Debug for HookReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookReport")
            .field("event", &self.event)
            .field("session_id", &self.session_id)
            .field("runtime", &self.runtime)
            .field("cwd", &self.cwd)
            .finish_non_exhaustive()
    }
}

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    clap::ValueEnum,
    rmcp::schemars::JsonSchema,
)]
#[schemars(crate = "rmcp::schemars", inline)]
#[serde(rename_all = "lowercase")]
pub enum HostedRuntime {
    Codex,
    Claude,
    // The Antigravity CLI.
    Agy,
}

/// A profile lookup at the daemon. Its runtime assertion travels inside
/// this selection so an older daemon cannot silently launch without it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnProfile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<HostedRuntime>,
}

/// The requested model and reasoning effort for a hosted session. An omitted
/// setting leaves the runtime in control. Some runtimes resolve model aliases
/// through their live catalog ([`HostedRuntime::efforts`] covers effort).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedModel {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// A launch-time mismatch or an unavailable verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_warning: Option<String>,
}

impl From<HostedRuntime> for Runtime {
    fn from(runtime: HostedRuntime) -> Self {
        match runtime {
            HostedRuntime::Codex => Self::Codex,
            HostedRuntime::Claude => Self::Claude,
            HostedRuntime::Agy => Self::Agy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingReply {
    pub waiting: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unanswered: Vec<Seq>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnReply {
    /// Canonical model when an explicit alias was expanded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_warning: Option<String>,
    pub senator: SenatorName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub session_id: String,
    pub committee: CommitteeId,
    /// The request carrying the spawn's prompt, when it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Seq>,
}

/// One step of `senate session replace`, in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceStep {
    /// Start the new session and wait until it is seated and connected.
    Spawn,
    /// Move the alias on the committee to the new seat.
    Alias,
    /// Give the new seat the old one's duty.
    Duty,
    /// Put the new seat where the old one stood among desk workers.
    Desk,
    /// Make the new seat the chair, when the old one was.
    Chair,
    /// Hand the old seat's open threads to the new one.
    Handover,
    /// Send the starting prompt.
    Prompt,
    /// Stop the old session's terminal.
    Stop,
    /// Unseat the old seat.
    Unseat,
}

impl ReplaceStep {
    pub const ALL: [Self; 9] = [
        Self::Spawn,
        Self::Alias,
        Self::Duty,
        Self::Desk,
        Self::Chair,
        Self::Handover,
        Self::Prompt,
        Self::Stop,
        Self::Unseat,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Alias => "alias",
            Self::Duty => "duty",
            Self::Desk => "desk",
            Self::Chair => "chair",
            Self::Handover => "handover",
            Self::Prompt => "prompt",
            Self::Stop => "stop",
            Self::Unseat => "unseat",
        }
    }
}

/// What one step of a replacement did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceStepReport {
    pub step: ReplaceStep,
    /// Done by an earlier run of the same replacement.
    #[serde(default)]
    pub earlier: bool,
    /// What it did, or why there was nothing to do.
    pub detail: String,
}

/// `Request::Replace` once every step is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceReply {
    pub committee: CommitteeId,
    pub old: SenatorName,
    pub new: SenatorName,
    pub session_id: String,
    pub steps: Vec<ReplaceStepReport>,
    /// The alias the new seat took over from the old one, if it had one:
    /// what to name it by from now on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<SenatorName>,
}

/// `Request::Sessions`: the hosted sessions on this machine, the running
/// first, then by name.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SessionsReply {
    pub rows: Vec<SessionRow>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local_rows: Vec<LocalSessionRow>,
    /// Hosts running with no session on record: what `senate session list
    /// prune` stops.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orphans: Vec<EventId>,
}

/// One hosted session: what `senate session list` lists, the picker offers
/// and `senate session watch` shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_health: Option<Box<HostHealth>>,
    /// A hosted Codex thread still uses the shared app-server.
    #[serde(default, skip_serializing_if = "is_false")]
    pub codex_scope_advisory: bool,
    /// The host's id: its directory under `hosts/`.
    pub host_id: EventId,
    pub session_id: String,
    /// The senator it speaks as.
    pub primary: SenatorName,
    /// Its alias on its committee, when it has one and the Senate said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<SenatorName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committee: Option<CommitteeId>,
    /// Whether this senator is the chair of its session’s committee.
    #[serde(default, skip_serializing_if = "is_false")]
    pub chair: bool,
    /// The alias as displayed in this Senate, qualified when ambiguous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shown_name: Option<String>,
    pub runtime: crate::session::Runtime,
    #[serde(default)]
    pub working: bool,
    #[serde(default)]
    pub last_activity: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The model and effort it was last started with.
    #[serde(default, skip_serializing_if = "HostedModel::is_default")]
    pub model: HostedModel,
    pub dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub state: HostState,
    /// When its host started; `None` when it is not running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// Someone is attached to its terminal now.
    #[serde(default, skip_serializing_if = "is_false")]
    pub attached: bool,
    /// Seconds since a person last typed into its terminal; `None`: never
    /// (since its host started).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typed_secs_ago: Option<u64>,
    /// The attached viewer's terminal device (`/dev/ttys012`), when it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewer_tty: Option<String>,
    /// The host's attach socket.
    pub tty: PathBuf,
    /// A permission prompt waits in its terminal for a person: what it is
    /// for and since when. A fact only: deciding stays in the terminal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<HostedPrompt>,
    /// Recorded paused (`senate session pause`): stopped, to be resumed.
    #[serde(default, skip_serializing_if = "is_false")]
    pub paused: bool,
}

/// An actionable warning about the host process, independent of its seat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostHealth {
    pub started_by: String,
    pub outdated: bool,
    pub daemon_unverified: bool,
}

impl HostHealth {
    pub fn notice(&self) -> String {
        if self.outdated {
            format!(
                "needs restart to be protected (started by {})",
                self.started_by
            )
        } else {
            format!(
                "terminal host has not verified the current daemon (started by {})",
                self.started_by
            )
        }
    }
}

/// A permission prompt waiting in a hosted terminal ([`SessionRow::prompt`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostedPrompt {
    /// The tool or command it asks about, when the runtime said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub since: DateTime<Utc>,
    /// A choice the host saw waiting on the screen, which no runtime
    /// signal told of (a "hooks need review" at startup, say), rather than
    /// a permission prompt.
    #[serde(default, skip_serializing_if = "is_false")]
    pub screen: bool,
}

/// A live session without a Senate terminal host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalSessionRow {
    pub session_id: String,
    pub primary: SenatorName,
    pub alias: Option<SenatorName>,
    pub committee: Option<CommitteeId>,
    pub chair: bool,
    pub shown_name: Option<String>,
    pub runtime: crate::session::Runtime,
    pub dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub working: bool,
    pub last_activity: DateTime<Utc>,
    pub title: Option<String>,
}

impl SessionRow {
    /// What a command takes to name it: `committee::alias`, else its
    /// senator name.
    pub fn label(&self) -> String {
        match (&self.committee, &self.alias) {
            (Some(committee), Some(alias)) => format!("{committee}::{alias}"),
            _ => self.primary.to_string(),
        }
    }

    pub fn display_name(&self, verbose: bool) -> String {
        if verbose {
            return self.primary.to_string();
        }
        self.shown_name
            .clone()
            .or_else(|| self.alias.as_ref().map(ToString::to_string))
            .unwrap_or_else(|| senate_core::alias::short_name(&self.primary))
    }

    pub fn is_active(&self) -> bool {
        senate_core::continuity::roster_active(None, self.is_running(), self.paused)
    }

    pub fn is_running(&self) -> bool {
        self.state == HostState::Running
    }
}

/// What [`Request::Terminal`] asks. Every action but `open`, `list` and a
/// `screen` of another session names a scratch terminal by its id, and the
/// daemon serves it only to whoever opened that terminal (or to the
/// person): an id is never the host of a hosted session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum TerminalAction {
    /// Reserve a scratch terminal for the caller ([`TerminalReply::Opened`]).
    /// The caller then starts its host itself
    /// ([`crate::host::scratch::start`]): the daemon runs nothing.
    Open {
        command: Vec<String>,
        cwd: PathBuf,
        rows: u16,
        cols: u16,
    },
    /// The caller's scratch terminals; every one, for the person.
    List,
    /// A screen as text: the caller's scratch terminal, or, for a
    /// committee's chair, a hosted session on its committee (read only).
    Screen {
        #[serde(flatten)]
        target: TerminalTarget,
        /// Rows above the screen to include, of what scrolled off.
        #[serde(default)]
        scrollback: u16,
    },
    /// Type `text` as it is; a line end is Enter.
    Type {
        id: EventId,
        text: String,
    },
    /// Press named keys, in order (`down down enter`, `ctrl-c`).
    Key {
        id: EventId,
        keys: String,
    },
    /// Wait until the screen shows `text`, or has been still for
    /// `still_ms`, or (given neither) is other than the caller was last
    /// shown; at most `timeout_ms`.
    Wait {
        id: EventId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        still_ms: Option<u64>,
        timeout_ms: u64,
    },
    Resize {
        id: EventId,
        rows: u16,
        cols: u16,
    },
    /// End the program and forget the terminal.
    Close {
        id: EventId,
    },
}

/// Whose screen a [`TerminalAction::Screen`] reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalTarget {
    /// A scratch terminal of the caller's.
    Id(EventId),
    /// A hosted session, by alias: the chair of its committee only.
    Session(AliasName),
}

/// What a [`Request::Terminal`] came to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminalReply {
    Opened { id: EventId },
    List { terminals: Vec<TerminalRow> },
    Screen(TerminalScreen),
    Closed { id: EventId },
}

/// One scratch terminal in a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalRow {
    pub id: EventId,
    /// The session that opened it, as a command names it; `None` when a
    /// person did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub opened_at: DateTime<Utc>,
    /// `starting`, `running`, or how the program ended (`code 0`).
    pub state: String,
    /// A person is attached and has its keyboard.
    pub attached: bool,
}

/// A terminal's screen as text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TerminalScreen {
    /// The scratch terminal read; `None` for another session's screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<EventId>,
    /// The hosted session whose screen this is (`committee::alias`). What
    /// it shows was not written for the reader: untrusted text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
    pub rows: u16,
    pub cols: u16,
    /// The cursor's row and column, counted from 1.
    pub cursor_row: u16,
    pub cursor_col: u16,
    #[serde(default, skip_serializing_if = "is_false")]
    pub cursor_hidden: bool,
    /// The program draws on the alternate screen (a full-screen program).
    #[serde(default, skip_serializing_if = "is_false")]
    pub alternate: bool,
    /// Rows that scrolled off above the screen, oldest first, when asked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scrollback: Vec<String>,
    /// Every row of the screen, top to bottom.
    pub lines: Vec<String>,
    /// How the program ended, once it has (`code 0`, `signal 9`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exited: Option<String>,
    /// A person is attached and has its keyboard.
    #[serde(default, skip_serializing_if = "is_false")]
    pub attached: bool,
    /// A `wait` ran out before what it waited for.
    #[serde(default, skip_serializing_if = "is_false")]
    pub timed_out: bool,
}

/// `Request::PruneHosts`: what it stopped and what it removed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PruneReply {
    /// Orphaned hosts stopped, with their TUIs.
    pub stopped: Vec<EventId>,
    /// Directories of ended hosts removed.
    pub removed: Vec<EventId>,
}

/// Whether a hosted session's terminal is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    Running,
    /// Its host has ended; `senate session restore` reopens it.
    Stopped,
}

impl HostState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopReply {
    pub senator: SenatorName,
    pub session_id: String,
    pub committee: CommitteeId,
    /// The stop note is queued after an uncertain append.
    #[serde(default, skip_serializing_if = "is_false")]
    pub audit_pending: bool,
}

/// How long `senate session pause` waits for a checkpoint by default.
pub const PAUSE_TIMEOUT_SECS: u64 = 90;

/// What [`Request::Pause`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseReply {
    pub senator: SenatorName,
    pub committee: CommitteeId,
    /// The checkpoint it left after the request, if it left one in time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Seq>,
    /// Seconds spent waiting for it.
    pub waited_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttachTarget {
    Session {
        session_id: String,
    },
    Senator {
        senator: senate_core::alias::AliasName,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachInfo {
    pub session_id: String,
    pub primary: SenatorName,
    pub tty: Option<PathBuf>,
    pub paused: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_health: Option<HostHealth>,
}

/// An office whose current holder can be used as a hosted session selector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionRole {
    Chair {
        committee: CommitteeId,
    },
    Secretary {
        senate: Option<senate_core::SenatePath>,
    },
    Speaker,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRoleHolder {
    pub primary: SenatorName,
    /// The current hosted session here, if recorded, even when stopped.
    pub session_id: Option<String>,
    /// All eligible holders in desk order, for locating one on a paired machine.
    pub candidates: Vec<SenatorName>,
}

/// What [`Request::Resume`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub senator: SenatorName,
    pub committee: CommitteeId,
    /// The checkpoint the resumed note points at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Seq>,
    /// The resumed note's seq, when it was posted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<Seq>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartHostReply {
    pub checkpoint: Option<Seq>,
    pub resumed: ResumeReply,
    pub started_empty: bool,
}

/// The hints available when a hook times out before reading its payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDropReport {
    pub kind: HookKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_hint: Option<Runtime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_id: Option<EventId>,
    pub parent_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookDropStatus {
    /// Reports received since this daemon started.
    pub count: u64,
    pub last_at: chrono::DateTime<chrono::Utc>,
    pub last: HookDropReport,
    /// Last drop for each current hosted Claude session with an active warning.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub hosted_last_at: std::collections::BTreeMap<String, chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineDirection {
    #[default]
    Tail,
    Before,
    After,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineReply {
    pub session_id: String,
    pub runtime: crate::session::Runtime,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<TimelineUnavailable>,
    pub items: Vec<TimelineItem>,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub has_older: bool,
    pub has_newer: bool,
    /// This daemon observed a prompt hook and has yet to observe its turn end.
    pub working: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineUnavailable {
    UnsupportedSchema,
    TemporarilyUnreadable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineUpdate {
    pub items: Vec<TimelineItem>,
    pub working: bool,
    /// The transcript changed identity. Fetch a fresh tail and reconnect.
    #[serde(default, skip_serializing_if = "is_false")]
    pub reset: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineItem {
    pub seq: u64,
    pub ts: Option<DateTime<Utc>>,
    pub truncated: bool,
    #[serde(flatten)]
    pub kind: TimelineKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineKind {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        call_id: String,
        name: String,
        status: TimelineToolStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        detail: TimelineToolDetail,
    },
    Todo {
        items: Vec<TimelineTodo>,
    },
    Notice {
        level: String,
        text: String,
    },
    SenateMessage {
        committee: Option<String>,
        #[serde(rename = "senate_seq")]
        seq: Option<u64>,
        thread: Option<u64>,
        from: Option<String>,
        #[serde(rename = "senate_kind")]
        kind: Option<String>,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineTodo {
    pub text: String,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineToolStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimelineToolDetail {
    Shell {
        command: String,
        cwd: Option<String>,
        output: Option<String>,
        exit_code: Option<i64>,
    },
    Read {
        path: String,
    },
    Edit {
        path: String,
        unified_diff: Option<String>,
        old: Option<String>,
        new: Option<String>,
    },
    Write {
        path: String,
    },
    Search {
        query: String,
    },
    Fetch {
        url: String,
    },
    SubAgent {
        description: String,
    },
    Other {
        summary: String,
    },
}

/// One page of ended or stopped sessions, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryReply {
    pub rows: Vec<SessionHistoryRow>,
    pub next_before: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHistoryRow {
    pub session_id: String,
    pub primary: SenatorName,
    pub alias: Option<SenatorName>,
    pub shown_name: Option<String>,
    pub committee: Option<CommitteeId>,
    pub runtime: crate::session::Runtime,
    pub model: HostedModel,
    pub dir: PathBuf,
    pub branch: Option<String>,
    pub title: Option<String>,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    pub readable: bool,
    pub restorable: bool,
    pub item_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchReply {
    pub hits: Vec<SessionSearchHit>,
    pub next_cursor: Option<String>,
    /// Retry the same query while the initial or incremental index build runs.
    #[serde(default, skip_serializing_if = "is_false")]
    pub indexing: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<SessionIndexProgress>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionIndexProgress {
    pub indexed: usize,
    pub total: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSearchHit {
    pub session_id: String,
    pub seq: u64,
    pub kind: SessionSearchKind,
    pub snippet: String,
    pub ts: Option<DateTime<Utc>>,
    #[serde(default)]
    pub live: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSearchKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardTarget {
    Own,
    Committee {
        committee: Option<CommitteeId>,
    },
    Member {
        senator: senate_core::alias::AliasName,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    #[doc(hidden)]
    RemoteControl {
        command: crate::remote::Control,
    },
    ObserveAdmin {
        command: senate_observe::AdminRequest,
    },
    SetCard {
        session: SessionSelector,
        target: CardTarget,
        document: String,
    },
    DeskWorker {
        command: senate_core::worker::Admin,
    },
    ServerHealth,
    Ping,
    Status,
    Profiles,
    /// A diagnostic for a stalled read, with no native hook payload.
    HookDropped(HookDropReport),
    /// Fixed-seat registrations from previous bridge clients.
    Hook(HookReport),
    /// Native coding session. Its immutable senator ID is derived on the daemon.
    SessionHook(HookReport),
    Spawn {
        /// UUIDv7 for one launch. Reuse it for up to seven days when retrying.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt_id: Option<String>,
        #[serde(default)]
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<HostedRuntime>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<SpawnProfile>,
        committee: CommitteeId,
        #[serde(default, skip_serializing_if = "is_false")]
        chair: bool,
        #[serde(default, skip_serializing_if = "is_false")]
        secretary: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<SenatorName>,
        cwd: PathBuf,
        /// A first instruction for the new seat: sent, once it is seated,
        /// as an ordinary Senate request from whoever spawned it. Never
        /// typed into its terminal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// The runtime's model and effort; unset, its own default.
        #[serde(flatten)]
        model: HostedModel,
    },
    NativeAuthorityCandidate {
        session_id: String,
    },
    AuthorizeNativeAuthority {
        session_id: String,
        approval: EventId,
    },
    /// Person-only repair of a legacy split hosted agy registration.
    RebindHost {
        host: String,
        to: AliasName,
    },
    Restore {
        #[serde(default)]
        session: SessionSelector,
        senator: AliasName,
        /// What to change of the recorded model and effort, for this
        /// restore and those after it.
        #[serde(flatten)]
        model: HostedModel,
    },
    /// `senate session replace`: start a fresh hosted session of the same
    /// runtime, directory and committee as `senator`, hand it everything
    /// the old seat held, then stop and unseat the old one. From the
    /// person's command line only. Asked again for a replacement that
    /// stopped partway, it resumes where it stopped.
    Replace {
        senator: AliasName,
        /// The new seat's first request, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// What to change of the old seat's model and effort.
        #[serde(flatten)]
        model: HostedModel,
    },
    Stop {
        #[serde(default)]
        session: SessionSelector,
        senator: AliasName,
        /// Only the local operator may override an attending viewer.
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    /// `senate session pause`: ask `senator` to checkpoint, wait for it
    /// (at most `timeout_secs`, default [`PAUSE_TIMEOUT_SECS`]), stop its
    /// terminal as [`Request::Stop`] does, and record it paused
    /// ([`PauseReply`]). The local operator's: the CLI only.
    Pause {
        senator: AliasName,
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// `senate session resume`: restore a paused `senator` into the same
    /// conversation and send it a note pointing at its last checkpoint
    /// ([`ResumeReply`]). The CLI only.
    Resume {
        senator: AliasName,
    },
    /// Every Senate-hosted session this machine has a record of, and every
    /// host running with none ([`SessionsReply`]).
    Sessions,
    /// After a successful readiness reply, send exactly `size` raw bytes.
    /// The final reply is the saved path and closes this connection.
    SessionDrop {
        session_id: String,
        name: String,
        size: u32,
    },
    /// Personal activity for inclusive local calendar dates.
    Stats {
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
        tz: String,
    },
    SessionHistory {
        #[serde(default)]
        committee: Option<CommitteeId>,
        #[serde(default)]
        before: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    SessionSearch {
        query: String,
        #[serde(default)]
        committee: Option<CommitteeId>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        cursor: Option<String>,
    },
    SessionForget {
        session_id: String,
    },
    Timeline {
        session_id: String,
        #[serde(default)]
        direction: TimelineDirection,
        #[serde(default)]
        cursor: Option<u64>,
        #[serde(default)]
        limit: Option<usize>,
    },
    CommitteeThreads {
        committee: CommitteeId,
        #[serde(default)]
        include_archived: bool,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        cursor: Option<String>,
    },
    ActivityWatch {
        /// None selects every readable, unarchived committee.
        #[serde(default)]
        committees: Option<Vec<CommitteeId>>,
        /// Omitted committees begin at their current head.
        #[serde(default)]
        after: std::collections::BTreeMap<CommitteeId, Seq>,
    },
    TimelineWatch {
        session_id: String,
        after: u64,
    },
    /// Stop every host running with no session on record, and remove the
    /// directories of hosts that have ended ([`PruneReply`]). The local
    /// operator's: the CLI only.
    PruneHosts,
    RestartHost {
        senator: senate_core::alias::AliasName,
        host: EventId,
        force: bool,
    },
    /// Resolve a locally hosted terminal for the person attaching to it.
    /// Look up a hosted session without starting its terminal.
    AttachInfo {
        #[serde(flatten)]
        target: AttachTarget,
    },
    /// List terminal choices without reading conversation transcripts.
    AttachChoices,
    ResolveSessionRole {
        role: SessionRole,
    },
    SessionRoleNames,
    Attach {
        senator: AliasName,
    },
    OpenViewer {
        host: EventId,
        writable: bool,
    },
    /// Terminal use ([`TerminalAction`]): the calling session's own scratch
    /// terminals, and a chair's read of a hosted session's screen. Without
    /// a session it is the person's request.
    Terminal {
        #[serde(default)]
        session: SessionSelector,
        action: TerminalAction,
    },
    /// How many messages wait for `session_id`'s next hook.
    Waiting {
        session_id: String,
    },
    SessionEnd {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    HostedSessionEnd {
        session_id: String,
        host_id: EventId,
    },
    /// An observation from a Claude hook installed by its terminal host.
    HostedClaudeApproval {
        session_id: String,
        host_id: EventId,
        at: u64,
        signal: ClaudeApprovalSignal,
    },
    Ask {
        session: SessionSelector,
        to: Vec<Address>,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refs: Option<Refs>,
        /// The question's event id; a retry resends the same one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
        /// A `request` (asks them to do something) rather than a question.
        /// It always goes to the person first.
        #[serde(default, skip_serializing_if = "is_false")]
        request: bool,
    },
    Answer {
        session: SessionSelector,
        thread: Seq,
        body: String,
        /// The answer's event id; a retry resends the same one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
        /// Act on this committee without a session.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        /// Without a session: which of this bridge's agents answers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<Name>,
        /// Answer as `senator::<person>` rather than as an agent.
        #[serde(default, skip_serializing_if = "is_false")]
        as_person: bool,
    },
    CatchUp {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    Join {
        session: SessionSelector,
        committee: CommitteeId,
    },
    /// `senate join` on a committee the server has no event of yet: open
    /// it there with one note from the person, so `spawn` (which never
    /// creates a committee) can start sessions on it.
    OpenCommittee {
        committee: CommitteeId,
    },
    /// The Speaker's office opens a new committee for a project and starts
    /// its chair there (the `open_committee` tool). Only a session working
    /// the Speaker's desk. A committee that exists is refused, unless this
    /// office opened it and its chair has not been started yet: the retry
    /// of a launch that failed. A chair once appointed is never replaced.
    DeskOpenCommittee {
        session: SessionSelector,
        committee: CommitteeId,
        cwd: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime: Option<HostedRuntime>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<SpawnProfile>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<SenatorName>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    /// Archive or unarchive a committee (`senate committees archive`): the
    /// Speaker in person; refused from inside a coding session's process
    /// tree, and while a hosted session on it runs here.
    ArchiveCommittee {
        committee: CommitteeId,
        archived: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// The archived committees (`senate committees --archived`, the app's
    /// Show archived).
    ArchivedCommittees,
    /// Make the terminal attached to a hosted session ring and flash its
    /// title (`senate session find`).
    FindSession {
        senator: AliasName,
    },
    /// The machine's default secretary, changing nothing (`senate setup
    /// doctor`, the app).
    DefaultSecretary,
    /// Make the machine's default secretary run: start, resume or restore
    /// it ([`DefaultSecretary`]). The person's own request only: refused
    /// from inside a coding session's process tree.
    EnsureSecretary,
    /// Separate operation so an older daemon cannot silently ignore the name.
    /// The committees and their current rosters.
    Roster,
    /// Set a committee alias for a senator the person runs.
    SetAlias {
        senator: AliasName,
        alias: SenatorName,
        committee: Option<CommitteeId>,
    },
    /// The Senate's aliases ([`KnownAliases`](senate_core::alias::KnownAliases)).
    Aliases,
    /// Remove `alias` while it stands for `primary`: from `committee`, or
    /// (an older server's Senate-wide alias) wherever it is.
    RemoveAlias {
        primary: SenatorName,
        alias: SenatorName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
    },
    /// Read one recorded event and the local bridge's delivery observations.
    Receipt {
        committee: CommitteeId,
        seq: Seq,
    },
    SetCommitteeSeat {
        committee: CommitteeId,
        #[serde(flatten)]
        request: senate_core::api::SetCommitteeSeat,
    },
    JoinAs {
        session: SessionSelector,
        committee: CommitteeId,
        senator: SenatorName,
    },
    /// What the committee knows: its memory points matching `query`
    /// (empty: the newest), only those in force unless `all`.
    Memory {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
    },
    /// The person corrects (`text` set) or deletes memory points, in their
    /// own name.
    MemoryEdit {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        targets: Vec<Seq>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    Search {
        session: SessionSelector,
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
    },
    Post {
        session: SessionSelector,
        body: String,
        /// A decision proposal rather than a plain note.
        #[serde(default, skip_serializing_if = "is_false")]
        proposal: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        /// Without a session: which of this bridge's agents posts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<Name>,
        /// The note's event id; a retry resends the same one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// Set a seat's duty on a committee ([`senate_core::duty`]): by
    /// default the session's own; `senator` names another seat (the
    /// committee's chair, or its owner with `as_person`). An empty `duty`
    /// clears it.
    Duty {
        session: SessionSelector,
        duty: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        senator: Option<SenatorName>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        /// As the person rather than the session's agent.
        #[serde(default, skip_serializing_if = "is_false")]
        as_person: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    Brief {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
    },
    Bound {
        session: SessionSelector,
    },
    /// The agent reports on a task thread (MCP `status`). The daemon sets
    /// `session` to the calling session, and the thread's owner to it and
    /// its agent.
    Task {
        session: SessionSelector,
        thread: Seq,
        status: StatusPayload,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        /// Without a session: which of this bridge's agents reports.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<Name>,
        /// Report as `senator::<person>` (e.g. a person closing the thread).
        #[serde(default, skip_serializing_if = "is_false")]
        as_person: bool,
    },
    /// The agent hands a received request to a person (MCP `escalate`);
    /// `to` (a person's handle) defaults to this bridge's own person.
    Escalate {
        session: SessionSelector,
        thread: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<Name>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// The developer takes a thread over (`/senate take`), as
    /// `senator::<person>`.
    Take {
        session: SessionSelector,
        thread: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
    },
    /// The person speaks (`senate ask`, `tell`, `request`, `reply`): a
    /// `question`, `note` or `request` written as `senator::<person>`.
    Say {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        kind: SayKind,
        /// Empty with `thread`: whoever took part in the thread.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        to: Vec<Address>,
        /// A qualified name instead of `to`: a seat on another committee
        /// (or Senate). The daemon resolves it, nearest first, and the
        /// person's message is delivered there directly.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<senate_core::Locator>,
        body: String,
        /// Any seq of the thread to speak in; the event goes in its root's
        /// thread.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<Seq>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// `senate chat send`: a simulated chat message
    /// ([`senate_core::chat_sim`]), from the person, or refused when the
    /// command runs in a coding session (the daemon names that session's
    /// agent as the caller, and the server refuses agents).
    ChatSend {
        #[serde(default)]
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        /// Its `by` is the daemon's to fill in.
        request: senate_core::chat_sim::SimulateChat,
    },
    /// `senate chat thread`: a simulated conversation by any of its refs.
    ChatThread {
        #[serde(default)]
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        message_ref: String,
    },
    /// The events of one thread, root first.
    Thread {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        /// Any seq in the thread.
        thread: Seq,
        /// The person is reading it (`senate thread`): they have seen it
        /// up to its last event.
        #[serde(default, skip_serializing_if = "is_false")]
        mark_seen: bool,
    },
    /// The person has seen these threads up to these seqs (`senate inbox
    /// --mark-read`, the app). Marks only move up; read state is this
    /// machine's, and nothing is appended to the log.
    Seen {
        committee: CommitteeId,
        marks: Vec<ThreadSeen>,
    },
    /// Open task threads that wait on this agent or this person (`/senate
    /// inbox`). Without a session, those of `agent` when given, else of
    /// every agent this bridge holds a stream for on the committee.
    Inbox {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<Name>,
        /// Also list settled threads (see [`senate_core::ThreadSummary::waits`]).
        #[serde(default, skip_serializing_if = "is_false")]
        all: bool,
    },
    /// Whether the session's agent presides over a Senate here.
    Presides {
        session: SessionSelector,
        #[serde(default)]
        runtime: Runtime,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_env: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_file: Option<String>,
    },
    /// What awaits the Speaker of the Senate the session presides over.
    DeskItem {
        committee: CommitteeId,
        seq: Seq,
    },
    DeskDecide {
        committee: CommitteeId,
        seq: Seq,
        item_id: EventId,
        id: EventId,
        decision: DeskDecision,
    },
    Docket {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
    },
    /// Relay an item awaiting the Speaker to one agent.
    Route {
        session: SessionSelector,
        committee: CommitteeId,
        seq: Seq,
        to: SenatorName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_committee: Option<CommitteeId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// Answer a question to an away agent on its behalf, or one for a
    /// child Senate that is offline on the child's.
    Cover {
        session: SessionSelector,
        committee: CommitteeId,
        thread: Seq,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<SenatorName>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// Tell the one who asked a child Senate that is offline that the
    /// question is held for it.
    Hold {
        session: SessionSelector,
        committee: CommitteeId,
        seq: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// Put an item awaiting the Speaker to the person.
    Refer {
        session: SessionSelector,
        committee: CommitteeId,
        seq: Seq,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// The Speaker's office gives a senator an alias on a committee (the
    /// `speaker_alias` tool). The agent keeps its owner.
    SpeakerAlias {
        session: SessionSelector,
        committee: CommitteeId,
        /// The senator: its primary id, or an alias it has there.
        senator: SenatorName,
        alias: SenatorName,
    },
    /// The Speaker's office removes a senator's seat on a committee (the
    /// `speaker_unseat` tool), at the person's explicit request.
    SpeakerUnseat {
        session: SessionSelector,
        committee: CommitteeId,
        /// The senator: its primary id, or an alias it has there.
        senator: SenatorName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// Pass a desk item meant for another committee on to that seat.
    Forward {
        session: SessionSelector,
        committee: CommitteeId,
        seq: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// Send a desk item back to its sender with the reason.
    Refuse {
        session: SessionSelector,
        committee: CommitteeId,
        seq: Seq,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// Make `senator` the chair of `committee` (default: the session's).
    /// With a session, for the Speaker's office; without one, as the person.
    SetChair {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        senator: SenatorName,
    },
    /// What awaits the chair of the session's committee.
    ChairDocket {
        session: SessionSelector,
    },
    /// The chair routes an item to one member.
    ChairRoute {
        session: SessionSelector,
        seq: Seq,
        to: SenatorName,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// The chair answers for an absent member from its own checkpoint.
    ChairCover {
        session: SessionSelector,
        thread: Seq,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<SenatorName>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// A secretary answers for the agent it is the secretary of, which is
    /// away, from that agent's own checkpoint and notes: a question only,
    /// in the agent's name (`on_behalf_of`), signed by the secretary.
    SecretaryCover {
        session: SessionSelector,
        thread: Seq,
        body: String,
        /// Which agent it answers for, when the question went to several.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<SenatorName>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// The chair puts an item one step up: to the Speaker's desk.
    ChairEscalate {
        session: SessionSelector,
        seq: Seq,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// An agent writes: to addresses on its committee, or to a qualified
    /// name (`at`).
    Send {
        session: SessionSelector,
        kind: SayKind,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        to: Vec<Address>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<senate_core::Locator>,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refs: Option<Refs>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// List, add or remove a senator's secretaries.
    Secretaries {
        session: SessionSelector,
        action: SecretaryOp,
        /// Whose: default the session's agent, or without a session the
        /// person.
        #[serde(default, rename = "for", skip_serializing_if = "Option::is_none")]
        for_: Option<SenatorName>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        secretary: Option<SenatorName>,
    },
    /// The person's appointment of a specific registered native session.
    SetSecretarySession {
        #[serde(default)]
        target_session: Option<String>,
        #[serde(default)]
        senator: Option<SenatorName>,
        #[serde(default)]
        principal: Option<SenatorName>,
        appointed: bool,
    },
    /// The Speaker's desk workers (its secretaries, in order), from the
    /// person's command line: read, or with `set` replaced in order. Not
    /// the secretary team ([`Request::SetSecretarySession`]).
    Desk {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        set: Option<Vec<SenatorName>>,
    },
    /// Explicit team work as this session, or as the person without a selector.
    SecretaryWork {
        session: SessionSelector,
        #[serde(default)]
        principal: Option<SenatorName>,
        operation: senate_core::secretary_work::SecretaryWorkOp,
    },
    /// The Speaker's office, the chairs, the child Senates.
    Roles,
    /// Codex approval requests waiting for an answer.
    Approvals,
    /// Answer a Codex approval request; the first answer wins.
    Approve {
        id: String,
        decision: Decision,
    },
    /// Answer the permission prompt waiting in a Senate-hosted session's
    /// terminal, as the person would there ([`ApproveSessionReply`]):
    /// Codex through its app-server, Claude Code by the host typing the
    /// chosen option of its permission dialog. The daemon refuses it from
    /// inside a coding session's process tree (see
    /// `daemon::approval_guard`).
    ApproveSession {
        senator: AliasName,
        /// `allow` (once), `allow_for_session` (always) or `deny`.
        decision: Decision,
    },
    /// Stream [`WatchEvent`]s on this connection until it closes. `app`:
    /// the client shows notifications itself (the desktop app), so the
    /// daemon posts no desktop notification while it is connected.
    Watch {
        #[serde(default, skip_serializing_if = "is_false")]
        app: bool,
    },
    /// Leave a checkpoint of the session's worktree (MCP `checkpoint`,
    /// `/senate handoff`).
    Checkpoint {
        session: SessionSelector,
        /// One line for the event's body.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// New contents of `.senate/PROGRESS.md`; `None` keeps the file.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decisions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open: Option<String>,
        /// Push the branch first (`git push origin HEAD:<branch>`).
        #[serde(default, skip_serializing_if = "is_false")]
        push: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
}

/// How far the person has seen one thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSeen {
    /// The thread's root seq.
    pub thread: Seq,
    /// The highest seq of it they have seen.
    pub seen: Seq,
}

/// What a person says with `say`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SayKind {
    /// Asks for an answer.
    Question,
    /// Says something; no answer expected.
    Note,
    /// Asks the other side to do something.
    Request,
}

impl SayKind {
    pub fn event_kind(self) -> Kind {
        match self {
            Self::Question => Kind::Question,
            Self::Note => Kind::Note,
            Self::Request => Kind::Request,
        }
    }
}

/// What `secretaries` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretaryOp {
    List,
    Add,
    Remove,
}

/// A senator's secretaries after `secretaries`, and every relation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretariesReply {
    /// Whose list was read or changed.
    pub senator: SenatorName,
    /// Its secretaries, in order.
    pub secretaries: Vec<SenatorName>,
    /// Every secretary relation of the Senate.
    #[serde(default)]
    pub all: senate_core::api::Secretaries,
}

/// The Speaker's desk after `desk`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskReply {
    /// The Speaker, a senator of this Senate.
    pub speaker: SenatorName,
    /// Its secretaries, in order: the desk workers set with `desk set`.
    pub secretaries: Vec<SenatorName>,
    /// Who works the desk as the server has it, in order: the Speaker
    /// itself first when it is an agent, then its secretaries (and the
    /// built-in session when it runs). The first present acts.
    pub desk: Vec<SenatorName>,
}

/// Where an agent's message went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendReply {
    #[serde(flatten)]
    pub posted: Posted,
    /// The qualified destination of a crossing, including direct chair discussions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intended: Option<senate_core::Locator>,
    /// The Speaker whose desk it was put to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<senate_core::SenatorId>,
    /// The Senate whose desk it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senate: Option<senate_core::SenatePath>,
}

impl SendReply {
    /// Put to the Speaker's desk rather than delivered.
    pub fn on_the_desk(&self) -> bool {
        self.posted.to.contains(&Address::Speaker) && self.intended.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeskDecision {
    Forward,
    Refuse {
        reason: String,
    },
    /// Answer it as the Speaker, in its thread, to whoever asked
    /// (Markdown). Takes it off the desk.
    Answer {
        body: String,
    },
    /// Close it as the Speaker with no answer: an archived status in its
    /// thread, with why when given. Takes it off the desk.
    Close {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

/// What a person may do with a desk item ([`DeskItemDetail::actions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeskAction {
    Forward,
    Refuse,
    Answer,
    Close,
}

/// An outcome stored on the original request's audit event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeskOutcome {
    Delivered {
        destination: String,
        committee: CommitteeId,
        seq: Seq,
    },
    Refused {
        destination: String,
        committee: CommitteeId,
        #[serde(default)]
        seq: Seq,
    },
    /// Answered as the Speaker: the answer's seq.
    Answered { committee: CommitteeId, seq: Seq },
    /// Closed as the Speaker: the archived status's seq.
    Closed {
        committee: CommitteeId,
        seq: Seq,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeskItemDetail {
    pub item: Event,
    pub events: Vec<Event>,
    pub origin: Option<String>,
    pub destination: Option<String>,
    /// Why forwarding (the oldest decision) is not possible, as before;
    /// [`DeskItemDetail::actions`] says what is.
    pub blocked: Option<String>,
    /// Exactly what this person may do with the item now.
    #[serde(default)]
    pub actions: Vec<DeskAction>,
    /// When nobody here can act: on whom or what it waits, ready to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub waits_on: Option<String>,
}

/// A desk item passed on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forwarded {
    /// The new root at the intended seat.
    #[serde(flatten)]
    pub posted: Posted,
    /// The note in the asker's thread.
    pub note: Posted,
    /// Where it went. Named apart from the flattened root's `to` (its
    /// recipients), which it would otherwise collide with.
    #[serde(rename = "forwarded_to")]
    pub to: senate_core::Locator,
}

/// Where `escalate` handed a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "to", rename_all = "snake_case")]
pub enum EscalatedTo {
    /// A person (named, the chair, or the Speaker).
    Person { person: Name },
    /// The committee's chair, an agent.
    Chair { chair: SenatorName },
    /// The Speaker's desk.
    Desk,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EscalateReply {
    #[serde(flatten)]
    pub posted: Posted,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub went: Option<EscalatedTo>,
}

/// What awaits the chair of one committee.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChairDocketReply {
    pub chair: SenatorName,
    pub page: senate_core::DocketPage,
    /// The latest checkpoint of every absent member an item names.
    #[serde(default)]
    pub checkpoints: Vec<senate_core::CheckpointRecord>,
}

/// The Speaker's office, the chairs and the child Senates, as `senate
/// status` shows them.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RolesReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senate: Option<senate_core::SenatePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub office: Option<senate_core::Office>,
    /// The desk's workers with a live session on this machine.
    #[serde(default)]
    pub present: Vec<SenatorName>,
    /// Each committee's chair.
    #[serde(default)]
    pub chairs: Vec<senate_core::api::ChairInfo>,
    #[serde(default)]
    pub children: Vec<senate_core::api::ChildSenate>,
    /// The Senate above, when this machine's Senate is a child of it: its
    /// address, its path and whether the registration was taken.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uplink: Option<UplinkStatus>,
}

/// The machine's Senate registered as a child of the Senate above, as its
/// uplink last found it. `senated --machine` writes it to
/// [`UplinkStatus::FILE`] in the hosted Senate's directory on every
/// (re)connect; `senate status` reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UplinkStatus {
    /// The Senate above's address.
    pub url: String,
    /// The Senate above's path, when it answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<senate_core::SenatePath>,
    /// The name the machine's Senate registers under.
    pub name: Name,
    /// The Speaker it registers with.
    pub speaker: senate_core::SenatorId,
    pub registration: Registration,
    pub at: DateTime<Utc>,
}

/// Where a child Senate's registration with the Senate above stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Registration {
    /// Not tried yet.
    Pending,
    /// Taken: the Senate above lists it among its children.
    Registered,
    /// The Senate above refused it, and why.
    Refused { reason: String },
    /// The Senate above did not answer.
    Unreachable { error: String },
}

impl UplinkStatus {
    /// Its file in the hosted Senate's directory.
    pub const FILE: &'static str = "uplink.json";

    /// Write it to `dir` (best effort: status is informational).
    pub fn write(&self, dir: &Path) {
        let path = dir.join(Self::FILE);
        let text = serde_json::to_string_pretty(self).unwrap_or_default();
        if let Err(e) = std::fs::write(&path, text) {
            tracing::debug!("{}: {e}", path.display());
        }
    }

    /// Where the registration stands now.
    pub fn now(
        url: &str,
        parent: Option<senate_core::SenatePath>,
        name: Name,
        speaker: senate_core::SenatorId,
        registration: Registration,
    ) -> Self {
        Self {
            url: url.to_owned(),
            parent,
            name,
            speaker,
            registration,
            at: Utc::now(),
        }
    }

    /// What was last written to `dir`, if anything readable.
    pub fn read(dir: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join(Self::FILE)).ok()?;
        serde_json::from_str(&text).ok()
    }
}

/// One error in the built-in session's `secretary.toml` or
/// `<committee>.secretary.toml`, as `senate status` lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretaryProblem {
    pub file: PathBuf,
    /// 1-based, when it is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// In the machine's language.
    pub message: String,
}

/// The errors the built-in session's configuration files have now.
/// `senated --machine` rewrites [`SecretaryProblems::FILE`] in the hosted
/// Senate's directory when they change; `senate status` reads it, with the
/// daemon up or down.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretaryProblems {
    pub problems: Vec<SecretaryProblem>,
}

impl SecretaryProblems {
    /// Its file in the hosted Senate's directory.
    pub const FILE: &'static str = "secretary-problems.json";

    /// Write it to `dir` (best effort: status is informational); an empty
    /// one removes the file.
    pub fn write(&self, dir: &Path) {
        let path = dir.join(Self::FILE);
        let result = if self.problems.is_empty() {
            match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        } else {
            std::fs::write(
                &path,
                serde_json::to_string_pretty(self).unwrap_or_default(),
            )
        };
        if let Err(e) = result {
            tracing::debug!("{}: {e}", path.display());
        }
    }

    /// What was last written to `dir`: none when there is no file.
    pub fn read(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(Self::FILE))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

/// One thread's events, root first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadReply {
    pub committee: CommitteeId,
    pub root: Seq,
    pub events: Vec<Event>,
}

/// What `presides` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresidesReply {
    /// The session's agent, registered or worked out.
    #[serde(default)]
    pub agent: Option<SenatorName>,
    /// The Senate it presides over, if it does.
    #[serde(default)]
    pub presiding: Option<Presiding>,
    /// The committees its agent chairs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chairs: Vec<CommitteeId>,
}

/// How a session's agent works the Speaker's desk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeskRole {
    /// It is the Speaker (an agent Speaker).
    #[default]
    Speaker,
    /// It is one of the Speaker's secretaries.
    Secretary,
}

/// A Senate a session presides over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presiding {
    /// The presiding agent: the one working the Speaker's desk.
    pub speaker: SenatorName,
    /// As the Speaker itself, or as a secretary.
    #[serde(default)]
    pub role: DeskRole,
    /// The Speaker, as a senator id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub office: Option<senate_core::SenatorId>,
    /// That Senate's name, as `/v1/me` gave it.
    #[serde(default)]
    pub senate: Option<Name>,
    /// The agents seated there.
    #[serde(default)]
    pub seated: Vec<SenatorName>,
}

/// One Senate's docket on each of its committees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SenateDocket {
    /// The Senate's name, as `/v1/me` gave it.
    #[serde(default)]
    pub senate: Option<Name>,
    pub pages: Vec<senate_core::DocketPage>,
    /// The latest checkpoint of every away agent an item names: what the
    /// Speaker covers from.
    #[serde(default)]
    pub checkpoints: Vec<senate_core::CheckpointRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocketReply {
    /// The presiding agent reading it; `None` for a person's read.
    #[serde(default)]
    pub speaker: Option<SenatorName>,
    pub senates: Vec<SenateDocket>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalsReply {
    /// Oldest first.
    pub approvals: Vec<Approval>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApproveReply {
    pub approval: Approval,
    pub decision: Decision,
}

/// A hosted session's permission prompt answered ([`Request::ApproveSession`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveSessionReply {
    pub senator: SenatorName,
    pub committee: CommitteeId,
    pub runtime: Runtime,
    /// What the prompt asked about, as the runtime said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub decision: Decision,
    /// The log note is queued after an uncertain append.
    #[serde(default, skip_serializing_if = "is_false")]
    pub audit_pending: bool,
}

/// One line of a `watch` stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WatchEvent {
    /// The daemon's state: first, then after every sweep (30 s).
    Status(Box<StatusReply>),
    /// A Codex approval is waiting.
    ApprovalRequested(Box<Approval>),
    /// It is answered (`by`: `bridge`, `elsewhere` — the TUI —, or `gone`).
    ApprovalResolved {
        id: String,
        session_id: String,
        by: ResolvedBy,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        decision: Option<Decision>,
    },
    /// An event arrived that may change an inbox (a question, request,
    /// answer or status): read the inbox again.
    Inbox {
        /// The agent whose stream carried it.
        agent: SenatorName,
        committee: CommitteeId,
        seq: Seq,
        kind: Kind,
    },
    /// An event arrived that is news for the person: addressed to them
    /// (an answer, a note, a question) or an escalation to them. What
    /// `senate inbox --follow` prints. Each event is announced once, whichever
    /// agent's stream carried it.
    ForPerson {
        /// The event itself.
        news: Box<Event>,
        /// An escalation (a `status`): `text` is its reason.
        escalated: bool,
        /// The body, or an escalation's reason.
        text: String,
    },
    /// The Speaker's office or a committee's chair is another one now, as
    /// this side knows them: what a session may do (`presides`, and so the
    /// tools it is given) may have changed with it. Ask again.
    Roles,
    /// This machine's hosted sessions changed: a host started or ended, or a
    /// permission prompt opened or closed in one. Read `sessions` again.
    Sessions,
    /// The watcher fell behind and missed `missed` events: read everything
    /// again.
    Lagged { missed: u64 },
}

/// What a `checkpoint` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointReply {
    pub posted: Posted,
    pub branch: String,
    pub commit: String,
    /// The branch's remote upstream contains the commit.
    pub pushed: bool,
    /// Files this call wrote.
    #[serde(default)]
    pub written: Vec<String>,
    /// Files present in the checkpoint.
    #[serde(default)]
    pub files: Vec<String>,
    /// Some file was cut to fit the cap.
    #[serde(default)]
    pub truncated: bool,
    /// `git push`'s summary, when asked to push and it worked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_output: Option<String>,
    /// Why the push was refused or failed; the checkpoint was written anyway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxReply {
    pub committee: CommitteeId,
    /// The agents whose inbox this is: the calling session's one.
    pub agents: Vec<SenatorName>,
    pub person: Name,
    /// Not archived, addressed to one of `agents` or escalated to `person`,
    /// longest in their state first. Without `all` only those that wait on
    /// someone ([`ThreadSummary::waits`]); with it settled ones too.
    pub threads: Vec<ThreadSummary>,
    /// Settled threads left out of `threads` (always 0 with `all`).
    #[serde(default)]
    pub settled: usize,
    /// How far the person has seen each thread of the committee they have
    /// seen any of (a thread not listed: nothing yet). The input of
    /// [`ThreadSummary::waits_on_person`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seen: Vec<ThreadSeen>,
}

impl InboxReply {
    /// The highest seq of `root` the person has seen.
    pub fn seen_of(&self, root: Seq) -> Seq {
        self.seen
            .iter()
            .find(|m| m.thread == root)
            .map_or(Seq::ZERO, |m| m.seen)
    }

    /// Whether `thread` waits on the person, and why: the person's rule
    /// ([`ThreadSummary::waits_on_person`]) with what they have seen.
    pub fn waits_on_person<'a>(
        &self,
        thread: &'a ThreadSummary,
    ) -> Option<senate_core::PersonWait<'a>> {
        thread.waits_on_person(&self.person, self.seen_of(thread.root))
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pong {
    pub protocol: u32,
    pub pid: u32,
    pub person: Name,
    pub locale: Locale,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HookReply {
    pub session: Session,
    /// Text for `hookSpecificOutput.additionalContext`, if any.
    #[serde(default)]
    pub context: Option<String>,
    /// Queue items this reply hands over.
    pub delivered: usize,
}

/// An event the daemon appended on the agent's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Posted {
    pub committee: CommitteeId,
    pub seq: Seq,
    pub to: Vec<Address>,
    #[serde(default)]
    pub thread: Option<Seq>,
    pub duplicate: bool,
    /// The person spoke in a thread and an escalation to them is still in
    /// force that they had not been shown when they spoke (the two crossed):
    /// its seq. Only on `say` and a person's `answer`; the reply left it
    /// new in their inbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unseen_escalation: Option<Seq>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatchUpReply {
    pub committee: CommitteeId,
    /// The agent that caught up: `senator::<agent>`.
    pub me: Address,
    /// The Senate's people: every other senator is an agent.
    #[serde(default)]
    pub people: senate_core::People,
    /// The agents run by the same person as `me`.
    #[serde(default)]
    pub own: Vec<SenatorName>,
    /// The committee's chair, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chair: Option<SenatorName>,
    /// The person `me` runs under: what they write is the user's own word
    /// ([`crate::format::Reader::user`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<SenatorName>,
    /// Directed first, then the rest, each group in seq order.
    pub items: Vec<CatchUpItem>,
    /// More events remain after this page.
    pub has_more: bool,
    /// The `read` cursor before and after this call.
    pub read_before: Seq,
    pub read: Seq,
    /// Some directed item here was in flight when the daemon last stopped
    /// and had not been recorded delivered: the agent may have seen it
    /// ([`crate::format::redelivered_note`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redelivered: bool,
}

impl CatchUpReply {
    /// How `me` reads these events ([`crate::format::Reader`]).
    pub fn reader(&self) -> crate::format::Reader {
        let reader = crate::format::Reader::new(
            self.me.clone(),
            self.people.clone(),
            self.own.iter().cloned().collect(),
        );
        let reader = match &self.user {
            Some(user) => reader.with_user(user.clone()),
            None => reader,
        };
        match &self.chair {
            Some(chair) => reader.with_chair(self.committee.clone(), chair.clone()),
            None => reader,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalDelivery {
    pub agent: SenatorName,
    pub state: crate::state::ItemStatus,
    pub via: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventReceipt {
    pub committee: CommitteeId,
    pub seq: Seq,
    pub id: EventId,
    pub thread: Seq,
    pub kind: Kind,
    pub from: Address,
    pub to: Vec<Address>,
    pub local_deliveries: Vec<LocalDelivery>,
}

impl EventReceipt {
    pub fn render(&self, locale: Locale) -> String {
        let mut lines = vec![match locale {
            Locale::Zh => format!(
                "{} 已记录 {}，seq {}，thread {}。",
                self.committee, self.kind, self.seq, self.thread
            ),
            Locale::En => format!(
                "{} recorded {} at seq {} in thread {}.",
                self.committee, self.kind, self.seq, self.thread
            ),
        }];
        if self.local_deliveries.is_empty() {
            lines.push(
                locale
                    .pick(
                        "此桥接器没有对应的投递记录。",
                        "This bridge has no delivery record for this event.",
                    )
                    .to_owned(),
            );
        }
        for delivery in &self.local_deliveries {
            let state = delivery.state.as_str();
            let agent = Address::senator(&delivery.agent).in_text();
            let via = delivery
                .via
                .as_deref()
                .map(|v| format!(" ({v})"))
                .unwrap_or_default();
            lines.push(format!("{agent}  {state}{via}"));
        }
        lines.join("\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinReply {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<SenatorName>,
    /// The agent the session speaks as.
    pub agent: SenatorName,
    pub committee: CommitteeId,
    #[serde(default)]
    pub previous: Option<CommitteeId>,
    /// Set when the server could not confirm the committee.
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryReply {
    pub committee: CommitteeId,
    pub query: String,
    /// Best match first; the newest first without a query.
    pub points: Vec<senate_core::MemoryHit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchReply {
    pub committee: CommitteeId,
    pub query: String,
    /// Best match first, as the server ranks them.
    pub events: Vec<Event>,
}

/// What [`Request::DeskOpenCommittee`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeskOpenReply {
    pub committee: CommitteeId,
    /// The opening note's seq; `None` when an earlier call had opened it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<Seq>,
    /// The chair started now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chair: Option<SpawnReply>,
    /// The chair an earlier call had already started: nothing new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_chair: Option<SenatorName>,
}

/// What [`Request::FindSession`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindReply {
    /// The session, as a command names it (`committee::alias`).
    pub name: String,
    /// Whether a terminal is attached to it: it now rings and flashes.
    pub attached: bool,
    /// That terminal's device, when it said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
}

/// The machine's default secretary ([`Request::DefaultSecretary`],
/// [`Request::EnsureSecretary`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultSecretary {
    /// Where it sits: `office`.
    pub committee: CommitteeId,
    /// Its alias there: `secretary`.
    pub alias: SenatorName,
    /// Its senator, once there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senator: Option<SenatorName>,
    pub state: DefaultSecretaryState,
    /// What an ensure did; absent for [`Request::DefaultSecretary`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<EnsureAction>,
}

/// Where the default secretary stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefaultSecretaryState {
    /// None yet: the first ask starts one.
    NotRecorded,
    Running,
    /// Paused: the next ask resumes it.
    Paused,
    /// Its terminal ended without anyone stopping it (a restart): the next
    /// ask restores it.
    Ended,
    /// Stopped on purpose: only `senate session restore` brings it back.
    Stopped,
    /// Its seat has no session on this machine.
    Elsewhere,
}

impl DefaultSecretaryState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotRecorded => "not recorded",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Ended => "ended",
            Self::Stopped => "stopped on purpose",
            Self::Elsewhere => "elsewhere",
        }
    }
}

/// What [`Request::EnsureSecretary`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnsureAction {
    Started,
    Resumed,
    Restored,
    AlreadyRunning,
}

/// What [`Request::OpenCommittee`] did: the note's seq when it opened the
/// committee, `None` when the server already had it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenCommitteeReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opened: Option<Seq>,
}

/// The committee a session is bound to, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundReply {
    pub session_id: String,
    #[serde(default)]
    pub committee: Option<CommitteeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
}

/// One committee stream's state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum StreamState {
    Connecting {
        attempt: u32,
    },
    Connected {
        since: DateTime<Utc>,
    },
    Backoff {
        attempt: u32,
        retry_at: DateTime<Utc>,
        error: String,
    },
    Stopped,
}

/// One page ordered by decreasing last activity sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteeThreadPage {
    pub rows: Vec<CommitteeThread>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteeThread {
    pub committee: CommitteeId,
    pub root: Seq,
    pub title: String,
    pub kind: String,
    pub opener: String,
    pub addressees: Vec<String>,
    pub owner: Option<String>,
    pub state: String,
    pub waits_on: Option<String>,
    pub escalated_to: Option<String>,
    pub takeover: Option<String>,
    pub last_seq: Seq,
    pub last_at: Option<DateTime<Utc>>,
    pub last_author: Option<String>,
    pub message_count: Option<u64>,
    pub other_committee: Option<CommitteeId>,
}

/// A bounded batch. A reset ends the stream and asks the caller to refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityBatch {
    pub events: Vec<ActivityEntry>,
    /// Initial resume positions. Later batches advance these via event seqs.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub cursors: std::collections::BTreeMap<CommitteeId, Seq>,
    pub reset: bool,
}

/// Log previews. Kinds include chat, binding, question, answer, request,
/// note, decision, checkpoint, status, escalation, presence, joined, left, state,
/// chair_changed, session_started, session_stopped, and unknown. State
/// describes the thread on status/escalation and the seat on lifecycle events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityEntry {
    pub committee: CommitteeId,
    pub seq: Seq,
    pub at: DateTime<Utc>,
    pub kind: String,
    pub from: String,
    pub to: Vec<String>,
    pub on_behalf_of: Option<String>,
    pub thread: Option<Seq>,
    pub line: String,
    pub state: Option<String>,
}

/// A committee as shown in the Senate roster.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterCommittee {
    pub committee: CommitteeId,
    pub chair: Option<SenatorName>,
    pub chair_display_name: Option<String>,
    pub chair_live: bool,
    pub members: Vec<RosterMember>,
    pub pending: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterMember {
    pub senator: SenatorName,
    pub display_name: String,
    pub active: bool,
    #[serde(default)]
    pub runtime: Option<Runtime>,
    #[serde(default)]
    pub session_state: Option<String>,
    #[serde(default)]
    pub status_title: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

impl RosterCommittee {
    /// How many members are active in this snapshot.
    pub fn active_members(&self) -> usize {
        self.members.iter().filter(|member| member.active).count()
    }
}

/// One agent's stream on one committee.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitteeStatus {
    pub agent: SenatorName,
    pub committee: CommitteeId,
    pub delivered: Seq,
    pub reported: Seq,
    /// `None` when no stream is open for it (the daemon is down, or the
    /// committee is no longer joined).
    #[serde(default)]
    pub stream: Option<StreamState>,
    /// Listed in `senate.toml`.
    pub configured: bool,
    /// Live sessions of the agent bound to it.
    pub sessions: usize,
    /// The agent's duty on the committee, as the committee's presence and
    /// log last said; `None`: it has none, or it is not known yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<String>,
}

impl CommitteeStatus {
    pub fn is_active(&self) -> bool {
        let presence = matches!(self.stream, Some(StreamState::Connected { .. }))
            .then_some(senate_core::PresenceState::Online);
        senate_core::continuity::roster_active(presence, self.sessions > 0, false)
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ServerHealthReply {
    pub me: senate_core::api::Me,
    pub ahead: Vec<(CommitteeId, Seq)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReply {
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub host_health: std::collections::BTreeMap<String, HostHealth>,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptors: Option<crate::fd::Snapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub person_guard_advisory: Option<String>,
    pub version: String,
    pub person: Name,
    /// The Senate this bridge serves, by its path (`acme/north-desk`), as
    /// `/v1/me` gave it; `None` until the server answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senate: Option<senate_core::SenatePath>,
    /// That Senate's people, as `/v1/me` named them: every other senator
    /// is an agent.
    #[serde(default)]
    pub people: senate_core::People,
    pub locale: Locale,
    pub server_url: String,
    pub committees: Vec<CommitteeStatus>,
    pub sessions: Vec<Session>,
    /// Agents whose hosted session is recorded paused (`senate session
    /// pause`).
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub paused: std::collections::BTreeSet<SenatorName>,
    /// Live terminal hosts by session ID, with the latest refused native ID if any.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub hosted: std::collections::BTreeMap<String, Option<String>>,
    /// The unbound native context that is holding a hosted session's delivery.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub hosted_unbound: std::collections::BTreeMap<String, String>,
    /// Running hosts without a readable viewer activity record.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub hosted_viewer_unavailable: std::collections::BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped_hooks: Option<HookDropStatus>,

    pub queue: QueueCounts,
    /// Who holds the Speaker's seat of that Senate, as `/v1/me` said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<senate_core::SpeakerSeat>,
    /// The Speaker's office (the Speaker as a senator id and its kind, the
    /// desk's workers and which are present here), each committee's chair,
    /// the child Senates, and the Senate above when this one is a child.
    /// Absent from an older daemon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<RolesReply>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct IpcError {
    pub code: String,
    pub message: String,
    /// With [`codes::AMBIGUOUS_RECIPIENT`]: the agents to choose from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<SenatorName>,
}

impl IpcError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            candidates: Vec::new(),
        }
    }

    /// The server refused a wildcard that names a person with several
    /// agents; the asker must name one of `candidates`.
    pub fn ambiguous(message: impl Into<String>, candidates: Vec<SenatorName>) -> Self {
        Self {
            code: codes::AMBIGUOUS_RECIPIENT.to_string(),
            message: message.into(),
            candidates,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(codes::BAD_REQUEST, message)
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(codes::INTERNAL, message.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<IpcError>,
}

impl Reply {
    pub fn ok<T: Serialize>(value: &T) -> Self {
        match serde_json::to_value(value) {
            Ok(v) => Self {
                ok: true,
                result: Some(v),
                error: None,
            },
            Err(e) => Self::err(IpcError::internal(e)),
        }
    }

    pub fn err(error: IpcError) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the Senate bridge daemon is not running (cannot connect to {path}): {source}")]
    Connect {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("the Senate bridge daemon did not answer in time")]
    Timeout,
    #[error("talking to the Senate bridge daemon: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected reply from the Senate bridge daemon: {0}")]
    Protocol(String),
    #[error(transparent)]
    Remote(IpcError),
}

/// Which caller proof a request carries on macOS, and whether the daemon
/// serves it at all without one (only `sessions`, which then leaves out the
/// person's view).
#[cfg(target_os = "macos")]
pub(crate) fn proof_kind(request: &Request) -> Option<(crate::mac_scope::ProofKind, bool)> {
    use crate::mac_scope::ProofKind;
    if crate::daemon::approval_guard::request_effect(request).requires_operator()
        || crate::daemon::approval_guard::person_only(request).is_some()
        || crate::daemon::approval_guard::operator_only(request)
        || matches!(
            request,
            Request::RemoteControl { .. }
                | Request::SessionDrop { .. }
                | Request::ActivityWatch { .. }
                | Request::TimelineWatch { .. }
                | Request::DeskDecide { .. }
                | Request::Sessions
                | Request::Watch { .. }
        )
    {
        Some((ProofKind::Operator, !matches!(request, Request::Sessions)))
    } else if matches!(
        request,
        Request::Hook(_) | Request::SessionHook(_) | Request::HostedSessionEnd { .. }
    ) || matches!(request, Request::SetCard { session, .. } if session != &SessionSelector::default())
        || crate::daemon::approval_guard::session_selector(request).is_some()
        || crate::daemon::approval_guard::session_id_claim(request).is_some()
    {
        // Sent without a proof when none is had: a session that predates
        // the per-request proofs is still served, advisorily.
        Some((ProofKind::Session, false))
    } else if crate::app_confirmation::required(request) {
        Some((ProofKind::Operator, true))
    } else {
        None
    }
}

/// Encode a request, with a one-time macOS scope proof for person operations.
/// A request the daemon only serves with a proof of its caller is not sent
/// without one: the error says at which stage the proof failed.
pub async fn request_line(socket: &Path, request: &Request) -> Result<Vec<u8>, ClientError> {
    let body = serde_json::to_vec(request).map_err(|e| ClientError::Protocol(e.to_string()))?;
    let mut frame = Vec::new();
    #[cfg(target_os = "macos")]
    if let Some((kind, required)) = proof_kind(request) {
        use crate::app_confirmation::macos::CallFailure;
        use crate::mac_scope::ProofFailure;
        match crate::mac_scope::prove(socket, &body, kind).await {
            Ok(ticket) => {
                frame.extend_from_slice(b"SCOPE1 ");
                frame.extend_from_slice(ticket.as_bytes());
                frame.push(b' ');
            }
            // An unsigned build has no proofs to give; its daemon decides.
            Err((ProofFailure::Call(CallFailure::NoTeam), _)) => {}
            Err((failure, service)) => {
                tracing::debug!(?failure, ?kind, "sending without a caller proof");
                if required {
                    return Err(ClientError::Remote(IpcError::new(
                        codes::NOT_A_PERSON,
                        failure.sentence(&service),
                    )));
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = socket;
    frame.extend(body);
    frame.push(b'\n');
    Ok(frame)
}

/// One connection to the daemon.
pub struct IpcClient {
    socket: PathBuf,
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl IpcClient {
    pub async fn connect(path: &Path, timeout: Duration) -> Result<Self, ClientError> {
        let stream = match tokio::time::timeout(timeout, UnixStream::connect(path)).await {
            Ok(Ok(s)) => s,
            Ok(Err(source)) => {
                return Err(ClientError::Connect {
                    path: path.to_path_buf(),
                    source,
                });
            }
            Err(_) => return Err(ClientError::Timeout),
        };
        let (r, w) = stream.into_split();
        Ok(Self {
            socket: path.to_path_buf(),
            reader: BufReader::new(r),
            writer: w,
        })
    }

    /// Send one request; decode its result as `T`.
    pub async fn call<T: DeserializeOwned>(
        &mut self,
        request: &Request,
        timeout: Duration,
    ) -> Result<T, ClientError> {
        let exchange = async {
            let line = request_line(&self.socket, request).await?;
            self.writer.write_all(&line).await?;
            self.read_reply().await
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    async fn read_reply<T: DeserializeOwned>(&mut self) -> Result<T, ClientError> {
        let reply = read_line(&mut self.reader)
            .await?
            .ok_or_else(|| ClientError::Protocol("connection closed".into()))?;
        let reply: Reply =
            serde_json::from_str(&reply).map_err(|e| ClientError::Protocol(e.to_string()))?;
        match (reply.ok, reply.result, reply.error) {
            (true, result, _) => serde_json::from_value(result.unwrap_or(Value::Null))
                .map_err(|e| ClientError::Protocol(e.to_string())),
            (false, _, Some(e)) => Err(ClientError::Remote(e)),
            (false, _, None) => Err(ClientError::Protocol("error without detail".into())),
        }
    }

    /// Upload one file over a dedicated local connection. The daemon
    /// checks the peer and metadata before accepting any file bytes.
    pub async fn session_drop(
        mut self,
        session_id: String,
        name: String,
        bytes: &[u8],
    ) -> Result<String, ClientError> {
        let size = u32::try_from(bytes.len())
            .map_err(|_| ClientError::Protocol("attachments are limited to 20 MiB".into()))?;
        let request = Request::SessionDrop {
            session_id,
            name,
            size,
        };
        self.call::<()>(&request, SESSION_DROP_TIMEOUT).await?;
        tokio::time::timeout(SESSION_DROP_TIMEOUT, async {
            self.writer.write_all(bytes).await?;
            self.read_reply().await
        })
        .await
        .map_err(|_| ClientError::Timeout)?
    }
}

/// An open `watch` connection: after its reply, [`WatchEvent`] lines.
pub struct WatchStream {
    reader: BufReader<OwnedReadHalf>,
    // Kept open: closing it is how the client stops watching.
    _writer: OwnedWriteHalf,
}

impl IpcClient {
    /// Turn this connection into a `watch` stream (`app`: it shows
    /// notifications itself).
    pub async fn watch(mut self, app: bool, timeout: Duration) -> Result<WatchStream, ClientError> {
        let _: Value = self.call(&Request::Watch { app }, timeout).await?;
        Ok(WatchStream {
            reader: self.reader,
            _writer: self.writer,
        })
    }
}

impl WatchStream {
    /// The next event; `None` when the daemon closed the stream. Lines this
    /// build does not know are skipped.
    pub async fn next(&mut self) -> Result<Option<WatchEvent>, ClientError> {
        loop {
            let Some(line) = read_line(&mut self.reader).await? else {
                return Ok(None);
            };
            if let Ok(event) = serde_json::from_str::<WatchEvent>(&line) {
                return Ok(Some(event));
            }
        }
    }
}

/// One request on a fresh connection.
pub async fn request<T: DeserializeOwned>(
    path: &Path,
    request: &Request,
    connect_timeout: Duration,
    timeout: Duration,
) -> Result<T, ClientError> {
    IpcClient::connect(path, connect_timeout)
        .await?
        .call(request, timeout)
        .await
}

/// A fresh ID for one spawn action, with its creation time for expiration.
pub(crate) fn new_spawn_attempt_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub(crate) fn spawn_outcome_unknown(attempt_id: &str, detail: impl std::fmt::Display) -> IpcError {
    IpcError::new(
        codes::SPAWN_OUTCOME_UNKNOWN,
        format!(
            "Spawn outcome unknown (attempt {attempt_id}): {detail}. \
             Check `senate session list`. To retry this spawn, reuse the ID with `--attempt-id` \
             in the CLI or `attempt_id` in MCP."
        ),
    )
}

/// Treat a lost or unreadable reply to a tracked spawn as ambiguous.
pub(crate) async fn request_spawn(
    path: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<SpawnReply, ClientError> {
    let mut client = IpcClient::connect(path, Duration::from_secs(1)).await?;
    match client.call(request, timeout).await {
        Err(error @ (ClientError::Timeout | ClientError::Io(_) | ClientError::Protocol(_)))
            if let Request::Spawn {
                attempt_id: Some(id),
                ..
            } = request =>
        {
            Err(ClientError::Remote(spawn_outcome_unknown(id, error)))
        }
        result => result,
    }
}

/// Read one `\n`-terminated line of at most [`MAX_LINE`] bytes; `None` at a
/// clean end of stream.
pub(crate) async fn read_line<R>(reader: &mut R) -> Result<Option<String>, ClientError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf = Vec::new();
    let limit = u64::try_from(MAX_LINE).unwrap_or(u64::MAX) + 1;
    let n = (&mut *reader)
        .take(limit)
        .read_until(b'\n', &mut buf)
        .await?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(ClientError::Protocol(if buf.len() > MAX_LINE {
            format!("line longer than {MAX_LINE} bytes")
        } else {
            "connection closed mid-line".into()
        }));
    }
    buf.pop();
    String::from_utf8(buf)
        .map(Some)
        .map_err(|e| ClientError::Protocol(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged_by_op() {
        let r: Request = serde_json::from_str(r#"{"op":"ping"}"#).unwrap();
        assert_eq!(r, Request::Ping);
        let r: Request = serde_json::from_str(
            r#"{"op":"answer","session":{"session_id":"s"},"thread":118,"body":"b","extra":1}"#,
        )
        .unwrap();
        assert_eq!(
            r,
            Request::Answer {
                session: SessionSelector {
                    session_id: Some("s".into()),
                    claude_pid: None
                },
                thread: Seq(118),
                body: "b".into(),
                id: None,
                committee: None,
                agent: None,
                as_person: false,
            }
        );
        let id = EventId::new();
        let ask = Request::Ask {
            session: SessionSelector::default(),
            to: vec![],
            body: "q".into(),
            refs: None,
            id: Some(id.clone()),
            request: false,
        };
        let json = serde_json::to_value(&ask).unwrap();
        assert_eq!(json["id"], id.to_string());
        assert_eq!(serde_json::from_value::<Request>(json).unwrap(), ask);
        let note = Request::Post {
            session: SessionSelector::default(),
            body: "n".into(),
            proposal: false,
            committee: None,
            agent: None,
            id: None,
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"op": "post", "session": {}, "body": "n"})
        );
        let r: Request = serde_json::from_str(
            r#"{"op":"post","session":{},"body":"p","proposal":true,"committee":"refund-v2"}"#,
        )
        .unwrap();
        assert!(
            matches!(&r, Request::Post { proposal: true, committee: Some(f), .. } if f.as_str() == "refund-v2"),
            "{r:?}"
        );
        let hook = Request::Hook(HookReport {
            event: HookEvent::SessionStart,
            session_id: "s".into(),
            cwd: "/w".into(),
            transcript_path: None,
            source: Some("startup".into()),
            branch: None,
            git_root: None,
            committee_file: None,
            socket: None,
            token: None,
            claude_pid: Some(1),
            ancestor_pids: Vec::new(),
            runtime: Runtime::Claude,
            agent_env: Some("reviewer".into()),
            agent_file: None,
            host: None,
        });
        let json = serde_json::to_value(&hook).unwrap();
        assert_eq!(json["op"], "hook");
        assert_eq!(json["event"], "session_start");
        assert_eq!(json["agent_env"], "reviewer");
        assert!(json.get("ancestor_pids").is_none(), "omitted when empty");
        assert!(json.get("agent_file").is_none(), "omitted when absent");
        assert_eq!(serde_json::from_value::<Request>(json).unwrap(), hook);
        let escalate: Request =
            serde_json::from_str(r#"{"op":"escalate","session":{},"thread":3,"to":"pm"}"#).unwrap();
        assert!(matches!(escalate, Request::Escalate { to: Some(p), .. } if p.as_str() == "pm"));
    }

    #[test]
    fn replies_carry_result_or_error() {
        let ok = serde_json::to_string(&Reply::ok(&serde_json::json!({"a": 1}))).unwrap();
        assert_eq!(ok, r#"{"ok":true,"result":{"a":1}}"#);
        let err = serde_json::to_string(&Reply::err(IpcError::bad_request("nope"))).unwrap();
        assert_eq!(
            err,
            r#"{"ok":false,"error":{"code":"bad_request","message":"nope"}}"#
        );
        let ambiguous = IpcError::ambiguous(
            "bob has several agents",
            vec!["claude".parse().unwrap(), "codex".parse().unwrap()],
        );
        let json = serde_json::to_value(Reply::err(ambiguous.clone())).unwrap();
        assert_eq!(json["error"]["code"], "ambiguous_recipient");
        assert_eq!(
            json["error"]["candidates"],
            serde_json::json!(["claude", "codex"])
        );
        let back: Reply = serde_json::from_value(json).unwrap();
        assert_eq!(back.error, Some(ambiguous));
    }

    #[tokio::test]
    async fn read_line_bounds_length() {
        let data = b"hello\nworld";
        let mut r = BufReader::new(&data[..]);
        assert_eq!(read_line(&mut r).await.unwrap().as_deref(), Some("hello"));
        assert!(read_line(&mut r).await.is_err());
        let mut empty = BufReader::new(&b""[..]);
        assert_eq!(read_line(&mut empty).await.unwrap(), None);
    }
}
