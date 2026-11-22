//! The HTTP/SSE API against a live server on 127.0.0.1:0 (`memory:` store).

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;
use std::time::Duration;

use chrono::DateTime;
use common::*;
use reqwest::{Method, StatusCode};
use senate_core::api::{
    AppendResponse, Cursor, DEFAULT_SEARCH, EventsPage, Heartbeat, MAX_PAGE, MAX_SEARCH, Me,
    RESOLVE_PATH, Resolved, SearchResults, ThreadsPage, brief_path, cursor_path, events_path,
    heartbeat_path, search_path, stream_path, threads_path,
};
use senate_core::{
    Address, Brief, Hub, Kind, Locator, MirrorRef, NewEvent, PROPOSAL_DECISION, People, Seq,
    StatusPayload, StatusState, meta,
};
use senate_log::MemoryStore;
use serde_json::json;

#[tokio::test]
async fn healthz_needs_no_auth() {
    let server = TestServer::start().await;
    let response = server
        .client
        .get(server.url("/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok");
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_v1_route_requires_a_token() {
    let server = TestServer::start().await;
    let f = committee();
    let routes = [
        (Method::GET, "/v1/me".to_string()),
        (Method::GET, events_path(&f)),
        (Method::POST, events_path(&f)),
        (Method::GET, stream_path(&f)),
        (Method::GET, cursor_path(&f, "delivered")),
        (Method::PUT, cursor_path(&f, "delivered")),
        (Method::POST, heartbeat_path(&f)),
        (Method::GET, format!("{}?q=refund", search_path(&f))),
        (Method::GET, brief_path(&f)),
        (Method::GET, threads_path(&f)),
        (Method::GET, format!("{RESOLVE_PATH}?name=senator::alice")),
    ];
    for (method, path) in routes {
        let missing = server
            .client
            .request(method.clone(), server.url(&path))
            .send()
            .await
            .unwrap();
        assert_eq!(
            missing.headers()["www-authenticate"].to_str().unwrap(),
            "Bearer"
        );
        api_error(missing, StatusCode::UNAUTHORIZED, "unauthorized").await;

        let wrong = server
            .request(method.clone(), &path, "not-a-token")
            .send()
            .await
            .unwrap();
        api_error(wrong, StatusCode::UNAUTHORIZED, "unauthorized").await;

        let basic = server
            .client
            .request(method, server.url(&path))
            .header("authorization", format!("Basic {ALICE}"))
            .send()
            .await
            .unwrap();
        api_error(basic, StatusCode::UNAUTHORIZED, "unauthorized").await;
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_routes_and_methods_are_api_errors() {
    let server = TestServer::start().await;
    let missing = server
        .request(Method::GET, "/v1/nope", ALICE)
        .send()
        .await
        .unwrap();
    api_error(missing, StatusCode::NOT_FOUND, "not_found").await;
    let wrong_method = server
        .request(Method::DELETE, &events_path(&committee()), ALICE)
        .send()
        .await
        .unwrap();
    api_error(wrong_method, StatusCode::METHOD_NOT_ALLOWED, "bad_request").await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn me_names_the_person_their_agents_and_the_committees_in_the_log() {
    let server = TestServer::start().await;
    let me: Me = server
        .request(Method::GET, "/v1/me", BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.person, name("bob"));
    assert_eq!(
        me.senate,
        name("senate"),
        "a Senate configured with no name"
    );
    assert_eq!(me.parent, None, "and no Senate above");
    assert_eq!(
        me.people,
        People::new(["alice", "bob", "carol"].map(name)),
        "every senator not among the people is an agent"
    );
    assert!(me.agents.is_empty(), "no agent has connected yet");
    assert!(me.committees.is_empty());

    server.append("hello").await;
    let me: Me = server
        .request(Method::GET, "/v1/me", BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.committees, vec![committee()]);

    // An agent that connects is seated, and is listed for its person only.
    let seated = server
        .request(Method::GET, &cursor_path(&committee(), "delivered"), BOB)
        .query(&[("senator", "claude")])
        .send()
        .await
        .unwrap();
    assert_eq!(seated.status(), StatusCode::OK);
    for (token, agents) in [(BOB, vec![path("claude")]), (ALICE, vec![])] {
        let me: Me = server
            .request(Method::GET, "/v1/me", token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(me.agents, agents);
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_person_acts_as_themselves_and_their_own_agents_only() {
    let server = TestServer::start().await;
    let f = committee();
    // Bob runs claude: he acted as it first.
    assert_eq!(server.act_as(BOB, "claude").await, StatusCode::OK);
    for from in ["copilot", "codex", "alice"] {
        let event = NewEvent::new(f.clone(), Kind::Question, person_addr(from), "q");
        assert_eq!(
            server.post_event(ALICE, &event).await.status(),
            StatusCode::OK,
            "{from}"
        );
    }
    assert_eq!(
        server.post_event(ALICE, &note("as user")).await.status(),
        StatusCode::OK
    );

    let forbidden = [
        // Somebody else's person and agent.
        person_addr("bob"),
        person_addr("claude"),
        // Not senators at all.
        Address::Speaker,
        Address::Committee,
        Address::Guest("chat/u_1".parse().unwrap()),
    ];
    for from in forbidden {
        let event = NewEvent::new(f.clone(), Kind::Note, from, "impersonation");
        let response = server.post_event(ALICE, &event).await;
        api_error(response, StatusCode::FORBIDDEN, "forbidden").await;
    }
    // Bob cannot post Alice's note either.
    api_error(
        server.post_event(BOB, &note("not bob")).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    // A person who is not on the roster at all is nobody.
    let stored = server.running.hub.read(&f, Seq::ZERO, 100).await.unwrap();
    // Seq 1 is claude's seat (its cursor was the first contact on the
    // committee); the four appends that followed are 2 to 5.
    assert_eq!(
        seqs(&stored),
        vec![1, 2, 3, 4, 5],
        "rejected appends store nothing"
    );

    // The agents alice acted as were seated under her; bob's stays his.
    let roster = server.running.hub.roster().await.unwrap();
    let seated: Vec<_> = roster.agents().iter().map(ToString::to_string).collect();
    assert_eq!(seated, ["claude", "copilot", "codex"]);
    assert_eq!(
        roster.agents_of(&name("alice")),
        [&path("copilot"), &path("codex")]
    );
    assert_eq!(roster.owner_of(&path("claude")), Some(&name("bob")));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_is_seated_on_first_contact_by_any_request_that_acts_as_it() {
    let server = TestServer::start().await;
    let f = committee();
    let seated = |server: &TestServer| {
        let hub = server.running.hub.clone();
        async move {
            hub.roster()
                .await
                .unwrap()
                .agents()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        }
    };
    assert!(seated(&server).await.is_empty());

    // A heartbeat, a stream, a cursor and an append each seat their agent.
    beat(&server, ALICE, "a", 0).await;
    assert_eq!(seated(&server).await, ["a"]);
    let _stream = server.stream_as(ALICE, Some("b"), None, None).await;
    server
        .request(Method::GET, &cursor_path(&f, "read"), ALICE)
        .query(&[("senator", "c")])
        .send()
        .await
        .unwrap();
    server
        .post_event(
            ALICE,
            &NewEvent::new(f.clone(), Kind::Note, person_addr("d"), "hi"),
        )
        .await;
    assert_eq!(seated(&server).await, ["a", "b", "c", "d"]);

    // Contact again: seated once. A refused request seats nothing.
    beat(&server, ALICE, "a", 0).await;
    assert_eq!(server.act_as(BOB, "claude").await, StatusCode::OK);
    let refused = server
        .request(Method::POST, &heartbeat_path(&f), ALICE)
        .json(&Heartbeat {
            paused: false,
            agent: path("claude"),
            sessions: 1,
            cursor: Seq::ZERO,
            left: false,
        })
        .send()
        .await
        .unwrap();
    api_error(refused, StatusCode::FORBIDDEN, "forbidden").await;
    assert_eq!(seated(&server).await, ["a", "b", "c", "d", "claude"]);
    server.running.shutdown().await.unwrap();
}

async fn beat(server: &TestServer, token: &str, agent: &str, cursor: u64) {
    let response = server
        .request(Method::POST, &heartbeat_path(&committee()), token)
        .json(&Heartbeat {
            paused: false,
            agent: path(agent),
            sessions: 1,
            cursor: Seq(cursor),
            left: false,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn senators_may_not_author_chat() {
    let server = TestServer::start().await;
    let f = committee();
    // `chat` is what a human said on a platform. An agent holding its
    // person's token must not be able to put words in a human's mouth, as
    // itself or as its user, threaded or not.
    for event in [
        NewEvent::new(f.clone(), Kind::Chat, person_addr("alice"), "x"),
        NewEvent::new(f.clone(), Kind::Chat, agent_addr("copilot"), "x"),
        NewEvent::new(f.clone(), Kind::Chat, person_addr("alice"), "不对")
            .in_thread(Seq(1))
            .to(agent_addr("claude")),
    ] {
        let error = api_error(
            server.post_event(ALICE, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
        assert!(error.message.contains("chat"), "{error:?}");
    }
    assert!(
        server
            .running
            .hub
            .read(&f, ZERO, 10)
            .await
            .unwrap()
            .is_empty()
    );

    // The server's own components (a gateway) append it in-process.
    let mut chat = note("from a platform");
    chat.kind = Kind::Chat;
    let appended = server.running.hub.append(chat).await.unwrap();
    assert_eq!(appended.event.kind, Kind::Chat);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn senators_cannot_state_server_provenance() {
    let server = TestServer::start().await;
    let f = committee();
    let me = agent_addr("copilot");
    let mut forged = vec![
        NewEvent::new(f.clone(), Kind::Binding, me.clone(), "bind"),
        NewEvent::new(f.clone(), Kind::Presence, me.clone(), "online"),
        NewEvent::new(f.clone(), Kind::Answer, me.clone(), "for bob")
            .on_behalf_of(agent_addr("claude")),
        NewEvent::new(f.clone(), Kind::Note, me.clone(), "from chat").with_source(
            senate_core::Source {
                platform: "chat".into(),
                group_id: "g".into(),
                group_name: None,
                simulated: false,
                simulated_by: None,
            },
        ),
    ];
    forged.push(
        NewEvent::new(f.clone(), Kind::Note, me, "mirrored").with_mirror(senate_core::Mirror {
            platform: "chat".into(),
            message_ref: "m".into(),
            thread_ref: None,
        }),
    );
    for event in forged {
        api_error(
            server.post_event(ALICE, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }
    let stored = server.running.hub.read(&f, Seq::ZERO, 100).await.unwrap();
    assert!(stored.is_empty(), "rejected appends store nothing");
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn only_the_speaker_writes_the_brief_top_and_only_people_decide() {
    let server = TestServer::start().await;
    let f = committee();
    let agent = agent_addr("copilot");
    let user = person_addr("alice");
    let refused = [
        // The key at all, whatever its value or the author.
        NewEvent::new(f.clone(), Kind::Note, agent.clone(), "1. top")
            .with_meta(meta::BRIEF_TOP, json!(true)),
        NewEvent::new(f.clone(), Kind::Note, user.clone(), "1. top")
            .with_meta(meta::BRIEF_TOP, json!(false)),
        NewEvent::new(f.clone(), Kind::Decision, user.clone(), "top decision")
            .with_meta(meta::BRIEF_TOP, json!(true)),
        // Agents propose; they do not decide.
        NewEvent::new(f.clone(), Kind::Decision, agent.clone(), "Idempotency-Key"),
    ];
    for event in refused {
        api_error(
            server.post_event(ALICE, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }
    assert!(
        server
            .running
            .hub
            .read(&f, ZERO, 10)
            .await
            .unwrap()
            .is_empty(),
        "rejected appends store nothing"
    );

    let allowed = [
        NewEvent::new(f.clone(), Kind::Note, agent, "Idempotency-Key")
            .with_meta(meta::PROPOSAL, json!(PROPOSAL_DECISION)),
        NewEvent::new(f.clone(), Kind::Decision, user, "退款上限 30 天"),
    ];
    for event in allowed {
        let response = server.post_event(ALICE, &event).await;
        assert_eq!(response.status(), StatusCode::OK);
    }
    // The server's own components are not refused.
    server
        .running
        .hub
        .append(
            NewEvent::new(f.clone(), Kind::Note, Address::Speaker, "1. 上限 30 天")
                .with_meta(meta::BRIEF_TOP, json!(true)),
        )
        .await
        .unwrap();
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn malformed_appends_are_bad_requests() {
    let server = TestServer::start().await;
    let mut elsewhere = note("wrong committee");
    elsewhere.committee = senate_core::CommitteeId::new("search").unwrap();
    let mismatched = server
        .request(Method::POST, &events_path(&committee()), ALICE)
        .json(&elsewhere)
        .send()
        .await
        .unwrap();
    api_error(mismatched, StatusCode::BAD_REQUEST, "bad_request").await;

    let bad_path = server
        .request(Method::POST, "/v1/committees/Not_A_Slug/events", ALICE)
        .json(&note("x"))
        .send()
        .await
        .unwrap();
    api_error(bad_path, StatusCode::BAD_REQUEST, "bad_request").await;

    let not_json = server
        .request(Method::POST, &events_path(&committee()), ALICE)
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    api_error(not_json, StatusCode::BAD_REQUEST, "bad_request").await;

    // Unknown address forms decode (as `Address::Other`) but are not valid
    // input, wherever they appear.
    for body in [
        json!({"committee": "refund-v2", "kind": "note", "from": "robot:x", "body": "b"}),
        json!({"committee": "refund-v2", "kind": "note", "from": "senator::alice",
               "to": ["senator::claude", "group:g-1"], "body": "b"}),
        json!({"committee": "refund-v2", "kind": "note", "from": "senator::alice",
               "to": ["agent:Not-A-Slug"], "body": "b"}),
    ] {
        let bad_address = server
            .request(Method::POST, &events_path(&committee()), ALICE)
            .json(&body)
            .send()
            .await
            .unwrap();
        api_error(bad_address, StatusCode::BAD_REQUEST, "bad_request").await;
    }
    assert!(server.running.hub.committees().await.unwrap().is_empty());
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn append_then_read_pages_in_seq_order() {
    let server = TestServer::start().await;
    for n in 1..=7 {
        let event = server.append(&format!("m{n}")).await;
        assert_eq!(event.seq, Seq(n));
        assert_eq!(event.committee, committee());
    }
    let page = |after: u64, limit: u64| {
        let path = format!("{}?after={after}&limit={limit}", events_path(&committee()));
        let request = server.request(Method::GET, &path, BOB);
        async move {
            let page: EventsPage = request.send().await.unwrap().json().await.unwrap();
            page
        }
    };
    let first = page(0, 3).await;
    assert_eq!(seqs(&first.events), vec![1, 2, 3]);
    assert!(first.has_more);
    let second = page(3, 3).await;
    assert_eq!(seqs(&second.events), vec![4, 5, 6]);
    assert!(second.has_more);
    let last = page(6, 3).await;
    assert_eq!(seqs(&last.events), vec![7]);
    assert_eq!(last.events[0].body, "m7");
    assert!(!last.has_more);
    let exact = page(4, 3).await;
    assert_eq!(seqs(&exact.events), vec![5, 6, 7]);
    assert!(!exact.has_more, "no more after exactly filling the page");

    let all: EventsPage = server
        .request(Method::GET, &events_path(&committee()), BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(all.events.len(), 7);

    let bad_query = server
        .request(
            Method::GET,
            &format!("{}?after=minus-one", events_path(&committee())),
            BOB,
        )
        .send()
        .await
        .unwrap();
    api_error(bad_query, StatusCode::BAD_REQUEST, "bad_request").await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn limit_is_clamped_to_max_page() {
    let server = TestServer::start().await;
    for n in 0..=MAX_PAGE {
        server
            .running
            .hub
            .append(note(&format!("bulk {n}")))
            .await
            .unwrap();
    }
    let path = format!("{}?limit=100000", events_path(&committee()));
    let page: EventsPage = server
        .request(Method::GET, &path, ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.events.len(), MAX_PAGE);
    assert!(page.has_more);
    let default: EventsPage = server
        .request(Method::GET, &events_path(&committee()), ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(default.events.len(), senate_core::api::DEFAULT_PAGE);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn reposting_an_event_id_returns_the_stored_event() {
    let server = TestServer::start().await;
    let event = note("once");
    let first: AppendResponse = server.post_event(ALICE, &event).await.json().await.unwrap();
    assert!(!first.duplicate);
    let retry: AppendResponse = server.post_event(ALICE, &event).await.json().await.unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.event, first.event);
    let next = server.append("next").await;
    assert_eq!(next.seq, Seq(2));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn stream_replays_then_goes_live_and_resumes_from_last_event_id() {
    let server = TestServer::start().await;
    for n in 1..=3 {
        server.append(&format!("before {n}")).await;
    }
    let mut stream = server.stream(BOB, Some(1), None).await;
    assert_eq!(
        stream.seqs(2).await,
        vec![2, 3],
        "replay starts after ?after"
    );
    server.append("live 4").await;
    server.append("live 5").await;
    let live = stream.event().await.unwrap();
    assert_eq!((live.seq, live.body.as_str()), (Seq(4), "live 4"));
    assert_eq!(stream.seqs(1).await, vec![5]);
    drop(stream);

    server.append("missed 6").await;
    server.append("missed 7").await;
    // Last-Event-ID wins over ?after.
    let mut resumed = server.stream(BOB, Some(0), Some(5)).await;
    assert_eq!(resumed.seqs(2).await, vec![6, 7]);
    server.append("live 8").await;
    assert_eq!(resumed.seqs(1).await, vec![8]);

    let bad = server
        .request(Method::GET, &stream_path(&committee()), BOB)
        .header("Last-Event-ID", "seven")
        .send()
        .await
        .unwrap();
    api_error(bad, StatusCode::BAD_REQUEST, "bad_request").await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn stream_has_no_gap_or_duplicate_while_appends_race_the_replay() {
    let server = TestServer::start().await;
    for n in 0..50 {
        server.append(&format!("before {n}")).await;
    }
    let hub = server.running.hub.clone();
    let writer = tokio::spawn(async move {
        for n in 0..250 {
            hub.append(note(&format!("racing {n}"))).await.unwrap();
            if n % 25 == 0 {
                tokio::task::yield_now().await;
            }
        }
    });
    let mut stream = server.stream(ALICE, None, None).await;
    let got = stream.seqs(300).await;
    writer.await.unwrap();
    assert_eq!(got, (1..=300).collect::<Vec<_>>());
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn stream_sends_pings() {
    let server =
        TestServer::start_with(std::sync::Arc::new(senate_log::MemoryStore::new()), Some(1)).await;
    let mut stream = server.stream(ALICE, None, None).await;
    // The office is said first, then nothing but pings on an empty log.
    assert!(matches!(stream.frame().await.unwrap(), Frame::Office(_)));
    match stream.frame().await.unwrap() {
        Frame::Ping(data) => {
            DateTime::parse_from_rfc3339(&data).expect("ping data is RFC 3339");
        }
        other => panic!("unexpected frame {other:?}"),
    }
    server.append("after a ping").await;
    assert_eq!(stream.event().await.unwrap().body, "after a ping");
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_open_stream_marks_the_agent_connected() {
    let server = TestServer::start().await;
    let hub = server.running.hub.clone();
    let f = committee();
    let alice = path("copilot");
    assert!(!hub.is_agent_connected(&f, &alice).await);

    let first = server.stream_as(ALICE, Some("copilot"), None, None).await;
    eventually("alice connected", || hub.is_agent_connected(&f, &alice)).await;
    assert!(!hub.is_agent_connected(&f, &path("claude")).await);
    let other = senate_core::CommitteeId::new("search").unwrap();
    assert!(!hub.is_agent_connected(&other, &alice).await);
    // Presence is per agent, not per person: a sibling is not connected
    // because alice's other agent is, and a stream that only reads counts
    // for nobody.
    assert!(!hub.is_agent_connected(&f, &path("codex")).await);
    assert!(!hub.is_agent_connected(&f, &path("alice")).await);
    let reader = server.stream_as(ALICE, Some("alice"), None, None).await;
    let anonymous = server.stream(ALICE, None, None).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!hub.is_agent_connected(&f, &path("alice")).await);
    assert!(!hub.is_agent_connected(&f, &path("codex")).await);
    drop((reader, anonymous));

    // A second connection keeps her connected after the first closes.
    let second = server.stream_as(ALICE, Some("copilot"), None, None).await;
    drop(first);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(hub.is_agent_connected(&f, &alice).await);
    drop(second);
    eventually("alice disconnected", || async {
        !hub.is_agent_connected(&f, &alice).await
    })
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn cursors_are_scoped_to_the_senator() {
    let server = TestServer::start().await;
    let f = committee();
    let get = |token: &'static str, senator: Option<&'static str>, name: &'static str| {
        let mut request = server.request(Method::GET, &cursor_path(&f, name), token);
        if let Some(senator) = senator {
            request = request.query(&[("senator", senator)]);
        }
        async move {
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.json::<Cursor>().await.unwrap().seq
        }
    };
    let put = |token: &'static str, senator: Option<&'static str>, seq: u64| {
        let mut request = server
            .request(Method::PUT, &cursor_path(&f, "delivered"), token)
            .json(&Cursor { seq: Seq(seq) });
        if let Some(senator) = senator {
            request = request.query(&[("senator", senator)]);
        }
        async move { request.send().await.unwrap() }
    };
    assert_eq!(
        get(ALICE, None, "delivered").await,
        Seq::ZERO,
        "unset reads as 0"
    );
    let put_response = put(ALICE, None, 5).await;
    assert_eq!(put_response.json::<Cursor>().await.unwrap().seq, Seq(5));
    assert_eq!(get(ALICE, None, "delivered").await, Seq(5));
    assert_eq!(get(ALICE, None, "read").await, Seq::ZERO);
    assert_eq!(
        get(BOB, None, "delivered").await,
        Seq::ZERO,
        "bob has his own"
    );

    // Two agents of one person: separate cursors, and the person has a third.
    put(ALICE, Some("copilot"), 7).await;
    put(ALICE, Some("codex"), 2).await;
    assert_eq!(get(ALICE, Some("copilot"), "delivered").await, Seq(7));
    assert_eq!(get(ALICE, Some("codex"), "delivered").await, Seq(2));
    assert_eq!(get(ALICE, Some("alice"), "delivered").await, Seq(5));
    assert_eq!(get(ALICE, None, "delivered").await, Seq(5));

    // Nobody else's: not another person, not an agent another person runs.
    assert_eq!(server.act_as(BOB, "claude").await, StatusCode::OK);
    for (token, senator) in [(ALICE, "bob"), (ALICE, "claude"), (BOB, "copilot")] {
        let response = server
            .request(Method::GET, &cursor_path(&f, "delivered"), token)
            .query(&[("senator", senator)])
            .send()
            .await
            .unwrap();
        api_error(response, StatusCode::FORBIDDEN, "forbidden").await;
        let response = put(token, Some(senator), 99).await;
        api_error(response, StatusCode::FORBIDDEN, "forbidden").await;
    }
    assert_eq!(get(ALICE, Some("copilot"), "delivered").await, Seq(7));
    assert_eq!(
        server.running.hub.cursor(&f, "delivered").await.unwrap(),
        Seq::ZERO,
        "hub cursors are a separate namespace"
    );

    let bad_name = server
        .request(Method::GET, &cursor_path(&f, "bad.name"), ALICE)
        .send()
        .await
        .unwrap();
    api_error(bad_name, StatusCode::BAD_REQUEST, "bad_request").await;
    let bad_body = server
        .request(Method::PUT, &cursor_path(&f, "delivered"), ALICE)
        .json(&json!({"seq": -1}))
        .send()
        .await
        .unwrap();
    api_error(bad_body, StatusCode::BAD_REQUEST, "bad_request").await;
    let bad_senator = server
        .request(Method::GET, &cursor_path(&f, "delivered"), ALICE)
        .query(&[("senator", "Not A Path")])
        .send()
        .await
        .unwrap();
    api_error(bad_senator, StatusCode::BAD_REQUEST, "bad_request").await;
    assert_eq!(get(ALICE, None, "delivered").await, Seq(5));
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn heartbeat_is_recorded_per_agent() {
    let server = TestServer::start().await;
    let f = committee();
    let beat = Heartbeat {
        paused: false,
        agent: path("claude"),
        sessions: 2,
        cursor: Seq(3),
        left: false,
    };
    let response = server
        .request(Method::POST, &heartbeat_path(&f), BOB)
        .json(&beat)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let last = |agent: &str| {
        let hub = server.running.hub.clone();
        let (f, agent) = (f.clone(), path(agent));
        async move { hub.last_heartbeat(&f, &agent).await.unwrap() }
    };
    assert_eq!(last("claude").await.unwrap().heartbeat, beat);
    assert!(last("aider").await.is_none(), "a sibling has none");
    assert!(last("copilot").await.is_none());

    // A second agent of the same person beats separately.
    let other = Heartbeat {
        paused: false,
        agent: path("aider"),
        sessions: 1,
        cursor: Seq(1),
        left: false,
    };
    server
        .request(Method::POST, &heartbeat_path(&f), BOB)
        .json(&other)
        .send()
        .await
        .unwrap();
    assert_eq!(last("aider").await.unwrap().heartbeat, other);
    assert_eq!(last("claude").await.unwrap().heartbeat, beat);

    // Not a person's, not somebody else's (alice runs copilot).
    assert_eq!(server.act_as(ALICE, "copilot").await, StatusCode::OK);
    for agent in ["bob", "copilot"] {
        let response = server
            .request(Method::POST, &heartbeat_path(&f), BOB)
            .json(&Heartbeat {
                agent: path(agent),
                ..beat.clone()
            })
            .send()
            .await
            .unwrap();
        let status = if agent == "bob" {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::FORBIDDEN
        };
        assert_eq!(response.status(), status, "{agent}");
    }
    let bad = server
        .request(Method::POST, &heartbeat_path(&f), BOB)
        .json(&json!({"sessions": "two"}))
        .send()
        .await
        .unwrap();
    api_error(bad, StatusCode::BAD_REQUEST, "bad_request").await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn hub_subscribers_see_http_appends_in_commit_order() {
    let server = TestServer::start().await;
    let mut events = server.running.hub.subscribe();
    for n in 1..=3 {
        server.append(&format!("m{n}")).await;
    }
    for n in 1..=3 {
        let event = tokio::time::timeout(WAIT, events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.seq, Seq(n));
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn every_response_carries_the_epoch() {
    let server = TestServer::start().await;
    let f = committee();
    let epoch = server.running.hub.epoch().await.unwrap();
    assert!(!epoch.is_empty());

    let response = server
        .request(Method::GET, "/v1/me", ALICE)
        .send()
        .await
        .unwrap();
    assert_eq!(epoch_of(&response), epoch);
    let me: Me = response.json().await.unwrap();
    assert_eq!(me.epoch, epoch, "Me::epoch matches the header");

    server.append("one").await;
    let after = |after: &str| format!("{}?after={after}", events_path(&f));
    let beat = Heartbeat {
        paused: false,
        agent: path("claude"),
        sessions: 1,
        cursor: Seq(1),
        left: false,
    };
    let requests = [
        (server.request(Method::GET, &events_path(&f), BOB), 200),
        (
            server
                .request(Method::POST, &events_path(&f), ALICE)
                .json(&note("two")),
            200,
        ),
        (
            server.request(Method::GET, &cursor_path(&f, "read"), BOB),
            200,
        ),
        (
            server
                .request(Method::POST, &heartbeat_path(&f), BOB)
                .json(&beat),
            204,
        ),
        (server.request(Method::GET, "/v1/me", "not-a-token"), 401),
        (
            server
                .request(Method::POST, &events_path(&f), BOB)
                .json(&note("not bob's")),
            403,
        ),
        (server.request(Method::GET, "/v1/nope", ALICE), 404),
        (server.request(Method::DELETE, &events_path(&f), ALICE), 405),
        (server.request(Method::GET, &after("x"), BOB), 400),
        (server.request(Method::GET, &after("99"), BOB), 409),
        (
            server.request(Method::GET, &format!("{}?q=one", search_path(&f)), BOB),
            200,
        ),
        (
            server.request(Method::GET, &format!("{}?q=+", search_path(&f)), BOB),
            400,
        ),
        (server.request(Method::GET, &brief_path(&f), BOB), 200),
        (server.client.get(server.url("/healthz")), 200),
    ];
    for (request, status) in requests {
        let response = request.send().await.unwrap();
        assert_eq!(response.status().as_u16(), status, "{}", response.url());
        assert_eq!(epoch_of(&response), epoch, "{}", response.url());
    }

    let stream = server
        .request(Method::GET, &stream_path(&f), BOB)
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    assert_eq!(epoch_of(&stream), epoch, "the SSE response too");
    drop(stream);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_cursor_beyond_the_head_is_cursor_ahead() {
    let server = TestServer::start().await;
    let f = committee();
    let page = |after: u64| {
        server
            .request(
                Method::GET,
                &format!("{}?after={after}", events_path(&f)),
                BOB,
            )
            .send()
    };
    let stream = |after: Option<u64>, last_event_id: Option<u64>| {
        let mut path = stream_path(&f);
        if let Some(after) = after {
            path.push_str(&format!("?after={after}"));
        }
        let mut request = server.request(Method::GET, &path, BOB);
        if let Some(id) = last_event_id {
            request = request.header("Last-Event-ID", id.to_string());
        }
        request.send()
    };

    // A committee with no events has head 0: any cursor beyond it is ahead.
    assert_eq!(page(0).await.unwrap().status(), StatusCode::OK);
    let error = api_error(page(1).await.unwrap(), StatusCode::CONFLICT, "cursor_ahead").await;
    assert!(error.message.contains("head 0"), "{error:?}");
    for (after, last_event_id) in [(Some(1), None), (None, Some(1))] {
        api_error(
            stream(after, last_event_id).await.unwrap(),
            StatusCode::CONFLICT,
            "cursor_ahead",
        )
        .await;
    }
    assert!(
        !server
            .running
            .hub
            .is_agent_connected(&f, &path("claude"))
            .await,
        "a refused stream never attached"
    );

    for n in 1..=3 {
        server.append(&format!("m{n}")).await;
    }
    // At the head is fine (nothing new yet); past it is not.
    let at_head: EventsPage = page(3).await.unwrap().json().await.unwrap();
    assert!(at_head.events.is_empty() && !at_head.has_more);
    let last: EventsPage = page(2).await.unwrap().json().await.unwrap();
    assert_eq!(seqs(&last.events), vec![3]);
    api_error(page(4).await.unwrap(), StatusCode::CONFLICT, "cursor_ahead").await;
    let mut live = server.stream(BOB, None, Some(3)).await;
    // Ahead by Last-Event-ID or by ?after; the header wins when both are set.
    for (after, last_event_id) in [(None, Some(4)), (Some(0), Some(4)), (Some(4), None)] {
        api_error(
            stream(after, last_event_id).await.unwrap(),
            StatusCode::CONFLICT,
            "cursor_ahead",
        )
        .await;
    }
    assert_eq!(
        stream(Some(9), Some(2)).await.unwrap().status(),
        StatusCode::OK
    );
    server.append("m4").await;
    assert_eq!(live.seqs(1).await, vec![4]);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_restarted_memory_server_is_a_new_epoch() {
    let before = TestServer::start().await;
    for n in 1..=50 {
        before.append(&format!("m{n}")).await;
    }
    let mut stream = before.stream(BOB, None, None).await;
    assert_eq!(stream.seqs(50).await.last(), Some(&50));
    let me = before
        .request(Method::GET, "/v1/me", BOB)
        .send()
        .await
        .unwrap();
    let old_epoch = epoch_of(&me);
    before.running.shutdown().await.unwrap();
    assert!(stream.event().await.is_none(), "the old stream ended");

    // `memory:` again: the same members, an empty log, a new epoch.
    let after = TestServer::start().await;
    after.append("first after restart").await;
    let reconnect = after
        .request(Method::GET, &stream_path(&committee()), BOB)
        .header("Last-Event-ID", "50")
        .send()
        .await
        .unwrap();
    let new_epoch = epoch_of(&reconnect);
    assert_ne!(new_epoch, old_epoch);
    api_error(reconnect, StatusCode::CONFLICT, "cursor_ahead").await;
    let me: Me = after
        .request(Method::GET, "/v1/me", BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.epoch, new_epoch);

    // Having dropped its cursor, the client starts over.
    let mut fresh = after.stream(BOB, None, None).await;
    assert_eq!(fresh.event().await.unwrap().body, "first after restart");
    after.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stream_without_heartbeats_stops_counting_after_the_grace() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |config| {
        config.presence_grace_secs = 1;
    })
    .await;
    let hub = server.running.hub.clone();
    let f = committee();
    let alice = path("copilot");
    let beat = || async {
        let response = server
            .request(Method::POST, &heartbeat_path(&f), ALICE)
            .json(&Heartbeat {
                paused: false,
                agent: alice.clone(),
                sessions: 1,
                cursor: Seq::ZERO,
                left: false,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    };
    let past_grace = Duration::from_millis(1300);

    // Like a laptop that went to sleep: its stream stays open on the server,
    // but no heartbeat arrives.
    let stream = server.stream_as(ALICE, Some("copilot"), None, None).await;
    eventually("a new stream counts", || hub.is_agent_connected(&f, &alice)).await;
    tokio::time::sleep(past_grace).await;
    assert!(!hub.is_agent_connected(&f, &alice).await);

    // It wakes up and heartbeats on the same stream.
    beat().await;
    assert!(hub.is_agent_connected(&f, &alice).await);
    tokio::time::sleep(past_grace).await;
    assert!(!hub.is_agent_connected(&f, &alice).await);

    // Heartbeats without a stream are not presence either.
    beat().await;
    assert!(hub.is_agent_connected(&f, &alice).await);
    drop(stream);
    eventually("alice disconnected", || async {
        beat().await;
        !hub.is_agent_connected(&f, &alice).await
    })
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn hub_queries_and_mirrors_come_from_the_store() {
    let server = TestServer::start().await;
    let hub = server.running.hub.clone();
    let f = committee();
    let root = server.append("Refund question").await;
    server.append("unrelated").await;
    let reply = NewEvent::new(f.clone(), Kind::Answer, agent_addr("copilot"), "30 days")
        .in_thread(root.seq);
    assert_eq!(
        server.post_event(ALICE, &reply).await.status(),
        StatusCode::OK
    );
    let binding = NewEvent::new(f.clone(), Kind::Binding, Address::Speaker, "bound");
    hub.append(binding).await.unwrap();

    assert_eq!(seqs(&hub.thread(&f, root.seq).await.unwrap()), vec![1, 3]);
    assert_eq!(seqs(&hub.bindings().await.unwrap()), vec![4]);
    assert_eq!(seqs(&hub.search(&f, "REFUND", 10).await.unwrap()), vec![1]);
    assert!(hub.search(&f, "", 2).await.unwrap().is_empty(), "no terms");

    let copy = |target: &str, message_ref: &str| MirrorRef {
        committee: f.clone(),
        seq: root.seq,
        platform: "chat".into(),
        target: target.into(),
        message_ref: message_ref.into(),
        thread_ref: None,
    };
    hub.put_mirror(copy("pm-group", "m-1")).await.unwrap();
    hub.put_mirror(copy("agents", "m-2")).await.unwrap();
    assert_eq!(
        hub.mirrors_of_seq(&f, root.seq, "chat").await.unwrap(),
        vec![copy("agents", "m-2"), copy("pm-group", "m-1")]
    );
    assert_eq!(
        hub.mirror_of_message("chat", "m-1").await.unwrap(),
        Some(copy("pm-group", "m-1"))
    );
    server.running.shutdown().await.unwrap();
}

async fn search_as(server: &TestServer, query: &str) -> reqwest::Response {
    server
        .request(
            Method::GET,
            &format!("{}?{query}", search_path(&committee())),
            BOB,
        )
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn search_matches_every_term_newest_first() {
    let server = TestServer::start().await;
    for body in [
        "Refund limit is 30 days",
        "unrelated",
        "the refund LIMIT moved",
        "退款上限 30 天",
    ] {
        server.append(body).await;
    }
    let found = |response: reqwest::Response| async move {
        assert_eq!(response.status(), StatusCode::OK);
        seqs(&response.json::<SearchResults>().await.unwrap().events)
    };
    assert_eq!(found(search_as(&server, "q=refund").await).await, [3, 1]);
    assert_eq!(
        found(search_as(&server, "q=limit%20REFUND&limit=1").await).await,
        [3]
    );
    assert_eq!(
        found(search_as(&server, "q=%E4%B8%8A%E9%99%90").await).await,
        [4]
    );
    assert_eq!(
        found(search_as(&server, "q=refund&limit=0").await).await,
        [3],
        "a zero limit is raised to one"
    );
    for bad in ["q=", "q=%20%09", "", "q=x&limit=many"] {
        api_error(
            search_as(&server, bad).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        )
        .await;
    }
    let bad_committee = server
        .request(
            Method::GET,
            "/v1/committees/Not%20A%20Committee/search?q=x",
            BOB,
        )
        .send()
        .await
        .unwrap();
    api_error(bad_committee, StatusCode::BAD_REQUEST, "bad_request").await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn search_limit_defaults_and_is_clamped() {
    let server = TestServer::start().await;
    for n in 0..MAX_SEARCH + 5 {
        server
            .running
            .hub
            .append(note(&format!("match {n}")))
            .await
            .unwrap();
    }
    let results = |query: &'static str| {
        let server = &server;
        async move {
            search_as(server, query)
                .await
                .json::<SearchResults>()
                .await
                .unwrap()
                .events
        }
    };
    let clamped = results("q=match&limit=100000").await;
    assert_eq!(clamped.len(), MAX_SEARCH);
    assert_eq!(clamped[0].seq, Seq((MAX_SEARCH + 5) as u64), "newest first");
    assert_eq!(results("q=match").await.len(), DEFAULT_SEARCH);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn brief_folds_the_log_as_it_grows() {
    let server = TestServer::start().await;
    let f = committee();
    let get_brief = || async {
        let response = server
            .request(Method::GET, &brief_path(&f), BOB)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.json::<Brief>().await.unwrap()
    };
    let whole = || async {
        let log = server.running.hub.read(&f, ZERO, 1_000).await.unwrap();
        Brief::from_events(f.clone(), &log)
    };

    let empty = get_brief().await;
    assert_eq!(empty.head, ZERO);
    assert!(empty.decisions.is_empty());

    let decide = |body: &str| NewEvent::new(f.clone(), Kind::Decision, person_addr("alice"), body);
    let first = server.post_event(ALICE, &decide("退款上限 14 天")).await;
    let first: AppendResponse = first.json().await.unwrap();
    let question = NewEvent::new(
        f.clone(),
        Kind::Question,
        agent_addr("copilot"),
        "用哪个 header？",
    )
    .to(agent_addr("claude"));
    server.post_event(ALICE, &question).await;
    let brief = get_brief().await;
    assert_eq!(brief, whole().await);
    assert_eq!(brief.head, Seq(2));
    assert_eq!(brief.decisions[0].body, "退款上限 14 天");
    assert_eq!(brief.open_questions.len(), 1);

    // Only what is new is folded, and the result is the same as a full scan.
    let second = decide("退款上限 30 天").with_meta(meta::SUPERSEDES, json!(first.event.seq.0));
    server.post_event(ALICE, &second).await;
    server
        .running
        .hub
        .append(
            NewEvent::new(f.clone(), Kind::Note, Address::Speaker, "1. 上限 30 天")
                .with_meta(meta::BRIEF_TOP, json!(true)),
        )
        .await
        .unwrap();
    let brief = get_brief().await;
    assert_eq!(brief, whole().await);
    assert_eq!(brief.head, Seq(4));
    assert_eq!(brief.decisions.len(), 1);
    assert_eq!(brief.decisions[0].body, "退款上限 30 天");
    assert_eq!(brief.top.map(|t| t.body), Some("1. 上限 30 天".to_string()));
    assert_eq!(server.running.hub.brief(&f).await.unwrap(), whole().await);
    server.running.shutdown().await.unwrap();
}

fn status(f: &senate_core::CommitteeId, from: Address, root: Seq, s: &StatusPayload) -> NewEvent {
    NewEvent::new(f.clone(), Kind::Status, from, "")
        .in_thread(root)
        .with_meta(meta::STATUS, s.value())
}

#[tokio::test]
async fn triage_and_output_level_are_the_speakers() {
    let server = TestServer::start().await;
    let f = committee();
    for key in [meta::TRIAGE, meta::OUTPUT_LEVEL] {
        let event = NewEvent::new(f.clone(), Kind::Note, agent_addr("copilot"), "")
            .with_meta(key, json!({"verdict": "agent"}));
        api_error(
            server.post_event(ALICE, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn status_belongs_to_a_task_thread_and_its_agent() {
    let server = TestServer::start().await;
    let f = committee();
    // copilot is alice's agent, claude bob's.
    let alice = agent_addr("copilot");
    let bob = agent_addr("claude");
    let root: AppendResponse = server
        .post_event(
            BOB,
            &NewEvent::new(f.clone(), Kind::Question, bob.clone(), "用哪个 header？")
                .to(alice.clone()),
        )
        .await
        .json()
        .await
        .unwrap();
    let root = root.event.seq;
    let note: AppendResponse = server
        .post_event(
            BOB,
            &NewEvent::new(f.clone(), Kind::Note, bob.clone(), "重试改了"),
        )
        .await
        .json()
        .await
        .unwrap();

    let processing = StatusPayload::new(StatusState::Processing);
    let mut takeover = StatusPayload::new(StatusState::Suspended);
    takeover.takeover = Some(name("alice"));
    let mut bad = StatusPayload::new(StatusState::Suspended);
    bad.title = Some("x".repeat(10_000));

    // Not a thread, not a task root, a bad payload: 400.
    for (event, token) in [
        (
            NewEvent::new(f.clone(), Kind::Status, alice.clone(), "")
                .with_meta(meta::STATUS, processing.value()),
            ALICE,
        ),
        (
            status(&f, alice.clone(), note.event.seq, &processing),
            ALICE,
        ),
        (status(&f, alice.clone(), root, &bad), ALICE),
        (
            NewEvent::new(f.clone(), Kind::Status, alice.clone(), "")
                .in_thread(root)
                .with_meta(meta::STATUS, json!({"state": "nope"})),
            ALICE,
        ),
    ] {
        api_error(
            server.post_event(token, &event).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        )
        .await;
    }
    // The asker's agent is not the thread's agent; agents never take over: 403.
    api_error(
        server
            .post_event(BOB, &status(&f, bob.clone(), root, &processing))
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    // A sibling agent of the thread's agent does not qualify either: the
    // thread is addressed to copilot, and codex is not it.
    let sibling = person_addr("codex");
    api_error(
        server
            .post_event(ALICE, &status(&f, sibling, root, &processing))
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    api_error(
        server
            .post_event(ALICE, &status(&f, alice.clone(), root, &takeover))
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;

    // The thread's agent reports; a person takes over.
    let mut titled = processing.clone();
    titled.title = Some("查幂等 header".into());
    let ok = server
        .post_event(ALICE, &status(&f, alice.clone(), root, &titled))
        .await;
    assert_eq!(ok.status(), StatusCode::OK);
    let ok = server
        .post_event(ALICE, &status(&f, person_addr("alice"), root, &takeover))
        .await;
    assert_eq!(ok.status(), StatusCode::OK);

    let page: ThreadsPage = server
        .request(
            Method::GET,
            &format!("{}?state=suspended", threads_path(&f)),
            BOB,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(page.threads.len(), 1);
    let thread = &page.threads[0];
    assert_eq!(thread.root, root);
    assert_eq!(thread.title, "查幂等 header");
    assert_eq!(thread.takeover, Some(name("alice")));

    let none: ThreadsPage = server
        .request(
            Method::GET,
            &format!("{}?state=archived", threads_path(&f)),
            BOB,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(none.threads.is_empty());
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_path_inside_a_name_is_refused_wherever_a_senator_is_named() {
    // A senator's name is one flat name: `alice/claude` is no agent of
    // alice's, and not a name at all. Nothing that carries one is served,
    // and nothing is seated.
    let server = TestServer::start().await;
    let f = committee();
    let nested = "alice/claude";

    let beat = server
        .request(Method::POST, &heartbeat_path(&f), ALICE)
        .json(&json!({"agent": nested, "sessions": 1, "cursor": 0}))
        .send()
        .await
        .unwrap();
    api_error(beat, StatusCode::BAD_REQUEST, "bad_request").await;
    let stream = server.try_stream(ALICE, Some(nested), None, None).await;
    api_error(stream, StatusCode::BAD_REQUEST, "bad_request").await;
    let cursor = server
        .request(Method::GET, &cursor_path(&f, "read"), ALICE)
        .query(&[("senator", nested)])
        .send()
        .await
        .unwrap();
    api_error(cursor, StatusCode::BAD_REQUEST, "bad_request").await;
    for body in [
        json!({"committee": "refund-v2", "kind": "note", "from": "senator::alice/claude",
               "body": "as a nested agent"}),
        json!({"committee": "refund-v2", "kind": "question", "from": "senator::alice",
               "to": ["senator::alice/claude"], "body": "to a nested agent"}),
        json!({"committee": "refund-v2", "kind": "question", "from": "senator::alice",
               "to": ["senator::alice/*"], "body": "to a person's agents"}),
    ] {
        api_error(
            server.post_json(ALICE, &body).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        )
        .await;
    }

    let roster = server.running.hub.roster().await.unwrap();
    assert!(roster.agents().is_empty(), "{:?}", roster.agents());
    assert!(server.running.hub.committees().await.unwrap().is_empty());
    server.running.shutdown().await.unwrap();
}

/// `GET /v1/resolve?name=<locator>[&committee=<writer's committee>]`.
async fn resolve(server: &TestServer, name: &str, committee: Option<&str>) -> Resolved {
    let mut query = vec![("name", name)];
    if let Some(committee) = committee {
        query.push(("committee", committee));
    }
    let response = server
        .request(Method::GET, RESOLVE_PATH, BOB)
        .query(&query)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{name}");
    response.json().await.unwrap()
}

fn here(locator: &str) -> Resolved {
    let locator: Locator = locator.parse().unwrap();
    Resolved::Here {
        committee: locator.committee().unwrap().clone(),
        address: locator.address().clone(),
        locator,
    }
}

#[tokio::test]
async fn resolve_finds_a_name_here_and_completes_it_with_this_senates_path() {
    let server = TestServer::start().await;
    server.append("refund-v2 exists").await;
    assert_eq!(server.act_as(ALICE, "codex").await, StatusCode::OK);

    for (name, committee, found) in [
        // A committee here: the name completed with this Senate's path.
        (
            "refund-v2::senator::codex",
            None,
            "/senate::refund-v2::senator::codex",
        ),
        ("refund-v2::speaker", None, "/senate::refund-v2::speaker"),
        (
            "refund-v2::senator::*",
            None,
            "/senate::refund-v2::senator::*",
        ),
        // No committee: the writer's own.
        (
            "senator::alice",
            Some("refund-v2"),
            "/senate::refund-v2::senator::alice",
        ),
        // This Senate named by its path, relative or from the root.
        (
            "senate::refund-v2::senator::codex",
            None,
            "/senate::refund-v2::senator::codex",
        ),
        (
            "/senate::refund-v2::senator::codex",
            None,
            "/senate::refund-v2::senator::codex",
        ),
    ] {
        assert_eq!(
            resolve(&server, name, committee).await,
            here(found),
            "{name}"
        );
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn resolve_says_elsewhere_for_what_another_senate_may_have() {
    let server = TestServer::start().await;
    server.append("refund-v2 exists").await;
    for (name, committee) in [
        // A committee that is not here: ask the Senate above.
        ("ledger::senator::codex", None),
        ("ledger::speaker", Some("refund-v2")),
        // Another Senate's path, even with a committee of the same name.
        ("acme::refund-v2::senator::alice", None),
        ("acme/north-desk::refund-v2::senator::alice", None),
    ] {
        assert_eq!(
            resolve(&server, name, committee).await,
            Resolved::Elsewhere,
            "{name}"
        );
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn resolve_says_unknown_for_what_this_senate_should_have_and_does_not() {
    let server = TestServer::start().await;
    server.append("refund-v2 exists").await;
    for (name, committee) in [
        // A senator nobody knows, on a committee here.
        ("refund-v2::senator::nobody", None),
        ("senator::nobody", Some("refund-v2")),
        // A committee that is not here, in a path naming this Senate.
        ("senate::ledger::senator::alice", None),
        // No committee in the name and none where it is written.
        ("senator::alice", None),
    ] {
        match resolve(&server, name, committee).await {
            Resolved::Unknown { reason } => assert!(!reason.is_empty(), "{name}"),
            other => panic!("{name}: expected unknown, got {other:?}"),
        }
    }
    // A prefix of a senator's name is not that senator: it is unknown, and
    // the reason names who it was probably meant to be.
    for (name, like) in [
        ("senator::ali", "[senator::alice]"),
        ("senator::car", "[senator::carol]"),
    ] {
        match resolve(&server, name, Some("refund-v2")).await {
            Resolved::Unknown { reason } => assert!(
                reason.contains(&format!("did you mean {like}")),
                "{name}: {reason}"
            ),
            other => panic!("{name}: expected unknown, got {other:?}"),
        }
    }
    // Not a qualified name at all.
    for bad in ["alice", "refund-v2::senator::alice/claude", ""] {
        let response = server
            .request(Method::GET, RESOLVE_PATH, BOB)
            .query(&[("name", bad)])
            .send()
            .await
            .unwrap();
        api_error(response, StatusCode::BAD_REQUEST, "bad_request").await;
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_configured_senate_name_is_in_me_and_in_what_resolve_answers() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |config| {
        config.name = Some(name("north-desk"));
        config.parent = Some("acme".parse().unwrap());
    })
    .await;
    server.append("refund-v2 exists").await;
    let me: Me = server
        .request(Method::GET, "/v1/me", BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.senate, name("north-desk"));
    assert_eq!(me.parent, Some("acme".parse().unwrap()));

    let found = here("/acme/north-desk::refund-v2::senator::alice");
    for name in [
        "refund-v2::senator::alice",
        "north-desk::refund-v2::senator::alice",
        "acme/north-desk::refund-v2::senator::alice",
        "/acme/north-desk::refund-v2::senator::alice",
    ] {
        assert_eq!(resolve(&server, name, None).await, found, "{name}");
    }
    // The default name and the Senate above are other Senates now.
    for name in [
        "senate::refund-v2::senator::alice",
        "acme::refund-v2::senator::alice",
        // From the root, a path must be all of this Senate's.
        "/north-desk::refund-v2::senator::alice",
    ] {
        assert_eq!(
            resolve(&server, name, None).await,
            Resolved::Elsewhere,
            "{name}"
        );
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn alias_rollback_is_conditional_and_conflicts_do_not_register_a_new_owner() {
    let server = TestServer::start().await;
    let first = "/v1/aliases/claude-first";
    let second = "/v1/aliases/claude-second";
    let response = server
        .request(Method::PUT, first, ALICE)
        .json(&json!({"alias":"reviewer","committee":"refund-v2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let conflict = server
        .request(Method::PUT, second, ALICE)
        .json(&json!({"alias":"reviewer","committee":"refund-v2"}))
        .send()
        .await
        .unwrap();
    assert!(!conflict.status().is_success());
    let me: Me = server
        .request(Method::GET, "/v1/me", ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !me.agents
            .iter()
            .any(|agent| agent.as_str() == "claude-second")
    );

    let alias = format!("{first}/reviewer");
    let forbidden = server
        .request(Method::DELETE, &alias, BOB)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    let wrong = server
        .request(Method::DELETE, &format!("{second}/reviewer"), ALICE)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::NO_CONTENT);
    let aliases: senate_core::api::SenatorAliases = server
        .request(Method::GET, "/v1/aliases", ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        aliases
            .get(&"reviewer".parse::<senate_core::SenatorName>().unwrap())
            .unwrap()
            .as_str(),
        "claude-first"
    );

    let removed = server
        .request(Method::DELETE, &alias, ALICE)
        .send()
        .await
        .unwrap();
    assert_eq!(removed.status(), StatusCode::NO_CONTENT);
    let reused = server
        .request(Method::PUT, second, ALICE)
        .json(&json!({"alias":"reviewer","committee":"refund-v2"}))
        .send()
        .await
        .unwrap();
    assert_eq!(reused.status(), StatusCode::OK);
    server.running.shutdown().await.unwrap();
}
