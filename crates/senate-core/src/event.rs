//! The one envelope.
//!
//! Everything in a committee's log is the same shape: a person's chat, an
//! agent's question and its answer, a decision, a checkpoint, a presence
//! change. They differ only in [`Kind`].
//!
//! The log assigns `seq` and `ts`; a writer supplies everything else as a
//! [`NewEvent`], including the [`EventId`] that makes a retried append
//! idempotent.
//!
//! Nothing in an event depends on a language. Who spoke for whom, takeover
//! and state are fields and [`meta`] keys; the words for them are chosen
//! when the event is shown ([`Event::byline`]).

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::address::Address;
use crate::ids::{CommitteeId, EventId, Seq};
use crate::locale::Locale;
use crate::mention::Mention;

/// What an event means. Serialized as its lowercase name; a name this build
/// does not know survives as [`Kind::Other`] so an older reader passes a
/// newer writer's events through untouched.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A person's message in a bound chat group, recorded as said.
    Chat,
    /// A group bound to or unbound from the committee. Payload in
    /// `meta["binding"]` ([`BindingPayload`]).
    Binding,
    Question,
    /// `thread` points at the question's `seq`.
    Answer,
    /// Asks the other side to *do* something. Always goes to the person
    /// first: triage parks it on them; once they approve, it is re-sent as
    /// their own `request` to their own agent, in the same thread.
    Request,
    Note,
    Decision,
    Checkpoint,
    /// A task thread's state change. Payload in `meta["status"]`
    /// ([`StatusPayload`](crate::task::StatusPayload)).
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
    /// Nobody said it on the platform: `senate chat send` injected it
    /// through the gateway, as a test of the Speaker
    /// ([`crate::chat_sim`]). The Speaker carries a chat's source onto what
    /// it derives from it (a relay, a task, a decision), so those are marked
    /// too; readers show them as "(simulated)" and the brief leaves them
    /// out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub simulated: bool,
    /// Who injected a [`Self::simulated`] message: the person who ran
    /// `senate chat send`, who may have sent it as someone else. Readers
    /// show "(simulated by <person>)"; `None` on a real message, and on
    /// simulated ones recorded before it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulated_by: Option<crate::Name>,
}

/// The platform identity of an event that began on a platform: which message
/// it *is*. Outbound copies of log events are recorded separately as
/// [`crate::MirrorRef`] rows, because the platform id is only known after the
/// event is already in the log.
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
    /// Always concrete: `senator::*` is resolved before an event is
    /// stored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<Address>,
    /// The `seq` of the thread's root event (a question's seq for its answers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<Seq>,
    /// Set when one party speaks for another: `from: speaker`,
    /// `on_behalf_of: senator::claude` is the Speaker answering for
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
    /// It began as a simulated platform message, or was derived from one
    /// ([`Source::simulated`]).
    pub fn is_simulated(&self) -> bool {
        self.source.as_ref().is_some_and(|s| s.simulated)
    }

    /// True when `addr` is named in `to`. `committee` is a broadcast for
    /// catch-up, not a push to every session, so it does not count here.
    pub fn is_directed_to(&self, addr: &Address) -> bool {
        self.to.iter().any(|a| a == addr)
    }

    /// A hosted terminal needs a person to attend to a permission prompt.
    pub fn is_hosted_approval(&self, people: &crate::People) -> bool {
        self.kind == Kind::Question
            && self.thread.is_none()
            && people.is_agent_address(&self.from)
            && self.on_behalf_of.is_none()
            && matches!(self.to.as_slice(), [Address::Chair | Address::Speaker])
            && self
                .meta
                .get(meta::HOSTED_APPROVAL)
                .and_then(Value::as_bool)
                == Some(true)
    }

    /// The server filed this event in a direct discussion between committees.
    pub fn is_chair_discussion(&self) -> bool {
        match (&self.from, self.meta.get(meta::CHAIR_DISCUSSION)) {
            (Address::Committee, Some(Value::Bool(true))) => {
                self.on_behalf_of.is_some() && self.meta.contains_key(meta::ORIGIN)
            }
            (Address::Senator(_), Some(marker)) => {
                // The HTTP boundary always gave intended messages a
                // recipient before it started writing discussion receipts.
                self.to.is_empty()
                    && self.meta.contains_key(meta::INTENDED)
                    && if self.thread.is_none() {
                        marker.is_string()
                    } else {
                        marker.as_bool() == Some(true)
                    }
            }
            _ => false,
        }
    }

    /// The label for who wrote this: `[senator::claude]`, or
    /// `[speaker::claude]` when the Speaker answered for that senator.
    pub fn author(&self) -> Option<Mention> {
        Mention::author(&self.from, self.on_behalf_of.as_ref())
    }

    /// [`Event::author`] as shown to people: the label, followed by one
    /// localized word when the Speaker wrote it while taken over for an
    /// away agent ([`is_takeover`]) or an agent's companion session wrote it
    /// ([`is_companion`]). The label itself is the same in every language:
    /// `[speaker::claude] 接管`, `[speaker::claude] takeover`.
    pub fn byline(&self, locale: Locale) -> Option<String> {
        if let Some((principal, secretary)) = self.by_secretary() {
            return Some(secretary_byline(principal, secretary, locale));
        }
        let label = self.author()?;
        Some(if is_takeover(self) {
            format!("{label} {}", locale.pick("接管", "takeover"))
        } else if is_companion(self) {
            format!("{label} {}", locale.pick("伴随", "companion"))
        } else {
            label.to_string()
        })
    }

    /// An answer a secretary gave on its principal's behalf while the
    /// principal was away: `from` the secretary, `on_behalf_of` the
    /// principal, flagged [`meta::TAKEOVER`]. The principal and the
    /// secretary.
    pub fn by_secretary(&self) -> Option<(&crate::SenatorName, &crate::SenatorName)> {
        match (&self.from, &self.on_behalf_of) {
            (Address::Senator(secretary), Some(Address::Senator(principal)))
                if flag(self, meta::TAKEOVER) =>
            {
                Some((principal, secretary))
            }
            _ => None,
        }
    }

    pub fn binding(&self) -> Option<BindingPayload> {
        self.meta
            .get(meta::BINDING)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    }
}

/// The byline of an answer `secretary` gave for `principal`: the
/// principal's answer, given by its secretary. One function, both
/// languages.
pub fn secretary_byline(
    principal: &crate::SenatorName,
    secretary: &crate::SenatorName,
    locale: Locale,
) -> String {
    let principal = Address::senator(principal).in_text();
    let secretary = Address::senator(secretary).in_text();
    match locale {
        Locale::Zh => format!("{principal}（由秘书 {secretary} 代答）"),
        Locale::En => format!("{principal} (answered by its secretary {secretary})"),
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
    /// A sender may write `senator::*` here. Whoever accepts the
    /// event resolves it against the roster
    /// ([`Roster::resolve_recipients`](crate::Roster::resolve_recipients))
    /// before it reaches the log; see [`NewEvent::is_resolved`].
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

    pub fn with_binding(self, binding: &BindingPayload) -> Self {
        let value = serde_json::to_value(binding).unwrap_or(Value::Null);
        self.with_meta(meta::BINDING, value)
    }

    /// True when the event can be stored as it is: no address in it is the
    /// wildcard `senator::*`. In `to` a wildcard is waiting to be
    /// resolved; in `from` or `on_behalf_of` it is simply wrong.
    pub fn is_resolved(&self) -> bool {
        !self.from.is_wildcard()
            && !self.on_behalf_of.as_ref().is_some_and(Address::is_wildcard)
            && !self.to.iter().any(Address::is_wildcard)
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

/// Keys of [`Event::meta`] with a meaning shared across crates. A key is
/// owned by the component that writes it; readers rely on these names.
pub mod meta {
    /// `binding` events: the [`BindingPayload`](super::BindingPayload).
    pub const BINDING: &str = "binding";
    /// `status` events: the [`StatusPayload`](crate::task::StatusPayload).
    pub const STATUS: &str = "status";
    /// True on a hosted approval notice. Its archive status carries the
    /// root's event ID under the same key, pinning it across log epochs.
    pub const HOSTED_APPROVAL: &str = "hosted_approval";
    /// The local relay thread and bounded prompt label associated with a hosted approval.
    pub const APPROVAL_WAIT: &str = "approval_wait";
    /// On a person's note that they answered a hosted session's permission
    /// prompt from outside its terminal: `{decision, runtime, session_id}`.
    pub const PROMPT_ANSWER: &str = "prompt_answer";
    /// The Speaker's triage verdict, a
    /// [`TriagePayload`](crate::task::TriagePayload): on a Speaker `note` in
    /// the root's thread, or inline on a Speaker relay. Speaker only; the
    /// server refuses the key from senators. See [`crate::task`].
    pub const TRIAGE: &str = "triage";
    /// `note` from the Speaker: the committee's
    /// [`OutputLevel`](crate::task::OutputLevel). Speaker only.
    pub const OUTPUT_LEVEL: &str = "output_level";
    /// A person's message in an existing thread, written through a
    /// developer-machine tool (`senate reply`, the app's answer, `ask`,
    /// `tell` and `request` into a thread): the highest seq of that thread
    /// the person had been shown when they spoke, a number, from the same
    /// read state as `person_seen`. A person's message ends an escalation
    /// in force only when `seen` is at least its seq, or when there is no
    /// `seen` (a chat platform's reply, an older writer). Nobody has seen
    /// beyond their own words, so the fold reads it as at most the event's
    /// own seq. See [`crate::task`].
    pub const SEEN: &str = "seen";
    /// `chat` in a task thread after an `answer`: the seq of the answer this
    /// correction overrides. A person's reply overrides an earlier answer.
    pub const OVERRIDES: &str = "overrides";
    /// `chat` from a platform: why the platform delivered it, lowercase
    /// (`direct`, `mention`, `reply`, `ambient`).
    pub const ADDRESSING: &str = "addressing";
    /// `chat` from a platform: the message it replies to, if any.
    pub const REPLY_TO: &str = "reply_to_message_ref";
    /// `chat` from a platform: the root of the platform thread holding it.
    pub const THREAD_ROOT: &str = "thread_root_message_ref";
    /// `chat` from a platform: people it mentions, `[{"address", "name"}]`,
    /// where `address` is the person's address here (`senator::alice`, or
    /// `guest::<platform>/<id>` for someone not on the roster). Platform ids
    /// stay out of the log.
    pub const MENTIONS: &str = "mentions";
    /// A Speaker relay: the `seq` of the `chat` it relays.
    pub const RELAYED_FROM: &str = "relayed_from";
    /// `decision`: the seq (or list of seqs) of earlier decisions it replaces.
    /// The brief drops a superseded decision.
    pub const SUPERSEDES: &str = "supersedes";
    /// `note` from the Speaker: `true` marks the brief's "top three things".
    /// Never mirrored to a chat platform. Only the Speaker's notes count;
    /// the server refuses the key from senators.
    pub const BRIEF_TOP: &str = "brief_top";
    /// `note` from an agent: `"decision"`
    /// ([`PROPOSAL_DECISION`](super::PROPOSAL_DECISION)) makes it a proposal
    /// the Speaker asks people to confirm: agents propose, people decide.
    /// The resulting `decision` has `thread` = the proposal's seq.
    pub const PROPOSAL: &str = "proposal";
    /// `decision` confirming a proposal: the proposing agent's address.
    pub const PROPOSED_BY: &str = "proposed_by";
    /// `checkpoint`: the
    /// [`CheckpointPayload`](crate::continuity::CheckpointPayload). Only from
    /// the agent itself; the server checks it.
    pub const CHECKPOINT: &str = "checkpoint";
    /// `presence`: the [`PresencePayload`](crate::continuity::PresencePayload).
    /// Server only.
    pub const PRESENCE: &str = "presence";
    /// `presence`: an explicit [`crate::CommitteeSeatChange`]. Server only.
    pub const COMMITTEE_SEAT: &str = "committee_seat";
    /// `answer` from an agent: `true` when its bridge's companion session (a
    /// headless fork of the developer's last session) wrote it rather than
    /// an interactive session. Shown as a word after the author's label
    /// ([`Event::byline`](super::Event::byline)).
    pub const COMPANION: &str = "companion";
    /// A work run: the [`WorkRun`](crate::work::WorkRun) on the Speaker
    /// `note` that roots the run's thread and on each Speaker `status` in
    /// it. Speaker only; the server refuses the key from senators.
    pub const WORK: &str = "work";
    /// `answer` from the Speaker on an agent's behalf: `true` when it is no
    /// answer at all, only where the question stands ("still with the
    /// agent", "the agent is away"). The asker is told, but the thread stays
    /// open: a person replying next takes it over, and the agent's own
    /// answer is the thread's answer. Speaker only; the server refuses the
    /// key from senators.
    pub const PROVISIONAL: &str = "provisional";
    /// `true` on a Speaker answer written while it had taken over for an
    /// away agent (`on_behalf_of` that agent, handed back to it when it
    /// returns), and on the `status` that sends such a question to people
    /// instead. Only the Speaker's events count. It is this flag, not
    /// anything in the body, that a reader shows as takeover
    /// ([`Event::byline`](super::Event::byline)).
    pub const TAKEOVER: &str = "takeover";
    /// A message the server addressed to the Speaker because its sender
    /// wrote `senator::*` and several agents could get it: the
    /// [`PickPayload`](crate::docket::PickPayload), whose candidates the
    /// Speaker picks among. Written by the server only; the server refuses
    /// the key from senators.
    pub const PICK: &str = "pick";
    /// A message meant for a seat outside the committee it is written in:
    /// the intended recipient as a qualified name
    /// ([`Locator`](crate::Locator)), completed with the Senate and
    /// committee it was resolved to. Plumbing: who decides what becomes of
    /// such a message is not settled yet.
    pub const INTENDED: &str = "intended";
    /// A message that started somewhere else (another committee, another
    /// Senate): an [`Origin`](crate::locator::Origin), naming the seat that
    /// asked and the root of its thread, so an answer can go back there.
    pub const ORIGIN: &str = "origin";
    /// A direct discussion between committees of this Senate. On the
    /// source root, the qualified name of the chair selected at delivery.
    /// On replies and copies, `true`. Written by the server only.
    pub const CHAIR_DISCUSSION: &str = "chair_discussion";
    /// `note` from `committee`, written by the server: the committee's
    /// chair, set explicitly ([`ChairPayload`](crate::chair::ChairPayload)).
    pub const CHAIR: &str = "chair";
    /// On an event `from: speaker`: the senator who acted for the Speaker's
    /// office (the Speaker itself, or which secretary), as a name.
    pub const ACTED_BY: &str = "acted_by";
    /// On the desk's note in an asker's thread: where the message was
    /// forwarded, as a qualified name.
    pub const FORWARDED_TO: &str = "forwarded_to";
    /// `note` from the Speaker: a digest of the discussion, a
    /// [`DigestPayload`](crate::memory::DigestPayload). Speaker only; the
    /// server refuses the key from senators. See [`crate::memory`].
    pub const DIGEST: &str = "digest";
    /// `note`: a memory point, a correction or a deletion, a
    /// [`MemoryPayload`](crate::memory::MemoryPayload). A point is the
    /// Speaker's; a correction or deletion is a person's, written by them or
    /// by the Speaker on their behalf. See [`crate::memory`].
    pub const MEMORY: &str = "memory";
    /// The seqs of the messages something was drawn from, a list: on the
    /// Speaker's proposal of a decision people agreed on in discussion, and
    /// on the decision that confirms it.
    pub const SOURCES: &str = "sources";
    /// The Speaker's proposal of a decision from discussion: what it is
    /// about, a few words, so the same topic is proposed once.
    pub const TOPIC: &str = "topic";
    /// On an answer relayed back into an asker's thread: who answered
    /// where the message was forwarded, as a qualified name.
    pub const ANSWERED_BY: &str = "answered_by";
    /// `note` from a senator: a seat's duty, a
    /// [`DutyPayload`](crate::duty::DutyPayload). Set by the senator itself,
    /// its owner person or the committee's chair; the server refuses anyone
    /// else. See [`crate::duty`].
    pub const DUTY: &str = "duty";
    /// On what the Speaker's office writes in the thread of a message for
    /// a child Senate that was offline: the note that it is held, or the
    /// answer given on the child's behalf. The child Senate's name.
    pub const CHILD_OFFLINE: &str = "child_offline";
    /// On the Speaker's note in a question's thread: the seat the question
    /// was put to is no longer on the committee (unseated here, or no
    /// longer declared by the child Senate it was in), as a qualified
    /// name. The question is closed: the brief no longer lists it as open.
    /// Only the Speaker's note counts.
    pub const SEAT_REMOVED: &str = "seat_removed";
    /// A message in an open thread handing it to another seat, because the
    /// seat it was put to was replaced (`senate session replace`): the
    /// senator (`"claude-…"`) that answers it now. The brief counts that
    /// seat among the question's recipients, so the question stays open
    /// when the replaced seat is unseated instead of being closed as gone.
    pub const HANDED_TO: &str = "handed_to";
}

fn flag(event: &Event, key: &str) -> bool {
    event.meta.get(key).and_then(Value::as_bool) == Some(true)
}

/// True for a [`meta::PROVISIONAL`] answer: one that leaves the thread open.
pub fn is_provisional(event: &Event) -> bool {
    event.kind == Kind::Answer && flag(event, meta::PROVISIONAL)
}

/// True for an event the Speaker wrote while taken over for an away agent
/// ([`meta::TAKEOVER`]). Anyone else's flag does not count.
pub fn is_takeover(event: &Event) -> bool {
    event.from == Address::Speaker && flag(event, meta::TAKEOVER)
}

/// True for an agent's answer written by its companion session
/// ([`meta::COMPANION`]).
pub fn is_companion(event: &Event) -> bool {
    // Only an agent's answer may carry the flag: the server refuses it from
    // anyone else.
    event.kind == Kind::Answer && event.from.as_senator().is_some() && flag(event, meta::COMPANION)
}

/// The [`meta::PROPOSAL`] value for a proposed decision.
pub const PROPOSAL_DECISION: &str = "decision";

/// Who takes part in the thread rooted at `root`: the root's `from`,
/// `on_behalf_of` and `to`, and the `from` of every reply, keeping senators
/// and guests (not `speaker`, `committee` or unknown forms), deduplicated in
/// first-seen order. `replies` are the events whose `thread` is `root.seq`.
pub fn thread_participants<'a>(
    root: &'a Event,
    replies: impl IntoIterator<Item = &'a Event>,
) -> Vec<Address> {
    let mut out: Vec<Address> = Vec::new();
    let mut add = |a: &Address| {
        if matches!(a, Address::Senator(_) | Address::Guest(_)) && !out.contains(a) {
            out.push(a.clone());
        }
    };
    add(&root.from);
    if let Some(a) = &root.on_behalf_of {
        add(a);
    }
    for a in &root.to {
        add(a);
    }
    for reply in replies {
        add(&reply.from);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BindingAction {
    Bind,
    Unbind,
}

/// What a bound group is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupRole {
    /// A people group: people talk here; only answers to people come back.
    #[default]
    People,
    /// The committee's agent group: all agent traffic is mirrored here.
    /// Created by the Speaker, or adopted from an existing group. Unbinding
    /// it is archiving it.
    Agents,
}

/// `meta["binding"]` of a [`Kind::Binding`] event. A gateway's group →
/// committee table is rebuilt by replaying these, whatever the role; the
/// mirror target is the latest bound `agents` group, else the first-bound
/// still-bound people group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingPayload {
    pub action: BindingAction,
    pub platform: String,
    pub group_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
    /// Absent means a people group.
    #[serde(default, skip_serializing_if = "is_people")]
    pub role: GroupRole,
}

fn is_people(role: &GroupRole) -> bool {
    *role == GroupRole::People
}

impl BindingPayload {
    pub const META_KEY: &'static str = meta::BINDING;
}

/// Task-thread states. Carried in `meta["status"]` of a [`Kind::Status`]
/// event as part of a [`StatusPayload`](crate::task::StatusPayload); see
/// [`crate::task`] for how a thread moves between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StatusState {
    Processing,
    Active,
    Suspended,
    Archived,
}

impl StatusState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Processing => "processing",
            Self::Active => "active",
            Self::Suspended => "suspended",
            Self::Archived => "archived",
        }
    }
}

impl fmt::Display for StatusState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use chrono::Utc;

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
            "from": "senator::copilot",
            "to": ["senator::claude"],
            "body": "retry cap?"
        }))
        .unwrap();
        assert_eq!(event.seq, Seq(118));
        assert_eq!(event.committee, committee());
        assert_eq!(event.kind, Kind::Question);
        assert!(event.is_directed_to(&addr("senator::claude")));
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
            "from": "guest:web/reviewer",
            "body": ""
        });
        let event: Event = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(event.kind, Kind::Other("poll".into()));
        assert_eq!(event.from, Address::Other("guest:web/reviewer".into()));
        assert_eq!(event.author(), None);
        assert_eq!(serde_json::to_value(&event).unwrap(), value);
    }

    #[test]
    fn the_speaker_answering_for_a_senator_is_fields_not_text() {
        let ts = "2026-11-16T09:00:00Z".parse().unwrap();
        let answer = NewEvent::new(committee(), Kind::Answer, Address::Speaker, "cap is 3")
            .on_behalf_of(addr("senator::claude"))
            .to(addr("senator::copilot"))
            .in_thread(Seq(118))
            .into_event(Seq(119), ts);

        assert_eq!(answer.body, "cap is 3");
        assert_eq!(answer.author().unwrap().to_string(), "[speaker::claude]");
        assert_eq!(answer.thread, Some(Seq(118)));
    }

    #[test]
    fn empty_refs_are_dropped() {
        let new =
            NewEvent::new(committee(), Kind::Note, Address::Speaker, "").with_refs(Refs::default());
        assert_eq!(new.refs, None);
    }

    #[test]
    fn a_wire_event_with_every_optional_field_round_trips() {
        let json = json!({
            "seq": 118,
            "committee": "refund-v2",
            "id": "01JB0000000000000000000000",
            "ts": "2026-11-16T01:14:03Z",
            "kind": "question",
            "from": "senator::copilot",
            "to": ["senator::cursor", "guest::chat/u_8f3a"],
            "thread": 112,
            "source": { "platform": "chat", "group_id": "backend", "group_name": "退款 v2 · 后端群" },
            "body": "网关的幂等键用哪个 header？",
            "refs": { "commit": "a41f9c2", "path": "gateway/src/idem.rs" },
            "mirror": { "platform": "chat", "message_ref": "mref_1" }
        });
        let event: Event = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(event.kind, Kind::Question);
        assert!(event.is_directed_to(&addr("senator::cursor")));
        assert_eq!(serde_json::to_value(&event).unwrap(), json);
    }

    #[test]
    fn a_new_event_with_a_wildcard_is_not_ready_to_store() {
        let new = NewEvent::new(committee(), Kind::Question, addr("senator::copilot"), "?");
        assert!(new.clone().to(addr("senator::cursor")).is_resolved());
        assert!(!new.clone().to(addr("senator::*")).is_resolved());
        assert!(!new.clone().on_behalf_of(addr("senator::*")).is_resolved());
        let mut from_wildcard = new;
        from_wildcard.from = addr("senator::*");
        assert!(!from_wildcard.is_resolved());
    }

    #[test]
    fn thread_participants_are_the_senators_and_guests_in_it() {
        let root = NewEvent::new(committee(), Kind::Question, Address::Speaker, "q")
            .on_behalf_of(addr("senator::pm"))
            .to(addr("senator::copilot"))
            .into_event(Seq(5), Utc::now());
        let reply = |from: Address| {
            NewEvent::new(committee(), Kind::Answer, from, "a")
                .in_thread(Seq(5))
                .into_event(Seq(6), Utc::now())
        };
        let replies = [
            reply(addr("senator::copilot")),
            reply(Address::Speaker),
            reply(addr("senator::carol")),
            reply(addr("guest::chat/u_1")),
            reply(Address::Committee),
            reply(Address::Other("group:x".into())),
        ];
        assert_eq!(
            thread_participants(&root, &replies),
            vec![
                addr("senator::pm"),
                addr("senator::copilot"),
                addr("senator::carol"),
                addr("guest::chat/u_1"),
            ]
        );
    }

    #[test]
    fn binding_payload_reads_back_and_a_people_group_is_the_default() {
        let b = BindingPayload {
            action: BindingAction::Bind,
            platform: "chat".into(),
            group_id: "backend".into(),
            group_name: None,
            role: GroupRole::People,
        };
        let e = NewEvent::new(committee(), Kind::Binding, Address::Speaker, "")
            .with_binding(&b)
            .into_event(Seq(1), Utc::now());
        assert_eq!(e.binding(), Some(b));
        assert_eq!(
            e.meta[meta::BINDING],
            json!({ "action": "bind", "platform": "chat", "group_id": "backend" })
        );
        let agents: BindingPayload = serde_json::from_value(json!({
            "action": "unbind", "platform": "chat", "group_id": "g", "role": "agents"
        }))
        .unwrap();
        assert_eq!(agents.role, GroupRole::Agents);
    }

    fn answer(from: Address) -> NewEvent {
        NewEvent::new(committee(), Kind::Answer, from, "cap is 3").in_thread(Seq(1))
    }

    #[test]
    fn takeover_is_a_flag_shown_as_a_localized_word_after_the_same_label() {
        let covered = answer(Address::Speaker)
            .on_behalf_of(addr("senator::claude"))
            .with_meta(meta::TAKEOVER, json!(true))
            .into_event(Seq(2), Utc::now());
        assert!(is_takeover(&covered));
        // Nothing about it is written into the body.
        assert_eq!(covered.body, "cap is 3");
        assert_eq!(
            covered.byline(Locale::Zh).unwrap(),
            "[speaker::claude] 接管"
        );
        assert_eq!(
            covered.byline(Locale::En).unwrap(),
            "[speaker::claude] takeover"
        );
        // The label is the one a plain answer on behalf of the agent carries.
        let plain = answer(Address::Speaker)
            .on_behalf_of(addr("senator::claude"))
            .into_event(Seq(3), Utc::now());
        for locale in Locale::ALL {
            assert_eq!(plain.byline(locale).unwrap(), "[speaker::claude]");
            assert!(
                covered
                    .byline(locale)
                    .unwrap()
                    .starts_with(&plain.author().unwrap().to_string())
            );
            crate::testing::assert_senate_vocabulary(&covered.byline(locale).unwrap());
        }
        // Only the Speaker's flag counts.
        let forged = answer(addr("senator::forger"))
            .with_meta(meta::TAKEOVER, json!(true))
            .into_event(Seq(4), Utc::now());
        assert!(!is_takeover(&forged));
        assert_eq!(forged.byline(Locale::En).unwrap(), "[senator::forger]");
    }

    #[test]
    fn a_companion_answer_is_a_flag_shown_as_a_localized_word() {
        let companion = answer(addr("senator::cursor"))
            .with_meta(meta::COMPANION, json!(true))
            .into_event(Seq(2), Utc::now());
        assert!(is_companion(&companion));
        assert_eq!(
            companion.byline(Locale::Zh).unwrap(),
            "[senator::cursor] 伴随"
        );
        assert_eq!(
            companion.byline(Locale::En).unwrap(),
            "[senator::cursor] companion"
        );
        // Only a senator's answer is one: the Speaker has no companion
        // session (the server refuses the flag from a person).
        let speaker = answer(Address::Speaker)
            .with_meta(meta::COMPANION, json!(true))
            .into_event(Seq(3), Utc::now());
        assert!(!is_companion(&speaker));
        // An unknown author has no label, so no byline.
        let unknown = answer(Address::Other("x:y".into())).into_event(Seq(4), Utc::now());
        assert_eq!(unknown.byline(Locale::En), None);
    }

    #[test]
    fn a_provisional_answer_is_an_answer_flagged_so() {
        let stands = answer(Address::Speaker)
            .with_meta(meta::PROVISIONAL, json!(true))
            .into_event(Seq(2), Utc::now());
        assert!(is_provisional(&stands));
        let note = NewEvent::new(committee(), Kind::Note, Address::Speaker, "")
            .with_meta(meta::PROVISIONAL, json!(true))
            .into_event(Seq(3), Utc::now());
        assert!(!is_provisional(&note));
        assert_eq!(StatusState::Suspended.to_string(), "suspended");
        assert_eq!(
            serde_json::to_value(StatusState::Processing).unwrap(),
            "processing"
        );
    }
}
