//! Senator aliases kept per committee: setting, resolving, removing, and
//! the migration from the Senate-wide map of before.

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use reqwest::{Method, StatusCode};
use senate_core::alias::CommitteeAliases;
use senate_core::api::{self, AppendResponse, Heartbeat, SenatorAliases};
use senate_core::{CommitteeId, CommitteeSeatAction, EventId, Kind, NewEvent, Seq};
use senate_log::{LogStore, MemoryStore};
use serde_json::json;

const A: &str = "claude-alias-a";
const B: &str = "claude-alias-b";

fn c(s: &str) -> CommitteeId {
    CommitteeId::new(s).unwrap()
}

/// Seat `agent` on `committee` with alice's token.
async fn sit(server: &TestServer, committee: &str, agent: &str) {
    let response = server
        .request(Method::POST, &api::heartbeat_path(&c(committee)), ALICE)
        .json(&Heartbeat {
            agent: path(agent),
            sessions: 1,
            cursor: Seq::ZERO,
            left: false,
        })
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success(), "{}", response.status());
}

async fn set(server: &TestServer, primary: &str, body: serde_json::Value) -> reqwest::Response {
    server
        .request(Method::PUT, &format!("/v1/aliases/{primary}"), ALICE)
        .json(&body)
        .send()
        .await
        .unwrap()
}

async fn table(server: &TestServer) -> CommitteeAliases {
    server
        .request(Method::GET, "/v1/committee-aliases", ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn flat(server: &TestServer) -> SenatorAliases {
    server
        .request(Method::GET, "/v1/aliases", ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// A note from alice on `committee` to `to`.
async fn tell(server: &TestServer, committee: &str, to: &str) -> reqwest::Response {
    let event =
        NewEvent::new(c(committee), Kind::Note, person_addr("alice"), "hello").to(agent_addr(to));
    server.post_event(ALICE, &event).await
}

async fn two_mains() -> TestServer {
    let server = TestServer::start().await;
    sit(&server, "nous", A).await;
    sit(&server, "nous-ios", B).await;
    for (primary, committee) in [(A, "nous"), (B, "nous-ios")] {
        let response = set(
            &server,
            primary,
            json!({"alias": "main", "committee": committee}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    server
}

#[tokio::test]
async fn the_same_alias_lives_on_two_committees() {
    let server = two_mains().await;
    let t = table(&server).await;
    assert_eq!(t.get(&c("nous"), &path("main")), Some(&path(A)));
    assert_eq!(t.get(&c("nous-ios"), &path("main")), Some(&path(B)));
    // The flat view older bridges read leaves out an alias of two agents.
    assert_eq!(flat(&server).await.get(&path("main")), None);

    // Taken on its own committee by another agent.
    let taken = set(&server, B, json!({"alias": "main", "committee": "nous"})).await;
    assert!(!taken.status().is_success());
    // A person's handle is never an alias.
    let handle = set(&server, A, json!({"alias": "bob", "committee": "nous"})).await;
    assert!(!handle.status().is_success());
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_bare_alias_resolves_on_the_writers_committee_or_lists_candidates() {
    let server = two_mains().await;
    let response = tell(&server, "nous-ios", "main").await;
    assert_eq!(response.status(), StatusCode::OK);
    let appended: AppendResponse = response.json().await.unwrap();
    assert_eq!(appended.event.to, vec![agent_addr(B)]);

    let refused = tell(&server, "refund-v2", "main").await;
    assert!(!refused.status().is_success());
    let text = refused.text().await.unwrap();
    assert!(
        text.contains("nous::main") && text.contains("nous-ios::main"),
        "{text}"
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_single_alias_resolves_from_any_committee() {
    let server = TestServer::start().await;
    sit(&server, "nous", A).await;
    assert_eq!(
        set(&server, A, json!({"alias": "main", "committee": "nous"}))
            .await
            .status(),
        StatusCode::OK
    );
    let response = tell(&server, "refund-v2", "main").await;
    assert_eq!(response.status(), StatusCode::OK);
    let appended: AppendResponse = response.json().await.unwrap();
    assert_eq!(appended.event.to, vec![agent_addr(A)]);
    assert_eq!(flat(&server).await.get(&path("main")), Some(&path(A)));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_older_bridge_naming_no_committee_uses_the_agents_only_one() {
    let server = TestServer::start().await;
    sit(&server, "nous", A).await;
    let response = set(&server, A, json!({"alias": "main"})).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        table(&server).await.get(&c("nous"), &path("main")),
        Some(&path(A))
    );

    sit(&server, "nous-ios", A).await;
    let refused = set(&server, A, json!({"alias": "lead"})).await;
    assert!(!refused.status().is_success());
    assert!(refused.text().await.unwrap().contains("upgrade"));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn removing_an_alias_is_for_its_runner_or_the_speakers_office() {
    let server = two_mains().await;
    let forbidden = server
        .request(Method::DELETE, &format!("/v1/aliases/{A}/main"), BOB)
        .query(&[("committee", "nous")])
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    let removed = server
        .request(Method::DELETE, &format!("/v1/aliases/{A}/main"), ALICE)
        .query(&[("committee", "nous")])
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    let t = table(&server).await;
    assert_eq!(t.get(&c("nous"), &path("main")), None);
    assert_eq!(t.get(&c("nous-ios"), &path("main")), Some(&path(B)));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn global_aliases_move_to_the_committees_their_agents_sit_in() {
    let store: Arc<dyn LogStore> = Arc::new(MemoryStore::new());
    let server = TestServer::start_with(store.clone(), None).await;
    sit(&server, "nous", A).await;
    sit(&server, "nous-ios", A).await;
    sit(&server, "senate", B).await;
    // An agent seated once and then unseated from its only committee
    // (like `smoke` and `namecheck`).
    assert_eq!(
        server.act_as(ALICE, "claude-alias-gone").await,
        StatusCode::OK
    );
    let unseat = server
        .request(Method::PUT, &api::committee_seat_path(&committee()), ALICE)
        .json(&api::SetCommitteeSeat {
            senator: path("claude-alias-gone"),
            action: CommitteeSeatAction::Unseat,
            id: EventId::new(),
            acted_by: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(unseat.status(), StatusCode::OK);
    server.running.shutdown().await.unwrap();

    // What a server before per-committee aliases kept.
    let global = BTreeMap::from([
        (path("main"), path(A)),
        (path("review"), path(B)),
        (path("smoke"), path("claude-alias-gone")),
    ]);
    store
        .put_registry(
            "senator-aliases",
            Some(serde_json::to_value(&global).unwrap()),
        )
        .await
        .unwrap();

    let server = TestServer::start_with(store.clone(), None).await;
    let t = table(&server).await;
    let rows: Vec<(String, String)> = t
        .iter()
        .map(|q| (q.to_string(), q.primary.to_string()))
        .collect();
    assert_eq!(
        rows,
        [
            ("nous::main".to_string(), A.to_string()),
            ("nous-ios::main".to_string(), A.to_string()),
            ("senate::review".to_string(), B.to_string()),
        ]
    );
    assert_eq!(store.registry_value("senator-aliases").await.unwrap(), None);
    // Both of A's entries stand for one agent, so the bare alias resolves.
    let response = tell(&server, "refund-v2", "main").await;
    assert_eq!(response.status(), StatusCode::OK);
    server.running.shutdown().await.unwrap();

    // Migrated once: a restart keeps the table as it is.
    let server = TestServer::start_with(store, None).await;
    assert_eq!(table(&server).await, t);
    server.running.shutdown().await.unwrap();
}
