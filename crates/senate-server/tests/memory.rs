//! The committee's memory over the API: who may write it, and searching it,
//! rebuilt from the log by a fresh server.

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;

use common::*;
use reqwest::{Method, StatusCode};
use senate_core::api::MemoryResults;
use senate_core::{
    Address, DigestPayload, DigestPoint, DigestTrigger, Hub, Kind, MemoryPayload, NewEvent, Seq,
    meta,
};
use senate_log::{LogStore, MemoryStore};

fn memory_path() -> String {
    format!("/v1/committees/{}/memory", committee())
}

fn point(body: &str, refs: &[u64], supersedes: &[u64]) -> NewEvent {
    point_from(Address::Speaker, body, refs, supersedes)
}

fn point_from(from: Address, body: &str, refs: &[u64], supersedes: &[u64]) -> NewEvent {
    NewEvent::new(committee(), Kind::Note, from, body).with_meta(
        meta::MEMORY,
        MemoryPayload::Point {
            refs: refs.iter().copied().map(Seq).collect(),
            supersedes: supersedes.iter().copied().map(Seq).collect(),
            digest: None,
        }
        .to_value(),
    )
}

fn correct(from: Address, target: u64, body: &str) -> NewEvent {
    NewEvent::new(committee(), Kind::Note, from, body).with_meta(
        meta::MEMORY,
        MemoryPayload::Correct {
            target: Seq(target),
            refs: vec![],
        }
        .to_value(),
    )
}

fn delete(from: Address, target: u64) -> NewEvent {
    NewEvent::new(committee(), Kind::Note, from, "").with_meta(
        meta::MEMORY,
        MemoryPayload::Delete {
            targets: vec![Seq(target)],
        }
        .to_value(),
    )
}

async fn search(server: &TestServer, query: &str) -> Vec<(u64, String)> {
    let response = server
        .request(
            Method::GET,
            &format!("{}?q={query}&all=false", memory_path()),
            ALICE,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let results: MemoryResults = response.json().await.unwrap();
    results
        .points
        .into_iter()
        .map(|h| (h.point.seq.0, h.point.text))
        .collect()
}

#[tokio::test]
async fn only_the_speaker_writes_points_and_only_people_correct_them() {
    let server = TestServer::start().await;
    assert_eq!(server.act_as(ALICE, "copilot").await, StatusCode::OK);
    let first = server
        .running
        .hub
        .append(point("Deploys happen at noon", &[1], &[]));
    let first = first.await.unwrap().event.seq.0;

    let agent = agent_addr("copilot");
    let refused = [
        // A point is the Speaker's.
        point_from(person_addr("alice"), "A person's point", &[1], &[]),
        point_from(agent.clone(), "An agent's point", &[1], &[]),
        // An agent neither corrects nor deletes.
        correct(agent.clone(), first, "Deploys happen at dawn"),
        delete(agent, first),
        // The digest is the Speaker's.
        NewEvent::new(committee(), Kind::Note, person_addr("alice"), "- a digest").with_meta(
            meta::DIGEST,
            DigestPayload {
                trigger: DigestTrigger::Daily,
                from: Seq(1),
                through: Seq(1),
                points: vec![DigestPoint {
                    text: "a digest".into(),
                    refs: vec![Seq(1)],
                }],
            }
            .to_value(),
        ),
    ];
    for event in refused {
        let error = api_error(
            server.post_event(ALICE, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
        assert!(
            error.message.contains("memory") || error.message.contains("digest"),
            "refused for its memory: {}",
            error.message
        );
    }

    // A person corrects and deletes.
    let response = server
        .post_event(
            ALICE,
            &correct(person_addr("alice"), first, "Deploys happen at night"),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        search(&server, "deploys").await,
        [(first + 1, "Deploys happen at night".to_string())]
    );
    let response = server
        .post_event(ALICE, &delete(person_addr("alice"), first + 1))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(search(&server, "deploys").await.is_empty());
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_fresh_server_rebuilds_memory_from_the_log() {
    let store: Arc<dyn LogStore> = Arc::new(MemoryStore::new());
    let server = TestServer::start_with(store.clone(), None).await;
    let hub = &server.running.hub;
    let noon = hub
        .append(point("Deploys happen at noon", &[1], &[]))
        .await
        .unwrap()
        .event
        .seq
        .0;
    hub.append(point("Deploys happen at night", &[2], &[noon]))
        .await
        .unwrap();
    hub.append(point("发布日期定在周五", &[3], &[]))
        .await
        .unwrap();
    let before = (
        search(&server, "deploys").await,
        search(&server, "发布").await,
    );
    let brief_before = hub.brief(&committee()).await.unwrap().memory;
    server.running.shutdown().await.unwrap();

    let server = TestServer::start_with(store, None).await;
    let after = (
        search(&server, "deploys").await,
        search(&server, "发布").await,
    );
    assert_eq!(before, after);
    assert_eq!(after.0.len(), 1, "the superseded point is not in force");
    assert_eq!(
        server.running.hub.brief(&committee()).await.unwrap().memory,
        brief_before
    );
    server.running.shutdown().await.unwrap();
}
