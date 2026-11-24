//! Committee archiving: only the Speaker in person, refused while an agent
//! is connected, recorded in the committee's log, and what an archived
//! committee still takes and refuses.

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;

use common::*;
use reqwest::{Method, Response, StatusCode};
use senate_core::api::{self, CommitteeArchiveInfo, Heartbeat, Me, SetCommitteeArchive};
use senate_core::{CommitteeArchive, EventId, Hub, Kind, NewEvent, Seq, meta};
use senate_log::MemoryStore;

const AGENT: &str = "codex-archive-test";

async fn archive(server: &TestServer, token: &str, archived: bool, id: &EventId) -> Response {
    server
        .request(
            Method::PUT,
            &api::committee_archive_path(&committee()),
            token,
        )
        .json(&SetCommitteeArchive {
            archived,
            id: id.clone(),
        })
        .send()
        .await
        .unwrap()
}

async fn me(server: &TestServer, token: &str) -> Me {
    server
        .request(Method::GET, "/v1/me", token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn leave(server: &TestServer) {
    let left = server
        .request(Method::POST, &api::heartbeat_path(&committee()), ALICE)
        .json(&Heartbeat {
            paused: false,
            agent: path(AGENT),
            sessions: 0,
            cursor: Seq::ZERO,
            left: true,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(left.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn only_the_speaker_in_person_archives_and_an_archived_committee_takes_no_new_threads() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |config| {
        config.roster.speaker = Some("senator::carol".parse().unwrap());
    })
    .await;
    let open = server.append("an open question").await;
    assert_eq!(
        server.heartbeat(ALICE, AGENT).await.status(),
        StatusCode::NO_CONTENT
    );

    // Not the Speaker: refused.
    let id = EventId::new();
    api_error(
        archive(&server, ALICE, true, &id).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    // The Speaker, while an agent is connected: refused, naming it.
    let busy = archive(&server, CAROL, true, &id).await;
    assert!(!busy.status().is_success());
    let text = busy.text().await.unwrap();
    assert!(
        text.contains(AGENT) && text.contains("stop their sessions first"),
        "{text}"
    );

    // Once it left: archived, recorded in its own log.
    leave(&server).await;
    let done = archive(&server, CAROL, true, &id).await;
    assert_eq!(done.status(), StatusCode::OK);
    let info: CommitteeArchiveInfo = done.json().await.unwrap();
    assert!(info.archived);
    let seq = info.seq.expect("an event");
    let event = server
        .running
        .hub
        .get(&committee(), seq)
        .await
        .unwrap()
        .unwrap();
    let change = CommitteeArchive::of(&event).unwrap();
    assert!(change.archived);
    assert_eq!(change.by, name("carol"));
    // Retried, or archived again: nothing new.
    let again: CommitteeArchiveInfo = archive(&server, CAROL, true, &id)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again.seq, None);

    // Hidden from what people can join; listed as archived.
    let listed = me(&server, ALICE).await;
    assert!(
        !listed.committees.contains(&committee()),
        "{:?}",
        listed.committees
    );
    assert_eq!(listed.archived, [committee()]);

    // No new thread; an answer in an open one is still taken.
    let root = NewEvent::new(committee(), Kind::Question, person_addr("alice"), "new?");
    let refused = server.post_event(ALICE, &root).await;
    assert!(!refused.status().is_success());
    let text = refused.text().await.unwrap();
    assert!(
        text.contains("is archived") && text.contains("senate committees unarchive"),
        "{text}"
    );
    let answer = NewEvent::new(
        committee(),
        Kind::Answer,
        person_addr("alice"),
        "closing it",
    )
    .in_thread(open.seq);
    assert!(
        server
            .post_event(ALICE, &answer)
            .await
            .status()
            .is_success()
    );
    // No agent connects, and no heartbeat seats one.
    assert!(
        !server
            .try_stream(ALICE, Some(AGENT), None, None)
            .await
            .status()
            .is_success()
    );
    assert!(!server.heartbeat(ALICE, AGENT).await.status().is_success());
    // Nobody writes the server's own archive key.
    let forged = NewEvent::new(committee(), Kind::Note, person_addr("carol"), "")
        .in_thread(open.seq)
        .with_meta(
            meta::COMMITTEE_ARCHIVE,
            serde_json::json!({"archived": false, "by": "carol"}),
        );
    api_error(
        server.post_event(CAROL, &forged).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    // Its log stays readable.
    assert!(
        server
            .running
            .hub
            .get(&committee(), open.seq)
            .await
            .unwrap()
            .is_some()
    );

    // Unarchived by the Speaker: back, and taking new threads.
    let back: CommitteeArchiveInfo = archive(&server, CAROL, false, &EventId::new())
        .await
        .json()
        .await
        .unwrap();
    assert!(!back.archived && back.seq.is_some());
    assert!(me(&server, ALICE).await.committees.contains(&committee()));
    assert!(server.post_event(ALICE, &root).await.status().is_success());
    server.running.shutdown().await.unwrap();
}

/// A Senate whose Speaker is not a person (an agent presides) has nobody to
/// archive in person.
#[tokio::test]
async fn an_agent_speaker_cannot_archive() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |config| {
        config.roster.agents = vec![seated("claude", "alice")];
        config.roster.speaker = Some("senator::claude".parse().unwrap());
    })
    .await;
    server.append("exists").await;
    for token in [ALICE, BOB, CAROL] {
        api_error(
            archive(&server, token, true, &EventId::new()).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }
    server.running.shutdown().await.unwrap();
}
