//! The one envelope.
//!
//! Everything in a committee's log is the same shape: a person's chat, an
//! agent's question and its answer, a decision, a checkpoint, a presence
//! change. They differ only in [`Kind`].
//!
//! The log assigns `seq` and `ts`; a writer supplies everything else as a
//! [`NewEvent`], including the [`EventId`] that makes a retried append
//! idempotent.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::address::Address;
use crate::ids::{CommitteeId, EventId, Seq};
use crate::mention::Mention;

/// What an event means. Serialized as its lowercase name; a name this build
/// does not know survives as [`Kind::Other`] so an older reader passes a
/// newer writer's events through untouched.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A person's message in a bound chat group, recorded as said.
    Chat,
    /// A group bound to or unbound from the committee.
    Binding,
    Question,
    /// `thread` points at the question's `seq`.
    Answer,
    /// Asks the other side to *do* something.
    Request,
    Note,
    Decision,
    Checkpoint,
    /// A task thread's state change.
    Status,
    Presence,
    Other(String),
}

impl Kind {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Chat => "chat",
            Self::Binding => "binding",
            Self::Question => "question",
            Self::Answer => "answer",
            Self::Request => "request",
            Self::Note => "note",
            Self::Decision => "decision",
            Self::Checkpoint => "checkpoint",
            Self::Status => "status",
            Self::Presence => "presence",
            Self::Other(s) => s,
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "chat" => Self::Chat,
            "binding" => Self::Binding,
            "question" => Self::Question,
            "answer" => Self::Answer,
            "request" => Self::Request,
            "note" => Self::Note,
            "decision" => Self::Decision,
            "checkpoint" => Self::Checkpoint,
            "status" => Self::Status,
            "presence" => Self::Presence,
            other => Self::Other(other.to_string()),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for Kind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Kind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse(&s))
    }
}

/// Where an event that began on an outside platform came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    /// The platform's name, as its gateway reports it.
    pub platform: String,
    /// The platform's stable group identity, as an opaque handle.
    pub group_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
}

/// The platform identity of an event that began on a platform: which message
/// it *is*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mirror {
    pub platform: String,
    pub message_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_ref: Option<String>,
}

/// Pointers into code. The log never stores code; these are refs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

impl Refs {
    pub fn is_empty(&self) -> bool {
        self.commit.is_none() && self.path.is_none() && self.branch.is_none()
    }
}

/// A stored log event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: Seq,
    pub committee: CommitteeId,
    pub id: EventId,
    pub ts: DateTime<Utc>,
    pub kind: Kind,
    pub from: Address,
    /// Empty means undirected: recorded, never pushed, seen on catch-up.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<Address>,
    /// The `seq` of the thread's root event (a question's seq for its answers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<Seq>,
    /// Set when one party speaks for another: `from: speaker`,
    /// `on_behalf_of: senator::wendell/claude` is the Speaker answering for
    /// an absent agent. Stored as fields, never as text in `body`, so the
    /// label is written when the event is shown ([`Event::author`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Refs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<Mirror>,
    /// Kind-specific structured payloads and forward-compatible extensions.
    /// Keys are owned by the kind that uses them.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub meta: Map<String, Value>,
}

impl Event {
    /// True when `addr` is named in `to`. `committee` is a broadcast for
    /// catch-up, not a push to every session, so it does not count here.
    pub fn is_directed_to(&self, addr: &Address) -> bool {
        self.to.iter().any(|a| a == addr)
    }

    /// The label for who wrote this: `[senator::wendell/claude]`, or
    /// `[speaker::wendell/claude]` when the Speaker answered for that senator.
    pub fn author(&self) -> Option<Mention> {
        Mention::author(&self.from, self.on_behalf_of.as_ref())
    }
}

/// An event before the log has assigned it a `seq` and `ts`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewEvent {
    #[serde(default)]
    pub id: EventId,
    pub committee: CommitteeId,
    pub kind: Kind,
    pub from: Address,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<Seq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Address>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refs: Option<Refs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<Mirror>,
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub meta: Map<String, Value>,
}

impl NewEvent {
    pub fn new(committee: CommitteeId, kind: Kind, from: Address, body: impl Into<String>) -> Self {
        Self {
            id: EventId::new(),
            committee,
            kind,
            from,
            to: Vec::new(),
            thread: None,
            on_behalf_of: None,
            source: None,
            body: body.into(),
            refs: None,
            mirror: None,
            meta: Map::new(),
        }
    }

    pub fn with_id(mut self, id: EventId) -> Self {
        self.id = id;
        self
    }

    pub fn to(mut self, addr: Address) -> Self {
        self.to.push(addr);
        self
    }

    pub fn with_to(mut self, to: Vec<Address>) -> Self {
        self.to = to;
        self
    }

    pub fn in_thread(mut self, thread: Seq) -> Self {
        self.thread = Some(thread);
        self
    }

    pub fn on_behalf_of(mut self, addr: Address) -> Self {
        self.on_behalf_of = Some(addr);
        self
    }

    pub fn with_source(mut self, source: Source) -> Self {
        self.source = Some(source);
        self
    }

    pub fn with_refs(mut self, refs: Refs) -> Self {
        self.refs = (!refs.is_empty()).then_some(refs);
        self
    }

    pub fn with_mirror(mut self, mirror: Mirror) -> Self {
        self.mirror = Some(mirror);
        self
    }

    pub fn with_meta(mut self, key: impl Into<String>, value: Value) -> Self {
        self.meta.insert(key.into(), value);
        self
    }

    /// The stored form, given what the log assigned.
    pub fn into_event(self, seq: Seq, ts: DateTime<Utc>) -> Event {
        Event {
            seq,
            committee: self.committee,
            id: self.id,
            ts,
            kind: self.kind,
            from: self.from,
            to: self.to,
            thread: self.thread,
            on_behalf_of: self.on_behalf_of,
            source: self.source,
            body: self.body,
            refs: self.refs,
            mirror: self.mirror,
            meta: self.meta,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn committee() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    #[test]
    fn decodes_the_wire_form() {
        let event: Event = serde_json::from_value(json!({
            "seq": 118,
            "committee": "refund-v2",
            "id": "01JB0000000000000000000000",
            "ts": "2026-11-16T09:00:00Z",
            "kind": "question",
            "from": "senator::alice/claude",
            "to": ["senator::wendell/claude"],
            "body": "retry cap?"
        }))
        .unwrap();
        assert_eq!(event.seq, Seq(118));
        assert_eq!(event.committee, committee());
        assert_eq!(event.kind, Kind::Question);
        assert!(event.is_directed_to(&addr("senator::wendell/claude")));
        assert!(!event.is_directed_to(&addr("senator::wendell")));
        assert_eq!(event.thread, None);
        assert!(event.meta.is_empty());
    }

    #[test]
    fn optional_fields_stay_off_the_wire() {
        let new = NewEvent::new(committee(), Kind::Note, addr("senator::wendell"), "hi");
        let value = serde_json::to_value(&new).unwrap();
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["body", "committee", "from", "id", "kind"]);
    }

    #[test]
    fn unknown_kinds_and_addresses_round_trip() {
        let value = json!({
            "seq": 1,
            "committee": "refund-v2",
            "id": "01JB0000000000000000000000",
            "ts": "2026-11-16T09:00:00Z",
            "kind": "poll",
            "from": "guest:dial/reviewer",
            "body": ""
        });
        let event: Event = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(event.kind, Kind::Other("poll".into()));
        assert_eq!(event.from, Address::Other("guest:dial/reviewer".into()));
        assert_eq!(event.author(), None);
        assert_eq!(serde_json::to_value(&event).unwrap(), value);
    }

    #[test]
    fn the_speaker_answering_for_a_senator_is_fields_not_text() {
        let ts = "2026-11-16T09:00:00Z".parse().unwrap();
        let answer = NewEvent::new(committee(), Kind::Answer, Address::Speaker, "cap is 3")
            .on_behalf_of(addr("senator::wendell/claude"))
            .to(addr("senator::alice/claude"))
            .in_thread(Seq(118))
            .into_event(Seq(119), ts);

        assert_eq!(answer.body, "cap is 3");
        assert_eq!(
            answer.author().unwrap().to_string(),
            "[speaker::wendell/claude]"
        );
        assert_eq!(answer.thread, Some(Seq(118)));
    }

    #[test]
    fn empty_refs_are_dropped() {
        let new =
            NewEvent::new(committee(), Kind::Note, Address::Speaker, "").with_refs(Refs::default());
        assert_eq!(new.refs, None);
    }
}
