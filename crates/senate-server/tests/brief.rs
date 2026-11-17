//! [`Hub::brief`]'s per-committee cache across a log reset.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::*;
use senate_core::api::Heartbeat;
use senate_core::{
    Appended, Brief, CommitteeId, Event, Hub, Kind, MirrorRef, NewEvent, SenatorName, Seq,
};
use senate_log::{CursorOwner, HeartbeatRecord, LogError, LogStore, MemoryStore};

/// A [`MemoryStore`] that can be swapped for a fresh one under a running
/// server: a log reset (new epoch, seq from 1) the server did not see.
#[derive(Default)]
struct Resettable {
    inner: Mutex<Arc<MemoryStore>>,
}

impl Resettable {
    fn reset(&self) {
        *self.inner.lock().unwrap() = Arc::new(MemoryStore::new());
    }

    fn inner(&self) -> Arc<MemoryStore> {
        self.inner.lock().unwrap().clone()
    }
}

#[async_trait]
impl LogStore for Resettable {
    async fn append(&self, event: NewEvent) -> Result<Appended, LogError> {
        self.inner().append(event).await
    }
    async fn epoch(&self) -> Result<String, LogError> {
        self.inner().epoch().await
    }
    async fn thread(&self, f: &CommitteeId, root: Seq) -> Result<Vec<Event>, LogError> {
        self.inner().thread(f, root).await
    }
    async fn bindings(&self) -> Result<Vec<Event>, LogError> {
        self.inner().bindings().await
    }
    async fn search(
        &self,
        f: &CommitteeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Event>, LogError> {
        self.inner().search(f, query, limit).await
    }
    async fn read(
        &self,
        f: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, LogError> {
        self.inner().read(f, after, limit).await
    }
    async fn get(&self, f: &CommitteeId, seq: Seq) -> Result<Option<Event>, LogError> {
        self.inner().get(f, seq).await
    }
    async fn head(&self, f: &CommitteeId) -> Result<Seq, LogError> {
        self.inner().head(f).await
    }
    async fn committees(&self) -> Result<Vec<CommitteeId>, LogError> {
        self.inner().committees().await
    }
    async fn cursor(
        &self,
        f: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
    ) -> Result<Option<Seq>, LogError> {
        self.inner().cursor(f, owner, name).await
    }
    async fn set_cursor(
        &self,
        f: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
        seq: Seq,
    ) -> Result<(), LogError> {
        self.inner().set_cursor(f, owner, name, seq).await
    }
    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), LogError> {
        self.inner().put_mirror(mirror).await
    }
    async fn mirrors_of_seq(
        &self,
        f: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, LogError> {
        self.inner().mirrors_of_seq(f, seq, platform).await
    }
    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, LogError> {
        self.inner().mirror_of_message(platform, message_ref).await
    }
    async fn record_heartbeat(
        &self,
        f: &CommitteeId,
        heartbeat: Heartbeat,
        at: DateTime<Utc>,
    ) -> Result<(), LogError> {
        self.inner().record_heartbeat(f, heartbeat, at).await
    }
    async fn last_heartbeat(
        &self,
        f: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<HeartbeatRecord>, LogError> {
        self.inner().last_heartbeat(f, agent).await
    }
    async fn heartbeats(&self, f: &CommitteeId) -> Result<Vec<HeartbeatRecord>, LogError> {
        self.inner().heartbeats(f).await
    }
}

fn decision(body: &str) -> NewEvent {
    NewEvent::new(committee(), Kind::Decision, person_addr("alice"), body)
}

#[tokio::test]
async fn a_new_epoch_drops_the_cached_brief() {
    let store = Arc::new(Resettable::default());
    let server = TestServer::start_with(store.clone(), None).await;
    let hub = server.running.hub.clone();
    let f = committee();
    for body in ["old one", "old two", "old three"] {
        hub.append(decision(body)).await.unwrap();
    }
    let before = hub.brief(&f).await.unwrap();
    assert_eq!(before.head, Seq(3));
    assert_eq!(before.decisions.len(), 3);

    // Seq starts over in the new log: a builder kept from the old one would
    // skip seq 1 as already folded.
    store.reset();
    hub.append(decision("new one")).await.unwrap();
    let after = hub.brief(&f).await.unwrap();
    let log = hub.read(&f, Seq::ZERO, 100).await.unwrap();
    assert_eq!(after, Brief::from_events(f.clone(), &log));
    assert_eq!(after.head, Seq(1));
    assert_eq!(
        after
            .decisions
            .iter()
            .map(|d| d.body.as_str())
            .collect::<Vec<_>>(),
        ["new one"]
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_briefs_agree_with_a_full_scan() {
    let server = TestServer::start().await;
    let hub = server.running.hub.clone();
    let f = committee();
    let mut tasks = Vec::new();
    for n in 0..20 {
        hub.append(decision(&format!("d{n}"))).await.unwrap();
        let hub = hub.clone();
        let f = f.clone();
        tasks.push(tokio::spawn(async move { hub.brief(&f).await.unwrap() }));
    }
    for task in tasks {
        let brief = task.await.unwrap();
        let log = hub
            .read(&f, Seq::ZERO, brief.head.0 as usize)
            .await
            .unwrap();
        assert_eq!(brief, Brief::from_events(f.clone(), &log), "a prefix");
    }
    let log = hub.read(&f, Seq::ZERO, 100).await.unwrap();
    assert_eq!(
        hub.brief(&f).await.unwrap(),
        Brief::from_events(f.clone(), &log)
    );
    server.running.shutdown().await.unwrap();
}
