//! The supervision tree: lazy writers, restart from the log, shutdown.

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use common::*;
use ractor::ActorId;
use reqwest::{Method, StatusCode};
use senate_core::api::{AppendResponse, Heartbeat, events_path};
use senate_core::{
    Address, Appended, CommitteeId, Event, Hub, Kind, MirrorRef, NewEvent, SenatorName, Seq, meta,
};
use senate_log::{CursorOwner, HeartbeatRecord, LogError, LogStore, MemoryStore};
use tokio::sync::broadcast::error::TryRecvError;

const NO_FAULT: u8 = 0;
/// The next append commits, then the writer panics: the worst moment for it
/// to die, in the log but not fanned out.
const CRASH_AFTER_COMMIT: u8 = 1;
/// The next append commits, then reports a failure, as when the connection
/// drops before the commit's acknowledgement arrives.
const LOSE_ACK_AFTER_COMMIT: u8 = 2;

/// A [`MemoryStore`] that injects one fault into the next append once armed.
#[derive(Default)]
struct Faulty {
    inner: MemoryStore,
    fault: AtomicU8,
}

impl Faulty {
    fn arm(&self, fault: u8) {
        self.fault.store(fault, Ordering::SeqCst);
    }
}

#[async_trait]
impl LogStore for Faulty {
    async fn append(&self, event: NewEvent) -> Result<Appended, LogError> {
        let result = self.inner.append(event).await;
        match self.fault.swap(NO_FAULT, Ordering::SeqCst) {
            CRASH_AFTER_COMMIT => panic!("injected crash after commit"),
            LOSE_ACK_AFTER_COMMIT => Err(LogError::Database(sqlx::Error::Io(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "injected lost ack",
            )))),
            _ => result,
        }
    }
    async fn epoch(&self) -> Result<String, LogError> {
        self.inner.epoch().await
    }
    async fn thread(&self, f: &CommitteeId, root: Seq) -> Result<Vec<Event>, LogError> {
        self.inner.thread(f, root).await
    }
    async fn bindings(&self) -> Result<Vec<Event>, LogError> {
        self.inner.bindings().await
    }
    async fn search(
        &self,
        f: &CommitteeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Event>, LogError> {
        self.inner.search(f, query, limit).await
    }
    async fn read(
        &self,
        f: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, LogError> {
        self.inner.read(f, after, limit).await
    }
    async fn get(&self, f: &CommitteeId, seq: Seq) -> Result<Option<Event>, LogError> {
        self.inner.get(f, seq).await
    }
    async fn head(&self, f: &CommitteeId) -> Result<Seq, LogError> {
        self.inner.head(f).await
    }
    async fn committees(&self) -> Result<Vec<CommitteeId>, LogError> {
        self.inner.committees().await
    }
    async fn cursor(
        &self,
        f: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
    ) -> Result<Option<Seq>, LogError> {
        self.inner.cursor(f, owner, name).await
    }
    async fn set_cursor(
        &self,
        f: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
        seq: Seq,
    ) -> Result<(), LogError> {
        self.inner.set_cursor(f, owner, name, seq).await
    }
    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), LogError> {
        self.inner.put_mirror(mirror).await
    }
    async fn mirrors_of_seq(
        &self,
        f: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, LogError> {
        self.inner.mirrors_of_seq(f, seq, platform).await
    }
    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, LogError> {
        self.inner.mirror_of_message(platform, message_ref).await
    }
    async fn record_heartbeat(
        &self,
        f: &CommitteeId,
        heartbeat: Heartbeat,
        at: DateTime<Utc>,
    ) -> Result<(), LogError> {
        self.inner.record_heartbeat(f, heartbeat, at).await
    }
    async fn last_heartbeat(
        &self,
        f: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<HeartbeatRecord>, LogError> {
        self.inner.last_heartbeat(f, agent).await
    }
    async fn heartbeats(&self, f: &CommitteeId) -> Result<Vec<HeartbeatRecord>, LogError> {
        self.inner.heartbeats(f).await
    }
}

fn writer_ids(server: &TestServer) -> Vec<ActorId> {
    server
        .running
        .supervisor
        .get_children()
        .iter()
        .map(|c| c.get_id())
        .collect()
}

#[tokio::test]
async fn writers_spawn_lazily_one_per_committee() {
    let server = TestServer::start().await;
    assert!(writer_ids(&server).is_empty());
    // Reads never need the writer.
    let page = server
        .request(Method::GET, &events_path(&committee()), ALICE)
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert!(writer_ids(&server).is_empty());

    server.append("one").await;
    server.append("two").await;
    assert_eq!(writer_ids(&server).len(), 1, "one writer for the committee");
    let other = CommitteeId::new("search").unwrap();
    let mut elsewhere = note("elsewhere");
    elsewhere.committee = other;
    server.running.hub.append(elsewhere).await.unwrap();
    assert_eq!(writer_ids(&server).len(), 2);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_crashed_writer_restarts_and_rebuilds_from_the_log() {
    let store = Arc::new(Faulty::default());
    let server = TestServer::start_with(store.clone(), None).await;
    let hub = server.running.hub.clone();
    let f = committee();
    let alice = path("claude");

    let mut stream = server.stream_as(ALICE, Some("claude"), None, None).await;
    server.append("one").await;
    server.append("two").await;
    // Seq 1 is claude's seat, written when it connected first.
    assert_eq!(stream.seqs(3).await, vec![1, 2, 3]);
    eventually("alice connected", || hub.is_agent_connected(&f, &alice)).await;
    let before = writer_ids(&server);
    assert_eq!(before.len(), 1);

    store.arm(CRASH_AFTER_COMMIT);
    let crashed = note("three");
    let response = server.post_event(ALICE, &crashed).await;
    api_error(response, StatusCode::SERVICE_UNAVAILABLE, "internal").await;

    // The writer's proxies die with it, ending their streams; presence follows.
    assert!(
        stream.event().await.is_none(),
        "stream ends on writer crash"
    );
    eventually("alice disconnected", || async {
        !hub.is_agent_connected(&f, &alice).await
    })
    .await;
    eventually("writer restarted", || async {
        let now = writer_ids(&server);
        now.len() == 1 && now != before
    })
    .await;

    // Seq continues from the log, including the event committed by the
    // append whose writer crashed.
    let four = server.append("four").await;
    assert_eq!(four.seq, Seq(5));
    let retry: AppendResponse = server
        .post_event(ALICE, &crashed)
        .await
        .json()
        .await
        .unwrap();
    assert!(retry.duplicate, "the crashed append had committed");
    assert_eq!(retry.event.seq, Seq(4));

    // Reconnecting replays what the dead stream missed, then goes live.
    let mut resumed = server.stream_as(ALICE, Some("claude"), None, Some(3)).await;
    assert_eq!(resumed.seqs(2).await, vec![4, 5]);
    server.append("five").await;
    assert_eq!(resumed.seqs(1).await, vec![6]);
    eventually("alice reconnected", || hub.is_agent_connected(&f, &alice)).await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_retry_after_a_lost_commit_ack_is_fanned_out_once() {
    let store = Arc::new(Faulty::default());
    let server = TestServer::start_with(store.clone(), None).await;
    let mut subscriber = server.running.hub.subscribe();
    let mut stream = server.stream(BOB, None, None).await;
    server.append("one").await;
    server.append("two").await;
    assert_eq!(stream.seqs(2).await, vec![1, 2]);
    for n in 1..=2 {
        assert_eq!(subscriber.recv().await.unwrap().seq, Seq(n));
    }

    // The commit lands, but the writer only sees an error: nothing to fan
    // out. (A writer fans out before it replies, so any fan-out would already
    // be in the subscriber's queue.)
    store.arm(LOSE_ACK_AFTER_COMMIT);
    let lost = note("three");
    api_error(
        server.post_event(ALICE, &lost).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
    )
    .await;
    assert!(matches!(subscriber.try_recv(), Err(TryRecvError::Empty)));

    // The retry finds it stored, and that is when live consumers get it.
    let retry: AppendResponse = server.post_event(ALICE, &lost).await.json().await.unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.event.seq, Seq(3));
    assert_eq!(subscriber.try_recv().unwrap(), retry.event);
    let live = stream.event().await.unwrap();
    assert_eq!((live.seq, live.body.as_str()), (Seq(3), "three"));

    // Never twice: another retry, then the next event, arrive once each.
    let again: AppendResponse = server.post_event(ALICE, &lost).await.json().await.unwrap();
    assert!(again.duplicate);
    assert!(matches!(subscriber.try_recv(), Err(TryRecvError::Empty)));
    server.append("four").await;
    assert_eq!(subscriber.try_recv().unwrap().seq, Seq(4));
    assert!(matches!(subscriber.try_recv(), Err(TryRecvError::Empty)));
    assert_eq!(stream.seqs(1).await, vec![4]);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_chair_discussion_retry_repairs_delivery_after_a_restart_and_chair_change() {
    let store = Arc::new(Faulty::default());
    let server = TestServer::start_with(store.clone(), None).await;
    let target = CommitteeId::new("ibot").unwrap();
    let open = NewEvent::new(
        target.clone(),
        Kind::Note,
        agent_addr("codex"),
        "target opens",
    );
    assert_eq!(server.post_event(BOB, &open).await.status(), StatusCode::OK);
    let question = NewEvent::new(
        committee(),
        Kind::Question,
        agent_addr("claude"),
        "question",
    )
    .with_meta(meta::INTENDED, "ibot::chair".into());
    store.arm(LOSE_ACK_AFTER_COMMIT);
    api_error(
        server.post_event(ALICE, &question).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
    )
    .await;
    let source = store.get(&committee(), Seq(1)).await.unwrap().unwrap();
    assert!(source.is_chair_discussion());
    assert_eq!(store.head(&target).await.unwrap(), Seq(1));
    server.running.shutdown().await.unwrap();

    let server = TestServer::start_with(store.clone(), None).await;
    let hub = &server.running.hub;
    hub.set_chair(&target, path("carol")).await.unwrap();
    let (a, b) = tokio::join!(
        server.post_event(ALICE, &question),
        server.post_event(ALICE, &question)
    );
    for response in [a, b] {
        let retried: AppendResponse = response.json().await.unwrap();
        assert!(retried.duplicate);
        assert_eq!(retried.event, source);
    }
    let copies = hub.read(&target, Seq(2), 10).await.unwrap();
    let [copy] = &copies[..] else {
        panic!("{copies:?}")
    };
    assert_eq!(copy.to, [agent_addr("codex")]);

    let new_chair = NewEvent::new(
        target.clone(),
        Kind::Answer,
        person_addr("carol"),
        "a local note from the new chair",
    )
    .in_thread(copy.seq);
    assert_eq!(
        server.post_event(CAROL, &new_chair).await.status(),
        StatusCode::OK
    );
    assert_eq!(hub.head(&committee()).await.unwrap(), source.seq);

    let answer =
        NewEvent::new(target, Kind::Answer, agent_addr("codex"), "answer").in_thread(copy.seq);
    store.arm(LOSE_ACK_AFTER_COMMIT);
    api_error(
        server.post_event(BOB, &answer).await,
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
    )
    .await;
    assert_eq!(hub.head(&committee()).await.unwrap(), source.seq);
    for _ in 0..2 {
        let retried: AppendResponse = server.post_event(BOB, &answer).await.json().await.unwrap();
        assert!(retried.duplicate);
    }
    let replies = hub.thread(&committee(), source.seq).await.unwrap();
    let [_, reply] = &replies[..] else {
        panic!("{replies:?}")
    };
    assert_eq!(reply.from, Address::Committee);
    assert_eq!(reply.to, [agent_addr("claude")]);
    assert_eq!(reply.on_behalf_of, Some(agent_addr("codex")));
    assert_eq!(reply.body, answer.body);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_ends_open_streams() {
    let server = TestServer::start().await;
    server.append("one").await;
    let mut stream = server.stream(ALICE, None, None).await;
    assert_eq!(stream.seqs(1).await, vec![1]);
    let hub = server.running.hub.clone();
    tokio::time::timeout(WAIT, server.running.shutdown())
        .await
        .expect("shutdown must not wait on open streams")
        .unwrap();
    assert!(stream.event().await.is_none());
    assert!(matches!(
        hub.append(note("after shutdown")).await,
        Err(senate_core::HubError::Unavailable(_))
    ));
}

#[tokio::test]
async fn a_writer_rebuilds_the_seat_fence_after_a_lost_commit_ack() {
    use senate_core::api::{CommitteeSeatInfo, SetCommitteeSeat, committee_seat_path};
    use senate_core::{CommitteeSeatAction, EventId, PresenceState};

    let store = Arc::new(Faulty::default());
    let server = TestServer::start_configured(store.clone(), |config| {
        config.roster.agents = vec![seated("claude", "alice")];
    })
    .await;
    assert_eq!(
        server.heartbeat(ALICE, "claude").await.status(),
        StatusCode::NO_CONTENT
    );
    let hub = &server.running.hub;
    let agent = path("claude");
    let committee_id = committee();
    let before = hub.last_heartbeat(&committee(), &agent).await.unwrap();
    let mut request = SetCommitteeSeat {
        senator: agent.clone(),
        action: CommitteeSeatAction::Unseat,
        id: EventId::new(),
        acted_by: None,
    };
    let seat = |request: &SetCommitteeSeat| {
        server
            .request(Method::PUT, &committee_seat_path(&committee()), ALICE)
            .json(request)
            .send()
    };
    store.arm(LOSE_ACK_AFTER_COMMIT);
    api_error(
        seat(&request).await.unwrap(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
    )
    .await;
    eventually("writer restored the excluded seat", || async {
        hub.presence(&committee())
            .await
            .is_ok_and(|page| page.agents.is_empty())
    })
    .await;
    assert_eq!(
        server.heartbeat(ALICE, "claude").await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.last_heartbeat(&committee(), &agent).await.unwrap(),
        before
    );
    let replay: CommitteeSeatInfo = seat(&request).await.unwrap().json().await.unwrap();
    assert!(replay.duplicate);
    assert_eq!(replay.action, CommitteeSeatAction::Unseat);

    request.action = CommitteeSeatAction::Join;
    request.id = EventId::new();
    store.arm(LOSE_ACK_AFTER_COMMIT);
    api_error(
        seat(&request).await.unwrap(),
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
    )
    .await;
    eventually("writer restored the explicit join", || async {
        hub.presence(&committee()).await.is_ok_and(|page| {
            page.agents
                .iter()
                .any(|p| p.agent == agent && p.state == PresenceState::Absent)
        })
    })
    .await;
    let _stream = server.stream_as(ALICE, Some("claude"), None, None).await;
    eventually("joined stream is connected", || {
        hub.is_agent_connected(&committee_id, &agent)
    })
    .await;
    server.running.shutdown().await.unwrap();
}
