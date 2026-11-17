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
use crate::senator::SenatorPath;

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

/// `meta["status"]` of a [`Kind::Status`] event, in the thread of a task.
///
/// Every field but `state` is optional, and `None` means "unchanged".
/// `steps` and `links`, when present, are the complete list and replace the
/// previous one.
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
    /// steps, links and labels.
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
        if self
            .title
            .as_deref()
            .is_some_and(|t| long(t, MAX_TITLE_CHARS))
        {
            return Err(format!("title is over {MAX_TITLE_CHARS} characters"));
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
/// agent, was not written by the Speaker, and carries no verdict of its own.
pub fn needs_triage(event: &Event) -> bool {
    matches!(event.kind, Kind::Question | Kind::Request)
        && event.thread.is_none()
        && event.from != Address::Speaker
        && event.to.iter().any(Address::is_junior)
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
    pub agent: SenatorPath,
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
    /// The status title, else the start of the root's body.
    pub title: String,
    pub state: StatusState,
    /// The seq at which the thread entered `state`.
    pub since: Seq,
    pub since_ts: DateTime<Utc>,
    /// The last seq in the thread.
    pub updated: Seq,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitsOn {
    /// The agents it was addressed to.
    Agents,
    /// A person: it was escalated or parked on them, or they took it over.
    Person,
}

impl ThreadSummary {
    /// The agents the task was addressed to, in the root's order.
    pub fn agents(&self) -> impl Iterator<Item = &SenatorPath> {
        self.to.iter().filter_map(Address::as_agent)
    }

    /// Whether the task was addressed to exactly this agent. Another agent
    /// of the same person does not count.
    pub fn is_for(&self, agent: &SenatorPath) -> bool {
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

    /// Whether the thread waits on someone, and on whom. This is the one
    /// rule every inbox uses (the `senate inbox` command, the `inbox` tool,
    /// the Speaker's per-agent count, the app's inbox); it is derived and
    /// stores nothing.
    ///
    /// A thread waits when it is not archived and
    ///
    /// - it is `suspended`: parked on a person (an escalation or a
    ///   takeover): [`WaitsOn::Person`];
    /// - or its root has no answer yet, or its task status is explicitly in
    ///   progress (`processing` after an answer, set by a `status`), or
    ///   something was said in it after the latest answer by someone other
    ///   than the answerer ([`ThreadSummary::reopened`]):
    ///   [`WaitsOn::Agents`].
    ///
    /// Otherwise its last word is an answer and nothing asks for more: it
    /// is [settled](ThreadSummary::is_settled).
    pub fn waits(&self) -> Option<WaitsOn> {
        match self.state {
            StatusState::Archived => None,
            _ if self.escalation.is_some() => Some(WaitsOn::Person),
            StatusState::Suspended => Some(WaitsOn::Person),
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
}

/// Characters of the root's body used as a title when the task has none.
pub const UNTITLED_CHARS: usize = 40;

/// Folds the log into the committee's task threads. Like the brief builder,
/// [`TaskBoard::observe`] ignores an event at or below the last one it saw.
#[derive(Debug, Clone, Default)]
pub struct TaskBoard {
    head: Seq,
    threads: BTreeMap<Seq, ThreadSummary>,
    output_level: OutputLevel,
}

impl TaskBoard {
    pub fn new() -> Self {
        Self::default()
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

    /// [`TaskBoard::threads`] narrowed to those addressed to `agent`: one
    /// agent's inbox, not its person's.
    pub fn threads_for(
        &self,
        agent: &SenatorPath,
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
            && event.to.iter().any(Address::is_junior))
            || crate::work::is_work_root(event);
        if !is_task {
            return;
        }
        self.threads.insert(
            event.seq,
            ThreadSummary {
                root: event.seq,
                kind: event.kind.clone(),
                from: event.from.clone(),
                on_behalf_of: event.on_behalf_of.clone(),
                to: event.to.clone(),
                title: untitled(&event.body),
                state: StatusState::Processing,
                since: event.seq,
                since_ts: event.ts,
                updated: event.seq,
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
            },
        );
    }

    fn observe_reply(&mut self, root: Seq, event: &Event) {
        let Some(thread) = self.threads.get_mut(&root) else {
            return;
        };
        thread.updated = event.seq;
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
        let by_person = person_of(&event.from);
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
                // A session is an agent's: a status from anyone else names
                // no owner.
                if thread.owner.is_none()
                    && let Some(session) = status.session
                    && let Some(agent) = event.from.as_agent()
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

/// Something an event says to a person: what `senate watch` prints and the
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
/// to them or an answer, note, question, request, decision or chat naming
/// them in `to`. What is put to the Speaker is not: that is an agent's
/// docket.
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
    );
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

/// The person an address names, when it is a person.
fn person_of(address: &Address) -> Option<Name> {
    address
        .is_senior()
        .then(|| address.as_senator().map(|p| p.owner().clone()))
        .flatten()
}

/// What the author of `event` had seen of its thread when they wrote it
/// ([`meta::SEEN`]), or `None` when it carries no such number. Clamped to
/// the event's own seq: nobody has seen beyond their own words.
pub fn seen_by_author(event: &Event) -> Option<Seq> {
    let seen = event.meta.get(meta::SEEN)?.as_u64()?;
    Some(Seq(seen.min(event.seq.0)))
}

/// Record `event` as the latest one addressed to `person`.
fn note_directed(
    thread: &mut ThreadSummary,
    event: &Event,
    person: &Name,
    escalated: bool,
    text: &str,
) {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty());
    let mut line: String = line
        .unwrap_or_default()
        .chars()
        .take(DIRECTED_LINE_CHARS)
        .collect();
    if text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .is_some_and(|l| l.chars().count() > DIRECTED_LINE_CHARS)
    {
        line.push('…');
    }
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

    fn path(s: &str) -> SenatorPath {
        s.parse().unwrap()
    }

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    struct Log(Vec<Event>);

    impl Log {
        fn push(&mut self, e: NewEvent) -> Seq {
            let seq = Seq(self.0.len() as u64 + 1);
            self.0.push(e.into_event(seq, Utc::now()));
            seq
        }
        fn board(&self) -> TaskBoard {
            let mut b = TaskBoard::new();
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
            "run_id": 1, "requester": "bob", "target_agent": "alice/claude",
            "instruction": "x", "branch": "feature/refund-v2/speaker-1", "state": "queued"
        })
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
            NewEvent::new(c(), Kind::Note, addr("senator::eve/claude"), "x")
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
                addr("senator::bob/claude"),
                "网关幂等用哪个 header？",
            )
            .to(addr("senator::alice/claude")),
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
        log.push(status(root, addr("senator::alice/claude"), s));
        let t = log.board().get(root).unwrap().clone();
        assert_eq!(t.title, "查幂等 header");
        assert_eq!(
            t.owner,
            Some(TaskOwner {
                agent: path("alice/claude"),
                session: "s-01".into()
            })
        );
        assert_eq!(t.steps.len(), 1);

        let answer = log.push(
            NewEvent::new(
                c(),
                Kind::Answer,
                addr("senator::alice/claude"),
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
            addr("senator::alice/claude"),
            StatusPayload::new(StatusState::Archived),
        ));
        let board = log.board();
        assert_eq!(board.get(root).unwrap().state, StatusState::Archived);
        // Steps survive a status that leaves them unchanged.
        assert_eq!(board.get(root).unwrap().steps.len(), 1);
    }

    /// A question from bob to alice/claude; returns its root.
    fn ask(log: &mut Log) -> Seq {
        log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::bob/claude"), "?")
                .to(addr("senator::alice/claude")),
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
                .on_behalf_of(addr("senator::alice/claude"))
                .in_thread(root)
                .with_meta(meta::PROVISIONAL, true.into()),
        );
        assert_eq!(waits(&log, root), Some(WaitsOn::Agents));
    }

    #[test]
    fn a_thread_whose_last_word_is_an_answer_is_settled() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        say(&mut log, Kind::Answer, "senator::alice/claude", root);
        // The answerer's own later words, status updates that keep it
        // answered, and the Speaker's bookkeeping do not reopen it.
        say(&mut log, Kind::Note, "senator::alice/claude", root);
        say(&mut log, Kind::Note, "speaker", root);
        let t = log.board().get(root).unwrap().clone();
        assert_eq!(t.waits(), None);
        assert!(t.is_settled());
        assert_eq!(t.answered_by, Some(addr("senator::alice/claude")));
    }

    #[test]
    fn a_follow_up_after_the_answer_reopens_it_until_the_next_answer() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        say(&mut log, Kind::Answer, "senator::alice/claude", root);
        for kind in [Kind::Question, Kind::Request, Kind::Note, Kind::Chat] {
            let mut l = Log(log.0.clone());
            say(&mut l, kind, "senator::bob/claude", root);
            assert_eq!(waits(&l, root), Some(WaitsOn::Agents));
        }
        say(&mut log, Kind::Note, "senator::bob", root);
        assert_eq!(waits(&log, root), Some(WaitsOn::Agents));
        // A person's correction, then the agent answers again.
        say(&mut log, Kind::Answer, "senator::alice/claude", root);
        assert_eq!(waits(&log, root), None);
        // The person answers after the agent: they are the answerer now, so
        // the agent's words reopen it.
        say(&mut log, Kind::Answer, "senator::alice", root);
        assert_eq!(waits(&log, root), None);
        say(&mut log, Kind::Note, "senator::alice/claude", root);
        assert_eq!(waits(&log, root), Some(WaitsOn::Agents));
    }

    #[test]
    fn a_thread_parked_on_a_person_waits_on_them_even_when_answered() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        say(&mut log, Kind::Answer, "senator::alice/claude", root);
        let mut s = StatusPayload::new(StatusState::Suspended);
        s.escalated_to = Some(name("alice"));
        log.push(status(root, addr("senator::alice/claude"), s));
        assert_eq!(waits(&log, root), Some(WaitsOn::Person));
        // Their answer settles it.
        say(&mut log, Kind::Answer, "senator::alice", root);
        assert_eq!(waits(&log, root), None);
    }

    #[test]
    fn an_explicit_in_progress_status_after_an_answer_waits() {
        let mut log = Log(Vec::new());
        let root = ask(&mut log);
        say(&mut log, Kind::Answer, "senator::alice/claude", root);
        log.push(status(
            root,
            addr("senator::alice/claude"),
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
            NewEvent::new(c(), Kind::Question, addr("senator::bob/claude"), "?")
                .to(addr("senator::alice/claude")),
        );
        log.push(
            NewEvent::new(c(), Kind::Answer, Address::Speaker, "")
                .on_behalf_of(addr("senator::alice/claude"))
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
            NewEvent::new(c(), Kind::Question, addr("senator::bob/claude"), "a")
                .to(addr("senator::alice/claude")),
        );
        let for_codex = log.push(
            NewEvent::new(c(), Kind::Request, addr("senator::bob/claude"), "b")
                .to(addr("senator::alice/codex")),
        );
        let for_both = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::bob"), "c")
                .to(addr("senator::alice/claude"))
                .to(addr("senator::alice/codex"))
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
        assert_eq!(roots("alice/claude"), [for_claude, for_both]);
        assert_eq!(roots("alice/codex"), [for_codex, for_both]);
        assert!(roots("alice").is_empty());
        assert!(roots("alice/gemini").is_empty());
        assert_eq!(
            board
                .get(for_both)
                .unwrap()
                .agents()
                .cloned()
                .collect::<Vec<_>>(),
            [path("alice/claude"), path("alice/codex")]
        );
    }

    #[test]
    fn the_owning_session_is_recorded_with_the_agent_it_belongs_to() {
        let mut log = Log(Vec::new());
        let root = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::bob"), "?")
                .to(addr("senator::alice/claude"))
                .to(addr("senator::alice/codex")),
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
        log.push(status(root, addr("senator::alice/codex"), claim("s-01")));
        log.push(status(root, addr("senator::alice/claude"), claim("s-01")));
        assert_eq!(
            log.board().get(root).unwrap().owner,
            Some(TaskOwner {
                agent: path("alice/codex"),
                session: "s-01".into()
            })
        );
    }

    #[test]
    fn suspended_threads_are_listed_oldest_first() {
        let mut log = Log(Vec::new());
        let a = log.push(
            NewEvent::new(c(), Kind::Request, addr("senator::bob/claude"), "改 header")
                .to(addr("senator::alice/claude")),
        );
        let b = log.push(
            NewEvent::new(c(), Kind::Question, addr("senator::pm"), "能延期吗")
                .to(addr("senator::alice/claude")),
        );
        let mut esc = StatusPayload::new(StatusState::Suspended);
        esc.escalated_to = Some(name("alice"));
        log.push(status(b, addr("senator::alice/claude"), esc));
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
        let question = || NewEvent::new(c(), Kind::Question, addr("senator::bob/claude"), "?");
        let q = log.push(question().to(addr("senator::alice/claude")));
        assert!(needs_triage(&log.0[0]));
        // Not a root, not to an agent, from the Speaker, or already judged.
        log.push(question().to(addr("senator::alice/claude")).in_thread(q));
        log.push(question().to(addr("senator::alice")));
        log.push(
            question()
                .to(addr("guest::chat/u_1"))
                .to(Address::Committee),
        );
        log.push(
            NewEvent::new(c(), Kind::Question, Address::Speaker, "?")
                .to(addr("senator::alice/claude")),
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
                .to(addr("senator::alice/claude"))
                .with_meta(meta::TRIAGE, verdict.value()),
        );
        assert!(log.0[1..].iter().all(|e| !needs_triage(e)));

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
            NewEvent::new(c(), Kind::Note, addr("senator::bob/claude"), "")
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
                .to(addr("senator::wendell/claude")),
        )
    }

    fn from_agent(log: &mut Log, root: Seq, s: StatusPayload) -> Seq {
        log.push(status(root, addr("senator::wendell/claude"), s))
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
            NewEvent::new(
                c(),
                Kind::Answer,
                addr("senator::wendell/claude"),
                "The result",
            )
            .to(addr("senator::wendell"))
            .in_thread(root),
        );
        escalate(&mut log, root, "nine decisions\nneeded");
        log.push(
            NewEvent::new(c(), Kind::Checkpoint, addr("senator::wendell/claude"), "cp")
                .in_thread(root),
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
        assert_eq!(e.by, addr("senator::wendell/claude"));
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
                NewEvent::new(
                    c(),
                    kind.clone(),
                    addr("senator::wendell/claude"),
                    "Done\nmore",
                )
                .to(addr("senator::wendell"))
                .in_thread(root),
            );
            if kind == Kind::Note {
                // A note is not an answer; make it settled for agents.
                say(&mut log, Kind::Answer, "senator::wendell/claude", root);
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
            NewEvent::new(c(), Kind::Answer, addr("senator::wendell/claude"), "one")
                .to(addr("senator::wendell"))
                .in_thread(root),
        );
        let second = log.push(
            NewEvent::new(c(), Kind::Note, addr("senator::wendell/claude"), "two")
                .to(addr("senator::wendell"))
                .in_thread(root),
        );
        let t = log.board().get(root).unwrap().clone();
        assert!(t.waits_on_person(&wendell, first).is_some());
        assert!(t.waits_on_person(&wendell, second).is_none());
        let third = log.push(
            NewEvent::new(c(), Kind::Note, addr("senator::wendell/claude"), "three")
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
                .on_behalf_of(addr("senator::wendell/claude"))
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
        let to_speaker = NewEvent::new(c(), Kind::Question, addr("senator::wendell/claude"), "x")
            .to(Address::Speaker)
            .into_event(Seq(9), Utc::now());
        assert!(attention(&to_speaker, &wendell).is_none());
    }
}
