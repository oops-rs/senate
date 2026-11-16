//! What changed from a one-agent-per-person world: a token names a person who
//! acts as agents, `senator::<person>/*` is resolved before it is stored,
//! guests cannot change state, and the seams a private layer replaces.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::*;
use reqwest::{Method, StatusCode};
use senate_core::api::{AppendResponse, Me, PresencePage, presence_path};
use senate_core::{
    Address, Hub, HubError, Kind, Locale, Name, NewEvent, PresenceState, ResolveError, Roster,
    Senior, Seq, meta,
};
use senate_log::MemoryStore;
use senate_server::{Authenticate, Server, ServerConfig};
use serde_json::json;

/// Seat `agents` of alice by having each of them connect.
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

fn question_to(to: &str) -> NewEvent {
    NewEvent::new(
        committee(),
        Kind::Question,
        person_addr("bob/claude"),
        "who owns the gateway?",
    )
    .to(to.parse().unwrap())
}

#[tokio::test]
async fn a_wildcard_to_one_agent_goes_to_that_agent() {
    let server = TestServer::start().await;
    seat(&server, &["alice/claude"]).await;
    let response = server
        .post_event(BOB, &question_to("senator::alice/*"))
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    let appended: AppendResponse = response.json().await.unwrap();
    assert_eq!(appended.event.to, [person_addr("alice/claude")]);
    // What is stored is what came back: the wildcard never reaches the log.
    let stored = server
        .running
        .hub
        .read(&committee(), Seq::ZERO, 10)
        .await
        .unwrap();
    assert_eq!(stored, [appended.event]);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_wildcard_to_several_agents_is_a_409_with_the_candidates() {
    let server = TestServer::start().await;
    seat(&server, &["alice/claude", "alice/codex"]).await;
    let response = server
        .post_event(BOB, &question_to("senator::alice/*"))
        .await;
    let error = api_error(response, StatusCode::CONFLICT, "ambiguous_recipient").await;
    assert_eq!(
        error.candidates,
        [path("alice/claude"), path("alice/codex")]
    );
    assert!(error.message.contains("senator::alice/claude"), "{error:?}");
    assert!(
        server
            .running
            .hub
            .read(&committee(), Seq::ZERO, 10)
            .await
            .unwrap()
            .is_empty(),
        "nothing was appended"
    );

    // The sender names one and tries again.
    let again = server
        .post_event(BOB, &question_to("senator::alice/codex"))
        .await;
    assert_eq!(again.status(), StatusCode::OK);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_wildcard_to_a_person_without_agents_goes_to_the_person() {
    let server = TestServer::start().await;
    let response = server
        .post_event(BOB, &question_to("senator::carol/*"))
        .await;
    let appended: AppendResponse = response.json().await.unwrap();
    assert_eq!(appended.event.to, [person_addr("carol")]);

    // Several wildcards, a repeat after resolution is dropped; an unknown
    // person is the sender's mistake.
    let both = question_to("senator::carol/*").to(person_addr("carol"));
    let appended: AppendResponse = server.post_event(BOB, &both).await.json().await.unwrap();
    assert_eq!(appended.event.to, [person_addr("carol")]);
    let unknown = server
        .post_event(BOB, &question_to("senator::dave/*"))
        .await;
    api_error(unknown, StatusCode::BAD_REQUEST, "bad_request").await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn in_process_callers_resolve_the_same_way() {
    let server = TestServer::start().await;
    let hub = server.running.hub.clone();
    let wildcard = || question_to("senator::alice/*");

    // None: the person. One: the agent.
    let appended = hub.resolve_and_append(wildcard()).await.unwrap();
    assert_eq!(appended.event.to, [person_addr("alice")]);
    seat(&server, &["alice/claude"]).await;
    let appended = hub.resolve_and_append(wildcard()).await.unwrap();
    assert_eq!(appended.event.to, [person_addr("alice/claude")]);

    // Several: nothing is appended, and the candidates come back.
    seat(&server, &["alice/codex"]).await;
    let head = hub.read(&committee(), Seq::ZERO, 10).await.unwrap().len();
    match hub.resolve_and_append(wildcard()).await {
        Err(HubError::Unresolved(ResolveError::Ambiguous { owner, candidates })) => {
            assert_eq!(owner, name("alice"));
            assert_eq!(candidates, [path("alice/claude"), path("alice/codex")]);
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
    let own = NewEvent::new(committee(), Kind::Note, person_addr("carol/claude"), "hi");
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
    for from in ["mallory", "mallory/claude"] {
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
async fn the_roster_seam_takes_a_roster_and_any_level() {
    // A nested Senate, presided over by alice/claude: it seats only agents
    // directly under her, and the presider comes from the roster.
    let nested = Roster::under(
        path("alice/claude"),
        vec![Senior::new(name("alice")), Senior::new(name("bob"))],
        vec![path("alice/claude/reviewer")],
    )
    .unwrap();
    let config = ServerConfig::new(roster(), tokens());
    let running = Server::builder(config, Arc::new(MemoryStore::new()))
        .roster(nested)
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
    assert_eq!(me.presider, Some(path("alice/claude")));
    assert_eq!(me.agents, [path("alice/claude/reviewer")]);

    let f = committee();
    let post = |from: &str| {
        let event = NewEvent::new(f.clone(), Kind::Note, person_addr(from), "hi");
        let server = &server;
        async move { server.post_event(ALICE, &event).await.status() }
    };
    assert_eq!(post("alice").await, StatusCode::OK, "people are global");
    assert_eq!(
        post("alice/claude/tester").await,
        StatusCode::OK,
        "seated now"
    );
    // The presider's own seat is upstairs, and its own subagents are not
    // seated beside it.
    assert_eq!(post("alice/claude").await, StatusCode::FORBIDDEN);
    assert_eq!(post("alice/codex").await, StatusCode::FORBIDDEN);
    assert_eq!(
        post("alice/claude/tester/deeper").await,
        StatusCode::FORBIDDEN
    );

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
async fn the_presider_and_locale_come_from_configuration() {
    let mut config = ServerConfig::from_toml_str(
        r#"
        listen = "127.0.0.1:0"
        database_url = "memory:"
        locale = "en"

        [roster]
        presider = "alice/claude"
        agents = ["alice/claude/reviewer"]

        [[roster.seniors]]
        handle = "alice"

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
    let seated = running.hub.roster().await.unwrap();
    assert_eq!(seated.presider(), Some(&path("alice/claude")));
    running.shutdown().await.unwrap();

    let top = Server::start(
        ServerConfig::new(roster(), tokens()),
        Arc::new(MemoryStore::new()),
    )
    .await
    .unwrap();
    assert_eq!(top.hub.locale(), Locale::default());
    assert_eq!(top.hub.roster().await.unwrap().presider(), None);
    top.shutdown().await.unwrap();
}

#[tokio::test]
async fn people_can_be_replaced_while_agents_stay_seated() {
    let server = TestServer::start().await;
    seat(&server, &["alice/claude"]).await;
    let hub = server.running.hub.clone();
    hub.set_seniors(vec![Senior::new(name("alice")), Senior::new(name("dave"))])
        .unwrap();
    let roster = hub.roster().await.unwrap();
    assert!(roster.senior(&name("dave")).is_some());
    assert!(roster.senior(&name("bob")).is_none());
    assert_eq!(roster.agents(), [path("alice/claude")]);

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
    let stream = server
        .stream_as(ALICE, Some("alice/claude"), None, None)
        .await;
    seat(&server, &["alice/claude", "alice/codex"]).await;
    assert!(hub.is_agent_connected(&f, &path("alice/claude")).await);
    assert!(!hub.is_agent_connected(&f, &path("alice/codex")).await);

    // Keep claude alive; codex is silent past the disconnect window.
    for _ in 0..8 {
        seat(&server, &["alice/claude"]).await;
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
    assert_eq!(state("alice/claude"), Some(PresenceState::Online));
    assert_eq!(state("alice/codex"), Some(PresenceState::Disconnected));
    assert_eq!(
        state("alice"),
        None,
        "a person has no presence of their own"
    );

    // Presence events are the server's, written with an empty body and the
    // payload in `meta`.
    let events = hub.read(&f, Seq::ZERO, 50).await.unwrap();
    let presence: Vec<_> = events.iter().filter(|e| e.kind == Kind::Presence).collect();
    assert_eq!(presence.len(), 1);
    assert_eq!(presence[0].body, "");
    assert_eq!(presence[0].from, Address::Committee);
    assert!(presence[0].meta.contains_key(meta::PRESENCE));

    // The delegation is present while either agent is.
    use senate_core::DelegationPresence;
    assert_eq!(
        hub.delegation_presence(&f, &name("alice")).await.unwrap(),
        DelegationPresence::Present
    );
    drop(stream);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_restart_seats_the_agents_it_finds_in_the_log_again() {
    let store: Arc<dyn senate_log::LogStore> = Arc::new(MemoryStore::new());
    let first = TestServer::start_configured(store.clone(), |_| {}).await;
    seat(&first, &["alice/claude", "alice/codex"]).await;
    // An agent that only ever wrote: no stream, no heartbeat.
    let wrote = NewEvent::new(committee(), Kind::Note, person_addr("bob/claude"), "hello");
    assert_eq!(first.post_event(BOB, &wrote).await.status(), StatusCode::OK);
    first.running.shutdown().await.unwrap();

    // Seats are not stored, but the log and the heartbeats are: the server
    // that comes back seats every agent again before it serves anything, so
    // `senator::alice/*` resolves against both of hers from the first
    // request, and the roster in-process components read at their start is
    // the whole one.
    let second = TestServer::start_configured(store, |_| {}).await;
    let mut seated: Vec<String> = second
        .running
        .hub
        .roster()
        .await
        .unwrap()
        .agents()
        .iter()
        .map(ToString::to_string)
        .collect();
    seated.sort();
    assert_eq!(seated, ["alice/claude", "alice/codex", "bob/claude"]);
    let me: Me = second
        .request(Method::GET, "/v1/me", BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(me.agents, [path("bob/claude")]);
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
    let response = server
        .try_stream(ALICE, Some("bob/claude"), None, None)
        .await;
    api_error(response, StatusCode::FORBIDDEN, "forbidden").await;
    assert!(
        !hub.is_agent_connected(&committee(), &path("bob/claude"))
            .await
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_may_not_decide_but_its_person_may() {
    let server = TestServer::start().await;
    let decision =
        |from: &str| NewEvent::new(committee(), Kind::Decision, person_addr(from), "limit 30");
    api_error(
        server.post_event(ALICE, &decision("alice/claude")).await,
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
