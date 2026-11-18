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
//! (`ibot::senator::codex`): the daemon resolves it through the server and
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
    /// A presiding op from a session whose agent does not work the
    /// Speaker's desk.
    pub const NOT_PRESIDING: &str = "not_presiding";
    /// A chair op from a session whose agent does not chair its committee.
    pub const NOT_CHAIRING: &str = "not_chairing";
    /// The Senate server refused or could not be reached.
    pub const SERVER: &str = "server";
    /// `senator::*` could reach several agents on the committee;
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
    /// A Claude inbox socket or a Senate-owned ACP listener.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<PathBuf>,
    /// Claude inbox token, or agy's SHA-256 launch proof.
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
    /// Fixed-seat registrations from previous bridge clients.
    Hook(HookReport),
    /// Native coding session. Its immutable senator ID is derived on the daemon.
    SessionHook(HookReport),
    /// Operator registration for one previously reported Antigravity session.
    RegisterAgy {
        session_id: String,
        endpoint: crate::delivery::agy::Endpoint,
    },
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
    /// Separate operation so an older daemon cannot silently ignore the name.
    Aliases,
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
        id: Option<EventId>,
    },
    /// Answer a question to an away agent on its behalf.
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
    /// Put an item awaiting the Speaker to the person.
    Refer {
        session: SessionSelector,
        committee: CommitteeId,
        seq: Seq,
        reason: String,
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
    /// The Speaker's office, the chairs, the child Senates.
    Roles,
    /// Codex approval requests waiting for an answer.
    Approvals,
    /// Answer a Codex approval request; the first answer wins.
    Approve {
        id: String,
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
    /// `senate watch` prints. Each event is announced once, whichever
    /// agent's stream carried it.
    ForPerson {
        /// The event itself.
        news: Box<Event>,
        /// An escalation (a `status`): `text` is its reason.
        escalated: bool,
        /// The body, or an escalation's reason.
        text: String,
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
    /// Directed first, then the rest, each group in seq order.
    pub items: Vec<CatchUpItem>,
    /// More events remain after this page.
    pub has_more: bool,
    /// The `read` cursor before and after this call.
    pub read_before: Seq,
    pub read: Seq,
}

impl CatchUpReply {
    /// How `me` reads these events ([`crate::format::Reader`]).
    pub fn reader(&self) -> crate::format::Reader {
        let reader = crate::format::Reader::new(
            self.me.clone(),
            self.people.clone(),
            self.own.iter().cloned().collect(),
        );
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
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReply {
    pub pid: u32,
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
