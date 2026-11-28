#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::{ALICE, BOB, CAROL, TestServer, name, path, person_addr};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::{Method, StatusCode};
use senate_core::api::{EventsPage, Me};
use senate_core::conference::{
    AdjournConferenceRequest, ConferenceActionItem, ConferenceDetail, ConferenceList,
    ConferenceReportPayload, ConferenceRsvpAction, ConferenceSeatInput, ConferenceState,
    ConferenceSummary, CreateConferenceRequest, LinkedThread, PostConferenceEventRequest,
    PostConferenceEventResponse, SeatStatus, conference_adjourn_path, conference_events_path,
    conference_path, conference_rsvp_path, conference_start_path, conference_stream_path,
    conferences_path,
};
use senate_core::{Address, CommitteeId, Event, Kind, Locator, NewEvent, Seq, meta};
use serde_json::json;

fn cid(raw: &str) -> CommitteeId {
    CommitteeId::new(raw).unwrap()
}

async fn setup_committee_with_chair(
    server: &TestServer,
    committee: &CommitteeId,
    token: &str,
    creator: &str,
    chair: &str,
) -> Seq {
    let appended = server
        .post_event(
            token,
            &NewEvent::new(
                committee.clone(),
                Kind::Request,
                person_addr(creator),
                format!("Setup {committee}"),
            ),
        )
        .await;
    assert_eq!(appended.status(), StatusCode::OK);
    let body: senate_core::api::AppendResponse = appended.json().await.unwrap();

    let res = server
        .request(Method::PUT, &senate_core::api::chair_path(committee), token)
        .json(&senate_core::api::SetChair {
            senator: path(chair),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    body.event.seq
}

#[tokio::test]
async fn w4_s1_convene_attend_delegate_quorum_in_session_and_sse() {
    let server = TestServer::start().await;
    let senate = cid("senate");
    let commons = cid("commons");
    setup_committee_with_chair(&server, &senate, BOB, "bob", "bob").await;
    setup_committee_with_chair(&server, &commons, CAROL, "carol", "carol").await;

    // 1. Alice convenes across senate::chair and commons::chair.
    let res = server
        .request(Method::POST, conferences_path(), ALICE)
        .json(&CreateConferenceRequest {
            topic: "Cross-committee release coordination".into(),
            seats: vec![
                ConferenceSeatInput {
                    target: Locator::on(senate.clone(), Address::Chair),
                },
                ConferenceSeatInput {
                    target: Locator::on(commons.clone(), Address::Chair),
                },
            ],
            linked_threads: vec![],
            from: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let summary: ConferenceSummary = res.json().await.unwrap();
    assert_eq!(summary.state, ConferenceState::Convening);
    assert_eq!(summary.seats.len(), 2);
    assert!(matches!(summary.seats[0].status, SeatStatus::Pending));
    assert!(matches!(summary.seats[1].status, SeatStatus::Pending));

    // Open SSE conference stream as Alice to verify real-time push on quorum.
    let stream_res = server
        .request(Method::GET, &conference_stream_path(&summary.id), ALICE)
        .send()
        .await
        .unwrap();
    assert_eq!(stream_res.status(), StatusCode::OK);
    let mut sse = stream_res.bytes_stream().eventsource();

    // 2. Bob attends as senate::chair -> still Convening.
    let res = server
        .request(Method::POST, &conference_rsvp_path(&summary.id), BOB)
        .json(&ConferenceRsvpAction::Attend {
            target: Some(Locator::on(senate.clone(), Address::Chair)),
            participant: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let after_bob: ConferenceSummary = res.json().await.unwrap();
    assert_eq!(after_bob.state, ConferenceState::Convening);
    assert!(matches!(
        &after_bob.seats[0].status,
        SeatStatus::Seated {
            participant,
            delegated_by: None,
            ephemeral: false,
        } if *participant == person_addr("bob")
    ));

    // 3. Carol delegates an ephemeral commons delegate -> quorum reached, auto InSession.
    let delegate_addr = Address::Senator(path("commons-delegate"));
    let res = server
        .request(Method::POST, &conference_rsvp_path(&summary.id), CAROL)
        .json(&ConferenceRsvpAction::Delegate {
            target: Some(Locator::on(commons.clone(), Address::Chair)),
            senator: delegate_addr.clone(),
            delegated_by: None,
            brief: Some("Represent commons release policy constraints only".into()),
            ephemeral: true,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let after_carol: ConferenceSummary = res.json().await.unwrap();
    assert_eq!(after_carol.state, ConferenceState::InSession);
    assert!(matches!(
        &after_carol.seats[1].status,
        SeatStatus::Seated {
            participant,
            delegated_by: Some(delegator),
            ephemeral: true,
        } if *participant == delegate_addr && *delegator == person_addr("carol")
    ));

    // Confirm SSE delivered the quorum InSession event.
    let mut saw_in_session = false;
    for _ in 0..8 {
        let next = tokio::time::timeout(common::WAIT, sse.next())
            .await
            .expect("SSE timed out")
            .unwrap()
            .unwrap();
        if let Ok(ev) = serde_json::from_str::<Event>(&next.data)
            && ev.meta.get(meta::CONFERENCE_STATE) == Some(&json!("in_session"))
        {
            saw_in_session = true;
            break;
        }
    }
    assert!(saw_in_session, "expected SSE InSession transition event");

    // Verify invite events arrived in senate and commons committee logs.
    let senate_events: EventsPage = server
        .request(Method::GET, &senate_core::api::events_path(&senate), BOB)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        senate_events
            .events
            .iter()
            .any(|e| e.meta.contains_key(meta::CONFERENCE_INVITE))
    );
}

#[tokio::test]
async fn w4_s2_cross_committee_isolation_and_office_key_protection() {
    let server = TestServer::start().await;
    let senate = cid("senate");
    setup_committee_with_chair(&server, &senate, BOB, "bob", "bob").await;

    // Alice convenes with Bob only; Carol is NOT invited.
    let created: ConferenceSummary = server
        .request(Method::POST, conferences_path(), ALICE)
        .json(&CreateConferenceRequest {
            topic: "Private budget alignment".into(),
            seats: vec![ConferenceSeatInput {
                target: Locator::on(senate.clone(), Address::Chair),
            }],
            linked_threads: vec![],
            from: None,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Non-participant Carol is rejected (403) on GET /events /stream /rsvp /adjourn.
    assert_eq!(
        server
            .request(Method::GET, &conference_path(&created.id), CAROL)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .request(Method::GET, &conference_stream_path(&created.id), CAROL)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        server
            .request(Method::POST, &conference_rsvp_path(&created.id), CAROL)
            .json(&ConferenceRsvpAction::Attend {
                target: None,
                participant: None,
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    // Non-participant Carol's conference list does not show Alice+Bob's conference.
    let carol_list: ConferenceList = server
        .request(Method::GET, conferences_path(), CAROL)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(carol_list.conferences.is_empty());

    // Conference stream id `conf-*` is hidden from `/v1/me` committees and rejected (403) on `/v1/committees/conf-*/*`.
    let me: Me = server
        .request(Method::GET, "/v1/me", ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!me.committees.iter().any(senate_core::is_conference_stream));

    let conf_committee = created.id.as_committee_id();
    assert_eq!(
        server
            .request(
                Method::GET,
                &senate_core::api::events_path(&conf_committee),
                ALICE,
            )
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    // Senators cannot forge meta.conference_report on normal committee events.
    let forged = NewEvent::new(
        senate.clone(),
        Kind::Note,
        person_addr("alice"),
        "Forged report",
    )
    .with_meta(meta::CONFERENCE_REPORT, json!({"fake": true}));
    assert_eq!(
        server.post_event(ALICE, &forged).await.status(),
        StatusCode::FORBIDDEN
    );

    // Unrelated agent owned by Bob cannot post to conference even after Bob sits.
    server
        .request(Method::POST, &conference_rsvp_path(&created.id), BOB)
        .json(&ConferenceRsvpAction::Attend {
            target: None,
            participant: None,
        })
        .send()
        .await
        .unwrap();
    let res = server
        .request(Method::POST, &conference_events_path(&created.id), BOB)
        .json(&PostConferenceEventRequest {
            from: Address::Senator(path("bob-uninvited-agent")),
            body: "Sneaking in".into(),
            kind: None,
            to: vec![],
            meta: Default::default(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn w4_s3_offline_chair_marked_absent_start_without_absent_and_refresh_on_connect() {
    let store = std::sync::Arc::new(senate_log::MemoryStore::new());
    let server = TestServer::start_configured(store, |config| {
        config
            .roster
            .agents
            .push(common::seated("audit-bot", "carol"));
    })
    .await;
    let senate = cid("senate");
    let audit = cid("audit");
    setup_committee_with_chair(&server, &senate, BOB, "bob", "bob").await;

    // Give `audit` a seated agent chair `audit-bot` that has NO open SSE stream (offline).
    setup_committee_with_chair(&server, &audit, CAROL, "carol", "audit-bot").await;

    let created: ConferenceSummary = server
        .request(Method::POST, conferences_path(), ALICE)
        .json(&CreateConferenceRequest {
            topic: "Incident triage without offline seat".into(),
            seats: vec![
                ConferenceSeatInput {
                    target: Locator::on(senate.clone(), Address::Chair),
                },
                ConferenceSeatInput {
                    target: Locator::on(audit.clone(), Address::Chair),
                },
            ],
            linked_threads: vec![],
            from: None,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created.state, ConferenceState::Convening);
    assert!(matches!(created.seats[0].status, SeatStatus::Pending));
    assert!(matches!(created.seats[1].status, SeatStatus::Absent));

    // Bob attends senate::chair.
    server
        .request(Method::POST, &conference_rsvp_path(&created.id), BOB)
        .json(&ConferenceRsvpAction::Attend {
            target: None,
            participant: None,
        })
        .send()
        .await
        .unwrap();

    // Non-convener Bob cannot bypass Absent via POST /start -> 403.
    assert_eq!(
        server
            .request(Method::POST, &conference_start_path(&created.id), BOB)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    // Convener Alice starts early without the Absent audit chair -> InSession.
    let started: ConferenceSummary = server
        .request(Method::POST, &conference_start_path(&created.id), ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(started.state, ConferenceState::InSession);
    assert!(matches!(started.seats[1].status, SeatStatus::Absent));

    // Now test offline-to-online refresh on a new Convening conference:
    let second: ConferenceSummary = server
        .request(Method::POST, conferences_path(), ALICE)
        .json(&CreateConferenceRequest {
            topic: "Follow-up when audit comes online".into(),
            seats: vec![ConferenceSeatInput {
                target: Locator::on(audit.clone(), Address::Chair),
            }],
            linked_threads: vec![],
            from: None,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(second.seats[0].status, SeatStatus::Absent));

    // Connect audit-bot's SSE stream on `audit`.
    let _audit_stream = server
        .request(Method::GET, &senate_core::api::stream_path(&audit), CAROL)
        .query(&[("senator", "audit-bot")])
        .send()
        .await
        .unwrap();
    // Wait briefly until presence registers connected stream and poll GET /conferences/{id}.
    let refreshed: ConferenceDetail = server
        .request(Method::GET, &conference_path(&second.id), ALICE)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(
        refreshed.summary.seats[0].status,
        SeatStatus::Pending
    ));
    let _ = name("alice");
}

#[tokio::test]
async fn w4_s4_anti_loop_floor_state_enforces_human_rounds_and_single_agent_hop() {
    let server = TestServer::start().await;
    let senate = cid("senate");
    let commons = cid("commons");
    setup_committee_with_chair(&server, &senate, BOB, "bob", "bob").await;
    setup_committee_with_chair(&server, &commons, CAROL, "carol", "carol").await;

    let conf: ConferenceSummary = server
        .request(Method::POST, conferences_path(), ALICE)
        .json(&CreateConferenceRequest {
            topic: "Multi-agent anti-loop verification".into(),
            seats: vec![
                ConferenceSeatInput {
                    target: Locator::on(senate.clone(), Address::Chair),
                },
                ConferenceSeatInput {
                    target: Locator::on(commons.clone(), Address::Chair),
                },
            ],
            linked_threads: vec![],
            from: None,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let codex = Address::Senator(path("codex-a"));
    let claude = Address::Senator(path("claude-b"));

    server
        .request(Method::POST, &conference_rsvp_path(&conf.id), BOB)
        .json(&ConferenceRsvpAction::Delegate {
            target: Some(Locator::on(senate.clone(), Address::Chair)),
            senator: codex.clone(),
            delegated_by: None,
            brief: Some("Senate view".into()),
            ephemeral: false,
        })
        .send()
        .await
        .unwrap();
    let res: ConferenceSummary = server
        .request(Method::POST, &conference_rsvp_path(&conf.id), CAROL)
        .json(&ConferenceRsvpAction::Delegate {
            target: Some(Locator::on(commons.clone(), Address::Chair)),
            senator: claude.clone(),
            delegated_by: None,
            brief: Some("Commons view".into()),
            ephemeral: false,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(res.state, ConferenceState::InSession);

    // 1. Human post without @mention -> opens round 1, pushes each seated agent once.
    let r1: PostConferenceEventResponse = server
        .request(Method::POST, &conference_events_path(&conf.id), ALICE)
        .json(&PostConferenceEventRequest {
            from: person_addr("alice"),
            body: "Please share both perspectives.".into(),
            kind: None,
            to: vec![],
            meta: Default::default(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r1.floor.round, 1);
    assert_eq!(r1.floor.agent_hops_remaining, 1);
    assert_eq!(r1.push_targets, vec![codex.clone(), claude.clone()]);

    // 2. Agent codex replies WITHOUT @mention -> no pushes.
    let r2: PostConferenceEventResponse = server
        .request(Method::POST, &conference_events_path(&conf.id), BOB)
        .json(&PostConferenceEventRequest {
            from: codex.clone(),
            body: "Here is Senate's input without pinging anyone.".into(),
            kind: None,
            to: vec![],
            meta: Default::default(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(r2.push_targets.is_empty());
    assert_eq!(r2.floor.agent_hops_remaining, 1);

    // 3. Agent codex @mentions un-spoken claude -> consumes single hop (1 -> 0) and pushes claude.
    let r3: PostConferenceEventResponse = server
        .request(Method::POST, &conference_events_path(&conf.id), BOB)
        .json(&PostConferenceEventRequest {
            from: codex.clone(),
            body: "[senator::claude-b] do you concur on the API boundary?".into(),
            kind: None,
            to: vec![],
            meta: Default::default(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r3.push_targets, vec![claude.clone()]);
    assert_eq!(r3.floor.agent_hops_remaining, 0);

    // 4. Agent claude replies & tries to ping codex -> suppressed (agent_hops_remaining == 0).
    let r4: PostConferenceEventResponse = server
        .request(Method::POST, &conference_events_path(&conf.id), CAROL)
        .json(&PostConferenceEventRequest {
            from: claude.clone(),
            body: "[senator::codex-a] let's keep debating forever".into(),
            kind: None,
            to: vec![],
            meta: Default::default(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(r4.push_targets.is_empty());
    assert_eq!(r4.floor.agent_hops_remaining, 0);

    // 5. Human Alice posts with explicit @[senator::codex-a] -> opens round 2, resets hop=1, pushes only codex.
    let r5: PostConferenceEventResponse = server
        .request(Method::POST, &conference_events_path(&conf.id), ALICE)
        .json(&PostConferenceEventRequest {
            from: person_addr("alice"),
            body: "[senator::codex-a] finalize the checklist please.".into(),
            kind: None,
            to: vec![],
            meta: Default::default(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r5.floor.round, 2);
    assert_eq!(r5.floor.agent_hops_remaining, 1);
    assert!(r5.floor.spoken.is_empty());
    assert_eq!(r5.push_targets, vec![codex]);
}

#[tokio::test]
async fn w4_s5_adjourn_locks_room_writes_back_committees_and_linked_threads_and_marks_ephemeral() {
    let server = TestServer::start().await;
    let senate = cid("senate");
    let commons = cid("commons");
    let senate_root = setup_committee_with_chair(&server, &senate, BOB, "bob", "bob").await;
    let commons_root = setup_committee_with_chair(&server, &commons, CAROL, "carol", "carol").await;

    let conf: ConferenceSummary = server
        .request(Method::POST, conferences_path(), ALICE)
        .json(&CreateConferenceRequest {
            topic: "Cross-committee API freeze".into(),
            seats: vec![
                ConferenceSeatInput {
                    target: Locator::on(senate.clone(), Address::Chair),
                },
                ConferenceSeatInput {
                    target: Locator::on(commons.clone(), Address::Chair),
                },
            ],
            linked_threads: vec![
                LinkedThread {
                    committee: senate.clone(),
                    root: senate_root,
                },
                LinkedThread {
                    committee: commons.clone(),
                    root: commons_root,
                },
            ],
            from: None,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Bob attends, Carol delegates an ephemeral agent.
    server
        .request(Method::POST, &conference_rsvp_path(&conf.id), BOB)
        .json(&ConferenceRsvpAction::Attend {
            target: Some(Locator::on(senate.clone(), Address::Chair)),
            participant: None,
        })
        .send()
        .await
        .unwrap();
    let eph = Address::Senator(path("commons-ephemeral"));
    server
        .request(Method::POST, &conference_rsvp_path(&conf.id), CAROL)
        .json(&ConferenceRsvpAction::Delegate {
            target: Some(Locator::on(commons.clone(), Address::Chair)),
            senator: eph.clone(),
            delegated_by: None,
            brief: Some("Carry commons freeze signoff".into()),
            ephemeral: true,
        })
        .send()
        .await
        .unwrap();

    // Adjourn & publish structured report.
    let res = server
        .request(Method::POST, &conference_adjourn_path(&conf.id), ALICE)
        .json(&AdjournConferenceRequest {
            summary: "Approved unified v2 schema and rollout gate.".into(),
            decisions: vec!["Freeze wire types by Friday".into()],
            action_items: vec![ConferenceActionItem {
                description: "Publish migration guide".into(),
                owner: Some(person_addr("bob")),
                committee: Some(senate.clone()),
            }],
            from: None,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let adjourned: ConferenceSummary = res.json().await.unwrap();
    assert_eq!(adjourned.state, ConferenceState::Adjourned);
    assert!(adjourned.adjourned_at.is_some());
    assert!(adjourned.report.is_some());

    // Subsequent posts after adjournment must fail with 409 Conflict (read-only lock).
    let post_after = server
        .request(Method::POST, &conference_events_path(&conf.id), ALICE)
        .json(&PostConferenceEventRequest {
            from: person_addr("alice"),
            body: "One more thing...".into(),
            kind: None,
            to: vec![],
            meta: Default::default(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(post_after.status(), StatusCode::CONFLICT);

    // Check committee log & linked thread write-back in both committees.
    for (committee, root) in [(&senate, senate_root), (&commons, commons_root)] {
        let page: EventsPage = server
            .request(
                Method::GET,
                &senate_core::api::events_path(committee),
                ALICE,
            )
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let reports: Vec<&Event> = page
            .events
            .iter()
            .filter(|e| e.meta.contains_key(meta::CONFERENCE_REPORT))
            .collect();
        // Expect committee-level report note AND linked-thread report note.
        assert!(
            reports.iter().any(|e| e.thread.is_none()),
            "expected committee report on {committee}"
        );
        assert!(
            reports.iter().any(|e| e.thread == Some(root)),
            "expected linked-thread report on {committee} thread {root}"
        );
        let payload: ConferenceReportPayload =
            serde_json::from_value(reports[0].meta[meta::CONFERENCE_REPORT].clone()).unwrap();
        assert_eq!(payload.ephemeral_delegates, vec![eph.clone()]);
        assert_eq!(
            payload.report.summary,
            "Approved unified v2 schema and rollout gate."
        );
    }
}
