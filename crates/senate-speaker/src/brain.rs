//! The Speaker's judgement, behind one object-safe trait.
//!
//! The actor decides *when* to think and does everything with the answer;
//! a [`Brain`] only reads and answers. Each method is one self-contained
//! question with everything it needs in its argument, so an implementation
//! keeps no conversation state between calls (the log is the memory) and a
//! new kind of judgement is a new method with a default body, as
//! [`Brain::brief_top`], [`Brain::triage`] and [`Brain::pick_agent`] have.
//!
//! Every input that leads to text people read carries the committee's
//! [`Locale`]; an implementation writes that language.

use async_trait::async_trait;
use std::time::Duration;

use senate_core::{
    Address, Brief, CheckpointRecord, ChildSenate, CommitteeId, Event, Kind, Locale, Locator, Name,
    OutputLevel, Roster, SenatorName, Seq, ThreadSummary, TriagePayload, TriageVerdict,
};
use serde::{Deserialize, Serialize};

/// Where an utterance was said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRef {
    pub platform: String,
    pub group_id: String,
    pub group_name: Option<String>,
    /// What was said here was simulated ([`senate_core::Source::simulated`]),
    /// carried onto what the Speaker records from it.
    pub simulated: bool,
}

/// Something a person said to the Speaker.
#[derive(Debug, Clone, PartialEq)]
pub struct Utterance {
    /// As said, mention stripped.
    pub text: String,
    /// Who said it: `senator::<handle>` for a person on the roster,
    /// `guest::<platform>/<id>` for anyone else.
    pub speaker: Address,
    /// The committee the group is bound to; `None` in a group bound to none.
    pub committee: Option<CommitteeId>,
    pub group: GroupRef,
    /// The committee's most recent events before this one, oldest first.
    /// Empty when the group is unbound.
    pub context: Vec<Event>,
    /// Who sits in this Senate: its people (with the names they go by) and
    /// the agents seated under each, so "alice's agent" can become the
    /// handle `alice` and "alice's codex" the agent `codex`.
    pub roster: Roster,
    /// Every seat on the committee, here and in child Senates, with its
    /// duty: whom an unaddressed question can be relayed to by duty. Empty
    /// in a group bound to no committee.
    pub seats: Vec<CommitteeSeat>,
    /// The language to reply in.
    pub locale: Locale,
    /// This Senate does coding work itself (`[speaker.work]` and a runner
    /// are configured): the `work` action is offered.
    pub work: bool,
    /// The seqs of relays in `context` put to a seat that is gone since
    /// (unseated, or no longer declared by its child Senate): a message
    /// sent again is not a duplicate of one of them.
    pub gone_relays: Vec<Seq>,
}

/// One seat on the acting committee, as a relay can reach it: an agent of
/// this Senate, or a seat a child Senate declares on the committee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitteeSeat {
    /// How a relay names it: the agent's own name (`codex`) here, or
    /// `<child>::<senator>` for a child's seat.
    pub target: String,
    /// How people are shown it, when it has an alias: the alias for an
    /// agent here, `senate-dev（dark-conclave）` for a child's seat. Display
    /// only; a relay names `target`.
    pub shown: Option<String>,
    /// What it does on the committee ([`senate_core::duty`]), when set.
    pub duty: Option<String>,
    /// It can be reached now: the agent is connected here, or the child is
    /// online and declares the seat online.
    pub online: bool,
    /// Its presence as people read it when it cannot be reached: the
    /// agent's presence here, or what the child declares (disconnected
    /// when the child has gone quiet). `None` when nothing is known.
    pub presence: Option<senate_core::PresenceState>,
}

impl CommitteeSeat {
    /// The seat as people are shown it: its alias when it has one, else
    /// its id as a mention (`[senator::codex]`, `[dark-conclave::claude-…]`).
    pub fn people_name(&self) -> String {
        match &self.shown {
            Some(shown) => shown.clone(),
            None if self.target.contains("::") => format!("[{}]", self.target),
            None => format!("[senator::{}]", self.target),
        }
    }
}

/// A directed question the Speaker must answer for an absent agent.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    /// The `question` event itself.
    pub event: Event,
    /// The agent it was put to. Answered from that agent's own checkpoint,
    /// never another agent's of the same person.
    pub agent: SenatorName,
    pub committee: CommitteeId,
    /// The committee's most recent events up to the question, oldest first.
    pub context: Vec<Event>,
    /// Set when the agent is away and the Speaker has taken over for it:
    /// answer only from this checkpoint, the brief and the log, and say when
    /// they do not cover the question.
    pub takeover: Option<Takeover>,
    /// The language to answer in.
    pub locale: Locale,
}

/// What a takeover answer may draw on besides the log.
#[derive(Debug, Clone, PartialEq)]
pub struct Takeover {
    /// The agent's own latest checkpoint: its files, branch, commit and seq.
    pub checkpoint: CheckpointRecord,
    /// How old the checkpoint was when the question was put to the Brain.
    pub age: Duration,
    /// The committee's brief, when it could be read.
    pub brief: Option<Brief>,
}

/// An answer drafted for an absent agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    /// False when the model said what it had to go on (in a takeover: the
    /// checkpoint files, the brief and the log) does not cover the
    /// question. The Speaker escalates such a takeover to people instead of
    /// posting the text.
    pub covered: bool,
}

impl Answer {
    /// A covered answer.
    pub fn covered(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            covered: true,
        }
    }

    /// An answer saying the material does not cover the question.
    pub fn not_covered(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            covered: false,
        }
    }
}

impl From<String> for Answer {
    fn from(text: String) -> Self {
        Self::covered(text)
    }
}

/// An incoming root a person may have to judge before an agent sees it: a
/// `question` or `request` to an agent, or a relay the Speaker is about to
/// write for a person.
#[derive(Debug, Clone, PartialEq)]
pub struct TriageInput {
    pub committee: CommitteeId,
    /// `question` or `request`.
    pub kind: Kind,
    /// Who asks: the agent or person behind it (for a relay, the person).
    pub asker: Address,
    /// The agents it is put to.
    pub to: Vec<Address>,
    pub body: String,
    /// The root's seq when it is already in the log; `None` for a relay
    /// not yet written.
    pub seq: Option<Seq>,
    /// The committee's most recent events before it, oldest first.
    pub context: Vec<Event>,
    /// The language of the one-line reason people read.
    pub locale: Locale,
}

/// The conservative threshold: a person is asked at
/// p(needs human) ≥ 0.5; otherwise the agent gets it when that is the most
/// likely reading, else it is delivered flagged as unsure.
pub fn triage_verdict(p_needs_human: f32, p_agent: f32, p_unsure: f32) -> TriageVerdict {
    if p_needs_human >= 0.5 {
        TriageVerdict::NeedsHuman
    } else if p_agent > p_unsure && p_agent >= p_needs_human {
        TriageVerdict::Agent
    } else {
        TriageVerdict::Unsure
    }
}

/// `payload` with its verdict recomputed from its probabilities by
/// [`triage_verdict`], when it carries any (a hard rule leaves them zero and
/// keeps its verdict).
pub fn apply_threshold(mut payload: TriagePayload) -> TriagePayload {
    let sum = payload.p_needs_human + payload.p_agent + payload.p_unsure;
    if sum > 0.0 && sum.is_finite() {
        payload.verdict = triage_verdict(payload.p_needs_human, payload.p_agent, payload.p_unsure);
    }
    payload
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RelayKind {
    /// The speaker expects an answer; a missing one is answered for the agent.
    Question,
    /// Information or an instruction to pass on; no answer is owed.
    Note,
    /// A change or a task for the agent to carry out: a `request`, which
    /// opens a task thread it reports back in.
    Request,
}

/// What the Speaker should do about an utterance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum Intent {
    /// Pass `body` to `targets`: each a person (handle or name as said),
    /// whose agent gets it, or one agent named outright (`codex`).
    Relay {
        targets: Vec<String>,
        kind: RelayKind,
        body: String,
    },
    /// Bind this group to `committee` (the raw id as said; validated later).
    Bind { committee: String },
    /// Unbind this group from its committee.
    Unbind,
    /// Report the binding and which agents are connected.
    Status,
    /// Record `body` as a decision of the committee, replacing the decision
    /// at `supersedes` when the speaker changes an earlier one.
    Decide {
        body: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supersedes: Option<Seq>,
    },
    /// Say what the committee has settled: the brief's top, decisions and
    /// open questions.
    Recall,
    /// Make this group the Agent group of `committee` (the group's own committee
    /// when `None`): agent traffic and task cards go here from now on.
    #[serde(rename = "adopt_agent_group")]
    AdoptAgentGroup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        committee: Option<String>,
    },
    /// Invite these people (handles or names as said) into the committee's
    /// Agent group.
    #[serde(rename = "invite_to_agent_group")]
    InviteToAgentGroup { who: Vec<String> },
    /// How much agent traffic the Agent group gets.
    #[serde(rename = "set_output_level")]
    SetOutputLevel { level: OutputLevel },
    /// Do coding work: `instruction`, starting from the latest checkpoint of
    /// `target`'s agent (a person as said, or one agent named outright).
    /// Only a developer of the committee may ask; it is always restated
    /// first.
    Work { target: String, instruction: String },
    /// Hand `from`'s work on this committee to `to` (each a person as said,
    /// or one agent named outright); `to` = the agent the work came from
    /// hands it back. Both are settled to concrete agents, and restated,
    /// before anything changes.
    Reassign { from: String, to: String },
    /// Nothing to do but say `reply` (which may be empty: say nothing).
    None { reply: String },
}

impl Intent {
    /// Whether running this changes the committee's or the group's state, so
    /// that agent mode restates it (with the rest of its plan) and waits for
    /// a yes before running it. Relays, `status`, `recall` and `none` run at
    /// once, as they do in intent mode.
    pub fn changes_state(&self) -> bool {
        !matches!(
            self,
            Self::Relay { .. } | Self::Status | Self::Recall | Self::None { .. }
        )
    }
}

/// One earlier turn of a conversation with the Speaker in agent mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// `senator::<handle>` (or a guest) for what a person said,
    /// [`Address::Speaker`] for the Speaker's reply.
    pub speaker: Address,
    pub text: String,
}

/// A conversation with the Speaker in agent mode (`[speaker] mode =
/// "agent"`): the latest thing a person said to it, and what was said
/// before in the same platform thread since it began. The Brain keeps no
/// state between calls; the engine holds the turns (in memory, for
/// `confirmation_ttl_secs` after the last one) and sends them every time.
#[derive(Debug, Clone, PartialEq)]
pub struct Conversation {
    /// The latest message, with its committee, group, log context, roster
    /// and locale.
    pub utterance: Utterance,
    /// The thread's earlier turns, oldest first; empty for a new
    /// conversation. Never includes `utterance` itself.
    pub turns: Vec<Turn>,
}

/// What the Speaker decided to do on one turn of a conversation.
///
/// The model reads and plans; the engine acts. Read-only lookups happen
/// inside the Brain's run (its tools) and show up only in `reply`. Every
/// state-changing operation is an entry in `actions`, which the engine
/// validates, checks permissions for, and runs through the same paths as
/// intent mode: relays (and `status` / `recall`) at once, and the
/// [`changes_state`](Intent::changes_state) ones restated together as one
/// plan that the speaker confirms with a yes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentTurn {
    /// What to say in the thread (may be empty: the actions speak for
    /// themselves). Posted before the actions' own replies.
    pub reply: String,
    /// Operations to run, in order. Never [`Intent::None`]; at most
    /// `agent_max_actions` (the engine refuses a longer plan).
    pub actions: Vec<Intent>,
    /// The Speaker asked the speaker something (a clarification) and expects
    /// an answer: their next message in this thread goes back to
    /// [`Brain::converse`] without an @.
    pub awaiting_reply: bool,
}

impl AgentTurn {
    /// A turn that only says `reply`.
    pub fn say(reply: impl Into<String>) -> Self {
        Self {
            reply: reply.into(),
            ..Self::default()
        }
    }

    /// A turn that asks the speaker `question` and waits for their answer.
    pub fn ask(question: impl Into<String>) -> Self {
        Self {
            reply: question.into(),
            actions: Vec::new(),
            awaiting_reply: true,
        }
    }

    /// A turn that runs `actions`, saying `reply` first.
    pub fn act(reply: impl Into<String>, actions: Vec<Intent>) -> Self {
        Self {
            reply: reply.into(),
            actions,
            awaiting_reply: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrainError {
    /// No model is configured for this committee: the fixed syntax is all
    /// there is, and a message that is not a command is not understood.
    #[error("no usable model is configured for this committee")]
    NoModel,
    /// The model could not be reached, ran out of budget, or failed.
    #[error("the brain is unavailable: {0}")]
    Unavailable(String),
    /// The model answered, but not in a usable shape.
    #[error("the brain answered out of shape: {0}")]
    Invalid(String),
}

#[async_trait]
pub trait Brain: Send + Sync + 'static {
    /// Read one utterance addressed to the Speaker. An error makes the
    /// Speaker fall back to its fixed syntax.
    async fn interpret(&self, utterance: Utterance) -> Result<Intent, BrainError>;

    /// Answer a directed question for its absent agent, from the log only
    /// (and, in a takeover, that agent's checkpoint and the brief). The text
    /// is stored as the Speaker's answer on the agent's behalf and shown
    /// under `[speaker::<agent>]` (with the localized takeover word in a
    /// takeover); an answer that is not [`covered`](Answer::covered) in a
    /// takeover is escalated to people instead.
    async fn answer_on_behalf(&self, question: Question) -> Result<Answer, BrainError>;

    /// The brief's "top of mind": at most three short lines, from the brief
    /// only, in `locale`. An error means no top this time; the brief is
    /// complete without one.
    async fn brief_top(&self, brief: &Brief, locale: Locale) -> Result<String, BrainError> {
        let _ = (brief, locale);
        Err(BrainError::Unavailable(
            "this brain does not write the brief's top".into(),
        ))
    }

    /// Judge an incoming root: the probabilities that it needs a person, that
    /// the agent can handle it, or that it is unclear, and one line of
    /// reason. No tools, no session; the Speaker bounds it by
    /// `triage_deadline_secs` and treats an error as *unsure*.
    async fn triage(&self, input: TriageInput) -> Result<TriagePayload, BrainError> {
        let _ = input;
        Err(BrainError::Unavailable("this brain does not triage".into()))
    }

    /// Agent mode: one turn of a conversation. The run may look things up
    /// with read-only tools (log, brief, status, members, presence,
    /// checkpoints) and returns what to say and which operations to run; it
    /// never changes anything itself. An error makes the Speaker fall back to
    /// its fixed syntax, as for [`interpret`](Self::interpret).
    async fn converse(&self, conversation: Conversation) -> Result<AgentTurn, BrainError> {
        let _ = conversation;
        Err(BrainError::Unavailable(
            "this brain does not converse".into(),
        ))
    }

    /// Which of a person's several agents a message for "that person's
    /// agent" should reach, when the one who asked did not say. The default
    /// is [`rule_pick`], which reads no duty; an implementation may weigh
    /// the task against each agent's duty and what it is working on instead
    /// (picking the one agent whose duty the task falls within), and answers [`Pick::Ask`] when it
    /// cannot tell, so the Speaker asks in the thread. An error, or no
    /// answer within the triage deadline, falls back to [`rule_pick`].
    async fn pick_agent(&self, input: PickInput) -> Result<Pick, BrainError> {
        Ok(rule_pick(&input))
    }

    /// An agent's message for a seat outside its own committee, put on the
    /// Speaker's desk on the way ([`meta::INTENDED`](senate_core::meta::INTENDED)):
    /// forward it, refuse it with a reason the asker reads, or leave it on
    /// the desk for the Speaker. The default is [`rule_crossing`]; an error,
    /// or no answer within the triage deadline, leaves it for the Speaker.
    async fn decide_crossing(&self, input: CrossingInput) -> Result<Crossing, BrainError> {
        Ok(rule_crossing(&input))
    }

    /// A question for a child Senate that is offline: answer it on the
    /// child's behalf from what this Senate can see, or hold it. Held, the
    /// asker is told the child is offline, and the question reaches the
    /// child when it reconnects either way. The default is
    /// [`rule_offline`]: hold. An error, or no answer within the triage
    /// deadline, holds it too.
    async fn decide_offline(&self, input: OfflineInput) -> Result<Offline, BrainError> {
        Ok(rule_offline(&input))
    }
}

/// A question addressed to a child Senate that is not online.
#[derive(Debug, Clone, PartialEq)]
pub struct OfflineInput {
    /// The committee it was written in.
    pub committee: CommitteeId,
    /// The question itself: a root addressed to the child Senate's desk.
    pub event: Event,
    /// The child Senate as it last declared itself here: its seats, each
    /// with its duty and presence, and when it was last seen. This is all
    /// this Senate knows of what is inside it.
    pub child: ChildSenate,
    /// The seat there the question was meant for, when one was named.
    pub intended: Option<Locator>,
    /// The committee's brief here (decisions in force, open questions,
    /// the latest checkpoints), when it could be read.
    pub brief: Option<Brief>,
    /// The committee's most recent events before the question, oldest
    /// first.
    pub context: Vec<Event>,
    /// The language of an answer.
    pub locale: Locale,
}

/// What the Speaker's office does with a question for an offline child
/// Senate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offline {
    /// Answer it on the child's behalf, with this text.
    Answer(String),
    /// Say the child is offline; the question waits for it.
    Hold,
}

/// The decision without a model: hold. Nothing here says what the child
/// would answer.
pub fn rule_offline(input: &OfflineInput) -> Offline {
    let _ = input;
    Offline::Hold
}

/// A message an agent wrote for a seat outside its committee, on the
/// Speaker's desk.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossingInput {
    /// The committee it was written in (the asker's).
    pub committee: CommitteeId,
    /// The message itself: a root addressed to `speaker`.
    pub event: Event,
    /// Where it was meant to go.
    pub intended: Locator,
    /// The committee's most recent events before it, oldest first.
    pub context: Vec<Event>,
    /// The language of a refusal's reason.
    pub locale: Locale,
}

/// What the desk does with a crossing message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Crossing {
    /// Deliver it where it was meant to go, as the Speaker's office.
    Forward,
    /// Tell the asker no, with this reason.
    Refuse(String),
    /// Leave it on the desk: the Speaker decides.
    Leave,
}

/// The crossing decision without a model, conservative: a plain question
/// is forwarded; anything else (a request, a note) is left for the
/// Speaker.
pub fn rule_crossing(input: &CrossingInput) -> Crossing {
    if input.event.kind == Kind::Question {
        Crossing::Forward
    } else {
        Crossing::Leave
    }
}

/// What the Speaker knows about one of a person's agents when it must pick
/// one of them.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentCandidate {
    pub agent: SenatorName,
    /// This agent's own connection: it can be reached now.
    pub present: bool,
    /// Its own latest checkpoint on the committee.
    pub checkpoint: Option<CheckpointRecord>,
    /// Task threads addressed to it that are not archived, longest waiting
    /// first.
    pub open_threads: Vec<ThreadSummary>,
    /// The thread the request was made in is this agent's: its root was
    /// addressed to it, or it claimed it.
    pub owns_thread: bool,
    /// What it does on the committee ([`senate_core::duty`]), when its seat
    /// has a duty: the first thing a pick weighs the task against.
    pub duty: Option<String>,
}

/// Several agents could be meant and the one who asked did not name one:
/// a person's agents, or whichever agent on the committee (`senator::*`).
#[derive(Debug, Clone, PartialEq)]
pub struct PickInput {
    pub committee: CommitteeId,
    /// The person whose agent is meant; `None` for whichever agent on the
    /// committee.
    pub owner: Option<Name>,
    /// Who asked.
    pub asker: Address,
    /// What the agent would be asked to do, in the asker's words.
    pub task: String,
    /// The task thread the request was made in, if any.
    pub thread: Option<Seq>,
    /// Every agent of `owner` seated here, in roster order (every agent on
    /// the committee when there is no owner).
    pub candidates: Vec<AgentCandidate>,
    pub locale: Locale,
}

/// The outcome of [`Brain::pick_agent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pick {
    /// This one (it must be one of the candidates; anything else is asked).
    Agent(SenatorName),
    /// It cannot be told: ask in the thread which one.
    Ask,
}

/// The pick without a model: the only candidate present, else the only one
/// whose thread the request was made in, else ask. Presence comes first
/// because a message that reaches a connected agent is answered now; the
/// thread decides between agents that are equally reachable.
pub fn rule_pick(input: &PickInput) -> Pick {
    let only = |keep: &dyn Fn(&AgentCandidate) -> bool| {
        let mut found = input.candidates.iter().filter(|c| keep(c));
        match (found.next(), found.next()) {
            (Some(one), None) => Some(one.agent.clone()),
            _ => None,
        }
    };
    only(&|c| c.present)
        .or_else(|| only(&|c| c.owns_thread))
        .map_or(Pick::Ask, Pick::Agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(agent: &str, present: bool, owns_thread: bool) -> AgentCandidate {
        AgentCandidate {
            agent: agent.parse().expect("path"),
            present,
            checkpoint: None,
            open_threads: Vec::new(),
            owns_thread,
            duty: None,
        }
    }

    fn input(candidates: Vec<AgentCandidate>) -> PickInput {
        PickInput {
            committee: CommitteeId::new("refund-v2").expect("committee"),
            owner: Some(Name::new("alice").expect("name")),
            asker: Address::senior(&Name::new("bob").expect("name")),
            task: "is the cap 3?".into(),
            thread: None,
            candidates,
            locale: Locale::En,
        }
    }

    fn agent(path: &str) -> Pick {
        Pick::Agent(path.parse().expect("path"))
    }

    #[test]
    fn the_only_present_agent_is_picked() {
        let picked = rule_pick(&input(vec![
            candidate("claude", false, true),
            candidate("codex", true, false),
        ]));
        // Presence first: the connected one, even when the other owns the thread.
        assert_eq!(picked, agent("codex"));
    }

    #[test]
    fn among_equally_reachable_agents_the_thread_decides() {
        for present in [true, false] {
            let picked = rule_pick(&input(vec![
                candidate("claude", present, false),
                candidate("codex", present, true),
            ]));
            assert_eq!(picked, agent("codex"), "present: {present}");
        }
    }

    #[test]
    fn otherwise_the_speaker_asks() {
        for candidates in [
            // Neither present, neither owns the thread.
            vec![
                candidate("claude", false, false),
                candidate("codex", false, false),
            ],
            // Both present, neither owns it.
            vec![
                candidate("claude", true, false),
                candidate("codex", true, false),
            ],
            // Both present and both own it (a thread addressed to both).
            vec![
                candidate("claude", true, true),
                candidate("codex", true, true),
            ],
            // Nobody to pick from.
            Vec::new(),
        ] {
            assert_eq!(rule_pick(&input(candidates)), Pick::Ask);
        }
    }

    #[test]
    fn the_default_brain_pick_is_the_rule() {
        struct Plain;
        #[async_trait]
        impl Brain for Plain {
            async fn interpret(&self, _: Utterance) -> Result<Intent, BrainError> {
                Err(BrainError::Unavailable("test".into()))
            }
            async fn answer_on_behalf(&self, _: Question) -> Result<Answer, BrainError> {
                Err(BrainError::Unavailable("test".into()))
            }
        }
        let input = input(vec![
            candidate("claude", true, false),
            candidate("codex", false, false),
        ]);
        let picked = futures_lite_block_on(Plain.pick_agent(input.clone()));
        assert_eq!(picked, Ok(rule_pick(&input)));
        assert_eq!(picked, Ok(agent("claude")));
    }

    /// Run a future that never waits on anything (the default pick is
    /// synchronous at heart).
    fn futures_lite_block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(future)
    }
}
