#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;

use common::*;
use reqwest::{Method, Response, StatusCode};
use senate_core::api::{self, Secretaries};
use senate_core::secretary_work::{
    SecretaryReminder, SecretaryWorkOp, SecretaryWorkReply, SecretaryWorkRequest,
    SecretaryWorkUpdate, SetSecretarySession,
};
use senate_core::{Address, EventId, Kind, NewEvent, meta};
use senate_log::SqliteStore;
use senate_server::ServerConfig;

const MEMBER: &str = "claude-team-member";
const SPEAKER: &str = "codex-presider";

fn configured(config: &mut ServerConfig) {
    config.roster.speaker = Some(format!("senator::{SPEAKER}").parse().unwrap());
    config.roster.agents = vec![seated(MEMBER, "alice"), seated(SPEAKER, "bob")];
    config
        .roster
        .secretaries
        .insert(path("alice"), vec![path(MEMBER)]);
}

async fn operation(
    server: &TestServer,
    token: &str,
    principal: &str,
    actor: &str,
    operation: SecretaryWorkOp,
) -> Response {
    server
        .request(
            Method::POST,
            &api::secretary_work_path(&path(principal)),
            token,
        )
        .json(&SecretaryWorkRequest {
            actor: path(actor),
            operation,
        })
        .send()
        .await
        .unwrap()
}

async fn reply(
    server: &TestServer,
    token: &str,
    principal: &str,
    actor: &str,
    op: SecretaryWorkOp,
) -> SecretaryWorkReply {
    let response = operation(server, token, principal, actor, op).await;
    assert_eq!(response.status(), StatusCode::OK);
    response.json().await.unwrap()
}

async fn reminders(server: &TestServer, token: &str) -> Vec<SecretaryReminder> {
    server
        .request(Method::POST, api::SECRETARY_REMINDERS_PATH, token)
        .json(&[path(MEMBER)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test]
async fn private_work_and_reminders_follow_team_authority_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("senate.db");
    let server =
        TestServer::start_configured(Arc::new(SqliteStore::open(&db).await.unwrap()), configured)
            .await;
    let item = EventId::new();
    let unknown = SetSecretarySession {
        senator: path("claude-unregistered"),
        appointed: true,
        acted_by: None,
    };
    api_error(
        server
            .request(
                Method::PUT,
                &api::secretary_session_path(&path("alice")),
                ALICE,
            )
            .json(&unknown)
            .send()
            .await
            .unwrap(),
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    assert!(!server.running.hub.roster_now().knows(&unknown.senator));
    let roster = reply(&server, ALICE, "alice", "alice", SecretaryWorkOp::Members).await;
    assert!(
        matches!(roster, SecretaryWorkReply::Members { members, .. } if members == [path(MEMBER)])
    );
    let create = |id, title: &str| SecretaryWorkOp::Create {
        id,
        title: title.into(),
        thread: None,
    };
    reply(
        &server,
        ALICE,
        "alice",
        MEMBER,
        create(item.clone(), "Private task"),
    )
    .await;
    let office_item = EventId::new();
    reply(
        &server,
        BOB,
        SPEAKER,
        "bob",
        create(office_item, "Speaker's work"),
    )
    .await;
    let as_speaker = reply(
        &server,
        BOB,
        SPEAKER,
        SPEAKER,
        SecretaryWorkOp::List { after: None },
    )
    .await;
    assert!(matches!(as_speaker, SecretaryWorkReply::List { items, .. } if items.len() == 1));
    api_error(
        operation(
            &server,
            ALICE,
            SPEAKER,
            "alice",
            SecretaryWorkOp::List { after: None },
        )
        .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    api_error(
        operation(
            &server,
            BOB,
            "alice",
            MEMBER,
            SecretaryWorkOp::List { after: None },
        )
        .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;

    let forged = NewEvent::new(
        committee(),
        Kind::Note,
        Address::Speaker,
        "forged office write",
    )
    .with_meta(meta::ACTED_BY, MEMBER.into());
    api_error(
        server.post_event(ALICE, &forged).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;

    let change = |update| SecretaryWorkOp::Change {
        item: item.clone(),
        id: EventId::new(),
        update,
    };
    reply(
        &server,
        ALICE,
        "alice",
        "alice",
        change(SecretaryWorkUpdate::Assign { to: path(MEMBER) }),
    )
    .await;
    let expected = reminders(&server, ALICE).await;
    assert_eq!(expected.len(), 1);
    assert!(reminders(&server, BOB).await.is_empty());
    api_error(
        server
            .request(Method::PUT, api::SECRETARY_REMINDERS_PATH, BOB)
            .json(&expected[0])
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();

    let server =
        TestServer::start_configured(Arc::new(SqliteStore::open(&db).await.unwrap()), configured)
            .await;
    assert_eq!(reminders(&server, ALICE).await, expected);
    let ack = || {
        server
            .request(Method::PUT, api::SECRETARY_REMINDERS_PATH, ALICE)
            .json(&expected[0])
            .send()
    };
    assert_eq!(ack().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(ack().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert!(reminders(&server, ALICE).await.is_empty());
    reply(
        &server,
        ALICE,
        "alice",
        "alice",
        change(SecretaryWorkUpdate::Assign { to: path(MEMBER) }),
    )
    .await;
    let current = reminders(&server, ALICE).await;
    assert_eq!(current.len(), 1);
    assert_ne!(expected[0].id, current[0].id);
    assert_eq!(ack().await.unwrap().status(), StatusCode::NO_CONTENT);
    assert_eq!(reminders(&server, ALICE).await, current);
    let dismiss = SetSecretarySession {
        senator: path(MEMBER),
        appointed: false,
        acted_by: None,
    };
    let path_string = api::secretary_session_path(&path("alice"));
    api_error(
        server
            .request(Method::PUT, &path_string, BOB)
            .json(&dismiss)
            .send()
            .await
            .unwrap(),
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    let after: Secretaries = server
        .request(Method::PUT, &path_string, ALICE)
        .json(&dismiss)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!after.contains_key(&path("alice")));
    assert!(reminders(&server, ALICE).await.is_empty());
    server.running.shutdown().await.unwrap();

    let server =
        TestServer::start_configured(Arc::new(SqliteStore::open(&db).await.unwrap()), configured)
            .await;
    let read = || SecretaryWorkOp::Read {
        item: item.clone(),
        offset: 0,
    };
    api_error(
        operation(&server, ALICE, "alice", MEMBER, read()).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    let history = reply(&server, ALICE, "alice", "alice", read()).await;
    let SecretaryWorkReply::History { item, entries, .. } = history else {
        panic!("{history:?}")
    };
    assert_eq!(item.owner, None);
    assert_eq!(entries.len(), 2);
    assert!(reminders(&server, ALICE).await.is_empty());
    let reappoint = SetSecretarySession {
        appointed: true,
        ..dismiss
    };
    assert_eq!(
        server
            .request(Method::PUT, &path_string, ALICE)
            .json(&reappoint)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let history = reply(&server, ALICE, "alice", MEMBER, read()).await;
    let SecretaryWorkReply::History { item, entries, .. } = history else {
        panic!("{history:?}")
    };
    assert_eq!(item.owner, None);
    assert_eq!(entries.len(), 2);
    assert!(reminders(&server, ALICE).await.is_empty());
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn members_can_resume_both_lists_and_history() {
    let server =
        TestServer::start_configured(Arc::new(senate_log::MemoryStore::new()), configured).await;
    let mut ids = Vec::new();
    for index in 0..18 {
        let id = EventId::new();
        reply(
            &server,
            ALICE,
            "alice",
            "alice",
            SecretaryWorkOp::Create {
                id: id.clone(),
                title: format!("Work {index}"),
                thread: None,
            },
        )
        .await;
        ids.push(id);
    }
    ids.sort();
    let head = reply(
        &server,
        ALICE,
        "alice",
        MEMBER,
        SecretaryWorkOp::List { after: None },
    )
    .await;
    let SecretaryWorkReply::List {
        mut items,
        next: Some(after),
    } = head
    else {
        panic!("{head:?}")
    };
    let tail = reply(
        &server,
        ALICE,
        "alice",
        MEMBER,
        SecretaryWorkOp::List { after: Some(after) },
    )
    .await;
    let SecretaryWorkReply::List {
        items: rest,
        next: None,
    } = tail
    else {
        panic!("{tail:?}")
    };
    items.extend(rest);
    assert_eq!(
        items.into_iter().map(|item| item.id).collect::<Vec<_>>(),
        ids
    );
    let id = ids[0].clone();
    for index in 0..18 {
        reply(
            &server,
            ALICE,
            "alice",
            MEMBER,
            SecretaryWorkOp::Change {
                item: id.clone(),
                id: EventId::new(),
                update: SecretaryWorkUpdate::Note {
                    text: format!("note {index}"),
                },
            },
        )
        .await;
    }
    let head = reply(
        &server,
        ALICE,
        "alice",
        "alice",
        SecretaryWorkOp::Read {
            item: id.clone(),
            offset: 0,
        },
    )
    .await;
    let SecretaryWorkReply::History {
        mut entries,
        next: Some(offset),
        ..
    } = head
    else {
        panic!("{head:?}")
    };
    let tail = reply(
        &server,
        ALICE,
        "alice",
        "alice",
        SecretaryWorkOp::Read { item: id, offset },
    )
    .await;
    let SecretaryWorkReply::History {
        entries: rest,
        next: None,
        ..
    } = tail
    else {
        panic!("{tail:?}")
    };
    entries.extend(rest);
    assert_eq!(entries.len(), 18);
    assert_eq!(
        entries[17].update,
        SecretaryWorkUpdate::Note {
            text: "note 17".into()
        }
    );
    server.running.shutdown().await.unwrap();
}
