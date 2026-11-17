//! The server's HTTP/SSE wire: appends and heartbeats over HTTPS, push over
//! SSE. Developer machines only ever make outbound connections.
//!
//! # Who is calling
//!
//! Every `/v1` route takes `Authorization: Bearer <token>`, and the token
//! names a **person** ([`Me::person`]). A machine never authenticates as an
//! agent. Each thing it does as an agent names that agent's flat
//! [`SenatorName`]: the `from` of an append, [`Heartbeat::agent`], the
//! `senator` of a stream or a cursor. The server accepts the name only when
//! [`Roster::may_act_as`](crate::Roster::may_act_as) does (the person
//! answers for the agent, or nobody does yet; else 403 `forbidden`), and
//! seats the agent on first contact under that person
//! ([`Roster::seat`](crate::Roster::seat)). One person may run several
//! agents this way, each with its own stream, cursors, presence and
//! checkpoints: a seat is a senator on a committee.
//!
//! Guests hold no token. They reach the log through a gateway inside the
//! server, which writes for them as `guest::<platform>/<id>`.
//!
//! # Routes
//!
//! | Method | Path | Body → Response |
//! | --- | --- | --- |
//! | GET  | `/healthz` | → `"ok"` (no auth) |
//! | GET  | `/v1/me` | → [`Me`] |
//! | POST | `/v1/committees/{committee}/events` | [`NewEvent`] → [`AppendResponse`] |
//! | GET  | `/v1/committees/{committee}/events?after=&limit=` | → [`EventsPage`] |
//! | GET  | `/v1/committees/{committee}/stream?after=&senator=` | → SSE, see below |
//! | GET  | `/v1/committees/{committee}/cursors/{name}?senator=` | → [`Cursor`] |
//! | PUT  | `/v1/committees/{committee}/cursors/{name}?senator=` | [`Cursor`] → [`Cursor`] |
//! | POST | `/v1/committees/{committee}/heartbeat` | [`Heartbeat`] → 204 |
//! | GET  | `/v1/committees/{committee}/search?q=&limit=` | → [`SearchResults`] |
//! | GET  | `/v1/committees/{committee}/brief` | → [`Brief`](crate::Brief) |
//! | GET  | `/v1/committees/{committee}/threads?state=&agent=` | → [`ThreadsPage`] |
//! | GET  | `/v1/committees/{committee}/presence` | → [`PresencePage`] |
//! | GET  | `/v1/committees/{committee}/checkpoints/latest?agent=` | → [`CheckpointRecord`](crate::CheckpointRecord) (404 `not_found` when none) |
//! | GET  | `/v1/committees/{committee}/docket` | → [`DocketPage`](crate::DocketPage) |
//! | GET  | `/v1/resolve?name=&committee=` | → [`Resolved`] |
//! | GET  | `/v1/committees/{committee}/chair` | → [`ChairInfo`] |
//! | PUT  | `/v1/committees/{committee}/chair` | [`SetChair`] → [`ChairInfo`] |
//! | GET  | `/v1/secretaries` | → [`Secretaries`] |
//! | PUT  | `/v1/secretaries/{senator}` | [`SetSecretaries`] → [`Secretaries`] |
//! | GET  | `/v1/children` | → [`Vec<ChildSenate>`](ChildSenate) |
//! | PUT  | `/v1/children/{name}` | [`RegisterChild`] → [`ChildSenate`] |
//!
//! A senator is named in a query, never in a path segment: `?agent=claude`.
//!
//! # Resolving a qualified name
//!
//! `GET /v1/resolve` answers for this Senate alone ([`ResolveQuery`],
//! [`Resolved`]): whether a qualified name ([`Locator`]) is here, and where;
//! or that it is not, so the caller asks the Senate above
//! ([`Resolved::Elsewhere`]). There is no central registry: a developer
//! machine walks outward, nearest first, asking each Senate it knows in
//! turn. A Senate's own name is [`Me::senate`].
//!
//! # Appends
//!
//! The `from` of an append is a senator the caller may act as. Every
//! senator in its `to` could sit in this Senate (a listed person, or an
//! agent no other person answers for); else 400 `bad_request`. Its `to`
//! may hold `senator::*`; the server resolves it against the agents on the
//! committee before the event is stored
//! ([`Roster::resolve_recipients_among`](crate::Roster::resolve_recipients_among)),
//! so the event that comes back names concrete senators. When several
//! agents could get it the append is refused with 409
//! [`code::AMBIGUOUS_RECIPIENT`] and the candidates in
//! [`ApiError::candidates`]: the sender names one and tries again.
//!
//! # Writing as the Speaker
//!
//! An append `from: speaker` is accepted only when the Speaker's seat
//! ([`Me::speaker`]) is held by a senator and the authenticated person
//! answers for it ([`Roster::seat_held_by`](crate::Roster::seat_held_by)):
//! the session presiding over the Senate does the Speaker's work over HTTP.
//! It may then write what only the Speaker writes (triage verdicts, the
//! output level, the brief's top, takeover, provisional answers) and speak
//! `on_behalf_of` a senator of this Senate or a guest; not work runs (the
//! built-in Speaker's alone), not a `decision` (people decide), and not
//! what only the server writes. Anyone else is 403 `forbidden`. The event
//! is stored `from: speaker`, exactly as the built-in Speaker's would be.
//!
//! # A message for whichever agent
//!
//! When `to` holds `senator::*`, several agents are on the committee, the
//! sender is not an agent (it cannot be expected to know them), and a
//! senator holds the Speaker's seat, the message is stored addressed to
//! `speaker` instead, with [`meta::PICK`](crate::meta::PICK) naming the
//! candidates: the Speaker routes it (see [`crate::docket`]). An agent
//! sender is refused with the candidates as below.
//!
//! Appends a senator may not make: `binding`, `presence` and `chat` (those
//! are written by server components), any event carrying
//! [`meta::BRIEF_TOP`](crate::meta::BRIEF_TOP),
//! [`meta::TRIAGE`](crate::meta::TRIAGE),
//! [`meta::OUTPUT_LEVEL`](crate::meta::OUTPUT_LEVEL),
//! [`meta::WORK`](crate::meta::WORK) or
//! [`meta::PROVISIONAL`](crate::meta::PROVISIONAL) (the Speaker's),
//! `decision` from an agent (agents propose a decision as a `note` with
//! [`meta::PROPOSAL`](crate::meta::PROPOSAL); people decide), and
//! `checkpoint` from anyone but an agent. Each is 403 `forbidden`.
//!
//! A `checkpoint` must carry a [`CheckpointPayload`](crate::CheckpointPayload)
//! in [`meta::CHECKPOINT`](crate::meta::CHECKPOINT) that passes its `check`
//! (branch and commit named, at most
//! [`MAX_CHECKPOINT_BYTES`](crate::continuity::MAX_CHECKPOINT_BYTES)); else
//! 400 `bad_request`. [`meta::COMPANION`](crate::meta::COMPANION) is accepted
//! only on an `answer` from an agent (400 otherwise).
//!
//! A `status` append must be in the thread of a task (a root `question` or
//! `request` addressed to an agent; else 400 `bad_request`), carry a
//! [`StatusPayload`](crate::task::StatusPayload) that passes its `check`
//! (else 400), and, from an agent, be in a thread addressed to that very
//! agent and carry no `takeover` (only people take over); else 403. An
//! escalation is such a status: `suspended` with `escalated_to`.
//!
//! # Presence
//!
//! Presence is the server's, and it is per agent: `presence` events come
//! from `committee`, one per transition, and `GET .../presence` gives each
//! agent's current state with `lag` = the committee's head minus the
//! delivered cursor of that agent's latest heartbeat.
//!
//! # The stream
//!
//! First every stored event with `seq > after` (or `> Last-Event-ID` when
//! the header is present, which wins), in order, then live events as they
//! commit. Each log event is an SSE message with `event: event`,
//! `id: <seq>`, `data: <Event JSON>`. A keepalive `event: ping` with
//! `data: <RFC 3339 server time>` is sent every [`SSE_PING_SECS`]. The
//! stream carries the whole committee log; the bridge decides what to push
//! (directed) and what waits for catch-up. A stream opened with `senator`
//! naming an agent is that agent's connection, and counts for its presence.
//!
//! # Epochs and cursors
//!
//! Every `/v1` response carries the log's epoch in the [`EPOCH_HEADER`]
//! header (also in [`Me::epoch`]). Seqs are only comparable within one epoch:
//! when it changes, the log was reset and a client must drop every cursor it
//! holds for it. A stream or page request whose `after` / `Last-Event-ID` is
//! beyond the committee's head is answered 409 `cursor_ahead`, which is the
//! same fact discovered from the other side.
//!
//! Cursors are per senator: `cursors/{name}` reads and writes the cursor
//! owned by the `senator` in the query (the authenticated person when it is
//! absent), so the `delivered` cursors of two agents of one person never
//! collide. A cursor never set reads as `{"seq": 0}`.
//!
//! Errors are a non-2xx status with an [`ApiError`] body.

use serde::{Deserialize, Serialize};

use crate::address::Address;
use crate::chair::ChairSource;
use crate::continuity::AgentPresence;
use crate::event::{Event, NewEvent, StatusState};
use crate::ids::{CommitteeId, Name, Seq};
use crate::locator::Locator;
use crate::roster::{DEFAULT_SENATE_NAME, Office, People, ResolveError, SpeakerSeat};
use crate::senate::SenatePath;
use crate::senator::SenatorName;
use crate::senator_id::SenatorId;
use crate::task::ThreadSummary;

/// Response header naming the log's epoch (see the module docs).
pub const EPOCH_HEADER: &str = "x-senate-epoch";

pub const SSE_EVENT: &str = "event";
pub const SSE_PING: &str = "ping";
pub const SSE_PING_SECS: u64 = 15;
/// Upper bound the server applies to `limit`.
pub const MAX_PAGE: usize = 500;
pub const DEFAULT_PAGE: usize = 100;

/// Cursor names the bridge uses. `delivered`: highest seq the bridge has
/// durably received. `read`: highest seq an agent's catch-up returned.
pub const CURSOR_DELIVERED: &str = "delivered";
pub const CURSOR_READ: &str = "read";

pub const HEALTH_PATH: &str = "/healthz";
pub const ME_PATH: &str = "/v1/me";

pub fn events_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/events")
}

pub fn stream_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/stream")
}

pub fn cursor_path(committee: &CommitteeId, name: &str) -> String {
    format!("/v1/committees/{committee}/cursors/{name}")
}

pub fn heartbeat_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/heartbeat")
}

pub fn search_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/search")
}

pub fn brief_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/brief")
}

pub fn threads_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/threads")
}

pub fn presence_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/presence")
}

/// Takes the agent as [`CheckpointQuery`].
pub fn latest_checkpoint_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/checkpoints/latest")
}

/// `GET .../checkpoints/latest`: whose. A checkpoint is one agent's, so the
/// agent is named in full; a person's handle is not enough.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointQuery {
    pub agent: SenatorName,
}

pub fn docket_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/docket")
}

pub const RESOLVE_PATH: &str = "/v1/resolve";
pub const SECRETARIES_PATH: &str = "/v1/secretaries";
pub const CHILDREN_PATH: &str = "/v1/children";

pub fn chair_path(committee: &CommitteeId) -> String {
    format!("/v1/committees/{committee}/chair")
}

pub fn secretaries_of_path(senator: &SenatorName) -> String {
    format!("{SECRETARIES_PATH}/{senator}")
}

pub fn child_path(name: &Name) -> String {
    format!("{CHILDREN_PATH}/{name}")
}

/// `GET` / `PUT .../chair`: a committee's chair and how it was found
/// ([`crate::chair`]). `chair` is `None` for a committee with no event yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChairInfo {
    pub committee: CommitteeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chair: Option<SenatorName>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ChairSource>,
}

/// `PUT .../chair`: make `senator` the committee's chair. Allowed for a
/// principal of the Senate and for the Speaker's office.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetChair {
    pub senator: SenatorName,
}

/// `GET /v1/secretaries`: every secretary relation, principal → its
/// secretaries in order.
pub type Secretaries = std::collections::BTreeMap<SenatorName, Vec<SenatorName>>;

/// `PUT /v1/secretaries/{senator}`: `senator`'s secretaries, in order
/// (empty removes them). Allowed for the senator itself, the principal
/// that runs it, and the Speaker's office; refused for anyone else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSecretaries {
    pub secretaries: Vec<SenatorName>,
    /// Writing for the Speaker's office: who acts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acted_by: Option<SenatorName>,
}

/// A child Senate registered with this one: its name, and its Speaker (a
/// principal of this Senate, who registered it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildSenate {
    pub name: Name,
    pub speaker: SenatorName,
}

/// `PUT /v1/children/{name}`: register a child Senate, authenticated as
/// its Speaker. `speaker` is the child's Speaker as the child names it:
/// `/<this Senate's path>::senator::<principal>`; the principal must be on
/// this Senate's roster and be who authenticated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterChild {
    pub speaker: SenatorId,
}

/// `GET /v1/resolve`: is `name` here?
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolveQuery {
    /// The qualified name, in its strict form.
    pub name: Locator,
    /// The committee the writer stands on: a name with no committee is
    /// on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committee: Option<CommitteeId>,
}

/// What one Senate answers for a qualified name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "found", rename_all = "snake_case")]
pub enum Resolved {
    /// It is in this Senate: `address` on `committee`. `locator` is the
    /// name with this Senate's path and the committee filled in.
    Here {
        locator: Locator,
        committee: CommitteeId,
        address: Address,
    },
    /// It is not in this Senate: ask the Senate above, if there is one.
    Elsewhere,
    /// Several things here answer to it; name one of `candidates`.
    Ambiguous { candidates: Vec<Locator> },
    /// It names something here that does not exist (a senator nobody
    /// knows on a committee that does).
    Unknown { reason: String },
}

/// `GET .../presence`: every agent with a presence on the committee, by path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresencePage {
    pub agents: Vec<AgentPresence>,
    /// The committee's head the lags are measured against.
    pub head: Seq,
}

/// `GET .../threads`. Without `state`, every task thread; with `agent`, only
/// those addressed to that agent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ThreadsQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<StatusState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<SenatorName>,
}

/// Longest in their state first (see [`TaskBoard::threads`](crate::TaskBoard::threads)).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadsPage {
    pub threads: Vec<ThreadSummary>,
    /// The last seq folded in.
    pub head: Seq,
}

/// `POST .../events` body is a [`NewEvent`]; its `committee` must equal the
/// path's. Re-posting an event `id` already stored returns that event with
/// `duplicate: true`.
pub type AppendRequest = NewEvent;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendResponse {
    /// As stored: `to` holds concrete senators, whatever was sent.
    pub event: Event,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub after: Seq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventsPage {
    pub events: Vec<Event>,
    /// True when more events exist after the last one returned.
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StreamQuery {
    #[serde(default)]
    pub after: Seq,
    /// The agent this stream is the connection of. Absent: the person is
    /// only reading, and no agent's presence follows from the stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senator: Option<SenatorName>,
}

/// `GET` / `PUT .../cursors/{name}`: whose cursor.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CursorQuery {
    /// Absent: the authenticated person's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub senator: Option<SenatorName>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub seq: Seq,
}

/// One agent's bridge saying it is alive. Leases and presence hang on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// The agent whose heartbeat this is. A machine running two agents for
    /// one person sends two.
    pub agent: SenatorName,
    /// That agent's sessions currently bound to the committee.
    pub sessions: u32,
    /// The agent's delivered cursor for the committee.
    pub cursor: Seq,
}

/// Upper bound the server applies to a search `limit`.
pub const MAX_SEARCH: usize = 50;
pub const DEFAULT_SEARCH: usize = 10;

/// `GET .../search`. An empty `q` (after trimming) is 400 `bad_request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

/// Best match first; see [`Hub::search`](crate::Hub::search) for the order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchResults {
    pub events: Vec<Event>,
}

/// Senate-wide human-readable addresses, keyed by alias with the primary senator as value.
pub type SenatorAliases = std::collections::BTreeMap<SenatorName, SenatorName>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetAlias {
    pub alias: SenatorName,
    /// A configured pin supplies an initial alias. An explicit choice takes precedence.
    #[serde(default)]
    pub initial: bool,
}

/// `GET /v1/me`: who the token is, and which Senate answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// The authenticated person.
    pub person: Name,
    /// This Senate's name (`senate` when its configuration gives none; a
    /// server that does not say is taken to be that one).
    #[serde(default = "default_senate_name")]
    pub senate: Name,
    /// The path of the Senate above, when this Senate knows it. Absent: no
    /// Senate above, or none it knows of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SenatePath>,
    /// Who works the Speaker's desk first, as the older single seat (the
    /// built-in session, or an agent); absent when nobody does. Kept for
    /// older readers: [`Me::office`] says it all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<SpeakerSeat>,
    /// The Speaker's office: who the Speaker is (a senator id, and what
    /// kind of senator), and who works its desk, in order. A bridge gives
    /// the session of an agent working the desk the presiding tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub office: Option<Office>,
    /// The child Senates registered with this one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildSenate>,
    /// The handles of this Senate's people: every senator not among them is
    /// an agent. A reader that folds the log (a bridge's task board) tells
    /// people from agents by it.
    #[serde(default)]
    pub people: People,
    /// The agents this person answers for, seated in this Senate now. May
    /// be none, one or several; an agent not listed is seated when it first
    /// connects.
    #[serde(default)]
    pub agents: Vec<SenatorName>,
    /// Committees this person can join: every committee that has at least
    /// one event; empty means none exist yet.
    #[serde(default)]
    pub committees: Vec<CommitteeId>,
    /// The log's epoch; see the module docs.
    #[serde(default)]
    pub epoch: String,
    /// Whether a Speaker on this server judges task roots (writes the
    /// `meta.triage` verdict a bridge holds a root for, see
    /// [`crate::task`]): only the built-in Speaker does
    /// ([`SpeakerSeat::triages`]). False where nothing does: no Speaker, or
    /// a Speaker held by a senator, whose session may be closed and has no
    /// triage duty. A bridge then delivers a root at once instead of waiting
    /// for a verdict that may never come. A server that does not say is
    /// taken to judge, as every server once did.
    #[serde(default = "judged_by_default")]
    pub triaged: bool,
}

impl Me {
    /// This Senate's path from the root: its parent's, then its own name.
    /// A Senate that names no parent is a root.
    pub fn senate_path(&self) -> SenatePath {
        let top = || SenatePath::top(self.senate.clone()).unwrap_or_else(|_| default_senate_path());
        match &self.parent {
            Some(parent) => parent
                .clone()
                .absolute()
                .child(self.senate.clone())
                .unwrap_or_else(|_| top()),
            None => top(),
        }
    }
}

fn default_senate_name() -> Name {
    Name::new(DEFAULT_SENATE_NAME).unwrap_or_else(|_| unreachable!("a valid name"))
}

fn default_senate_path() -> SenatePath {
    SenatePath::top(default_senate_name()).unwrap_or_else(|_| unreachable!("a valid path"))
}

fn judged_by_default() -> bool {
    true
}

/// The stable machine codes of [`ApiError::code`].
pub mod code {
    pub const UNAUTHORIZED: &str = "unauthorized";
    pub const FORBIDDEN: &str = "forbidden";
    pub const NOT_FOUND: &str = "not_found";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const CONFLICT: &str = "conflict";
    pub const CURSOR_AHEAD: &str = "cursor_ahead";
    /// `senator::*` and several agents could get it;
    /// [`ApiError::candidates`](super::ApiError::candidates) lists them.
    pub const AMBIGUOUS_RECIPIENT: &str = "ambiguous_recipient";
    /// A qualified name that several things answer to;
    /// [`ApiError::locators`](super::ApiError::locators) lists them.
    pub const AMBIGUOUS_NAME: &str = "ambiguous_name";
    /// A person's message for a seat that is not in this Senate nor in a
    /// child registered with it: the person's own machine posts it to the
    /// Senate above.
    pub const ELSEWHERE: &str = "elsewhere";
    pub const INTERNAL: &str = "internal";
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiError {
    /// One of [`code`]. A client treats a code it does not know by its HTTP
    /// status.
    pub code: String,
    /// For whoever is debugging the client. English; not shown to people as
    /// it is.
    pub message: String,
    /// With [`code::AMBIGUOUS_RECIPIENT`]: the agents to choose from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<SenatorName>,
    /// With [`code::AMBIGUOUS_NAME`]: the qualified names to choose from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locators: Vec<Locator>,
}

impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            candidates: Vec::new(),
            locators: Vec::new(),
        }
    }
}

/// How a wildcard that could not be resolved is refused: several agents is
/// [`code::AMBIGUOUS_RECIPIENT`] with the candidates, none and nobody to
/// receive it instead is [`code::BAD_REQUEST`].
impl From<&ResolveError> for ApiError {
    fn from(err: &ResolveError) -> Self {
        match err {
            ResolveError::Ambiguous { candidates } => Self {
                code: code::AMBIGUOUS_RECIPIENT.to_string(),
                message: err.to_string(),
                candidates: candidates.clone(),
                locators: Vec::new(),
            },
            ResolveError::NoAgent => Self::new(code::BAD_REQUEST, err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::{Principal, Roster, SeatedAgent};
    use serde_json::json;

    fn c() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn path(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    #[test]
    fn routes_are_under_committees() {
        assert_eq!(events_path(&c()), "/v1/committees/refund-v2/events");
        assert_eq!(
            cursor_path(&c(), CURSOR_DELIVERED),
            "/v1/committees/refund-v2/cursors/delivered"
        );
        assert_eq!(
            latest_checkpoint_path(&c()),
            "/v1/committees/refund-v2/checkpoints/latest"
        );
        for p in [
            events_path(&c()),
            stream_path(&c()),
            heartbeat_path(&c()),
            search_path(&c()),
            brief_path(&c()),
            threads_path(&c()),
            presence_path(&c()),
        ] {
            assert!(p.starts_with("/v1/committees/refund-v2/"), "{p}");
            crate::testing::assert_senate_vocabulary(&p);
        }
    }

    #[test]
    fn everything_done_as_an_agent_names_the_agent() {
        let hb: Heartbeat = serde_json::from_value(json!({
            "agent": "claude", "sessions": 2, "cursor": 40
        }))
        .unwrap();
        assert_eq!(hb.agent, path("claude"));
        // A heartbeat with no agent is nobody's; an owner-prefixed one is
        // not a name.
        assert!(
            serde_json::from_value::<Heartbeat>(json!({ "sessions": 2, "cursor": 40 })).is_err()
        );
        assert!(
            serde_json::from_value::<Heartbeat>(json!({
                "agent": "wendell/claude", "sessions": 2, "cursor": 40
            }))
            .is_err()
        );

        let stream: StreamQuery =
            serde_json::from_value(json!({ "after": 7, "senator": "codex" })).unwrap();
        assert_eq!(stream.senator, Some(path("codex")));
        // A person reading, with no agent behind the stream.
        let reading: StreamQuery = serde_json::from_value(json!({})).unwrap();
        assert_eq!((reading.after, reading.senator), (Seq::ZERO, None));

        let cursor: CursorQuery = serde_json::from_value(json!({ "senator": "claude" })).unwrap();
        assert_eq!(cursor.senator, Some(path("claude")));
        let whose: CheckpointQuery = serde_json::from_value(json!({ "agent": "claude" })).unwrap();
        assert_eq!(whose.agent, path("claude"));
        assert!(serde_json::from_value::<CheckpointQuery>(json!({})).is_err());
    }

    #[test]
    fn me_names_the_senate_and_the_persons_agents() {
        let old: Me = serde_json::from_value(json!({ "person": "wendell" })).unwrap();
        assert_eq!(old.senate.as_str(), "senate", "an older server is `senate`");
        assert_eq!(old.parent, None);
        assert!(old.agents.is_empty() && old.committees.is_empty());
        assert_eq!(old.epoch, "");
        assert_eq!(old.speaker, None, "an older server names no Speaker");
        assert!(
            old.triaged,
            "a server that does not say judges, as all once did"
        );
        // An older server's `presider` is ignored.
        assert!(
            serde_json::from_value::<Me>(json!({ "person": "wendell", "presider": "x/y" })).is_ok()
        );

        let me = Me {
            person: Name::new("wendell").unwrap(),
            senate: Name::new("north-desk").unwrap(),
            parent: Some("acme".parse().unwrap()),
            people: People::new([Name::new("wendell").unwrap()]),
            speaker: Some(SpeakerSeat::Senator(path("claude"))),
            office: None,
            children: Vec::new(),
            agents: vec![path("claude"), path("codex")],
            committees: vec![c()],
            epoch: "e1".into(),
            triaged: false,
        };
        let value = serde_json::to_value(&me).unwrap();
        assert_eq!(value["senate"], "north-desk");
        assert_eq!(value["parent"], "acme", "as configured");
        assert_eq!(value["people"], json!(["wendell"]));
        assert_eq!(value["speaker"], json!({ "senator": "claude" }));
        assert_eq!(value["triaged"], false);
        assert_eq!(me.senate_path().to_string(), "/acme/north-desk");
        assert_eq!(old.senate_path().to_string(), "/senate");
        assert_eq!(serde_json::from_value::<Me>(value).unwrap(), me);
    }

    #[test]
    fn an_ambiguous_recipient_is_refused_with_the_candidates() {
        let name = |s: &str| Name::new(s).unwrap();
        let roster = Roster::new(
            vec![Principal::new(name("wendell"))],
            vec![
                SeatedAgent::new(path("claude"), name("wendell")),
                SeatedAgent::new(path("codex"), name("wendell")),
            ],
        )
        .unwrap();
        let to = ["senator::*".parse().unwrap()];
        let refused = ApiError::from(&roster.resolve_recipients(&to).unwrap_err());
        assert_eq!(refused.code, code::AMBIGUOUS_RECIPIENT);
        assert_eq!(refused.candidates, [path("claude"), path("codex")]);
        let value = serde_json::to_value(&refused).unwrap();
        assert_eq!(value["candidates"], json!(["claude", "codex"]));
        assert_eq!(serde_json::from_value::<ApiError>(value).unwrap(), refused);

        let refused = ApiError::from(&ResolveError::NoAgent);
        assert_eq!(refused.code, code::BAD_REQUEST);
        // Other errors carry no candidates, on the wire or off it.
        let plain = ApiError::new(code::FORBIDDEN, "no");
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            json!({ "code": "forbidden", "message": "no" })
        );
    }

    #[test]
    fn a_resolution_says_here_elsewhere_or_which() {
        let q: ResolveQuery = serde_json::from_value(
            json!({ "name": "ibot::senator::codex", "committee": "refund-v2" }),
        )
        .unwrap();
        assert_eq!(q.name.to_string(), "ibot::senator::codex");
        assert_eq!(q.committee, Some(c()));
        let here = Resolved::Here {
            locator: "acme::ibot::senator::codex".parse().unwrap(),
            committee: CommitteeId::new("ibot").unwrap(),
            address: "senator::codex".parse().unwrap(),
        };
        let value = serde_json::to_value(&here).unwrap();
        assert_eq!(value["found"], "here");
        assert_eq!(value["locator"], "acme::ibot::senator::codex");
        assert_eq!(serde_json::from_value::<Resolved>(value).unwrap(), here);
        assert_eq!(
            serde_json::to_value(Resolved::Elsewhere).unwrap(),
            json!({ "found": "elsewhere" })
        );
    }

    #[test]
    fn threads_can_be_asked_for_one_agent() {
        let q: ThreadsQuery =
            serde_json::from_value(json!({ "state": "suspended", "agent": "codex" })).unwrap();
        assert_eq!(q.state, Some(StatusState::Suspended));
        assert_eq!(q.agent, Some(path("codex")));
        assert_eq!(
            serde_json::to_value(ThreadsQuery::default()).unwrap(),
            json!({})
        );
    }
}
