//! The roles as the server keeps them: the Speaker named by id and its
//! office, who writes for it, the chair, the boundary rule, the relay of an
//! answer back where it was asked, secretaries, child Senates, and seats
//! that outlive a restart.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use reqwest::{Method, StatusCode};
use senate_core::Hub as _;
use senate_core::api::{
    AppendResponse, ChairInfo, ChildSenate, Me, RegisterChild, Secretaries, SetChair,
    SetSecretaries,
};
use senate_core::{
    Address, Awaiting, ChairSource, CommitteeId, DocketPage, Event, Kind, NewEvent, Origin,
    PickPayload, Principal, SenatorKind, SpeakerName, SpeakerSeat, meta,
};
use senate_log::{LogStore, MemoryStore};
use senate_server::{Running, Server, ServerConfig, ServerError};
use serde_json::json;

fn ibot() -> CommitteeId {
    CommitteeId::new("ibot").unwrap()
}

fn speaker_id(s: &str) -> Option<SpeakerName> {
    Some(s.parse().unwrap())
}

/// A server over `store` whose configuration is `roster()` and `tokens()`
/// changed by `configure`, with the built-in session `built_in` beside it.
async fn try_start(
    store: Arc<dyn LogStore>,
    built_in: Option<&str>,
    configure: impl FnOnce(&mut ServerConfig),
) -> Result<TestServer, ServerError> {
    let mut config = ServerConfig::new(roster(), tokens());
    configure(&mut config);
    let running: Running = Server::builder(config, store)
        .built_in_session(built_in.map(path))
        .start()
        .await?;
    Ok(TestServer {
        running,
        client: reqwest::Client::new(),
    })
}

async fn start(built_in: Option<&str>, configure: impl FnOnce(&mut ServerConfig)) -> TestServer {
    try_start(Arc::new(MemoryStore::new()), built_in, configure)
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

async fn stored(server: &TestServer, token: &str, event: &NewEvent) -> Event {
    let response = server.post_event(token, event).await;
    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{event:?}: {text}");
    serde_json::from_str::<AppendResponse>(&text).unwrap().event
}

async fn put<T: serde::Serialize>(
    server: &TestServer,
    token: &str,
    route: &str,
    body: &T,
) -> reqwest::Response {
    server
        .request(Method::PUT, route, token)
        .json(body)
        .send()
        .await
        .unwrap()
}

async fn get<T: serde::de::DeserializeOwned>(server: &TestServer, token: &str, route: &str) -> T {
    let response = server
        .request(Method::GET, route, token)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{route}");
    response.json().await.unwrap()
}

async fn docket(server: &TestServer, committee: &CommitteeId, role: &str) -> DocketPage {
    get(
        server,
        ALICE,
        &format!("{}?role={role}", senate_core::api::docket_path(committee)),
    )
    .await
}

fn from_speaker(committee: CommitteeId, kind: Kind, body: &str) -> NewEvent {
    NewEvent::new(committee, kind, Address::Speaker, body)
}

fn acted_by(event: &Event) -> Option<&str> {
    event.meta.get(meta::ACTED_BY).and_then(|v| v.as_str())
}

fn intended(event: &Event) -> Option<&str> {
    event.meta.get(meta::INTENDED).and_then(|v| v.as_str())
}

/// A note on `ibot` from alice, so the committee exists.
async fn open_ibot(server: &TestServer) {
    stored(
        server,
        ALICE,
        &NewEvent::new(ibot(), Kind::Note, person_addr("alice"), "ibot opens"),
    )
    .await;
}

// The Speaker, named by id.

#[tokio::test]
async fn the_speaker_is_named_by_id_in_each_accepted_form() {
    // A root presided over by one of its people.
    let server = start(None, |c| c.roster.speaker = speaker_id("senator::alice")).await;
    let office = me(&server, BOB).await.office.unwrap();
    assert_eq!(office.speaker.to_string(), "senator::alice");
    assert_eq!(office.kind, SenatorKind::Person);
    assert!(
        office.desk.is_empty(),
        "nobody works the desk: it waits on alice"
    );
    assert!(server.running.hub.notes().is_empty());
    server.running.shutdown().await.unwrap();

    // An unattended root: an agent, configured explicitly, works its own
    // desk as it did alone.
    let server = start(None, |c| {
        c.roster.speaker = speaker_id("senator::claude");
        c.roster.agents = vec![seated("claude", "alice")];
    })
    .await;
    let alice = me(&server, ALICE).await;
    let office = alice.office.unwrap();
    assert_eq!(office.kind, SenatorKind::Agent);
    assert_eq!(office.desk, [path("claude")]);
    assert_eq!(alice.speaker, Some(SpeakerSeat::Senator(path("claude"))));
    assert!(!alice.triaged);
    server.running.shutdown().await.unwrap();

    // A child: the parent's senator.
    let server = start(None, |c| {
        c.parent = Some("acme".parse().unwrap());
        c.roster.speaker = speaker_id("/acme::senator::alice");
    })
    .await;
    let office = me(&server, ALICE).await.office.unwrap();
    assert_eq!(office.speaker.to_string(), "/acme::senator::alice");
    assert_eq!(office.speaker_here(), None);
    assert_eq!(office.kind, SenatorKind::Person);
    // Whether it is a person there is the parent's to check.
    assert_eq!(server.running.hub.notes().len(), 1);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn older_forms_of_the_speaker_are_read_with_a_note() {
    // A bare name in a root: this Senate's senator.
    let server = start(None, |c| c.roster.speaker = speaker_id("alice")).await;
    let office = me(&server, ALICE).await.office.unwrap();
    assert_eq!(office.speaker.to_string(), "senator::alice");
    let notes = server.running.hub.notes();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("[senator::alice]"), "{notes:?}");
    server.running.shutdown().await.unwrap();

    // A bare name under a parent: the parent's senator.
    let server = start(None, |c| {
        c.parent = Some("acme".parse().unwrap());
        c.locale = senate_core::Locale::Zh;
        c.roster.speaker = speaker_id("alice");
    })
    .await;
    let office = me(&server, ALICE).await.office.unwrap();
    assert_eq!(office.speaker.to_string(), "/acme::senator::alice");
    let notes = server.running.hub.notes();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("[/acme::senator::alice]") && n.contains("上级")),
        "in the server's language: {notes:?}"
    );
    server.running.shutdown().await.unwrap();

    // Configured before Speakers were named: the built-in session runs
    // and nothing names a Speaker, so it is the Speaker, and judges roots.
    let server = start(Some("secretary"), |_| {}).await;
    let alice = me(&server, ALICE).await;
    let office = alice.office.unwrap();
    assert_eq!(office.speaker.to_string(), "senator::secretary");
    assert_eq!(office.kind, SenatorKind::BuiltIn);
    assert_eq!(alice.speaker, Some(SpeakerSeat::BuiltIn));
    assert!(alice.triaged);
    assert_eq!(server.running.hub.notes().len(), 1);
    // Nobody acts as the built-in session over HTTP.
    assert_eq!(
        server.act_as(ALICE, "secretary").await,
        StatusCode::FORBIDDEN
    );
    server.running.shutdown().await.unwrap();

    // The built-in session named as the Speaker, under another name, among
    // several people.
    let server = start(Some("clerk"), |c| {
        c.roster.speaker = speaker_id("senator::clerk")
    })
    .await;
    let office = me(&server, ALICE).await.office.unwrap();
    assert_eq!(office.kind, SenatorKind::BuiltIn);
    assert_eq!(office.built_in, Some(path("clerk")));
    assert!(server.running.hub.notes().is_empty());
    server.running.shutdown().await.unwrap();

    // `built_in_speaker(true)` still reads as the built-in session under
    // its default name.
    let running = Server::builder(
        ServerConfig::new(roster(), tokens()),
        Arc::new(MemoryStore::new()),
    )
    .built_in_speaker(true)
    .start()
    .await
    .unwrap();
    assert_eq!(running.hub.built_in(), Some(&path("secretary")));
    assert_eq!(running.hub.speaker_seat(), Some(&SpeakerSeat::BuiltIn));
    running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_note_is_said_once_across_restarts() {
    let store: Arc<dyn LogStore> = Arc::new(MemoryStore::new());
    for _ in 0..2 {
        let server = try_start(store.clone(), Some("secretary"), |_| {})
            .await
            .unwrap();
        // Kept for whoever shows the status, every start.
        assert_eq!(server.running.hub.notes().len(), 1);
        server.running.shutdown().await.unwrap();
    }
    let said = store.registry("note:").await.unwrap();
    assert_eq!(said.len(), 1, "remembered as said: {said:?}");
}

#[tokio::test]
async fn a_speaker_of_another_senate_than_the_parent_is_refused_at_start() {
    for (parent, speaker) in [
        (Some("acme"), "/other::senator::alice"),
        (None, "/acme::senator::alice"),
        // Under a parent, the Speaker is the parent's senator.
        (Some("acme"), "senator::alice"),
    ] {
        let refused = try_start(Arc::new(MemoryStore::new()), None, |c| {
            c.parent = parent.map(|p| p.parse().unwrap());
            c.roster.speaker = speaker_id(speaker);
        })
        .await;
        match refused {
            Err(ServerError::Config(e)) => {
                assert!(e.to_string().contains("Speaker"), "{e}");
            }
            Err(other) => panic!("{speaker}: {other}"),
            Ok(_) => panic!("{speaker} under {parent:?} was accepted"),
        }
    }
}

// Writing for the Speaker's office.

/// alice presides; claude and aider (alice's) and codex (bob's) are her
/// secretaries; the built-in session `clerk` runs too.
fn office_of_alice(c: &mut ServerConfig) {
    c.roster.speaker = speaker_id("senator::alice");
    c.roster.agents = vec![
        seated("claude", "alice"),
        seated("aider", "alice"),
        seated("codex", "bob"),
    ];
    c.roster.secretaries = [(
        path("alice"),
        vec![path("claude"), path("codex"), path("clerk")],
    )]
    .into_iter()
    .collect();
}

fn with_acted_by(event: NewEvent, who: &str) -> NewEvent {
    event.with_meta(meta::ACTED_BY, json!(who))
}

#[tokio::test]
async fn writing_as_the_speaker_names_who_acts() {
    let server = start(Some("clerk"), office_of_alice).await;
    let office = me(&server, CAROL).await.office.unwrap();
    assert_eq!(office.desk, [path("claude"), path("codex"), path("clerk")]);
    assert!(office.triages(), "the built-in session works the desk");
    let note = || from_speaker(committee(), Kind::Note, "the desk speaks");

    // Each desk worker, through the principal that runs it; and the
    // Speaker itself.
    for (token, who) in [(ALICE, "claude"), (BOB, "codex"), (ALICE, "alice")] {
        let event = stored(&server, token, &with_acted_by(note(), who)).await;
        assert_eq!(event.from, Address::Speaker);
        assert_eq!(acted_by(&event), Some(who));
    }
    // Not given: the one desk worker its principal runs.
    let event = stored(&server, BOB, &note()).await;
    assert_eq!(acted_by(&event), Some("codex"));
    let event = stored(&server, ALICE, &note()).await;
    assert_eq!(acted_by(&event), Some("claude"));

    // Anyone else, someone else's worker, an agent that is not on the
    // desk, the built-in session: refused.
    for (token, who) in [
        (CAROL, Some("claude")),
        (CAROL, Some("alice")),
        (CAROL, None),
        (ALICE, Some("codex")),
        (BOB, Some("claude")),
        (ALICE, Some("aider")),
        (ALICE, Some("clerk")),
        (BOB, Some("alice")),
    ] {
        let event = match who {
            Some(who) => with_acted_by(note(), who),
            None => note(),
        };
        let refused = api_error(
            server.post_event(token, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
        assert!(refused.message.contains("senator::alice"), "{refused:?}");
    }
    server.running.shutdown().await.unwrap();

    // A principal running two desk workers must say which.
    let server = start(None, |c| {
        office_of_alice(c);
        c.roster.secretaries = [(path("alice"), vec![path("claude"), path("aider")])]
            .into_iter()
            .collect();
    })
    .await;
    api_error(
        server.post_event(ALICE, &note()).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    stored(&server, ALICE, &with_acted_by(note(), "aider")).await;
    // A senator does not pass its words off as the desk's.
    for key in [meta::ACTED_BY, meta::ORIGIN, meta::ANSWERED_BY] {
        let event = NewEvent::new(committee(), Kind::Note, person_addr("alice"), "x")
            .with_meta(key, json!("claude"));
        api_error(
            server.post_event(ALICE, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }
    server.running.shutdown().await.unwrap();
}

// The chair.

async fn chair(server: &TestServer, committee: &CommitteeId) -> ChairInfo {
    get(server, BOB, &senate_core::api::chair_path(committee)).await
}

#[tokio::test]
async fn a_committee_is_chaired_by_its_creator_then_its_first_agent_until_one_is_set() {
    let server = TestServer::start().await;
    assert_eq!(chair(&server, &committee()).await.chair, None);
    server.append("hello").await;
    let info = chair(&server, &committee()).await;
    assert_eq!(
        (info.chair, info.source),
        (Some(path("alice")), Some(ChairSource::Creator))
    );
    // The first agent to take a seat: its first appearance is held by the
    // leases, its first event of its own is in the log.
    let _claude = server.stream_as(BOB, Some("claude"), None, None).await;
    stored(
        &server,
        BOB,
        &NewEvent::new(committee(), Kind::Note, agent_addr("claude"), "here"),
    )
    .await;
    let info = chair(&server, &committee()).await;
    assert_eq!(
        (info.chair, info.source),
        (Some(path("claude")), Some(ChairSource::FirstAgent))
    );
    let _codex = server.stream_as(ALICE, Some("codex"), None, None).await;
    stored(
        &server,
        ALICE,
        &NewEvent::new(committee(), Kind::Note, agent_addr("codex"), "here too"),
    )
    .await;
    assert_eq!(
        chair(&server, &committee()).await.chair,
        Some(path("claude"))
    );

    // Any principal sets it, and it is a fact of the log.
    let route = senate_core::api::chair_path(&committee());
    let response = put(
        &server,
        CAROL,
        &route,
        &SetChair {
            senator: path("codex"),
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let info: ChairInfo = response.json().await.unwrap();
    assert_eq!(
        (info.chair, info.source),
        (Some(path("codex")), Some(ChairSource::Set))
    );
    let log = server
        .running
        .hub
        .read(&committee(), senate_core::Seq::ZERO, 100)
        .await
        .unwrap();
    let set = log
        .iter()
        .rfind(|e| e.meta.contains_key(meta::CHAIR))
        .unwrap();
    assert_eq!(set.from, Address::Committee);
    // A person may chair too.
    let response = put(
        &server,
        BOB,
        &route,
        &SetChair {
            senator: path("bob"),
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(chair(&server, &committee()).await.chair, Some(path("bob")));

    // Nobody this Senate knows cannot chair.
    api_error(
        put(
            &server,
            ALICE,
            &route,
            &SetChair {
                senator: path("nobody"),
            },
        )
        .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    // Someone who is no longer a principal here may not set it.
    server
        .running
        .hub
        .set_principals(vec![
            Principal::new(name("alice")),
            Principal::new(name("bob")),
        ])
        .unwrap();
    api_error(
        put(
            &server,
            CAROL,
            &route,
            &SetChair {
                senator: path("alice"),
            },
        )
        .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn whichever_agent_is_put_to_the_chair_then_the_desk_then_refused() {
    let server = start(None, |c| {
        c.roster.speaker = speaker_id("senator::alice");
        c.roster.agents = vec![seated("claude", "alice"), seated("codex", "alice")];
        c.roster.secretaries = [(path("alice"), vec![path("codex")])].into_iter().collect();
    })
    .await;
    // claude takes the first seat (it chairs) and is connected; codex
    // only heartbeats.
    let claude = server.stream_as(ALICE, Some("claude"), None, None).await;
    stored(
        &server,
        ALICE,
        &NewEvent::new(committee(), Kind::Note, agent_addr("claude"), "here"),
    )
    .await;
    assert_eq!(
        chair(&server, &committee()).await.chair,
        Some(path("claude"))
    );
    server.heartbeat(ALICE, "codex").await;
    let ask = || {
        NewEvent::new(
            committee(),
            Kind::Question,
            person_addr("bob"),
            "who owns refunds?",
        )
        .to(Address::any_agent())
    };
    let to_chair = stored(&server, BOB, &ask()).await;
    assert_eq!(to_chair.to, [Address::Chair]);
    assert_eq!(
        PickPayload::of(&to_chair).unwrap().candidates,
        [path("claude"), path("codex")]
    );
    // It waits on the chair, not on the desk.
    let chairs = docket(&server, &committee(), "chair").await;
    assert!(matches!(
        &chairs.items[..],
        [item] if item.event.seq == to_chair.seq && matches!(item.awaiting, Awaiting::Pick { .. })
    ));
    assert!(
        docket(&server, &committee(), "speaker")
            .await
            .items
            .is_empty()
    );
    // The chair picks: a relay from the chair takes it off.
    stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Question,
            agent_addr("claude"),
            "who owns refunds?",
        )
        .to(agent_addr("codex"))
        .in_thread(to_chair.seq),
    )
    .await;
    assert!(
        docket(&server, &committee(), "chair")
            .await
            .items
            .iter()
            .all(|i| i.event.seq != to_chair.seq)
    );

    // The chair goes away: the desk picks.
    drop(claude);
    eventually("claude is gone", || async {
        !server
            .running
            .hub
            .is_agent_connected(&committee(), &path("claude"))
            .await
    })
    .await;
    // Still on the committee (a heartbeat), but no longer connected.
    server.heartbeat(ALICE, "claude").await;
    let to_desk = stored(&server, BOB, &ask()).await;
    assert_eq!(to_desk.to, [Address::Speaker]);
    let desk = docket(&server, &committee(), "speaker").await;
    assert!(desk.items.iter().any(|i| i.event.seq == to_desk.seq));
    // An agent sender is refused with the candidates, as always.
    api_error(
        server
            .post_event(
                ALICE,
                &NewEvent::new(committee(), Kind::Question, agent_addr("claude"), "?")
                    .to(Address::any_agent()),
            )
            .await,
        StatusCode::CONFLICT,
        "ambiguous_recipient",
    )
    .await;
    server.running.shutdown().await.unwrap();

    // No chair present and nobody on the desk: refused with the candidates.
    let server = start(None, |c| {
        c.roster.speaker = speaker_id("senator::alice");
        c.roster.agents = vec![seated("claude", "alice"), seated("codex", "alice")];
    })
    .await;
    for agent in ["claude", "codex"] {
        server.heartbeat(ALICE, agent).await;
    }
    let refused = api_error(
        server.post_event(BOB, &ask()).await,
        StatusCode::CONFLICT,
        "ambiguous_recipient",
    )
    .await;
    assert_eq!(refused.candidates, [path("claude"), path("codex")]);
    server.running.shutdown().await.unwrap();
}

// The boundary rule.

/// alice presides with claude (hers) as her secretary; codex is bob's.
fn boundary(c: &mut ServerConfig) {
    c.roster.speaker = speaker_id("senator::alice");
    c.roster.agents = vec![seated("claude", "alice"), seated("codex", "bob")];
    c.roster.secretaries = [(path("alice"), vec![path("claude")])]
        .into_iter()
        .collect();
}

fn meant_for(from: &str, locator: &str) -> NewEvent {
    NewEvent::new(
        committee(),
        Kind::Question,
        person_addr(from),
        "is the API frozen?",
    )
    .with_meta(meta::INTENDED, json!(locator))
}

#[tokio::test]
async fn a_persons_message_crosses_directly_and_an_agents_by_way_of_the_desk() {
    let server = start(None, boundary).await;
    open_ibot(&server).await;

    // A person: delivered on the other committee, a new root there.
    let direct = stored(&server, ALICE, &meant_for("alice", "ibot::senator::codex")).await;
    assert_eq!(direct.committee, ibot());
    assert_eq!(direct.to, [agent_addr("codex")]);
    assert_eq!(direct.thread, None);
    assert_eq!(intended(&direct), Some("/senate::ibot::senator::codex"));

    // An agent: kept on its own committee, for the desk, with where it was
    // meant to go.
    let held = stored(&server, ALICE, &meant_for("claude", "ibot::senator::codex")).await;
    assert_eq!(held.committee, committee());
    assert_eq!(held.to, [Address::Speaker]);
    assert_eq!(intended(&held), Some("/senate::ibot::senator::codex"));
    let desk = docket(&server, &committee(), "speaker").await;
    let item = desk.items.iter().find(|i| i.event.seq == held.seq).unwrap();
    assert_eq!(item.awaiting, Awaiting::Addressed);
    assert_eq!(
        item.intended.as_ref().map(ToString::to_string).as_deref(),
        Some("/senate::ibot::senator::codex")
    );

    // On its own committee: simply addressed.
    let here = stored(&server, ALICE, &meant_for("claude", "senator::codex")).await;
    assert_eq!(
        (here.committee, here.to),
        (committee(), vec![agent_addr("codex")])
    );

    // Above, or a Senate nobody here knows: an agent's goes to the desk; a
    // person's is posted above by their own machine.
    let up = stored(
        &server,
        ALICE,
        &meant_for("claude", "acme::ibot::senator::codex"),
    )
    .await;
    assert_eq!(up.to, [Address::Speaker]);
    assert_eq!(intended(&up), Some("acme::ibot::senator::codex"));
    api_error(
        server
            .post_event(ALICE, &meant_for("alice", "acme::ibot::senator::codex"))
            .await,
        StatusCode::CONFLICT,
        "elsewhere",
    )
    .await;
    // Something here that does not exist: refused with the reason.
    let unknown = api_error(
        server
            .post_event(ALICE, &meant_for("alice", "ibot::senator::nobody"))
            .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    assert!(unknown.message.contains("nobody"), "{unknown:?}");
    api_error(
        server
            .post_event(ALICE, &meant_for("alice", "not a name"))
            .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agents_message_with_no_speaker_to_hold_it_is_refused() {
    let server = TestServer::start().await;
    open_ibot(&server).await;
    api_error(
        server
            .post_event(BOB, &meant_for("copilot", "ibot::senator::alice"))
            .await,
        StatusCode::CONFLICT,
        "conflict",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_answer_where_the_desk_forwarded_goes_back_to_the_asking_thread() {
    let server = start(None, boundary).await;
    open_ibot(&server).await;
    let asked = stored(&server, ALICE, &meant_for("claude", "ibot::senator::codex")).await;

    // The desk (claude, for alice) forwards it: a new root on ibot.
    let origin = Origin {
        locator: "/senate::refund-v2::senator::claude".parse().unwrap(),
        thread: asked.seq,
    };
    let forwarded = stored(
        &server,
        ALICE,
        &from_speaker(ibot(), Kind::Question, "is the API frozen?")
            .to(agent_addr("codex"))
            .with_meta(meta::ORIGIN, origin.value()),
    )
    .await;
    assert_eq!(acted_by(&forwarded), Some("claude"));

    // codex answers there.
    let answer = NewEvent::new(
        ibot(),
        Kind::Answer,
        agent_addr("codex"),
        "frozen since Monday",
    )
    .in_thread(forwarded.seq)
    .to(Address::Speaker);
    stored(&server, BOB, &answer).await;
    let relayed = || async {
        server
            .running
            .hub
            .thread(&committee(), asked.seq)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == Kind::Answer)
            .collect::<Vec<_>>()
    };
    let answers = relayed().await;
    let [back] = &answers[..] else {
        panic!("{answers:?}")
    };
    assert_eq!(back.from, Address::Speaker);
    assert_eq!(back.body, "frozen since Monday");
    assert_eq!(back.to, [agent_addr("claude")]);
    assert_eq!(
        back.meta.get(meta::ANSWERED_BY).and_then(|v| v.as_str()),
        Some("/senate::ibot::senator::codex")
    );
    assert_eq!(
        acted_by(back),
        Some("claude"),
        "the desk worker that forwarded it"
    );
    // The asking thread is dealt with on the desk.
    assert!(
        docket(&server, &committee(), "speaker")
            .await
            .items
            .iter()
            .all(|i| i.event.seq != asked.seq)
    );

    // The same answer again is stored once, and relayed once.
    let again = server.post_event(BOB, &answer).await;
    assert!(again.json::<AppendResponse>().await.unwrap().duplicate);
    assert_eq!(relayed().await.len(), 1);

    // A forged origin on a senator's root is refused, so no answer is
    // relayed into another committee's thread from one.
    api_error(
        server
            .post_event(
                BOB,
                &NewEvent::new(ibot(), Kind::Question, agent_addr("codex"), "?")
                    .with_meta(meta::ORIGIN, origin.value()),
            )
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

// Secretaries.

#[tokio::test]
async fn secretaries_are_registered_by_whom_the_rules_allow_and_kept_across_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("senate.db");
    let configure = |c: &mut ServerConfig| {
        c.roster.speaker = speaker_id("senator::alice");
        c.roster.agents = vec![
            seated("claude", "alice"),
            seated("codex", "bob"),
            seated("gemini", "carol"),
        ];
    };
    let open = || async {
        let store: Arc<dyn LogStore> =
            Arc::new(senate_log::SqliteStore::open(&file).await.unwrap());
        try_start(store, None, configure).await.unwrap()
    };
    let set = |list: &[&str], acted_by: Option<&str>| SetSecretaries {
        secretaries: list.iter().map(|s| path(s)).collect(),
        acted_by: acted_by.map(path),
    };
    let route = |senator: &str| senate_core::api::secretaries_of_path(&path(senator));

    let server = open().await;
    // The senator itself (a person), and the principal that runs an agent.
    for (token, senator, list) in [
        (ALICE, "alice", &["claude", "codex"][..]),
        (ALICE, "claude", &["codex"][..]),
        (BOB, "codex", &["claude"][..]),
    ] {
        let response = put(&server, token, &route(senator), &set(list, None)).await;
        assert_eq!(response.status(), StatusCode::OK, "{senator}");
    }
    // The Speaker's office: bob, for codex on alice's desk.
    let response = put(
        &server,
        BOB,
        &route("gemini"),
        &set(&["codex"], Some("codex")),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    // Anyone else is refused, and told who may.
    for (token, senator, acted_by) in [
        (CAROL, "claude", None),
        (BOB, "claude", None),
        (CAROL, "alice", None),
        (CAROL, "claude", Some("gemini")),
        (BOB, "alice", Some("claude")),
    ] {
        let refused = api_error(
            put(&server, token, &route(senator), &set(&["codex"], acted_by)).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
        assert!(
            refused.message.contains("principal that runs it"),
            "{refused:?}"
        );
    }
    // The roster's rules: never its own, never a principal. (A name not
    // seated yet is seated now, run by whoever registers it: a secretary a
    // daemon provides.)
    for (senator, list) in [("claude", &["claude"][..]), ("claude", &["bob"])] {
        api_error(
            put(&server, ALICE, &route(senator), &set(list, None)).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        )
        .await;
    }
    let listed: Secretaries = get(&server, CAROL, senate_core::api::SECRETARIES_PATH).await;
    let office = me(&server, CAROL).await.office.unwrap();
    assert_eq!(office.desk, [path("claude"), path("codex")]);
    server.running.shutdown().await.unwrap();

    // Kept across a restart, over the configuration's.
    let server = open().await;
    let again: Secretaries = get(&server, CAROL, senate_core::api::SECRETARIES_PATH).await;
    assert_eq!(again, listed);
    assert_eq!(again[&path("gemini")], [path("codex")]);
    assert_eq!(
        me(&server, CAROL).await.office.unwrap().desk,
        [path("claude"), path("codex")]
    );
    // Removing them is kept too.
    let response = put(&server, ALICE, &route("claude"), &set(&[], None)).await;
    assert_eq!(response.status(), StatusCode::OK);
    server.running.shutdown().await.unwrap();
    let server = open().await;
    let after: Secretaries = get(&server, CAROL, senate_core::api::SECRETARIES_PATH).await;
    assert!(!after.contains_key(&path("claude")));
    server.running.shutdown().await.unwrap();
}

// Child Senates.

fn register(speaker: &str) -> RegisterChild {
    RegisterChild {
        speaker: speaker.parse().unwrap(),
    }
}

#[tokio::test]
async fn a_child_senate_registers_as_its_speaker() {
    let store: Arc<dyn LogStore> = Arc::new(MemoryStore::new());
    let server = try_start(store.clone(), None, |c| {
        c.roster.agents = vec![seated("claude", "alice")];
    })
    .await
    .unwrap();
    let route = senate_core::api::child_path(&name("north-desk"));
    let response = put(&server, ALICE, &route, &register("/senate::senator::alice")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let child: ChildSenate = response.json().await.unwrap();
    assert_eq!(child.speaker, path("alice"));
    let listed: Vec<ChildSenate> = get(&server, BOB, senate_core::api::CHILDREN_PATH).await;
    assert_eq!(listed, std::slice::from_ref(&child));
    assert_eq!(
        me(&server, BOB).await.children,
        std::slice::from_ref(&child)
    );
    let one: ChildSenate = get(&server, BOB, &route).await;
    assert_eq!(one, child);

    for (token, child_name, speaker, status) in [
        // Not on this Senate's roster.
        (
            ALICE,
            "south",
            "/senate::senator::dave",
            StatusCode::BAD_REQUEST,
        ),
        // Someone other than the Speaker authenticated.
        (
            BOB,
            "south",
            "/senate::senator::alice",
            StatusCode::FORBIDDEN,
        ),
        // An agent as a child's Speaker: not supported yet.
        (
            ALICE,
            "south",
            "/senate::senator::claude",
            StatusCode::FORBIDDEN,
        ),
        // Not a senator of this Senate.
        (
            ALICE,
            "south",
            "/acme::senator::alice",
            StatusCode::BAD_REQUEST,
        ),
        // This Senate's own name, and a keyword.
        (
            ALICE,
            "senate",
            "/senate::senator::alice",
            StatusCode::BAD_REQUEST,
        ),
        (
            ALICE,
            "chair",
            "/senate::senator::alice",
            StatusCode::BAD_REQUEST,
        ),
        // Registered already, by another Speaker.
        (
            BOB,
            "north-desk",
            "/senate::senator::bob",
            StatusCode::CONFLICT,
        ),
    ] {
        let response = put(
            &server,
            token,
            &format!("{}/{child_name}", senate_core::api::CHILDREN_PATH),
            &register(speaker),
        )
        .await;
        let status_seen = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status_seen, status, "{child_name} {speaker}: {body}");
        if speaker.ends_with("claude") {
            assert!(body.contains("not supported yet"), "{body}");
        }
    }
    let response = server
        .request(
            Method::GET,
            &format!("{}/south", senate_core::api::CHILDREN_PATH),
            BOB,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    server.running.shutdown().await.unwrap();

    // Kept across a restart.
    let server = try_start(store, None, |_| {}).await.unwrap();
    assert_eq!(me(&server, BOB).await.children, [child]);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_registered_child_is_addressed_and_speaks_through_its_speaker() {
    let server = start(None, boundary).await;
    let response = put(
        &server,
        ALICE,
        &senate_core::api::child_path(&name("north-desk")),
        &register("/senate::senator::alice"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let child: Address = "north-desk::ibot::speaker".parse().unwrap();

    // Addressed directly, by anyone.
    let to_child = stored(
        &server,
        BOB,
        &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "status?")
            .to(child.clone()),
    )
    .await;
    assert_eq!(to_child.to, std::slice::from_ref(&child));
    // Through the boundary rule, from an agent: to the child's office.
    let meant = stored(
        &server,
        ALICE,
        &meant_for("claude", "north-desk::ibot::senator::codex"),
    )
    .await;
    assert_eq!(meant.to, std::slice::from_ref(&child));
    assert_eq!(
        intended(&meant),
        Some("/senate/north-desk::ibot::senator::codex")
    );
    // A child nobody registered is no address.
    api_error(
        server
            .post_event(
                BOB,
                &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "?")
                    .to("south::ibot::speaker".parse().unwrap()),
            )
            .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;

    // Its office answers, authenticated as its Speaker; nobody else may.
    let answer = NewEvent::new(committee(), Kind::Answer, child.clone(), "green")
        .in_thread(to_child.seq)
        .to(person_addr("bob"));
    let answered = stored(&server, ALICE, &answer).await;
    assert_eq!(answered.from, child);
    api_error(
        server.post_event(BOB, &answer).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    api_error(
        server
            .post_event(
                ALICE,
                &NewEvent::new(
                    committee(),
                    Kind::Note,
                    "south::ibot::speaker".parse().unwrap(),
                    "x",
                ),
            )
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_team_principal_runs_agents_but_only_a_person_decides() {
    let server = start(None, |c| {
        c.roster.principals.push(senate_core::Principal::of_kind(
            name("acme"),
            senate_core::PrincipalKind::Team,
        ));
        c.tokens.push(senate_server::TokenConfig::with_token(
            name("acme"),
            "acme-token",
        ));
    })
    .await;
    // The team runs an agent of its own, seated on first contact.
    let note = NewEvent::new(
        committee(),
        Kind::Note,
        "senator::acme-bot".parse().unwrap(),
        "from the team's agent",
    );
    stored(&server, "acme-token", &note).await;
    let roster = server.running.hub.roster().await.unwrap();
    assert_eq!(roster.owner_of(&path("acme-bot")), Some(&name("acme")));
    // A decision: refused for the team, accepted for a person.
    let decision = |from: &str| {
        NewEvent::new(
            committee(),
            Kind::Decision,
            from.parse().unwrap(),
            "ship it",
        )
    };
    let refused = server
        .request(
            Method::POST,
            &senate_core::api::events_path(&committee()),
            "acme-token",
        )
        .json(&decision("senator::acme"))
        .send()
        .await
        .unwrap();
    api_error(refused, StatusCode::FORBIDDEN, "forbidden").await;
    stored(&server, ALICE, &decision("senator::alice")).await;
    server.running.shutdown().await.unwrap();
}
