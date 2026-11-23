//! Continuity: checkpoints and presence.
//!
//! Both are kept **per agent**. A person may have several agents, each on
//! its own machine or in its own session, so everything here is keyed by the
//! agent's [`SenatorName`], never by its person's handle. A person's
//! delegation is present while any one of its agents is
//! ([`Roster::delegation_presence`](crate::Roster::delegation_presence) over
//! [`PresenceBoard::is_present`]); a message for one specific agent that is
//! away is answered from *that* agent's checkpoint, not rerouted to another
//! agent of the same person.
//!
//! # Checkpoints
//!
//! A `checkpoint` event is an agent's handover note, written by its bridge
//! as the agent itself (the server refuses one from anyone else). Its
//! [`CheckpointPayload`] rides in [`meta::CHECKPOINT`]:
//! the branch and commit, whether that commit is on the remote, and the
//! three Markdown files the agent keeps in `<worktree>/`[`CHECKPOINT_DIR`]
//! (`PROGRESS.md`, `DECISIONS.md`, `OPEN.md`), at most
//! [`MAX_CHECKPOINT_BYTES`] in all. The body is a one-line summary in the
//! agent's own words. The code itself never leaves the developer's machine;
//! neither does the session transcript, whose existence alone is recorded.
//!
//! [`Checkpoints`] folds a log into each agent's latest checkpoint.
//!
//! # Presence
//!
//! Per (agent, committee), written by the server only, from heartbeats and
//! streams:
//!
//! ```text
//! online ──2 min without a heartbeat──▶ disconnected ──15 min──▶ absent
//!   ▲                                                             │
//!   └─────────── a heartbeat or a stream, from any state ──── taken_over (the Speaker
//!                                                              picked up for it)
//! ```
//!
//! `reassigned` is set when the agent's work on the committee moves to
//! another agent; [`PresencePayload::new_owner`] names which. Each
//! transition is one `presence` event from `committee` (the server speaking
//! for itself) with a [`PresencePayload`] in
//! [`meta::PRESENCE`] and an id derived from
//! (committee, agent, state, since) ([`presence_event`]), so a transition
//! written twice is stored once. An agent's first `online` is held by the
//! server but not written (every connecting bridge would add an event); its
//! first event is its first `disconnected`, and `online` is written when it
//! comes back. [`PresenceBoard`] folds a log into each agent's latest state;
//! the server's own view adds the live lag.
//!
//! A presence event's body is empty. What it says is in its payload, and
//! [`PresencePayload::line`] puts it into words when it is shown.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::address::Address;
use crate::committee_seat::{CommitteeSeatAction, CommitteeSeatChange, CommitteeSeats};
use crate::event::{Event, Kind, NewEvent, meta};
use crate::ids::{CommitteeId, EventId, Seq};
use crate::locale::Locale;
use crate::senator::SenatorName;

/// Largest `meta.checkpoint`, serialized, in bytes.
pub const MAX_CHECKPOINT_BYTES: usize = 64 * 1024;
/// Directory of the checkpoint files, relative to the worktree root.
pub const CHECKPOINT_DIR: &str = ".senate";
pub const PROGRESS_FILE: &str = "PROGRESS.md";
pub const DECISIONS_FILE: &str = "DECISIONS.md";
pub const OPEN_FILE: &str = "OPEN.md";

/// Default time without a heartbeat before `online` becomes `disconnected`.
pub const PRESENCE_DISCONNECTED_SECS: u64 = 120;
/// Default time without a heartbeat before an agent's lease expires and it
/// is `absent`.
pub const LEASE_SECS: u64 = 900;

/// What made the bridge write a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointTrigger {
    /// The checked-out commit or branch of a bound session changed.
    Commit,
    /// The session ended.
    SessionEnd,
    /// The session has been open for another interval.
    Interval,
    /// A handoff: a checkpoint that also pushes the branch.
    Handoff,
    /// The agent called the `checkpoint` tool.
    Tool,
}

impl CheckpointTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::SessionEnd => "session_end",
            Self::Interval => "interval",
            Self::Handoff => "handoff",
            Self::Tool => "tool",
        }
    }
}

/// The three checkpoint files, as the agent last wrote them. `None`: the
/// file does not exist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointFiles {
    /// `PROGRESS.md`: done / in progress / next.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<String>,
    /// `DECISIONS.md`: local decisions, each with the log seq it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisions: Option<String>,
    /// `OPEN.md`: questions the agent waits on, with their thread seqs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<String>,
}

impl CheckpointFiles {
    pub fn is_empty(&self) -> bool {
        self.progress.is_none() && self.decisions.is_none() && self.open.is_none()
    }

    /// Bytes of the three files together.
    pub fn len(&self) -> usize {
        [&self.progress, &self.decisions, &self.open]
            .into_iter()
            .flatten()
            .map(String::len)
            .sum()
    }
}

/// `meta["checkpoint"]` of a [`Kind::Checkpoint`] event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointPayload {
    /// The branch checked out; `HEAD` when detached.
    pub branch: String,
    /// The full commit id of `HEAD`.
    pub commit: String,
    /// Whether the commit is on the branch's remote upstream, as the machine's
    /// git last saw the remote.
    pub pushed: bool,
    /// `origin`'s URL without credentials, if there is an `origin`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub files: CheckpointFiles,
    /// The session's transcript exists on the machine. It is never uploaded.
    #[serde(default)]
    pub session_transcript: bool,
    pub trigger: CheckpointTrigger,
}

impl CheckpointPayload {
    pub const META_KEY: &'static str = meta::CHECKPOINT;

    /// The payload of a `checkpoint` event, if it is one and it parses.
    pub fn of(event: &Event) -> Option<Self> {
        if event.kind != Kind::Checkpoint {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::CHECKPOINT)?.clone()).ok()
    }

    /// The payload as a `meta` value.
    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Bytes of the serialized payload.
    pub fn size(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |v| v.len())
    }

    /// What is wrong with it as an append, if anything: a branch and a
    /// commit are named, and the whole payload fits [`MAX_CHECKPOINT_BYTES`].
    pub fn check(&self) -> Result<(), String> {
        if self.branch.trim().is_empty() {
            return Err("a checkpoint names its branch".into());
        }
        if self.commit.trim().is_empty() {
            return Err("a checkpoint names its commit".into());
        }
        let size = self.size();
        if size > MAX_CHECKPOINT_BYTES {
            return Err(format!(
                "meta.checkpoint is {size} bytes; the cap is {MAX_CHECKPOINT_BYTES}"
            ));
        }
        Ok(())
    }

    /// `branch@<first 7 of commit>`.
    pub fn at(&self) -> String {
        format!("{}@{}", self.branch, short_commit(&self.commit))
    }
}

/// The first seven characters of a commit id.
pub fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

/// One agent's checkpoint as read back from the log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointRecord {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    /// The agent that wrote it. Never a person: a checkpoint is one agent's
    /// handover note, and a person with several agents has one per agent.
    pub agent: SenatorName,
    /// The summary line.
    pub body: String,
    pub checkpoint: CheckpointPayload,
}

impl CheckpointRecord {
    /// The record of a checkpoint event from an agent with a readable
    /// payload. The server takes a checkpoint from an agent only, so its
    /// `from` is one.
    pub fn of(event: &Event) -> Option<Self> {
        let agent = event.from.as_senator()?;
        Some(Self {
            seq: event.seq,
            ts: event.ts,
            agent: agent.clone(),
            body: event.body.clone(),
            checkpoint: CheckpointPayload::of(event)?,
        })
    }
}

/// Folds a committee's log into each agent's latest checkpoint. Like
/// [`BriefBuilder`](crate::BriefBuilder), it ignores events at or below its
/// head, so overlapping pages are harmless.
#[derive(Debug, Clone, Default)]
pub struct Checkpoints {
    head: Seq,
    latest: BTreeMap<SenatorName, CheckpointRecord>,
}

impl Checkpoints {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn head(&self) -> Seq {
        self.head
    }

    pub fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        if let Some(record) = CheckpointRecord::of(event) {
            self.latest.insert(record.agent.clone(), record);
        }
    }

    /// That agent's latest. Another agent of the same person does not stand
    /// in for it.
    pub fn latest(&self, agent: &SenatorName) -> Option<&CheckpointRecord> {
        self.latest.get(agent)
    }

    /// Every agent's latest, by path, so one person's agents are together.
    pub fn all(&self) -> impl Iterator<Item = &CheckpointRecord> {
        self.latest.values()
    }
}

/// Where an agent is, as the server sees it (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    Online,
    Disconnected,
    Absent,
    TakenOver,
    Reassigned,
}

/// A seat belongs in the current roster while the server reports it online
/// or a local hosted process is running.
pub fn roster_active(presence: Option<PresenceState>, hosted_running: bool) -> bool {
    hosted_running || presence == Some(PresenceState::Online)
}

impl PresenceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Disconnected => "disconnected",
            Self::Absent => "absent",
            Self::TakenOver => "taken_over",
            Self::Reassigned => "reassigned",
        }
    }

    /// The state in words, as an agent group and the brief show it.
    pub fn label(self, locale: Locale) -> &'static str {
        match self {
            Self::Online => locale.pick("在线", "online"),
            Self::Disconnected => locale.pick("失联", "disconnected"),
            Self::Absent => locale.pick("缺席", "absent"),
            Self::TakenOver => locale.pick("由 Speaker 接管", "taken over by the Speaker"),
            Self::Reassigned => locale.pick("已改派", "reassigned"),
        }
    }

    /// Not online: its directed events wait (or the Speaker covers them).
    pub fn is_away(self) -> bool {
        self != Self::Online
    }
}

impl fmt::Display for PresenceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `N events behind`, in words.
pub(crate) fn lag_words(lag: u64, locale: Locale) -> String {
    match locale {
        Locale::Zh => format!("落后 {lag} 条事件"),
        Locale::En if lag == 1 => "1 event behind".to_string(),
        Locale::En => format!("{lag} events behind"),
    }
}

/// `meta["presence"]` of a [`Kind::Presence`] event. Server-written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresencePayload {
    pub agent: SenatorName,
    pub state: PresenceState,
    /// When the state began.
    pub since: DateTime<Utc>,
    /// The committee's head when the state began.
    pub since_seq: Seq,
    /// The state it left, if the server knew one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<PresenceState>,
    /// The agent's last heartbeat (or stream opening) the server saw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    /// The delivered cursor its bridge last reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Seq>,
    /// Events behind: `since_seq - cursor`, when a cursor is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag: Option<u64>,
    /// `reassigned`: the agent that has the work now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_owner: Option<SenatorName>,
    /// Away on purpose: its terminal was paused (`senate session pause`)
    /// and is expected back. The state is still `absent`, so a client that
    /// does not know the flag treats it as any absent agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
}

impl PresencePayload {
    pub const META_KEY: &'static str = meta::PRESENCE;

    /// The payload of a `presence` event, if it is one and it parses.
    pub fn of(event: &Event) -> Option<Self> {
        if event.kind != Kind::Presence {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::PRESENCE)?.clone()).ok()
    }

    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// The transition in one line, e.g. `[senator::claude] 失联` or
    /// `[senator::copilot] online · 14 events behind`. Rendered when the
    /// event is shown; it is not what the event stores.
    pub fn line(&self, locale: Locale) -> String {
        presence_line(
            &self.agent,
            self.state,
            self.paused,
            self.new_owner.as_ref(),
            self.lag,
            locale,
        )
    }
}

/// The state in words, `paused` for an absent agent paused on purpose.
pub fn presence_label(state: PresenceState, paused: bool, locale: Locale) -> &'static str {
    if paused && state == PresenceState::Absent {
        locale.pick("已暂停", "paused")
    } else {
        state.label(locale)
    }
}

fn presence_line(
    agent: &SenatorName,
    state: PresenceState,
    paused: bool,
    new_owner: Option<&SenatorName>,
    lag: Option<u64>,
    locale: Locale,
) -> String {
    let mut line = format!(
        "{} {}",
        Address::senator(agent).in_text(),
        presence_label(state, paused, locale)
    );
    if let Some(owner) = new_owner {
        line.push_str(&format!(" → {}", Address::senator(owner).in_text()));
    }
    if let Some(lag) = lag.filter(|l| *l > 0) {
        line.push_str(&format!(" · {}", lag_words(lag, locale)));
    }
    line
}

/// The `presence` event for one transition, `from: committee`, with an id
/// derived from (committee, agent, state, since): the same transition
/// written twice (a retried append, a restarted writer) is stored once. Its
/// body is empty; see the module docs.
pub fn presence_event(committee: CommitteeId, payload: &PresencePayload) -> NewEvent {
    let since_ms = u64::try_from(payload.since.timestamp_millis()).unwrap_or(0);
    let key = format!(
        "presence/{committee}/{}/{}/{since_ms}",
        payload.agent, payload.state
    );
    NewEvent::new(committee, Kind::Presence, Address::Committee, "")
        .with_id(EventId::derived(since_ms, &key))
        .with_meta(meta::PRESENCE, payload.value())
}

/// One agent's presence on a committee: its latest state and, from the
/// server, how far behind its bridge is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPresence {
    pub agent: SenatorName,
    pub state: PresenceState,
    pub since: DateTime<Utc>,
    /// The committee's head when the state began.
    pub since_seq: Seq,
    /// The `presence` event that set the state; `None` for a state the
    /// server holds but has not written yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<Seq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    /// The bridge's delivered cursor, from its latest heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<Seq>,
    /// Events behind the committee's head now (the server's view), or at the
    /// transition (a view folded from the log).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_owner: Option<SenatorName>,
    /// The seat's duty ([`crate::duty`]), filled by whoever lists
    /// presence from the committee's log; no `presence` event carries it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<String>,
    /// Paused, not gone; see [`PresencePayload::paused`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
}

impl AgentPresence {
    pub fn from_payload(payload: &PresencePayload, seq: Option<Seq>) -> Self {
        Self {
            agent: payload.agent.clone(),
            state: payload.state,
            since: payload.since,
            since_seq: payload.since_seq,
            seq,
            last_seen: payload.last_seen,
            cursor: payload.cursor,
            lag: payload.lag,
            new_owner: payload.new_owner.clone(),
            duty: None,
            paused: payload.paused,
        }
    }

    /// Where the agent is now, in one line; see [`PresencePayload::line`].
    pub fn line(&self, locale: Locale) -> String {
        presence_line(
            &self.agent,
            self.state,
            self.paused,
            self.new_owner.as_ref(),
            self.lag,
            locale,
        )
    }
}

/// Folds a committee's log into each agent's latest presence. Only
/// `presence` events from `committee` count (the server's); overlapping
/// pages are ignored like [`Checkpoints`].
#[derive(Debug, Clone, Default)]
pub struct PresenceBoard {
    head: Seq,
    agents: BTreeMap<SenatorName, AgentPresence>,
    seats: CommitteeSeats,
}

impl PresenceBoard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn head(&self) -> Seq {
        self.head
    }

    pub fn seats(&self) -> &CommitteeSeats {
        &self.seats
    }

    pub fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        if event.from != Address::Committee {
            return;
        }
        if let Some(change) = CommitteeSeatChange::of(event) {
            self.seats.apply(&change, event.seq);
            match change.action {
                CommitteeSeatAction::Unseat => {
                    self.agents.remove(&change.senator);
                }
                CommitteeSeatAction::Join => {
                    self.agents
                        .entry(change.senator.clone())
                        .or_insert_with(|| {
                            AgentPresence::from_payload(
                                &change.joined_presence(event),
                                Some(event.seq),
                            )
                        });
                }
            }
        } else if let Some(p) = PresencePayload::of(event)
            && !self.seats.is_unseated(&p.agent)
        {
            self.agents.insert(
                p.agent.clone(),
                AgentPresence::from_payload(&p, Some(event.seq)),
            );
        }
    }

    pub fn get(&self, agent: &SenatorName) -> Option<&AgentPresence> {
        self.agents.get(agent)
    }

    /// Whether the log says this agent is online. An agent with no presence
    /// event is not: the log has not seen it leave, and has not seen it at
    /// all. (The server's live view knows a first `online` the log does
    /// not; see the module docs.)
    pub fn is_present(&self, agent: &SenatorName) -> bool {
        self.get(agent)
            .is_some_and(|p| p.state == PresenceState::Online)
    }

    /// Every agent with a presence, by path.
    pub fn agents(&self) -> Vec<AgentPresence> {
        self.agents.values().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Name;
    use crate::roster::{DelegationPresence, Principal, Roster, SeatedAgent};
    use crate::testing::assert_senate_vocabulary;

    /// `paused` is additive: an older client, which knows only the fields
    /// below, still parses a paused presence as plain `absent`; and a
    /// presence written before the flag parses as not paused.
    #[test]
    fn roster_activity_comes_from_presence_or_a_running_host() {
        assert!(roster_active(Some(PresenceState::Online), false));
        assert!(roster_active(None, true));
        assert!(roster_active(Some(PresenceState::Disconnected), true));
        for inactive in [
            None,
            Some(PresenceState::Disconnected),
            Some(PresenceState::Absent),
            Some(PresenceState::TakenOver),
            Some(PresenceState::Reassigned),
        ] {
            assert!(!roster_active(inactive, false));
        }
    }

    #[test]
    fn paused_is_additive_on_the_wire() {
        #[derive(Deserialize)]
        struct OlderPresence {
            agent: SenatorName,
            state: PresenceState,
        }
        let paused = AgentPresence {
            agent: "claude".parse().unwrap(),
            state: PresenceState::Absent,
            since: Utc::now(),
            since_seq: Seq(4),
            seq: Some(Seq(5)),
            last_seen: None,
            cursor: None,
            lag: None,
            new_owner: None,
            duty: None,
            paused: true,
        };
        let json = serde_json::to_value(&paused).unwrap();
        assert_eq!(json["paused"], true);
        assert_eq!(json["state"], "absent");
        let older: OlderPresence = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(older.state, PresenceState::Absent);
        assert_eq!(older.agent.to_string(), "claude");

        let mut before = json;
        before.as_object_mut().unwrap().remove("paused");
        let parsed: AgentPresence = serde_json::from_value(before).unwrap();
        assert!(!parsed.paused);
        let plain = AgentPresence {
            paused: false,
            ..paused.clone()
        };
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("paused")
                .is_none()
        );
        assert_eq!(
            paused.line(Locale::En),
            "[senator::claude] paused",
            "shown paused"
        );
    }

    fn path(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn agent(s: &str) -> Address {
        Address::Senator(path(s))
    }

    fn c() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn payload(files: CheckpointFiles) -> CheckpointPayload {
        CheckpointPayload {
            branch: "senate/refund-v2/alice".into(),
            commit: "a41f9c2e0000000000000000000000000000beef".into(),
            pushed: true,
            remote_url: Some("git@example.com:acme/pay.git".into()),
            files,
            session_transcript: true,
            trigger: CheckpointTrigger::Tool,
        }
    }

    fn checkpoint(seq: u64, from: Address, body: &str) -> Event {
        NewEvent::new(c(), Kind::Checkpoint, from, body)
            .with_meta(
                meta::CHECKPOINT,
                payload(CheckpointFiles::default()).value(),
            )
            .into_event(Seq(seq), Utc::now())
    }

    #[test]
    fn checkpoint_round_trips_and_is_capped() {
        let p = payload(CheckpointFiles {
            progress: Some("- done: idem keys".into()),
            decisions: None,
            open: Some("- seq 112".into()),
        });
        assert!(p.check().is_ok());
        assert_eq!(p.at(), "senate/refund-v2/alice@a41f9c2");
        let json = p.value();
        assert_eq!(json["trigger"], "tool");
        assert!(json["files"].get("decisions").is_none());
        let e = NewEvent::new(c(), Kind::Checkpoint, agent("claude"), "s")
            .with_meta(meta::CHECKPOINT, json)
            .into_event(Seq(3), Utc::now());
        assert_eq!(CheckpointPayload::of(&e), Some(p.clone()));
        let record = CheckpointRecord::of(&e).unwrap();
        assert_eq!(record.agent, path("claude"));

        let big = payload(CheckpointFiles {
            progress: Some("x".repeat(MAX_CHECKPOINT_BYTES)),
            ..CheckpointFiles::default()
        });
        assert!(big.check().unwrap_err().contains("cap"));
        let mut nameless = p;
        nameless.commit = " ".into();
        assert!(nameless.check().is_err());
    }

    #[test]
    fn a_checkpoint_is_a_senators_and_never_the_speakers_or_a_guests() {
        // Whether the senator is an agent is the server's to check: it takes
        // a checkpoint from an agent only.
        assert!(CheckpointRecord::of(&checkpoint(1, Address::Speaker, "x")).is_none());
        assert!(
            CheckpointRecord::of(&checkpoint(1, "guest::chat/u_1".parse().unwrap(), "x")).is_none()
        );
        assert!(CheckpointRecord::of(&checkpoint(1, agent("reviewer"), "x")).is_some());
    }

    #[test]
    fn checkpoints_keep_each_agents_latest_and_one_persons_agents_apart() {
        let mut all = Checkpoints::new();
        all.observe(&checkpoint(1, agent("claude"), "one"));
        all.observe(&checkpoint(2, agent("copilot"), "two"));
        all.observe(&checkpoint(3, agent("claude"), "three"));
        all.observe(&checkpoint(4, agent("codex"), "four"));
        all.observe(&checkpoint(2, agent("claude"), "stale page"));
        assert_eq!(all.latest(&path("claude")).unwrap().body, "three");
        assert_eq!(all.latest(&path("codex")).unwrap().body, "four");
        // The person has no checkpoint; neither does an agent that wrote none.
        assert!(all.latest(&path("alice")).is_none());
        assert!(all.latest(&path("gemini")).is_none());
        assert_eq!(all.all().count(), 3);
        assert_eq!(all.head(), Seq(4));
    }

    fn transition(agent: &str, state: PresenceState, since: DateTime<Utc>) -> PresencePayload {
        PresencePayload {
            paused: false,
            agent: path(agent),
            state,
            since,
            since_seq: Seq(40),
            previous: Some(PresenceState::Online),
            last_seen: Some(since),
            cursor: Some(Seq(26)),
            lag: Some(14),
            new_owner: None,
        }
    }

    #[test]
    fn presence_events_are_idempotent_and_fold() {
        let since = Utc::now();
        let p = transition("claude", PresenceState::Disconnected, since);
        let a = presence_event(c(), &p);
        let b = presence_event(c(), &p);
        assert_eq!(a.id, b.id, "derived ids");
        assert_eq!(a.from, Address::Committee);
        let online = transition("claude", PresenceState::Online, since);
        assert_ne!(presence_event(c(), &online).id, a.id);
        // Another agent of the same person is another transition.
        let codex = transition("codex", PresenceState::Disconnected, since);
        assert_ne!(presence_event(c(), &codex).id, a.id);

        let mut board = PresenceBoard::new();
        board.observe(&a.clone().into_event(Seq(41), since));
        // A senator could never write one, but a non-server presence is ignored.
        let mut forged = presence_event(c(), &online);
        forged.from = agent("claude");
        board.observe(&forged.into_event(Seq(42), since));
        let got = board.get(&path("claude")).unwrap();
        assert_eq!(got.state, PresenceState::Disconnected);
        assert_eq!(got.seq, Some(Seq(41)));
        assert_eq!(board.agents().len(), 1);
        assert!(board.get(&path("alice")).is_none());
        let back: PresencePayload =
            serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        assert_eq!(serde_json::to_value(&p).unwrap()["agent"], "claude");
        assert_eq!(
            serde_json::to_value(PresenceState::TakenOver).unwrap(),
            "taken_over"
        );
    }

    #[test]
    fn a_presence_event_stores_fields_and_no_words() {
        let since = Utc::now();
        let mut p = transition("claude", PresenceState::Reassigned, since);
        p.new_owner = Some(path("copilot"));
        let event = presence_event(c(), &p);
        assert_eq!(event.body, "");
        // The same stored event reads in either language.
        let stored = event.into_event(Seq(41), since);
        let read = PresencePayload::of(&stored).unwrap();
        assert_ne!(read.line(Locale::Zh), read.line(Locale::En));
    }

    #[test]
    fn a_presence_line_is_written_in_each_language() {
        let since = Utc::now();
        let mut p = transition("claude", PresenceState::Disconnected, since);
        assert_eq!(
            p.line(Locale::Zh),
            "[senator::claude] 失联 · 落后 14 条事件"
        );
        assert_eq!(
            p.line(Locale::En),
            "[senator::claude] disconnected · 14 events behind"
        );
        p.lag = Some(1);
        assert_eq!(
            p.line(Locale::En),
            "[senator::claude] disconnected · 1 event behind"
        );
        p.lag = Some(0);
        p.state = PresenceState::Online;
        assert_eq!(p.line(Locale::Zh), "[senator::claude] 在线");
        assert_eq!(p.line(Locale::En), "[senator::claude] online");

        p.state = PresenceState::Reassigned;
        p.new_owner = Some(path("copilot"));
        assert_eq!(
            p.line(Locale::Zh),
            "[senator::claude] 已改派 → [senator::copilot]"
        );
        assert_eq!(
            p.line(Locale::En),
            "[senator::claude] reassigned → [senator::copilot]"
        );
        assert_eq!(
            AgentPresence::from_payload(&p, None).line(Locale::En),
            p.line(Locale::En)
        );

        for state in [
            PresenceState::Online,
            PresenceState::Disconnected,
            PresenceState::Absent,
            PresenceState::TakenOver,
            PresenceState::Reassigned,
        ] {
            p.state = state;
            p.lag = Some(3);
            for locale in Locale::ALL {
                assert_senate_vocabulary(&p.line(locale));
                assert!(!state.label(locale).is_empty());
            }
            assert_ne!(state.label(Locale::Zh), state.label(Locale::En));
        }
        assert_eq!(
            PresenceState::TakenOver.label(Locale::Zh),
            "由 Speaker 接管"
        );
        assert_eq!(
            PresenceState::TakenOver.label(Locale::En),
            "taken over by the Speaker"
        );
    }

    #[test]
    fn a_delegation_is_present_while_any_one_agent_is_online() {
        let name = |s: &str| Name::new(s).unwrap();
        let roster = Roster::new(
            vec![
                Principal::new(name("alice")),
                Principal::new(name("bob")),
                Principal::new(name("carol")),
            ],
            vec![
                SeatedAgent::new(path("claude"), name("alice")),
                SeatedAgent::new(path("codex"), name("alice")),
                SeatedAgent::new(path("copilot"), name("bob")),
            ],
        )
        .unwrap();
        let since = Utc::now();
        let mut board = PresenceBoard::new();
        let mut seq = 0;
        let mut put = |board: &mut PresenceBoard, agent: &str, state| {
            seq += 1;
            board.observe(
                &presence_event(c(), &transition(agent, state, since)).into_event(Seq(seq), since),
            );
        };
        put(&mut board, "claude", PresenceState::Absent);
        put(&mut board, "codex", PresenceState::Online);
        put(&mut board, "copilot", PresenceState::Disconnected);

        let present = |a: &SenatorName| board.is_present(a);
        assert_eq!(
            roster.delegation_presence(&name("alice"), present),
            DelegationPresence::Present
        );
        assert_eq!(
            roster.delegation_presence(&name("bob"), present),
            DelegationPresence::Absent
        );
        assert_eq!(
            roster.delegation_presence(&name("carol"), present),
            DelegationPresence::NoAgents
        );
        // The delegation being present says nothing about the one agent.
        assert!(!board.is_present(&path("claude")));
    }
}
