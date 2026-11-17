//! An in-memory [`Hub`] for the Speaker's tests. Appends never yield, which
//! is what lets the lag test overrun a small broadcast buffer on purpose.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use senate_core::{
    Appended, CommitteeId, Event, EventId, Hub, HubError, MirrorRef, Name, NewEvent, Roster,
    SenatorName, Seq,
};
use tokio::sync::broadcast;

#[derive(Default)]
struct State {
    logs: BTreeMap<CommitteeId, Vec<Event>>,
    cursors: HashMap<(CommitteeId, String), Seq>,
    mirrors: Vec<MirrorRef>,
    connected: HashSet<(CommitteeId, SenatorName)>,
    roster: Roster,
    last_ts: Option<DateTime<Utc>>,
    gets: usize,
    reads: usize,
    append_faults: HashMap<String, VecDeque<HubError>>,
    append_attempts: HashMap<String, usize>,
}

/// Each one is a fresh log with its own epoch, as a restarted `memory:`
/// server would be.
pub(crate) struct MemoryHub {
    state: Mutex<State>,
    tx: broadcast::Sender<Event>,
    epoch: String,
}

impl MemoryHub {
    pub(crate) fn new() -> Arc<Self> {
        Self::with_capacity(1024)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Arc<Self> {
        let (tx, _) = broadcast::channel(capacity);
        Arc::new(Self {
            state: Mutex::new(State {
                roster: super::rig::roster(),
                ..State::default()
            }),
            tx,
            epoch: EventId::new().to_string(),
        })
    }

    pub(crate) fn epoch_now(&self) -> &str {
        &self.epoch
    }

    /// Point reads (`get`) and page reads (`read`) since the last reset.
    pub(crate) fn gets(&self) -> usize {
        self.lock().gets
    }

    pub(crate) fn reads(&self) -> usize {
        self.lock().reads
    }

    pub(crate) fn reset_counts(&self) {
        let mut state = self.lock();
        state.gets = 0;
        state.reads = 0;
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn events(&self, committee: &CommitteeId) -> Vec<Event> {
        self.lock().logs.get(committee).cloned().unwrap_or_default()
    }

    /// `agent`'s own connection, as its bridge's stream would make it.
    /// Seats it first, as the server does on first contact.
    pub(crate) fn connect(&self, committee: &CommitteeId, agent: &SenatorName) {
        let mut state = self.lock();
        let _ = state.roster.seat_again(agent.clone());
        state.connected.insert((committee.clone(), agent.clone()));
    }

    /// Seat another agent in the Senate under `owner`, the person who runs
    /// it (its first contact), unconnected.
    pub(crate) fn seat(&self, agent: &SenatorName, owner: &str) {
        let owner = Name::new(owner).expect("handle");
        self.lock()
            .roster
            .seat(agent.clone(), &owner)
            .expect("seatable");
    }

    /// Replace the roster, e.g. with one that has guests' people missing.
    pub(crate) fn set_roster(&self, roster: Roster) {
        self.lock().roster = roster;
    }

    pub(crate) fn disconnect(&self, committee: &CommitteeId, agent: &SenatorName) {
        self.lock()
            .connected
            .remove(&(committee.clone(), agent.clone()));
    }

    pub(crate) fn fail_append(&self, body: &str, error: HubError) {
        self.lock()
            .append_faults
            .entry(body.to_owned())
            .or_default()
            .push_back(error);
    }

    pub(crate) fn append_attempts(&self, body: &str) -> usize {
        self.lock()
            .append_attempts
            .get(body)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn forget_mirror(&self, committee: &CommitteeId, seq: Seq) {
        self.lock()
            .mirrors
            .retain(|m| !(&m.committee == committee && m.seq == seq));
    }
}

#[async_trait]
impl Hub for MemoryHub {
    async fn append(&self, event: NewEvent) -> Result<Appended, HubError> {
        let stored = {
            let mut state = self.lock();
            *state.append_attempts.entry(event.body.clone()).or_default() += 1;
            if let Some(error) = state
                .append_faults
                .get_mut(&event.body)
                .and_then(VecDeque::pop_front)
            {
                return Err(error);
            }
            if let Some(existing) = state
                .logs
                .get(&event.committee)
                .and_then(|log| log.iter().find(|e| e.id == event.id))
            {
                return Ok(Appended {
                    event: existing.clone(),
                    duplicate: true,
                });
            }
            let mut ts = Utc::now();
            if let Some(last) = state.last_ts
                && ts <= last
            {
                ts = last + chrono::Duration::microseconds(1);
            }
            state.last_ts = Some(ts);
            let log = state.logs.entry(event.committee.clone()).or_default();
            let stored = event.into_event(Seq(log.len() as u64 + 1), ts);
            log.push(stored.clone());
            stored
        };
        // No receivers is fine: nothing is listening yet.
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
        let mut state = self.lock();
        state.reads += 1;
        Ok(state
            .logs
            .get(committee)
            .map(|log| {
                log.iter()
                    .filter(|e| e.seq > after)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, HubError> {
        let mut state = self.lock();
        state.gets += 1;
        Ok(state
            .logs
            .get(committee)
            .and_then(|log| log.iter().find(|e| e.seq == seq).cloned()))
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    async fn committees(&self) -> Result<Vec<CommitteeId>, HubError> {
        Ok(self.lock().logs.keys().cloned().collect())
    }

    async fn roster(&self) -> Result<Roster, HubError> {
        Ok(self.lock().roster.clone())
    }

    /// The agents connected on `committee`, in roster order: like the
    /// server, which counts the agents with a connection there.
    async fn committee_agents(
        &self,
        committee: &CommitteeId,
    ) -> Result<Vec<SenatorName>, HubError> {
        let state = self.lock();
        Ok(state
            .roster
            .agents()
            .into_iter()
            .filter(|a| state.connected.contains(&(committee.clone(), a.clone())))
            .collect())
    }

    async fn is_agent_connected(&self, committee: &CommitteeId, agent: &SenatorName) -> bool {
        self.lock()
            .connected
            .contains(&(committee.clone(), agent.clone()))
    }

    async fn cursor(&self, committee: &CommitteeId, name: &str) -> Result<Seq, HubError> {
        Ok(self
            .lock()
            .cursors
            .get(&(committee.clone(), name.to_owned()))
            .copied()
            .unwrap_or_default())
    }

    async fn set_cursor(
        &self,
        committee: &CommitteeId,
        name: &str,
        seq: Seq,
    ) -> Result<(), HubError> {
        self.lock()
            .cursors
            .insert((committee.clone(), name.to_owned()), seq);
        Ok(())
    }

    async fn epoch(&self) -> Result<String, HubError> {
        Ok(self.epoch.clone())
    }

    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), HubError> {
        let mut state = self.lock();
        let exists = state.mirrors.iter().any(|m| {
            m.committee == mirror.committee
                && m.seq == mirror.seq
                && m.platform == mirror.platform
                && m.target == mirror.target
        });
        if !exists {
            state.mirrors.push(mirror);
        }
        Ok(())
    }

    async fn mirrors_of_seq(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, HubError> {
        Ok(self
            .lock()
            .mirrors
            .iter()
            .filter(|m| &m.committee == committee && m.seq == seq && m.platform == platform)
            .cloned()
            .collect())
    }

    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, HubError> {
        Ok(self
            .lock()
            .mirrors
            .iter()
            .filter(|m| m.platform == platform && m.message_ref == message_ref)
            .min_by_key(|m| m.seq)
            .cloned())
    }

    /// The server's rules ([`senate_core::reassign_transition`]) over the
    /// presence folded from the log.
    async fn reassign(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
        new_owner: &SenatorName,
        by: &Name,
    ) -> Result<Option<Event>, HubError> {
        let page = self.presence(committee).await?;
        let people = self.lock().roster.people();
        let payload = senate_core::reassign_transition(
            &page.agents,
            &people,
            page.head,
            agent,
            new_owner,
            Utc::now(),
        )
        .map_err(HubError::from)?;
        let Some(payload) = payload else {
            return Ok(None);
        };
        let appended = self
            .append(senate_core::reassign_event(committee.clone(), &payload, by))
            .await?;
        Ok(Some(appended.event))
    }
}
