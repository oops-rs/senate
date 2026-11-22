//! What the Speaker does with each log event, in `seq` order per committee.
//!
//! # Task threads
//!
//! A root `question` / `request` to an agent opens a task thread, whose
//! state is the [`TaskBoard`] folded from the log (`engine::tasks`):
//!
//! - **Triage.** A root the bridge holds for a verdict
//!   ([`needs_triage`](senate_core::needs_triage)) is judged before anything
//!   else happens to it: a person's own root to their own agents is *agent*
//!   (rule `own_agent`), every other `request` needs a person (rule
//!   `request`), anything else goes to [`Brain::triage`] within
//!   `triage_deadline_secs`, thresholded conservatively (p(needs human) ≥
//!   0.5 → needs a person); no verdict in time is *unsure*. The verdict is a
//!   Speaker `note` in the root's thread with `meta.triage`. A relay the
//!   Speaker writes for a person is judged before it is written and carries
//!   its verdict inline.
//! - **Needs a person.** The thread is parked: a Speaker `status`
//!   `suspended` with `escalated_to` the person behind the agent it was put
//!   to (so it is in their inbox and on the "waiting on people" card), and,
//!   when that status is processed, a native mention of them in the thread
//!   (`@<handle>` written out where the surface has none, or where they have
//!   no account on it). Nobody answers for the agent of a parked thread.
//!   The person's hand-off in the thread (「交给我的 agent」, "hand it to my
//!   agent") re-addresses the root, as their own task (`from:
//!   senator::<them>`, inline verdict *agent*, rule `approved`), to the
//!   agent it was put to, and the thread is `processing` again. See
//!   `engine::tasks` for which agent when the root names none of theirs.
//! - **Human first.** Any other person's reply in a task thread, before an
//!   answer and before anyone took it, is a takeover: `suspended` with
//!   `takeover` (a person is handling it), which makes bridges cancel what
//!   is still queued. The asker adding to their own question is not one;
//!   after an answer a reply is a correction (the gateway marks it).
//! - **Escalate.** An agent's `status` with `escalated_to` mentions the
//!   person in the thread and tells the asker it went to that person: in
//!   their own platform thread when they asked through the Speaker, else as
//!   a note to the asking agent.
//!
//! # Which agent
//!
//! A person may have several agents. Wherever the Speaker starts from a
//! person (a relay, a hand-off, a work order, a reassignment), the roster
//! decides: one agent is it, none is the person, several is the one named,
//! else the one [`Brain::pick_agent`](crate::Brain::pick_agent) settles on,
//! else a question in the thread (`engine::route`).
//!
//! # The Agent group
//!
//! A committee's mirror group is its latest bound `agents` group, else its
//! first-bound people group still bound (`engine::agents`). The first
//! people group's binding creates the Agent group with the binder and every
//! person one of whose agents is connected; where the surface cannot, one
//! hint is posted in the people group, and `/agents` (or saying so) adopts a
//! group. A person one of whose agents shows up on the committee is
//! invited. When the last people group unbinds, the Agent group is renamed
//! as archived and unbound.
//!
//! Every task root posted to the mirror group is a card (a plain post where
//! the surface has no cards), updated in place on each `status` and
//! `answer` in its thread, at most once per `card_debounce_ms`; the update
//! that is due renders whatever the thread is by then, so the final state
//! always lands. At output level "answers only" a card changes only when it
//! is answered or closed; at "everything" each status is also a thread
//! reply. In a committee with an Agent group, a task thread's copy in a
//! people group gets only answers, signed with who answered and where it
//! came from. The two pinned cards and where every card's ref is recorded
//! are in the `engine::cards` docs.
//!
//! # Continuity
//!
//! `presence` and hand-off `checkpoint` events are lines in the mirror
//! group; `/status` and the pinned "today" card show each agent's presence.
//! A directed question to an agent that is `absent` (or already
//! `taken_over`) is answered at once from *that agent's own* latest
//! checkpoint, shown as the Speaker speaking for it with the takeover word,
//! or escalated to the committee's other developers when the checkpoint is
//! missing, stale, or does not cover it; the agent gets its mailbox handed
//! back when it returns. Another agent of the same person never stands in.
//! The decision table and the keys are in the `engine::takeover` docs.
//!
//! # Work on request and reassignment
//!
//! A developer of the committee may have the Speaker do coding work from an
//! agent's checkpoint, as a task thread with a card; any person on the
//! roster may hand one agent's work to another. Who may ask, how a run is
//! represented and made restart-safe, and what a reassignment changes are
//! in the `engine::work` docs.
//!
//! # Restart safety
//!
//! Every effect is idempotent, so reprocessing an event after a crash (the
//! cursor `speaker` is advanced only after an event's effects land) changes
//! nothing:
//!
//! - **Appends** carry an [`EventId`](senate_core::EventId) derived from the
//!   event that caused them (`ids::derived_event_id`), so the log stores a
//!   re-decided relay, binding, or answer once.
//! - **Posts** are skipped when [`Hub::mirrors_of_seq`] already holds the
//!   event's copy for that target (the group posted into), and otherwise
//!   carry an `operation_id` naming the log's epoch, the committee, the seq
//!   and the target (`ids::operation_id`). A post that landed but was not
//!   yet recorded comes back from the gateway as a duplicate, and a reset
//!   log (seq starting over under a new epoch) never reuses an old key.
//!   [`Hub::put_mirror`] is recorded right after.
//! - **State** (bindings and their roles, open questions, the latest
//!   decision and brief top, the task board, the brief) is rebuilt by
//!   observing the log up to the cursor; fallback timers are re-armed from
//!   the questions' `ts`, a brief top older than the last decision gets one
//!   refresh, a card whose thread moved past its last landed revision gets
//!   one update, and the pinned cards one refresh. Whether a proposal is
//!   still open, and whether a root was already judged, is read from the
//!   log when it is needed, so a replay never asks the model twice.
//!   Restatements, open questions about which agent was meant, and agent
//!   conversations are held in memory and lost on restart.
//! - **Surface calls** other than posts (cards, updates, pins, group
//!   creation, invites, renames) carry operation ids the same way and are
//!   recorded as `#`-suffixed mirror targets where they need finding again
//!   (`engine::cards`).
//!
//! # Thinking off the mailbox
//!
//! A Brain call can take most of a minute, so none runs on the actor's
//! mailbox. Each is spawned (at most [`BRAIN_CONCURRENCY`] at once, each
//! given up on the sum of its model tiers' `brain_deadline_secs` (the model,
//! its one retry, the fallback) + [`BRAIN_SLACK`] after it starts) and
//! its result comes back as a [`SpeakerMsg::Thought`]. A committee whose
//! `chat` awaits an interpretation stops at that chat, as on a stall, and
//! resumes from it when the result arrives, so its appends and posts keep
//! `seq` order; other committees carry on meanwhile. A task root waiting for
//! its triage, or a relaying `chat` for its relay's, stops its committee the
//! same way, but never longer than `triage_deadline_secs` from the moment
//! it is asked, a wait for a free Brain slot included. An answer for an
//! agent is appended when its draft arrives and mirrored in `seq` order like
//! any other answer; a brief top likewise, and it is never mirrored. The
//! one exception is the agent pick (`engine::route`), which is bounded by
//! `triage_deadline_secs` and answered at once by the rule-based default.
//!
//! A question due at once (an agent away, or with no live connection) is
//! answered for it only after the committee has caught up (the fallback
//! fires at once, but from the mailbox), and the agent's presence is read
//! again then: replaying old questions after a restart must not answer one
//! the agent already answered further down the log.
//!
//! # Failure policy
//!
//! A *permanent* failure (the log or the platform refuses) is logged and the
//! event is passed. A *transient* one ([`Stall`]), including a post with no
//! answer within [`POST_TIMEOUT`], stops the committee at that event; it is
//! retried from the cursor shortly, and live events for the committee are
//! ignored meanwhile (the retry reads them). A Brain failure or timeout is
//! never a stall: the fixed syntax and honest canned replies cover it.
//!
//! # Who may ask for what
//!
//! Only `chat` that came from the platform (`source` set) is read as an
//! instruction. Everything that changes state (binding, unbinding, a
//! decision, adopting an Agent group, inviting into it, its output level, a
//! work order, a reassignment) is for people on the roster only
//! ([`Address::may_change_state`] and listed); a guest gets one short
//! localized refusal. A guest may ask: a question relayed to an agent, the
//! status, the brief; a note or instruction pushed into an agent's session
//! is refused too. Only a person's yes confirms an agent's proposal; only
//! the person a thread waits on can hand it to their agent, and only a
//! person's reply takes a thread over. A restated action is confirmed or
//! cancelled only by the person who asked for it; a question about which
//! agent was meant is answered by the person who asked or the person whose
//! agents they are.
//!
//! # Languages
//!
//! Every text people read is rendered by `render` in the committee's
//! locale ([`SpeakerConfig::locale_of`](crate::SpeakerConfig::locale_of));
//! the unbound groups' in the configured one. Who wrote what is never text
//! in a body: `on_behalf_of` and the `meta` flags are set, and the copy is
//! headed by the event's byline.

mod agent;
mod agents;
mod agreement;
pub(crate) mod announcement;
mod cards;
mod desk;
mod gone;
mod memory;
mod offline;
mod route;
mod takeover;
mod tasks;
mod work;

pub(crate) use work::RunEvent;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use ractor::ActorRef;
use ractor::concurrency::JoinHandle;
use senate_core::{
    Address, BindingAction, BindingPayload, BriefBuilder, Card, ChatSurface, CommitteeId, Event,
    GroupRole, Hub, HubError, Kind, Locale, MirrorRef, Name, NewEvent, OutputLevel,
    PROPOSAL_DECISION, People, PostTarget, Posted, PresencePayload, PresenceState, Roster,
    SenatorName, Seq, Source, StatusPayload, SurfaceError, TaskBoard, TriagePayload, TriageVerdict,
    UnboundMention, WorkRun, meta,
};
use senate_work::{Forge, NoForge, Runner};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::actor::SpeakerArgs;
use crate::actor::{SpeakerMsg, Thought};
use crate::brain::{
    Answer, Brain, BrainError, GroupRef, Intent, Question, RelayKind, Takeover, TriageInput,
    Utterance, apply_threshold,
};
use crate::config::{SpeakerConfig, SpeakerMode, WorkConfig};
use crate::ids::{content_operation_id, derived_event_id, operation_id, unbound_operation_id};
use crate::render;
use crate::secretary::{Effective, SecretaryConfigs};
use crate::syntax::{is_affirmative, parse_command};
use route::{Choice, Settled, Waiting};

/// The Speaker's named cursor on every committee.
pub const CURSOR: &str = "speaker";

/// [`meta::TAKEOVER`]: `true` on an answer the Speaker wrote while it had
/// taken over for an away agent (shown as `[speaker::<agent>]` followed by
/// the localized takeover word, and handed back when the agent returns), and
/// on the `status` that sends such a question to people instead. Only the
/// Speaker's events count.
pub const TAKEOVER_META: &str = meta::TAKEOVER;

const PAGE: usize = 200;
/// Memory points one `/memory` lists.
const MEMORY_LIST: usize = 10;
/// How much recent log a Brain call is shown.
const CONTEXT_EVENTS: u64 = 30;
/// Pause before retrying a stalled committee.
const STALL_RETRY: Duration = Duration::from_secs(2);
/// Bound on one post, whatever the surface does inside it. Past it the post
/// counts as unavailable: the committee stalls and replays the same operation.
pub(crate) const POST_TIMEOUT: Duration = Duration::from_secs(30);
/// Added to the Brain's own run deadline for the outer bound on one call.
pub(crate) const BRAIN_SLACK: Duration = Duration::from_secs(15);
/// Brain calls in flight at once.
pub(crate) const BRAIN_CONCURRENCY: usize = 4;

/// A transient failure: retry this event later rather than pass it.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Stall {
    #[error("log unavailable: {0}")]
    Hub(#[from] HubError),
    #[error("chat surface unavailable: {0}")]
    Surface(String),
}

/// A checked operation either stalls for retry or is permanently refused.
enum ActionError {
    Retry(Stall),
    Refused,
}

impl From<Stall> for ActionError {
    fn from(error: Stall) -> Self {
        match error {
            Stall::Hub(HubError::Rejected(_) | HubError::NotFound(_)) => {
                warn!(%error, "state-changing action refused");
                Self::Refused
            }
            error => Self::Retry(error),
        }
    }
}

/// How far one event got, short of a stall.
enum Flow {
    /// Its effects landed; move the cursor past it.
    Done,
    /// It waits for a Brain call; the committee stops here until it returns.
    Await,
}

type Timer = JoinHandle<Result<(), ractor::MessagingErr<SpeakerMsg>>>;

/// The brief's top for one committee: rewritten a debounce after a decision
/// lands, one draft at a time.
#[derive(Default)]
struct TopRefresh {
    timer: Option<Timer>,
    /// A draft is with the Brain.
    drafting: bool,
    /// A decision landed while drafting: refresh again after this one.
    again: bool,
    /// The next refresh runs even if a top was written after the last
    /// decision (that top was drafted before the decision).
    forced: bool,
}

/// A directed question still owed an answer by one agent.
struct Open {
    question: Seq,
    ts: DateTime<Utc>,
    timer: Option<Timer>,
    /// An answer for the agent is being drafted.
    answering: bool,
}

/// A group bound to the committee, as observed.
#[derive(Debug, Clone)]
struct BoundGroup {
    group: GroupRef,
    role: GroupRole,
    /// The `binding` that bound it: the anchor of its pinned cards.
    seq: Seq,
}

/// The two pinned cards of the Agent group: debounced updates, and what
/// each last showed (in memory; a restart sends one refresh).
#[derive(Default)]
struct Pinned {
    timer: Option<Timer>,
    /// The day a midnight refresh is armed for.
    midnight_for: Option<NaiveDate>,
    midnight: Option<Timer>,
    waiting: Option<Card>,
    today: Option<Card>,
}

#[derive(Default)]
struct CommitteeState {
    /// The language this committee's people read.
    locale: Locale,
    cursor: Seq,
    /// Bound groups in bind order. The mirror group is the latest `agents`
    /// group, else the first people group.
    groups: Vec<BoundGroup>,
    /// `(thread root, agent)` → the question awaiting that agent's answer.
    open: HashMap<(Seq, SenatorName), Open>,
    stalled: bool,
    /// The `chat` this committee is stopped at until its interpretation
    /// returns.
    awaiting: Option<Seq>,
    /// Decisions already taken for a `chat`, so a step repeated after a
    /// stall or an interpretation acts on the same one.
    decisions: HashMap<Seq, Decision>,
    /// The latest `decision` event and the latest brief top, as observed.
    last_decision: Seq,
    last_top: Seq,
    top: TopRefresh,
    /// The committee's task threads, folded from the log like everything
    /// here.
    board: TaskBoard,
    /// The Senate's people as the roster said when this state was loaded:
    /// every other senator in the log is an agent.
    people: People,
    brief: Option<BriefBuilder>,
    /// Task root → the start of its latest answer, for its card.
    answers: HashMap<Seq, String>,
    /// Task root → when it was asked (today's tasks).
    root_ts: HashMap<Seq, DateTime<Utc>>,
    /// Roots that began as simulated chat (`senate chat send`): left out of
    /// the "today" card with everything said in them, as the brief leaves
    /// them out.
    simulated_roots: HashSet<Seq>,
    /// Each agent's latest event: `(seq, ts, what)`.
    activity: HashMap<SenatorName, (Seq, DateTime<Utc>, String)>,
    /// Each seat in a child Senate's latest event here, by its relay
    /// target (`<child>::<senator>`): a relay down meant for it, or an
    /// answer it gave coming up. `(seq, ts, what)`.
    seat_activity: HashMap<String, (Seq, DateTime<Utc>, String)>,
    /// Verdicts that came back from the Brain for a task root or a relaying
    /// `chat`, keyed by that seq, until the step that uses them is done.
    triage: HashMap<Seq, TriagePayload>,
    /// Agents the Speaker picked for a person while acting on a `chat`, by
    /// (chat seq, step key, person), so a step repeated after a stall or a
    /// triage wait acts on the same agent.
    picks: HashMap<(Seq, String, Option<Name>), SenatorName>,
    /// The seat told in a child Senate for a relay going down to it
    /// (`None`: it could not be told, the desk there decides), by (chat
    /// seq, step key, child Senate), kept like `picks`.
    seat_picks: HashMap<(Seq, String, Name), Option<SenatorName>>,
    /// Debounced card updates, per task root.
    cards: HashMap<Seq, Timer>,
    pinned: Pinned,
    /// Each people group's announcement, by group id.
    announcements: HashMap<String, announcement::Announced>,
    /// People invited into the Agent group this run (because one of their
    /// agents showed up).
    invited: HashSet<Name>,
    /// Each agent's latest `presence` event, as folded from the log.
    presence: HashMap<SenatorName, PresenceSeen>,
    /// Each agent's latest `taken_over`, and the answers given for it since.
    taken_over: HashMap<SenatorName, (Seq, Vec<Covered>)>,
    /// Each work run's latest state, by root.
    work: BTreeMap<Seq, WorkRun>,
    /// Agents whose work is reassigned → the agent it is with.
    reassigned: HashMap<SenatorName, SenatorName>,
    /// A hand-back's `presence` seq → the agent that held the work.
    handbacks: HashMap<Seq, SenatorName>,
    /// The built-in session's own name: its answers for an agent whose
    /// secretary it is count as that agent's.
    me: Option<SenatorName>,
    /// Crossing decisions back from the Brain, by desk item seq, until the
    /// step that uses them is done.
    crossings: HashMap<Seq, crate::brain::Crossing>,
    /// What was decided for a question to a child Senate that is offline,
    /// by the question's seq, until it is carried out.
    offline: HashMap<Seq, crate::brain::Offline>,
    /// Questions whose seat is gone that the office has closed this run
    /// ([`gone`]), so the note is not written again before it is observed.
    seat_removed: HashSet<Seq>,
    /// What was said since the last digest, and the digest timers
    /// ([`memory`]).
    discussion: memory::Discussion,
    /// Each group's latest burst of discussion, and the decisions proposed
    /// from discussion before ([`agreement`]).
    bursts: agreement::Bursts,
}

/// The seat in a child Senate an event is about, as a relay names it
/// (`<child>::<senator>`): a message to the child's desk meant for that
/// seat ([`meta::INTENDED`]), or an answer from the child that the seat
/// gave ([`meta::ANSWERED_BY`]).
fn child_seat_of(event: &Event) -> Option<String> {
    let (child, locator) = if let Address::Child { senate, .. } = &event.from {
        let by = event.meta.get(meta::ANSWERED_BY)?.as_str()?;
        (senate, by.parse::<senate_core::Locator>().ok()?)
    } else {
        let locator = senate_core::locator::intended(event)?;
        let child = event.to.iter().find_map(|a| match a {
            Address::Child { senate, .. } => Some(senate),
            _ => None,
        })?;
        (child, locator)
    };
    let seat = locator.address().as_senator()?;
    (locator.senate()?.name() == child).then(|| format!("{child}::{seat}"))
}

/// An agent's latest presence transition in the log.
#[derive(Debug, Clone, Copy)]
struct PresenceSeen {
    seq: Seq,
    state: PresenceState,
    /// Paused on purpose ([`senate_core::PresencePayload::paused`]).
    paused: bool,
    /// The state before it: the previous event's, else the payload's own.
    previous: Option<PresenceState>,
}

/// An answer the Speaker gave for an agent while it had taken over.
#[derive(Debug, Clone)]
struct Covered {
    answer: Seq,
    /// The question it answered (its thread root when not known).
    question: Seq,
    body: String,
}

impl CommitteeState {
    /// Observing is idempotent: a repeated step observes its event again.
    fn observe(&mut self, platform: &str, event: &Event) {
        self.board.observe(event);
        if let Some(run) = work::work_root_run(event) {
            self.work.entry(event.seq).or_insert(run);
        } else if event.kind == Kind::Status
            && let Some(root) = event.thread
            && self.work.contains_key(&root)
            && let Some(run) = WorkRun::of(event)
        {
            self.work.insert(root, run);
        }
        self.brief
            .get_or_insert_with(|| BriefBuilder::new(event.committee.clone()))
            .observe(event);
        self.discussion.observe(event);
        self.bursts.observe(event);
        let simulated = event.is_simulated()
            || event
                .thread
                .is_some_and(|root| self.simulated_roots.contains(&root));
        if simulated && event.thread.is_none() {
            self.simulated_roots.insert(event.seq);
        }
        if let Some(agent) = self.people.as_agent(&event.from)
            && !simulated
        {
            self.activity.insert(
                agent.clone(),
                (event.seq, event.ts, render::activity(self.locale, event)),
            );
        }
        if let Some(seat) = child_seat_of(event) {
            self.seat_activity.insert(
                seat,
                (event.seq, event.ts, render::activity(self.locale, event)),
            );
        }
        match event.kind {
            Kind::Binding => {
                // Only the Speaker binds groups (the gateway applies the same
                // rule); anything else must not redirect where mirrors go.
                if event.from != Address::Speaker {
                    return;
                }
                let Some(binding) = event.binding().filter(|b| b.platform == platform) else {
                    return;
                };
                self.groups.retain(|g| g.group.group_id != binding.group_id);
                if binding.action == BindingAction::Bind {
                    self.groups.push(BoundGroup {
                        group: GroupRef {
                            platform: binding.platform,
                            group_id: binding.group_id,
                            group_name: binding.group_name,
                            simulated: false,
                            simulated_by: None,
                        },
                        role: binding.role,
                        seq: event.seq,
                    });
                }
            }
            Kind::Question | Kind::Request => {
                let root = event.thread.unwrap_or(event.seq);
                if event.thread.is_none() && self.board.get(event.seq).is_some() && !simulated {
                    self.root_ts.insert(event.seq, event.ts);
                }
                if event.kind == Kind::Request || parked_inline(event) {
                    // Nothing to answer on behalf: a request is not a
                    // question, and a parked relay waits on its person.
                    return;
                }
                for agent in question_targets(event, &self.people) {
                    self.open.entry((root, agent)).or_insert(Open {
                        question: event.seq,
                        ts: event.ts,
                        timer: None,
                        answering: false,
                    });
                }
            }
            Kind::Answer => {
                if let Some(root) = event.thread
                    && self.board.get(root).is_some()
                    && !senate_core::is_provisional(event)
                {
                    self.answers.insert(root, render::excerpt(&event.body));
                }
                let (Some(root), Some(agent)) = (
                    event.thread,
                    answering_agent(event, &self.people, self.me.as_ref()),
                ) else {
                    return;
                };
                // An answer by the agent an agent's work is reassigned to
                // answers for it too.
                let covered: Vec<SenatorName> = self
                    .reassigned
                    .iter()
                    .filter(|(_, owner)| **owner == agent)
                    .map(|(old, _)| old.clone())
                    .collect();
                for old in covered {
                    if let Some(open) = self.open.remove(&(root, old))
                        && let Some(timer) = open.timer
                    {
                        timer.abort();
                    }
                }
                let open = self.open.remove(&(root, agent.clone()));
                if desk::is_cover(event, self.me.as_ref())
                    && let Some((_, covered)) = self.taken_over.get_mut(&agent)
                    && !covered.iter().any(|c| c.answer == event.seq)
                {
                    covered.push(Covered {
                        answer: event.seq,
                        question: open.as_ref().map_or(root, |o| o.question),
                        body: event.body.clone(),
                    });
                }
                if let Some(open) = open
                    && let Some(timer) = open.timer
                {
                    timer.abort();
                }
            }
            Kind::Presence if event.from == Address::Committee => {
                let Some(payload) = PresencePayload::of(event) else {
                    return;
                };
                let seen = self.presence.get(&payload.agent).copied();
                if seen.is_some_and(|s| s.seq >= event.seq) {
                    return; // observed already
                }
                if payload.state == PresenceState::TakenOver {
                    self.taken_over
                        .insert(payload.agent.clone(), (event.seq, Vec::new()));
                }
                match (payload.state, &payload.new_owner) {
                    (PresenceState::Reassigned, Some(owner)) => {
                        self.reassigned.insert(payload.agent.clone(), owner.clone());
                    }
                    (state, _) => {
                        if let Some(owner) = self.reassigned.remove(&payload.agent)
                            && state == PresenceState::Online
                        {
                            self.handbacks.insert(event.seq, owner);
                        }
                    }
                }
                self.presence.insert(
                    payload.agent.clone(),
                    PresenceSeen {
                        seq: event.seq,
                        state: payload.state,
                        paused: payload.paused,
                        previous: seen.map(|s| s.state).or(payload.previous),
                    },
                );
            }
            Kind::Decision => self.last_decision = self.last_decision.max(event.seq),
            Kind::Note if event.from == Address::Speaker && is_brief_top(event) => {
                self.last_top = self.last_top.max(event.seq);
            }
            Kind::Note if event.from == Address::Speaker => {
                // Parked on its person: nobody answers for the agent.
                if let Some(root) = event.thread
                    && TriagePayload::of(event)
                        .is_some_and(|t| t.verdict == TriageVerdict::NeedsHuman)
                {
                    self.close_open(root);
                }
            }
            Kind::Status => {
                // Waiting on a person, or a person took it, or a takeover sent
                // it to people: nobody answers for the agent either.
                if let (Some(root), Some(status)) = (event.thread, StatusPayload::of(event))
                    && (status.takeover.is_some()
                        || status.escalated_to.is_some()
                        || render::is_takeover(event))
                {
                    self.close_open(root);
                }
            }
            _ => {}
        }
    }

    /// Stop waiting for answers in thread `root`.
    fn close_open(&mut self, root: Seq) {
        self.open.retain(|(thread, _), open| {
            if *thread != root {
                return true;
            }
            if let Some(timer) = open.timer.take() {
                timer.abort();
            }
            false
        });
    }

    /// The latest still-bound Agent group.
    fn agents_group(&self) -> Option<&BoundGroup> {
        self.groups
            .iter()
            .rev()
            .find(|g| g.role == GroupRole::Agents)
    }

    fn people_groups(&self) -> impl Iterator<Item = &BoundGroup> {
        self.groups.iter().filter(|g| g.role == GroupRole::People)
    }

    fn output_level(&self) -> OutputLevel {
        self.board.output_level()
    }

    /// A decision landed after the last top (or one landed mid-draft).
    fn top_stale(&self) -> bool {
        self.top.forced || self.last_decision > self.last_top
    }

    /// Where agents' traffic goes: the Agent group when there is one, else
    /// the first-bound people group still bound.
    fn mirror_group(&self) -> Option<&GroupRef> {
        self.agents_group()
            .or_else(|| self.people_groups().next())
            .map(|g| &g.group)
    }

    /// Stopped at an event, for a retry or a Brain result.
    fn paused(&self) -> bool {
        self.stalled || self.awaiting.is_some()
    }
}

/// A person's agent: their one agent seated here, several of them, or none
/// (the person, then, gets what was meant for it). Agents are seated under
/// the person who runs them ([`Roster::owner_of`]).
pub(crate) fn agent_of(roster: &Roster, owner: &Name) -> senate_core::AgentOf {
    use senate_core::AgentOf;
    match roster.agents_of(owner).as_slice() {
        [] => AgentOf::None(SenatorName::new(owner.clone()).ok()),
        [one] => AgentOf::One((*one).clone()),
        several => AgentOf::Several(several.iter().map(|a| (*a).clone()).collect()),
    }
}

/// The agents a question is put to (its `to`, minus the asker). A person in
/// `to` is not here: nobody answers for a person.
fn question_targets(event: &Event, people: &People) -> Vec<SenatorName> {
    if event.is_chair_discussion() {
        return Vec::new();
    }
    event
        .to
        .iter()
        .filter(|address| event.from != **address)
        .filter_map(|address| people.as_agent(address))
        .cloned()
        .collect()
}

/// Whose answer this is: the agent itself, or the Speaker (or the built-in
/// session as its secretary, `me`) speaking for it.
fn answering_agent(
    event: &Event,
    people: &People,
    me: Option<&SenatorName>,
) -> Option<SenatorName> {
    match (&event.from, &event.on_behalf_of) {
        (Address::Speaker, Some(behalf)) => people.as_agent(behalf).cloned(),
        (Address::Senator(from), Some(behalf)) if Some(from) == me => {
            people.as_agent(behalf).cloned()
        }
        (from, _) => people.as_agent(from).cloned(),
    }
}

#[derive(Debug, Clone)]
enum Decision {
    Run {
        intent: Intent,
        confirm: bool,
    },
    Agent(crate::brain::AgentTurn),
    Say(String),
    /// The model failed on every tier: the asker gets one line
    /// ([`render::model_unavailable`]), the error goes to the people who
    /// run the Speaker ([`Engine::report_model_failure`]).
    ModelDown(String),
}

#[derive(Debug, Clone, PartialEq)]
enum PendingAction {
    Bind {
        from: Option<CommitteeId>,
        to: CommitteeId,
    },
    Unbind {
        committee: CommitteeId,
    },
    /// Make the group `committee`'s Agent group; `from_people` when it is
    /// that committee's people group now, which it then stops being.
    AdoptAgents {
        committee: CommitteeId,
        from_people: bool,
    },
    /// Make the group, now `committee`'s Agent group, its people group
    /// again.
    ToPeople {
        committee: CommitteeId,
    },
    /// Record a decision said in `chat` of `committee`; `chat` is `None` when
    /// it was said in another committee's log (an agent plan's step after a
    /// Bind), and the decision is acknowledged in the plan's result instead.
    Decide {
        committee: CommitteeId,
        chat: Option<Seq>,
        key: String,
        body: String,
        supersedes: Option<Seq>,
    },
    Invite {
        committee: CommitteeId,
        group: String,
        handles: Vec<String>,
        ids: Vec<String>,
    },
    OutputLevel {
        committee: CommitteeId,
        level: OutputLevel,
    },
    /// A relay planned after a Bind in an agent plan: written on the new
    /// committee once the plan is confirmed.
    Relay {
        event: Box<NewEvent>,
    },
    /// `status` / `recall` planned after a Bind: answered for the new
    /// committee once the plan is confirmed.
    Status {
        committee: CommitteeId,
    },
    Recall {
        committee: CommitteeId,
    },
    /// Start a work run.
    Work {
        committee: CommitteeId,
        run: Box<WorkRun>,
    },
    /// Hand `agent`'s work to `to`; `to == agent` hands it back from
    /// `owner`.
    Reassign {
        committee: CommitteeId,
        agent: SenatorName,
        to: SenatorName,
        owner: Option<SenatorName>,
    },
    /// A person's correction or deletion of what the committee knows.
    Memory {
        committee: CommitteeId,
        key: String,
        payload: senate_core::MemoryPayload,
        body: String,
    },
}

impl PendingAction {
    fn restate(&self, l: Locale) -> String {
        match self {
            Self::Work { run, .. } => render::confirm_work(l, run),
            Self::ToPeople { committee } => render::offer_back_to_people(l, committee),
            _ => render::confirm_action(l, &self.describe(l)),
        }
    }

    /// The committee this action acts on, when its description does not
    /// already name it.
    /// The committee this action acts on: its texts are in that
    /// committee's language.
    fn committee(&self) -> &CommitteeId {
        match self {
            Self::Bind { to, .. } => to,
            Self::Unbind { committee }
            | Self::AdoptAgents { committee, .. }
            | Self::ToPeople { committee }
            | Self::Decide { committee, .. }
            | Self::Invite { committee, .. }
            | Self::OutputLevel { committee, .. }
            | Self::Status { committee }
            | Self::Recall { committee }
            | Self::Work { committee, .. }
            | Self::Reassign { committee, .. }
            | Self::Memory { committee, .. } => committee,
            Self::Relay { event } => &event.committee,
        }
    }

    fn unnamed_committee(&self) -> Option<&CommitteeId> {
        match self {
            Self::Bind { .. }
            | Self::Unbind { .. }
            | Self::AdoptAgents { .. }
            | Self::ToPeople { .. } => None,
            Self::Decide { committee, .. }
            | Self::Invite { committee, .. }
            | Self::OutputLevel { committee, .. }
            | Self::Status { committee }
            | Self::Recall { committee }
            | Self::Work { committee, .. }
            | Self::Reassign { committee, .. }
            | Self::Memory { committee, .. } => Some(committee),
            Self::Relay { event } => Some(&event.committee),
        }
    }

    /// One line of an agent plan: the description, naming the committee the
    /// step acts on (a step after a Bind acts on the new one).
    fn plan_line(&self, l: Locale) -> String {
        match self.unnamed_committee() {
            Some(committee) => render::on_committee(l, committee, &self.describe(l)),
            None => self.describe(l),
        }
    }

    fn describe(&self, l: Locale) -> String {
        match self {
            Self::Decide {
                body, supersedes, ..
            } => render::describe_decide(l, body, *supersedes),
            Self::Bind { from: None, to } => render::describe_bind(l, to),
            Self::Bind {
                from: Some(from),
                to,
            } => render::describe_rebind(l, from, to),
            Self::Unbind { committee } => render::describe_unbind(l, committee),
            Self::AdoptAgents {
                committee,
                from_people,
            } => render::describe_adopt(l, committee, *from_people),
            Self::ToPeople { committee } => render::describe_back_to_people(l, committee),
            Self::Invite { handles, .. } => render::describe_invite(l, handles),
            Self::OutputLevel { level, .. } => render::describe_output_level(l, *level),
            Self::Relay { event } => render::describe_relay(l, &event.to, &event.body),
            Self::Status { .. } => render::describe_status(l),
            Self::Recall { .. } => render::describe_recall(l),
            Self::Work { run, .. } => render::describe_work(l, run),
            Self::Memory { payload, body, .. } => match payload {
                senate_core::MemoryPayload::Delete { targets } => {
                    render::describe_forget(l, targets)
                }
                senate_core::MemoryPayload::Correct { target, .. } => {
                    render::describe_correct(l, *target, body)
                }
                senate_core::MemoryPayload::Point { .. } => String::new(),
            },
            Self::Reassign {
                committee,
                agent,
                to,
                owner,
            } => match owner {
                Some(owner) if agent == to => render::describe_handback(l, committee, agent, owner),
                _ => render::describe_reassign(l, committee, agent, to),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    /// Asked by `chat` `seq` in `committee`.
    Bound { committee: CommitteeId, seq: Seq },
    /// Asked in a group bound to no committee.
    Unbound,
}

/// A state-changing action restated and awaiting a yes.
#[derive(Debug, Clone)]
struct Pending {
    /// The restatement the Speaker posted.
    confirm_ref: String,
    /// The thread it lives in.
    thread_ref: Option<String>,
    /// The human message that asked (the thread's root).
    origin_ref: String,
    origin: Origin,
    group: GroupRef,
    actions: Vec<PendingAction>,
    agent_plan: bool,
    execution: Option<agent::Execution>,
    /// Who asked: the only person whose reply confirms or cancels.
    requester: Address,
    expires: Instant,
}

impl Pending {
    /// Whether an unbound-group message sits in this restatement's thread
    /// or replies to it.
    fn answered_by(&self, mention: &UnboundMention) -> bool {
        (mention.thread_ref.is_some() && mention.thread_ref == self.thread_ref)
            || mention.reply_to_message_ref.as_deref() == Some(self.confirm_ref.as_str())
            || (!self.origin_ref.is_empty()
                && mention.thread_root_message_ref.as_deref() == Some(self.origin_ref.as_str()))
    }
}

/// What a finished Brain call carries back to the mailbox.
pub(crate) enum ThoughtKind {
    /// `chat` `seq` of `committee`, interpreted.
    Interpreted {
        committee: CommitteeId,
        seq: Seq,
        intent: Result<Intent, BrainError>,
    },
    Conversed {
        committee: CommitteeId,
        seq: Seq,
        turn: Result<crate::brain::AgentTurn, BrainError>,
    },
    UnboundConversed {
        mention: Box<UnboundMention>,
        turn: Result<crate::brain::AgentTurn, BrainError>,
    },
    /// A mention in a group bound to no committee, interpreted.
    UnboundInterpreted {
        mention: Box<UnboundMention>,
        intent: Result<Intent, BrainError>,
    },
    /// An answer drafted for `agent` to `question` (thread `root`), owed to
    /// `asker`; `takeover` when it was drafted from the agent's own
    /// checkpoint while it is away.
    Answered {
        committee: CommitteeId,
        root: Seq,
        agent: SenatorName,
        question: Seq,
        asker: Address,
        takeover: bool,
        /// Drafted as the agent's secretary rather than for the desk.
        secretary: bool,
        answer: Result<Answer, BrainError>,
    },
    /// The desk's decision on a crossing at `seq`.
    Crossed {
        committee: CommitteeId,
        seq: Seq,
        crossing: Result<crate::brain::Crossing, BrainError>,
    },
    /// The decision for the question at `seq`, put to a child Senate that
    /// is offline.
    OfflineDecided {
        committee: CommitteeId,
        seq: Seq,
        decision: Result<crate::brain::Offline, BrainError>,
    },
    /// A triage verdict for the task root, or the relaying `chat`, at
    /// `seq`.
    Triaged {
        committee: CommitteeId,
        seq: Seq,
        triage: Result<TriagePayload, BrainError>,
    },
    /// The brief's top drafted for `committee`, from a brief that covers the
    /// decisions up to `basis`.
    TopWritten {
        committee: CommitteeId,
        basis: Seq,
        top: Result<String, BrainError>,
    },
    /// A digest drafted from the discussion `from..=through`, which showed
    /// the Brain the seqs in `shown`.
    Digested {
        committee: CommitteeId,
        trigger: senate_core::DigestTrigger,
        from: Seq,
        through: Seq,
        shown: std::collections::BTreeSet<Seq>,
        out: Result<crate::brain::DigestOut, BrainError>,
    },
    /// What the Brain found in a burst of discussion.
    Spotted {
        committee: CommitteeId,
        burst: Vec<Event>,
        agreement: Result<Option<crate::brain::Agreement>, BrainError>,
    },
}

impl ThoughtKind {
    /// The committee the thought is about; `None` for a mention in a group
    /// bound to none.
    pub(crate) fn committee(&self) -> Option<&CommitteeId> {
        match self {
            Self::Interpreted { committee, .. }
            | Self::Conversed { committee, .. }
            | Self::Answered { committee, .. }
            | Self::Crossed { committee, .. }
            | Self::OfflineDecided { committee, .. }
            | Self::Triaged { committee, .. }
            | Self::TopWritten { committee, .. }
            | Self::Digested { committee, .. }
            | Self::Spotted { committee, .. } => Some(committee),
            Self::UnboundConversed { .. } | Self::UnboundInterpreted { .. } => None,
        }
    }
}

type Deliver<T> = Box<dyn FnOnce(Result<T, BrainError>) -> ThoughtKind + Send>;

/// Carries one Brain call's result to the mailbox exactly once. Dropped
/// without a result (the Brain panicked, or the call was aborted), it
/// reports a failure instead, so a committee never waits on a call that will
/// not return.
struct Delivery<T> {
    myself: ActorRef<SpeakerMsg>,
    deliver: Option<Deliver<T>>,
}

impl<T> Delivery<T> {
    fn send(mut self, result: Result<T, BrainError>) {
        self.deliver_now(result);
    }

    fn deliver_now(&mut self, result: Result<T, BrainError>) {
        if let Some(deliver) = self.deliver.take() {
            // Fails only when the Speaker is gone, and then nobody waits.
            let _ = self
                .myself
                .send_message(SpeakerMsg::Thought(Thought(deliver(result))));
        }
    }
}

impl<T> Drop for Delivery<T> {
    fn drop(&mut self) {
        self.deliver_now(Err(BrainError::Unavailable(
            "the brain call ended without a result".into(),
        )));
    }
}

pub(crate) struct Engine {
    hub: Arc<dyn Hub>,
    surface: Arc<dyn ChatSurface>,
    brain: Arc<dyn Brain>,
    /// Who sits in this Senate, as [`Hub::roster`] said last. Read again
    /// before each event is acted on; when the hub cannot say, the last
    /// snapshot stands.
    roster: Roster,
    /// Locales, the bot's name, and the rest of the configuration.
    config: SpeakerConfig,
    /// Questions the Speaker asked about which agent was meant.
    choices: Vec<Choice>,
    fallback_after: Duration,
    confirmation_ttl: Duration,
    top_debounce: Duration,
    brain_timeout: Duration,
    triage_deadline: Duration,
    card_debounce: Duration,
    takeover_stale: Duration,
    /// Cards were found unsupported (logged once).
    cards_unsupported: AtomicBool,
    /// The log's epoch, part of every operation id.
    epoch: Option<String>,
    myself: ActorRef<SpeakerMsg>,
    committees: HashMap<CommitteeId, CommitteeState>,
    pending: Vec<Pending>,
    mode: SpeakerMode,
    agent_max_actions: usize,
    conversations: Vec<agent::Session>,
    agent_progress: HashMap<(CommitteeId, Seq), agent::Progress>,
    preparing: Option<Vec<PendingAction>>,
    /// While an agent plan is prepared: the committee its next step acts on
    /// when that is not the chat's own (a Bind earlier in the plan), and
    /// whether read-only steps wait for a yes too (they come after a Bind).
    acting: Option<CommitteeId>,
    deferring: bool,
    agent_replies: agent::Replies,
    permits: Arc<Semaphore>,
    /// Brain calls in flight. Aborted when the engine drops, which happens
    /// with the actor's state however the actor ends.
    thinking: JoinSet<()>,
    /// Calls spawned whose result has not been handled yet.
    in_flight: usize,
    /// Answers-for-agents sent to the mailbox to run as soon as it gets to
    /// them.
    due_now: HashSet<(CommitteeId, Seq, SenatorName)>,
    /// What does work runs, and what opens their PRs.
    runner: Option<Arc<dyn Runner>>,
    forge: Arc<dyn Forge>,
    work: Option<WorkConfig>,
    run_permits: Arc<Semaphore>,
    /// Work runs in flight in this process, by (committee, root).
    runs: HashMap<(CommitteeId, Seq), work::Run>,
    /// Their tasks; aborted with the engine like the Brain calls.
    running: JoinSet<()>,
    /// The built-in session's configuration, re-read before it acts on a
    /// committee (`engine::desk`).
    secretary: Arc<SecretaryConfigs>,
    /// The configuration the fields above were last set from.
    applied: Option<Arc<Effective>>,
    /// The built-in session's senator name: what it signs as
    /// [`meta::ACTED_BY`], and what an office or a secretary relation names.
    me: SenatorName,
    /// The child Senates registered here, as the hub said last.
    children: Vec<senate_core::ChildSenate>,
    /// Every committee's aliases here, as the hub said last: how people
    /// are shown an agent.
    aliases: senate_core::alias::CommitteeAliases,
    /// The configuration errors the Speaker was last told of (`None`: none
    /// outstanding).
    told_problems: Option<String>,
}

impl Engine {
    pub(crate) fn new(
        args: SpeakerArgs,
        epoch: Option<String>,
        roster: Roster,
        myself: ActorRef<SpeakerMsg>,
    ) -> Self {
        let SpeakerArgs {
            hub,
            surface,
            brain,
            config,
            runner,
            forge,
            secretary,
            ..
        } = args;
        let me = secretary.name().clone();
        let owned = config.clone();
        let config = &config;
        let max_runs = config
            .work
            .as_ref()
            .map_or(1, |w| w.max_concurrent_runs.max(1));
        Self {
            hub,
            surface,
            brain,
            roster,
            config: owned,
            choices: Vec::new(),
            fallback_after: config.answer_fallback(),
            confirmation_ttl: config.confirmation_ttl(),
            top_debounce: config.brief_top_debounce(),
            brain_timeout: config.brain_deadline() + BRAIN_SLACK,
            triage_deadline: config.triage_deadline(),
            card_debounce: config.card_debounce(),
            takeover_stale: config.takeover_stale(),
            cards_unsupported: AtomicBool::new(false),
            epoch,
            myself,
            committees: HashMap::new(),
            pending: Vec::new(),
            mode: config.mode,
            agent_max_actions: config.agent_max_actions,
            conversations: Vec::new(),
            agent_progress: HashMap::new(),
            preparing: None,
            acting: None,
            deferring: false,
            agent_replies: HashMap::new(),
            permits: Arc::new(Semaphore::new(BRAIN_CONCURRENCY)),
            thinking: JoinSet::new(),
            in_flight: 0,
            due_now: HashSet::new(),
            runner,
            forge: forge.unwrap_or_else(|| Arc::new(NoForge)),
            work: config.work.clone(),
            run_permits: Arc::new(Semaphore::new(max_runs)),
            runs: HashMap::new(),
            running: JoinSet::new(),
            secretary,
            applied: None,
            me,
            children: Vec::new(),
            aliases: Default::default(),
            told_problems: None,
        }
    }

    /// Stop every fallback timer and Brain call (the actor is going away).
    pub(crate) fn shutdown(&mut self) {
        for state in self.committees.values_mut() {
            for open in state.open.values_mut() {
                if let Some(timer) = open.timer.take() {
                    timer.abort();
                }
            }
            if let Some(timer) = state.top.timer.take() {
                timer.abort();
            }
            for (_, timer) in state.cards.drain() {
                timer.abort();
            }
            state.discussion.shutdown();
            state.bursts.shutdown();
            for timer in [state.pinned.timer.take(), state.pinned.midnight.take()]
                .into_iter()
                .flatten()
            {
                timer.abort();
            }
        }
        self.thinking.abort_all();
        for run in self.runs.values_mut() {
            run.abort();
        }
        self.running.abort_all();
    }

    /// Forget finished Brain tasks (their results travel as messages).
    pub(crate) fn reap(&mut self) {
        while self.thinking.try_join_next().is_some() {}
        while self.running.try_join_next().is_some() {}
    }

    /// No Brain result, no immediate fallback and no work run is
    /// outstanding.
    pub(crate) fn idle(&self) -> bool {
        self.in_flight == 0 && self.due_now.is_empty() && self.runs.is_empty()
    }

    // ----- driving -------------------------------------------------------

    /// Catch every committee up from its cursor.
    pub(crate) async fn resync_all(&mut self) {
        match self.hub.committees().await {
            Ok(committees) => {
                for committee in committees {
                    self.catch_up(&committee).await;
                }
            }
            Err(error) => {
                warn!(%error, "cannot list committees; retrying");
                self.myself.send_after(STALL_RETRY, || SpeakerMsg::Resync);
            }
        }
    }

    /// A live event off the subscription.
    pub(crate) async fn on_event(&mut self, event: Event) {
        if !self.ensure_loaded(&event.committee).await {
            return;
        }
        let Some(state) = self.committees.get(&event.committee) else {
            return;
        };
        if state.paused() || event.seq <= state.cursor {
            return;
        }
        if event.seq == state.cursor.next() {
            self.step(event).await;
        } else {
            // A gap (lag, or events committed before we subscribed).
            let committee = event.committee.clone();
            self.catch_up(&committee).await;
        }
    }

    /// The scheduled retry of a stalled (or unloadable) committee.
    pub(crate) async fn retry(&mut self, committee: &CommitteeId) {
        if let Some(state) = self.committees.get_mut(committee) {
            state.stalled = false;
        }
        self.catch_up(committee).await;
    }

    async fn catch_up(&mut self, committee: &CommitteeId) {
        if !self.ensure_loaded(committee).await {
            return;
        }
        loop {
            let Some(state) = self.committees.get(committee) else {
                return;
            };
            if state.paused() {
                return;
            }
            let page = match self.hub.read(committee, state.cursor, PAGE).await {
                Ok(page) => page,
                Err(error) => {
                    self.stall(committee, &Stall::Hub(error));
                    return;
                }
            };
            if page.is_empty() {
                return;
            }
            for event in page {
                if !self.step(event).await {
                    return;
                }
            }
        }
    }

    /// Process the event right after the cursor. False when the committee
    /// stopped at it (a stall, or a Brain call to wait for).
    async fn step(&mut self, event: Event) -> bool {
        self.use_config(Some(&event.committee));
        self.sync_roster().await;
        let platform = self.surface.platform().to_owned();
        let committee = event.committee.clone();
        {
            let Some(state) = self.committees.get_mut(&committee) else {
                return false;
            };
            if event.seq <= state.cursor {
                return true;
            }
            state.observe(&platform, &event);
        }
        let flow = match self.ensure_epoch().await {
            Ok(()) => self.act(&event).await,
            Err(stall) => Err(stall),
        };
        match flow {
            Ok(Flow::Done) => {
                if let Some(state) = self.committees.get_mut(&committee) {
                    state.cursor = event.seq;
                    state.decisions.remove(&event.seq);
                    state.triage.remove(&event.seq);
                    state.crossings.remove(&event.seq);
                    state.offline.remove(&event.seq);
                    state.picks.retain(|(seq, _, _), _| *seq != event.seq);
                    state.seat_picks.retain(|(seq, _, _), _| *seq != event.seq);
                    self.agent_progress.remove(&(committee.clone(), event.seq));
                }
                if memory::is_discussion(&event) {
                    self.arm_quiet(&committee);
                }
                // Simulated discussion too, in bursts of its own.
                self.arm_agreement(&committee, &event);
                if let Err(error) = self.hub.set_cursor(&committee, CURSOR, event.seq).await {
                    // Harmless: a restart reprocesses idempotently.
                    warn!(%committee, %error, "cannot persist the speaker cursor");
                }
                true
            }
            Ok(Flow::Await) => {
                if let Some(state) = self.committees.get_mut(&committee) {
                    state.awaiting = Some(event.seq);
                }
                false
            }
            Err(stall) => {
                self.stall(&committee, &stall);
                false
            }
        }
    }

    fn stall(&mut self, committee: &CommitteeId, stall: &Stall) {
        warn!(%committee, error = %stall, "speaker paused on this committee; retrying shortly");
        if let Some(state) = self.committees.get_mut(committee) {
            if state.stalled {
                return;
            }
            state.stalled = true;
        }
        let committee = committee.clone();
        self.myself
            .send_after(STALL_RETRY, move || SpeakerMsg::Retry(committee));
    }

    /// The epoch, read once; an unreadable one is a stall.
    async fn ensure_epoch(&mut self) -> Result<(), Stall> {
        if self.epoch.is_none() {
            self.epoch = Some(self.hub.epoch().await?);
        }
        Ok(())
    }

    /// The language `committee`'s people read.
    fn locale(&self, committee: &CommitteeId) -> Locale {
        self.secretary
            .effective(Some(committee))
            .speaker
            .locale_of(committee)
    }

    /// Read the roster again: agents are seated as they connect. When the
    /// hub cannot say, the last snapshot stands.
    async fn sync_roster(&mut self) {
        match self.hub.roster().await {
            Ok(roster) => self.roster = roster,
            Err(error) => debug!(%error, "cannot read the roster; using the last one"),
        }
        match self.hub.children().await {
            Ok(children) => self.children = children,
            Err(error) => debug!(%error, "cannot read the child Senates; using the last list"),
        }
        match self.hub.aliases().await {
            Ok(aliases) => self.aliases = aliases,
            Err(error) => debug!(%error, "cannot read the aliases; using the last ones"),
        }
    }

    /// How people are shown a seat, from what the hub said last.
    fn seat_names(&self) -> render::SeatNames<'_> {
        render::SeatNames {
            aliases: &self.aliases,
            children: &self.children,
        }
    }

    /// A person on the roster: someone who may change state through the
    /// Speaker. A guest (`guest::…`) may ask and be answered, nothing more;
    /// so may a `senator::` address the roster does not list, which the
    /// Speaker cannot vouch for either.
    fn is_member(&self, who: &Address) -> bool {
        who.may_change_state() && who.as_senator().is_some_and(|p| self.roster.is_person(p))
    }

    /// What people call a person: the first of their names on the roster,
    /// else their handle.
    fn display_name(&self, person: &Name) -> String {
        self.roster
            .principal(person)
            .and_then(|s| s.names.first().cloned())
            .unwrap_or_else(|| person.to_string())
    }

    /// The person behind `who`, if it is one on the roster.
    fn person_of(&self, who: &Address) -> Option<Name> {
        self.is_member(who)
            .then(|| who.as_senator().map(|p| p.name().clone()))
            .flatten()
    }

    /// The person behind `senator`: the person themselves, or the person
    /// an agent's seat records as answering for it.
    fn person_behind(&self, senator: &SenatorName) -> Option<Name> {
        if self.roster.is_person(senator) {
            Some(senator.name().clone())
        } else {
            self.roster.owner_of(senator).cloned()
        }
    }

    fn op_id(&self, committee: &CommitteeId, seq: Seq, target: &str) -> Result<String, Stall> {
        let epoch = self.epoch.as_deref().ok_or_else(|| {
            Stall::Hub(HubError::Unavailable(
                "the log's epoch is not known yet".into(),
            ))
        })?;
        Ok(operation_id(epoch, committee, seq, target))
    }

    /// [`op_id`](Self::op_id) for an update that can repeat at `seq` with
    /// other content (a card, an announcement): `payload` is part of it.
    fn content_op_id<T: serde::Serialize>(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        target: &str,
        payload: &T,
    ) -> Result<String, Stall> {
        let epoch = self.epoch.as_deref().ok_or_else(|| {
            Stall::Hub(HubError::Unavailable(
                "the log's epoch is not known yet".into(),
            ))
        })?;
        Ok(content_operation_id(epoch, committee, seq, target, payload))
    }

    /// Load a committee the first time it is seen: its cursor, then the state
    /// the log up to that cursor implies.
    async fn ensure_loaded(&mut self, committee: &CommitteeId) -> bool {
        if self.committees.contains_key(committee) {
            return true;
        }
        match self.load(committee).await {
            Ok(state) => {
                let stale = state.top_stale();
                self.committees.insert(committee.clone(), state);
                self.rearm(committee);
                self.arm_digests(committee);
                self.rearm_cards(committee).await;
                self.interrupt_runs(committee).await;
                if stale {
                    // A decision before the cursor never got its top (a
                    // crash, or the model was down): one refresh, not one
                    // per decision replayed.
                    self.schedule_top(committee);
                }
                true
            }
            Err(error) => {
                warn!(%committee, %error, "cannot load committee state; retrying");
                let committee = committee.clone();
                self.myself
                    .send_after(STALL_RETRY, move || SpeakerMsg::Retry(committee));
                false
            }
        }
    }

    async fn load(&self, committee: &CommitteeId) -> Result<CommitteeState, HubError> {
        let platform = self.surface.platform().to_owned();
        let cursor = self.hub.cursor(committee, CURSOR).await?;
        let mut state = CommitteeState {
            locale: self.locale(committee),
            cursor,
            board: TaskBoard::new(self.roster.people()),
            people: self.roster.people(),
            me: Some(self.me.clone()),
            ..CommitteeState::default()
        };
        let mut after = Seq::ZERO;
        while after < cursor {
            let page = self.hub.read(committee, after, PAGE).await?;
            let Some(last) = page.last() else {
                break;
            };
            after = last.seq;
            for event in page.iter().filter(|e| e.seq <= cursor) {
                state.observe(&platform, event);
            }
        }
        Ok(state)
    }

    /// Arm a timer for every open question that has none, from its `ts`.
    fn rearm(&mut self, committee: &CommitteeId) {
        let keys: Vec<(Seq, SenatorName, DateTime<Utc>)> = self
            .committees
            .get(committee)
            .map(|state| {
                state
                    .open
                    .iter()
                    .filter(|(_, open)| open.timer.is_none())
                    .map(|((root, agent), open)| (*root, agent.clone(), open.ts))
                    .collect()
            })
            .unwrap_or_default();
        for (root, agent, ts) in keys {
            let delay = self.remaining(ts);
            self.arm(committee, root, agent, delay);
        }
    }

    fn remaining(&self, asked: DateTime<Utc>) -> Duration {
        let elapsed = (Utc::now() - asked).to_std().unwrap_or(Duration::ZERO);
        self.fallback_after.saturating_sub(elapsed)
    }

    fn arm(&mut self, committee: &CommitteeId, root: Seq, agent: SenatorName, delay: Duration) {
        if delay.is_zero() {
            self.arm_now(committee, root, agent);
            return;
        }
        let message = (committee.clone(), root, agent.clone());
        let timer = self.myself.send_after(delay, move || {
            let (committee, root, agent) = message;
            SpeakerMsg::FallbackDue {
                committee,
                root,
                agent,
            }
        });
        let previous = self
            .committees
            .get_mut(committee)
            .and_then(|state| state.open.get_mut(&(root, agent)))
            .and_then(|open| open.timer.replace(timer));
        if let Some(previous) = previous {
            previous.abort();
        }
    }

    /// The fallback is due now: queue it behind whatever the mailbox holds,
    /// so the committee is caught up before it runs.
    fn arm_now(&mut self, committee: &CommitteeId, root: Seq, agent: SenatorName) {
        if let Some(previous) = self
            .committees
            .get_mut(committee)
            .and_then(|state| state.open.get_mut(&(root, agent.clone())))
            .and_then(|open| open.timer.take())
        {
            previous.abort();
        }
        let key = (committee.clone(), root, agent.clone());
        if !self.due_now.insert(key.clone()) {
            return; // already queued
        }
        let due = SpeakerMsg::FallbackDue {
            committee: committee.clone(),
            root,
            agent,
        };
        if self.myself.send_message(due).is_err() {
            self.due_now.remove(&key);
        }
    }

    // ----- Brain calls ----------------------------------------------------

    /// Run one Brain call off the mailbox; `deliver` shapes its result into
    /// the message that comes back.
    fn think<T, F>(
        &mut self,
        call: F,
        deliver: impl FnOnce(Result<T, BrainError>) -> ThoughtKind + Send + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T, BrainError>> + Send + 'static,
    {
        let delivery = Delivery {
            myself: self.myself.clone(),
            deliver: Some(Box::new(deliver)),
        };
        let permits = Arc::clone(&self.permits);
        let limit = self.brain_timeout;
        self.in_flight += 1;
        self.thinking.spawn(async move {
            let result = match permits.acquire_owned().await {
                Ok(_permit) => match tokio::time::timeout(limit, call).await {
                    Ok(result) => result,
                    Err(_) => Err(BrainError::Unavailable(format!(
                        "no result within {} s",
                        limit.as_secs()
                    ))),
                },
                Err(_) => Err(BrainError::Unavailable("the speaker is stopping".into())),
            };
            delivery.send(result);
        });
    }

    /// [`think`](Self::think) bounded by `limit` from the moment it is
    /// asked, waiting for a free slot included: triage holds a committee's
    /// `seq` order, so it may not queue behind slow calls past its deadline.
    fn think_within<T, F>(
        &mut self,
        limit: Duration,
        call: F,
        deliver: impl FnOnce(Result<T, BrainError>) -> ThoughtKind + Send + 'static,
    ) where
        T: Send + 'static,
        F: Future<Output = Result<T, BrainError>> + Send + 'static,
    {
        let delivery = Delivery {
            myself: self.myself.clone(),
            deliver: Some(Box::new(deliver)),
        };
        let permits = Arc::clone(&self.permits);
        self.in_flight += 1;
        self.thinking.spawn(async move {
            let bounded = async move {
                match permits.acquire_owned().await {
                    Ok(_permit) => call.await,
                    Err(_) => Err(BrainError::Unavailable("the speaker is stopping".into())),
                }
            };
            let result = tokio::time::timeout(limit, bounded)
                .await
                .unwrap_or_else(|_| {
                    Err(BrainError::Unavailable(format!(
                        "no result within {} s",
                        limit.as_secs()
                    )))
                });
            delivery.send(result);
        });
    }

    pub(crate) async fn on_thought(&mut self, thought: ThoughtKind) {
        self.in_flight = self.in_flight.saturating_sub(1);
        match thought {
            ThoughtKind::Interpreted {
                committee,
                seq,
                intent,
            } => self.on_interpreted(committee, seq, intent).await,
            ThoughtKind::Conversed {
                committee,
                seq,
                turn,
            } => {
                let l = self.locale(&committee);
                if let Some(state) = self.committees.get_mut(&committee) {
                    state.decisions.insert(seq, agent::decision(turn, l));
                    state.awaiting = None;
                }
                self.catch_up(&committee).await;
            }
            ThoughtKind::UnboundConversed { mention, turn } => {
                if let Err(stall) = self
                    .run_unbound_agent(&mention, agent::decision(turn, self.config.locale))
                    .await
                {
                    warn!(error = %stall, "could not answer an unbound agent conversation");
                }
            }
            ThoughtKind::UnboundInterpreted { mention, intent } => {
                if let Err(stall) = self
                    .run_unbound(&mention, decision_of(intent, self.config.locale))
                    .await
                {
                    warn!(error = %stall, "could not answer a mention in an unbound group");
                }
            }
            ThoughtKind::Answered {
                committee,
                root,
                agent,
                question,
                asker,
                takeover,
                secretary,
                answer,
            } => {
                let draft = Drafted {
                    committee,
                    root,
                    agent,
                    question,
                    asker,
                };
                if secretary {
                    self.on_secretary_answered(draft, answer).await;
                } else if takeover {
                    self.on_takeover_answered(draft, answer).await;
                } else {
                    self.on_answered(draft, answer.map(|a| a.text)).await;
                }
            }
            ThoughtKind::Crossed {
                committee,
                seq,
                crossing,
            } => self.on_crossed(committee, seq, crossing).await,
            ThoughtKind::OfflineDecided {
                committee,
                seq,
                decision,
            } => self.on_offline_decided(committee, seq, decision).await,
            ThoughtKind::TopWritten {
                committee,
                basis,
                top,
            } => self.on_top_written(committee, basis, top).await,
            ThoughtKind::Triaged {
                committee,
                seq,
                triage,
            } => self.on_triaged(committee, seq, triage).await,
            ThoughtKind::Digested {
                committee,
                trigger,
                from,
                through,
                shown,
                out,
            } => {
                self.on_digested(committee, trigger, (from, through), shown, out)
                    .await;
            }
            ThoughtKind::Spotted {
                committee,
                burst,
                agreement,
            } => self.on_spotted(committee, burst, agreement).await,
        }
    }

    async fn on_interpreted(
        &mut self,
        committee: CommitteeId,
        seq: Seq,
        intent: Result<Intent, BrainError>,
    ) {
        let decision = decision_of(intent, self.locale(&committee));
        if let Some(state) = self.committees.get_mut(&committee) {
            state.decisions.insert(seq, decision);
            if state.awaiting == Some(seq) {
                state.awaiting = None;
            }
        }
        self.catch_up(&committee).await;
    }

    // ----- acting on one event --------------------------------------------

    async fn act(&mut self, event: &Event) -> Result<Flow, Stall> {
        if event.is_chair_discussion() {
            return Ok(Flow::Done);
        }
        if desk::is_crossing(event) {
            return self.on_crossing(event).await;
        }
        let flow = match event.kind {
            Kind::Chat => self.on_chat(event).await?,
            Kind::Binding => {
                self.on_binding(event).await?;
                Flow::Done
            }
            Kind::Decision => {
                self.mirror(event).await?;
                self.schedule_top(&event.committee);
                Flow::Done
            }
            Kind::Status => {
                self.on_status(event).await?;
                Flow::Done
            }
            Kind::Presence => {
                self.on_presence(event).await?;
                Flow::Done
            }
            Kind::Checkpoint => {
                self.on_checkpoint(event).await?;
                Flow::Done
            }
            Kind::Note if senate_core::is_work_root(event) => {
                self.on_work_root(event).await?;
                Flow::Done
            }
            _ if self.is_task_root(event) => self.on_task_root(event).await?,
            _ => {
                self.refuse_agent_work(event).await?;
                self.mirror(event).await?;
                self.fallback_on_arrival(event).await;
                Flow::Done
            }
        };
        // Put to a child Senate that is offline: said, or answered for it.
        let flow = match flow {
            Flow::Done => self.on_child_offline(event).await?,
            flow => flow,
        };
        // Put to a seat that is gone since: closed, the asker told.
        if matches!(flow, Flow::Done) {
            self.close_gone_seats(&event.committee).await?;
        }
        if matches!(flow, Flow::Done) {
            self.after(event).await;
        }
        Ok(flow)
    }

    /// Whether `event` opens a task thread (a root `question` or `request`
    /// to an agent; see [`TaskBoard`]).
    fn is_task_root(&self, event: &Event) -> bool {
        event.thread.is_none()
            && self
                .committees
                .get(&event.committee)
                .is_some_and(|state| state.board.get(event.seq).is_some())
    }

    /// Post a log event to the platform per the mirroring table, once per
    /// target.
    async fn mirror(&mut self, event: &Event) -> Result<(), Stall> {
        if event.mirror.is_some() {
            return Ok(()); // it came from the platform; it is already there
        }
        let platform = self.surface.platform().to_owned();
        for plan in self.mirror_plans(event, &platform).await? {
            self.run_plan(&event.committee, event.seq, plan).await?;
        }
        Ok(())
    }

    /// Post one copy of log event `seq` unless it is already there, and
    /// record it. A card falls back to its text where cards are unsupported;
    /// a card that landed is also recorded as `<target>#card`, which is what
    /// later updates look for.
    async fn run_plan(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        plan: MirrorPlan,
    ) -> Result<(), Stall> {
        if self.has_copy(committee, seq, &plan.target).await? {
            return Ok(());
        }
        let platform = self.surface.platform().to_owned();
        let operation = self.op_id(committee, seq, &plan.target)?;
        let mut is_card = false;
        let posted = match &plan.card {
            Some(card) => match self
                .call(self.surface.post_card(plan.to.clone(), card, &operation))
                .await?
            {
                Outcome::Done(posted) => {
                    is_card = true;
                    Some(posted)
                }
                Outcome::Unsupported => {
                    self.cards_unsupported_once();
                    self.post(plan.to, &plan.text, &operation).await?
                }
                Outcome::Refused => None,
            },
            None => self.post(plan.to, &plan.text, &operation).await?,
        };
        let Some(posted) = posted else {
            return Ok(());
        };
        let message_ref = plan.record_as.unwrap_or(posted.message_ref);
        let mirror = MirrorRef {
            committee: committee.clone(),
            seq,
            platform,
            target: plan.target.clone(),
            message_ref,
            thread_ref: posted.thread_ref,
        };
        if is_card {
            self.hub
                .put_mirror(MirrorRef {
                    target: card_target(&plan.target),
                    ..mirror.clone()
                })
                .await?;
        }
        self.hub.put_mirror(mirror).await?;
        Ok(())
    }

    async fn mirror_plans(&self, event: &Event, platform: &str) -> Result<Vec<MirrorPlan>, Stall> {
        let l = self.locale(&event.committee);
        if is_brief_top(event)
            || event.meta.contains_key(meta::DIGEST)
            || event.meta.contains_key(meta::MEMORY)
        {
            // The brief's own header, a digest, the committee's memory:
            // never chat.
            return Ok(Vec::new());
        }
        if event.kind == Kind::Decision && event.from == Address::Speaker {
            // Recorded for someone who said it on the platform: the only copy
            // is the acknowledgement in their thread.
            return Ok(self
                .decision_ack_plan(event, platform)
                .await?
                .into_iter()
                .collect());
        }
        let state = self.committees.get(&event.committee);
        let agents = state
            .and_then(CommitteeState::agents_group)
            .map(|g| g.group.group_id.clone());
        if relayed_from(event).is_some() {
            let mut plans: Vec<MirrorPlan> = self
                .relay_ack_plan(event, platform)
                .await?
                .into_iter()
                .collect();
            if let Some(agents) = agents
                && self.is_task_root(event)
            {
                // Its task thread in the Agent group too.
                plans.extend(self.card_plan(event, &agents));
            }
            return Ok(plans);
        }
        if let Some(root) = event.thread {
            return self.reply_plans(event, root, platform).await;
        }
        if agreement::is_discussion_proposal(event) {
            // Asked in the group it was agreed in, as a new root: a reply in
            // its thread is the answer.
            let Some(source) = event.source.as_ref().filter(|s| s.platform == platform) else {
                return Ok(Vec::new());
            };
            return Ok(vec![MirrorPlan {
                to: PostTarget::NewRoot {
                    group_id: source.group_id.clone(),
                },
                text: render::discussion_proposal(l, event),
                target: source.group_id.clone(),
                record_as: None,
                card: None,
            }]);
        }
        if event.from != Address::Speaker
            && matches!(
                event.kind,
                Kind::Question | Kind::Note | Kind::Decision | Kind::Request
            )
        {
            let Some(group) = state.and_then(CommitteeState::mirror_group) else {
                return Ok(Vec::new());
            };
            if self.is_task_root(event) {
                return Ok(self.card_plan(event, &group.group_id).into_iter().collect());
            }
            // A proposal's root is what a confirming reply maps back to.
            let text = if is_proposal(event, &self.roster.people()) {
                render::proposal_root(l, self.seat_names(), event)
            } else {
                render::root_mirror(l, self.seat_names(), event)
            };
            return Ok(vec![MirrorPlan {
                to: PostTarget::NewRoot {
                    group_id: group.group_id.clone(),
                },
                text,
                target: group.group_id.clone(),
                record_as: None,
                card: None,
            }]);
        }
        Ok(Vec::new())
    }

    /// A task root as a card in `group`, with its plain mirror as the
    /// fallback text.
    fn card_plan(&self, event: &Event, group: &str) -> Option<MirrorPlan> {
        let l = self.locale(&event.committee);
        let state = self.committees.get(&event.committee)?;
        let thread = state.board.get(event.seq)?;
        let verdict = state.triage.get(&event.seq).map(|t| t.verdict);
        Some(MirrorPlan {
            to: PostTarget::NewRoot {
                group_id: group.to_owned(),
            },
            text: render::root_mirror(l, self.seat_names(), event),
            target: group.to_owned(),
            record_as: None,
            card: Some(render::task_card(l, thread, verdict, None)),
        })
    }

    /// A reply in a thread, to each copy of its root that should see it.
    ///
    /// Never as text: a `status` (the card shows it), the Speaker's own notes
    /// (triage verdicts and acknowledgements it already said; not the note
    /// that a child Senate is offline, nor that a question's seat is gone,
    /// which nothing else says), and a
    /// person's approval the Speaker wrote for them. In a task thread of a
    /// committee with an Agent group, the Agent group's copy gets answers
    /// (everything at output level "everything") and a people group's copy
    /// gets answers and the note that the seat is gone only, signed; otherwise the plain rule: the reply goes
    /// under the root's copy.
    async fn reply_plans(
        &self,
        event: &Event,
        root: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorPlan>, Stall> {
        let l = self.locale(&event.committee);
        let approval = self.roster.is_person_address(&event.from)
            && event.meta.contains_key(meta::RELAYED_FROM);
        // What the office says of an offline child Senate is news to the
        // one who asked, not something already said in chat.
        let own_note = event.from == Address::Speaker
            && event.kind == Kind::Note
            && !event.meta.contains_key(meta::CHILD_OFFLINE)
            && !senate_core::brief::is_seat_removed(event);
        if event.kind == Kind::Status || own_note || approval {
            return Ok(Vec::new());
        }
        let covering = self.covering_for(event, root).await?;
        let state = self.committees.get(&event.committee);
        let task = state.is_some_and(|s| s.board.get(root).is_some());
        let agents = state
            .and_then(CommitteeState::agents_group)
            .map(|g| g.group.group_id.clone());
        if let (true, Some(agents)) = (task, agents) {
            let level = state.map(CommitteeState::output_level).unwrap_or_default();
            let mut plans = Vec::new();
            for copy in self.plain_copies(&event.committee, root).await? {
                let group = group_of_target(&copy.target).to_owned();
                let text = if group == agents {
                    if event.kind != Kind::Answer && level != OutputLevel::All {
                        continue;
                    }
                    match &covering {
                        Some(old) => {
                            render::thread_mirror_covering(l, self.seat_names(), event, old)
                        }
                        None => render::thread_mirror(l, self.seat_names(), event),
                    }
                } else if event.kind == Kind::Answer || senate_core::brief::is_seat_removed(event) {
                    match &covering {
                        Some(old) => render::signed_covering(l, self.seat_names(), event, old),
                        None => render::signed(l, self.seat_names(), event),
                    }
                } else {
                    continue;
                };
                plans.push(MirrorPlan {
                    to: PostTarget::Reply {
                        message_ref: copy.message_ref,
                        in_thread: true,
                    },
                    text,
                    target: group,
                    record_as: None,
                    card: None,
                });
            }
            return Ok(plans);
        }
        Ok(self
            .thread_target(&event.committee, root, platform)
            .await?
            .map(|(message_ref, group)| MirrorPlan {
                to: PostTarget::Reply {
                    message_ref,
                    in_thread: true,
                },
                text: match &covering {
                    Some(old) => render::thread_mirror_covering(l, self.seat_names(), event, old),
                    None => render::thread_mirror(l, self.seat_names(), event),
                },
                target: group,
                record_as: None,
                card: None,
            })
            .into_iter()
            .collect())
    }

    /// A relay's acknowledgement in the human's thread. The human's message
    /// is recorded as the relay's copy, so replies there map to the relay's
    /// log thread.
    async fn relay_ack_plan(
        &self,
        event: &Event,
        platform: &str,
    ) -> Result<Option<MirrorPlan>, Stall> {
        let l = self.locale(&event.committee);
        if let Some(chat_seq) = relayed_from(event) {
            // A relay: acknowledge in the human's thread, and record the
            // human's message as the relay's copy so replies there map to
            // the relay's log thread.
            let Some(chat) = self.hub.get(&event.committee, chat_seq).await? else {
                return Ok(None);
            };
            let group = chat
                .source
                .as_ref()
                .or(event.source.as_ref())
                .map(|source| source.group_id.clone());
            let (Some(mirror), Some(group)) =
                (chat.mirror.filter(|m| m.platform == platform), group)
            else {
                return Ok(None);
            };
            return Ok(Some(MirrorPlan {
                to: PostTarget::Reply {
                    message_ref: mirror.message_ref.clone(),
                    in_thread: true,
                },
                text: render::relay_ack_for(l, event, &self.roster, self.seat_names()),
                target: group,
                record_as: Some(mirror.message_ref),
                card: None,
            }));
        }
        Ok(None)
    }

    /// `已记录决策（seq N）` in the speaker's thread: under the chat a
    /// Speaker-recorded decision came from, or under the proposal's root for
    /// a confirmed proposal; nothing when it came from no chat.
    async fn decision_ack_plan(
        &self,
        event: &Event,
        platform: &str,
    ) -> Result<Option<MirrorPlan>, Stall> {
        let l = self.locale(&event.committee);
        let Some(chat_seq) = event
            .meta
            .get(meta::RELAYED_FROM)
            .and_then(Value::as_u64)
            .map(Seq)
        else {
            return Ok(None);
        };
        if let Some(root) = event.thread {
            // A confirmed proposal: under the proposal's root, where the
            // yes was said.
            return Ok(self
                .thread_target(&event.committee, root, platform)
                .await?
                .map(|(message_ref, group)| MirrorPlan {
                    to: PostTarget::Reply {
                        message_ref,
                        in_thread: true,
                    },
                    text: render::decision_ack(l, event.seq),
                    target: group,
                    record_as: None,
                    card: None,
                }));
        }
        let Some(chat) = self.hub.get(&event.committee, chat_seq).await? else {
            return Ok(None);
        };
        let (Some(mirror), Some(source)) = (
            chat.mirror.filter(|m| m.platform == platform),
            chat.source.or_else(|| event.source.clone()),
        ) else {
            return Ok(None);
        };
        Ok(Some(MirrorPlan {
            to: PostTarget::Reply {
                message_ref: mirror.message_ref,
                in_thread: true,
            },
            text: render::decision_ack(l, event.seq),
            target: source.group_id,
            record_as: None,
            card: None,
        }))
    }

    /// The copies of `seq` that are messages people see and reply under:
    /// not the `#`-suffixed markers (a card, a pinned card, an extra ack).
    async fn plain_copies(
        &self,
        committee: &CommitteeId,
        seq: Seq,
    ) -> Result<Vec<MirrorRef>, Stall> {
        Ok(self
            .hub
            .mirrors_of_seq(committee, seq, self.surface.platform())
            .await?
            .into_iter()
            .filter(|copy| !copy.target.contains('#'))
            .collect())
    }

    /// The platform message a thread's replies go under, and the group it
    /// is in: the root's copy (the one in the Agent group when it has
    /// several), or the root itself when it came from the platform.
    async fn thread_target(
        &self,
        committee: &CommitteeId,
        root: Seq,
        platform: &str,
    ) -> Result<Option<(String, String)>, Stall> {
        let agents = self
            .committees
            .get(committee)
            .and_then(CommitteeState::agents_group)
            .map(|g| g.group.group_id.clone());
        let copies = self.plain_copies(committee, root).await?;
        let preferred = copies
            .iter()
            .position(|c| Some(group_of_target(&c.target)) == agents.as_deref())
            .unwrap_or(0);
        if let Some(copy) = copies.into_iter().nth(preferred) {
            let group = group_of_target(&copy.target).to_owned();
            return Ok(Some((copy.message_ref, group)));
        }
        Ok(self.hub.get(committee, root).await?.and_then(|event| {
            let group = event.source?.group_id;
            let mirror = event.mirror.filter(|m| m.platform == platform)?;
            Some((mirror.message_ref, group))
        }))
    }

    /// Whether event `seq` already has its copy in `target`.
    async fn has_copy(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        target: &str,
    ) -> Result<bool, Stall> {
        Ok(self
            .hub
            .mirrors_of_seq(committee, seq, self.surface.platform())
            .await?
            .iter()
            .any(|copy| copy.target == target))
    }

    /// A question to an agent that is away (`absent`, or already
    /// `taken_over`) is due at once, for a takeover; one to an agent only
    /// `disconnected` gets its answer window (it may be back within it).
    /// Otherwise (online, or no presence known: an agent that never
    /// connected has none) the plain rule: a live connection gets the window,
    /// none is due at once.
    /// "At once" is only from the mailbox: the committee may be catching up,
    /// and the real answer may be a later event not processed yet.
    async fn fallback_on_arrival(&mut self, event: &Event) {
        if event.kind != Kind::Question {
            return;
        }
        let root = event.thread.unwrap_or(event.seq);
        for agent in question_targets(event, &self.roster.people()) {
            let watching_this = self
                .committees
                .get(&event.committee)
                .and_then(|state| state.open.get(&(root, agent.clone())))
                .is_some_and(|open| open.question == event.seq);
            if !watching_this {
                continue; // answered already, or an earlier question is watched
            }
            if let Some(owner) = self.cover_of(&event.committee, &agent) {
                // Reassigned: its new owner's bridge is pushed it.
                if self.hub.is_agent_connected(&event.committee, &owner).await {
                    let delay = self.remaining(event.ts);
                    self.arm(&event.committee, root, agent, delay);
                } else {
                    self.arm_now(&event.committee, root, agent);
                }
                continue;
            }
            let window = match self.presence_state(&event.committee, &agent).await {
                Ok(Some(PresenceState::Absent | PresenceState::TakenOver)) => false,
                Ok(Some(PresenceState::Disconnected)) => true,
                Ok(_) | Err(_) => self.hub.is_agent_connected(&event.committee, &agent).await,
            };
            if window {
                let delay = self.remaining(event.ts);
                self.arm(&event.committee, root, agent, delay);
            } else {
                self.arm_now(&event.committee, root, agent);
            }
        }
    }

    pub(crate) async fn on_fallback_due(
        &mut self,
        committee: CommitteeId,
        root: Seq,
        agent: SenatorName,
    ) {
        self.due_now
            .remove(&(committee.clone(), root, agent.clone()));
        // An answer may be committed but not yet processed; process it first.
        self.catch_up(&committee).await;
        let Some(state) = self.committees.get_mut(&committee) else {
            return;
        };
        let paused = state.paused();
        let Some(open) = state.open.get_mut(&(root, agent.clone())) else {
            return; // answered meanwhile
        };
        open.timer = None;
        if open.answering {
            return;
        }
        let question = open.question;
        if paused {
            // The answer may sit behind the event the committee stopped at.
            self.arm(&committee, root, agent, STALL_RETRY);
            return;
        }
        if let Some(owner) = self.cover_of(&committee, &agent) {
            // Reassigned: no answer for it while the new owner is there,
            // the plain one when it is not, never a takeover.
            if self.hub.is_agent_connected(&committee, &owner).await {
                if let Some(state) = self.committees.get_mut(&committee) {
                    state.open.remove(&(root, agent));
                }
            } else {
                self.draft_answer(committee, root, agent, question, None)
                    .await;
            }
            return;
        }
        match self.presence_state(&committee, &agent).await {
            Ok(Some(PresenceState::Absent | PresenceState::TakenOver)) => {
                self.take_over(committee, root, agent, question).await;
            }
            Ok(_) => {
                self.draft_answer(committee, root, agent, question, None)
                    .await;
            }
            Err(stall) => {
                warn!(%committee, error = %stall, "presence unavailable; answering for the agent deferred");
                self.arm(&committee, root, agent, STALL_RETRY);
            }
        }
    }

    /// Ask the Brain to answer for the agent `question` (from the checkpoint, in a
    /// takeover); the draft comes back as [`ThoughtKind::Answered`].
    async fn draft_answer(
        &mut self,
        committee: CommitteeId,
        root: Seq,
        agent: SenatorName,
        question: Seq,
        takeover: Option<Takeover>,
    ) {
        self.draft_answer_as(committee, root, agent, question, takeover, false)
            .await;
    }

    /// [`Engine::draft_answer`], as the agent's `secretary` or for the desk.
    pub(super) async fn draft_answer_as(
        &mut self,
        committee: CommitteeId,
        root: Seq,
        agent: SenatorName,
        question: Seq,
        takeover: Option<Takeover>,
        secretary: bool,
    ) {
        match self.question_for(&committee, question, &agent).await {
            Ok(Some(mut draft)) => {
                if let Some(open) = self
                    .committees
                    .get_mut(&committee)
                    .and_then(|state| state.open.get_mut(&(root, agent.clone())))
                {
                    open.answering = true;
                }
                let asker = draft
                    .event
                    .on_behalf_of
                    .clone()
                    .unwrap_or_else(|| draft.event.from.clone());
                let is_takeover = takeover.is_some();
                draft.takeover = takeover;
                let brain = Arc::clone(&self.brain);
                self.think(
                    async move { brain.answer_on_behalf(draft).await },
                    move |answer| ThoughtKind::Answered {
                        committee,
                        root,
                        agent,
                        question,
                        asker,
                        takeover: is_takeover,
                        secretary,
                        answer,
                    },
                );
            }
            Ok(None) => {
                if let Some(state) = self.committees.get_mut(&committee) {
                    state.open.remove(&(root, agent));
                }
            }
            Err(stall) => {
                warn!(%committee, error = %stall, "answering for the agent deferred");
                self.arm(&committee, root, agent, STALL_RETRY);
            }
        }
    }

    /// What the Brain needs to answer `question` for `agent`.
    async fn question_for(
        &self,
        committee: &CommitteeId,
        question: Seq,
        agent: &SenatorName,
    ) -> Result<Option<Question>, Stall> {
        let Some(event) = self.hub.get(committee, question).await? else {
            return Ok(None);
        };
        let context = self.context(committee, question.next()).await?;
        Ok(Some(Question {
            event,
            agent: agent.clone(),
            committee: committee.clone(),
            context,
            takeover: None,
            locale: self.locale(committee),
        }))
    }

    /// A drafted answer for an agent: append it unless the agent answered meanwhile. It is
    /// mirrored when the Speaker reaches it in the log.
    async fn on_answered(&mut self, draft: Drafted, answer: Result<String, BrainError>) {
        let l = self.locale(&draft.committee);
        let Drafted {
            committee,
            root,
            agent,
            question,
            asker,
        } = draft;
        let Some(open) = self
            .committees
            .get_mut(&committee)
            .and_then(|state| state.open.get_mut(&(root, agent.clone())))
        else {
            return; // the agent answered while the Brain was drafting
        };
        if open.question != question {
            return;
        }
        open.answering = false;
        let behalf = Address::senator(&agent);
        let (body, provisional) = match answer {
            Ok(text) if !text.trim().is_empty() => (text.trim().to_owned(), false),
            other => {
                if let Err(error) = other {
                    warn!(%committee, %error, "the brain could not answer on behalf");
                }
                // Say where the question is: with a connected agent that has
                // not answered yet, or waiting for one that is away.
                // Not an answer: it leaves the thread open (provisional).
                let text = if self.hub.is_agent_connected(&committee, &agent).await {
                    render::fallback_pending(l, &behalf)
                } else {
                    render::fallback_unavailable(l, &behalf)
                };
                (text, true)
            }
        };
        let mut answer = behalf_answer(&committee, root, &agent, question, asker, body);
        if provisional {
            answer = answer.with_meta(meta::PROVISIONAL, json!(true));
        }
        match self.append(answer).await {
            Ok(_) => {
                if let Some(open) = self
                    .committees
                    .get_mut(&committee)
                    .and_then(|state| state.open.remove(&(root, agent)))
                    && let Some(timer) = open.timer
                {
                    timer.abort();
                }
            }
            Err(stall) => {
                warn!(%committee, error = %stall, "answering for the agent deferred");
                self.arm(&committee, root, agent, STALL_RETRY);
            }
        }
    }

    // ----- the brief's top --------------------------------------------------

    /// A decision landed: rewrite the brief's top `brief_top_debounce_secs`
    /// from now, unless a rewrite is already due or being drafted. A burst of
    /// decisions (or a replay of old ones) makes one rewrite, not one each.
    fn schedule_top(&mut self, committee: &CommitteeId) {
        let delay = self.top_debounce;
        let Some(state) = self.committees.get_mut(committee) else {
            return;
        };
        if state.top.drafting {
            state.top.again = true;
            return;
        }
        if state.top.timer.is_some() {
            return;
        }
        let committee = committee.clone();
        state.top.timer = Some(
            self.myself
                .send_after(delay, move || SpeakerMsg::TopDue(committee)),
        );
    }

    pub(crate) async fn on_top_due(&mut self, committee: CommitteeId) {
        // Decisions may be committed but not yet processed; see them first.
        self.catch_up(&committee).await;
        let Some(state) = self.committees.get_mut(&committee) else {
            return;
        };
        state.top.timer = None;
        if state.top.drafting {
            state.top.again = true;
            return;
        }
        if !state.top_stale() {
            return; // a top written after the last decision (a replay)
        }
        if state.paused() {
            let committee = committee.clone();
            state.top.timer = Some(
                self.myself
                    .send_after(STALL_RETRY, move || SpeakerMsg::TopDue(committee)),
            );
            return;
        }
        let basis = state.last_decision;
        let brief = match self.hub.brief(&committee).await {
            Ok(brief) => brief,
            Err(error) => {
                warn!(%committee, %error, "cannot read the brief; the top waits for the next decision");
                return;
            }
        };
        if let Some(state) = self.committees.get_mut(&committee) {
            state.top.drafting = true;
            state.top.forced = false;
        }
        let brain = Arc::clone(&self.brain);
        let locale = self.locale(&committee);
        self.think(
            async move { brain.brief_top(&brief, locale).await },
            move |top| ThoughtKind::TopWritten {
                committee,
                basis,
                top,
            },
        );
    }

    /// A drafted top: append it as the Speaker's `brief_top` note. No model,
    /// or nothing usable, means no top this time; the brief stands without.
    async fn on_top_written(
        &mut self,
        committee: CommitteeId,
        basis: Seq,
        top: Result<String, BrainError>,
    ) {
        let again = match self.committees.get_mut(&committee) {
            Some(state) => {
                state.top.drafting = false;
                std::mem::take(&mut state.top.again)
            }
            None => return,
        };
        let body = match top {
            Ok(text) => render::top_lines(&text),
            Err(error) => {
                warn!(%committee, %error, "the brain could not write the brief's top; skipping it");
                String::new()
            }
        };
        if !body.is_empty() {
            let note = NewEvent::new(committee.clone(), Kind::Note, Address::Speaker, body)
                .with_id(derived_event_id(&[
                    "brief-top",
                    committee.as_str(),
                    &basis.to_string(),
                ]))
                .with_meta(meta::BRIEF_TOP, json!(true));
            if let Err(stall) = self.append(note).await {
                warn!(%committee, error = %stall, "cannot append the brief's top; skipping it");
            }
        }
        if again {
            // That draft predates a decision; this top will be newer than
            // the decision in the log, so the next one must not wait on it.
            if let Some(state) = self.committees.get_mut(&committee) {
                state.top.forced = true;
            }
            self.schedule_top(&committee);
        }
    }

    // ----- chat ------------------------------------------------------------

    async fn on_chat(&mut self, chat: &Event) -> Result<Flow, Stall> {
        // Only what was said on the platform is an instruction. The server
        // refuses chat appended by senators; this is the second lock.
        let Some(source) = chat.source.as_ref() else {
            return Ok(Flow::Done);
        };
        let group = group_ref(source);
        if let Some(index) = self.choice_for_chat(chat) {
            return self.answer_choice(index, chat).await;
        }
        if let Some(index) = self.pending_for_chat(chat) {
            self.resolve_for_chat(index, chat).await?;
            return Ok(Flow::Done);
        }
        if self.confirm_proposal(chat).await? {
            return Ok(Flow::Done);
        }
        if self.on_task_chat(chat).await? {
            return Ok(Flow::Done);
        }
        let continuing = self.mode == SpeakerMode::Agent && self.continuing_chat(chat);
        let decided = self
            .committees
            .get(&chat.committee)
            .is_some_and(|s| s.decisions.contains_key(&chat.seq));
        if !chat.is_directed_to(&Address::Speaker) && !continuing && !decided {
            return Ok(Flow::Done);
        }
        if self
            .has_copy(&chat.committee, chat.seq, &group.group_id)
            .await?
        {
            return Ok(Flow::Done); // answered before a restart
        }
        let cached = self
            .committees
            .get(&chat.committee)
            .and_then(|state| state.decisions.get(&chat.seq))
            .cloned();
        let l = self.locale(&chat.committee);
        let decision = match cached {
            Some(decision) => decision,
            None => match parse_command(&chat.body, l) {
                Some(parsed) => command_decision(parsed),
                None => {
                    let context = self.context(&chat.committee, chat.seq).await?;
                    let utterance = Utterance {
                        text: chat.body.clone(),
                        speaker: chat.from.clone(),
                        committee: Some(chat.committee.clone()),
                        group,
                        gone_relays: self.gone_relays(&context),
                        context,
                        roster: self.roster.clone(),
                        seats: self.committee_seats(&chat.committee).await,
                        locale: l,
                        work: self.does_work(),
                    };
                    if self.mode == SpeakerMode::Agent {
                        self.start_conversation(chat, utterance);
                        return Ok(Flow::Await);
                    }
                    let brain = Arc::clone(&self.brain);
                    let (committee, seq) = (chat.committee.clone(), chat.seq);
                    self.think(
                        async move { brain.interpret(utterance).await },
                        move |intent| ThoughtKind::Interpreted {
                            committee,
                            seq,
                            intent,
                        },
                    );
                    return Ok(Flow::Await);
                }
            },
        };
        self.run_bound(chat, decision).await
    }

    /// The committee a step of `chat` acts on: the chat's own, or, in an agent
    /// plan after a Bind, the one it binds to.
    fn acting_committee(&self, chat: &Event) -> CommitteeId {
        self.acting
            .clone()
            .unwrap_or_else(|| chat.committee.clone())
    }

    /// In an agent plan after a Bind, a read-only step waits for a yes with
    /// the rest of the plan instead of running now.
    fn defer(&mut self, action: PendingAction) -> bool {
        match &mut self.preparing {
            Some(actions) if self.deferring => {
                actions.push(action);
                true
            }
            _ => false,
        }
    }

    async fn run_bound(&mut self, chat: &Event, decision: Decision) -> Result<Flow, Stall> {
        let committee = self.acting_committee(chat);
        let l = self.locale(&chat.committee);
        let (intent, confirm) = match decision {
            Decision::Say(text) => return self.say(chat, &text).await,
            Decision::ModelDown(error) => {
                let text = render::model_unavailable(l, as_command(&chat.body, l).as_deref());
                let flow = self.say(chat, &text).await?;
                self.report_model_failure(chat, &error).await?;
                return Ok(flow);
            }
            Decision::Agent(turn) => return Box::pin(self.run_agent(chat, turn)).await,
            Decision::Run { intent, confirm } => (intent, confirm),
        };
        if let Some(refusal) = self.refusal_for(&chat.from, &intent, l) {
            return self.say(chat, &refusal).await;
        }
        match intent {
            Intent::None { reply } => {
                if reply.trim().is_empty() {
                    Ok(Flow::Done)
                } else {
                    self.say(chat, reply.trim()).await
                }
            }
            Intent::Status => {
                if self.defer(PendingAction::Status {
                    committee: committee.clone(),
                }) {
                    return Ok(Flow::Done);
                }
                let text = self.status_text(&committee).await;
                self.say(chat, &text).await
            }
            Intent::Relay {
                targets,
                kind,
                body,
            } => self.relay(chat, &targets, kind, &body).await,
            Intent::Bind { committee: raw } => {
                let Ok(target) = CommitteeId::new(raw.trim()) else {
                    return self.say(chat, &render::invalid_committee(l, &raw)).await;
                };
                if target == committee {
                    if self.group_role(&committee, &group_of(chat)) == Some(GroupRole::Agents) {
                        // The Agent group: say so, and offer to make it the
                        // people group again.
                        self.restate_or_run(chat, PendingAction::ToPeople { committee }, true)
                            .await?;
                        return Ok(Flow::Done);
                    }
                    return self.say(chat, &render::already_bound(l, &committee)).await;
                }
                self.restate_or_run(
                    chat,
                    PendingAction::Bind {
                        from: Some(committee),
                        to: target,
                    },
                    confirm,
                )
                .await?;
                Ok(Flow::Done)
            }
            Intent::Unbind => {
                self.restate_or_run(chat, PendingAction::Unbind { committee }, confirm)
                    .await?;
                Ok(Flow::Done)
            }
            Intent::Decide { body, supersedes } => {
                let body = body.trim();
                if body.is_empty() {
                    return self.say(chat, &render::empty_decision(l)).await;
                }
                if let Some(old) = supersedes {
                    let replaces = self.hub.get(&committee, old).await?;
                    if replaces.is_none_or(|e| e.kind != Kind::Decision) {
                        return self.say(chat, &render::not_a_decision(l, old)).await;
                    }
                }
                let action = PendingAction::Decide {
                    chat: (committee == chat.committee).then_some(chat.seq),
                    committee,
                    key: agent::event_key(chat),
                    body: body.to_owned(),
                    supersedes,
                };
                self.restate_or_run(chat, action, confirm).await?;
                Ok(Flow::Done)
            }
            Intent::Recall => {
                if self.defer(PendingAction::Recall {
                    committee: committee.clone(),
                }) {
                    return Ok(Flow::Done);
                }
                let brief = self.hub.brief(&committee).await?;
                self.say(chat, &render::brief(l, &brief)).await
            }
            Intent::AdoptAgentGroup { committee: raw } => {
                let asked = match raw.as_deref().map(str::trim) {
                    None | Some("") => committee.clone(),
                    Some(raw) => match CommitteeId::new(raw) {
                        Ok(asked) => asked,
                        Err(_) => return self.say(chat, &render::invalid_committee(l, raw)).await,
                    },
                };
                if asked != committee {
                    return self
                        .say(chat, &render::adopt_other_committee(l, &committee, &asked))
                        .await;
                }
                // A people group stops being one: never without a yes, even
                // by fixed syntax.
                let from_people =
                    self.group_role(&committee, &group_of(chat)) == Some(GroupRole::People);
                let action = PendingAction::AdoptAgents {
                    committee,
                    from_people,
                };
                self.restate_or_run(chat, action, confirm || from_people)
                    .await?;
                Ok(Flow::Done)
            }
            Intent::InviteToAgentGroup { who } => self.invite_to_agents(chat, &who).await,
            Intent::SetOutputLevel { level } => self.set_output_level(chat, level).await,
            Intent::Work {
                target,
                instruction,
            } => self.request_work(chat, &target, &instruction).await,
            Intent::Reassign { from, to } => self.request_reassign(chat, &from, &to).await,
            Intent::Remember { query } => {
                let hits = self
                    .hub
                    .memory_search(&committee, &query, MEMORY_LIST, false)
                    .await?;
                self.say(chat, &render::memory_list(l, &query, &hits)).await
            }
            Intent::EditMemoryByQuery { query, replacement } => {
                // Empty search terms would list the newest points.
                let hits = if senate_core::memory::terms(&query).is_empty() {
                    Vec::new()
                } else {
                    self.hub
                        .memory_search(&committee, &query, MEMORY_LIST, false)
                        .await?
                };
                let [hit] = hits.as_slice() else {
                    return self
                        .say(chat, &render::memory_edit_choices(l, &query, &hits))
                        .await;
                };
                let target = hit.point.seq;
                let intent = match replacement {
                    Some(text) => Intent::Correct { target, text },
                    None => Intent::Forget {
                        targets: vec![target],
                    },
                };
                Box::pin(self.run_bound(
                    chat,
                    Decision::Run {
                        intent,
                        confirm: true,
                    },
                ))
                .await
            }
            Intent::Forget { targets } => {
                if let Some(seq) = targets
                    .iter()
                    .find(|seq| self.memory_point(&committee, **seq).is_none())
                {
                    return self.say(chat, &render::not_a_memory_point(l, *seq)).await;
                }
                let action = PendingAction::Memory {
                    committee,
                    key: agent::event_key(chat),
                    payload: senate_core::MemoryPayload::Delete { targets },
                    body: String::new(),
                };
                self.restate_or_run(chat, action, confirm).await?;
                Ok(Flow::Done)
            }
            Intent::Correct { target, text } => {
                let Some(point) = self.memory_point(&committee, target) else {
                    return self.say(chat, &render::not_a_memory_point(l, target)).await;
                };
                let mut refs = point.refs;
                if committee == chat.committee {
                    refs.push(chat.seq);
                }
                let action = PendingAction::Memory {
                    committee,
                    key: agent::event_key(chat),
                    payload: senate_core::MemoryPayload::Correct { target, refs },
                    body: text,
                };
                self.restate_or_run(chat, action, confirm).await?;
                Ok(Flow::Done)
            }
        }
    }

    /// The memory point at `seq` of `committee`, when it is in force.
    fn memory_point(&self, committee: &CommitteeId, seq: Seq) -> Option<senate_core::MemoryPoint> {
        let memory = self.committees.get(committee)?.brief.as_ref()?.memory();
        memory
            .is_in_force(seq)
            .then(|| memory.get(seq).map(|hit| hit.point))
            .flatten()
    }

    /// The role `group` is bound in on `committee`, as observed.
    fn group_role(&self, committee: &CommitteeId, group: &Option<String>) -> Option<GroupRole> {
        let group = group.as_deref()?;
        self.committees
            .get(committee)?
            .groups
            .iter()
            .find(|g| g.group.group_id == group)
            .map(|g| g.role)
    }

    /// Reply to `chat` and be done with it.
    async fn say(&mut self, chat: &Event, text: &str) -> Result<Flow, Stall> {
        self.reply_to_chat(chat, text).await.map(|_| Flow::Done)
    }

    /// The technical side of a model failure on `chat`, once: into the
    /// committee's Agent group when it has one, else a note in the log to
    /// the Speaker's person (the Speaker, or the person who runs the agent
    /// that is Speaker); always to the log of the daemon.
    async fn report_model_failure(&mut self, chat: &Event, error: &str) -> Result<(), Stall> {
        let committee = chat.committee.clone();
        let l = self.locale(&committee);
        warn!(%committee, seq = %chat.seq, %error, "the speaker's model failed on a chat");
        let text = render::model_failure_note(l, &chat.from, chat.seq, error);
        let agents = self
            .committees
            .get(&committee)
            .and_then(|state| state.agents_group().cloned());
        if let Some(agents) = agents {
            let group = agents.group.group_id;
            let target = format!("{group}#model-failure");
            if self.has_copy(&committee, chat.seq, &target).await? {
                return Ok(());
            }
            let operation = self.op_id(&committee, chat.seq, &target)?;
            if let Some(posted) = self
                .post(PostTarget::NewRoot { group_id: group }, &text, &operation)
                .await?
            {
                self.hub
                    .put_mirror(MirrorRef {
                        committee: committee.clone(),
                        seq: chat.seq,
                        platform: self.surface.platform().to_owned(),
                        target,
                        message_ref: posted.message_ref,
                        thread_ref: posted.thread_ref,
                    })
                    .await?;
            }
            return Ok(());
        }
        let Some(person) = self.speakers_person() else {
            debug!(%committee, "no Agent group and no Speaker's person; the failure is logged only");
            return Ok(());
        };
        let note = NewEvent::new(committee.clone(), Kind::Note, Address::Speaker, text)
            .with_id(derived_event_id(&[
                "model-failure",
                committee.as_str(),
                &chat.seq.to_string(),
            ]))
            .to(Address::senator(&person));
        self.append(note).await.map(drop)
    }

    /// The person behind the Speaker: the Speaker when it is a person of
    /// this Senate, else the person who runs the agent that is Speaker.
    fn speakers_person(&self) -> Option<SenatorName> {
        let office = self.roster.office()?;
        let speaker = office.speaker_here()?;
        match office.kind {
            senate_core::SenatorKind::Person => Some(speaker.clone()),
            senate_core::SenatorKind::Agent => self
                .roster
                .owner_of(speaker)
                .and_then(|owner| SenatorName::new(owner.clone()).ok()),
            _ => None,
        }
    }

    /// Restate `action` and wait for a yes, or (fixed syntax) run it now.
    async fn restate_or_run(
        &mut self,
        chat: &Event,
        action: PendingAction,
        confirm: bool,
    ) -> Result<(), Stall> {
        if let Some(actions) = &mut self.preparing {
            actions.push(action);
            return Ok(());
        }
        let Some(group) = chat.source.as_ref().map(group_ref) else {
            debug!("a binding change from outside any group; nothing to bind");
            return Ok(());
        };
        if confirm {
            let l = self.locale(&chat.committee);
            let Some(posted) = self.reply_to_chat(chat, &action.restate(l)).await? else {
                return Ok(());
            };
            let origin_ref = chat
                .mirror
                .as_ref()
                .map(|m| m.message_ref.clone())
                .unwrap_or_default();
            self.pending.push(Pending {
                confirm_ref: posted.message_ref,
                thread_ref: posted.thread_ref,
                origin_ref,
                origin: Origin::Bound {
                    committee: chat.committee.clone(),
                    seq: chat.seq,
                },
                group,
                actions: vec![action],
                agent_plan: false,
                execution: None,
                requester: chat.from.clone(),
                expires: Instant::now() + self.confirmation_ttl,
            });
            return Ok(());
        }
        let trigger = format!("{}:{}", chat.committee, chat.seq);
        let text = self
            .execute_now(&action, &group, &chat.from, &trigger)
            .await?;
        if text.is_empty() {
            return Ok(());
        }
        self.reply_to_chat(chat, &text).await.map(drop)
    }

    /// What `who` may not ask for, as the refusal to say: a guest asks and
    /// is answered, so nothing that changes state, and no note or
    /// instruction pushed into an agent's session (a question is fine).
    fn refusal_for(&self, who: &Address, intent: &Intent, l: Locale) -> Option<String> {
        if self.is_member(who) {
            return None;
        }
        if intent.changes_state() {
            return Some(render::guest_refused(l));
        }
        if matches!(
            intent,
            Intent::Relay {
                kind: RelayKind::Note,
                ..
            }
        ) {
            return Some(render::guest_note_refused(l));
        }
        None
    }

    async fn relay(
        &mut self,
        chat: &Event,
        targets: &[String],
        kind: RelayKind,
        body: &str,
    ) -> Result<Flow, Stall> {
        let l = self.locale(&chat.committee);
        let body = body.trim();
        if body.is_empty() {
            return self.say(chat, &render::empty_relay(l)).await;
        }
        let mut to: Vec<Address> = Vec::new();
        let mut unknown = Vec::new();
        // The seats named in child Senates, as qualified names.
        let mut seats: Vec<senate_core::Locator> = Vec::new();
        for target in targets {
            let waiting = Waiting::Intent {
                intent: Intent::Relay {
                    targets: targets.to_vec(),
                    kind,
                    body: body.to_owned(),
                },
                said: target.clone(),
                confirm: true,
            };
            match self.settle_reach(chat, target, body, waiting).await? {
                Settled::To(senator) => {
                    let address = Address::Senator(senator);
                    if !to.contains(&address) {
                        to.push(address);
                    }
                }
                Settled::Child(target) => {
                    // That Senate's Speaker's desk, which delivers to the
                    // seat when one is named.
                    let address = Address::Child {
                        senate: target.senate.clone(),
                        committee: target.committee.clone(),
                    };
                    if !to.contains(&address) {
                        to.push(address);
                    }
                    if let Some(seat) = target
                        .seat
                        .as_ref()
                        .and_then(|seat| self.seat_locator(&target, seat))
                    {
                        seats.push(seat);
                    }
                }
                Settled::Unknown => unknown.push(target.clone()),
                // Carried on when the person answers which agent.
                Settled::Asked => return Ok(Flow::Done),
            }
        }
        if to.is_empty() || !unknown.is_empty() {
            let text = render::unknown_targets(l, &unknown, &self.roster);
            return self.say(chat, &text).await;
        }
        let kind = match kind {
            RelayKind::Question => Kind::Question,
            RelayKind::Note => Kind::Note,
            RelayKind::Request => Kind::Request,
        };
        let seq = agent::event_key(chat);
        let committee = self.acting_committee(chat);
        let mut relay = NewEvent::new(committee.clone(), kind.clone(), Address::Speaker, body)
            .with_id(derived_event_id(&["relay", committee.as_str(), &seq]))
            .with_to(to.clone())
            .on_behalf_of(chat.from.clone());
        if committee == chat.committee {
            // Acknowledged under the chat when processed; a relay on another
            // committee (after a Bind) is acknowledged in the plan's result.
            relay = relay.with_meta(meta::RELAYED_FROM, json!(chat.seq.0));
        }
        // One message carries one intended name: a relay to several
        // recipients names no seat, and the desk there decides.
        if let ([_], [seat]) = (to.as_slice(), seats.as_slice()) {
            relay = relay.with_meta(meta::INTENDED, json!(seat.to_string()));
        }
        if kind == Kind::Question {
            // A task: judged before it is written, and the verdict travels
            // on the relay, so the bridge pushes it at once.
            let cached = self
                .committees
                .get(&chat.committee)
                .and_then(|state| state.triage.get(&chat.seq))
                .cloned();
            let Some(triage) = cached else {
                let input = TriageInput {
                    committee: committee.clone(),
                    kind,
                    asker: chat.from.clone(),
                    to,
                    body: body.to_owned(),
                    seq: None,
                    context: self.context(&chat.committee, chat.seq).await?,
                    locale: self.locale(&committee),
                };
                self.start_triage(&chat.committee, chat.seq, input);
                return Ok(Flow::Await);
            };
            relay = relay.with_meta(meta::TRIAGE, triage.value());
        }
        if let Some(source) = &chat.source {
            relay = relay.with_source(source.clone());
        }
        if self.defer(PendingAction::Relay {
            event: Box::new(relay.clone()),
        }) {
            return Ok(Flow::Done);
        }
        // The acknowledgement is posted when the relay itself is processed.
        self.append(relay).await.map(|_| Flow::Done)
    }

    /// The pending confirmation `chat` answers: in its thread (or replying
    /// to the restatement), and from the person who asked. Anyone else's
    /// message there is just chat.
    fn pending_for_chat(&mut self, chat: &Event) -> Option<usize> {
        self.purge_expired();
        let group_id = chat.source.as_ref()?.group_id.as_str();
        let reply_to = chat.meta.get(meta::REPLY_TO).and_then(Value::as_str);
        let thread_root = chat.meta.get(meta::THREAD_ROOT).and_then(Value::as_str);
        let thread_ref = chat.mirror.as_ref().and_then(|m| m.thread_ref.as_deref());
        self.pending.iter().position(|pending| {
            pending.group.group_id == group_id
                && pending.requester == chat.from
                && (reply_to == Some(pending.confirm_ref.as_str())
                    || (!pending.origin_ref.is_empty()
                        && thread_root == Some(pending.origin_ref.as_str()))
                    || (thread_ref.is_some() && thread_ref == pending.thread_ref.as_deref())
                    || matches!(&pending.origin, Origin::Bound { committee, seq }
                        if *committee == chat.committee && chat.thread == Some(*seq)))
        })
    }

    async fn resolve_for_chat(&mut self, index: usize, chat: &Event) -> Result<(), Stall> {
        let l = self.locale(&chat.committee);
        let Some(pending) = self.pending.get(index).cloned() else {
            return Ok(());
        };
        let text = if is_affirmative(&chat.body) {
            let trigger = format!("{}:{}", chat.committee, chat.seq);
            self.execute_pending(index, &pending, &trigger).await?
        } else {
            render::cancelled(l)
        };
        if !text.is_empty() {
            self.reply_to_chat(chat, &text).await?;
        }
        if pending.agent_plan {
            self.remember_confirmation(chat, &text);
        }
        self.pending
            .retain(|p| p.confirm_ref != pending.confirm_ref);
        Ok(())
    }

    fn purge_expired(&mut self) {
        let now = Instant::now();
        self.pending
            .retain(|pending| pending.expires > now || pending.execution.is_some());
        self.conversations.retain(|session| session.expires > now);
    }

    /// A person's yes (对 / 是 / 好 / yes / ok, …) in the thread of an agent's
    /// proposed decision that no decision answers yet records it. True when
    /// `chat` did that; anything else in the thread is ordinary chat.
    /// Whether the proposal is open is read from the log, so a confirmation
    /// counts however late, and across restarts.
    async fn confirm_proposal(&mut self, chat: &Event) -> Result<bool, Stall> {
        let Some(root) = chat.thread else {
            return Ok(false);
        };
        if !is_affirmative(&chat.body) || !self.is_member(&chat.from) {
            return Ok(false);
        }
        let Some(proposal) = self
            .hub
            .get(&chat.committee, root)
            .await?
            .filter(|e| is_proposal(e, &self.roster.people()))
        else {
            return Ok(false);
        };
        let decided = self
            .hub
            .thread(&chat.committee, root)
            .await?
            .iter()
            .any(|e| e.kind == Kind::Decision);
        if decided {
            return Ok(false);
        }
        let mut decision = NewEvent::new(
            chat.committee.clone(),
            Kind::Decision,
            Address::Speaker,
            proposal.body.trim(),
        )
        // One decision per proposal, whoever confirms and however often.
        .with_id(derived_event_id(&[
            "proposal-decision",
            chat.committee.as_str(),
            &root.to_string(),
        ]))
        .in_thread(root)
        .on_behalf_of(chat.from.clone())
        .with_meta(meta::RELAYED_FROM, json!(chat.seq.0))
        .with_meta(meta::PROPOSED_BY, json!(proposal.from.to_string()));
        if let Some(sources) = proposal.meta.get(meta::SOURCES) {
            decision = decision.with_meta(meta::SOURCES, sources.clone());
        }
        if let Some(source) = &chat.source {
            decision = decision.with_source(source.clone());
        }
        // The acknowledgement is posted when the decision is processed.
        self.append(decision).await?;
        Ok(true)
    }

    /// Intent mode retains its existing refusal replies and retry behaviour.
    async fn execute_now(
        &self,
        action: &PendingAction,
        group: &GroupRef,
        requester: &Address,
        trigger: &str,
    ) -> Result<String, Stall> {
        let l = self.locale(action.committee());
        match self.execute(action, group, requester, trigger, false).await {
            Ok(text) => Ok(text),
            Err(ActionError::Retry(stall)) => Err(stall),
            Err(ActionError::Refused) => Ok(render::action_not_allowed(l)),
        }
    }

    /// Append the events an action means; returns what to say (empty: the
    /// event's own mirror says it).
    async fn execute(
        &self,
        action: &PendingAction,
        group: &GroupRef,
        requester: &Address,
        trigger: &str,
        strict: bool,
    ) -> Result<String, ActionError> {
        let l = self.locale(action.committee());
        match action {
            PendingAction::Bind { from, to } => {
                if let Some(from) = from {
                    self.append_action(
                        binding_event(l, from, group, BindingAction::Unbind, requester, trigger),
                        strict,
                    )
                    .await?;
                }
                self.append_action(
                    binding_event(l, to, group, BindingAction::Bind, requester, trigger),
                    strict,
                )
                .await?;
                Ok(render::bound(l, to))
            }
            PendingAction::Unbind { committee } => {
                self.append_action(
                    binding_event(
                        l,
                        committee,
                        group,
                        BindingAction::Unbind,
                        requester,
                        trigger,
                    ),
                    strict,
                )
                .await?;
                Ok(render::unbound(l, committee))
            }
            PendingAction::ToPeople { committee } => {
                self.append_action(
                    binding_event(l, committee, group, BindingAction::Bind, requester, trigger),
                    strict,
                )
                .await?;
                Ok(render::back_to_people(l, committee))
            }
            PendingAction::AdoptAgents { committee, .. } => {
                let id = derived_event_id(&[
                    "binding",
                    "bind",
                    "agents",
                    committee.as_str(),
                    &group.group_id,
                    trigger,
                ]);
                self.append_action(
                    group_binding(
                        l,
                        committee,
                        group,
                        BindingAction::Bind,
                        GroupRole::Agents,
                        Some(requester),
                        id,
                    ),
                    strict,
                )
                .await?;
                Ok(render::adopted(l, committee))
            }
            PendingAction::Decide {
                committee,
                chat,
                key,
                body,
                supersedes,
            } => {
                let mut decision =
                    NewEvent::new(committee.clone(), Kind::Decision, Address::Speaker, body)
                        .with_id(derived_event_id(&["decision", committee.as_str(), key]))
                        .on_behalf_of(requester.clone())
                        .with_source(Source {
                            platform: group.platform.clone(),
                            group_id: group.group_id.clone(),
                            group_name: group.group_name.clone(),
                            simulated: group.simulated,
                            simulated_by: group.simulated_by.clone(),
                        });
                if let Some(chat) = chat {
                    decision = decision.with_meta(meta::RELAYED_FROM, json!(chat.0));
                }
                if let Some(old) = supersedes {
                    decision = decision.with_meta(meta::SUPERSEDES, json!(old.0));
                }
                // Acknowledged in the speaker's thread when it is processed,
                // or here when it was said in another committee's log.
                let recorded = self.append_action(decision, strict).await?;
                Ok(match (chat, recorded) {
                    (None, Some(event)) => render::decision_ack(l, event.seq),
                    _ => String::new(),
                })
            }
            PendingAction::Relay { event } => {
                let relayed = self.append_action((**event).clone(), strict).await?;
                Ok(relayed
                    .map(|event| render::relay_ack_for(l, &event, &self.roster, self.seat_names()))
                    .unwrap_or_default())
            }
            PendingAction::Status { committee } => Ok(self.status_text(committee).await),
            PendingAction::Memory {
                committee,
                key,
                payload,
                body,
            } => {
                let edit = NewEvent::new(committee.clone(), Kind::Note, Address::Speaker, body)
                    .with_id(derived_event_id(&["memory-edit", committee.as_str(), key]))
                    .on_behalf_of(requester.clone())
                    .with_meta(meta::MEMORY, payload.to_value())
                    // Where it was asked: a simulated `/forget` changes
                    // nothing the committee knows.
                    .with_source(Source {
                        platform: group.platform.clone(),
                        group_id: group.group_id.clone(),
                        group_name: group.group_name.clone(),
                        simulated: group.simulated,
                        simulated_by: group.simulated_by.clone(),
                    });
                let recorded = self.append_action(edit, strict).await?;
                Ok(recorded
                    .map(|event| render::memory_edited(l, event.seq))
                    .unwrap_or_default())
            }
            PendingAction::Recall { committee } => {
                let brief = self.hub.brief(committee).await.map_err(Stall::from)?;
                Ok(render::brief(l, &brief))
            }
            PendingAction::Invite {
                committee,
                group,
                handles,
                ids,
            } => {
                let operation =
                    self.op_id(committee, Seq::ZERO, &format!("{group}#invite-{trigger}"))?;
                match self
                    .call(self.surface.invite(group, ids, &operation))
                    .await?
                {
                    Outcome::Done(()) => Ok(render::invited(l, handles)),
                    Outcome::Unsupported | Outcome::Refused => Err(ActionError::Refused),
                }
            }
            PendingAction::OutputLevel { committee, level } => {
                let note = NewEvent::new(
                    committee.clone(),
                    Kind::Note,
                    Address::Speaker,
                    render::output_level_note(l, *level),
                )
                .with_id(derived_event_id(&[
                    "output-level",
                    committee.as_str(),
                    trigger,
                ]))
                .on_behalf_of(requester.clone())
                .with_meta(meta::OUTPUT_LEVEL, json!(level));
                self.append_action(note, strict).await?;
                Ok(render::output_level_set(l, *level))
            }
            PendingAction::Work { committee, run } => self
                .execute_work(committee, run, strict)
                .await
                .map_err(ActionError::from),
            PendingAction::Reassign {
                committee,
                agent,
                to,
                ..
            } => self
                .execute_reassign(committee, agent, to, requester, strict)
                .await
                .map_err(ActionError::from),
        }
    }

    /// `/status`: every seat on the committee, here and in child Senates,
    /// as the Speaker's prompt lists them ([`Engine::committee_seats`]),
    /// then the presence lines of the agents here. The built-in session is
    /// no member and is never listed.
    async fn status_text(&self, committee: &CommitteeId) -> String {
        let l = self.locale(committee);
        let seats = self.committee_seats(committee).await;
        let presence = self.presence_lines(committee).await;
        render::status(l, committee, &seats, &presence)
    }

    /// Reply in the thread of the platform message a `chat` is, and record
    /// the reply as that chat's copy in its group (restart safety).
    async fn reply_to_chat(&mut self, chat: &Event, text: &str) -> Result<Option<Posted>, Stall> {
        let platform = self.surface.platform().to_owned();
        let (Some(mirror), Some(source)) = (
            chat.mirror.as_ref().filter(|m| m.platform == platform),
            chat.source.as_ref(),
        ) else {
            debug!("this chat has no platform message to reply to");
            return Ok(None);
        };
        let target = agent::reply_target(chat, &source.group_id);
        if let Some(copy) = self
            .hub
            .mirrors_of_seq(&chat.committee, chat.seq, &platform)
            .await?
            .into_iter()
            .find(|copy| copy.target == target)
        {
            return Ok(Some(Posted {
                message_ref: copy.message_ref,
                thread_ref: copy.thread_ref,
                duplicate: true,
            }));
        }
        let operation = self.op_id(&chat.committee, chat.seq, &target)?;
        let posted = self
            .post(
                PostTarget::Reply {
                    message_ref: mirror.message_ref.clone(),
                    in_thread: true,
                },
                text,
                &operation,
            )
            .await?;
        if let Some(posted) = &posted {
            agent::remember_reply(self, chat, text);
            self.hub
                .put_mirror(MirrorRef {
                    committee: chat.committee.clone(),
                    seq: chat.seq,
                    platform,
                    target,
                    message_ref: posted.message_ref.clone(),
                    thread_ref: posted.thread_ref.clone(),
                })
                .await?;
        }
        Ok(posted)
    }

    // ----- unbound groups ---------------------------------------------------

    pub(crate) async fn on_unbound(&mut self, mention: UnboundMention) {
        if let Err(stall) = self.handle_unbound(mention).await {
            warn!(error = %stall, "could not answer a mention in an unbound group");
        }
    }

    async fn handle_unbound(&mut self, mention: UnboundMention) -> Result<(), Stall> {
        self.sync_roster().await;
        let l = self.config.locale;
        self.purge_expired();
        let pending = self
            .pending
            .iter()
            .position(|p| {
                p.origin == Origin::Unbound
                    && p.group.group_id == mention.group_id
                    && p.answered_by(&mention)
            })
            .and_then(|index| {
                self.pending
                    .get(index)
                    .cloned()
                    .map(|pending| (index, pending))
            });
        if let Some((index, pending)) = pending {
            if pending.requester == mention.sender {
                let text = if is_affirmative(&mention.text) {
                    let trigger = format!("unbound:{}:{}", mention.group_id, mention.message_ref);
                    self.execute_pending(index, &pending, &trigger).await?
                } else {
                    render::cancelled(l)
                };
                self.reply_unbound(&mention, &text).await?;
                self.pending
                    .retain(|p| p.confirm_ref != pending.confirm_ref);
                return Ok(());
            }
            // Someone else in that thread: no confirmation, and an
            // instruction only if they mention the Speaker.
            if mention.addressing != "mention" {
                return Ok(());
            }
        } else if !matches!(mention.addressing.as_str(), "mention" | "reply")
            && !self.continuing_unbound(&mention)
        {
            // Forwarded because it answers a Speaker post, but nothing there
            // awaits an answer.
            return Ok(());
        }

        if self.mode == SpeakerMode::Agent && self.ignores_unbound_reply(&mention) {
            return Ok(());
        }
        match parse_command(&mention.text, l) {
            Some(parsed) => self.run_unbound(&mention, command_decision(parsed)).await,
            None => {
                let utterance = Utterance {
                    text: mention.text.clone(),
                    speaker: mention.sender.clone(),
                    committee: None,
                    group: mention_group(&mention),
                    context: Vec::new(),
                    roster: self.roster.clone(),
                    seats: Vec::new(),
                    locale: l,
                    // Work needs a bound committee.
                    work: false,
                    gone_relays: Vec::new(),
                };
                if self.mode == SpeakerMode::Agent {
                    self.start_unbound_conversation(mention, utterance);
                    return Ok(());
                }
                let brain = Arc::clone(&self.brain);
                let mention = Box::new(mention);
                self.think(
                    async move { brain.interpret(utterance).await },
                    move |intent| ThoughtKind::UnboundInterpreted { mention, intent },
                );
                Ok(())
            }
        }
    }

    async fn run_unbound(
        &mut self,
        mention: &UnboundMention,
        decision: Decision,
    ) -> Result<(), Stall> {
        let (intent, confirm) = match decision {
            Decision::Say(text) => return self.reply_unbound(mention, &text).await.map(drop),
            Decision::ModelDown(error) => {
                // No committee, so no Agent group or Speaker's note: the log.
                warn!(%error, group = %mention.group_id, "the speaker's model failed on an unbound mention");
                let l = self.config.locale;
                let text = render::model_unavailable(l, as_command(&mention.text, l).as_deref());
                return self.reply_unbound(mention, &text).await.map(drop);
            }
            Decision::Agent(turn) => {
                return Box::pin(self.run_unbound_agent(mention, Decision::Agent(turn))).await;
            }
            Decision::Run { intent, confirm } => (intent, confirm),
        };
        let l = self.config.locale;
        if let Some(refusal) = self.refusal_for(&mention.sender, &intent, l) {
            return self.reply_unbound(mention, &refusal).await.map(drop);
        }
        let group = mention_group(mention);
        let trigger = format!("unbound:{}:{}", mention.group_id, mention.message_ref);
        let action = match intent {
            Intent::Bind { committee: raw } => CommitteeId::new(raw.trim())
                .map(|to| PendingAction::Bind { from: None, to })
                .map_err(|_| render::invalid_committee(l, &raw)),
            Intent::AdoptAgentGroup { committee: raw } => match raw.as_deref().map(str::trim) {
                None | Some("") => Err(render::adopt_needs_committee(l)),
                Some(raw) => CommitteeId::new(raw)
                    .map(|committee| PendingAction::AdoptAgents {
                        committee,
                        from_people: false,
                    })
                    .map_err(|_| render::invalid_committee(l, raw)),
            },
            Intent::Unbind => Err(render::nothing_to_unbind(l)),
            Intent::Status => Err(render::unbound_status(l)),
            Intent::Relay { .. } => Err(render::relay_needs_binding(l)),
            Intent::Decide { .. } => Err(render::decide_needs_binding(l)),
            Intent::Recall
            | Intent::Remember { .. }
            | Intent::Forget { .. }
            | Intent::Correct { .. }
            | Intent::EditMemoryByQuery { .. } => Err(render::recall_needs_binding(l)),
            Intent::InviteToAgentGroup { .. } | Intent::SetOutputLevel { .. } => {
                Err(render::agents_need_binding(l))
            }
            Intent::Work { .. } | Intent::Reassign { .. } => Err(render::work_needs_binding(l)),
            Intent::None { reply } => Err(reply.trim().to_owned()),
        };
        let text = match action {
            Err(text) => text,
            Ok(action) => {
                if confirm {
                    if let Some(posted) = self.reply_unbound(mention, &action.restate(l)).await? {
                        self.pending.push(Pending {
                            confirm_ref: posted.message_ref,
                            thread_ref: posted.thread_ref,
                            origin_ref: mention.message_ref.clone(),
                            origin: Origin::Unbound,
                            group,
                            actions: vec![action],
                            agent_plan: false,
                            execution: None,
                            requester: mention.sender.clone(),
                            expires: Instant::now() + self.confirmation_ttl,
                        });
                    }
                    return Ok(());
                }
                self.execute_now(&action, &group, &mention.sender, &trigger)
                    .await?
            }
        };
        if text.is_empty() {
            return Ok(());
        }
        self.reply_unbound(mention, &text).await.map(drop)
    }

    async fn reply_unbound(
        &self,
        mention: &UnboundMention,
        text: &str,
    ) -> Result<Option<Posted>, Stall> {
        if mention.message_ref.is_empty() {
            return Ok(None);
        }
        self.post(
            PostTarget::Reply {
                message_ref: mention.message_ref.clone(),
                in_thread: true,
            },
            text,
            &unbound_operation_id(&mention.group_id, &mention.message_ref),
        )
        .await
    }

    // ----- plumbing ---------------------------------------------------------

    /// Append, treating a refusal as permanent (logged, passed).
    async fn append(&self, event: NewEvent) -> Result<Option<Event>, Stall> {
        let committee = event.committee.clone();
        match self.hub.append(self.stamped(event)).await {
            Ok(appended) => Ok(Some(appended.event)),
            Err(error @ (HubError::Rejected(_) | HubError::NotFound(_))) => {
                warn!(%committee, %error, "the log refused a speaker event; passing it");
                Ok(None)
            }
            Err(error) => Err(Stall::Hub(error)),
        }
    }

    /// Post, treating a refusal as permanent (logged, passed) and no answer
    /// within [`POST_TIMEOUT`] as the surface being unavailable.
    async fn post(
        &self,
        target: PostTarget,
        text: &str,
        operation_id: &str,
    ) -> Result<Option<Posted>, Stall> {
        let result =
            tokio::time::timeout(POST_TIMEOUT, self.surface.post(target, text, operation_id))
                .await
                .unwrap_or_else(|_| {
                    Err(SurfaceError::Unavailable(format!(
                        "no answer within {} s",
                        POST_TIMEOUT.as_secs()
                    )))
                });
        match result {
            Ok(posted) => Ok(Some(posted)),
            Err(SurfaceError::Rejected(reason) | SurfaceError::Unsupported(reason)) => {
                warn!(operation_id, %reason, "the chat surface refused a post; passing it");
                Ok(None)
            }
            Err(SurfaceError::Unavailable(reason)) => Err(Stall::Surface(reason)),
        }
    }

    /// One surface call other than a plain post, bounded by
    /// [`POST_TIMEOUT`]. Unavailable (or no answer) is a stall; a refusal is
    /// logged and passed; unsupported is the caller's to fall back from.
    async fn call<T>(
        &self,
        call: impl Future<Output = Result<T, SurfaceError>>,
    ) -> Result<Outcome<T>, Stall> {
        match tokio::time::timeout(POST_TIMEOUT, call).await {
            Err(_) => Err(Stall::Surface(format!(
                "no answer within {} s",
                POST_TIMEOUT.as_secs()
            ))),
            Ok(Ok(value)) => Ok(Outcome::Done(value)),
            Ok(Err(SurfaceError::Unsupported(reason))) => {
                debug!(%reason, "the chat surface cannot do this; falling back");
                Ok(Outcome::Unsupported)
            }
            Ok(Err(SurfaceError::Rejected(reason))) => {
                warn!(%reason, "the chat surface refused a call; passing it");
                Ok(Outcome::Refused)
            }
            Ok(Err(SurfaceError::Unavailable(reason))) => Err(Stall::Surface(reason)),
        }
    }

    /// Cards are unsupported: say so once, not on every post.
    fn cards_unsupported_once(&self) {
        if !self.cards_unsupported.swap(true, Ordering::Relaxed) {
            warn!(
                "the chat surface has no cards: task cards are plain posts, pinned cards are skipped"
            );
        }
    }

    /// The committee's most recent events before `before`, oldest first.
    async fn context(&self, committee: &CommitteeId, before: Seq) -> Result<Vec<Event>, Stall> {
        let after = Seq(before.0.saturating_sub(CONTEXT_EVENTS + 1));
        let events = self
            .hub
            .read(committee, after, CONTEXT_EVENTS as usize)
            .await?;
        Ok(events.into_iter().filter(|e| e.seq < before).collect())
    }
}

/// Which answer-for-an-agent a Brain draft is for.
struct Drafted {
    committee: CommitteeId,
    root: Seq,
    agent: SenatorName,
    question: Seq,
    asker: Address,
}

/// The Speaker's answer for `agent` to `question`. Its id is derived
/// from (committee, question, agent) whether or not it is a takeover, so one
/// question gets at most one answer for the agent however often it is redecided.
fn behalf_answer(
    committee: &CommitteeId,
    root: Seq,
    agent: &SenatorName,
    question: Seq,
    asker: Address,
    body: String,
) -> NewEvent {
    NewEvent::new(committee.clone(), Kind::Answer, Address::Speaker, body)
        .with_id(derived_event_id(&[
            "fallback",
            committee.as_str(),
            &question.to_string(),
            &agent.to_string(),
        ]))
        .in_thread(root)
        .on_behalf_of(Address::senator(agent))
        .to(asker)
}

/// One planned copy of a log event.
struct MirrorPlan {
    to: PostTarget,
    text: String,
    /// The [`MirrorRef::target`]: the group posted into.
    target: String,
    /// The `message_ref` the copy records, when not the post itself.
    record_as: Option<String>,
    /// Posted as this card where the surface has cards; `text` otherwise.
    card: Option<Card>,
}

/// The group a copy's target names (`refund-backend#ack` → `refund-backend`).
fn group_of_target(target: &str) -> &str {
    target.split_once('#').map_or(target, |(group, _)| group)
}

/// A Speaker relay whose inline verdict parks it on its person.
fn parked_inline(event: &Event) -> bool {
    event.from == Address::Speaker
        && TriagePayload::of(event).is_some_and(|t| t.verdict == TriageVerdict::NeedsHuman)
}

/// The marker recorded beside a copy in `target` that is a card.
fn card_target(target: &str) -> String {
    format!("{target}#card")
}

/// What a surface call came to, short of a stall.
enum Outcome<T> {
    Done(T),
    /// The surface cannot do it at all; the caller falls back.
    Unsupported,
    /// The platform refused this one (logged); passed.
    Refused,
}

/// A Speaker note carrying the brief's top.
fn is_brief_top(event: &Event) -> bool {
    event.meta.get(meta::BRIEF_TOP).and_then(Value::as_bool) == Some(true)
}

/// A root `note` proposing a decision: an agent's, or the Speaker's for
/// what people agreed on in discussion ([`agreement`]).
fn is_proposal(event: &Event, people: &People) -> bool {
    agreement::is_discussion_proposal(event)
        || (event.kind == Kind::Note
            && people.is_agent_address(&event.from)
            && event.thread.is_none()
            && event.meta.get(meta::PROPOSAL).and_then(Value::as_str) == Some(PROPOSAL_DECISION))
}

fn decision_of(intent: Result<Intent, BrainError>, l: Locale) -> Decision {
    match intent {
        Ok(intent) => Decision::Run {
            intent,
            confirm: true,
        },
        Err(error) => failed(error, l),
    }
}

/// What a Brain failure comes to: with no model, the fixed syntax is all
/// there is and the message is not understood; otherwise the model failed
/// and the asker is not blamed for it.
fn failed(error: BrainError, l: Locale) -> Decision {
    if matches!(error, BrainError::NoModel) {
        debug!("no model; answering with the fixed syntax");
        Decision::Say(render::not_understood(l))
    } else {
        warn!(%error, "the speaker's model failed on every tier");
        Decision::ModelDown(error.to_string())
    }
}

/// `text` as a fixed-syntax command once it has its leading `/`, when it
/// reads as one: `tell alice hi` → `/tell alice hi`.
fn as_command(text: &str, l: Locale) -> Option<String> {
    let text = text.trim();
    if text.starts_with('/') || text.starts_with('／') {
        return None;
    }
    let command = format!("/{text}");
    matches!(parse_command(&command, l), Some(Ok(_))).then_some(command)
}

fn command_decision(parsed: Result<Intent, String>) -> Decision {
    match parsed {
        Ok(intent) => Decision::Run {
            intent,
            confirm: false,
        },
        Err(usage) => Decision::Say(usage),
    }
}

fn relayed_from(event: &Event) -> Option<Seq> {
    if event.from != Address::Speaker || event.thread.is_some() {
        return None;
    }
    event
        .meta
        .get(meta::RELAYED_FROM)
        .and_then(Value::as_u64)
        .map(Seq)
}

fn group_ref(source: &Source) -> GroupRef {
    GroupRef {
        platform: source.platform.clone(),
        group_id: source.group_id.clone(),
        group_name: source.group_name.clone(),
        simulated: source.simulated,
        simulated_by: source.simulated_by.clone(),
    }
}

fn mention_group(mention: &UnboundMention) -> GroupRef {
    GroupRef {
        platform: mention.platform.clone(),
        group_id: mention.group_id.clone(),
        group_name: mention.group_name.clone(),
        simulated: false,
        simulated_by: None,
    }
}

fn binding_event(
    l: Locale,
    committee: &CommitteeId,
    group: &GroupRef,
    action: BindingAction,
    requester: &Address,
    trigger: &str,
) -> NewEvent {
    let bind = action == BindingAction::Bind;
    let id = derived_event_id(&[
        "binding",
        if bind { "bind" } else { "unbind" },
        committee.as_str(),
        &group.group_id,
        trigger,
    ]);
    group_binding(
        l,
        committee,
        group,
        action,
        GroupRole::People,
        Some(requester),
        id,
    )
}

/// The group `chat` was said in, by id.
fn group_of(chat: &Event) -> Option<String> {
    chat.source.as_ref().map(|s| s.group_id.clone())
}

/// A `binding` of `group` in `role`, from the Speaker.
fn group_binding(
    l: Locale,
    committee: &CommitteeId,
    group: &GroupRef,
    action: BindingAction,
    role: GroupRole,
    requester: Option<&Address>,
    id: senate_core::EventId,
) -> NewEvent {
    let bind = action == BindingAction::Bind;
    let label = group.group_name.as_deref().unwrap_or(&group.group_id);
    let mut event = NewEvent::new(
        committee.clone(),
        Kind::Binding,
        Address::Speaker,
        render::binding_body(l, bind, label),
    )
    .with_id(id)
    .with_source(Source {
        platform: group.platform.clone(),
        group_id: group.group_id.clone(),
        group_name: group.group_name.clone(),
        simulated: group.simulated,
        simulated_by: group.simulated_by.clone(),
    })
    .with_binding(&BindingPayload {
        action,
        platform: group.platform.clone(),
        group_id: group.group_id.clone(),
        group_name: group.group_name.clone(),
        role,
    });
    if let Some(requester) = requester {
        event = event.on_behalf_of(requester.clone());
    }
    event
}
