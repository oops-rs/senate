//! The in-process seam for server-side components.
//!
//! The Speaker and any gateway live in the server process and talk to the
//! log through [`Hub`], never through HTTP and never by holding the store.
//! The server implements it over its supervision tree; tests implement it
//! over a vector. Everything a component needs to survive a restart (its
//! cursor, which events it already mirrored) is on this trait, so components
//! hold no state the log cannot rebuild.
//!
//! A hub serves one Senate. [`Hub::roster`] says which (its presider) and
//! who sits in it now; nothing here assumes it is the only level.
//!
//! # Agents are named, never derived
//!
//! Every method that concerns an agent takes that agent's [`SenatorName`].
//! A component that starts from a person (a chat message for "alice's
//! agent") asks the roster which agent that is, and must be ready for one,
//! several or none ([`Roster::agent_of`]). [`Hub::resolve_and_append`] does
//! this for the wildcard address and hands back the candidates when there
//! are several, so the Speaker can pick.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use crate::address::Address;
use crate::api::PresencePage;
use crate::brief::{Brief, BriefBuilder};
use crate::continuity::{
    AgentPresence, CheckpointRecord, Checkpoints, PresenceBoard, PresencePayload, PresenceState,
    presence_event,
};
use crate::event::{Event, Kind, NewEvent};
use crate::ids::{CommitteeId, Name, Seq};
use crate::memory::{MemoryFold, MemoryHit};
use crate::roster::{DelegationPresence, People, ResolveError, Roster};
use crate::senator::SenatorName;
use crate::task::TaskBoard;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("event rejected: {0}")]
    Rejected(String),
    /// A `senator::*` recipient could not be made concrete. For
    /// [`ResolveError::Ambiguous`] the candidates are in the error.
    #[error("recipient not resolved: {0}")]
    Unresolved(#[from] ResolveError),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("hub unavailable: {0}")]
    Unavailable(String),
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Appended {
    pub event: Event,
    /// The event id was already stored; `event` is the original.
    pub duplicate: bool,
}

/// One outbound copy of a log event on a platform. Lets a person's reply in
/// a platform thread be tied back to the log thread it mirrors, and lets a
/// restarted Speaker skip copies it already posted.
///
/// An event may have several copies: a relay's acknowledgement in the
/// person's thread *and* the question as a root in the agent group.
/// `target` names which copy this is and is unique per `(committee, seq,
/// platform)`; the Speaker uses the group it posted into, plus a role suffix
/// when one group gets two copies (e.g. `refund-backend#ack`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorRef {
    pub committee: CommitteeId,
    pub seq: Seq,
    pub platform: String,
    pub target: String,
    pub message_ref: String,
    pub thread_ref: Option<String>,
}

#[async_trait]
pub trait Hub: Send + Sync + 'static {
    /// Append through the committee's single writer, then fan out.
    /// Idempotent on `event.id`.
    ///
    /// The event is stored as given, so it must be storable: an event that
    /// still holds `senator::*` ([`NewEvent::is_resolved`] is
    /// false) is [`HubError::Rejected`]. Use [`Hub::resolve_and_append`] for
    /// an event that may hold one.
    async fn append(&self, event: NewEvent) -> Result<Appended, HubError>;

    /// Stored events with `seq > after`, ascending, at most `limit`.
    async fn read(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, HubError>;

    /// One stored event.
    async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, HubError>;

    /// Every committed event of every committee, in commit order per
    /// committee. A lagging receiver must recover with [`Hub::read`] from
    /// its cursor.
    fn subscribe(&self) -> broadcast::Receiver<Event>;

    /// Committees that have at least one event.
    async fn committees(&self) -> Result<Vec<CommitteeId>, HubError>;

    /// The roster of the Senate this hub serves, as it is now: its presider
    /// (`None` for the top Senate), its people, and the agents seated so
    /// far. Agents are seated as they connect, so this is a snapshot; ask
    /// again rather than keep it.
    async fn roster(&self) -> Result<Roster, HubError>;

    /// Whether this agent's bridge is reachable on the committee: an open
    /// stream, and a heartbeat (or the stream's opening) within the server's
    /// presence grace, so a sleeping machine whose socket never closed reads
    /// as absent. It answers for the one agent named; whether the person can
    /// be reached through *any* agent is [`Hub::delegation_presence`].
    async fn is_agent_connected(&self, committee: &CommitteeId, agent: &SenatorName) -> bool;

    /// Named cursors for server-side consumers (e.g. `speaker`).
    async fn cursor(&self, committee: &CommitteeId, name: &str) -> Result<Seq, HubError>;

    async fn set_cursor(
        &self,
        committee: &CommitteeId,
        name: &str,
        seq: Seq,
    ) -> Result<(), HubError>;

    /// The log's identity. A log that is reset (an in-memory server
    /// restarted, a database restored or recreated) gets a new epoch, and
    /// seq starts over; anything that remembers a seq across the reset (a
    /// cursor, an idempotency key) must include the epoch or be discarded.
    async fn epoch(&self) -> Result<String, HubError>;

    /// Record one copy; idempotent on `(committee, seq, platform, target)`,
    /// first write wins.
    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), HubError>;

    /// Every recorded copy of one event on `platform`.
    async fn mirrors_of_seq(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, HubError>;

    /// The copy that is this platform message; when several events claim it,
    /// the lowest seq.
    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, HubError>;

    /// The agents on `committee`: whom `senator::*` there may reach. The
    /// default is every agent seated in the Senate; a hub that knows which
    /// agents have a seat on the committee (the server: a presence there)
    /// answers with those.
    async fn committee_agents(
        &self,
        committee: &CommitteeId,
    ) -> Result<Vec<SenatorName>, HubError> {
        let _ = committee;
        Ok(self.roster().await?.agents())
    }

    /// The child Senates registered with this one, each with its Speaker
    /// (a principal here). The default is none: a hub that keeps children
    /// (the server) answers with them.
    async fn children(&self) -> Result<Vec<crate::api::ChildSenate>, HubError> {
        Ok(Vec::new())
    }

    /// Every committee's aliases: how people are shown an agent. The
    /// default is none: a hub that keeps aliases (the server) answers with
    /// them.
    async fn aliases(&self) -> Result<crate::alias::CommitteeAliases, HubError> {
        Ok(Default::default())
    }

    /// This Senate's path from the root, when the hub knows it. The
    /// default is `None`: a caller then takes the Senate a complete
    /// qualified name was resolved to as its own.
    fn senate_path(&self) -> Option<crate::senate::SenatePath> {
        None
    }

    /// Resolve every `senator::*` in `event.to` against the agents on the
    /// committee ([`Hub::committee_agents`]), then [`Hub::append`].
    ///
    /// One agent: it goes to that agent. None: to the Senate's one person.
    /// Several: nothing is appended and the error is
    /// [`HubError::Unresolved`] with the candidates; the caller names one
    /// (the Speaker picks by the task and by what each agent is working on,
    /// and asks the person when unsure) and appends again.
    async fn resolve_and_append(&self, mut event: NewEvent) -> Result<Appended, HubError> {
        if event.to.iter().any(Address::is_wildcard) {
            let candidates = self.committee_agents(&event.committee).await?;
            event.to = self
                .roster()
                .await?
                .resolve_recipients_among(&event.to, &candidates)?;
        }
        self.append(event).await
    }

    /// Whether `owner` can be reached through an agent on the committee: a
    /// delegation is present while any one of its agents is connected.
    async fn delegation_presence(
        &self,
        committee: &CommitteeId,
        owner: &Name,
    ) -> Result<DelegationPresence, HubError> {
        let roster = self.roster().await?;
        let agents = roster.agents_of(owner);
        if agents.is_empty() {
            return Ok(DelegationPresence::NoAgents);
        }
        for agent in agents {
            if self.is_agent_connected(committee, agent).await {
                return Ok(DelegationPresence::Present);
            }
        }
        Ok(DelegationPresence::Absent)
    }

    /// The thread rooted at `root`: the root itself (if stored) followed by
    /// every later event (`seq > root`) with `thread == root`, ascending.
    /// The default scans the committee's log; a store with a thread index
    /// should override it.
    async fn thread(&self, committee: &CommitteeId, root: Seq) -> Result<Vec<Event>, HubError> {
        let mut out = Vec::new();
        if let Some(event) = self.get(committee, root).await? {
            out.push(event);
        }
        let mut after = root;
        loop {
            let page = self.read(committee, after, SCAN_PAGE).await?;
            let Some(last) = page.last() else { break };
            after = last.seq;
            out.extend(page.into_iter().filter(|e| e.thread == Some(root)));
        }
        Ok(out)
    }

    /// Every `binding` event of every committee, each committee's in seq
    /// order (committees in [`Hub::committees`] order). Readers decide which
    /// to trust.
    async fn bindings(&self) -> Result<Vec<Event>, HubError> {
        let mut out = Vec::new();
        for committee in self.committees().await? {
            let mut after = Seq::ZERO;
            loop {
                let page = self.read(&committee, after, SCAN_PAGE).await?;
                let Some(last) = page.last() else { break };
                after = last.seq;
                out.extend(page.into_iter().filter(|e| e.kind == Kind::Binding));
            }
        }
        Ok(out)
    }

    /// Up to `limit` events matching `query`, best first. Every
    /// whitespace-separated term of `query` must occur in the body
    /// (case-insensitively); matches are newest first. A store with a text
    /// index may also return near matches after the exact ones, ranked by
    /// similarity. This is the seam an embedding index replaces later.
    async fn search(
        &self,
        committee: &CommitteeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Event>, HubError> {
        let terms = search_terms(query);
        let mut hits = Vec::new();
        let mut after = Seq::ZERO;
        loop {
            let page = self.read(committee, after, SCAN_PAGE).await?;
            let Some(last) = page.last() else { break };
            after = last.seq;
            hits.extend(page.into_iter().filter(|e| matches_terms(&e.body, &terms)));
        }
        hits.reverse();
        hits.truncate(limit);
        Ok(hits)
    }

    /// Up to `limit` of the committee's memory points matching `query`,
    /// best first ([`MemoryFold::search`]); an empty query lists the newest.
    /// Points no longer in force only with `all`. The default scans; the
    /// server searches the fold it keeps with the brief.
    async fn memory_search(
        &self,
        committee: &CommitteeId,
        query: &str,
        limit: usize,
        all: bool,
    ) -> Result<Vec<MemoryHit>, HubError> {
        let fold = scan(self, committee, MemoryFold::new(), MemoryFold::observe).await?;
        Ok(fold.search(query, limit, all))
    }

    /// The committee's brief over its whole log. The default scans; the
    /// server keeps a builder per committee and folds only what is new.
    async fn brief(&self, committee: &CommitteeId) -> Result<Brief, HubError> {
        let builder = scan(
            self,
            committee,
            BriefBuilder::new(committee.clone()),
            BriefBuilder::observe,
        )
        .await?;
        Ok(builder.brief())
    }

    /// The committee's task threads over its whole log. The default scans;
    /// the server keeps a board per committee and folds only what is new.
    async fn tasks(&self, committee: &CommitteeId) -> Result<TaskBoard, HubError> {
        let people = self.roster().await?.people();
        scan(self, committee, TaskBoard::new(people), TaskBoard::observe).await
    }

    /// Each agent's presence on the committee. The default folds the log's
    /// `presence` events, so `lag` is as of each transition; the server
    /// answers from its live state machine, with `lag` measured now.
    async fn presence(&self, committee: &CommitteeId) -> Result<PresencePage, HubError> {
        let board = scan(
            self,
            committee,
            PresenceBoard::new(),
            PresenceBoard::observe,
        )
        .await?;
        let duties = scan(
            self,
            committee,
            crate::duty::DutyFold::new(),
            crate::duty::DutyFold::observe,
        )
        .await?
        .duties();
        let agents = board
            .agents()
            .into_iter()
            .map(|mut p| {
                p.duty = duties.get(&p.agent).cloned();
                p
            })
            .collect();
        Ok(PresencePage {
            head: board.head(),
            agents,
            duties,
        })
    }

    /// This agent's latest checkpoint on the committee, if any. It is that
    /// agent's alone: another agent of the same person has its own. The
    /// default scans; the server keeps a fold per committee.
    async fn latest_checkpoint(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<CheckpointRecord>, HubError> {
        let all = scan(self, committee, Checkpoints::new(), Checkpoints::observe).await?;
        Ok(all.latest(agent).cloned())
    }

    /// The Speaker picked up a directed event for `agent` while it was away:
    /// record `taken_over` (presence is the server's, so in-process
    /// components ask for it here rather than append it). Allowed from
    /// `disconnected` and `absent`; `Ok(None)` when it is already
    /// `taken_over`; [`HubError::Rejected`] when the agent is online, or has
    /// no presence. Returns the `presence` event written.
    ///
    /// The default appends the transition itself; the server routes it
    /// through the committee's presence state machine.
    async fn mark_taken_over(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<Event>, HubError> {
        let board = scan(
            self,
            committee,
            PresenceBoard::new(),
            PresenceBoard::observe,
        )
        .await?;
        let who = Address::senator(agent);
        let Some(current) = board.get(agent) else {
            return Err(HubError::Rejected(format!(
                "{who} has no presence on {committee}"
            )));
        };
        match current.state {
            PresenceState::TakenOver => return Ok(None),
            PresenceState::Disconnected | PresenceState::Absent => {}
            other => {
                return Err(HubError::Rejected(format!(
                    "{who} is {other} on {committee}; only an away agent is taken over"
                )));
            }
        }
        let payload = PresencePayload {
            agent: agent.clone(),
            state: PresenceState::TakenOver,
            since: Utc::now(),
            since_seq: board.head(),
            previous: Some(current.state),
            last_seen: current.last_seen,
            cursor: current.cursor,
            lag: current.cursor.map(|c| board.head().0.saturating_sub(c.0)),
            new_owner: None,
        };
        let appended = self
            .append(presence_event(committee.clone(), &payload))
            .await?;
        Ok(Some(appended.event))
    }

    /// A person (through the Speaker, which has restated the request and
    /// had it confirmed) hands `agent`'s work on the committee to
    /// `new_owner`. Presence is the server's, so in-process components ask
    /// for the transition here rather than append it.
    ///
    /// Both are agents, named in full. "Give alice's work to bob" names two
    /// people; which agent of each is meant is settled against the roster
    /// before this is called.
    ///
    /// - `new_owner` another agent: `reassigned` with
    ///   [`new_owner`](PresencePayload::new_owner), from any state. It
    ///   stands until undone; the old owner's heartbeats do not end it.
    /// - `new_owner` = `agent` itself: the undo, `online` (the lease machine
    ///   carries on from there). [`HubError::Rejected`] when the agent is
    ///   not reassigned.
    ///
    /// `Ok(None)` when it already is so. [`HubError::Rejected`] when either
    /// is a person, or `new_owner` is itself reassigned (no chains). The
    /// `presence` event written is `on_behalf_of` `by`, the person who
    /// asked. The default refuses: only the server keeps presence.
    async fn reassign(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
        new_owner: &SenatorName,
        by: &Name,
    ) -> Result<Option<Event>, HubError> {
        let _ = (agent, new_owner, by);
        Err(HubError::Rejected(format!(
            "this hub cannot reassign work on {committee}; presence is the server's"
        )))
    }
}

/// Why [`reassign_transition`] refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReassignRefused {
    #[error("work moves from an agent to an agent, and {0} is a person")]
    NotAnAgent(SenatorName),
    #[error("{0} is not reassigned")]
    NotReassigned(SenatorName),
    #[error("{0}'s own work is reassigned; hand that back first")]
    OwnerReassigned(SenatorName),
}

impl From<ReassignRefused> for HubError {
    fn from(refused: ReassignRefused) -> Self {
        Self::Rejected(refused.to_string())
    }
}

/// What [`Hub::reassign`] should write, given the agents' presence now:
/// `Ok(None)` when nothing changes. Shared by the server and test hubs so
/// both apply the same rules.
pub fn reassign_transition(
    board: &[AgentPresence],
    people: &People,
    head: Seq,
    agent: &SenatorName,
    new_owner: &SenatorName,
    now: DateTime<Utc>,
) -> Result<Option<PresencePayload>, ReassignRefused> {
    for senator in [agent, new_owner] {
        if people.is_person(senator) {
            return Err(ReassignRefused::NotAnAgent(senator.clone()));
        }
    }
    let current = board.iter().find(|p| p.agent == *agent);
    let undo = new_owner == agent;
    if undo {
        match current {
            Some(p) if p.state == PresenceState::Reassigned => {}
            _ => return Err(ReassignRefused::NotReassigned(agent.clone())),
        }
    } else {
        if board
            .iter()
            .any(|p| p.agent == *new_owner && p.state == PresenceState::Reassigned)
        {
            return Err(ReassignRefused::OwnerReassigned(new_owner.clone()));
        }
        if let Some(p) = current
            && p.state == PresenceState::Reassigned
            && p.new_owner.as_ref() == Some(new_owner)
        {
            return Ok(None);
        }
    }
    Ok(Some(PresencePayload {
        agent: agent.clone(),
        state: if undo {
            PresenceState::Online
        } else {
            PresenceState::Reassigned
        },
        since: now,
        since_seq: head,
        previous: current.map(|p| p.state),
        last_seen: current.and_then(|p| p.last_seen),
        cursor: current.and_then(|p| p.cursor),
        lag: current
            .and_then(|p| p.cursor)
            .map(|c| head.0.saturating_sub(c.0)),
        new_owner: (!undo).then(|| new_owner.clone()),
    }))
}

/// [`presence_event`] for a reassignment, `on_behalf_of` the person who
/// asked for it.
pub fn reassign_event(committee: CommitteeId, payload: &PresencePayload, by: &Name) -> NewEvent {
    presence_event(committee, payload).on_behalf_of(Address::senior(by))
}

/// The lowercased whitespace-separated terms of a search query.
pub fn search_terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

/// Whether `body` contains every term (terms from [`search_terms`]). No terms
/// matches nothing.
pub fn matches_terms(body: &str, terms: &[String]) -> bool {
    if terms.is_empty() {
        return false;
    }
    let body = body.to_lowercase();
    terms.iter().all(|t| body.contains(t.as_str()))
}

/// Page size the default scans read with.
pub const SCAN_PAGE: usize = 500;

/// Fold the whole log of `committee` into `acc` (the scanning defaults).
async fn scan<H: Hub + ?Sized, T: Send>(
    hub: &H,
    committee: &CommitteeId,
    mut acc: T,
    observe: fn(&mut T, &Event),
) -> Result<T, HubError> {
    let mut after = Seq::ZERO;
    loop {
        let page = hub.read(committee, after, SCAN_PAGE).await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        for event in &page {
            observe(&mut acc, event);
        }
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::sync::Mutex;

    use super::*;
    use crate::continuity::{CheckpointFiles, CheckpointPayload, CheckpointTrigger};
    use crate::event::{BindingAction, BindingPayload, GroupRole, meta};
    use crate::roster::{Principal, SeatedAgent};

    fn path(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn c() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    /// A hub over a vector: only the required methods, so every default is
    /// the one under test.
    struct VecHub {
        logs: Mutex<BTreeMap<CommitteeId, Vec<Event>>>,
        roster: Roster,
        connected: Mutex<HashSet<SenatorName>>,
        tx: broadcast::Sender<Event>,
    }

    impl VecHub {
        /// alice runs claude and codex, bob runs copilot, carol none.
        fn new() -> Self {
            Self::with(
                Roster::new(
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
                .unwrap(),
            )
        }

        fn with(roster: Roster) -> Self {
            Self {
                logs: Mutex::default(),
                roster,
                connected: Mutex::default(),
                tx: broadcast::channel(16).0,
            }
        }

        fn connect(&self, agent: &str) {
            self.connected.lock().unwrap().insert(path(agent));
        }
    }

    #[async_trait]
    impl Hub for VecHub {
        async fn append(&self, event: NewEvent) -> Result<Appended, HubError> {
            if !event.is_resolved() {
                return Err(HubError::Rejected("unresolved recipient".into()));
            }
            let mut logs = self.logs.lock().unwrap();
            let log = logs.entry(event.committee.clone()).or_default();
            if let Some(stored) = log.iter().find(|e| e.id == event.id) {
                return Ok(Appended {
                    event: stored.clone(),
                    duplicate: true,
                });
            }
            let stored = event.into_event(Seq(log.len() as u64 + 1), Utc::now());
            log.push(stored.clone());
            let _ = self.tx.send(stored.clone());
            Ok(Appended {
                event: stored,
                duplicate: false,
            })
        }

        async fn read(
            &self,
            committee: &CommitteeId,
            after: Seq,
            limit: usize,
        ) -> Result<Vec<Event>, HubError> {
            let logs = self.logs.lock().unwrap();
            Ok(logs
                .get(committee)
                .into_iter()
                .flatten()
                .filter(|e| e.seq > after)
                .take(limit)
                .cloned()
                .collect())
        }

        async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, HubError> {
            Ok(self
                .read(committee, Seq(seq.0.saturating_sub(1)), 1)
                .await?
                .pop())
        }

        fn subscribe(&self) -> broadcast::Receiver<Event> {
            self.tx.subscribe()
        }

        async fn committees(&self) -> Result<Vec<CommitteeId>, HubError> {
            Ok(self.logs.lock().unwrap().keys().cloned().collect())
        }

        async fn roster(&self) -> Result<Roster, HubError> {
            Ok(self.roster.clone())
        }

        async fn is_agent_connected(&self, _committee: &CommitteeId, agent: &SenatorName) -> bool {
            self.connected.lock().unwrap().contains(agent)
        }

        async fn cursor(&self, _committee: &CommitteeId, _name: &str) -> Result<Seq, HubError> {
            Ok(Seq::ZERO)
        }

        async fn set_cursor(
            &self,
            _committee: &CommitteeId,
            _name: &str,
            _seq: Seq,
        ) -> Result<(), HubError> {
            Ok(())
        }

        async fn epoch(&self) -> Result<String, HubError> {
            Ok("test".into())
        }

        async fn put_mirror(&self, _mirror: MirrorRef) -> Result<(), HubError> {
            Ok(())
        }

        async fn mirrors_of_seq(
            &self,
            _committee: &CommitteeId,
            _seq: Seq,
            _platform: &str,
        ) -> Result<Vec<MirrorRef>, HubError> {
            Ok(Vec::new())
        }

        async fn mirror_of_message(
            &self,
            _platform: &str,
            _message_ref: &str,
        ) -> Result<Option<MirrorRef>, HubError> {
            Ok(None)
        }
    }

    fn ev(kind: Kind, from: &str, body: &str) -> NewEvent {
        NewEvent::new(c(), kind, addr(from), body)
    }

    fn at(agent: &str, state: PresenceState, owner: Option<&str>) -> AgentPresence {
        AgentPresence {
            agent: path(agent),
            state,
            since: Utc::now(),
            since_seq: Seq(1),
            seq: Some(Seq(1)),
            last_seen: None,
            cursor: Some(Seq(4)),
            lag: None,
            new_owner: owner.map(path),
            duty: None,
        }
    }

    #[test]
    fn reassignment_rules() {
        let people = People::new([name("alice"), name("bob")]);
        let now = Utc::now();
        let alice = path("claude");
        let bob = path("copilot");
        // From any state, also with no presence known.
        let p = reassign_transition(&[], &people, Seq(9), &alice, &bob, now)
            .unwrap()
            .unwrap();
        assert_eq!(p.state, PresenceState::Reassigned);
        assert_eq!(p.new_owner, Some(bob.clone()));
        let online = [at("claude", PresenceState::Online, None)];
        let p = reassign_transition(&online, &people, Seq(9), &alice, &bob, now)
            .unwrap()
            .unwrap();
        assert_eq!(p.previous, Some(PresenceState::Online));
        assert_eq!(p.lag, Some(5));
        // Already so; undo; nothing to undo; no chains.
        let moved = [at("claude", PresenceState::Reassigned, Some("copilot"))];
        assert_eq!(
            reassign_transition(&moved, &people, Seq(9), &alice, &bob, now),
            Ok(None)
        );
        let back = reassign_transition(&moved, &people, Seq(9), &alice, &alice, now)
            .unwrap()
            .unwrap();
        assert_eq!(back.state, PresenceState::Online);
        assert_eq!(back.new_owner, None);
        assert_eq!(
            reassign_transition(&online, &people, Seq(9), &alice, &alice, now),
            Err(ReassignRefused::NotReassigned(alice.clone()))
        );
        let bob_moved = [at("copilot", PresenceState::Reassigned, Some("aider"))];
        assert_eq!(
            reassign_transition(&bob_moved, &people, Seq(9), &alice, &bob, now),
            Err(ReassignRefused::OwnerReassigned(bob.clone()))
        );
    }

    #[test]
    fn work_is_reassigned_between_agents_named_in_full() {
        let people = People::new([name("alice"), name("bob")]);
        let now = Utc::now();
        let alice = path("claude");
        // A person is not an agent, on either side.
        assert_eq!(
            reassign_transition(&[], &people, Seq(9), &alice, &path("bob"), now),
            Err(ReassignRefused::NotAnAgent(path("bob")))
        );
        assert_eq!(
            reassign_transition(&[], &people, Seq(9), &path("alice"), &path("copilot"), now),
            Err(ReassignRefused::NotAnAgent(path("alice")))
        );
        // One of a person's agents may hand its work to another of theirs,
        // and that is not an undo.
        let p = reassign_transition(&[], &people, Seq(9), &alice, &path("codex"), now)
            .unwrap()
            .unwrap();
        assert_eq!(p.state, PresenceState::Reassigned);
        assert_eq!(p.new_owner, Some(path("codex")));
        // A sibling being reassigned does not make this agent so.
        let sibling = [at("codex", PresenceState::Reassigned, Some("copilot"))];
        assert_eq!(
            reassign_transition(&sibling, &people, Seq(9), &alice, &alice, now),
            Err(ReassignRefused::NotReassigned(alice.clone()))
        );

        let event = reassign_event(c(), &p, &name("pm"));
        assert_eq!(event.on_behalf_of, Some(addr("senator::pm")));
        assert_eq!(event.from, Address::Committee);
    }

    #[tokio::test]
    async fn the_wildcard_is_resolved_before_anything_is_appended() {
        let question = || ev(Kind::Question, "senator::carol", "?");

        // One agent on the committee: it goes to that agent.
        let one = VecHub::with(
            Roster::new(
                vec![Principal::new(name("carol"))],
                vec![SeatedAgent::new(path("copilot"), name("carol"))],
            )
            .unwrap(),
        );
        let appended = one
            .resolve_and_append(question().to(addr("senator::*")))
            .await
            .unwrap();
        assert_eq!(appended.event.to, [addr("senator::copilot")]);
        // None: to the Senate's person.
        let none = VecHub::with(Roster::of_person(name("carol")));
        let appended = none
            .resolve_and_append(question().to(addr("senator::*")))
            .await
            .unwrap();
        assert_eq!(appended.event.to, [addr("senator::carol")]);
        // Several: refused with the candidates, and nothing is stored.
        let hub = VecHub::new();
        let err = hub
            .resolve_and_append(question().to(addr("senator::*")))
            .await
            .unwrap_err();
        match err {
            HubError::Unresolved(ResolveError::Ambiguous { candidates }) => {
                assert_eq!(candidates, [path("claude"), path("codex"), path("copilot")]);
            }
            other => panic!("{other}"),
        }
        assert!(hub.read(&c(), Seq::ZERO, 10).await.unwrap().is_empty());
        // A plain append never stores a wildcard.
        assert!(matches!(
            hub.append(question().to(addr("senator::*"))).await,
            Err(HubError::Rejected(_))
        ));
        assert!(hub.read(&c(), Seq::ZERO, 10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_delegation_is_reachable_through_any_one_connected_agent() {
        let hub = VecHub::new();
        hub.connect("codex");
        assert!(!hub.is_agent_connected(&c(), &path("claude")).await);
        assert_eq!(
            hub.delegation_presence(&c(), &name("alice")).await.unwrap(),
            DelegationPresence::Present
        );
        assert_eq!(
            hub.delegation_presence(&c(), &name("bob")).await.unwrap(),
            DelegationPresence::Absent
        );
        assert_eq!(
            hub.delegation_presence(&c(), &name("carol")).await.unwrap(),
            DelegationPresence::NoAgents
        );
    }

    #[tokio::test]
    async fn thread_and_search_scan_the_log() {
        let hub = VecHub::new();
        let q = hub
            .append(
                ev(Kind::Question, "senator::copilot", "Which HEADER?").to(addr("senator::claude")),
            )
            .await
            .unwrap()
            .event
            .seq;
        hub.append(ev(Kind::Note, "senator::copilot", "unrelated header note"))
            .await
            .unwrap();
        hub.append(ev(Kind::Answer, "senator::claude", "the idempotency header").in_thread(q))
            .await
            .unwrap();

        let thread = hub.thread(&c(), q).await.unwrap();
        assert_eq!(
            thread.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [Seq(1), Seq(3)]
        );
        // Every term, any case, newest first, bounded.
        let hits = hub.search(&c(), "header", 10).await.unwrap();
        assert_eq!(
            hits.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [Seq(3), Seq(2), Seq(1)]
        );
        assert_eq!(hub.search(&c(), "header", 2).await.unwrap().len(), 2);
        let both = hub.search(&c(), "Idempotency header", 10).await.unwrap();
        assert_eq!(both.len(), 1);
        assert!(hub.search(&c(), "   ", 10).await.unwrap().is_empty());

        let tasks = hub.tasks(&c()).await.unwrap();
        assert!(tasks.get(q).unwrap().answered);
        let brief = hub.brief(&c()).await.unwrap();
        assert_eq!(brief.head, Seq(3));
        assert!(brief.open_questions.is_empty());
    }

    #[tokio::test]
    async fn bindings_are_gathered_across_committees() {
        let hub = VecHub::new();
        let other = CommitteeId::new("billing").unwrap();
        let bind = |committee: &CommitteeId, group: &str| {
            NewEvent::new(committee.clone(), Kind::Binding, Address::Speaker, "").with_binding(
                &BindingPayload {
                    action: BindingAction::Bind,
                    platform: "chat".into(),
                    group_id: group.into(),
                    group_name: None,
                    role: GroupRole::People,
                },
            )
        };
        hub.append(bind(&c(), "backend")).await.unwrap();
        hub.append(ev(Kind::Note, "speaker", "x")).await.unwrap();
        hub.append(bind(&other, "finance")).await.unwrap();
        let groups: Vec<String> = hub
            .bindings()
            .await
            .unwrap()
            .iter()
            .filter_map(|e| e.binding())
            .map(|b| b.group_id)
            .collect();
        assert_eq!(groups, ["finance", "backend"]);
    }

    #[tokio::test]
    async fn the_latest_checkpoint_is_the_named_agents_own() {
        let hub = VecHub::new();
        let payload = CheckpointPayload {
            branch: "senate/refund-v2/alice".into(),
            commit: "a41f9c2".into(),
            pushed: true,
            remote_url: None,
            files: CheckpointFiles::default(),
            session_transcript: false,
            trigger: CheckpointTrigger::Commit,
        };
        for (from, body) in [
            ("senator::claude", "claude one"),
            ("senator::codex", "codex one"),
            ("senator::claude", "claude two"),
        ] {
            hub.append(
                ev(Kind::Checkpoint, from, body).with_meta(meta::CHECKPOINT, payload.value()),
            )
            .await
            .unwrap();
        }
        let latest = |agent: &str| {
            let agent = path(agent);
            let hub = &hub;
            async move { hub.latest_checkpoint(&c(), &agent).await.unwrap() }
        };
        assert_eq!(latest("claude").await.unwrap().body, "claude two");
        assert_eq!(latest("codex").await.unwrap().body, "codex one");
        // An agent with none is not covered by its sibling's.
        assert!(latest("copilot").await.is_none());
        assert!(latest("alice").await.is_none());
    }

    #[tokio::test]
    async fn only_an_away_agent_is_taken_over() {
        let hub = VecHub::new();
        let alice = path("claude");
        // No presence at all.
        assert!(matches!(
            hub.mark_taken_over(&c(), &alice).await,
            Err(HubError::Rejected(_))
        ));
        let transition = |agent: &str, state| PresencePayload {
            agent: path(agent),
            state,
            since: Utc::now(),
            since_seq: Seq(0),
            previous: None,
            last_seen: None,
            cursor: Some(Seq(0)),
            lag: None,
            new_owner: None,
        };
        hub.append(presence_event(
            c(),
            &transition("claude", PresenceState::Disconnected),
        ))
        .await
        .unwrap();
        hub.append(presence_event(
            c(),
            &transition("codex", PresenceState::Online),
        ))
        .await
        .unwrap();

        let written = hub.mark_taken_over(&c(), &alice).await.unwrap().unwrap();
        let payload = PresencePayload::of(&written).unwrap();
        assert_eq!(payload.agent, alice);
        assert_eq!(payload.state, PresenceState::TakenOver);
        assert_eq!(payload.previous, Some(PresenceState::Disconnected));
        assert_eq!(payload.lag, Some(2));
        // Already so.
        assert_eq!(hub.mark_taken_over(&c(), &alice).await.unwrap(), None);
        // The sibling is online, and stays its own.
        let err = hub.mark_taken_over(&c(), &path("codex")).await.unwrap_err();
        assert!(
            err.to_string().contains("senator::codex is online"),
            "{err}"
        );
        crate::testing::assert_senate_vocabulary(&err.to_string());

        let page = hub.presence(&c()).await.unwrap();
        assert_eq!(page.head, Seq(3));
        assert_eq!(
            page.agents
                .iter()
                .map(|p| (p.agent.to_string(), p.state))
                .collect::<Vec<_>>(),
            [
                ("claude".to_string(), PresenceState::TakenOver),
                ("codex".to_string(), PresenceState::Online)
            ]
        );
    }

    #[tokio::test]
    async fn a_hub_that_does_not_keep_presence_refuses_to_reassign() {
        let hub = VecHub::new();
        let refused = hub
            .reassign(&c(), &path("claude"), &path("copilot"), &name("pm"))
            .await;
        assert!(matches!(refused, Err(HubError::Rejected(_))));
        assert_eq!(hub.epoch().await.unwrap(), "test");
        assert_eq!(hub.roster().await.unwrap().named_speaker(), None);
    }

    #[test]
    fn search_terms_are_lowercased_and_all_must_match() {
        let terms = search_terms("  Retry  CAP ");
        assert_eq!(terms, ["retry", "cap"]);
        assert!(matches_terms("the retry cap is 3", &terms));
        assert!(!matches_terms("the retry limit is 3", &terms));
        assert!(!matches_terms("anything", &[]));
    }
}
