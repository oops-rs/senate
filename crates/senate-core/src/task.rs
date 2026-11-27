//! Task threads: state, triage, and the view derived from them.
//!
//! A **task thread** is the thread rooted at a `question` or `request`
//! addressed to an agent, or at a Speaker `note` carrying
//! [`meta::WORK`] (a work run, [`crate::work`]). Its
//! state is never stored: [`TaskBoard`] folds it from the log, the same way
//! [`BriefBuilder`](crate::BriefBuilder) folds the brief, so a restarted
//! Speaker, the server's `threads` route and the bridge's inbox all agree
//! and nothing is lost on restart.
//!
//! # Whose task
//!
//! A task is addressed to agents by path, and a person may have several. So
//! a task belongs to the agents its root names ([`ThreadSummary::is_for`]),
//! never to "the agent of" a person, and the session that picks it up is
//! recorded together with the agent it belongs to ([`TaskOwner`]): a session
//! id means nothing without the agent whose bridge issued it. People appear
//! only where a person is meant, and are typed as one: who a thread waits on
//! ([`StatusPayload::escalated_to`]) and who took it over
//! ([`StatusPayload::takeover`]).
//!
//! # State
//!
//! The four states of [`StatusState`] move like this:
//!
//! - the root arrives → `processing`;
//! - an `answer` in the thread → `active` (unless it is `archived`);
//! - a `status` event in the thread sets the state, and whatever else its
//!   [`StatusPayload`] carries, explicitly. That is how an agent reports
//!   progress (`processing` with steps), how the Speaker parks a thread on a
//!   person (`suspended`), how an escalation and a person's takeover are
//!   recorded, and how a thread is closed (`archived`).
//!
//! # What waits
//!
//! An inbox shows what waits on someone, not everything not archived:
//! [`ThreadSummary::waits`] is the one rule. A thread whose last word is an
//! answer is settled ([`ThreadSummary::is_settled`]) and left out of the
//! default listing; a follow-up after the answer, an escalation or an
//! explicit in-progress status brings it back. Nothing is stored for it.
//!
//! That rule is an *agent's*: "the last word is an answer, so nobody waits"
//! is true of its inbox. A person's is [`ThreadSummary::waits_on_person`]:
//! what was said to them that they have not seen, and what was escalated to
//! them and not yet acted on.
//!
//! # An escalation lasts until the person acts
//!
//! A `status` with `escalated_to` is a hand-off to a person
//! ([`Escalation`]). It stays in force, whatever state later statuses name,
//! until
//!
//! - that person says anything in the thread (a reply, a correction, an
//!   answer, a decision) having seen it, or takes it over. A message from a
//!   developer-machine tool carries [`meta::SEEN`], the highest seq of the
//!   thread shown to them; it ends the escalation only when `seen` is at
//!   least the escalation's seq. A reply written before the escalation was
//!   shown (they crossed) leaves it in force, and the thread keeps waiting on
//!   the person. A message without `seen` (a chat platform's, an older
//!   writer) ends it as it always did;
//! - or whoever made it withdraws it with a `status` carrying
//!   [`StatusPayload::withdraw`] (the `status` tool's `withdraw_escalation`);
//! - or the thread is archived.
//!
//! Meanwhile the agent's own progress updates still change the title, steps
//! and links, but not the state, and do not replace the reason.
//!
//! # Triage gating
//!
//! A root for which [`needs_triage`] is true is not pushed to the target
//! agent's session until the Speaker has judged it. The verdict is a `note`
//! from the Speaker with `thread` = the root's seq and
//! [`meta::TRIAGE`] = a [`TriagePayload`]. A relay the
//! Speaker itself writes carries its verdict inline in `meta.triage`, and is
//! pushed at once. The bridge acts on the verdict:
//!
//! - [`TriageVerdict::Agent`]: push it.
//! - [`TriageVerdict::Unsure`]: push it, flagged as possibly needing the
//!   person, so the agent asks its developer in the terminal before
//!   answering.
//! - [`TriageVerdict::NeedsHuman`]: do not push it. It still shows up on
//!   catch-up, marked as waiting on the person.
//!
//! When no verdict arrives within [`TRIAGE_WAIT_SECS`] (an older server, or
//! a Speaker with neither model nor rules for it), the bridge pushes as
//! `unsure`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::address::Address;
use crate::event::{Event, Kind, StatusState, is_provisional, meta};
use crate::ids::{Name, Seq};
use crate::roster::People;
use crate::senator::SenatorName;

/// How long a bridge holds a root that [`needs_triage`] for a verdict.
pub const TRIAGE_WAIT_SECS: u64 = 10;
/// Most steps a [`StatusPayload`] may carry.
pub const MAX_STEPS: usize = 20;
/// Most links a [`StatusPayload`] may carry.
pub const MAX_LINKS: usize = 8;
/// Longest step label or link label, in characters.
pub const MAX_LABEL_CHARS: usize = 80;
/// Longest task title, in characters.
pub const MAX_TITLE_CHARS: usize = 80;
/// Longest phase label on a status card, in characters.
pub const MAX_PHASE_CHARS: usize = 40;
/// Most artifacts a [`StatusPayload`] may carry.
pub const MAX_ARTIFACTS: usize = 8;
/// Longest artifact label, in characters.
pub const MAX_ARTIFACT_LABEL_CHARS: usize = 24;
/// Longest artifact value, in characters.
pub const MAX_ARTIFACT_VALUE_CHARS: usize = 80;

/// One line of a task's checklist ("read the log · checked
/// gateway/src/idem.rs · drafting the answer"). The label is the agent's
/// own words, in whatever language it wrote them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    /// Stable within the thread, so a later status advances the same step.
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub done: bool,
}

/// A link on a task's card: branch, PR, preview, document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub label: String,
    pub url: String,
}

/// Structured delivery evidence attached to a task card (`commit`, `tag`, `host`, `pr`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub label: String,
    pub value: String,
}

/// Read-only mirror of a cross-committee downstream task card, projected
/// onto the origin thread when the reader also has access to `committee`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkedTaskSummary {
    pub committee: crate::CommitteeId,
    pub root: Seq,
    pub state: StatusState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    pub steps_done: u32,
    pub steps_total: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<Escalation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_owner: Option<Address>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_acceptance: bool,
    pub updated: Seq,
}

impl LinkedTaskSummary {
    pub fn from_thread(committee: crate::CommitteeId, thread: &ThreadSummary) -> Self {
        let steps_total = u32::try_from(thread.steps.len()).unwrap_or(u32::MAX);
        let steps_done =
            u32::try_from(thread.steps.iter().filter(|s| s.done).count()).unwrap_or(u32::MAX);
        Self {
            committee,
            root: thread.root,
            state: thread.state,
            phase: thread.phase.clone(),
            steps_done,
            steps_total,
            escalation: thread.escalation.clone(),
            artifacts: thread.artifacts.clone(),
            next_owner: thread.next_owner.clone(),
            awaiting_acceptance: thread.awaiting_acceptance,
            updated: thread.updated,
        }
    }
}

/// `meta["status"]` of a [`Kind::Status`] event, in the thread of a task.
///
/// Every field but `state` is optional, and `None` means "unchanged".
/// `steps`, `links`, and `artifacts`, when present, replace the previous list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusPayload {
    pub state: StatusState,
    /// The task's name, given by the agent that takes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<Step>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<Link>>,
    /// Short engineering phase badge (`<= 40` chars, single-line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Key structured delivery artifacts (`<= 8` items).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
    /// Who owns the next handoff or verification step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_owner: Option<Address>,
    /// True when the agent delivered an answer/handoff that awaits explicit acceptance.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_acceptance: bool,
    /// The bridge session that owns the thread: one thread, one session in
    /// charge. Set by the bridge on its agent's first status in the thread;
    /// follow-ups in the thread go to this session only. The id is the
    /// bridge's own, so it is read together with the event's `from`
    /// ([`TaskOwner`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// An escalation: the person the request now waits on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated_to: Option<Name>,
    /// The person who took the thread over: they replied before any answer,
    /// or took it explicitly. Bridges cancel any queued delivery of the
    /// thread when they see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover: Option<Name>,
    /// Why, in one line, as its writer put it: the triage rule, the
    /// escalation reason, who stopped it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Withdraws the escalation in force, when the author of this status is
    /// the one who made it. An ordinary progress update never does.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub withdraw: bool,
}

impl StatusPayload {
    pub const META_KEY: &'static str = meta::STATUS;

    pub fn new(state: StatusState) -> Self {
        Self {
            state,
            title: None,
            steps: None,
            links: None,
            phase: None,
            artifacts: Vec::new(),
            next_owner: None,
            awaiting_acceptance: false,
            session: None,
            escalated_to: None,
            takeover: None,
            reason: None,
            withdraw: false,
        }
    }

    /// The payload of a `status` event, if it is one and it parses.
    pub fn of(event: &Event) -> Option<Self> {
        if event.kind != Kind::Status {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::STATUS)?.clone()).ok()
    }

    /// The payload as a `meta` value.
    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// What is wrong with it as a senator's append, if anything: limits on
    /// steps, links, phase, artifacts and labels.
    pub fn check(&self) -> Result<(), String> {
        if self.withdraw
            && (self.escalated_to.is_some()
                || self.takeover.is_some()
                || self.state == StatusState::Suspended)
        {
            return Err(
                "a withdrawal names the state the thread goes back to (not suspended) and \
                 neither escalates nor takes over"
                    .to_string(),
            );
        }
        let long = |s: &str, max: usize| s.chars().count() > max;
        let multiline = |s: &str| s.contains('\n') || s.contains('\r');
        if self
            .title
            .as_deref()
            .is_some_and(|t| long(t, MAX_TITLE_CHARS))
        {
            return Err(format!("title is over {MAX_TITLE_CHARS} characters"));
        }
        if let Some(phase) = self.phase.as_deref() {
            if multiline(phase) {
                return Err("phase must be a single line".to_string());
            }
            if long(phase, MAX_PHASE_CHARS) {
                return Err(format!("phase is over {MAX_PHASE_CHARS} characters"));
            }
        }
        if self.artifacts.len() > MAX_ARTIFACTS {
            return Err(format!("more than {MAX_ARTIFACTS} artifacts"));
        }
        for artifact in &self.artifacts {
            let label = artifact.label.trim();
            let value = artifact.value.trim();
            if label.is_empty() || value.is_empty() {
                return Err("artifact label and value must not be empty".to_string());
            }
            if multiline(&artifact.label) || multiline(&artifact.value) {
                return Err("artifact label and value must be single-line".to_string());
            }
            if long(&artifact.label, MAX_ARTIFACT_LABEL_CHARS) {
                return Err(format!(
                    "an artifact label is over {MAX_ARTIFACT_LABEL_CHARS} characters"
                ));
            }
            if long(&artifact.value, MAX_ARTIFACT_VALUE_CHARS) {
                return Err(format!(
                    "an artifact value is over {MAX_ARTIFACT_VALUE_CHARS} characters"
                ));
            }
        }
        if let Some(steps) = &self.steps {
            if steps.len() > MAX_STEPS {
                return Err(format!("more than {MAX_STEPS} steps"));
            }
            if steps.iter().any(|s| long(&s.label, MAX_LABEL_CHARS)) {
                return Err(format!("a step label is over {MAX_LABEL_CHARS} characters"));
            }
        }
        if let Some(links) = &self.links {
            if links.len() > MAX_LINKS {
                return Err(format!("more than {MAX_LINKS} links"));
            }
            if links.iter().any(|l| long(&l.label, MAX_LABEL_CHARS)) {
                return Err(format!("a link label is over {MAX_LABEL_CHARS} characters"));
            }
        }
        Ok(())
    }
}

/// The Speaker's judgement of an incoming root.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriageVerdict {
    /// A person must judge it: the Speaker mentions them, the agent is not
    /// pushed it.
    NeedsHuman,
    /// The agent can answer it from code or the log.
    Agent,
    /// Pushed to the agent flagged, so it asks its developer first.
    Unsure,
}

/// `meta["triage"]`: on a Speaker `note` in the root's thread, or inline on a
/// relay the Speaker writes. Written by the Speaker only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriagePayload {
    pub verdict: TriageVerdict,
    /// The classifier's probabilities; all zero when a hard rule decided.
    #[serde(default)]
    pub p_needs_human: f32,
    #[serde(default)]
    pub p_agent: f32,
    #[serde(default)]
    pub p_unsure: f32,
    /// The hard rule that matched (`request`, `other_committee`,
    /// `sensitive`, `checkpoint`, ...), if one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// One line, shown to people in the thread, in the committee's language
    /// as the Speaker wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl TriagePayload {
    pub const META_KEY: &'static str = meta::TRIAGE;

    /// The verdict carried by `event`'s `meta.triage`, if any.
    pub fn of(event: &Event) -> Option<Self> {
        serde_json::from_value(event.meta.get(meta::TRIAGE)?.clone()).ok()
    }

    /// The payload as a `meta` value.
    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Whether `event` is a root the bridge must hold for a triage verdict: a
/// `question` or `request` that starts a thread, is addressed to at least one
/// agent (a senator not among `people`), was not written by the Speaker,
/// belongs to no direct chair discussion, and carries no verdict of its own.
pub fn needs_triage(event: &Event, people: &People) -> bool {
    matches!(event.kind, Kind::Question | Kind::Request)
        && event.thread.is_none()
        && event.from != Address::Speaker
        && !event.is_chair_discussion()
        && event.to.iter().any(|a| people.is_agent_address(a))
        && !event.meta.contains_key(meta::TRIAGE)
}

/// How much agent traffic the committee's agent group gets. Set by a Speaker
/// `note` carrying [`meta::OUTPUT_LEVEL`]; the
/// latest one wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputLevel {
    /// Answers only. Cards are posted but not updated until they finish.
    Answers,
    /// Answers plus card updates and triage notes.
    #[default]
    Status,
    /// Everything: also every step and every agent ↔ agent message as thread
    /// replies.
    All,
}

impl OutputLevel {
    pub fn of(event: &Event) -> Option<Self> {
        if event.from != Address::Speaker {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::OUTPUT_LEVEL)?.clone()).ok()
    }
}

/// Who is in charge of a task thread: one session of one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskOwner {
    /// The agent whose status claimed the thread.
    pub agent: SenatorName,
    /// That agent's bridge's id for the session.
    pub session: String,
}

/// One task thread as [`TaskBoard`] sees it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    /// The root's seq, which is the thread's id.
    pub root: Seq,
    /// `question` or `request`; `note` for a work run.
    pub kind: Kind,
    pub from: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Address>,
    pub to: Vec<Address>,
    /// The agents among `to`: the senators that are not people.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<SenatorName>,
    /// The status title, else the start of the root's body.
    pub title: String,
    pub state: StatusState,
    /// The seq at which the thread entered `state`.
    pub since: Seq,
    pub since_ts: DateTime<Utc>,
    /// The last seq in the thread.
    pub updated: Seq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<ThreadActivity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
    /// The first agent session to claim the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TaskOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage: Option<TriageVerdict>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalated_to: Option<Name>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeover: Option<Name>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Whether any `answer` has been posted in the thread.
    pub answered: bool,
    /// Who wrote the latest (non-provisional) answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<Address>,
    /// Something was said in the thread after the latest answer by someone
    /// other than its author (a follow-up, a correction): the answer does
    /// not settle it any more. A new answer clears it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reopened: bool,
    /// The escalation in force: a hand-off to a person that nobody has
    /// withdrawn and the person has not acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation: Option<Escalation>,
    /// For each person, the latest event in the thread addressed to them
    /// (their `to`) and the latest escalation to them, that they have not
    /// answered by speaking in the thread themselves.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub directed: Vec<Directed>,
    /// Person who authorized/approved the root on the Speaker's desk (`meta::DESK_PERSON`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desk_person: Option<Name>,
    /// Execution authorization attached to the root (`meta::EXECUTION_AUTH`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_auth: Option<crate::ExecutionAuthPayload>,
    /// Cross-committee origin carried on a forwarded root (`meta::ORIGIN`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<crate::Origin>,
    /// Seq of the latest non-provisional `Answer` in the thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_seq: Option<Seq>,
    /// First non-empty line of the latest non-provisional `Answer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_line: Option<String>,
    /// Latest engineering phase badge (`StatusPayload::phase`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Latest delivery artifacts (`StatusPayload::artifacts`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<ArtifactRef>,
    /// Next handoff/verification owner (`StatusPayload::next_owner`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_owner: Option<Address>,
    /// True while awaiting explicit acceptance/archive after delivery.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_acceptance: bool,
    /// Server-projected read-only mirror of a cross-committee downstream thread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_downstream: Option<LinkedTaskSummary>,
}

/// Log metadata used by committee-wide readers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadActivity {
    pub last_at: DateTime<Utc>,
    pub last_author: String,
    pub opener: Option<String>,
    pub addressee: Option<String>,
    pub message_count: u64,
    pub other_committee: Option<crate::CommitteeId>,
}

fn other_committee(event: &Event) -> Option<crate::CommitteeId> {
    crate::Origin::of(event)
        .map(|origin| origin.locator)
        .into_iter()
        .chain(crate::locator::intended(event))
        .filter_map(|locator| locator.committee().cloned())
        .find(|committee| committee != &event.committee)
}

fn activity_author(event: &Event) -> String {
    if event.is_chair_discussion()
        && let Some(origin) = crate::Origin::of(event)
    {
        return origin.locator.to_string();
    }
    event
        .on_behalf_of
        .as_ref()
        .unwrap_or(&event.from)
        .to_string()
}

fn is_message(event: &Event) -> bool {
    matches!(
        event.kind,
        Kind::Question | Kind::Request | Kind::Answer | Kind::Note | Kind::Chat
    )
}

/// Longest first line kept for [`Directed::line`], in characters.
pub const DIRECTED_LINE_CHARS: usize = 200;

/// A hand-off to a person that is still in force.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Escalation {
    pub to: Name,
    /// Who made it: the agent (or the Speaker) whose status it was.
    pub by: Address,
    /// The status event that made it.
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    /// Why, as its author wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// An event in a task thread addressed to a person.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Directed {
    pub person: Name,
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    pub from: Address,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<Address>,
    pub kind: Kind,
    /// The event is an escalation to the person (a `status`); `line` is
    /// then its reason.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub escalated: bool,
    /// The first line of the body (the reason, for an escalation).
    pub line: String,
}

/// Why a thread waits on a person ([`ThreadSummary::waits_on_person`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersonWait<'a> {
    /// An escalation to them is in force.
    pub escalation: Option<&'a Escalation>,
    /// The latest event addressed to them that they have not seen.
    pub unread: Option<&'a Directed>,
}

/// Who a thread that [waits](ThreadSummary::waits) waits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitsOn {
    /// The agents it was addressed to.
    Agents,
    /// A person: it was escalated or parked on them, or they took it over.
    Person,
    /// Delivered and explicitly marked `awaiting_acceptance`; awaits verification or archive.
    Acceptance { owner: Address },
}

impl ThreadSummary {
    /// The agents the task was addressed to, in the root's order.
    pub fn agents(&self) -> impl Iterator<Item = &SenatorName> {
        self.agents.iter()
    }

    /// Whether the task was addressed to exactly this agent. Another agent
    /// of the same person does not count.
    pub fn is_for(&self, agent: &SenatorName) -> bool {
        self.agents().any(|a| a == agent)
    }

    /// The person an escalation in force hands the thread to: what every
    /// listing shows as "escalated to". It is the field [`waits`](Self::waits)
    /// reads, so it is gone as soon as the escalation ends (the person acted,
    /// it was withdrawn, the thread was taken over or archived), unlike
    /// [`ThreadSummary::escalated_to`], which only remembers the latest one.
    pub fn escalated_now(&self) -> Option<&Name> {
        self.escalation.as_ref().map(|e| &e.to)
    }

    /// Resolved owner for a pending acceptance handoff: explicit `next_owner`,
    /// else sponsor (`execution_auth`, `desk_person`, `on_behalf_of`, `from`).
    pub fn acceptance_owner(&self) -> Address {
        if let Some(owner) = &self.next_owner {
            return owner.clone();
        }
        if let Some(auth) = &self.execution_auth {
            return Address::senior(&auth.authorized_by);
        }
        if let Some(person) = &self.desk_person {
            return Address::senior(person);
        }
        self.on_behalf_of
            .clone()
            .unwrap_or_else(|| self.from.clone())
    }

    /// Whether the thread waits on someone, and on whom. This is the one
    /// rule every inbox uses (the `senate inbox` command, the `inbox` tool,
    /// the Speaker's per-agent count, the app's inbox); it is derived and
    /// stores nothing.
    pub fn waits(&self) -> Option<WaitsOn> {
        match self.state {
            StatusState::Archived => None,
            _ if self.escalation.is_some() => Some(WaitsOn::Person),
            StatusState::Suspended => Some(WaitsOn::Person),
            _ if self.awaiting_acceptance => Some(WaitsOn::Acceptance {
                owner: self.acceptance_owner(),
            }),
            StatusState::Processing => Some(WaitsOn::Agents),
            StatusState::Active => (!self.answered || self.reopened).then_some(WaitsOn::Agents),
        }
    }

    /// Whether the thread waits on `person`, and why: this is the person's
    /// rule where [`ThreadSummary::waits`] is an agent's, and it is the one
    /// every reader of a person's inbox uses (`senate inbox`, the app).
    ///
    /// It waits on them when it is not archived and
    ///
    /// - an [escalation](Escalation) to them is in force, whatever they have
    ///   seen: it lasts until they act; or
    /// - the thread holds an event addressed to them (`to` contains them, or
    ///   an escalation to them) with a seq above `seen`, the highest seq of
    ///   it they have seen. An answer or a note the person has not read is
    ///   what their inbox is for, though the thread is settled for agents.
    ///
    /// Something the person said in the thread themselves answers
    /// everything before it ([`ThreadSummary::directed`]).
    pub fn waits_on_person(&self, person: &Name, seen: Seq) -> Option<PersonWait<'_>> {
        if self.state == StatusState::Archived {
            return None;
        }
        let escalation = self.escalation.as_ref().filter(|e| &e.to == person);
        let unread = self
            .directed
            .iter()
            .filter(|d| &d.person == person && d.seq > seen)
            .max_by_key(|d| d.seq);
        (escalation.is_some() || unread.is_some()).then_some(PersonWait { escalation, unread })
    }

    /// What was addressed to `person` in the thread above `seen`, oldest
    /// first.
    pub fn unread_for(&self, person: &Name, seen: Seq) -> Vec<&Directed> {
        let mut out: Vec<&Directed> = self
            .directed
            .iter()
            .filter(|d| &d.person == person && d.seq > seen)
            .collect();
        out.sort_by_key(|d| d.seq);
        out
    }

    /// The latest event in the thread addressed to `person` that they have
    /// not answered by speaking.
    pub fn directed_to(&self, person: &Name) -> Option<&Directed> {
        self.directed
            .iter()
            .filter(|d| &d.person == person)
            .max_by_key(|d| d.seq)
    }

    /// Open (not archived) and waiting on no one: its last word is an
    /// answer.
    pub fn is_settled(&self) -> bool {
        self.state != StatusState::Archived && self.waits().is_none()
    }

    /// Whether `person` sponsored this task: initiated directly as themselves,
    /// approved it in person on the Speaker's desk (`meta::DESK_PERSON`), or
    /// authorized execution (`meta::EXECUTION_AUTH`, directly or via proxy).
    pub fn is_sponsored_by(&self, person: &Name) -> bool {
        is_root_sponsored_by(
            &self.from,
            self.desk_person.as_ref(),
            self.execution_auth.as_ref(),
            person,
        )
    }

    /// Sort bucket for "My Commissions" / 「委派追踪」 given how far `person`
    /// has seen this thread.
    pub fn commission_bucket(&self, seen: Seq) -> CommissionBucket {
        if self.state == StatusState::Suspended || self.escalation.is_some() {
            CommissionBucket::Blocked
        } else if self.awaiting_acceptance && self.state != StatusState::Archived {
            CommissionBucket::PendingAcceptance
        } else if self.answered && !self.reopened && self.state != StatusState::Processing {
            let unread = self
                .answer_seq
                .map_or(self.updated > seen, |seq| seq > seen);
            if unread {
                CommissionBucket::CompletedUnread
            } else {
                CommissionBucket::CompletedRead
            }
        } else {
            CommissionBucket::InProgress
        }
    }
}

fn is_root_sponsored_by(
    from: &Address,
    desk_person: Option<&Name>,
    execution_auth: Option<&crate::ExecutionAuthPayload>,
    person: &Name,
) -> bool {
    person_of(from).as_ref() == Some(person)
        || desk_person == Some(person)
        || execution_auth.is_some_and(|auth| &auth.authorized_by == person)
}

/// Unified commission priority bucket (`Blocked → PendingAcceptance → CompletedUnread → InProgress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommissionBucket {
    Blocked = 0,
    /// Reserved hook for Phase C W3 (`awaiting_acceptance`).
    PendingAcceptance = 1,
    CompletedUnread = 2,
    InProgress = 3,
    CompletedRead = 4,
}

impl CommissionBucket {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::PendingAcceptance => "pending_acceptance",
            Self::CompletedUnread => "completed_unread",
            Self::InProgress => "in_progress",
            Self::CompletedRead => "completed_read",
        }
    }
}

/// Notification signal kind emitted for a sponsored task thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommissionSignalKind {
    Blocked,
    Completed,
}

/// Root metadata cached by the bridge to classify sponsored task events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionRoot {
    pub from: Address,
    pub desk_person: Option<Name>,
    pub execution_auth: Option<crate::ExecutionAuthPayload>,
    pub origin: Option<crate::Origin>,
    pub answered: bool,
}

impl CommissionRoot {
    pub fn from_event(root: &Event) -> Self {
        Self {
            from: root.from.clone(),
            desk_person: root_desk_person(root),
            execution_auth: crate::ExecutionAuthPayload::of(root),
            origin: crate::Origin::of(root),
            answered: false,
        }
    }

    pub fn is_sponsored_by(&self, person: &Name) -> bool {
        is_root_sponsored_by(
            &self.from,
            self.desk_person.as_ref(),
            self.execution_auth.as_ref(),
            person,
        )
    }
}

/// A deduplicated notification signal for the human sponsor of a task thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommissionSignal {
    pub kind: CommissionSignalKind,
    /// Canonical thread key: origin `(committee, thread)` when forwarded across
    /// committees, otherwise `(committee, root)`.
    pub origin_committee: crate::CommitteeId,
    pub origin_root: Seq,
    /// Per-signal dedup key (blocker seq for `Blocked`, canonical downstream
    /// answer `EventId` for `Completed`).
    pub discriminator: String,
    pub text: String,
}

/// Classify `event` in a thread rooted at `root` for `person`:
/// - `StatusState::Suspended` (without takeover) -> `Blocked`
/// - First non-provisional `Kind::Answer` -> `Completed`
/// - Progress / `Processing` / unrelated threads -> `None`
pub fn commission_signal(
    event: &Event,
    root: &CommissionRoot,
    person: &Name,
    already_answered: bool,
) -> Option<CommissionSignal> {
    if !root.is_sponsored_by(person)
        || person_of(&event.from).as_ref() == Some(person)
        || is_provisional(event)
    {
        return None;
    }
    let (origin_committee, origin_root) = root
        .origin
        .as_ref()
        .and_then(|o| o.locator.committee().map(|c| (c.clone(), o.thread)))
        .unwrap_or_else(|| (event.committee.clone(), event.thread.unwrap_or(event.seq)));

    match event.kind {
        Kind::Status => {
            let status = StatusPayload::of(event)?;
            if status.state == StatusState::Suspended && status.takeover.is_none() {
                Some(CommissionSignal {
                    kind: CommissionSignalKind::Blocked,
                    origin_committee,
                    origin_root,
                    discriminator: event.seq.0.to_string(),
                    text: status.reason.unwrap_or_default(),
                })
            } else {
                None
            }
        }
        Kind::Answer if !already_answered => {
            let discriminator = event
                .meta
                .get(meta::RELAYED_EVENT_ID)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| event.id.to_string());
            Some(CommissionSignal {
                kind: CommissionSignalKind::Completed,
                origin_committee,
                origin_root,
                discriminator,
                text: event.body.clone(),
            })
        }
        _ => None,
    }
}

fn root_desk_person(event: &Event) -> Option<Name> {
    event
        .meta
        .get(meta::DESK_PERSON)
        .and_then(Value::as_str)
        .and_then(|raw| Name::new(raw).ok())
}

/// Characters of the root's body used as a title when the task has none.
pub const UNTITLED_CHARS: usize = 40;

/// Folds the log into the committee's task threads. Like the brief builder,
/// [`TaskBoard::observe`] ignores an event at or below the last one it saw.
#[derive(Debug, Clone, Default)]
pub struct TaskBoard {
    head: Seq,
    threads: BTreeMap<Seq, ThreadSummary>,
    discussions: BTreeMap<Seq, ThreadSummary>,
    delivered_targets: BTreeMap<Seq, (crate::CommitteeId, Seq)>,
    include_discussions: bool,
    output_level: OutputLevel,
    /// Who the Senate's people are: a `senator::` address that is not one
    /// of them is an agent's.
    people: People,
}

impl TaskBoard {
    /// The board of a committee whose Senate's people are `people`.
    pub fn new(people: People) -> Self {
        Self {
            people,
            ..Self::default()
        }
    }

    /// A separate, on-demand fold for readers outside the task inbox.
    pub fn with_discussions(people: People) -> Self {
        Self {
            include_discussions: true,
            ..Self::new(people)
        }
    }

    /// The last seq folded in.
    pub fn head(&self) -> Seq {
        self.head
    }

    pub fn output_level(&self) -> OutputLevel {
        self.output_level
    }

    pub fn get(&self, root: Seq) -> Option<&ThreadSummary> {
        self.threads.get(&root)
    }

    /// Downstream `(committee, root_seq)` when the Speaker delivered `root` across committees.
    pub fn delivered_target(&self, root: Seq) -> Option<&(crate::CommitteeId, Seq)> {
        self.delivered_targets.get(&root)
    }

    /// All unarchived questions from one author, including desk discussions.
    pub fn open_questions_by<'a>(
        &'a self,
        author: &'a Address,
    ) -> impl Iterator<Item = &'a ThreadSummary> {
        self.threads
            .values()
            .chain(self.discussions.values())
            .filter(move |thread| {
                &thread.from == author
                    && thread.kind == Kind::Question
                    && thread.state != StatusState::Archived
            })
    }

    /// Threads in `state` (all when `None`), longest in that state first.
    pub fn threads(&self, state: Option<StatusState>) -> Vec<&ThreadSummary> {
        let mut out: Vec<_> = self
            .threads
            .values()
            .filter(|t| state.is_none_or(|s| t.state == s))
            .collect();
        out.sort_by_key(|t| (t.since, t.root));
        out
    }

    /// Committee discussion threads in addition to tasks. Unanswered standalone
    /// notes and chats do not form discussion threads until they get a reply.
    pub fn committee_threads(
        &self,
        state: Option<StatusState>,
        include_archived: bool,
        before: Option<Seq>,
        limit: usize,
    ) -> Vec<&ThreadSummary> {
        let mut rows: Vec<_> = self
            .threads
            .values()
            .chain(self.discussions.values().filter(|t| {
                t.updated > t.root
                    || matches!(t.kind, Kind::Question | Kind::Request)
                    || t.activity
                        .as_ref()
                        .is_some_and(|a| a.other_committee.is_some())
            }))
            .filter(|t| state.is_none_or(|s| t.state == s))
            .filter(|t| include_archived || t.state != StatusState::Archived)
            .filter(|t| before.is_none_or(|before| t.updated < before))
            .collect();
        rows.sort_unstable_by_key(|t| std::cmp::Reverse(t.updated));
        rows.truncate(limit);
        rows
    }

    /// [`TaskBoard::threads`] narrowed to those addressed to `agent`: one
    /// agent's inbox, not its person's.
    pub fn threads_for(
        &self,
        agent: &SenatorName,
        state: Option<StatusState>,
    ) -> Vec<&ThreadSummary> {
        let mut out = self.threads(state);
        out.retain(|t| t.is_for(agent));
        out
    }

    pub fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        if let Some(level) = OutputLevel::of(event) {
            self.output_level = level;
        }
        match event.thread {
            None => self.observe_root(event),
            Some(root) => self.observe_reply(root, event),
        }
    }

    fn observe_root(&mut self, event: &Event) {
        let is_task = (matches!(event.kind, Kind::Question | Kind::Request)
            && event.to.iter().any(|a| self.people.is_agent_address(a)))
            || crate::work::is_work_root(event);
        let agents = event
            .to
            .iter()
            .filter_map(|a| self.people.as_agent(a).cloned())
            .collect();
        let threads = if is_task {
            &mut self.threads
        } else if self.include_discussions && is_message(event) {
            &mut self.discussions
        } else {
            return;
        };
        threads.insert(
            event.seq,
            ThreadSummary {
                root: event.seq,
                kind: event.kind.clone(),
                from: event.from.clone(),
                on_behalf_of: event.on_behalf_of.clone(),
                to: event.to.clone(),
                agents,
                title: untitled(&event.body),
                state: StatusState::Processing,
                since: event.seq,
                since_ts: event.ts,
                updated: event.seq,
                activity: self.include_discussions.then(|| ThreadActivity {
                    last_at: event.ts,
                    last_author: activity_author(event),
                    opener: event.is_chair_discussion().then(|| activity_author(event)),
                    addressee: event
                        .to
                        .is_empty()
                        .then(|| crate::locator::intended(event).map(|locator| locator.to_string()))
                        .flatten(),
                    message_count: u64::from(is_message(event)),
                    other_committee: other_committee(event),
                }),
                steps: Vec::new(),
                links: Vec::new(),
                owner: None,
                triage: TriagePayload::of(event).map(|t| t.verdict),
                escalated_to: None,
                takeover: None,
                reason: None,
                answered: false,
                answered_by: None,
                reopened: false,
                escalation: None,
                directed: Vec::new(),
                desk_person: root_desk_person(event),
                execution_auth: crate::ExecutionAuthPayload::of(event),
                origin: crate::Origin::of(event),
                answer_seq: None,
                answer_line: None,
                phase: None,
                artifacts: Vec::new(),
                next_owner: None,
                awaiting_acceptance: false,
                linked_downstream: None,
            },
        );
    }

    fn observe_reply(&mut self, root: Seq, event: &Event) {
        if event.from == Address::Speaker
            && let Some(outcome) = event.meta.get(meta::DESK_OUTCOME)
            && outcome.get("kind").and_then(Value::as_str) == Some("delivered")
            && let Some(committee) = outcome
                .get("committee")
                .and_then(Value::as_str)
                .and_then(|c| crate::CommitteeId::new(c).ok())
            && let Some(seq) = outcome.get("seq").and_then(Value::as_u64)
        {
            self.delivered_targets.insert(root, (committee, Seq(seq)));
        }
        let Some(thread) = self
            .threads
            .get_mut(&root)
            .or_else(|| self.discussions.get_mut(&root))
        else {
            return;
        };
        thread.updated = event.seq;
        if let Some(activity) = &mut thread.activity {
            activity.last_at = event.ts;
            activity.last_author = activity_author(event);
            activity.message_count += u64::from(is_message(event));
            if activity.other_committee.is_none() {
                activity.other_committee = other_committee(event);
            }
        }
        let enter = |thread: &mut ThreadSummary, state: StatusState| {
            if thread.state != state {
                thread.state = state;
                thread.since = event.seq;
                thread.since_ts = event.ts;
            }
        };
        if event.from == Address::Speaker
            && let Some(triage) = TriagePayload::of(event)
        {
            thread.triage = Some(triage.verdict);
        }
        // A person speaking in the thread answers what was addressed to
        // them before, and ends an escalation to them: it lasts until they
        // act. A provisional answer is the Speaker's, not theirs.
        let by_person = person_of(event.uplink_chat_sender().unwrap_or(&event.from));
        let seen = seen_by_author(event);
        let mut acted = false;
        //
        // A `chat` is the exception for the escalation: a person's words in
        // a chat group are the Speaker's to read (a hand-off, a takeover),
        // and it writes the status that follows.
        if let Some(person) = &by_person
            && !is_provisional(event)
        {
            // Only what they had seen is answered; a message that carries
            // no `seen` answers everything before it.
            thread
                .directed
                .retain(|d| &d.person != person || seen.is_some_and(|s| d.seq > s));
            if event.kind != Kind::Chat
                && thread
                    .escalation
                    .as_ref()
                    .is_some_and(|e| &e.to == person && seen.is_none_or(|s| s >= e.seq))
            {
                thread.escalation = None;
                acted = true;
            }
        }
        match event.kind {
            // A provisional answer only says where the question stands: the
            // thread stays open for the agent or a person.
            Kind::Answer if is_provisional(event) => {}
            Kind::Answer => {
                thread.answered = true;
                thread.answered_by = Some(event.from.clone());
                thread.reopened = false;
                thread.answer_seq = Some(event.seq);
                thread.answer_line = Some(summarize_line(&event.body));
                // An answer from the agent does not withdraw its own
                // escalation.
                if thread.state != StatusState::Archived && thread.escalation.is_none() {
                    enter(thread, StatusState::Active);
                }
            }
            Kind::Status => {
                let Some(status) = StatusPayload::of(event) else {
                    return;
                };
                let archives = status.state == StatusState::Archived;
                let takes = status.takeover.is_some();
                let withdraws = status.withdraw
                    && thread
                        .escalation
                        .as_ref()
                        .is_some_and(|e| e.by == event.from);
                if archives || takes {
                    thread.escalation = None;
                } else if withdraws && let Some(gone) = thread.escalation.take() {
                    thread
                        .directed
                        .retain(|d| !(d.escalated && d.seq == gone.seq));
                }
                if let Some(to) = &status.escalated_to
                    && !archives
                    && !takes
                {
                    thread.escalation = Some(Escalation {
                        to: to.clone(),
                        by: event.from.clone(),
                        seq: event.seq,
                        ts: event.ts,
                        reason: status.reason.clone(),
                    });
                }
                // A progress update under an escalation keeps it: the
                // thread stays parked on the person and the reason stays.
                let under_escalation = thread.escalation.is_some()
                    && !(status.escalated_to.is_some()
                        && thread
                            .escalation
                            .as_ref()
                            .is_some_and(|e| e.seq == event.seq));
                if !under_escalation {
                    enter(thread, status.state);
                    thread.reason = status.reason.clone();
                }
                if let Some(title) = status.title {
                    thread.title = title;
                }
                if let Some(steps) = status.steps {
                    thread.steps = steps;
                }
                if let Some(links) = status.links {
                    thread.links = links;
                }
                if let Some(phase) = status.phase {
                    thread.phase = Some(phase);
                }
                if !status.artifacts.is_empty() {
                    thread.artifacts = status.artifacts;
                }
                if let Some(next_owner) = status.next_owner {
                    thread.next_owner = Some(next_owner);
                }
                if archives {
                    thread.awaiting_acceptance = false;
                } else if status.awaiting_acceptance {
                    thread.awaiting_acceptance = true;
                } else if status.state == StatusState::Processing && !under_escalation {
                    thread.awaiting_acceptance = false;
                }
                // A session is an agent's; the asker archiving its own
                // thread (it is not addressed to it) names no owner.
                let asker_only = !thread.to.contains(&event.from)
                    && (thread.from == event.from
                        || thread.on_behalf_of.as_ref() == Some(&event.from));
                if thread.owner.is_none()
                    && !asker_only
                    && let Some(session) = status.session
                    && let Some(agent) = self.people.as_agent(&event.from)
                {
                    thread.owner = Some(TaskOwner {
                        agent: agent.clone(),
                        session,
                    });
                }
                if status.escalated_to.is_some() {
                    thread.escalated_to = status.escalated_to.clone();
                }
                if status.takeover.is_some() {
                    thread.takeover = status.takeover;
                }
                if let Some(to) = status.escalated_to.filter(|_| !archives && !takes) {
                    note_directed(
                        thread,
                        event,
                        &to,
                        true,
                        status.reason.as_deref().unwrap_or(""),
                    );
                }
                return;
            }
            // Words after an answer, from anyone but its author and the
            // Speaker's own bookkeeping, ask for more.
            Kind::Question | Kind::Request | Kind::Note | Kind::Chat
                if thread.answered
                    && event.from != Address::Speaker
                    && thread.answered_by.as_ref() != Some(&event.from) =>
            {
                thread.reopened = true;
            }
            _ => {}
        }
        // The person's words leave an escalation's park: the agent's turn.
        if acted
            && event.kind != Kind::Answer
            && thread.state == StatusState::Suspended
            && thread.takeover.is_none()
        {
            enter(thread, StatusState::Processing);
        }
        if matches!(
            event.kind,
            Kind::Answer
                | Kind::Question
                | Kind::Request
                | Kind::Note
                | Kind::Chat
                | Kind::Decision
        ) && !is_provisional(event)
        {
            for to in event.to.iter().filter_map(person_of) {
                if by_person.as_ref() != Some(&to) {
                    note_directed(thread, event, &to, false, &event.body);
                }
            }
        }
    }
}

/// Something an event says to a person: what `senate inbox --follow` prints and the
/// desktop notification announces. The same rule as
/// [`ThreadSummary::directed`] uses, for one event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attention {
    /// An escalation to the person (a `status`): `text` is its reason.
    pub escalated: bool,
    /// The event's body, or an escalation's reason.
    pub text: String,
}

/// Whether `event` is addressed to `person` in a way that is news for them:
/// not written by them, not a provisional answer, and either an escalation
/// to them or a message naming them in `to`, including a notice on a
/// committee seat change. What is put to the Speaker is an agent's docket.
pub fn attention(event: &Event, person: &Name) -> Option<Attention> {
    if person_of(&event.from).as_ref() == Some(person) || is_provisional(event) {
        return None;
    }
    if event.kind == Kind::Status {
        let status = StatusPayload::of(event)?;
        return (status.escalated_to.as_ref() == Some(person)
            && status.takeover.is_none()
            && status.state != StatusState::Archived)
            .then(|| Attention {
                escalated: true,
                text: status.reason.unwrap_or_default(),
            });
    }
    let kinds = matches!(
        event.kind,
        Kind::Answer | Kind::Question | Kind::Request | Kind::Note | Kind::Chat | Kind::Decision
    ) || (event.kind == Kind::Presence
        && !event.body.is_empty()
        && crate::CommitteeSeatChange::of(event).is_some());
    (kinds
        && event
            .to
            .iter()
            .any(|a| person_of(a).as_ref() == Some(person)))
    .then(|| Attention {
        escalated: false,
        text: event.body.clone(),
    })
}

/// The handle an address names, when it names a senator: compared with a
/// person's handle, it says whether the address is that person.
fn person_of(address: &Address) -> Option<Name> {
    address.as_senator().map(|p| p.name().clone())
}

/// What the author of `event` had seen of its thread when they wrote it
/// ([`meta::SEEN`]), or `None` when it carries no such number. Clamped to
/// the event's own seq: nobody has seen beyond their own words.
pub fn seen_by_author(event: &Event) -> Option<Seq> {
    let seen = event.meta.get(meta::SEEN)?.as_u64()?;
    Some(Seq(seen.min(event.seq.0)))
}

/// Record `event` as the latest one addressed to `person`.
fn summarize_line(text: &str) -> String {
    let first = text.lines().map(str::trim).find(|l| !l.is_empty());
    let mut line: String = first
        .unwrap_or_default()
        .chars()
        .take(DIRECTED_LINE_CHARS)
        .collect();
    if first.is_some_and(|l| l.chars().count() > DIRECTED_LINE_CHARS) {
        line.push('…');
    }
    line
}

fn note_directed(
    thread: &mut ThreadSummary,
    event: &Event,
    person: &Name,
    escalated: bool,
    text: &str,
) {
    let line = summarize_line(text);
    thread
        .directed
        .retain(|d| !(&d.person == person && d.escalated == escalated));
    thread.directed.push(Directed {
        person: person.clone(),
        seq: event.seq,
        ts: event.ts,
        from: event.from.clone(),
        on_behalf_of: event.on_behalf_of.clone(),
        kind: event.kind.clone(),
        escalated,
        line,
    });
}

fn untitled(body: &str) -> String {
    let line = body.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(UNTITLED_CHARS).collect();
    if line.chars().count() > UNTITLED_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NewEvent;
    use crate::ids::CommitteeId;

    fn c() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn path(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    /// The people of the Senate these logs are in: every other senator
    /// named in them is an agent.
    fn people() -> People {
        People::new(["alice", "bob", "carol", "wendell", "pm"].map(name))
    }

    struct Log(Vec<Event>);

    impl Log {
        fn push(&mut self, e: NewEvent) -> Seq {
            let seq = Seq(self.0.len() as u64 + 1);
            self.0.push(e.into_event(seq, Utc::now()));
            seq
        }
        fn board(&self) -> TaskBoard {
            let mut b = TaskBoard::new(people());
            for e in &self.0 {
                b.observe(e);
            }
            b
        }
    }

    fn status(root: Seq, from: Address, s: StatusPayload) -> NewEvent {
        NewEvent::new(c(), Kind::Status, from, "")
            .in_thread(root)
            .with_meta(meta::STATUS, s.value())
    }

    fn json_work() -> serde_json::Value {
        serde_json::json!({
            "run_id": 1, "requester": "bob", "target_agent": "claude",
            "instruction": "x", "branch": "senate/refund-v2/speaker-1", "state": "queued"
        })
    }

    /// The asker archiving its own thread folds as the addressee's archive
    /// does: escalation ended, takeover kept, out of every inbox; it names
    /// no owner; and a later message behaves the same either way.
    #[test]
    fn an_askers_archive_folds_like_the_addressees() {
        let fold = |archiver: &str| {
            let mut log = Log(Vec::new());
            let root = log.push(
                NewEvent::new(c(), Kind::Request, addr("senator::lead"), "merge feat/x")
                    .to(addr("senator::claude")),
            );
            let mut working = StatusPayload::new(StatusState::Processing);
            working.session = Some("s-claude".into());
            log.push(status(root, addr("senator::claude"), working));
            let mut escalated = StatusPayload::new(StatusState::Suspended);
            escalated.escalated_to = Some(name("alice"));
            log.push(status(root, addr("senator::claude"), escalated));
            let mut took = StatusPayload::new(StatusState::Suspended);
            took.takeover = Some(name("bob"));
            log.push(status(root, addr("senator::bob"), took));
            let mut archived = StatusPayload::new(StatusState::Archived);
            archived.session = Some(format!("s-{archiver}"));
            log.push(status(
                root,
                addr(&format!("senator::{archiver}")),
                archived,
            ));
            let after = log.board().get(root).cloned().unwrap();
            log.push(
                NewEvent::new(c(), Kind::Note, addr("senator::claude"), "one more thing")
                    .in_thread(root),
            );
            (after, log.board().get(root).cloned().unwrap())
        };
        let (by_asker, later_asker) = fold("lead");
        let (by_agent, later_agent) = fold("claude");
        assert_eq!(by_asker.state, StatusState::Archived);
        assert_eq!(by_asker.waits(), None, "in no inbox");
        assert!(by_asker.escalation.is_none(), "the escalation ended");
        assert_eq!(by_asker.takeover, Some(name("bob")), "the takeover stays");
        assert_eq!(
            by_asker.owner.as_ref().map(|o| o.agent.as_str()),
            Some("claude"),
            "the agent that worked it owns it; the asker took nothing on"
        );
        assert_eq!(
            (by_asker.state, by_asker.waits(), &by_asker.takeover),
            (by_agent.state, by_agent.waits(), &by_agent.takeover)
        );
        assert_eq!(
            (later_asker.state, later_asker.waits()),
            (later_agent.state, later_agent.waits()),
            "a later message in the thread behaves as it does today"
        );

        // Archived before any agent reported: still no owner from the asker.
        let mut log = Log(Vec::new());
        let root = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::lead"), "which header?")
                .to(addr("senator::claude")),
        );
        let mut archived = StatusPayload::new(StatusState::Archived);
        archived.session = Some("s-lead".into());
        log.push(status(root, addr("senator::lead"), archived));
        assert!(log.board().get(root).unwrap().owner.is_none());
    }

    #[test]
    fn a_speakers_work_note_is_a_task_and_a_senators_is_not() {
        let mut log = Log(Vec::new());
        let work = json_work();
        let root = log.push(
            NewEvent::new(c(), Kind::Note, Address::Speaker, "把重试做完")
                .on_behalf_of(addr("senator::bob"))
                .with_meta(meta::WORK, work.clone()),
        );
        let forged = log.push(
            NewEvent::new(c(), Kind::Note, addr("senator::forger"), "x")
                .with_meta(meta::WORK, work),
        );
        let plain = log.push(NewEvent::new(c(), Kind::Note, Address::Speaker, "hi"));
        let board = log.board();
        let t = board.get(root).expect("a task");
        assert_eq!(t.kind, Kind::Note);
        assert_eq!(t.state, StatusState::Processing);
        assert!(board.get(forged).is_none());
        assert!(board.get(plain).is_none());
    }

    #[test]
    fn a_thread_moves_through_its_states() {
        let mut log = Log(Vec::new());
        let root = log.push(
            NewEvent::new(
                c(),
                Kind::Question,
                addr("senator::copilot"),
                "网关幂等用哪个 header？",
            )
            .to(addr("senator::claude")),
        );
        assert_eq!(
            log.board().get(root).unwrap().state,
            StatusState::Processing
        );

        let mut s = StatusPayload::new(StatusState::Processing);
        s.title = Some("查幂等 header".into());
        s.session = Some("s-01".into());
        s.steps = Some(vec![Step {
            id: "read".into(),
            label: "已读日志".into(),
            done: true,
        }]);
        log.push(status(root, addr("senator::claude"), s));
        let t = log.board().get(root).unwrap().clone();
        assert_eq!(t.title, "查幂等 header");
        assert_eq!(
            t.owner,
            Some(TaskOwner {
                agent: path("claude"),
                session: "s-01".into()
            })
        );
        assert_eq!(t.steps.len(), 1);

        let answer = log.push(
            NewEvent::new(
                c(),
                Kind::Answer,
                addr("senator::claude"),
                "Idempotency-Key",
            )
            .in_thread(root),
        );
        let t = log.board().get(root).unwrap().clone();
        assert_eq!(
            (t.state, t.since, t.answered),
            (StatusState::Active, answer, true)
        );

        log.push(status(
            root,
            addr("senator::claude"),
            StatusPayload::new(StatusState::Archived),
        ));
        let board = log.board();
        assert_eq!(board.get(root).unwrap().state, StatusState::Archived);
        // Steps survive a status that leaves them unchanged.
        assert_eq!(board.get(root).unwrap().steps.len(), 1);
    }

    /// A question from bob to claude; returns its root.
    fn ask(log: &mut Log) -> Seq {
        log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::copilot"), "?")
                .to(addr("senator::claude")),
        )
    }

    fn say(log: &mut Log, kind: Kind, from: &str, root: Seq) -> Seq {
        log.push(NewEvent::new(c(), kind, addr(from), "x").in_thread(root))
    }

    fn waits(log: &Log, root: Seq) -> Option<WaitsOn> {
        log.board().get(root).unwrap().waits()
    }

    #[test]
    fn a_thread_without_an_answer_waits_on_its_agents() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        assert_eq!(waits(&log, root), Some(WaitsOn::Agents));
        assert!(!log.board().get(root).unwrap().is_settled());
        // A provisional answer does not settle it.
        log.push(
            NewEvent::new(c(), Kind::Answer, Address::Speaker, "")
                .on_behalf_of(addr("senator::claude"))
                .in_thread(root)
                .with_meta(meta::PROVISIONAL, true.into()),
        );
        assert_eq!(waits(&log, root), Some(WaitsOn::Agents));
    }

    #[test]
    fn a_thread_whose_last_word_is_an_answer_is_settled() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        say(&mut log, Kind::Answer, "senator::claude", root);
        // The answerer's own later words, status updates that keep it
        // answered, and the Speaker's bookkeeping do not reopen it.
        say(&mut log, Kind::Note, "senator::claude", root);
        say(&mut log, Kind::Note, "speaker", root);
        let t = log.board().get(root).unwrap().clone();
        assert_eq!(t.waits(), None);
        assert!(t.is_settled());
        assert_eq!(t.answered_by, Some(addr("senator::claude")));
    }

    #[test]
    fn a_follow_up_after_the_answer_reopens_it_until_the_next_answer() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        say(&mut log, Kind::Answer, "senator::claude", root);
        for kind in [Kind::Question, Kind::Request, Kind::Note, Kind::Chat] {
            let mut l = Log(log.0.clone());
            say(&mut l, kind, "senator::copilot", root);
            assert_eq!(waits(&l, root), Some(WaitsOn::Agents));
        }
        say(&mut log, Kind::Note, "senator::bob", root);
        assert_eq!(waits(&log, root), Some(WaitsOn::Agents));
        // A person's correction, then the agent answers again.
        say(&mut log, Kind::Answer, "senator::claude", root);
        assert_eq!(waits(&log, root), None);
        // The person answers after the agent: they are the answerer now, so
        // the agent's words reopen it.
        say(&mut log, Kind::Answer, "senator::alice", root);
        assert_eq!(waits(&log, root), None);
        say(&mut log, Kind::Note, "senator::claude", root);
        assert_eq!(waits(&log, root), Some(WaitsOn::Agents));
    }

    #[test]
    fn a_thread_parked_on_a_person_waits_on_them_even_when_answered() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        say(&mut log, Kind::Answer, "senator::claude", root);
        let mut s = StatusPayload::new(StatusState::Suspended);
        s.escalated_to = Some(name("alice"));
        log.push(status(root, addr("senator::claude"), s));
        assert_eq!(waits(&log, root), Some(WaitsOn::Person));
        // Their answer settles it.
        say(&mut log, Kind::Answer, "senator::alice", root);
        assert_eq!(waits(&log, root), None);
    }

    #[test]
    fn an_explicit_in_progress_status_after_an_answer_waits() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        say(&mut log, Kind::Answer, "senator::claude", root);
        log.push(status(
            root,
            addr("senator::claude"),
            StatusPayload::new(StatusState::Processing),
        ));
        assert_eq!(waits(&log, root), Some(WaitsOn::Agents));
    }

    #[test]
    fn an_archived_thread_neither_waits_nor_is_settled() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        log.push(status(
            root,
            addr("senator::alice"),
            StatusPayload::new(StatusState::Archived),
        ));
        let t = log.board().get(root).unwrap().clone();
        assert_eq!(t.waits(), None);
        assert!(!t.is_settled());
    }

    #[test]
    fn a_provisional_answer_leaves_the_thread_open() {
        let mut log = Log(Vec::new());
        let root = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::copilot"), "?")
                .to(addr("senator::claude")),
        );
        log.push(
            NewEvent::new(c(), Kind::Answer, Address::Speaker, "")
                .on_behalf_of(addr("senator::claude"))
                .in_thread(root)
                .with_meta(meta::PROVISIONAL, true.into()),
        );
        let t = log.board().get(root).unwrap().clone();
        assert_eq!((t.state, t.answered), (StatusState::Processing, false));
    }

    #[test]
    fn a_task_belongs_to_the_agent_it_names_not_to_that_persons_other_agents() {
        let mut log = Log(Vec::new());
        let for_claude = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::copilot"), "a")
                .to(addr("senator::claude")),
        );
        let for_codex = log.push(
            NewEvent::new(c(), Kind::Request, addr("senator::copilot"), "b")
                .to(addr("senator::codex")),
        );
        let for_both = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::bob"), "c")
                .to(addr("senator::claude"))
                .to(addr("senator::codex"))
                .to(addr("senator::alice")),
        );
        // To the person alone: no agent, no task.
        let for_alice = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::bob"), "d")
                .to(addr("senator::alice")),
        );
        let board = log.board();
        assert!(board.get(for_alice).is_none());

        let roots = |agent: &str| -> Vec<Seq> {
            board
                .threads_for(&path(agent), None)
                .iter()
                .map(|t| t.root)
                .collect()
        };
        assert_eq!(roots("claude"), [for_claude, for_both]);
        assert_eq!(roots("codex"), [for_codex, for_both]);
        assert!(roots("alice").is_empty());
        assert!(roots("gemini").is_empty());
        assert_eq!(
            board
                .get(for_both)
                .unwrap()
                .agents()
                .cloned()
                .collect::<Vec<_>>(),
            [path("claude"), path("codex")]
        );
    }

    #[test]
    fn the_owning_session_is_recorded_with_the_agent_it_belongs_to() {
        let mut log = Log(Vec::new());
        let root = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::bob"), "?")
                .to(addr("senator::claude"))
                .to(addr("senator::codex")),
        );
        let claim = |session: &str| {
            let mut s = StatusPayload::new(StatusState::Processing);
            s.session = Some(session.into());
            s
        };
        // A session named by someone who is not an agent owns nothing.
        log.push(status(root, Address::Speaker, claim("s-00")));
        log.push(status(root, addr("senator::alice"), claim("s-00")));
        assert_eq!(log.board().get(root).unwrap().owner, None);
        // Two of alice's agents use the same session id; the first claim
        // stands, and says whose session it is.
        log.push(status(root, addr("senator::codex"), claim("s-01")));
        log.push(status(root, addr("senator::claude"), claim("s-01")));
        assert_eq!(
            log.board().get(root).unwrap().owner,
            Some(TaskOwner {
                agent: path("codex"),
                session: "s-01".into()
            })
        );
    }

    #[test]
    fn suspended_threads_are_listed_oldest_first() {
        let mut log = Log(Vec::new());
        let a = log.push(
            NewEvent::new(c(), Kind::Request, addr("senator::copilot"), "改 header")
                .to(addr("senator::claude")),
        );
        let b = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::pm"), "能延期吗")
                .to(addr("senator::claude")),
        );
        let mut esc = StatusPayload::new(StatusState::Suspended);
        esc.escalated_to = Some(name("alice"));
        log.push(status(b, addr("senator::claude"), esc));
        log.push(status(
            a,
            Address::Speaker,
            StatusPayload::new(StatusState::Suspended),
        ));
        let board = log.board();
        let waiting: Vec<Seq> = board
            .threads(Some(StatusState::Suspended))
            .iter()
            .map(|t| t.root)
            .collect();
        assert_eq!(waiting, vec![b, a]);
        assert_eq!(board.get(b).unwrap().escalated_to, Some(name("alice")));
    }

    #[test]
    fn triage_gating() {
        let mut log = Log(Vec::new());
        let question = || NewEvent::new(c(), Kind::Question, addr("senator::copilot"), "?");
        let q = log.push(question().to(addr("senator::claude")));
        assert!(needs_triage(&log.0[0], &people()));
        // Not a root, not to an agent, from the Speaker, or already judged.
        log.push(question().to(addr("senator::claude")).in_thread(q));
        log.push(question().to(addr("senator::alice")));
        log.push(
            question()
                .to(addr("guest::chat/u_1"))
                .to(Address::Committee),
        );
        log.push(
            NewEvent::new(c(), Kind::Question, Address::Speaker, "?").to(addr("senator::claude")),
        );
        let verdict = TriagePayload {
            verdict: TriageVerdict::Agent,
            p_needs_human: 0.1,
            p_agent: 0.8,
            p_unsure: 0.1,
            rule: None,
            reason: None,
        };
        log.push(
            question()
                .to(addr("senator::claude"))
                .with_meta(meta::TRIAGE, verdict.value()),
        );
        assert!(log.0[1..].iter().all(|e| !needs_triage(e, &people())));

        log.push(
            NewEvent::new(c(), Kind::Note, Address::Speaker, "")
                .in_thread(q)
                .with_meta(meta::TRIAGE, verdict.value()),
        );
        assert_eq!(
            log.board().get(q).unwrap().triage,
            Some(TriageVerdict::Agent)
        );
    }

    #[test]
    fn output_level_from_the_speaker_only() {
        let mut log = Log(Vec::new());
        log.push(
            NewEvent::new(c(), Kind::Note, addr("senator::copilot"), "")
                .with_meta(meta::OUTPUT_LEVEL, Value::from("all")),
        );
        assert_eq!(log.board().output_level(), OutputLevel::Status);
        log.push(
            NewEvent::new(c(), Kind::Note, Address::Speaker, "")
                .with_meta(meta::OUTPUT_LEVEL, Value::from("answers")),
        );
        assert_eq!(log.board().output_level(), OutputLevel::Answers);
    }

    #[test]
    fn check_limits() {
        let mut s = StatusPayload::new(StatusState::Suspended);
        assert!(s.check().is_ok());
        s.title = Some("x".repeat(MAX_TITLE_CHARS + 1));
        assert!(s.check().is_err());
        s.title = None;
        s.steps = Some(
            (0..=MAX_STEPS)
                .map(|i| Step {
                    id: i.to_string(),
                    label: String::new(),
                    done: false,
                })
                .collect(),
        );
        assert!(s.check().is_err());
    }

    #[test]
    fn a_thread_waits_on_and_is_taken_over_by_a_person_and_nothing_else() {
        let mut s = StatusPayload::new(StatusState::Suspended);
        s.escalated_to = Some(name("alice"));
        s.takeover = Some(name("pm"));
        let value = s.value();
        assert_eq!(value["escalated_to"], "alice");
        assert_eq!(value["takeover"], "pm");
        for (field, bad) in [
            ("escalated_to", "alice/claude"),
            ("escalated_to", "senator::alice"),
            ("takeover", "guest::chat/u_1"),
            ("takeover", "speaker::x"),
        ] {
            let mut v = value.clone();
            v[field] = bad.into();
            assert!(
                serde_json::from_value::<StatusPayload>(v).is_err(),
                "{field}={bad}"
            );
        }
    }

    // ---- back to the person ------------------------------------------------

    /// The first real run: wendell asks his own agent for something.
    fn request_to_own_agent(log: &mut Log) -> Seq {
        log.push(
            NewEvent::new(c(), Kind::Request, addr("senator::wendell"), "do the thing")
                .to(addr("senator::cursor")),
        )
    }

    fn from_agent(log: &mut Log, root: Seq, s: StatusPayload) -> Seq {
        log.push(status(root, addr("senator::cursor"), s))
    }

    fn escalate(log: &mut Log, root: Seq, reason: &str) -> Seq {
        let mut s = StatusPayload::new(StatusState::Suspended);
        s.escalated_to = Some(name("wendell"));
        s.reason = Some(reason.into());
        from_agent(log, root, s)
    }

    /// processing, answer to the person, suspended + escalated, a
    /// checkpoint, then the agent ticks off its steps with `active`.
    fn the_real_sequence() -> (Log, Seq) {
        let mut log = Log(Vec::new());
        let root = request_to_own_agent(&mut log);
        let mut s = StatusPayload::new(StatusState::Processing);
        s.title = Some("Plan".into());
        s.steps = Some(vec![Step {
            id: "a".into(),
            label: "read".into(),
            done: false,
        }]);
        from_agent(&mut log, root, s);
        log.push(
            NewEvent::new(c(), Kind::Answer, addr("senator::cursor"), "The result")
                .to(addr("senator::wendell"))
                .in_thread(root),
        );
        escalate(&mut log, root, "nine decisions\nneeded");
        log.push(
            NewEvent::new(c(), Kind::Checkpoint, addr("senator::cursor"), "cp").in_thread(root),
        );
        let mut s = StatusPayload::new(StatusState::Active);
        s.steps = Some(vec![Step {
            id: "a".into(),
            label: "read".into(),
            done: true,
        }]);
        from_agent(&mut log, root, s);
        (log, root)
    }

    #[test]
    fn an_escalation_survives_the_agent_ticking_off_its_steps() {
        let (log, root) = the_real_sequence();
        let t = log.board().get(root).unwrap().clone();
        assert_eq!(t.state, StatusState::Suspended);
        assert_eq!(t.waits(), Some(WaitsOn::Person));
        assert!(!t.is_settled());
        // The progress update still landed.
        assert_eq!(t.title, "Plan");
        assert!(t.steps[0].done);
        let w = t.waits_on_person(&name("wendell"), Seq(0)).unwrap();
        let e = w.escalation.unwrap();
        assert_eq!(e.reason.as_deref(), Some("nine decisions\nneeded"));
        assert_eq!(e.by, addr("senator::cursor"));
        assert_eq!(t.reason.as_deref(), Some("nine decisions\nneeded"));
        // Seen everything: the escalation still waits, it is not new.
        let w = t.waits_on_person(&name("wendell"), Seq(99)).unwrap();
        assert!(w.escalation.is_some() && w.unread.is_none());
    }

    #[test]
    fn an_escalation_ends_when_the_person_acts() {
        let words = |kind: Kind| {
            let (mut log, root) = the_real_sequence();
            say(&mut log, kind, "senator::wendell", root);
            let t = log.board().get(root).unwrap().clone();
            assert!(t.escalation.is_none());
            assert!(t.waits_on_person(&name("wendell"), Seq(0)).is_none());
            t
        };
        // A reply or a correction hands the thread back to the agent.
        for kind in [Kind::Note, Kind::Question, Kind::Request] {
            assert_eq!(words(kind).waits(), Some(WaitsOn::Agents));
        }
        // Their answer settles it.
        assert_eq!(words(Kind::Answer).waits(), None);
        // Words in a chat group are the Speaker's to read: they answer what
        // was addressed to them, but only the Speaker's status that follows
        // (a hand-off, a takeover) ends the escalation.
        let (mut log, root) = the_real_sequence();
        say(&mut log, Kind::Chat, "senator::wendell", root);
        let t = log.board().get(root).unwrap().clone();
        assert!(t.escalation.is_some() && t.state == StatusState::Suspended);
        assert!(t.directed.is_empty());

        // Taking it over: nothing waits on them as a hand-off any more.
        let (mut log, root) = the_real_sequence();
        let mut take = StatusPayload::new(StatusState::Suspended);
        take.takeover = Some(name("wendell"));
        log.push(status(root, addr("senator::wendell"), take));
        let t = log.board().get(root).unwrap().clone();
        assert!(t.escalation.is_none());
        assert!(t.waits_on_person(&name("wendell"), Seq(0)).is_none());
        assert_eq!(t.takeover, Some(name("wendell")));

        // Somebody else's words do not end it.
        let (mut log, root) = the_real_sequence();
        say(&mut log, Kind::Note, "senator::bob", root);
        assert!(log.board().get(root).unwrap().escalation.is_some());
    }

    /// The person writes in the thread having seen it up to `seen`.
    fn say_seen(log: &mut Log, kind: Kind, root: Seq, seen: Option<u64>) -> Seq {
        let mut e = NewEvent::new(c(), kind, addr("senator::wendell"), "x").in_thread(root);
        if let Some(seen) = seen {
            e = e.with_meta(meta::SEEN, serde_json::json!(seen));
        }
        log.push(e)
    }

    /// The seq of the escalation in `the_real_sequence`.
    fn escalation_seq(log: &Log, root: Seq) -> Seq {
        log.board()
            .get(root)
            .unwrap()
            .escalation
            .as_ref()
            .unwrap()
            .seq
    }

    #[test]
    fn a_reply_that_crossed_the_escalation_does_not_end_it() {
        let (mut log, root) = the_real_sequence();
        let esc = escalation_seq(&log, root);
        // Written before the escalation was shown: seen is one short.
        say_seen(&mut log, Kind::Note, root, Some(esc.0 - 1));
        let t = log.board().get(root).unwrap().clone();
        assert_eq!(t.escalation.as_ref().map(|e| e.seq), Some(esc));
        assert_eq!(t.state, StatusState::Suspended);
        assert_eq!(t.waits(), Some(WaitsOn::Person));
        // Still new for the person, with its reason.
        let w = t.waits_on_person(&name("wendell"), Seq(esc.0 - 1)).unwrap();
        assert!(w.escalation.is_some());
        let unread = w.unread.unwrap();
        assert!(unread.escalated && unread.seq == esc);
        assert_eq!(unread.line, "nine decisions");
        // A message that never saw anything does not end it either.
        say_seen(&mut log, Kind::Question, root, Some(0));
        assert!(log.board().get(root).unwrap().escalation.is_some());
    }

    #[test]
    fn a_reply_after_seeing_the_escalation_ends_it() {
        for kind in [Kind::Note, Kind::Question, Kind::Request, Kind::Answer] {
            let (mut log, root) = the_real_sequence();
            let esc = escalation_seq(&log, root);
            say_seen(&mut log, kind, root, Some(esc.0));
            let t = log.board().get(root).unwrap().clone();
            assert!(t.escalation.is_none());
            assert!(t.waits_on_person(&name("wendell"), Seq(0)).is_none());
        }
        // A `seen` beyond the thread is clamped to the event's own seq: it
        // still covers the escalation.
        let (mut log, root) = the_real_sequence();
        say_seen(&mut log, Kind::Note, root, Some(9_999));
        assert!(log.board().get(root).unwrap().escalation.is_none());
        assert_eq!(
            seen_by_author(log.0.last().unwrap()),
            Some(log.0.last().unwrap().seq)
        );
        // Not a number: no `seen` at all.
        let (mut log, root) = the_real_sequence();
        log.push(
            NewEvent::new(c(), Kind::Note, addr("senator::wendell"), "x")
                .in_thread(root)
                .with_meta(meta::SEEN, serde_json::json!("lots")),
        );
        assert!(log.board().get(root).unwrap().escalation.is_none());
    }

    #[test]
    fn a_reply_without_seen_ends_the_escalation_as_before() {
        let (mut log, root) = the_real_sequence();
        say_seen(&mut log, Kind::Note, root, None);
        assert!(log.board().get(root).unwrap().escalation.is_none());
    }

    #[test]
    fn taking_over_ends_an_escalation_whatever_was_seen() {
        let (mut log, root) = the_real_sequence();
        say_seen(&mut log, Kind::Note, root, Some(0));
        assert!(log.board().get(root).unwrap().escalation.is_some());
        let mut take = StatusPayload::new(StatusState::Suspended);
        take.takeover = Some(name("wendell"));
        log.push(
            status(root, addr("senator::wendell"), take)
                .with_meta(meta::SEEN, serde_json::json!(0)),
        );
        let t = log.board().get(root).unwrap().clone();
        assert!(t.escalation.is_none());
        assert_eq!(t.takeover, Some(name("wendell")));
    }

    #[test]
    fn a_crossed_reply_keeps_what_it_did_not_see_unread() {
        let (mut log, root) = the_real_sequence();
        let esc = escalation_seq(&log, root);
        say_seen(&mut log, Kind::Note, root, Some(esc.0 - 1));
        // The answer before the escalation was seen; the escalation was not.
        let t = log.board().get(root).unwrap().clone();
        assert!(t.directed.iter().all(|d| d.seq > Seq(esc.0 - 1)));
        // An agent's withdrawal still ends it, unchanged.
        let mut back = StatusPayload::new(StatusState::Processing);
        back.withdraw = true;
        from_agent(&mut log, root, back);
        let t = log.board().get(root).unwrap().clone();
        assert!(t.escalation.is_none());
        assert_eq!(t.waits(), Some(WaitsOn::Agents));
    }

    #[test]
    fn only_its_author_withdraws_an_escalation_and_a_progress_update_does_not() {
        let (mut log, root) = the_real_sequence();
        let mut back = StatusPayload::new(StatusState::Processing);
        back.withdraw = true;
        // The Speaker did not make it.
        log.push(status(root, Address::Speaker, back.clone()));
        assert!(log.board().get(root).unwrap().escalation.is_some());
        // A plain update from its author does not withdraw it.
        from_agent(&mut log, root, StatusPayload::new(StatusState::Processing));
        assert!(log.board().get(root).unwrap().escalation.is_some());
        // Its author's explicit withdrawal does.
        from_agent(&mut log, root, back);
        let t = log.board().get(root).unwrap().clone();
        assert!(t.escalation.is_none());
        assert_eq!(t.state, StatusState::Processing);
        assert_eq!(t.waits(), Some(WaitsOn::Agents));
        // The withdrawn escalation is no longer news for the person; the
        // answer to them still is.
        let w = t.waits_on_person(&name("wendell"), Seq(0)).unwrap();
        assert!(w.escalation.is_none());
        assert_eq!(w.unread.unwrap().kind, Kind::Answer);
        // Archiving ends it too.
        let (mut log, root) = the_real_sequence();
        log.push(status(
            root,
            addr("senator::wendell"),
            StatusPayload::new(StatusState::Archived),
        ));
        let t = log.board().get(root).unwrap().clone();
        assert!(t.escalation.is_none());
        assert!(t.waits_on_person(&name("wendell"), Seq(0)).is_none());
    }

    #[test]
    fn a_withdrawal_must_name_where_the_thread_goes() {
        let mut s = StatusPayload::new(StatusState::Suspended);
        s.withdraw = true;
        assert!(s.check().is_err());
        let mut s = StatusPayload::new(StatusState::Active);
        s.withdraw = true;
        assert!(s.check().is_ok());
        s.escalated_to = Some(name("wendell"));
        assert!(s.check().is_err());
    }

    #[test]
    fn what_the_person_has_not_seen_waits_on_them_though_it_is_settled() {
        let wendell = name("wendell");
        for kind in [Kind::Answer, Kind::Note] {
            let mut log = Log(Vec::new());
            let root = request_to_own_agent(&mut log);
            let e = log.push(
                NewEvent::new(c(), kind.clone(), addr("senator::cursor"), "Done\nmore")
                    .to(addr("senator::wendell"))
                    .in_thread(root),
            );
            if kind == Kind::Note {
                // A note is not an answer; make it settled for agents.
                say(&mut log, Kind::Answer, "senator::cursor", root);
            }
            let t = log.board().get(root).unwrap().clone();
            // The agents' rule: nobody waits (or, for the note, the answer
            // settles it).
            assert_eq!(t.is_settled(), kind == Kind::Answer || t.answered);
            let w = t.waits_on_person(&wendell, Seq(0)).expect("unread");
            let d = w.unread.unwrap();
            assert_eq!((d.seq, &d.kind, d.line.as_str()), (e, &kind, "Done"));
            assert!(w.escalation.is_none());
            // Seen up to it, or past it: nothing waits.
            assert!(t.waits_on_person(&wendell, e).is_none());
            assert!(t.waits_on_person(&wendell, Seq(e.0 + 5)).is_none());
            // Somebody else's mark is not theirs.
            assert!(t.waits_on_person(&name("bob"), Seq(0)).is_none());
        }
    }

    #[test]
    fn a_new_event_after_seen_waits_again_and_their_own_words_clear_it() {
        let wendell = name("wendell");
        let mut log = Log(Vec::new());
        let root = request_to_own_agent(&mut log);
        let first = log.push(
            NewEvent::new(c(), Kind::Answer, addr("senator::cursor"), "one")
                .to(addr("senator::wendell"))
                .in_thread(root),
        );
        let second = log.push(
            NewEvent::new(c(), Kind::Note, addr("senator::cursor"), "two")
                .to(addr("senator::wendell"))
                .in_thread(root),
        );
        let t = log.board().get(root).unwrap().clone();
        assert!(t.waits_on_person(&wendell, first).is_some());
        assert!(t.waits_on_person(&wendell, second).is_none());
        let third = log.push(
            NewEvent::new(c(), Kind::Note, addr("senator::cursor"), "three")
                .to(addr("senator::wendell"))
                .in_thread(root),
        );
        let t = log.board().get(root).unwrap().clone();
        let w = t.waits_on_person(&wendell, second).unwrap();
        assert_eq!(w.unread.unwrap().seq, third);
        // They speak in the thread (from anywhere): all of it is answered.
        say(&mut log, Kind::Note, "senator::wendell", root);
        let t = log.board().get(root).unwrap().clone();
        assert!(t.waits_on_person(&wendell, Seq(0)).is_none());
    }

    #[test]
    fn an_escalation_is_unread_news_until_seen_and_a_provisional_answer_is_not() {
        let wendell = name("wendell");
        let mut log = Log(Vec::new());
        let root = request_to_own_agent(&mut log);
        log.push(
            NewEvent::new(c(), Kind::Answer, Address::Speaker, "where it stands")
                .on_behalf_of(addr("senator::cursor"))
                .to(addr("senator::wendell"))
                .in_thread(root)
                .with_meta(meta::PROVISIONAL, true.into()),
        );
        assert!(
            log.board()
                .get(root)
                .unwrap()
                .waits_on_person(&wendell, Seq(0))
                .is_none()
        );
        let esc = escalate(&mut log, root, "need a call");
        let t = log.board().get(root).unwrap().clone();
        let w = t.waits_on_person(&wendell, Seq(0)).unwrap();
        let d = w.unread.unwrap();
        assert!(d.escalated && d.seq == esc && d.line == "need a call");
        // An escalation to someone else is theirs.
        assert!(t.waits_on_person(&name("bob"), Seq(0)).is_none());
    }

    #[test]
    fn what_is_news_for_a_person_is_what_the_board_records_for_them() {
        let wendell = name("wendell");
        let (log, root) = the_real_sequence();
        let news: Vec<(Seq, bool, String)> = log
            .0
            .iter()
            .filter_map(|e| attention(e, &wendell).map(|a| (e.seq, a.escalated, a.text)))
            .collect();
        // The answer (3) and the escalation (4): not the root, not statuses
        // of progress, not the checkpoint.
        assert_eq!(
            news,
            [
                (Seq(3), false, "The result".to_string()),
                (Seq(4), true, "nine decisions\nneeded".to_string())
            ]
        );
        let t = log.board().get(root).unwrap().clone();
        let recorded: Vec<Seq> = t
            .unread_for(&wendell, Seq(0))
            .iter()
            .map(|d| d.seq)
            .collect();
        assert_eq!(recorded, [Seq(3), Seq(4)]);
        // Their own words, provisional answers and other people's mail are not.
        let own = NewEvent::new(c(), Kind::Note, addr("senator::wendell"), "x")
            .to(addr("senator::wendell"))
            .into_event(Seq(9), Utc::now());
        assert!(attention(&own, &wendell).is_none());
        let to_speaker = NewEvent::new(c(), Kind::Question, addr("senator::cursor"), "x")
            .to(Address::Speaker)
            .into_event(Seq(9), Utc::now());
        assert!(attention(&to_speaker, &wendell).is_none());
    }

    #[test]
    fn sponsored_rules_signals_and_buckets_follow_w2_spec() {
        let wendell = name("wendell");
        let bob = name("bob");
        let mut board = TaskBoard::new(People::new([wendell.clone(), bob.clone()]));
        let origin = crate::Origin {
            locator: "/senate::office::senator::secretary".parse().unwrap(),
            thread: Seq(194),
        };

        // 1. Direct person root
        let direct_root = NewEvent::new(c(), Kind::Request, Address::senior(&wendell), "Do A")
            .to(addr("senator::claude"))
            .into_event(Seq(1), Utc::now());
        board.observe(&direct_root);
        assert!(board.get(Seq(1)).unwrap().is_sponsored_by(&wendell));
        assert!(!board.get(Seq(1)).unwrap().is_sponsored_by(&bob));

        // 2. DESK_PERSON root
        let desk_root = NewEvent::new(c(), Kind::Request, Address::Speaker, "Do B")
            .to(addr("senator::claude"))
            .with_meta(meta::ORIGIN, origin.value())
            .with_meta(meta::DESK_PERSON, serde_json::json!("wendell"))
            .into_event(Seq(2), Utc::now());
        board.observe(&desk_root);
        assert!(board.get(Seq(2)).unwrap().is_sponsored_by(&wendell));

        // 3. EXECUTION_AUTH authorized_by via SecretaryProxy
        let proxy_root = NewEvent::new(c(), Kind::Request, Address::Speaker, "Do C")
            .to(addr("senator::claude"))
            .with_meta(meta::ORIGIN, origin.value())
            .with_meta(
                meta::EXECUTION_AUTH,
                crate::ExecutionAuthPayload {
                    authorized_by: wendell.clone(),
                    mode: crate::ExecutionAuthMode::SecretaryProxy {
                        secretary: path("secretary"),
                    },
                    note: None,
                }
                .value(),
            )
            .into_event(Seq(3), Utc::now());
        board.observe(&proxy_root);
        assert!(board.get(Seq(3)).unwrap().is_sponsored_by(&wendell));
        assert!(!board.get(Seq(3)).unwrap().is_sponsored_by(&bob));

        // 4. Unrelated agent forward (neither from, desk_person, nor execution_auth)
        let unrelated_root = NewEvent::new(c(), Kind::Request, Address::Speaker, "Do D")
            .to(addr("senator::claude"))
            .with_meta(meta::ORIGIN, origin.value())
            .into_event(Seq(4), Utc::now());
        board.observe(&unrelated_root);
        assert!(!board.get(Seq(4)).unwrap().is_sponsored_by(&wendell));

        // Commission signals for proxy_root
        let croot = CommissionRoot::from_event(&proxy_root);
        let processing = NewEvent::new(c(), Kind::Status, addr("senator::claude"), "")
            .in_thread(Seq(3))
            .with_meta(
                meta::STATUS,
                StatusPayload::new(StatusState::Processing).value(),
            )
            .into_event(Seq(10), Utc::now());
        assert_eq!(
            commission_signal(&processing, &croot, &wendell, false),
            None
        );

        let mut sp = StatusPayload::new(StatusState::Suspended);
        sp.reason = Some("needs key".into());
        let blocked = NewEvent::new(c(), Kind::Status, addr("senator::claude"), "")
            .in_thread(Seq(3))
            .with_meta(meta::STATUS, sp.value())
            .into_event(Seq(11), Utc::now());
        let sig_block = commission_signal(&blocked, &croot, &wendell, false).unwrap();
        assert_eq!(sig_block.kind, CommissionSignalKind::Blocked);
        assert_eq!(sig_block.origin_committee.as_str(), "office");
        assert_eq!(sig_block.origin_root, Seq(194));

        let ans = NewEvent::new(c(), Kind::Answer, addr("senator::claude"), "Done!")
            .in_thread(Seq(3))
            .into_event(Seq(12), Utc::now());
        let sig_done = commission_signal(&ans, &croot, &wendell, false).unwrap();
        assert_eq!(sig_done.kind, CommissionSignalKind::Completed);
        assert_eq!(sig_done.discriminator, ans.id.to_string());
        // Already answered -> no second Completed signal
        assert_eq!(commission_signal(&ans, &croot, &wendell, true), None);

        // Relayed answer on origin matches canonical discriminator
        let origin_committee = CommitteeId::new("office").unwrap();
        let relayed = NewEvent::new(
            origin_committee.clone(),
            Kind::Answer,
            Address::Speaker,
            "Done!",
        )
        .in_thread(Seq(194))
        .with_meta(
            meta::ANSWERED_BY,
            serde_json::json!("refund-v2::senator::claude"),
        )
        .with_meta(
            meta::RELAYED_EVENT_ID,
            serde_json::json!(ans.id.to_string()),
        )
        .into_event(Seq(199), Utc::now());
        let origin_root_ev = NewEvent::new(
            origin_committee,
            Kind::Request,
            addr("senator::secretary"),
            "Do C",
        )
        .to(Address::Speaker)
        .into_event(Seq(194), Utc::now());
        let mut oroot = CommissionRoot::from_event(&origin_root_ev);
        oroot.desk_person = Some(wendell.clone());
        let sig_relay = commission_signal(&relayed, &oroot, &wendell, false).unwrap();
        assert_eq!(sig_relay.discriminator, sig_done.discriminator);
        assert_eq!(sig_relay.origin_committee, sig_done.origin_committee);
        assert_eq!(sig_relay.origin_root, sig_done.origin_root);

        // Buckets
        board.observe(&blocked);
        assert_eq!(
            board.get(Seq(3)).unwrap().commission_bucket(Seq(0)),
            CommissionBucket::Blocked
        );
        board.observe(&ans);
        assert_eq!(
            board.get(Seq(3)).unwrap().commission_bucket(Seq(11)),
            CommissionBucket::CompletedUnread
        );
        assert_eq!(
            board.get(Seq(3)).unwrap().commission_bucket(Seq(12)),
            CommissionBucket::CompletedRead
        );
        assert_eq!(
            board.get(Seq(1)).unwrap().commission_bucket(Seq(0)),
            CommissionBucket::InProgress
        );
    }

    #[test]
    fn status_payload_bounds_and_pending_acceptance_lifecycle() {
        let mut s = StatusPayload::new(StatusState::Active);
        s.phase = Some("staging-verified".into());
        s.artifacts = vec![
            ArtifactRef {
                label: "commit".into(),
                value: "035b2c43".into(),
            },
            ArtifactRef {
                label: "tag".into(),
                value: "v0.4.2".into(),
            },
        ];
        s.next_owner = Some(addr("senator::alice"));
        s.awaiting_acceptance = true;
        assert!(s.check().is_ok());

        let mut bad_phase = s.clone();
        bad_phase.phase = Some("x".repeat(MAX_PHASE_CHARS + 1));
        assert!(bad_phase.check().is_err());

        let mut multiline_phase = s.clone();
        multiline_phase.phase = Some("line1\nline2".into());
        assert!(multiline_phase.check().is_err());

        let mut too_many = s.clone();
        too_many.artifacts = vec![
            ArtifactRef {
                label: "k".into(),
                value: "v".into()
            };
            MAX_ARTIFACTS + 1
        ];
        assert!(too_many.check().is_err());

        let mut bad_art = s.clone();
        bad_art.artifacts = vec![ArtifactRef {
            label: " ".into(),
            value: "v".into(),
        }];
        assert!(bad_art.check().is_err());

        // Pending acceptance keeps answered thread unsettled in PendingAcceptance until archived.
        let bob: Name = Name::new("bob").unwrap();
        let alice: Name = Name::new("alice").unwrap();
        let people = People::new([alice.clone(), bob]);
        let mut board = TaskBoard::new(people);
        let root = NewEvent::new(
            CommitteeId::new("refund-v2").unwrap(),
            Kind::Request,
            addr("senator::bob"),
            "Deploy release",
        )
        .to(addr("senator::claude"))
        .into_event(Seq(1), Utc::now());
        board.observe(&root);
        let ans = NewEvent::new(
            CommitteeId::new("refund-v2").unwrap(),
            Kind::Answer,
            addr("senator::claude"),
            "Deployed to staging",
        )
        .in_thread(Seq(1))
        .into_event(Seq(2), Utc::now());
        board.observe(&ans);
        assert!(board.get(Seq(1)).unwrap().is_settled());

        let st_ev = NewEvent::new(
            CommitteeId::new("refund-v2").unwrap(),
            Kind::Status,
            addr("senator::claude"),
            "",
        )
        .in_thread(Seq(1))
        .with_meta(meta::STATUS, s.value())
        .into_event(Seq(3), Utc::now());
        board.observe(&st_ev);

        let sum = board.get(Seq(1)).unwrap();
        assert!(!sum.is_settled());
        assert_eq!(
            sum.waits(),
            Some(WaitsOn::Acceptance {
                owner: addr("senator::alice")
            })
        );
        assert_eq!(
            sum.commission_bucket(Seq(3)),
            CommissionBucket::PendingAcceptance
        );
        let linked = LinkedTaskSummary::from_thread(CommitteeId::new("refund-v2").unwrap(), sum);
        assert!(linked.awaiting_acceptance);
        assert_eq!(linked.phase.as_deref(), Some("staging-verified"));
        assert_eq!(linked.artifacts.len(), 2);

        // Archiving clears awaiting_acceptance and settles/closes.
        let archived = NewEvent::new(
            CommitteeId::new("refund-v2").unwrap(),
            Kind::Status,
            addr("senator::alice"),
            "",
        )
        .in_thread(Seq(1))
        .with_meta(
            meta::STATUS,
            StatusPayload::new(StatusState::Archived).value(),
        )
        .into_event(Seq(4), Utc::now());
        board.observe(&archived);
        let sum = board.get(Seq(1)).unwrap();
        assert!(!sum.awaiting_acceptance);
        assert_eq!(sum.waits(), None);

        // Legacy StatusPayload without W3 fields still deserializes cleanly.
        let legacy: StatusPayload =
            serde_json::from_value(serde_json::json!({ "state": "active" })).unwrap();
        assert!(!legacy.awaiting_acceptance);
        assert!(legacy.artifacts.is_empty());
        assert_eq!(legacy.phase, None);
    }
}
