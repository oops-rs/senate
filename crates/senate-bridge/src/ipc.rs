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
//! either side may close between them.
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
//! | `session_end` | `session_id`, `reason?` | `{}` |
//! | `ask` | `session`, `to` (addresses), `body`, `refs?`, `id?`, `request?` | [`Posted`] |
//! | `answer` | `session`, `thread` (root seq), `body`, `id?` | [`Posted`] |
//! | `catch_up` | `session`, `limit?` | [`CatchUpReply`] |
//! | `join` | `session`, `committee` | [`JoinReply`] |
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
//! | `refer` | `session`, `committee`, `seq`, `reason`, `id?` | [`Posted`] |
//!
//! | `approvals` | — | [`ApprovalsReply`] |
//! | `approve` | `id`, `decision` | [`ApproveReply`] |
//! | `watch` | — | `{"watching": true}`, then a stream of [`WatchEvent`] lines |
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
//! `approvals` / `approve` / `watch` are for a desktop app.
//! `approvals` lists the Codex approval requests waiting
//! ([`crate::approvals`]); `approve` answers one (`decision`: `allow`,
//! `allow_for_session`, `deny`, `cancel`), and fails when it was answered
//! elsewhere first. `watch` turns the connection into a one-way stream:
//! after its reply, every line is a [`WatchEvent`], starting with a
//! `status` and an `approval_requested` per pending approval; the client
//! sends nothing more and closes the connection to stop.
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
//! Claude does not export `CLAUDE_PID`). A Codex session is selected by
//! its thread id alone: the MCP shim takes it from each tool call's
//! `_meta.threadId`, and Codex hooks report no pids (every thread of an
//! app-server shares the same ones).
//!
//! `ask` accepts an agent (`senator::bob/claude`) or whichever of a
//! person's agents should get it (`senator::bob/*`); the Speaker, the
//! committee, a person alone and guests are refused. When the server
//! resolves `senator::bob/*` and bob has several agents, the reply is the
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
use senate_core::{
    Address, CommitteeId, Event, EventId, Kind, Locale, Name, Refs, SenatorPath, Seq,
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
use crate::session::{Runtime, Session};
use crate::state::QueueCounts;

pub const PROTOCOL_VERSION: u32 = 1;
/// Longest accepted line, in bytes.
pub const MAX_LINE: usize = 1 << 20;

/// Error codes.
pub mod codes {
    /// The request was malformed or its arguments invalid.
    pub const BAD_REQUEST: &str = "bad_request";
    /// No registered session matches the selector.
    pub const NO_SESSION: &str = "no_session";
    /// The session is not bound to any committee.
    pub const NOT_JOINED: &str = "not_joined";
    /// A presiding op from a session whose agent holds no Speaker's seat.
    pub const NOT_PRESIDING: &str = "not_presiding";
    /// The Senate server refused or could not be reached.
    pub const SERVER: &str = "server";
    /// `senator::<person>/*` named a person with several agents;
    /// [`IpcError::candidates`](super::IpcError::candidates) lists them.
    pub const AMBIGUOUS_RECIPIENT: &str = senate_core::api::code::AMBIGUOUS_RECIPIENT;
    pub const INTERNAL: &str = "internal";
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

/// What a SessionStart / UserPromptSubmit hook saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// `CLAUDE_CODE_MESSAGING_SOCKET`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// `CLAUDE_CODE_MESSAGING_TOKEN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_pid: Option<u32>,
    /// The hook's parent, grandparent, ... (nearest first), reported when
    /// `CLAUDE_PID` is not set: Claude is one of them, so the MCP shim's
    /// parent pid can still find this session.
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Hook(HookReport),
    SessionEnd {
        session_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
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
    /// Open task threads that wait on this agent or this person (`/senate
    /// inbox`). Without a session, those of `agent` when given, else of
    /// every agent this bridge holds a stream for on the committee.
    Inbox {
        session: SessionSelector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<CommitteeId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<Name>,
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
        to: SenatorPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<EventId>,
    },
    /// Answer a question to an away agent on its behalf.
    Cover {
        session: SessionSelector,
        committee: CommitteeId,
        thread: Seq,
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<SenatorPath>,
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
    /// Codex approval requests waiting for an answer.
    Approvals,
    /// Answer a Codex approval request; the first answer wins.
    Approve {
        id: String,
        decision: Decision,
    },
    /// Stream [`WatchEvent`]s on this connection until it closes.
    Watch,
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

/// What `presides` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresidesReply {
    /// The session's agent, registered or worked out.
    #[serde(default)]
    pub agent: Option<SenatorPath>,
    /// The Senate it presides over, if it does.
    #[serde(default)]
    pub presiding: Option<Presiding>,
}

/// A Senate a session presides over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presiding {
    /// The presiding agent: the holder of the Speaker's seat.
    pub speaker: SenatorPath,
    /// That Senate, by its presider (`None`: a top Senate used alone).
    #[serde(default)]
    pub senate: Option<SenatorPath>,
    /// The agents seated there.
    #[serde(default)]
    pub seated: Vec<SenatorPath>,
}

/// One Senate's docket on each of its committees.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SenateDocket {
    /// The Senate, by its presider (`None`: a top Senate).
    #[serde(default)]
    pub senate: Option<SenatorPath>,
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
    pub speaker: Option<SenatorPath>,
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
        agent: SenatorPath,
        committee: CommitteeId,
        seq: Seq,
        kind: Kind,
    },
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
    /// The commit is on a remote-tracking branch.
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
    pub agents: Vec<SenatorPath>,
    pub person: Name,
    /// Not archived, addressed to one of `agents` or escalated to `person`,
    /// longest in their state first.
    pub threads: Vec<ThreadSummary>,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatchUpReply {
    pub committee: CommitteeId,
    /// The agent that caught up: `senator::<agent>`.
    pub me: Address,
    /// Directed first, then the rest, each group in seq order.
    pub items: Vec<CatchUpItem>,
    /// More events remain after this page.
    pub has_more: bool,
    /// The `read` cursor before and after this call.
    pub read_before: Seq,
    pub read: Seq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinReply {
    pub session_id: String,
    /// The agent the session speaks as.
    pub agent: SenatorPath,
    pub committee: CommitteeId,
    #[serde(default)]
    pub previous: Option<CommitteeId>,
    /// Set when the server could not confirm the committee.
    #[serde(default)]
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchReply {
    pub committee: CommitteeId,
    pub query: String,
    /// Best match first, as the server ranks them.
    pub events: Vec<Event>,
}

/// The committee a session is bound to, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundReply {
    pub session_id: String,
    #[serde(default)]
    pub committee: Option<CommitteeId>,
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

/// One agent's stream on one committee.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitteeStatus {
    pub agent: SenatorPath,
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReply {
    pub pid: u32,
    pub version: String,
    pub person: Name,
    /// The Senate this bridge is connected to, by its presider; `None` for
    /// the top Senate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presider: Option<SenatorPath>,
    pub locale: Locale,
    pub server_url: String,
    pub committees: Vec<CommitteeStatus>,
    pub sessions: Vec<Session>,
    pub queue: QueueCounts,
    /// Who holds the Speaker's seat of that Senate, as `/v1/me` said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<senate_core::SpeakerSeat>,
    /// The other Senates this machine serves, each with its own streams,
    /// sessions and queue.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub others: Vec<StatusReply>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct IpcError {
    pub code: String,
    pub message: String,
    /// With [`codes::AMBIGUOUS_RECIPIENT`]: the agents to choose from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<SenatorPath>,
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
    pub fn ambiguous(message: impl Into<String>, candidates: Vec<SenatorPath>) -> Self {
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

/// One connection to the daemon.
pub struct IpcClient {
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
            let mut line =
                serde_json::to_string(request).map_err(|e| ClientError::Protocol(e.to_string()))?;
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await?;
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
        };
        tokio::time::timeout(timeout, exchange)
            .await
            .map_err(|_| ClientError::Timeout)?
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
            vec!["bob/claude".parse().unwrap(), "bob/codex".parse().unwrap()],
        );
        let json = serde_json::to_value(Reply::err(ambiguous.clone())).unwrap();
        assert_eq!(json["error"]["code"], "ambiguous_recipient");
        assert_eq!(
            json["error"]["candidates"],
            serde_json::json!(["bob/claude", "bob/codex"])
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
