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
    NewEvent, OutputLevel, PickPayload, Principal, SpeakerSeat, StatusPayload, StatusState,
    TriagePayload, TriageVerdict, meta,
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

/// claude, alice's agent, holds the Speaker's seat: with several people
/// listed, a named Speaker is listed under its person.
fn claude_presides(c: &mut ServerConfig) {
    c.roster.speaker = Some(path("claude").into());
    c.roster.agents = vec![seated("claude", "alice")];
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
            .on_behalf_of(Address::Senator(path("codex")))
            .with_meta(meta::TAKEOVER, json!(true)),
    ]
}

#[tokio::test]
async fn a_senate_of_one_person_seats_its_named_speaker_under_her() {
    // Alice alone, and the Speaker named with no owner: it is hers.
    let Built { server } = start(false, |c| {
        c.roster.principals = vec![Principal::new(name("alice"))];
        c.roster.speaker = Some(path("claude").into());
    })
    .await;
    let alice = me(&server, ALICE).await;
    assert_eq!(alice.speaker, Some(SpeakerSeat::Senator(path("claude"))));
    assert_eq!(alice.agents, [path("claude")], "seated from the start");
    assert!(!alice.triaged, "a senator-held seat holds no root back");

    // One agent asks another; alice's presiding session covers as the
    // Speaker for the one that is away.
    let asked = stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Question,
            Address::Senator(path("reviewer")),
            "covered the edge cases?",
        )
        .to(Address::Senator(path("codex"))),
    )
    .await;
    let covered = stored(
        &server,
        ALICE,
        &from_speaker(Kind::Answer, "yes, per its checkpoint")
            .in_thread(asked.seq)
            .to(Address::Senator(path("reviewer")))
            .on_behalf_of(Address::Senator(path("codex")))
            .with_meta(meta::TAKEOVER, json!(true)),
    )
    .await;
    assert_eq!(covered.from, Address::Speaker, "stored as the Speaker's");
    assert!(senate_core::is_takeover(&covered));
    assert_eq!(covered.author().unwrap().to_string(), "[speaker::codex]");

    // Bob is not of this Senate, and holds no seat.
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
    let Built { server } = start(false, claude_presides).await;
    assert_eq!(
        me(&server, BOB).await.speaker,
        Some(SpeakerSeat::Senator(path("claude"))),
        "everyone is told who presides"
    );
    let asked = stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Question,
            Address::Senator(path("claude")),
            "is the migration done?",
        )
        .to(Address::Senator(path("codex"))),
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
                owner: None,
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
    // On behalf of no senator: a path inside a name is not one.
    let mut nested =
        serde_json::to_value(from_speaker(Kind::Answer, "x").in_thread(asked.seq)).unwrap();
    nested["on_behalf_of"] = json!("senator::alice/claude");
    api_error(
        server.post_json(ALICE, &nested).await,
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
                Address::Senator(path("claude")),
                "q",
            )
            .to(Address::Senator(path("codex"))),
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
async fn a_named_speaker_with_the_built_in_session_beside_it_is_one_speaker() {
    // Replaces "the built-in Speaker never sits beside a named one": a
    // Senate has one Speaker, and the built-in session may work its desk
    // beside it; the named Speaker is the one who presides.
    let mut config = ServerConfig::new(roster(), tokens());
    claude_presides(&mut config);
    let running = Server::builder(config, Arc::new(MemoryStore::new()))
        .built_in_speaker(true)
        .start()
        .await
        .unwrap();
    assert_eq!(
        running.hub.speaker_seat(),
        Some(&senate_core::SpeakerSeat::Senator(path("claude")))
    );
    running.shutdown().await.unwrap();

    // A named Speaker nobody runs, among several people, is no Speaker.
    for built_in in [true, false] {
        let mut config = ServerConfig::new(roster(), tokens());
        config.roster.speaker = Some(path("claude").into());
        let refused = Server::builder(config, Arc::new(MemoryStore::new()))
            .built_in_speaker(built_in)
            .start()
            .await;
        assert!(refused.is_err());
    }
}

#[tokio::test]
async fn a_persons_message_for_one_of_several_agents_is_put_to_the_speaker() {
    let Built { server } = start(false, |c| {
        c.roster.speaker = Some(path("claude").into());
        c.roster.agents = vec![seated("claude", "alice"), seated("codex", "alice")];
    })
    .await;
    // Both are on the committee (a heartbeat, and no stream: neither is
    // connected), so `senator::*` could be either.
    for agent in ["claude", "codex"] {
        assert_eq!(
            server.heartbeat(ALICE, agent).await.status(),
            StatusCode::NO_CONTENT
        );
    }
    let wildcard = Address::any_agent();
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
    assert_eq!(
        pick.owner, None,
        "the agents on the committee, not a person's"
    );
    assert_eq!(pick.candidates, [path("claude"), path("codex")]);

    // An agent is refused with the candidates, as everywhere.
    let refused = api_error(
        server
            .post_event(
                BOB,
                &NewEvent::new(
                    committee(),
                    Kind::Question,
                    Address::Senator(path("aider")),
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
    for agent in ["claude", "codex"] {
        assert!(page.seats.iter().any(|s| s.agent == path(agent)));
    }
    assert!(page.seats.iter().all(|s| !s.connected));

    // The Speaker routes it: a relay in the sender's name.
    let relay = stored(
        &server,
        ALICE,
        &from_speaker(Kind::Question, "who owns refunds?")
            .to(Address::Senator(path("codex")))
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
            && item.awaiting == Awaiting::Away { agents: vec![path("codex")] }
    ));
}

#[tokio::test]
async fn a_question_to_the_seat_holder_is_never_for_the_speaker_to_cover() {
    let Built { server } = start(false, |c| {
        c.roster.speaker = Some(path("claude").into());
        c.roster.agents = vec![seated("claude", "alice"), seated("codex", "alice")];
    })
    .await;
    // Nobody is connected: both agents are away.
    let to_holder = stored(
        &server,
        BOB,
        &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "status?")
            .to(Address::Senator(path("claude"))),
    )
    .await;
    let to_other = stored(
        &server,
        BOB,
        &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "status?")
            .to(Address::Senator(path("codex"))),
    )
    .await;
    let page = docket(&server).await;
    // The other agent is covered for; the seat holder's own question is not.
    assert!(
        matches!(
            &page.items[..],
            [item] if item.event.seq == to_other.seq
                && item.awaiting == Awaiting::Away { agents: vec![path("codex")] }
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
            c.roster.agents = vec![seated("claude", "alice"), seated("codex", "alice")];
        })
        .await;
        for agent in ["claude", "codex"] {
            assert_eq!(
                server.heartbeat(ALICE, agent).await.status(),
                StatusCode::NO_CONTENT
            );
        }
        let refused = api_error(
            server
                .post_event(
                    BOB,
                    &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "?")
                        .to(Address::any_agent()),
                )
                .await,
            StatusCode::CONFLICT,
            "ambiguous_recipient",
        )
        .await;
        assert_eq!(refused.candidates, [path("claude"), path("codex")]);
    }
}

#[tokio::test]
async fn the_docket_names_each_agents_latest_checkpoint() {
    let Built { server } = start(false, claude_presides).await;
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
            Address::Senator(path("codex")),
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
            Address::Senator(path("reviewer")),
            "done?",
        )
        .to(Address::Senator(path("codex"))),
    )
    .await;
    let page = docket(&server).await;
    let codex = page
        .seats
        .iter()
        .find(|s| s.agent == path("codex"))
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
            Address::Senator(path("codex")),
            "yes",
        )
        .in_thread(asked.seq)
        .to(Address::Senator(path("reviewer"))),
    )
    .await;
    assert!(docket(&server).await.items.is_empty());
}

#[tokio::test]
async fn the_seat_holder_follows_the_senate_as_the_speaker_and_sits_in_it() {
    let Built { server } = start(false, claude_presides).await;
    // Its stream and heartbeat: the Speaker's connection, and its own.
    let mut stream = server.stream_as(ALICE, Some("claude"), None, None).await;
    assert_eq!(
        server.heartbeat(ALICE, "claude").await.status(),
        StatusCode::NO_CONTENT
    );
    // It sits here, under alice, and writes as itself too.
    let me = me(&server, ALICE).await;
    assert_eq!(me.agents, [path("claude")]);
    stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Note,
            Address::Senator(path("claude")),
            "x",
        ),
    )
    .await;
    // What is said to the Speaker reaches it.
    let asked = stored(
        &server,
        ALICE,
        &from_speaker(Kind::Question, "callbacks covered?").to(Address::Senator(path("reviewer"))),
    )
    .await;
    let answer = stored(
        &server,
        ALICE,
        &NewEvent::new(
            committee(),
            Kind::Answer,
            Address::Senator(path("reviewer")),
            "yes",
        )
        .in_thread(asked.seq)
        .to(Address::Speaker),
    )
    .await;
    let mut seen = Vec::new();
    while let Some(event) = stream.event().await {
        if event.seq >= asked.seq {
            seen.push(event.seq);
        }
        if seen.len() == 2 {
            break;
        }
    }
    assert_eq!(seen, [asked.seq, answer.seq]);
    // Bob cannot follow it as the Speaker.
    assert_eq!(
        server
            .try_stream(BOB, Some("claude"), None, None)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
}

/// The agent that holds the seat sits in the Senate under its person from
/// the start. Its stream and heartbeat are the Speaker's connection and its
/// own, so once it is on the committee `senator::*` counts it like any
/// other agent.
#[tokio::test]
async fn the_seat_holder_is_seated_from_the_start_and_counted_by_the_wildcard() {
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

    let Built { server } = start(false, claude_presides).await;
    assert_eq!(seated(&server).await, ["claude"]);
    // A heartbeat seats it once.
    assert_eq!(
        server.heartbeat(ALICE, "claude").await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(seated(&server).await, ["claude"]);

    // Its sibling next to it, and the wildcard counts both: bob's message
    // for either is put to the Speaker to pick.
    let _codex = server.stream_as(ALICE, Some("codex"), None, None).await;
    assert_eq!(seated(&server).await, ["claude", "codex"]);
    let picked = stored(
        &server,
        BOB,
        &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "?")
            .to(Address::any_agent()),
    )
    .await;
    assert_eq!(picked.to, [Address::Speaker]);
    assert_eq!(
        PickPayload::of(&picked).unwrap().candidates,
        [path("claude"), path("codex")]
    );
}

#[tokio::test]
async fn a_recipient_is_a_flat_name_and_an_agent_need_not_have_connected() {
    let Built { server } = start(true, |_| {}).await;
    // A person, and agents seated or not yet: claude and copilot sit once
    // they first connect.
    assert_eq!(server.act_as(ALICE, "claude").await, StatusCode::OK);
    for to in ["alice", "claude", "copilot"] {
        let response = server
            .post_event(
                BOB,
                &NewEvent::new(committee(), Kind::Question, person_addr("bob"), "?")
                    .to(Address::Senator(path(to))),
            )
            .await;
        assert_eq!(response.status(), StatusCode::OK, "{to}");
    }
    // A path inside a name names nobody.
    for to in ["senator::alice/claude", "senator::alice/claude/reviewer"] {
        let body = json!({"committee": "refund-v2", "kind": "question",
                          "from": "senator::bob", "to": [to], "body": "?"});
        api_error(
            server.post_json(BOB, &body).await,
            StatusCode::BAD_REQUEST,
            "bad_request",
        )
        .await;
    }
}
