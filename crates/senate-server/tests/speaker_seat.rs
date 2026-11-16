//! Who holds the Speaker's seat, and what that lets a person's session
//! write: the rules of `senate_core::SpeakerSeat` as the HTTP API applies
//! them.

#![allow(clippy::unwrap_used)]

mod common;

use std::sync::Arc;

use common::*;
use reqwest::{Method, StatusCode};
use senate_core::Hub as _;
use senate_core::api::{AppendResponse, Me};
use senate_core::{
    Address, Awaiting, CheckpointFiles, CheckpointPayload, CheckpointTrigger, DocketPage, Kind,
    NewEvent, OutputLevel, PickPayload, SpeakerSeat, StatusPayload, StatusState, TriagePayload,
    TriageVerdict, meta,
};
use senate_log::MemoryStore;
use senate_server::{Server, ServerConfig};
use serde_json::json;

struct Built {
    server: TestServer,
}

/// A server whose roster is `roster()` changed by `configure`, with the
/// built-in Speaker attached or not.
async fn start(built_in: bool, configure: impl FnOnce(&mut ServerConfig)) -> Built {
    let mut config = ServerConfig::new(roster(), tokens());
    configure(&mut config);
    let running = Server::builder(config, Arc::new(MemoryStore::new()))
        .built_in_speaker(built_in)
        .start()
        .await
        .unwrap();
    Built {
        server: TestServer {
            running,
            client: reqwest::Client::new(),
        },
    }
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

async fn docket(server: &TestServer) -> DocketPage {
    server
        .request(
            Method::GET,
            &senate_core::api::docket_path(&committee()),
            ALICE,
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn stored(server: &TestServer, token: &str, event: &NewEvent) -> senate_core::Event {
    let response = server.post_event(token, event).await;
    assert_eq!(response.status(), StatusCode::OK, "{event:?}");
    response.json::<AppendResponse>().await.unwrap().event
}

fn from_speaker(kind: Kind, body: &str) -> NewEvent {
    NewEvent::new(committee(), kind, Address::Speaker, body)
}

fn verdict() -> serde_json::Value {
    TriagePayload {
        verdict: TriageVerdict::Agent,
        p_needs_human: 0.0,
        p_agent: 0.0,
        p_unsure: 0.0,
        rule: None,
        reason: None,
    }
    .value()
}

/// Every Speaker-only write a session might try.
fn speaker_only_writes(root: senate_core::Seq) -> Vec<NewEvent> {
    vec![
        from_speaker(Kind::Note, "verdict")
            .in_thread(root)
            .with_meta(meta::TRIAGE, verdict()),
        from_speaker(Kind::Note, "level").with_meta(meta::OUTPUT_LEVEL, json!(OutputLevel::All)),
        from_speaker(Kind::Note, "top three").with_meta(meta::BRIEF_TOP, json!(true)),
        from_speaker(Kind::Answer, "from its checkpoint")
            .in_thread(root)
            .to(person_addr("alice"))
            .on_behalf_of(Address::Senator(path("alice/codex")))
            .with_meta(meta::TAKEOVER, json!(true)),
    ]
}

#[tokio::test]
async fn a_nested_senate_is_presided_over_by_its_presider() {
    let Built { server } = start(false, |c| {
        c.roster.presider = Some(path("alice/claude"));
    })
    .await;
    let alice = me(&server, ALICE).await;
    assert_eq!(
        alice.speaker,
        Some(SpeakerSeat::Senator(path("alice/claude")))
    );
    assert!(!alice.triaged, "a senator-held seat holds no root back");

    // A subagent asks another; alice's presiding session writes as the Speaker.
    let asked = stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Question,
            Address::Senator(path("alice/claude/reviewer")),
            "covered the edge cases?",
        )
        .to(Address::Senator(path("alice/claude/codex"))),
    )
    .await;
    let covered = stored(
        &server,
        ALICE,
        &from_speaker(Kind::Answer, "yes, per its checkpoint")
            .in_thread(asked.seq)
            .to(Address::Senator(path("alice/claude/reviewer")))
            .on_behalf_of(Address::Senator(path("alice/claude/codex")))
            .with_meta(meta::TAKEOVER, json!(true)),
    )
    .await;
    assert_eq!(covered.from, Address::Speaker, "stored as the Speaker's");
    assert!(senate_core::is_takeover(&covered));
    assert_eq!(
        covered.author().unwrap().to_string(),
        "[speaker::alice/claude/codex]"
    );

    // Bob is known here and holds no seat.
    for event in speaker_only_writes(asked.seq) {
        api_error(
            server.post_event(BOB, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }
}

#[tokio::test]
async fn the_seat_holders_person_writes_what_only_the_speaker_writes() {
    let Built { server } = start(false, |c| {
        c.roster.speaker = Some(path("alice/claude"));
    })
    .await;
    assert_eq!(
        me(&server, BOB).await.speaker,
        Some(SpeakerSeat::Senator(path("alice/claude"))),
        "everyone is told who presides"
    );
    let asked = stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Question,
            Address::Senator(path("alice/claude")),
            "is the migration done?",
        )
        .to(Address::Senator(path("alice/codex"))),
    )
    .await;
    for event in speaker_only_writes(asked.seq) {
        // Nobody else, and alice not as herself.
        api_error(
            server.post_event(BOB, &event).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
        let mut as_person = event.clone();
        as_person.from = person_addr("alice");
        assert_eq!(
            server.post_event(ALICE, &as_person).await.status(),
            StatusCode::FORBIDDEN
        );
        let event = stored(&server, ALICE, &event).await;
        assert_eq!(event.from, Address::Speaker);
    }
    // The Speaker's verdicts read as the Speaker's.
    let tasks = server
        .request(
            Method::GET,
            &senate_core::api::threads_path(&committee()),
            ALICE,
        )
        .send()
        .await
        .unwrap()
        .json::<senate_core::api::ThreadsPage>()
        .await
        .unwrap();
    assert_eq!(tasks.threads[0].triage, Some(TriageVerdict::Agent));

    // Not the built-in Speaker's hands, and no decisions.
    for refused in [
        from_speaker(Kind::Note, "run").with_meta(meta::WORK, json!({})),
        from_speaker(Kind::Decision, "we ship"),
        from_speaker(Kind::Note, "x").with_meta(
            meta::PICK,
            PickPayload {
                owner: name("alice"),
                candidates: vec![],
            }
            .value(),
        ),
    ] {
        api_error(
            server.post_event(ALICE, &refused).await,
            StatusCode::FORBIDDEN,
            "forbidden",
        )
        .await;
    }
    // On behalf of someone who could not sit here.
    api_error(
        server
            .post_event(
                ALICE,
                &from_speaker(Kind::Answer, "x")
                    .in_thread(asked.seq)
                    .on_behalf_of(Address::Senator(path("dave/claude"))),
            )
            .await,
        StatusCode::BAD_REQUEST,
        "bad_request",
    )
    .await;
    // Escalating to the person is a status from the Speaker.
    let mut escalation = StatusPayload::new(StatusState::Suspended);
    escalation.escalated_to = Some(name("alice"));
    stored(
        &server,
        ALICE,
        &from_speaker(Kind::Status, "needs alice")
            .in_thread(asked.seq)
            .with_meta(meta::STATUS, escalation.value()),
    )
    .await;
}

#[tokio::test]
async fn nobody_writes_as_the_built_in_speaker_or_where_nobody_presides() {
    for built_in in [true, false] {
        let Built { server } = start(built_in, |_| {}).await;
        let alice = me(&server, ALICE).await;
        assert_eq!(alice.speaker, built_in.then_some(SpeakerSeat::BuiltIn));
        assert_eq!(alice.triaged, built_in);
        let asked = stored(
            &server,
            ALICE,
            &NewEvent::new(
                committee(),
                Kind::Question,
                Address::Senator(path("alice/claude")),
                "q",
            )
            .to(Address::Senator(path("alice/codex"))),
        )
        .await;
        for event in speaker_only_writes(asked.seq) {
            for token in [ALICE, BOB] {
                api_error(
                    server.post_event(token, &event).await,
                    StatusCode::FORBIDDEN,
                    "forbidden",
                )
                .await;
            }
        }
    }
}

#[tokio::test]
async fn the_built_in_speaker_never_sits_in_a_nested_senate_nor_beside_a_named_one() {
    for configure in [
        (|c: &mut ServerConfig| c.roster.presider = Some(path("alice/claude")))
            as fn(&mut ServerConfig),
        |c: &mut ServerConfig| c.roster.speaker = Some(path("alice/claude")),
    ] {
        let mut config = ServerConfig::new(roster(), tokens());
        configure(&mut config);
        let refused = Server::builder(config, Arc::new(MemoryStore::new()))
            .built_in_speaker(true)
            .start()
            .await;
        assert!(refused.is_err());
    }
}

#[tokio::test]
async fn a_persons_message_for_one_of_several_agents_is_put_to_the_speaker() {
    let Built { server } = start(false, |c| {
        c.roster.speaker = Some(path("alice/claude"));
        c.roster.agents = vec![path("alice/claude"), path("alice/codex")];
    })
    .await;
    let wildcard = Address::AgentOf(name("alice"));
    let picked = stored(
        &server,
        BOB,
        &NewEvent::new(
            committee(),
            Kind::Question,
            person_addr("bob"),
            "who owns refunds?",
        )
        .to(wildcard.clone()),
    )
    .await;
    assert_eq!(picked.to, [Address::Speaker]);
    let pick = PickPayload::of(&picked).unwrap();
    assert_eq!(pick.owner, name("alice"));
    assert_eq!(pick.candidates, [path("alice/claude"), path("alice/codex")]);

    // An agent is refused with the candidates, as everywhere.
    let refused = api_error(
        server
            .post_event(
                BOB,
                &NewEvent::new(
                    committee(),
                    Kind::Question,
                    Address::Senator(path("bob/claude")),
                    "who owns refunds?",
                )
                .to(wildcard.clone()),
            )
            .await,
        StatusCode::CONFLICT,
        "ambiguous_recipient",
    )
    .await;
    assert_eq!(refused.candidates.len(), 2);

    // The docket has it, with the facts to pick by.
    let page = docket(&server).await;
    assert_eq!(page.items.len(), 1);
    assert!(matches!(page.items[0].awaiting, Awaiting::Pick { .. }));
    for agent in ["alice/claude", "alice/codex"] {
        assert!(page.seats.iter().any(|s| s.agent == path(agent)));
    }
    assert!(page.seats.iter().all(|s| !s.connected));

    // The Speaker routes it: a relay in the sender's name.
    let relay = stored(
        &server,
        ALICE,
        &from_speaker(Kind::Question, "who owns refunds?")
            .to(Address::Senator(path("alice/codex")))
            .on_behalf_of(person_addr("bob"))
            .with_meta(meta::RELAYED_FROM, json!(picked.seq.0)),
    )
    .await;
    assert_eq!(relay.author().unwrap().to_string(), "[speaker::bob]");
    let page = docket(&server).await;
    assert!(
        page.items
            .iter()
            .all(|i| !matches!(i.awaiting, Awaiting::Pick { .. })),
        "routed"
    );
    // The relayed question is to an agent with no connection: cover it.
    assert!(matches!(
        &page.items[..],
        [item] if item.event.seq == relay.seq
            && item.awaiting == Awaiting::Away { agents: vec![path("alice/codex")] }
    ));
}

#[tokio::test]
async fn a_question_to_the_seat_holder_is_never_for_the_speaker_to_cover() {
    let Built { server } = start(false, |c| {
        c.roster.speaker = Some(path("alice/claude"));
        c.roster.agents = vec![path("alice/claude"), path("alice/codex")];
    })
    .await;
    // Nobody is connected: both agents are away.
    let to_holder = stored(
        &server,
        BOB,
        &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "status?")
            .to(Address::Senator(path("alice/claude"))),
    )
    .await;
    let to_other = stored(
        &server,
        BOB,
        &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "status?")
            .to(Address::Senator(path("alice/codex"))),
    )
    .await;
    let page = docket(&server).await;
    // The other agent is covered for; the seat holder's own question is not.
    assert!(
        matches!(
            &page.items[..],
            [item] if item.event.seq == to_other.seq
                && item.awaiting == Awaiting::Away { agents: vec![path("alice/codex")] }
        ),
        "{:?}",
        page.items
    );
    assert!(page.items.iter().all(|i| i.event.seq != to_holder.seq));
}

#[tokio::test]
async fn without_a_senator_in_the_seat_a_wildcard_is_still_refused() {
    for built_in in [true, false] {
        let Built { server } = start(built_in, |c| {
            c.roster.agents = vec![path("alice/claude"), path("alice/codex")];
        })
        .await;
        api_error(
            server
                .post_event(
                    BOB,
                    &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "?")
                        .to(Address::AgentOf(name("alice"))),
                )
                .await,
            StatusCode::CONFLICT,
            "ambiguous_recipient",
        )
        .await;
    }
}

#[tokio::test]
async fn the_docket_names_each_agents_latest_checkpoint() {
    let Built { server } = start(false, |c| {
        c.roster.presider = Some(path("alice/claude"));
    })
    .await;
    let payload = CheckpointPayload {
        branch: "feature/refund-v2/alice".into(),
        commit: "a41f9c2e00000000000000000000000000000000".into(),
        pushed: false,
        remote_url: None,
        files: CheckpointFiles {
            progress: Some("callbacks done".into()),
            ..CheckpointFiles::default()
        },
        session_transcript: false,
        trigger: CheckpointTrigger::Tool,
    };
    stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Checkpoint,
            Address::Senator(path("alice/claude/codex")),
            "callbacks done",
        )
        .with_meta(meta::CHECKPOINT, payload.value()),
    )
    .await;
    let asked = stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Question,
            Address::Senator(path("alice/claude/reviewer")),
            "done?",
        )
        .to(Address::Senator(path("alice/claude/codex"))),
    )
    .await;
    let page = docket(&server).await;
    let codex = page
        .seats
        .iter()
        .find(|s| s.agent == path("alice/claude/codex"))
        .unwrap();
    assert_eq!(codex.checkpoint.as_ref().unwrap().summary, "callbacks done");
    assert_eq!(codex.open_threads, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].event.seq, asked.seq);
    // An answer takes it off.
    stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Answer,
            Address::Senator(path("alice/claude/codex")),
            "yes",
        )
        .in_thread(asked.seq)
        .to(Address::Senator(path("alice/claude/reviewer"))),
    )
    .await;
    assert!(docket(&server).await.items.is_empty());
}

#[tokio::test]
async fn the_presiding_agent_follows_the_senate_it_presides_over_without_sitting_in_it() {
    let Built { server } = start(false, |c| {
        c.roster.presider = Some(path("alice/claude"));
    })
    .await;
    // Its stream and heartbeat, as the Speaker's connection.
    let mut stream = server
        .stream_as(ALICE, Some("alice/claude"), None, None)
        .await;
    let beat = server
        .request(
            Method::POST,
            &senate_core::api::heartbeat_path(&committee()),
            ALICE,
        )
        .json(&senate_core::api::Heartbeat {
            agent: path("alice/claude"),
            sessions: 1,
            cursor: senate_core::Seq::ZERO,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(beat.status(), StatusCode::NO_CONTENT);
    // It is not seated, and does not write as itself here.
    let me = me(&server, ALICE).await;
    assert!(me.agents.is_empty(), "{:?}", me.agents);
    api_error(
        server
            .post_event(
                ALICE,
                &NewEvent::new(
                    committee(),
                    Kind::Note,
                    Address::Senator(path("alice/claude")),
                    "x",
                ),
            )
            .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    )
    .await;
    // What is said to the Speaker reaches it.
    let asked = stored(
        &server,
        ALICE,
        &from_speaker(Kind::Question, "callbacks covered?")
            .to(Address::Senator(path("alice/claude/reviewer"))),
    )
    .await;
    let answer = stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Answer,
            Address::Senator(path("alice/claude/reviewer")),
            "yes",
        )
        .in_thread(asked.seq)
        .to(Address::Speaker),
    )
    .await;
    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.push(stream.event().await.unwrap().seq);
    }
    assert_eq!(seen, [asked.seq, answer.seq]);
    // Bob cannot follow it as the Speaker.
    assert_eq!(
        server
            .try_stream(BOB, Some("alice/claude"), None, None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

/// The agent that holds the seat of a top Senate also sits in it, directly
/// under its person. Its stream and heartbeat are the Speaker's connection
/// (no seat needed) and its own (a seat), so contact as that agent seats it
/// like any other agent: `senator::alice/*` must then count it.
#[tokio::test]
async fn the_seat_holder_of_the_top_senate_is_seated_by_its_first_contact_too() {
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

    // A heartbeat.
    let Built { server } = start(false, |c| c.roster.speaker = Some(path("alice/claude"))).await;
    assert!(seated(&server).await.is_empty());
    let beat = server
        .request(
            Method::POST,
            &senate_core::api::heartbeat_path(&committee()),
            ALICE,
        )
        .json(&senate_core::api::Heartbeat {
            agent: path("alice/claude"),
            sessions: 1,
            cursor: senate_core::Seq::ZERO,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(beat.status(), StatusCode::NO_CONTENT);
    assert_eq!(seated(&server).await, ["alice/claude"]);

    // A stream.
    let Built { server } = start(false, |c| c.roster.speaker = Some(path("alice/claude"))).await;
    let _stream = server
        .stream_as(ALICE, Some("alice/claude"), None, None)
        .await;
    assert_eq!(seated(&server).await, ["alice/claude"]);

    // Its sibling next to it, and the wildcard counts both.
    let _codex = server
        .stream_as(ALICE, Some("alice/codex"), None, None)
        .await;
    assert_eq!(seated(&server).await, ["alice/claude", "alice/codex"]);
}

#[tokio::test]
async fn an_agent_of_another_level_is_not_reached_from_here() {
    let Built { server } = start(true, |_| {}).await;
    for (to, status) in [
        ("alice/claude/reviewer", StatusCode::BAD_REQUEST),
        ("dave/claude", StatusCode::BAD_REQUEST),
        ("alice/claude", StatusCode::OK),
        ("alice", StatusCode::OK),
    ] {
        let response = server
            .post_event(
                BOB,
                &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "?")
                    .to(Address::Senator(path(to))),
            )
            .await;
        assert_eq!(response.status(), status, "{to}");
    }
}
