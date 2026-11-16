//! The in-memory store: tests and `memory:` dev mode. Nothing survives the
//! process.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};
use senate_core::api::Heartbeat;
use senate_core::hub::{matches_terms, search_terms};
use senate_core::{
    Appended, CommitteeId, Event, EventId, Kind, MirrorRef, NewEvent, SenatorPath, Seq,
};

use crate::error::LogError;
use crate::store::{CursorOwner, HeartbeatRecord, LogStore, check_storable, mint_epoch, now};

#[derive(Default)]
struct CommitteeLog {
    /// `events[i].seq == Seq(i + 1)`.
    events: Vec<Event>,
    by_id: HashMap<EventId, Seq>,
    /// Thread root → the seqs of the events naming it, ascending.
    replies: HashMap<Seq, Vec<Seq>>,
    /// Seqs of the `binding` events, ascending.
    bindings: Vec<Seq>,
}

/// `(committee, seq, platform, target)`; the `BTreeMap` order is the
/// tie-break [`LogStore::mirror_of_message`] documents.
type MirrorKey = (CommitteeId, Seq, String, String);

#[derive(Default)]
struct Inner {
    /// A `BTreeMap` so [`LogStore::committees`] comes out sorted.
    committees: BTreeMap<CommitteeId, CommitteeLog>,
    cursors: HashMap<(CommitteeId, String, String), Seq>,
    mirrors: BTreeMap<MirrorKey, MirrorRef>,
    /// `(platform, message_ref)` → the keys of the copies that are that
    /// message, lowest first.
    mirrors_by_message: HashMap<(String, String), BTreeSet<MirrorKey>>,
    heartbeats: HashMap<(CommitteeId, SenatorPath), HeartbeatRecord>,
}

/// A [`LogStore`] over a mutex. Every call is a short critical section with
/// no await inside, so a std mutex is the right tool.
pub struct MemoryStore {
    epoch: String,
    inner: Mutex<Inner>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// An empty log with a freshly minted epoch.
    pub fn new() -> Self {
        Self {
            epoch: mint_epoch(),
            inner: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock cannot leave a half-written event:
        // every mutation below is a single push/insert after validation.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CommitteeLog {
    fn get(&self, seq: Seq) -> Option<&Event> {
        let index = usize::try_from(seq.0).ok()?.checked_sub(1)?;
        self.events.get(index)
    }
}

#[async_trait]
impl LogStore for MemoryStore {
    async fn append(&self, event: NewEvent) -> Result<Appended, LogError> {
        check_storable(&event)?;
        let mut inner = self.lock();
        let log = inner.committees.entry(event.committee.clone()).or_default();
        if let Some(seq) = log.by_id.get(&event.id) {
            let stored = log
                .get(*seq)
                .cloned()
                .ok_or_else(|| LogError::Corrupt(format!("id index points at missing {seq}")))?;
            return Ok(Appended {
                event: stored,
                duplicate: true,
            });
        }
        let seq = Seq(log.events.len() as u64 + 1);
        let stored = event.into_event(seq, now());
        log.by_id.insert(stored.id.clone(), seq);
        if let Some(root) = stored.thread {
            log.replies.entry(root).or_default().push(seq);
        }
        if stored.kind == Kind::Binding {
            log.bindings.push(seq);
        }
        log.events.push(stored.clone());
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
    ) -> Result<Vec<Event>, LogError> {
        let inner = self.lock();
        let Some(log) = inner.committees.get(committee) else {
            return Ok(Vec::new());
        };
        let start = usize::try_from(after.0).unwrap_or(usize::MAX);
        Ok(log.events.iter().skip(start).take(limit).cloned().collect())
    }

    async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, LogError> {
        let inner = self.lock();
        Ok(inner
            .committees
            .get(committee)
            .and_then(|log| log.get(seq))
            .cloned())
    }

    async fn head(&self, committee: &CommitteeId) -> Result<Seq, LogError> {
        let inner = self.lock();
        Ok(inner
            .committees
            .get(committee)
            .map_or(Seq::ZERO, |log| Seq(log.events.len() as u64)))
    }

    async fn committees(&self) -> Result<Vec<CommitteeId>, LogError> {
        let inner = self.lock();
        Ok(inner
            .committees
            .iter()
            .filter(|(_, log)| !log.events.is_empty())
            .map(|(committee, _)| committee.clone())
            .collect())
    }

    async fn cursor(
        &self,
        committee: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
    ) -> Result<Option<Seq>, LogError> {
        let inner = self.lock();
        Ok(inner
            .cursors
            .get(&(committee.clone(), owner.key(), name.to_string()))
            .copied())
    }

    async fn set_cursor(
        &self,
        committee: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
        seq: Seq,
    ) -> Result<(), LogError> {
        let mut inner = self.lock();
        inner
            .cursors
            .insert((committee.clone(), owner.key(), name.to_string()), seq);
        Ok(())
    }

    async fn epoch(&self) -> Result<String, LogError> {
        Ok(self.epoch.clone())
    }

    async fn thread(&self, committee: &CommitteeId, root: Seq) -> Result<Vec<Event>, LogError> {
        let inner = self.lock();
        let Some(log) = inner.committees.get(committee) else {
            return Ok(Vec::new());
        };
        let replies = log.replies.get(&root).into_iter().flatten();
        Ok(log
            .get(root)
            .into_iter()
            .chain(
                replies
                    .filter(|seq| **seq > root)
                    .filter_map(|seq| log.get(*seq)),
            )
            .cloned()
            .collect())
    }

    async fn bindings(&self) -> Result<Vec<Event>, LogError> {
        let inner = self.lock();
        Ok(inner
            .committees
            .values()
            .flat_map(|log| log.bindings.iter().filter_map(|seq| log.get(*seq)))
            .cloned()
            .collect())
    }

    async fn search(
        &self,
        committee: &CommitteeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Event>, LogError> {
        let inner = self.lock();
        let Some(log) = inner.committees.get(committee) else {
            return Ok(Vec::new());
        };
        // The same comparison as `Hub::search`'s default.
        let terms = search_terms(query);
        Ok(log
            .events
            .iter()
            .rev()
            .filter(|e| matches_terms(&e.body, &terms))
            .take(limit)
            .cloned()
            .collect())
    }

    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), LogError> {
        let mut inner = self.lock();
        let key = (
            mirror.committee.clone(),
            mirror.seq,
            mirror.platform.clone(),
            mirror.target.clone(),
        );
        if inner.mirrors.contains_key(&key) {
            return Ok(());
        }
        inner
            .mirrors_by_message
            .entry((mirror.platform.clone(), mirror.message_ref.clone()))
            .or_default()
            .insert(key.clone());
        inner.mirrors.insert(key, mirror);
        Ok(())
    }

    async fn mirrors_of_seq(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, LogError> {
        let inner = self.lock();
        let first = (committee.clone(), seq, platform.to_string(), String::new());
        Ok(inner
            .mirrors
            .range(first..)
            .take_while(|((f, s, p, _), _)| f == committee && *s == seq && p == platform)
            .map(|(_, mirror)| mirror.clone())
            .collect())
    }

    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, LogError> {
        let inner = self.lock();
        Ok(inner
            .mirrors_by_message
            .get(&(platform.to_string(), message_ref.to_string()))
            .and_then(BTreeSet::first)
            .and_then(|key| inner.mirrors.get(key))
            .cloned())
    }

    async fn record_heartbeat(
        &self,
        committee: &CommitteeId,
        heartbeat: Heartbeat,
        at: DateTime<Utc>,
    ) -> Result<(), LogError> {
        let mut inner = self.lock();
        inner.heartbeats.insert(
            (committee.clone(), heartbeat.agent.clone()),
            HeartbeatRecord {
                committee: committee.clone(),
                agent: heartbeat.agent.clone(),
                heartbeat,
                at: at.trunc_subsecs(6),
            },
        );
        Ok(())
    }

    async fn last_heartbeat(
        &self,
        committee: &CommitteeId,
        agent: &SenatorPath,
    ) -> Result<Option<HeartbeatRecord>, LogError> {
        let inner = self.lock();
        Ok(inner
            .heartbeats
            .get(&(committee.clone(), agent.clone()))
            .cloned())
    }
}
