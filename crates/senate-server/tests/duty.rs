//! A seat's duty: who may set it, and where the server shows it (presence,
//! the brief, the facts a Speaker or chair picks by).

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use reqwest::{Method, StatusCode};
use senate_core::api::{PresencePage, SetChair};
use senate_core::{Address, Brief, DutyPayload, Kind, NewEvent, meta};

fn duty(from: Address, senator: &str, text: &str) -> NewEvent {
    DutyPayload {
        senator: path(senator),
        duty: text.into(),
    }
    .note(committee(), from)
}

async fn get<T: serde::de::DeserializeOwned>(server: &TestServer, route: &str) -> T {
    let response = server
        .request(Method::GET, route, ALICE)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{route}");
    response.json().await.unwrap()
}

async fn accepted(server: &TestServer, token: &str, event: &NewEvent) {
    let response = server.post_event(token, event).await;
    let status = response.status();
    assert_eq!(status, StatusCode::OK, "{}", response.text().await.unwrap());
}

/// bob runs claude, alice runs codex; both are seated on the committee.
async fn seated_pair() -> TestServer {
    let server = TestServer::start().await;
    server.append("hello").await;
    for (token, agent) in [(BOB, "claude"), (ALICE, "codex")] {
        accepted(
            &server,
            token,
            &NewEvent::new(committee(), Kind::Note, agent_addr(agent), "here"),
        )
        .await;
    }
    server
}

#[tokio::test]
async fn the_agent_its_owner_and_the_chair_set_a_duty_and_nobody_else() {
    let server = seated_pair().await;
    let hub = &server.running.hub;

    // The agent itself.
    accepted(
        &server,
        BOB,
        &duty(agent_addr("claude"), "claude", "reviews"),
    )
    .await;
    assert_eq!(
        hub.duties(&committee()).await.unwrap().get(&path("claude")),
        Some(&"reviews".to_owned())
    );
    // Its owner, as themselves.
    accepted(
        &server,
        BOB,
        &duty(person_addr("bob"), "claude", "owns the app"),
    )
    .await;
    assert_eq!(
        hub.duties(&committee()).await.unwrap().get(&path("claude")),
        Some(&"owns the app".to_owned())
    );
    // Another agent, and another person, are refused.
    api_error(
        server
            .post_event(ALICE, &duty(agent_addr("codex"), "claude", "taken"))
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    api_error(
        server
            .post_event(CAROL, &duty(person_addr("carol"), "claude", "taken"))
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    // The chair: claude, the first agent seated, sets codex's duty.
    accepted(
        &server,
        BOB,
        &duty(agent_addr("claude"), "codex", "builds the server"),
    )
    .await;
    // Once carol chairs, she may too.
    let response = server
        .request(
            Method::PUT,
            &senate_core::api::chair_path(&committee()),
            CAROL,
        )
        .json(&SetChair {
            senator: path("carol"),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    accepted(
        &server,
        CAROL,
        &duty(person_addr("carol"), "codex", "ships"),
    )
    .await;
    // ...and claude, no longer chair, may not.
    api_error(
        server
            .post_event(BOB, &duty(agent_addr("claude"), "codex", "again"))
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;

    let duties = hub.duties(&committee()).await.unwrap();
    assert_eq!(duties.get(&path("codex")), Some(&"ships".to_owned()));
    // An empty duty clears it.
    accepted(&server, BOB, &duty(agent_addr("claude"), "claude", "")).await;
    assert_eq!(
        hub.duties(&committee()).await.unwrap().get(&path("claude")),
        None
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_duty_must_be_sound_and_ride_on_a_note() {
    let server = seated_pair().await;
    // Too long, or more than one line.
    for text in ["x".repeat(301), "two\nlines".to_owned()] {
        api_error(
            server
                .post_event(BOB, &duty(agent_addr("claude"), "claude", &text))
                .await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        )
        .await;
    }
    // Not on a question.
    let mut question = duty(agent_addr("claude"), "claude", "reviews");
    question.kind = Kind::Question;
    question.to = vec![agent_addr("codex")];
    api_error(
        server.post_event(BOB, &question).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    // Not as the Speaker's office.
    let mut speaker = duty(Address::Speaker, "claude", "reviews");
    speaker.meta.insert(meta::ACTED_BY.into(), "claude".into());
    let response = server.post_event(BOB, &speaker).await;
    assert!(response.status().is_client_error());
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn presence_the_brief_and_the_seat_facts_show_the_duty() {
    let server = seated_pair().await;
    let _claude = server.stream_as(BOB, Some("claude"), None, None).await;
    assert_eq!(server.heartbeat(BOB, "claude").await.status(), StatusCode::NO_CONTENT);
    accepted(
        &server,
        BOB,
        &duty(agent_addr("claude"), "claude", "reviews the server"),
    )
    .await;

    let route = senate_core::api::presence_path(&committee());
    eventually("claude's presence", || async {
        let page: PresencePage = get(&server, &route).await;
        page.agents.iter().any(|p| p.agent == path("claude"))
    })
    .await;
    let page: PresencePage = get(&server, &route).await;
    let claude = page
        .agents
        .iter()
        .find(|p| p.agent == path("claude"))
        .unwrap();
    assert_eq!(claude.duty.as_deref(), Some("reviews the server"));

    let brief: Brief = get(&server, &senate_core::api::brief_path(&committee())).await;
    let claude = brief
        .presence
        .iter()
        .find(|p| p.agent == path("claude"))
        .unwrap();
    assert_eq!(claude.duty.as_deref(), Some("reviews the server"));
    assert!(
        brief
            .to_markdown(senate_core::Locale::En, 8_000)
            .contains("· duty: reviews the server"),
        "{}",
        brief.to_markdown(senate_core::Locale::En, 8_000)
    );

    let docket = server.running.hub.chair_docket(&committee()).await.unwrap();
    let seat = docket
        .seats
        .iter()
        .find(|s| s.agent == path("claude"))
        .unwrap();
    assert_eq!(seat.duty.as_deref(), Some("reviews the server"));
    let codex = docket
        .seats
        .iter()
        .find(|s| s.agent == path("codex"))
        .unwrap();
    assert_eq!(codex.duty, None);
    server.running.shutdown().await.unwrap();
}
