//! Checkpoint appends, presence leases and the routes, per agent.

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use reqwest::{Method, StatusCode};
use senate_core::Locale;
use senate_core::api::{Heartbeat, PresencePage, heartbeat_path, latest_checkpoint_path};
use senate_core::{
    Address, CheckpointFiles, CheckpointPayload, CheckpointRecord, CheckpointTrigger, Event, Hub,
    Kind, NewEvent, PresencePayload, PresenceState, Seq, meta,
};
use senate_log::{LogStore, MemoryStore};
use serde_json::json;

fn checkpoint(from: Address, progress: &str) -> NewEvent {
    let payload = CheckpointPayload {
        branch: "feature/refund-v2/alice".into(),
        commit: "a41f9c2e00000000000000000000000000000000".into(),
        pushed: false,
        remote_url: None,
        files: CheckpointFiles {
            progress: Some(progress.into()),
            ..CheckpointFiles::default()
        },
        session_transcript: true,
        trigger: CheckpointTrigger::Tool,
    };
    NewEvent::new(committee(), Kind::Checkpoint, from, "idempotency keys done")
        .with_meta(meta::CHECKPOINT, payload.value())
}

#[tokio::test]
async fn checkpoints_are_an_agents_own_and_the_latest_is_served() {
    let server = TestServer::start().await;
    let alice = agent_addr("claude");
    let claude = path("claude");

    let as_user = checkpoint(person_addr("alice"), "x");
    api_error(
        server.post_event(ALICE, &as_user).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    let mut bare = checkpoint(alice.clone(), "x");
    bare.meta.clear();
    api_error(
        server.post_event(ALICE, &bare).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    let huge = checkpoint(alice.clone(), &"x".repeat(70 * 1024));
    api_error(
        server.post_event(ALICE, &huge).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    let latest = |who: &str| {
        let path = format!("{}?agent={who}", latest_checkpoint_path(&committee()));
        server.request(Method::GET, &path, BOB).send()
    };
    api_error(
        latest("claude").await.unwrap(),
        StatusCode::NOT_FOUND,
        "not_found",
    )
    .await;

    for progress in ["first", "second"] {
        let response = server
            .post_event(ALICE, &checkpoint(alice.clone(), progress))
            .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    assert_eq!(
        latest_checkpoint_path(&committee()),
        "/v1/committees/refund-v2/checkpoints/latest"
    );
    let record: CheckpointRecord = latest("claude").await.unwrap().json().await.unwrap();
    assert_eq!(record.agent, claude);
    assert_eq!(record.checkpoint.files.progress.as_deref(), Some("second"));

    // A checkpoint is one agent's: its sibling has none, and neither does
    // the person.
    for who in ["codex", "alice"] {
        api_error(
            latest(who).await.unwrap(),
            StatusCode::NOT_FOUND,
            "not_found",
        )
        .await;
    }
    let sibling = Address::Senator(path("codex"));
    assert_eq!(
        server
            .post_event(ALICE, &checkpoint(sibling, "codex's"))
            .await
            .status(),
        StatusCode::OK
    );
    let record: CheckpointRecord = latest("claude").await.unwrap().json().await.unwrap();
    assert_eq!(record.checkpoint.files.progress.as_deref(), Some("second"));
    let record: CheckpointRecord = latest("codex").await.unwrap().json().await.unwrap();
    assert_eq!(record.checkpoint.files.progress.as_deref(), Some("codex's"));
    let hub = server.running.hub.clone();
    assert_eq!(
        hub.latest_checkpoint(&committee(), &claude)
            .await
            .unwrap()
            .unwrap()
            .seq,
        Seq(2)
    );
    let brief = hub.brief(&committee()).await.unwrap();
    assert_eq!(
        brief.checkpoints[0].commit.as_deref(),
        Some("a41f9c2e00000000000000000000000000000000")
    );

    // The companion label only marks an agent's answer.
    let labelled_note = NewEvent::new(committee(), Kind::Note, alice.clone(), "n")
        .with_meta(meta::COMPANION, json!(true));
    api_error(
        server.post_event(ALICE, &labelled_note).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    let labelled_answer = NewEvent::new(committee(), Kind::Answer, alice, "a")
        .in_thread(Seq(1))
        .with_meta(meta::COMPANION, json!(true));
    assert_eq!(
        server.post_event(ALICE, &labelled_answer).await.status(),
        StatusCode::OK
    );
    server.running.shutdown().await.unwrap();
}

async fn beat(server: &TestServer, token: &str, agent: &str, cursor: u64) {
    let response = server
        .request(Method::POST, &heartbeat_path(&committee()), token)
        .json(&Heartbeat {
            agent: path(agent),
            sessions: 1,
            cursor: Seq(cursor),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

async fn presence_events(hub: &dyn Hub) -> Vec<(PresencePayload, Event)> {
    hub.read(&committee(), Seq::ZERO, 500)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| Some((PresencePayload::of(&e)?, e)))
        .collect()
}

/// An earlier agent's first event: it takes the committee's seat, so the
/// agents these tests follow are not the first, and their first `online`
/// stays unwritten.
async fn first_agent_writes(server: &TestServer, agent: &str) {
    let event = NewEvent::new(committee(), Kind::Note, agent_addr(agent), "hello");
    let response = server.post_event(ALICE, &event).await;
    assert_eq!(response.status(), StatusCode::OK);
}

async fn states(hub: &dyn Hub) -> Vec<PresenceState> {
    presence_events(hub)
        .await
        .into_iter()
        .map(|(p, _)| p.state)
        .collect()
}

#[tokio::test]
async fn presence_goes_disconnected_absent_taken_over_and_back_online() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |config| {
        config.presence_disconnected_secs = 1;
        config.lease_secs = 2;
    })
    .await;
    let hub = server.running.hub.clone();
    let f = committee();
    let alice = path("claude");
    // The first agent to sit chairs the committee, and its seat is the one
    // `online` that is written; claude is the second, so its own `online`
    // is not.
    first_agent_writes(&server, "codex").await;
    for i in 1..3 {
        server.append(&format!("note {i}")).await;
    }
    let stream = server.stream_as(ALICE, Some("claude"), None, None).await;
    beat(&server, ALICE, "claude", 1).await;

    // Online, but not written: every bridge connecting would be noise.
    let page: PresencePage = server
        .request(Method::GET, &senate_core::api::presence_path(&f), BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.head, Seq(3));
    assert_eq!(page.agents.len(), 1);
    assert_eq!(page.agents[0].state, PresenceState::Online);
    assert_eq!(page.agents[0].seq, None);
    assert_eq!(page.agents[0].lag, Some(2), "head 3, cursor 1");
    assert!(states(hub.as_ref()).await.is_empty());
    assert!(
        hub.mark_taken_over(&f, &alice).await.is_err(),
        "an online agent is not taken over"
    );

    // The laptop closes.
    drop(stream);
    eventually("disconnected, then absent", || async {
        states(hub.as_ref()).await == [PresenceState::Disconnected, PresenceState::Absent]
    })
    .await;
    let events = presence_events(hub.as_ref()).await;
    let (disconnected, event) = &events[0];
    assert_eq!(event.from, Address::Committee);
    assert_eq!(disconnected.previous, Some(PresenceState::Online));
    assert_eq!(disconnected.cursor, Some(Seq(1)));
    // Nothing in the log depends on a language: the body is empty, the
    // payload is in `meta`, and the line is rendered when shown.
    assert_eq!(event.body, "");
    assert!(
        disconnected
            .line(Locale::En)
            .starts_with("[senator::claude] disconnected"),
        "{}",
        disconnected.line(Locale::En)
    );
    assert_eq!(events[1].1.body, "");
    assert_eq!(
        events[1].0.line(Locale::Zh),
        "[senator::claude] 缺席 · 落后 3 条事件"
    );

    let taken = hub.mark_taken_over(&f, &alice).await.unwrap().unwrap();
    assert_eq!(
        PresencePayload::of(&taken).unwrap().state,
        PresenceState::TakenOver
    );
    assert_eq!(hub.mark_taken_over(&f, &alice).await.unwrap(), None);

    // She comes back: online is written now.
    beat(&server, ALICE, "claude", 6).await;
    eventually("back online", || async {
        states(hub.as_ref()).await.last() == Some(&PresenceState::Online)
    })
    .await;
    let (online, _) = presence_events(hub.as_ref()).await.pop().unwrap();
    assert_eq!(online.previous, Some(PresenceState::TakenOver));
    let brief = hub.brief(&f).await.unwrap();
    assert_eq!(brief.presence[0].state, PresenceState::Online);
    assert_eq!(
        brief.presence[0].lag,
        Some(1),
        "its own online event is new"
    );

    // Ids are derived per transition: nothing is stored twice.
    let ids: std::collections::HashSet<_> = presence_events(hub.as_ref())
        .await
        .into_iter()
        .map(|(_, e)| e.id)
        .collect();
    assert_eq!(ids.len(), 4);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_restart_restores_leases_without_writing_transitions() {
    let store: Arc<dyn LogStore> = Arc::new(MemoryStore::new());
    let configure = |config: &mut senate_server::ServerConfig| {
        config.presence_disconnected_secs = 1;
        config.lease_secs = 60;
    };
    let first = TestServer::start_configured(store.clone(), configure).await;
    first_agent_writes(&first, "codex").await;
    let stream = first.stream_as(ALICE, Some("claude"), None, None).await;
    beat(&first, ALICE, "claude", 1).await;
    drop(stream);
    let hub = first.running.hub.clone();
    eventually("alice disconnected", || async {
        states(hub.as_ref()).await == [PresenceState::Disconnected]
    })
    .await;
    // Bob is online, never written.
    beat(&first, BOB, "copilot", 1).await;
    first.running.shutdown().await.unwrap();

    let second = TestServer::start_configured(store, configure).await;
    let hub = second.running.hub.clone();
    // Nobody touches the committee: its writer starts with the server, and
    // the restart itself writes nothing.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(states(hub.as_ref()).await, [PresenceState::Disconnected]);
    // Bob never comes back after the restart: his first event is his
    // disconnect, a window after the restart.
    eventually("bob disconnected", || async {
        presence_events(hub.as_ref())
            .await
            .iter()
            .any(|(p, _)| p.agent == path("copilot") && p.state == PresenceState::Disconnected)
    })
    .await;
    let page = hub.presence(&committee()).await.unwrap();
    let alice = page
        .agents
        .iter()
        .find(|a| a.agent == path("claude"))
        .unwrap();
    assert_eq!(alice.state, PresenceState::Disconnected);
    assert_eq!(alice.seq, Some(Seq(2)));
    second.running.shutdown().await.unwrap();
}

/// Default timings, real minutes: the clock is the store's, so the downtime
/// is made by backdating what the first server stored.
fn restart_config(config: &mut senate_server::ServerConfig) {
    config.presence_disconnected_secs = 120;
    config.lease_secs = 900;
}

/// A store holding one heartbeat of alice's, `age_secs` old: the server that
/// took it went down and the time passed.
async fn store_with_old_beat(age_secs: i64) -> (Arc<dyn LogStore>, chrono::DateTime<chrono::Utc>) {
    let store: Arc<dyn LogStore> = Arc::new(MemoryStore::new());
    // A committee exists once it has an event; its writer starts with the
    // server.
    store.append(note("hello")).await.unwrap();
    let at = senate_log::now() - chrono::Duration::seconds(age_secs);
    store
        .record_heartbeat(
            &committee(),
            Heartbeat {
                agent: path("claude"),
                sessions: 1,
                cursor: Seq(0),
            },
            at,
        )
        .await
        .unwrap();
    (store, at)
}

async fn alice_now(hub: &dyn Hub) -> PresenceState {
    hub.presence(&committee())
        .await
        .unwrap()
        .agents
        .iter()
        .find(|a| a.agent == path("claude"))
        .map(|a| a.state)
        .expect("alice has a presence")
}

#[tokio::test]
async fn a_restart_within_the_grace_leaves_the_agent_online_and_writes_nothing() {
    let (store, _) = store_with_old_beat(30).await;
    let server = TestServer::start_configured(store, restart_config).await;
    let hub = server.running.hub.clone();
    assert_eq!(alice_now(hub.as_ref()).await, PresenceState::Online);
    assert!(presence_events(hub.as_ref()).await.is_empty());
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_restart_between_the_grace_and_the_lease_finds_the_agent_disconnected() {
    let (store, beat) = store_with_old_beat(300).await;
    let server = TestServer::start_configured(store, restart_config).await;
    let hub = server.running.hub.clone();
    // At once: the downtime counts, nothing waits for a tick or a window.
    assert_eq!(alice_now(hub.as_ref()).await, PresenceState::Disconnected);
    let events = presence_events(hub.as_ref()).await;
    assert_eq!(events.len(), 1);
    let (p, _) = &events[0];
    assert_eq!(p.state, PresenceState::Disconnected);
    assert_eq!(p.previous, Some(PresenceState::Online));
    assert_eq!(p.last_seen, Some(beat));
    assert_eq!(p.since, beat + chrono::Duration::seconds(120));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_restart_beyond_the_lease_finds_the_agent_absent() {
    let (store, beat) = store_with_old_beat(14 * 60 + 600).await;
    let server = TestServer::start_configured(store, restart_config).await;
    let hub = server.running.hub.clone();
    assert_eq!(alice_now(hub.as_ref()).await, PresenceState::Absent);
    let events = presence_events(hub.as_ref()).await;
    assert_eq!(states(hub.as_ref()).await, [PresenceState::Absent]);
    assert_eq!(events[0].0.since, beat + chrono::Duration::seconds(900));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_logged_disconnect_becomes_absent_once_the_downtime_passes_the_lease() {
    let (store, beat) = store_with_old_beat(1000).await;
    // The first server had already written the disconnect.
    let disconnected = senate_core::PresencePayload {
        agent: path("claude"),
        state: PresenceState::Disconnected,
        since: beat + chrono::Duration::seconds(120),
        since_seq: Seq(0),
        previous: Some(PresenceState::Online),
        last_seen: Some(beat),
        cursor: Some(Seq(0)),
        lag: Some(0),
        new_owner: None,
    };
    store
        .append(senate_core::presence_event(committee(), &disconnected))
        .await
        .unwrap();
    let server = TestServer::start_configured(store, restart_config).await;
    let hub = server.running.hub.clone();
    assert_eq!(
        states(hub.as_ref()).await,
        [PresenceState::Disconnected, PresenceState::Absent]
    );
    assert_eq!(alice_now(hub.as_ref()).await, PresenceState::Absent);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn restarting_again_writes_no_transition_twice() {
    for age in [300, 2000] {
        let (store, _) = store_with_old_beat(age).await;
        let mut ids = Vec::new();
        for _ in 0..3 {
            let server = TestServer::start_configured(store.clone(), restart_config).await;
            let hub = server.running.hub.clone();
            tokio::time::sleep(Duration::from_millis(300)).await;
            ids = presence_events(hub.as_ref())
                .await
                .into_iter()
                .map(|(_, e)| (e.seq, e.id))
                .collect();
            assert_eq!(ids.len(), 1, "age {age}: {ids:?}");
            server.running.shutdown().await.unwrap();
        }
        assert_eq!(ids.len(), 1);
    }
}

#[tokio::test]
async fn a_sqlite_store_restarts_into_what_the_clock_says() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("senate.db");
    let beat = senate_log::now() - chrono::Duration::seconds(300);
    {
        let store = senate_log::SqliteStore::open(&file).await.unwrap();
        store.append(note("hello")).await.unwrap();
        store
            .record_heartbeat(
                &committee(),
                Heartbeat {
                    agent: path("claude"),
                    sessions: 1,
                    cursor: Seq(0),
                },
                beat,
            )
            .await
            .unwrap();
    }
    for _ in 0..2 {
        let store: Arc<dyn LogStore> =
            Arc::new(senate_log::SqliteStore::open(&file).await.unwrap());
        let server = TestServer::start_configured(store, restart_config).await;
        let hub = server.running.hub.clone();
        assert_eq!(alice_now(hub.as_ref()).await, PresenceState::Disconnected);
        assert_eq!(states(hub.as_ref()).await, [PresenceState::Disconnected]);
        server.running.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn an_agent_that_reconnects_after_a_restart_is_online_again() {
    let (store, _) = store_with_old_beat(1000).await;
    let server = TestServer::start_configured(store, restart_config).await;
    let hub = server.running.hub.clone();
    assert_eq!(alice_now(hub.as_ref()).await, PresenceState::Absent);
    let _stream = server.stream_as(ALICE, Some("claude"), None, None).await;
    beat(&server, ALICE, "claude", 0).await;
    eventually("alice online again", || async {
        states(hub.as_ref()).await == [PresenceState::Absent, PresenceState::Online]
    })
    .await;
    let (online, _) = presence_events(hub.as_ref()).await.pop().unwrap();
    assert_eq!(online.previous, Some(PresenceState::Absent));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_reassignment_is_the_servers_and_stands_until_handed_back() {
    let store: Arc<dyn LogStore> = Arc::new(MemoryStore::new());
    let configure = |config: &mut senate_server::ServerConfig| {
        config.presence_disconnected_secs = 1;
        config.lease_secs = 60;
    };
    let server = TestServer::start_configured(store.clone(), configure).await;
    let hub = server.running.hub.clone();
    let f = committee();
    let alice = path("claude");
    let bob = path("copilot");
    let asker = name("bob");
    first_agent_writes(&server, "codex").await;
    let stream = server.stream_as(ALICE, Some("claude"), None, None).await;
    beat(&server, ALICE, "claude", 1).await;

    assert!(
        hub.reassign(&f, &alice, &path("bob"), &asker)
            .await
            .is_err(),
        "only to an agent"
    );
    assert!(
        hub.reassign(&f, &alice, &alice, &asker).await.is_err(),
        "nothing to hand back"
    );
    let moved = hub
        .reassign(&f, &alice, &bob, &asker)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(moved.from, Address::Committee);
    assert_eq!(moved.on_behalf_of, Some(person_addr("bob")));
    let payload = PresencePayload::of(&moved).unwrap();
    assert_eq!(payload.state, PresenceState::Reassigned);
    assert_eq!(payload.new_owner, Some(bob.clone()));
    assert_eq!(payload.previous, Some(PresenceState::Online));
    assert_eq!(hub.reassign(&f, &alice, &bob, &asker).await.unwrap(), None);

    // Her heartbeats and her silence leave it standing.
    beat(&server, ALICE, "claude", 2).await;
    drop(stream);
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(states(hub.as_ref()).await, [PresenceState::Reassigned]);
    let page = hub.presence(&f).await.unwrap();
    assert_eq!(page.agents[0].new_owner, Some(bob.clone()));
    server.running.shutdown().await.unwrap();

    // A restart too.
    let second = TestServer::start_configured(store, configure).await;
    let hub = second.running.hub.clone();
    beat(&second, ALICE, "claude", 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(states(hub.as_ref()).await, [PresenceState::Reassigned]);
    let back = hub
        .reassign(&f, &alice, &alice, &asker)
        .await
        .unwrap()
        .unwrap();
    let payload = PresencePayload::of(&back).unwrap();
    assert_eq!(payload.state, PresenceState::Online);
    assert_eq!(payload.previous, Some(PresenceState::Reassigned));
    assert_eq!(payload.new_owner, None);
    second.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_senator_cannot_write_a_work_run() {
    let server = TestServer::start().await;
    let forged = NewEvent::new(committee(), Kind::Note, agent_addr("claude"), "PR")
        .with_meta(meta::WORK, json!({"state": "succeeded"}));
    api_error(
        server.post_event(ALICE, &forged).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_senator_cannot_write_a_provisional_answer() {
    let server = TestServer::start().await;
    let forged = NewEvent::new(committee(), Kind::Answer, agent_addr("claude"), "…")
        .with_meta(meta::PROVISIONAL, json!(true));
    api_error(
        server.post_event(ALICE, &forged).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}
