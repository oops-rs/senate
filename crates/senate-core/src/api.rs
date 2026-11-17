//! The server's HTTP/SSE wire: appends and heartbeats over HTTPS, push over
//! SSE. Developer machines only ever make outbound connections.
//!
//! # Who is calling
//!
//! Every `/v1` route takes `Authorization: Bearer <token>`, and the token
//! names a **person** ([`Me::person`]). A machine never authenticates as an
//! agent. Each thing it does as an agent names that agent's
//! [`SenatorName`]: the `from` of an append, [`Heartbeat::agent`], the
//! `senator` of a stream or a cursor. The server accepts the name only when
//! [`Roster::may_act_as`](crate::Roster::may_act_as) does (the agent is
//! directly under the authenticated person in the top Senate, directly
//! under the presider in a nested one; else 403 `forbidden`), and seats the
//! agent on first contact ([`Roster::seat`](crate::Roster::seat)). One
//! person may run several agents this way, each with its own stream,
//! cursors, presence and checkpoints.
//!
//! The senator holding the Speaker's seat ([`Me::speaker`]) is the one
//! exception: its person may open a stream, read and write cursors and send
//! heartbeats as it even where it cannot sit (the presider of a nested
//! Senate sits in the Senate above). It is not seated, and it writes in
//! that Senate only as `speaker`.
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
//!
//! A senator is named in a query, never in a path segment, because a
//! senator path holds slashes: `?agent=wendell/claude`.
//!
//! # Appends
//!
//! The `from` of an append is a senator the caller may act as. Every
//! senator in its `to` could sit in this Senate (a listed person, or an
//! agent at this Senate's level); else 400 `bad_request`. Its `to` may
//! hold `senator::<person>/*`; the server resolves it against its roster
//! before the event is stored
//! ([`Roster::resolve_recipients`](crate::Roster::resolve_recipients)), so
//! the event that comes back names concrete senators. When the person has
//! several agents the append is refused with 409
//! [`code::AMBIGUOUS_RECIPIENT`] and the candidates in
//! [`ApiError::candidates`]: the sender names one and tries again.
//!
//! # Writing as the Speaker
//!
//! An append `from: speaker` is accepted only when the Speaker's seat
//! ([`Me::speaker`]) is held by a senator and the authenticated person is
//! that senator's person ([`SpeakerSeat::held_by`](crate::SpeakerSeat::held_by)):
//! the session presiding over the Senate does the Speaker's work over HTTP.
//! It may then write what only the Speaker writes (triage verdicts, the
//! output level, the brief's top, takeover, provisional answers) and speak
//! `on_behalf_of` a senator of this Senate or a guest; not work runs (the
//! built-in Speaker's alone), not a `decision` (people decide), and not
//! what only the server writes. Anyone else is 403 `forbidden`. The event
//! is stored `from: speaker`, exactly as the built-in Speaker's would be.
//!
//! # A message for a person's agent
//!
//! When `to` holds `senator::<person>/*`, the person has several agents
//! here, the sender is not an agent (it cannot be expected to know their
//! agents), and a senator holds the Speaker's seat, the message is stored
//! addressed to `speaker` instead, with [`meta::PICK`](crate::meta::PICK)
//! naming the candidates: the Speaker routes it (see [`crate::docket`]).
//! An agent sender is refused with the candidates as below.
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

use crate::continuity::AgentPresence;
use crate::event::{Event, NewEvent, StatusState};
use crate::ids::{CommitteeId, Name, Seq};
use crate::roster::{ResolveError, SpeakerSeat};
use crate::senator::SenatorName;
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

/// `GET /v1/me`: who the token is, and which Senate answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// The authenticated person.
    pub person: Name,
    /// The senator presiding over the Senate this server serves. Absent for
    /// the top Senate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presider: Option<SenatorName>,
    /// Who holds this Senate's Speaker's seat: the built-in Speaker, a
    /// senator (a nested Senate's presider, or the agent a top Senate used
    /// alone names), or nobody (absent). A bridge gives the session of an
    /// agent holding the seat the presiding tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<SpeakerSeat>,
    /// The person's agents seated in this Senate now. May be none, one or
    /// several; an agent not listed is seated when it first connects.
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
    /// `senator::<person>/*` and the person has several agents;
    /// [`ApiError::candidates`](super::ApiError::candidates) lists them.
    pub const AMBIGUOUS_RECIPIENT: &str = "ambiguous_recipient";
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
}

impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
            candidates: Vec::new(),
        }
    }
}

/// How a wildcard that could not be resolved is refused: several agents is
/// [`code::AMBIGUOUS_RECIPIENT`] with the candidates, an unknown person is
/// [`code::BAD_REQUEST`].
impl From<&ResolveError> for ApiError {
    fn from(err: &ResolveError) -> Self {
        match err {
            ResolveError::Ambiguous { candidates, .. } => Self {
                code: code::AMBIGUOUS_RECIPIENT.to_string(),
                message: err.to_string(),
                candidates: candidates.clone(),
            },
            ResolveError::UnknownSenior(_) => Self::new(code::BAD_REQUEST, err.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::{Roster, Senior};
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
            "agent": "wendell/claude", "sessions": 2, "cursor": 40
        }))
        .unwrap();
        assert_eq!(hb.agent, path("wendell/claude"));
        // A heartbeat with no agent is nobody's.
        assert!(
            serde_json::from_value::<Heartbeat>(json!({ "sessions": 2, "cursor": 40 })).is_err()
        );

        let stream: StreamQuery =
            serde_json::from_value(json!({ "after": 7, "senator": "wendell/codex" })).unwrap();
        assert_eq!(stream.senator, Some(path("wendell/codex")));
        // A person reading, with no agent behind the stream.
        let reading: StreamQuery = serde_json::from_value(json!({})).unwrap();
        assert_eq!((reading.after, reading.senator), (Seq::ZERO, None));

        let cursor: CursorQuery =
            serde_json::from_value(json!({ "senator": "wendell/claude" })).unwrap();
        assert_eq!(cursor.senator, Some(path("wendell/claude")));
        let whose: CheckpointQuery =
            serde_json::from_value(json!({ "agent": "wendell/claude" })).unwrap();
        assert_eq!(whose.agent, path("wendell/claude"));
        assert!(serde_json::from_value::<CheckpointQuery>(json!({})).is_err());
    }

    #[test]
    fn me_is_a_person_with_any_number_of_agents_in_one_senate() {
        let top: Me = serde_json::from_value(json!({ "person": "wendell" })).unwrap();
        assert_eq!(top.presider, None);
        assert!(top.agents.is_empty() && top.committees.is_empty());
        assert_eq!(top.epoch, "");
        assert!(
            top.triaged,
            "a server that does not say judges, as all once did"
        );

        let nested = Me {
            person: Name::new("wendell").unwrap(),
            presider: Some(path("wendell/claude")),
            speaker: Some(SpeakerSeat::Senator(path("wendell/claude"))),
            agents: vec![
                path("wendell/claude/reviewer"),
                path("wendell/claude/codex"),
            ],
            committees: vec![c()],
            epoch: "e1".into(),
            triaged: false,
        };
        let value = serde_json::to_value(&nested).unwrap();
        assert_eq!(value["presider"], "wendell/claude");
        assert_eq!(value["speaker"], json!({ "senator": "wendell/claude" }));
        assert_eq!(top.speaker, None, "an older server names no Speaker");
        assert_eq!(value["triaged"], false);
        assert_eq!(serde_json::from_value::<Me>(value).unwrap(), nested);
    }

    #[test]
    fn an_ambiguous_recipient_is_refused_with_the_candidates() {
        let name = |s: &str| Name::new(s).unwrap();
        let roster = Roster::top(
            vec![Senior::new(name("wendell"))],
            vec![path("wendell/claude"), path("wendell/codex")],
        )
        .unwrap();
        let to = ["senator::wendell/*".parse().unwrap()];
        let refused = ApiError::from(&roster.resolve_recipients(&to).unwrap_err());
        assert_eq!(refused.code, code::AMBIGUOUS_RECIPIENT);
        assert_eq!(
            refused.candidates,
            [path("wendell/claude"), path("wendell/codex")]
        );
        let value = serde_json::to_value(&refused).unwrap();
        assert_eq!(
            value["candidates"],
            json!(["wendell/claude", "wendell/codex"])
        );
        assert_eq!(serde_json::from_value::<ApiError>(value).unwrap(), refused);

        let unknown = ["senator::bob/*".parse().unwrap()];
        let refused = ApiError::from(&roster.resolve_recipients(&unknown).unwrap_err());
        assert_eq!(refused.code, code::BAD_REQUEST);
        // Other errors carry no candidates, on the wire or off it.
        let plain = ApiError::new(code::FORBIDDEN, "no");
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            json!({ "code": "forbidden", "message": "no" })
        );
    }

    #[test]
    fn threads_can_be_asked_for_one_agent() {
        let q: ThreadsQuery =
            serde_json::from_value(json!({ "state": "suspended", "agent": "alice/codex" }))
                .unwrap();
        assert_eq!(q.state, Some(StatusState::Suspended));
        assert_eq!(q.agent, Some(path("alice/codex")));
        assert_eq!(
            serde_json::to_value(ThreadsQuery::default()).unwrap(),
            json!({})
        );
    }
}
