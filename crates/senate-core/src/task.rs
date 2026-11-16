//! Task threads: state, triage, and the view derived from them.
//!
//! A **task thread** is the thread rooted at a `question` or `request`
//! addressed to an agent, or at a Speaker `note` carrying
//! [`meta::WORK`](crate::meta::WORK) (a work run, [`crate::work`]). Its
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
//! # Triage gating
//!
//! A root for which [`needs_triage`] is true is not pushed to the target
//! agent's session until the Speaker has judged it. The verdict is a `note`
//! from the Speaker with `thread` = the root's seq and
//! [`meta::TRIAGE`](crate::meta::TRIAGE) = a [`TriagePayload`]. A relay the
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
/// `note` carrying [`meta::OUTPUT_LEVEL`](crate::meta::OUTPUT_LEVEL); the
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
        match event.kind {
            // A provisional answer only says where the question stands: the
            // thread stays open for the agent or a person.
            Kind::Answer if is_provisional(event) => {}
            Kind::Answer => {
                thread.answered = true;
                if thread.state != StatusState::Archived {
                    enter(thread, StatusState::Active);
                }
            }
            Kind::Status => {
                let Some(status) = StatusPayload::of(event) else {
                    return;
                };
                enter(thread, status.state);
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
                    thread.escalated_to = status.escalated_to;
                }
                if status.takeover.is_some() {
                    thread.takeover = status.takeover;
                }
                thread.reason = status.reason;
            }
            _ => {}
        }
    }
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
}
