//! What changed from a one-agent-per-person world: a token names a person who
//! acts as agents, `senator::*` is resolved among the agents on the
//! committee before it is stored,
//! guests cannot change state, and the seams a private layer replaces.

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::*;
use reqwest::{Method, StatusCode};
use senate_core::PresencePayload;
use senate_core::api::{AppendResponse, Me, PresencePage, presence_path};
use senate_core::{
    Address, CommitteeId, Hub, HubError, Kind, Locale, Name, NewEvent, PresenceState, Principal,
    ResolveError, Roster, Seq, SpeakerSeat, meta,
};
use senate_log::MemoryStore;
use senate_server::{Authenticate, Server, ServerConfig};
use serde_json::json;

/// Seat `agents` of alice by having each of them heartbeat: each is then
/// on the committee, where `senator::*` looks for an agent.
async fn seat(server: &TestServer, agents: &[&str]) {
    for agent in agents {
        let response = server
            .request(
                Method::POST,
                &senate_core::api::heartbeat_path(&committee()),
                ALICE,
            )
            .json(&senate_core::api::Heartbeat {
                agent: path(agent),
                sessions: 1,
                cursor: Seq::ZERO,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
}

/// A question from claude (bob's agent, seated by asking) to `to`.
fn question_to(to: &str) -> NewEvent {
    NewEvent::new(
        committee(),
        Kind::Question,
        person_addr("claude"),
        "who owns the gateway?",
    )
    .to(to.parse().unwrap())
}

#[tokio::test]
async fn a_wildcard_to_one_agent_goes_to_that_agent() {
    let server = TestServer::start().await;
    seat(&server, &["copilot"]).await;
    let response = server.post_event(BOB, &question_to("senator::*")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let appended: AppendResponse = response.json().await.unwrap();
    assert_eq!(appended.event.to, [person_addr("copilot")]);
    // What is stored is what came back: the wildcard never reaches the log.
    // (Beside it is copilot's seat, the `presence` its heartbeat wrote as
    // the first agent on the committee.)
    let stored: Vec<_> = server
        .running
        .hub
        .read(&committee(), Seq::ZERO, 10)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind != Kind::Presence)
        .collect();
    assert_eq!(stored, [appended.event]);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_wildcard_to_several_agents_is_a_409_with_the_candidates() {
    let server = TestServer::start().await;
    seat(&server, &["copilot", "codex"]).await;
    let response = server.post_event(BOB, &question_to("senator::*")).await;
    let error = api_error(response, StatusCode::CONFLICT, "ambiguous_recipient").await;
    assert_eq!(error.candidates, [path("copilot"), path("codex")]);
    assert!(error.message.contains("senator::copilot"), "{error:?}");
    assert!(
        server
            .running
            .hub
            .read(&committee(), Seq::ZERO, 10)
            .await
            .unwrap()
            .iter()
            .all(|e| e.kind == Kind::Presence),
        "nothing was appended (only the first agent's seat is in the log)"
    );

    // The sender names one and tries again.
    let again = server.post_event(BOB, &question_to("senator::codex")).await;
    assert_eq!(again.status(), StatusCode::OK);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_wildcard_reaches_only_the_agents_on_the_committee() {
    let server = TestServer::start().await;
    // codex is on another committee only; claude (the asker) and aider are
    // seated here but have no presence, stream or heartbeat on it.
    let search = CommitteeId::new("search").unwrap();
    let elsewhere = server
        .request(
            Method::POST,
            &senate_core::api::heartbeat_path(&search),
            ALICE,
        )
        .json(&senate_core::api::Heartbeat {
            agent: path("codex"),
            sessions: 1,
            cursor: Seq::ZERO,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(elsewhere.status(), StatusCode::NO_CONTENT);
    // aider's cursor is on `search` too: any contact on this committee is a
    // seat on it.
    let aider = server
        .request(
            Method::GET,
            &senate_core::api::cursor_path(&search, "read"),
            CAROL,
        )
        .query(&[("senator", "aider")])
        .send()
        .await
        .unwrap();
    assert_eq!(aider.status(), StatusCode::OK);
    seat(&server, &["copilot"]).await;
    assert_eq!(
        server
            .running
            .hub
            .committee_agents(&committee())
            .await
            .unwrap(),
        [path("copilot")]
    );
    let appended: AppendResponse = server
        .post_event(BOB, &question_to("senator::*"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(appended.event.to, [person_addr("copilot")]);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_wildcard_with_no_agent_on_the_committee_goes_to_the_only_person() {
    // A Senate of one person: what no agent can take goes to her.
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |config| {
        config.roster.principals = vec![Principal::new(name("alice"))];
        config.tokens.retain(|t| t.person == name("alice"));
    })
    .await;
    let from_alice = |to: &str| {
        NewEvent::new(committee(), Kind::Question, person_addr("alice"), "?")
            .to(to.parse().unwrap())
    };
    let appended: AppendResponse = server
        .post_event(ALICE, &from_alice("senator::*"))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(appended.event.to, [person_addr("alice")]);
    // A repeat after resolution is dropped.
    let both = from_alice("senator::*").to(person_addr("alice"));
    let appended: AppendResponse = server.post_event(ALICE, &both).await.json().await.unwrap();
    assert_eq!(appended.event.to, [person_addr("alice")]);
    server.running.shutdown().await.unwrap();

    // Several people and no agent on the committee: nobody to take it, the
    // sender's mistake.
    let server = TestServer::start().await;
    let error = api_error(
        server.post_event(BOB, &question_to("senator::*")).await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    assert!(error.candidates.is_empty(), "{error:?}");
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn in_process_callers_resolve_the_same_way() {
    let server = TestServer::start().await;
    let hub = server.running.hub.clone();
    let wildcard = || question_to("senator::*");

    // None, with several people: refused. One: the agent.
    assert!(matches!(
        hub.resolve_and_append(wildcard()).await,
        Err(HubError::Unresolved(ResolveError::NoAgent))
    ));
    seat(&server, &["copilot"]).await;
    let appended = hub.resolve_and_append(wildcard()).await.unwrap();
    assert_eq!(appended.event.to, [person_addr("copilot")]);

    // Several: nothing is appended, and the candidates come back.
    seat(&server, &["codex"]).await;
    let head = hub.read(&committee(), Seq::ZERO, 10).await.unwrap().len();
    match hub.resolve_and_append(wildcard()).await {
        Err(HubError::Unresolved(ResolveError::Ambiguous { candidates })) => {
            assert_eq!(candidates, [path("copilot"), path("codex")]);
        }
        other => panic!("expected the candidates, got {other:?}"),
    }
    assert_eq!(
        hub.read(&committee(), Seq::ZERO, 10).await.unwrap().len(),
        head
    );

    // `append` itself stores only what is resolved.
    assert!(matches!(
        hub.append(wildcard()).await,
        Err(HubError::Rejected(_))
    ));
    assert_eq!(
        hub.read(&committee(), Seq::ZERO, 10).await.unwrap().len(),
        head
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_guest_may_ask_but_not_change_state() {
    let server = TestServer::start().await;
    let hub = server.running.hub.clone();
    let guest: Address = "guest::chat/u_8f3a".parse().unwrap();
    let f = committee();

    let question = NewEvent::new(
        f.clone(),
        Kind::Question,
        guest.clone(),
        "what is the limit?",
    )
    .to(Address::Speaker);
    hub.append(question).await.unwrap();
    hub.append(NewEvent::new(
        f.clone(),
        Kind::Chat,
        guest.clone(),
        "thanks",
    ))
    .await
    .unwrap();

    let refused = [
        NewEvent::new(f.clone(), Kind::Decision, guest.clone(), "limit is 99 days"),
        NewEvent::new(f.clone(), Kind::Binding, guest.clone(), "bind me"),
        NewEvent::new(f.clone(), Kind::Request, guest.clone(), "open a PR"),
        NewEvent::new(f.clone(), Kind::Presence, guest.clone(), ""),
        NewEvent::new(f.clone(), Kind::Note, guest.clone(), "propose")
            .with_meta(meta::PROPOSAL, json!("decision")),
        NewEvent::new(f.clone(), Kind::Question, guest.clone(), "q")
            .with_meta(meta::WORK, json!({"state": "succeeded"})),
        NewEvent::new(f.clone(), Kind::Note, guest.clone(), "n")
            .with_meta(meta::SUPERSEDES, json!(1)),
    ];
    for event in refused {
        let kind = event.kind.clone();
        match hub.append(event).await {
            Err(HubError::Rejected(message)) => {
                assert!(message.contains("guest::chat/u_8f3a"), "{kind}: {message}");
            }
            other => panic!("a guest's {kind} was not refused: {other:?}"),
        }
    }
    assert_eq!(hub.read(&f, Seq::ZERO, 10).await.unwrap().len(), 2);

    // The same kinds from the Speaker or a person stand.
    hub.append(NewEvent::new(
        f.clone(),
        Kind::Binding,
        Address::Speaker,
        "bound",
    ))
    .await
    .unwrap();

    // Over HTTP a guest never holds a token, and cannot be written as.
    let as_guest = NewEvent::new(f.clone(), Kind::Question, guest, "q");
    api_error(
        server.post_event(ALICE, &as_guest).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

/// An authenticator that is not the configured tokens: one magic token names
/// whoever it is built for.
struct Fake {
    token: &'static str,
    person: Name,
}

#[async_trait]
impl Authenticate for Fake {
    async fn authenticate(&self, token: &str) -> Option<Name> {
        (token == self.token).then(|| self.person.clone())
    }
}

#[tokio::test]
async fn the_auth_seam_can_be_swapped_for_a_fake() {
    // The configured tokens are not consulted at all.
    let config = ServerConfig::new(roster(), tokens());
    let running = Server::builder(config, Arc::new(MemoryStore::new()))
        .auth(Arc::new(Fake {
            token: "sso-ticket-42",
            person: name("carol"),
        }))
        .start()
        .await
        .unwrap();
    let server = TestServer {
        running,
        client: reqwest::Client::new(),
    };
    let me: Me = server
        .request(Method::GET, "/v1/me", "sso-ticket-42")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.person, name("carol"));
    for token in [ALICE, BOB, "", "sso-ticket-43"] {
        let response = server
            .request(Method::GET, "/v1/me", token)
            .send()
            .await
            .unwrap();
        api_error(response, StatusCode::UNAUTHORIZED, "unauthorized").await;
    }

    // What the person may do is still the roster's rule, not the fake's: carol
    // is carol, and her agents are hers.
    let own = NewEvent::new(committee(), Kind::Note, person_addr("aider"), "hi");
    let ok = server.post_event("sso-ticket-42", &own).await;
    assert_eq!(ok.status(), StatusCode::OK);
    let theirs = NewEvent::new(committee(), Kind::Note, person_addr("alice"), "hi");
    api_error(
        server.post_event("sso-ticket-42", &theirs).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_person_the_roster_does_not_know_may_act_as_nobody() {
    let config = ServerConfig::new(roster(), tokens());
    let running = Server::builder(config, Arc::new(MemoryStore::new()))
        .auth(Arc::new(Fake {
            token: "t",
            person: name("mallory"),
        }))
        .start()
        .await
        .unwrap();
    let server = TestServer {
        running,
        client: reqwest::Client::new(),
    };
    for from in ["mallory", "cline"] {
        let event = NewEvent::new(committee(), Kind::Note, person_addr(from), "hi");
        api_error(
            server.post_event("t", &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_roster_seam_takes_a_roster() {
    // A roster from somewhere other than configuration: alice and bob, and
    // alice's reviewer seated from the start.
    let given = Roster::new(
        vec![Principal::new(name("alice")), Principal::new(name("bob"))],
        vec![seated("reviewer", "alice")],
    )
    .unwrap();
    let config = ServerConfig::new(roster(), tokens());
    let running = Server::builder(config, Arc::new(MemoryStore::new()))
        .roster(given)
        .start()
        .await
        .unwrap();
    let server = TestServer {
        running,
        client: reqwest::Client::new(),
    };
    let me: Me = server
        .request(Method::GET, "/v1/me", ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.agents, [path("reviewer")]);
    assert_eq!(
        me.people.handles().cloned().collect::<Vec<_>>(),
        [name("alice"), name("bob")]
    );
    assert_eq!(me.senate, name("senate"));

    let f = committee();
    let post = |token: &'static str, from: &str| {
        let event = NewEvent::new(f.clone(), Kind::Note, person_addr(from), "hi");
        let server = &server;
        async move { server.post_event(token, &event).await.status() }
    };
    assert_eq!(post(ALICE, "alice").await, StatusCode::OK);
    assert_eq!(post(ALICE, "reviewer").await, StatusCode::OK, "seated");
    assert_eq!(post(ALICE, "tester").await, StatusCode::OK, "seated now");
    // Each is alice's: bob runs neither.
    assert_eq!(post(BOB, "reviewer").await, StatusCode::FORBIDDEN);
    assert_eq!(post(BOB, "tester").await, StatusCode::FORBIDDEN);

    // Carol is in the configuration but not in the roster passed in.
    let carol = NewEvent::new(f.clone(), Kind::Note, person_addr("carol"), "hi");
    api_error(
        server.post_event(CAROL, &carol).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_people_cannot_run_agents_under_one_name() {
    let server = TestServer::start().await;
    let f = committee();
    // Alice acts as claude first: she runs it.
    assert_eq!(server.act_as(ALICE, "claude").await, StatusCode::OK);

    // Bob can take that name by no route: not by a heartbeat, a stream, a
    // cursor or an append.
    api_error(
        server.heartbeat(BOB, "claude").await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    api_error(
        server.try_stream(BOB, Some("claude"), None, None).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    assert_eq!(server.act_as(BOB, "claude").await, StatusCode::FORBIDDEN);
    api_error(
        server
            .post_event(
                BOB,
                &NewEvent::new(f.clone(), Kind::Note, person_addr("claude"), "mine"),
            )
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    // Nor a person's handle: a person and an agent share one namespace.
    assert_eq!(server.act_as(BOB, "carol").await, StatusCode::FORBIDDEN);

    // His own agent goes by another name.
    assert_eq!(server.act_as(BOB, "codex").await, StatusCode::OK);
    let roster = server.running.hub.roster().await.unwrap();
    assert_eq!(roster.owner_of(&path("claude")), Some(&name("alice")));
    assert_eq!(roster.owner_of(&path("codex")), Some(&name("bob")));
    for (token, agents) in [(ALICE, [path("claude")]), (BOB, [path("codex")])] {
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
async fn the_senate_name_the_speaker_and_the_locale_come_from_configuration() {
    let mut config = ServerConfig::from_toml_str(
        r#"
        name = "north-desk"
        listen = "127.0.0.1:0"
        database_url = "memory:"
        locale = "en"

        [roster]
        speaker = "copilot"
        agents = [{ name = "copilot", owner = "alice" }]

        [[roster.principals]]
        handle = "alice"

        [[roster.principals]]
        handle = "bob"

        [[tokens]]
        person = "alice"
        token = "alice-dev-token"
        "#,
    )
    .unwrap();
    config.listen = "127.0.0.1:0".into();
    let running = Server::start(config, Arc::new(MemoryStore::new()))
        .await
        .unwrap();
    assert_eq!(running.hub.locale(), Locale::En);
    assert_eq!(running.hub.senate_name(), &name("north-desk"));
    assert_eq!(
        running.hub.speaker_seat(),
        Some(&SpeakerSeat::Senator(path("copilot")))
    );
    let seated = running.hub.roster().await.unwrap();
    assert_eq!(seated.named_speaker(), Some(&path("copilot")));
    assert_eq!(seated.owner_of(&path("copilot")), Some(&name("alice")));
    running.shutdown().await.unwrap();

    let top = Server::start(
        ServerConfig::new(roster(), tokens()),
        Arc::new(MemoryStore::new()),
    )
    .await
    .unwrap();
    assert_eq!(top.hub.locale(), Locale::default());
    assert_eq!(top.hub.senate_name(), &name("senate"));
    assert_eq!(top.hub.speaker_seat(), None);
    assert_eq!(top.hub.roster().await.unwrap().named_speaker(), None);
    top.shutdown().await.unwrap();
}

#[tokio::test]
async fn people_can_be_replaced_while_agents_stay_seated() {
    let server = TestServer::start().await;
    seat(&server, &["copilot"]).await;
    let hub = server.running.hub.clone();
    hub.set_principals(vec![
        Principal::new(name("alice")),
        Principal::new(name("dave")),
    ])
    .await
    .unwrap();
    let roster = hub.roster().await.unwrap();
    assert!(roster.principal(&name("dave")).is_some());
    assert!(roster.principal(&name("bob")).is_none());
    assert_eq!(roster.agents(), [path("copilot")]);

    // Bob's token still authenticates, but he is nobody here now.
    let event = NewEvent::new(committee(), Kind::Note, person_addr("bob"), "hi");
    api_error(
        server.post_event(BOB, &event).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn two_agents_of_one_person_have_separate_presence() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |config| {
        config.presence_disconnected_secs = 1;
        config.lease_secs = 60;
    })
    .await;
    let hub = server.running.hub.clone();
    let f = committee();
    server.append("hello").await;

    // claude has a stream; codex only heartbeats once and then goes quiet.
    let stream = server.stream_as(ALICE, Some("copilot"), None, None).await;
    seat(&server, &["copilot", "codex"]).await;
    assert!(hub.is_agent_connected(&f, &path("copilot")).await);
    assert!(!hub.is_agent_connected(&f, &path("codex")).await);

    // Keep claude alive; codex is silent past the disconnect window.
    for _ in 0..8 {
        seat(&server, &["copilot"]).await;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let page: PresencePage = server
        .request(Method::GET, &presence_path(&f), BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let state = |agent: &str| {
        page.agents
            .iter()
            .find(|a| a.agent == path(agent))
            .map(|a| a.state)
    };
    assert_eq!(state("copilot"), Some(PresenceState::Online));
    assert_eq!(state("codex"), Some(PresenceState::Disconnected));
    assert_eq!(
        state("alice"),
        None,
        "a person has no presence of their own"
    );

    // Presence events are the server's, written with an empty body and the
    // payload in `meta`.
    let events = hub.read(&f, Seq::ZERO, 50).await.unwrap();
    let presence: Vec<_> = events.iter().filter(|e| e.kind == Kind::Presence).collect();
    // The first agent's seat (copilot's `online`), then codex's disconnect;
    // copilot's own `online` after the seat is not written.
    assert_eq!(presence.len(), 2);
    for event in &presence {
        assert_eq!(event.body, "");
        assert_eq!(event.from, Address::Committee);
        assert!(event.meta.contains_key(meta::PRESENCE));
    }
    let first = PresencePayload::of(presence[0]).unwrap();
    assert_eq!(
        (first.agent, first.state),
        (path("copilot"), PresenceState::Online)
    );
    let second = PresencePayload::of(presence[1]).unwrap();
    assert_eq!(
        (second.agent, second.state),
        (path("codex"), PresenceState::Disconnected)
    );

    // The delegation is present while either agent is.
    use senate_core::DelegationPresence;
    assert_eq!(
        hub.delegation_presence(&f, &name("alice")).await.unwrap(),
        DelegationPresence::Present
    );
    drop(stream);
    server.running.shutdown().await.unwrap();
}

/// Replaces "a restart seats the agents it finds in the log again" (which
/// asserted that, with several people, who runs each agent was lost on
/// restart and the first person to act as it again claimed it): seats are
/// kept in the store's registry, so every agent sits again under the
/// principal that ran it.
#[tokio::test]
async fn a_restart_seats_the_agents_again_under_the_principals_that_run_them() {
    let store: Arc<dyn senate_log::LogStore> = Arc::new(MemoryStore::new());
    let first = TestServer::start_configured(store.clone(), |_| {}).await;
    seat(&first, &["copilot", "codex"]).await;
    // An agent that only ever wrote: no stream, no heartbeat.
    let wrote = NewEvent::new(committee(), Kind::Note, person_addr("claude"), "hello");
    assert_eq!(first.post_event(BOB, &wrote).await.status(), StatusCode::OK);
    first.running.shutdown().await.unwrap();

    // The server that comes back seats every agent again before it serves
    // anything, so `senator::*` resolves against the two on the committee
    // from the first request, and the roster in-process components read at
    // their start is the whole one.
    let second = TestServer::start_configured(store, |_| {}).await;
    let roster = second.running.hub.roster().await.unwrap();
    let mut seated: Vec<String> = roster.agents().iter().map(ToString::to_string).collect();
    seated.sort();
    assert_eq!(seated, ["claude", "codex", "copilot"]);
    // Two principals, and each agent is still its own principal's.
    assert_eq!(roster.owner_of(&path("claude")), Some(&name("bob")));
    assert_eq!(roster.owner_of(&path("copilot")), Some(&name("alice")));
    assert_eq!(roster.owner_of(&path("codex")), Some(&name("alice")));
    let agents_of = |token: &'static str| {
        let request = second.request(Method::GET, "/v1/me", token);
        async move {
            request
                .send()
                .await
                .unwrap()
                .json::<Me>()
                .await
                .unwrap()
                .agents
        }
    };
    assert_eq!(agents_of(BOB).await, [path("claude")]);
    let mut alices = agents_of(ALICE).await;
    alices.sort();
    assert_eq!(alices, [path("codex"), path("copilot")]);

    let mut on = second
        .running
        .hub
        .committee_agents(&committee())
        .await
        .unwrap();
    on.sort();
    assert_eq!(on, [path("codex"), path("copilot")]);
    let error = api_error(
        second.post_event(BOB, &question_to("senator::*")).await,
        StatusCode::CONFLICT,
        "ambiguous_recipient",
    )
    .await;
    assert_eq!(error.candidates.len(), 2, "{error:?}");
    // Nobody else acts as an agent kept as another principal's.
    assert_eq!(second.act_as(ALICE, "claude").await, StatusCode::FORBIDDEN);
    assert_eq!(second.act_as(BOB, "codex").await, StatusCode::FORBIDDEN);
    second.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_stream_without_a_senator_reads_everything_and_counts_for_nobody() {
    let server = TestServer::start().await;
    let hub = server.running.hub.clone();
    let mut reading = server.stream(ALICE, None, None).await;
    server.append("one").await;
    assert_eq!(reading.event().await.unwrap().body, "one");
    assert!(!hub.is_agent_connected(&committee(), &path("alice")).await);

    // Naming someone else's agent is refused, and attaches nothing.
    assert_eq!(server.act_as(BOB, "claude").await, StatusCode::OK);
    let response = server.try_stream(ALICE, Some("claude"), None, None).await;
    api_error(response, StatusCode::FORBIDDEN, "forbidden").await;
    assert!(!hub.is_agent_connected(&committee(), &path("claude")).await);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_may_not_decide_but_its_person_may() {
    let server = TestServer::start().await;
    let decision =
        |from: &str| NewEvent::new(committee(), Kind::Decision, person_addr(from), "limit 30");
    api_error(
        server.post_event(ALICE, &decision("copilot")).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    assert_eq!(
        server.post_event(ALICE, &decision("alice")).await.status(),
        StatusCode::OK
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn me_says_whether_roots_are_judged_here() {
    let me = |server: &TestServer| {
        let request = server.request(Method::GET, "/v1/me", ALICE);
        async move { request.send().await.unwrap().json::<Me>().await.unwrap() }
    };
    // The server alone judges nothing: a bridge must not wait for verdicts.
    let alone = TestServer::start().await;
    assert!(!me(&alone).await.triaged);
    alone.running.shutdown().await.unwrap();

    // Whoever attaches the Speaker says so.
    let config = ServerConfig::new(roster(), tokens());
    let running = Server::builder(config, Arc::new(MemoryStore::new()))
        .triaged(true)
        .start()
        .await
        .unwrap();
    let judged = TestServer {
        running,
        client: reqwest::Client::new(),
    };
    assert!(me(&judged).await.triaged);
    judged.running.shutdown().await.unwrap();
}
