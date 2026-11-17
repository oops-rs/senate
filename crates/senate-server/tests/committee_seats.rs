#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use reqwest::{Method, Response, StatusCode};
use senate_core::api::{self, AppendResponse, CommitteeSeatInfo, SetCommitteeSeat};
use senate_core::{
    Address, CommitteeId, CommitteeSeatAction, CommitteeSeatChange, DocketPage, EventId, Hub, Kind,
    NewEvent, PresenceState, meta,
};
use senate_log::{MemoryStore, SqliteStore};

const PRIMARY: &str = "codex-seat-test";

async fn put_seat(server: &TestServer, token: &str, request: &SetCommitteeSeat) -> Response {
    server
        .request(Method::PUT, &api::committee_seat_path(&committee()), token)
        .json(request)
        .send()
        .await
        .unwrap()
}

async fn change_seat(
    server: &TestServer,
    token: &str,
    request: &SetCommitteeSeat,
) -> CommitteeSeatInfo {
    let response = put_seat(server, token, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

fn change(senator: &str, action: CommitteeSeatAction) -> SetCommitteeSeat {
    SetCommitteeSeat {
        senator: path(senator),
        action,
        id: EventId::new(),
        acted_by: None,
    }
}

#[tokio::test]
async fn unseating_an_alias_retires_one_seat_until_an_explicit_join() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("senate.db");
    let server =
        TestServer::start_with(Arc::new(SqliteStore::open(&db).await.unwrap()), None).await;
    let hub = &server.running.hub;
    let primary = path(PRIMARY);
    let committee = committee();
    let commons = CommitteeId::new("commons").unwrap();
    let mut stream = server.stream_as(ALICE, Some(PRIMARY), Some(0), None).await;
    let commons_stream = hub.open_stream(&commons, Some(&primary)).await.unwrap();
    eventually("commons chair", || async {
        hub.chair(&commons).await.unwrap().chair.as_ref() == Some(&primary)
    })
    .await;

    assert_eq!(
        server.heartbeat(ALICE, PRIMARY).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        server.heartbeat(BOB, "copilot").await.status(),
        StatusCode::NO_CONTENT
    );
    let before = hub
        .last_heartbeat(&committee, &primary)
        .await
        .unwrap()
        .unwrap();
    let history = NewEvent::new(committee.clone(), Kind::Note, agent_addr(PRIMARY), "kept");
    let old: AppendResponse = server
        .post_event(ALICE, &history)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        server
            .post_event(
                BOB,
                &NewEvent::new(
                    committee.clone(),
                    Kind::Note,
                    agent_addr("copilot"),
                    "remaining seat"
                ),
            )
            .await
            .status(),
        StatusCode::OK
    );
    let mut candidates = hub.committee_agents(&committee).await.unwrap();
    candidates.sort();
    assert_eq!(candidates, [primary.clone(), path("copilot")]);
    let alias = api::SetAlias {
        alias: path("reviewer"),
        initial: false,
    };
    let response = server
        .request(Method::PUT, &format!("/v1/aliases/{primary}"), ALICE)
        .json(&alias)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let remove = change("reviewer", CommitteeSeatAction::Unseat);
    let removed = change_seat(&server, ALICE, &remove).await;
    assert_eq!(removed.action, CommitteeSeatAction::Unseat);
    assert_eq!(removed.senator, primary);
    assert!(!removed.duplicate);
    loop {
        let event = stream.event().await.unwrap();
        if event.seq == removed.seq {
            assert_eq!(
                CommitteeSeatChange::of(&event).unwrap().action,
                CommitteeSeatAction::Unseat
            );
            break;
        }
    }
    assert!(stream.event().await.is_none());
    let again = change_seat(&server, ALICE, &remove).await;
    assert!(again.duplicate);
    assert_eq!(again.seq, removed.seq);

    assert_eq!(
        server.heartbeat(ALICE, PRIMARY).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.last_heartbeat(&committee, &primary).await.unwrap(),
        Some(before)
    );
    assert_eq!(
        server
            .try_stream(ALICE, Some(PRIMARY), None, None)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert!(!hub.is_agent_connected(&committee, &primary).await);
    assert_eq!(server.act_as(ALICE, PRIMARY).await, StatusCode::OK);
    let post = NewEvent::new(
        committee.clone(),
        Kind::Note,
        agent_addr(PRIMARY),
        "needs a seat",
    );
    assert_eq!(
        server.post_event(ALICE, &post).await.status(),
        StatusCode::BAD_REQUEST
    );
    let presence = hub.presence(&committee).await.unwrap();
    assert!(presence.agents.iter().all(|fact| fact.agent != primary));
    assert_eq!(
        hub.committee_agents(&committee).await.unwrap(),
        [path("copilot")]
    );
    assert_eq!(
        hub.chair(&committee).await.unwrap().chair,
        Some(path("copilot"))
    );
    let rejected = server
        .request(Method::PUT, &api::chair_path(&committee), ALICE)
        .json(&api::SetChair {
            senator: primary.clone(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    let docket: DocketPage = server
        .request(Method::GET, &api::docket_path(&committee), ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(docket.seats.iter().all(|fact| fact.agent != primary));
    assert_eq!(
        hub.get(&committee, old.event.seq).await.unwrap(),
        Some(old.event.clone())
    );
    assert!(hub.is_agent_connected(&commons, &primary).await);
    assert_eq!(
        hub.chair(&commons).await.unwrap().chair,
        Some(primary.clone())
    );

    let wildcard = NewEvent::new(
        committee.clone(),
        Kind::Question,
        agent_addr("asker"),
        "who",
    )
    .to(Address::AnyAgent);
    let resolved: AppendResponse = server
        .post_event(CAROL, &wildcard)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(resolved.event.to, [agent_addr("copilot")]);

    drop(commons_stream);
    server.running.shutdown().await.unwrap();
    let server =
        TestServer::start_with(Arc::new(SqliteStore::open(&db).await.unwrap()), None).await;
    let hub = &server.running.hub;
    assert_eq!(
        server.heartbeat(ALICE, PRIMARY).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.committee_agents(&committee).await.unwrap(),
        [path("copilot")]
    );
    assert_eq!(
        hub.get(&committee, old.event.seq).await.unwrap(),
        Some(old.event)
    );
    assert_eq!(hub.roster_now().owner_of(&primary), Some(&name("alice")));
    assert_eq!(
        hub.chair(&commons).await.unwrap().chair,
        Some(primary.clone())
    );
    let alias_result: api::SenatorAliases = server
        .request(Method::GET, "/v1/aliases", ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(alias_result.get(&alias.alias), Some(&primary));
    let joined = change_seat(
        &server,
        ALICE,
        &change("reviewer", CommitteeSeatAction::Join),
    )
    .await;
    assert_eq!(joined.senator, primary);
    assert_eq!(joined.action, CommitteeSeatAction::Join);
    let replay = change_seat(&server, ALICE, &remove).await;
    assert!(replay.duplicate);
    assert_eq!(
        (replay.seq, replay.action),
        (joined.seq, CommitteeSeatAction::Join)
    );
    assert!(
        hub.committee_agents(&committee)
            .await
            .unwrap()
            .contains(&primary)
    );
    assert_eq!(
        hub.chair(&committee).await.unwrap().chair,
        Some(path("copilot"))
    );

    server.running.shutdown().await.unwrap();
    let server =
        TestServer::start_with(Arc::new(SqliteStore::open(&db).await.unwrap()), None).await;
    let hub = &server.running.hub;
    let presence = hub.presence(&committee).await.unwrap();
    let restored = presence
        .agents
        .iter()
        .find(|agent| agent.agent == primary)
        .unwrap();
    assert_eq!(restored.state, PresenceState::Absent);
    assert_eq!(restored.last_seen, None);
    let _stream = server.stream_as(ALICE, Some(PRIMARY), None, None).await;
    eventually("rejoined stream", || {
        hub.is_agent_connected(&committee, &primary)
    })
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn changing_someone_elses_seat_requires_the_speakers_authority() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |config| {
        config.roster.speaker = Some("senator::carol".parse().unwrap());
    })
    .await;
    assert_eq!(
        server.heartbeat(ALICE, PRIMARY).await.status(),
        StatusCode::NO_CONTENT
    );
    let mut remove = change(PRIMARY, CommitteeSeatAction::Unseat);
    api_error(
        put_seat(&server, BOB, &remove).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    remove.acted_by = Some(path("carol"));
    api_error(
        put_seat(&server, BOB, &remove).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;

    let forged = NewEvent::new(committee(), Kind::Note, agent_addr(PRIMARY), "").with_meta(
        meta::COMMITTEE_SEAT,
        serde_json::json!({
            "senator": PRIMARY, "action": "join", "by": "alice"
        }),
    );
    api_error(
        server.post_event(ALICE, &forged).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;

    let removed = change_seat(&server, CAROL, &remove).await;
    assert_eq!(removed.action, CommitteeSeatAction::Unseat);
    let event = server
        .running
        .hub
        .get(&committee(), removed.seq)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(CommitteeSeatChange::of(&event).unwrap().by, name("carol"));
    api_error(
        put_seat(&server, BOB, &change(PRIMARY, CommitteeSeatAction::Join)).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}
