//! The roles as the server keeps them: the Speaker named by id and its
//! office, who writes for it, the chair, the boundary rule, the relay of an
//! answer back where it was asked, secretaries, child Senates, and seats
//! that outlive a restart.

#![allow(clippy::unwrap_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;

use common::*;
use reqwest::{Method, StatusCode};
use senate_core::Hub as _;
use senate_core::api::{
    AppendResponse, ChairInfo, ChildSeat, ChildSeats, ChildSenate, Me, RegisterChild, Secretaries,
    SetChair, SetSecretaries,
};
use senate_core::{
    Address, Awaiting, ChairSource, CommitteeId, DocketPage, Event, Kind, NewEvent, Origin,
    PickPayload, Principal, Refs, SenatorKind, Seq, SpeakerName, SpeakerSeat, meta,
};
use senate_log::{LogStore, MemoryStore};
use senate_server::{Running, Server, ServerConfig, ServerError};
use serde_json::json;

fn ledger() -> CommitteeId {
    CommitteeId::new("ledger").unwrap()
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

/// A note on `ledger` from alice, so the committee exists.
async fn open_ledger(server: &TestServer) {
    stored(
        server,
        ALICE,
        &NewEvent::new(ledger(), Kind::Note, person_addr("alice"), "ledger opens"),
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
        .await
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

/// How many `presence` events the committee's log holds.
async fn presences(server: &TestServer) -> usize {
    server
        .running
        .hub
        .read(&committee(), senate_core::Seq::ZERO, 100)
        .await
        .unwrap()
        .iter()
        .filter(|e| e.kind == Kind::Presence)
        .count()
}

async fn chaired_by(
    server: &TestServer,
) -> (Option<senate_core::SenatorName>, Option<ChairSource>) {
    let info = chair(server, &committee()).await;
    (info.chair, info.source)
}

#[tokio::test]
async fn connecting_takes_the_seat_before_the_agent_has_written() {
    // Each first contact alone, none of them a write: a stream, a
    // heartbeat, a cursor read, a cursor write.
    for contact in ["stream", "heartbeat", "cursor read", "cursor write"] {
        let server = TestServer::start().await;
        server.append("created by alice").await;
        assert_eq!(
            chaired_by(&server).await,
            (Some(path("alice")), Some(ChairSource::Creator)),
            "{contact}: until an agent sits, its creator chairs"
        );
        let _stream;
        match contact {
            "stream" => _stream = Some(server.stream_as(BOB, Some("claude"), None, None).await),
            "heartbeat" => {
                _stream = None;
                assert_eq!(
                    server.heartbeat(BOB, "claude").await.status(),
                    StatusCode::NO_CONTENT
                );
            }
            "cursor read" => {
                _stream = None;
                assert_eq!(server.act_as(BOB, "claude").await, StatusCode::OK);
            }
            _ => {
                _stream = None;
                let response = server
                    .request(
                        reqwest::Method::PUT,
                        &senate_core::api::cursor_path(&committee(), "read"),
                        BOB,
                    )
                    .query(&[("senator", "claude")])
                    .json(&senate_core::api::Cursor {
                        seq: senate_core::Seq::ZERO,
                    })
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
            }
        }
        assert_eq!(
            chaired_by(&server).await,
            (Some(path("claude")), Some(ChairSource::FirstAgent)),
            "{contact}: the first contact takes the seat, written or not"
        );
        server.running.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn the_seat_is_taken_once_and_an_explicit_chair_is_never_overridden() {
    let server = TestServer::start().await;
    server.append("created by alice").await;
    // A person's cursor is no seat.
    assert_eq!(server.act_as(ALICE, "alice").await, StatusCode::OK);
    assert_eq!(presences(&server).await, 0);

    let _claude = server.stream_as(BOB, Some("claude"), None, None).await;
    assert_eq!(presences(&server).await, 1, "the first agent's seat");
    // Later contacts, of claude or of another agent, write no seat.
    let _again = server.stream_as(BOB, Some("claude"), None, None).await;
    assert_eq!(
        server.heartbeat(ALICE, "codex").await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(server.act_as(ALICE, "codex").await, StatusCode::OK);
    assert_eq!(presences(&server).await, 1, "once");
    assert_eq!(
        chaired_by(&server).await,
        (Some(path("claude")), Some(ChairSource::FirstAgent))
    );

    // A chair set before any agent sits stays set when one does.
    let server2 = TestServer::start().await;
    server2.append("created by alice").await;
    let route = senate_core::api::chair_path(&committee());
    let response = put(
        &server2,
        CAROL,
        &route,
        &SetChair {
            senator: path("bob"),
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let _codex = server2.stream_as(ALICE, Some("codex"), None, None).await;
    assert_eq!(
        chaired_by(&server2).await,
        (Some(path("bob")), Some(ChairSource::Set))
    );
    server.running.shutdown().await.unwrap();
    server2.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_first_write_is_the_seat_and_later_contacts_add_nothing() {
    let server = TestServer::start().await;
    server.append("created by alice").await;
    stored(
        &server,
        BOB,
        &NewEvent::new(committee(), Kind::Note, agent_addr("claude"), "here"),
    )
    .await;
    assert_eq!(presences(&server).await, 0, "the write is its own seat");
    assert_eq!(server.act_as(ALICE, "codex").await, StatusCode::OK);
    assert_eq!(presences(&server).await, 0);
    assert_eq!(
        chaired_by(&server).await,
        (Some(path("claude")), Some(ChairSource::FirstAgent))
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_committee_chaired_by_its_creator_hands_the_chair_to_the_first_agent_after_a_restart() {
    // A log from before seats were written: the creator's events, and an
    // agent's bridge that had only heartbeated.
    let store: Arc<dyn LogStore> = Arc::new(MemoryStore::new());
    let first = try_start(store.clone(), None, |_| {}).await.unwrap();
    first.append("created by alice").await;
    assert_eq!(
        chaired_by(&first).await,
        (Some(path("alice")), Some(ChairSource::Creator))
    );
    first.running.shutdown().await.unwrap();
    let second = try_start(store, None, |_| {}).await.unwrap();
    assert_eq!(
        chaired_by(&second).await,
        (Some(path("alice")), Some(ChairSource::Creator))
    );
    assert_eq!(
        second.heartbeat(BOB, "claude").await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        chaired_by(&second).await,
        (Some(path("claude")), Some(ChairSource::FirstAgent))
    );
    assert_eq!(
        second.heartbeat(ALICE, "codex").await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        chaired_by(&second).await,
        (Some(path("claude")), Some(ChairSource::FirstAgent))
    );
    second.running.shutdown().await.unwrap();
}

/// The roots a docket lists as awaiting cover, by seq.
fn away_roots(page: &DocketPage) -> Vec<senate_core::Seq> {
    page.items
        .iter()
        .filter(|i| matches!(i.awaiting, Awaiting::Away { .. }))
        .map(|i| i.event.seq)
        .collect()
}

#[tokio::test]
async fn a_question_to_an_away_agent_is_its_present_agent_secretarys_before_the_chair_and_desk() {
    let server = start(None, |c| {
        c.roster.speaker = speaker_id("senator::alice");
        c.roster.agents = vec![
            seated("claude", "alice"),
            seated("codex", "alice"),
            seated("copilot", "alice"),
        ];
        c.roster.secretaries = [(path("codex"), vec![path("claude")])]
            .into_iter()
            .collect();
    })
    .await;
    // copilot chairs; codex and claude are both away.
    let _copilot = server.stream_as(ALICE, Some("copilot"), None, None).await;
    let ask = |kind: Kind, body: &str| {
        NewEvent::new(committee(), kind, person_addr("bob"), body).to(agent_addr("codex"))
    };
    let question = stored(&server, BOB, &ask(Kind::Question, "who owns refunds?")).await;
    for role in ["speaker", "chair"] {
        let page = docket(&server, &committee(), role).await;
        assert_eq!(
            away_roots(&page),
            [question.seq],
            "{role}: no secretary present"
        );
        assert!(page.covered.is_empty(), "{role}");
    }

    // Its secretary connects: the question is the secretary's, and the
    // chain stops there (not on the chair's docket, nor the desk's).
    let claude = server.stream_as(ALICE, Some("claude"), None, None).await;
    for role in ["speaker", "chair"] {
        let page = docket(&server, &committee(), role).await;
        assert!(away_roots(&page).is_empty(), "{role}: {:?}", page.items);
        assert_eq!(
            page.covered.get(&question.seq),
            Some(&path("claude")),
            "{role}"
        );
    }
    // A request is never listed to cover, by anyone.
    let request = stored(&server, BOB, &ask(Kind::Request, "rename the flag")).await;
    let page = docket(&server, &committee(), "speaker").await;
    assert!(!page.covered.contains_key(&request.seq));
    assert!(away_roots(&page).iter().all(|s| *s != request.seq));

    // The secretary goes away: the chain goes on.
    drop(claude);
    eventually("the question is the chair's again", || async {
        let page = docket(&server, &committee(), "chair").await;
        away_roots(&page) == [question.seq] && page.covered.is_empty()
    })
    .await;
    let page = docket(&server, &committee(), "speaker").await;
    assert_eq!(away_roots(&page), [question.seq]);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_built_in_session_listed_before_an_agent_secretary_covers_first() {
    let server = start(Some("secretary"), |c| {
        c.roster.speaker = speaker_id("senator::alice");
        c.roster.agents = vec![seated("claude", "alice"), seated("codex", "alice")];
        c.roster.secretaries = [(path("codex"), vec![path("secretary"), path("claude")])]
            .into_iter()
            .collect();
    })
    .await;
    let _claude = server.stream_as(ALICE, Some("claude"), None, None).await;
    let question = stored(
        &server,
        BOB,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person_addr("bob"),
            "who owns refunds?",
        )
        .to(agent_addr("codex")),
    )
    .await;
    let page = docket(&server, &committee(), "speaker").await;
    assert!(page.covered.is_empty(), "the built-in session is first");
    assert!(page.items.iter().any(|i| i.event.seq == question.seq));
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
    open_ledger(&server).await;

    // A person: delivered on the other committee, a new root there.
    let direct = stored(
        &server,
        ALICE,
        &meant_for("alice", "ledger::senator::codex"),
    )
    .await;
    assert_eq!(direct.committee, ledger());
    assert_eq!(direct.to, [agent_addr("codex")]);
    assert_eq!(direct.thread, None);
    assert_eq!(intended(&direct), Some("/senate::ledger::senator::codex"));

    // An agent: kept on its own committee, for the desk, with where it was
    // meant to go.
    let held = stored(
        &server,
        ALICE,
        &meant_for("claude", "ledger::senator::codex"),
    )
    .await;
    assert_eq!(held.committee, committee());
    assert_eq!(held.to, [Address::Speaker]);
    assert_eq!(intended(&held), Some("/senate::ledger::senator::codex"));
    let desk = docket(&server, &committee(), "speaker").await;
    let item = desk.items.iter().find(|i| i.event.seq == held.seq).unwrap();
    assert_eq!(item.awaiting, Awaiting::Addressed);
    assert_eq!(
        item.intended.as_ref().map(ToString::to_string).as_deref(),
        Some("/senate::ledger::senator::codex")
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
        &meant_for("claude", "acme::ledger::senator::codex"),
    )
    .await;
    assert_eq!(up.to, [Address::Speaker]);
    assert_eq!(intended(&up), Some("acme::ledger::senator::codex"));
    api_error(
        server
            .post_event(ALICE, &meant_for("alice", "acme::ledger::senator::codex"))
            .await,
        StatusCode::CONFLICT,
        "elsewhere",
    )
    .await;
    // Something here that does not exist: refused with the reason.
    let unknown = api_error(
        server
            .post_event(ALICE, &meant_for("alice", "ledger::senator::nobody"))
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
    open_ledger(&server).await;
    api_error(
        server
            .post_event(BOB, &meant_for("copilot", "ledger::senator::alice"))
            .await,
        StatusCode::CONFLICT,
        "conflict",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_direct_chair_discussion_has_two_local_threads_and_two_participants() {
    let server = TestServer::start().await;
    let hub = &server.running.hub;
    server.append("source opens").await;
    server.append("source local note").await;
    let target_open = stored(
        &server,
        BOB,
        &NewEvent::new(ledger(), Kind::Note, agent_addr("codex"), "target opens"),
    )
    .await;
    let question = meant_for("claude", "ledger::chair")
        .to(Address::Speaker)
        .with_refs(Refs {
            branch: Some("senate/discussion".into()),
            ..Refs::default()
        });
    let receipt = stored(&server, ALICE, &question).await;
    assert_eq!(receipt.id, question.id);
    assert_eq!(receipt.committee, committee());
    assert_eq!(receipt.from, agent_addr("claude"));
    assert_eq!(receipt.thread, None);
    assert!(receipt.to.is_empty());
    assert_eq!(intended(&receipt), Some("/senate::ledger::chair"));
    assert_eq!(
        receipt.meta[meta::CHAIR_DISCUSSION],
        "/senate::ledger::senator::codex"
    );

    let events = hub.read(&ledger(), target_open.seq, 10).await.unwrap();
    let [copy] = &events[..] else {
        panic!("{events:?}")
    };
    assert_ne!(copy.seq, receipt.seq);
    assert_eq!(copy.thread, None);
    assert_eq!(copy.kind, Kind::Question);
    assert_eq!(copy.from, Address::Committee);
    assert_eq!(copy.on_behalf_of, Some(agent_addr("claude")));
    assert_eq!(copy.to, [agent_addr("codex")]);
    assert_eq!(copy.body, question.body);
    assert_eq!(copy.refs, question.refs);
    assert_eq!(copy.meta[meta::CHAIR_DISCUSSION], true);
    assert_eq!(copy.meta.len(), 2);
    assert_eq!(
        Origin::of(copy),
        Some(Origin {
            locator: "/senate::refund-v2::senator::claude".parse().unwrap(),
            thread: receipt.seq,
        })
    );
    assert_eq!(
        hub.chair(&ledger()).await.unwrap().chair,
        Some(path("codex"))
    );
    assert!(!senate_core::needs_triage(copy, &hub.roster_now().people()));
    let desk = docket(&server, &ledger(), "speaker").await;
    assert!(desk.items.is_empty());
    assert!(desk.covered.is_empty());

    let retry = server.post_event(ALICE, &question).await;
    let retry: AppendResponse = retry.json().await.unwrap();
    assert!(retry.duplicate);
    assert_eq!(retry.event, receipt);
    assert_eq!(
        hub.read(&ledger(), target_open.seq, 10).await.unwrap(),
        events
    );

    let local = NewEvent::new(
        ledger(),
        Kind::Answer,
        agent_addr("observer"),
        "local answer",
    )
    .to(Address::Committee)
    .in_thread(copy.seq);
    let local = stored(&server, CAROL, &local).await;
    assert!(!local.is_chair_discussion());
    assert_eq!(
        hub.thread(&committee(), receipt.seq).await.unwrap().len(),
        1
    );

    let answer = NewEvent::new(ledger(), Kind::Answer, agent_addr("codex"), "frozen Monday")
        .in_thread(copy.seq)
        .to(agent_addr("claude"));
    let local_answer = stored(&server, BOB, &answer).await;
    assert_eq!(local_answer.thread, Some(copy.seq));
    assert!(local_answer.to.is_empty());
    let retry = server.post_event(BOB, &answer).await;
    assert!(retry.json::<AppendResponse>().await.unwrap().duplicate);
    let source_thread = hub.thread(&committee(), receipt.seq).await.unwrap();
    let [_, back] = &source_thread[..] else {
        panic!("{source_thread:?}")
    };
    assert_eq!(back.thread, Some(receipt.seq));
    assert_eq!(back.from, Address::Committee);
    assert_eq!(back.on_behalf_of, Some(agent_addr("codex")));
    assert_eq!(back.to, [agent_addr("claude")]);
    assert_eq!(back.body, answer.body);
    assert_eq!(
        Origin::of(back),
        Some(Origin {
            locator: "/senate::ledger::senator::codex".parse().unwrap(),
            thread: copy.seq,
        })
    );

    let follow_up = NewEvent::new(committee(), Kind::Note, agent_addr("claude"), "thanks")
        .in_thread(receipt.seq)
        .to(Address::Committee);
    let local_follow_up = stored(&server, ALICE, &follow_up).await;
    assert!(local_follow_up.to.is_empty());
    let target_thread = hub.thread(&ledger(), copy.seq).await.unwrap();
    let [_, _, _, follow_up_copy] = &target_thread[..] else {
        panic!("{target_thread:?}")
    };
    assert_eq!(follow_up_copy.thread, Some(copy.seq));
    assert_eq!(follow_up_copy.on_behalf_of, Some(agent_addr("claude")));
    assert_eq!(follow_up_copy.to, [agent_addr("codex")]);
    assert_eq!(follow_up_copy.body, follow_up.body);
    assert_eq!(
        hub.chair(&committee()).await.unwrap().chair,
        Some(path("claude"))
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn only_plain_agent_questions_and_notes_open_direct_chair_discussions() {
    let server = start(None, boundary).await;
    open_ledger(&server).await;
    let hub = &server.running.hub;

    let mut request = meant_for("claude", "ledger::chair");
    request.kind = Kind::Request;
    let mut proposal = meant_for("claude", "ledger::chair")
        .with_meta(meta::PROPOSAL, json!(senate_core::PROPOSAL_DECISION));
    proposal.kind = Kind::Note;
    let mut chair_note =
        meant_for("claude", "ledger::chair").with_meta(meta::CHAIR, json!({"senator": "claude"}));
    chair_note.kind = Kind::Note;
    for other in [
        request,
        proposal,
        chair_note,
        meant_for("claude", "ledger::chair").with_meta(meta::RELAYED_FROM, json!(1)),
        meant_for("claude", "ledger::chair").with_meta(meta::SEEN, json!(0)),
        meant_for("claude", "/another::ledger::chair"),
    ] {
        let held = stored(&server, ALICE, &other).await;
        assert_eq!(held.committee, committee());
        assert_eq!(held.to, [Address::Speaker]);
        assert!(!held.is_chair_discussion());
    }
    let local = stored(&server, ALICE, &meant_for("claude", "refund-v2::chair")).await;
    assert_eq!(local.to, [Address::Chair]);
    assert!(!local.is_chair_discussion());

    let mut note = meant_for("claude", "/senate::ledger::chair");
    note.kind = Kind::Note;
    let sent = stored(&server, ALICE, &note).await;
    assert!(sent.is_chair_discussion());
    let events = hub.read(&ledger(), Seq(1), 10).await.unwrap();
    let [copy] = &events[..] else {
        panic!("{events:?}")
    };
    assert_eq!(copy.kind, Kind::Note);
    assert_eq!(copy.to, [person_addr("alice")]);

    for forged in [
        meant_for("claude", "ledger::chair").with_meta(meta::CHAIR_DISCUSSION, json!(true)),
        from_speaker(ledger(), Kind::Note, "office note").with_meta(
            meta::CHAIR_DISCUSSION,
            json!("/senate::ledger::senator::alice"),
        ),
    ] {
        api_error(
            server.post_event(ALICE, &forged).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }

    // Before this key was reserved, an intended message still had a
    // recipient. Reopening a historical desk item cannot create a copy.
    let historical = hub
        .append(
            meant_for("claude", "/senate::ledger::chair")
                .to(Address::Speaker)
                .with_meta(
                    meta::CHAIR_DISCUSSION,
                    json!("/senate::ledger::senator::alice"),
                ),
        )
        .await
        .unwrap()
        .event;
    assert!(!historical.is_chair_discussion());
    let reply = NewEvent::new(
        committee(),
        Kind::Answer,
        agent_addr("claude"),
        "local follow-up",
    )
    .in_thread(historical.seq);
    assert!(!stored(&server, ALICE, &reply).await.is_chair_discussion());
    assert_eq!(hub.read(&ledger(), Seq(1), 10).await.unwrap(), events);
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_chair_discussion_validates_reply_metadata_before_relaying() {
    let server = TestServer::start().await;
    open_ledger(&server).await;
    let hub = &server.running.hub;
    let source = stored(&server, BOB, &meant_for("codex", "ledger::chair")).await;
    let root = hub.get(&ledger(), Seq(2)).await.unwrap().unwrap();
    assert_eq!(root.to, [person_addr("alice")]);
    let answer = || {
        NewEvent::new(ledger(), Kind::Answer, person_addr("alice"), "reply")
            .in_thread(root.seq)
            .to(Address::Committee)
    };
    let first = stored(&server, ALICE, &answer().with_meta(meta::SEEN, json!(0))).await;
    assert!(first.is_chair_discussion());
    for seen in [root.seq, first.seq] {
        let receipt = stored(
            &server,
            ALICE,
            &answer().with_meta(meta::SEEN, json!(seen.0)),
        )
        .await;
        assert!(receipt.is_chair_discussion());
        assert_eq!(receipt.meta[meta::SEEN], seen.0);
    }
    let mirrored = hub.thread(&committee(), source.seq).await.unwrap();
    assert_eq!(mirrored.len(), 4);
    for reply in &mirrored[1..] {
        assert_eq!(reply.to, [agent_addr("codex")]);
        assert!(!reply.meta.contains_key(meta::SEEN));
    }

    let unrelated = stored(
        &server,
        ALICE,
        &NewEvent::new(ledger(), Kind::Note, person_addr("alice"), "unrelated"),
    )
    .await;
    for event in [
        answer().with_meta(meta::RELAYED_FROM, json!(1)),
        answer()
            .with_meta(meta::SEEN, json!(root.seq.0))
            .with_meta(meta::CHAIR, json!({"senator": "alice"})),
        answer().with_meta(meta::SEEN, json!(-1)),
        answer().with_meta(meta::SEEN, json!("2")),
        answer().with_meta(meta::SEEN, json!(1.5)),
        answer().with_meta(meta::SEEN, json!(u64::MAX)),
        answer().with_meta(meta::SEEN, json!(unrelated.seq.0)),
        answer().with_meta(meta::SEEN, json!(unrelated.seq.0 + 100)),
    ] {
        let local = stored(&server, ALICE, &event).await;
        assert!(!local.is_chair_discussion(), "{event:?}");
        assert_eq!(local.to, [Address::Committee]);
    }
    assert_eq!(
        hub.thread(&committee(), source.seq).await.unwrap(),
        mirrored
    );
    server.running.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_answer_where_the_desk_forwarded_goes_back_to_the_asking_thread() {
    let server = start(None, boundary).await;
    open_ledger(&server).await;
    let asked = stored(
        &server,
        ALICE,
        &meant_for("claude", "ledger::senator::codex"),
    )
    .await;

    // The desk (claude, for alice) forwards it: a new root on ledger.
    let origin = Origin {
        locator: "/senate::refund-v2::senator::claude".parse().unwrap(),
        thread: asked.seq,
    };
    let forwarded = stored(
        &server,
        ALICE,
        &from_speaker(ledger(), Kind::Question, "is the API frozen?")
            .to(agent_addr("codex"))
            .with_meta(meta::ORIGIN, origin.value()),
    )
    .await;
    assert_eq!(acted_by(&forwarded), Some("claude"));

    // codex answers there.
    let answer = NewEvent::new(
        ledger(),
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
        Some("/senate::ledger::senator::codex")
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
                &NewEvent::new(ledger(), Kind::Question, agent_addr("codex"), "?")
                    .with_meta(meta::ORIGIN, origin.value()),
            )
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    server.running.shutdown().await.unwrap();
}

// A child Senate that is offline.

/// A root question put to a child Senate that is offline is the Speaker's
/// office's to settle, so it is on the desk's docket: until the office
/// says it is held (or answers for the child), the child replies, or the
/// child is back. A child that never declared itself is not known to be
/// offline.
#[tokio::test]
async fn a_question_for_an_offline_child_senate_awaits_the_office() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |c| {
        c.roster.speaker = speaker_id("senator::alice");
        c.roster.agents = vec![seated("claude", "alice")];
    })
    .await;
    let hub = &server.running.hub;
    let child = |name: &str, seen: Option<i64>| {
        let mut child = ChildSenate::new(common::name(name), path("bob"));
        child.last_seen = seen.map(|m| chrono::Utc::now() - chrono::Duration::minutes(m));
        child
    };
    hub.register_child(child("north-desk", Some(10)))
        .await
        .unwrap();
    hub.register_child(child("south-desk", None)).await.unwrap();
    let ask = |desk: &str, body: &str| {
        let event = NewEvent::new(committee(), Kind::Question, person_addr("carol"), body)
            .to(Address::child(common::name(desk), committee()));
        let server = &server;
        async move {
            let response = server.post_event(CAROL, &event).await;
            assert!(response.status().is_success(), "{}", response.status());
            response.json::<AppendResponse>().await.unwrap().event
        }
    };
    let offline = |page: &DocketPage| -> Vec<(u64, String)> {
        page.items
            .iter()
            .filter_map(|item| match &item.awaiting {
                Awaiting::ChildOffline { child } => Some((item.event.seq.0, child.to_string())),
                _ => None,
            })
            .collect()
    };

    let first = ask("north-desk", "Is the cap 3?").await;
    let second = ask("north-desk", "Which header carries the key?").await;
    // south-desk has not declared itself: not known to be offline.
    ask("south-desk", "Anyone there?").await;
    // A note is never answered for anyone.
    let note = NewEvent::new(committee(), Kind::Note, person_addr("carol"), "fyi")
        .to(Address::child(common::name("north-desk"), committee()));
    assert!(server.post_event(CAROL, &note).await.status().is_success());
    let page = docket(&server, &committee(), "speaker").await;
    assert_eq!(
        offline(&page),
        [
            (first.seq.0, "north-desk".to_owned()),
            (second.seq.0, "north-desk".to_owned())
        ]
    );

    // The office says the first is held: it leaves the docket.
    hub.append(
        NewEvent::new(committee(), Kind::Note, Address::Speaker, "held")
            .in_thread(first.seq)
            .to(person_addr("carol"))
            .with_meta(meta::CHILD_OFFLINE, json!("north-desk")),
    )
    .await
    .unwrap();
    let page = docket(&server, &committee(), "speaker").await;
    assert_eq!(offline(&page), [(second.seq.0, "north-desk".to_owned())]);

    // The child declares itself again: nothing of it awaits the office.
    hub.set_child_seats(&common::name("north-desk"), Vec::new())
        .unwrap();
    let page = docket(&server, &committee(), "speaker").await;
    assert_eq!(offline(&page), []);
    server.running.shutdown().await.unwrap();
}

// Secretaries.

/// The Speaker's office is in no log, so a stream says it: as it is when
/// the stream opens, before any event, and again when the secretaries of
/// the Speaker are set. Another senator's secretaries are not the office,
/// and say nothing.
#[tokio::test]
async fn a_stream_says_the_office_when_it_opens_and_when_the_desk_changes() {
    let server = TestServer::start_configured(Arc::new(MemoryStore::new()), |c| {
        c.roster.speaker = speaker_id("senator::alice");
        c.roster.agents = vec![seated("claude", "alice"), seated("codex", "bob")];
    })
    .await;
    server.append("before the stream").await;
    let set = |list: &[&str]| SetSecretaries {
        secretaries: list.iter().map(|s| path(s)).collect(),
        acted_by: None,
    };
    let route = |senator: &str| senate_core::api::secretaries_of_path(&path(senator));
    let me = |token: &'static str| {
        let request = server.request(Method::GET, senate_core::api::ME_PATH, token);
        async move { request.send().await.unwrap().json::<Me>().await.unwrap() }
    };

    // Open: the office of now, first, and it is what `/v1/me` says.
    let mut stream = server.stream(BOB, None, None).await;
    match stream.frame().await.unwrap() {
        Frame::Office(office) => {
            let office = office.expect("alice presides");
            assert!(office.desk.is_empty(), "{office:?}");
            assert_eq!(Some(office), me(BOB).await.office);
        }
        other => panic!("the office comes before {other:?}"),
    }
    assert_eq!(stream.event().await.unwrap().body, "before the stream");

    // codex's secretaries are not the office: nothing is said.
    let response = put(&server, BOB, &route("codex"), &set(&["claude"])).await;
    assert_eq!(response.status(), StatusCode::OK);
    // Alice puts claude on her desk: every open stream says so.
    let response = put(&server, ALICE, &route("alice"), &set(&["claude"])).await;
    assert_eq!(response.status(), StatusCode::OK);
    match stream.frame().await.unwrap() {
        Frame::Office(office) => {
            let office = office.expect("alice presides");
            assert_eq!(office.desk, [path("claude")]);
            assert_eq!(Some(office), me(BOB).await.office);
        }
        other => panic!("only the Speaker's desk changed, not {other:?}"),
    }
    // Set to what it is already: nothing more, the log goes on.
    let response = put(&server, ALICE, &route("alice"), &set(&["claude"])).await;
    assert_eq!(response.status(), StatusCode::OK);
    server.append("after the desk changed").await;
    match stream.frame().await.unwrap() {
        Frame::Event(event) => assert_eq!(event.body, "after the desk changed"),
        other => panic!("the office did not change, yet {other:?}"),
    }

    // A stream opened now is told the desk as it is now.
    let mut late = server.stream(ALICE, Some(2), None).await;
    assert_eq!(late.office().await.unwrap().desk, [path("claude")]);
    server.running.shutdown().await.unwrap();
}

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
    // The built-in Speaker reads the children through the `Hub` trait, not
    // the HTTP route: it must see the same list, or a relay to a person's
    // child Senate falls back to the person.
    let hub: Arc<dyn senate_core::Hub> = server.running.hub.clone();
    assert_eq!(hub.children().await.unwrap(), std::slice::from_ref(&child));

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

/// A child declares its seats as its Speaker: the whole set, each time,
/// and the declaration marks it seen. The Speaker reads them through the
/// `Hub` trait; registering again keeps them.
#[tokio::test]
async fn a_child_senate_declares_its_seats_as_its_speaker() {
    let server = try_start(Arc::new(MemoryStore::new()), None, |c| {
        c.roster.agents = vec![seated("claude", "alice")];
    })
    .await
    .unwrap();
    let child = name("north-desk");
    let seats_route = senate_core::api::child_seats_path(&child);
    let seats = |duty: &str| ChildSeats {
        seats: vec![ChildSeat {
            committee: committee(),
            senator: path("responder"),
            alias: Some(path("refund-desk")),
            duty: Some(duty.into()),
            presence: senate_core::continuity::PresenceState::Online,
        }],
    };
    // Not registered yet.
    let response = put(&server, ALICE, &seats_route, &seats("refunds")).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    let route = senate_core::api::child_path(&child);
    put(&server, ALICE, &route, &register("/senate::senator::alice")).await;
    let hub: Arc<dyn senate_core::Hub> = server.running.hub.clone();
    assert!(!hub.children().await.unwrap()[0].is_online(chrono::Utc::now()));

    // Only its Speaker declares them.
    let response = put(&server, BOB, &seats_route, &seats("refunds")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let response = put(&server, ALICE, &seats_route, &seats("refunds")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let declared: ChildSenate = response.json().await.unwrap();
    assert_eq!(declared.seats, seats("refunds").seats);
    assert!(declared.is_online(chrono::Utc::now()));
    let listed = hub.children().await.unwrap();
    assert_eq!(listed, std::slice::from_ref(&declared));

    // The next declaration replaces it; registering again keeps it.
    put(&server, ALICE, &seats_route, &seats("billing")).await;
    put(&server, ALICE, &route, &register("/senate::senator::alice")).await;
    let one: ChildSenate = get(&server, BOB, &route).await;
    assert_eq!(one.seats, seats("billing").seats);
    assert!(one.is_online(chrono::Utc::now()));
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
    let child: Address = "north-desk::ledger::speaker".parse().unwrap();

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
        &meant_for("claude", "north-desk::ledger::senator::codex"),
    )
    .await;
    assert_eq!(meant.to, std::slice::from_ref(&child));
    assert_eq!(
        intended(&meant),
        Some("/senate/north-desk::ledger::senator::codex")
    );
    // A child nobody registered is no address.
    api_error(
        server
            .post_event(
                BOB,
                &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "?")
                    .to("south::ledger::speaker".parse().unwrap()),
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
                    "south::ledger::speaker".parse().unwrap(),
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
