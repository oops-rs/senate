#![allow(clippy::unwrap_used)]
use crate::common;

use common::{Harness, path};
use senate_bridge::{
    agent::AgentSource,
    ipc::{self, DeskDecision, DeskItemDetail, DeskOutcome, Request},
    session::Runtime,
    state::SessionUpsert,
};
use senate_core::{Address, CommitteeId, EventId, Kind, NewEvent, meta};
use serde_json::json;
use std::{path::Path, time::Duration};

const WAIT: Duration = Duration::from_secs(6);
const SOCKET_ENV: &str = "SENATE_DESK_AGENT_TEST_SOCKET";
const GO_ENV: &str = "SENATE_DESK_AGENT_TEST_READY";

async fn call<T: serde::de::DeserializeOwned>(
    socket: &Path,
    request: &Request,
) -> Result<T, ipc::ClientError> {
    ipc::request(socket, request, WAIT, WAIT).await
}

#[tokio::test]
async fn the_original_request_gets_one_person_attributed_delivery_receipt_on_retry() {
    let mut h = Harness::new().await;
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(Some("senator::alice".parse().unwrap()))
        .unwrap();
    let make_item = || {
        h.server.append(
            NewEvent::new(
                common::committee(),
                Kind::Request,
                Address::senior(&common::name("bob")),
                "A full request",
            )
            .to(Address::Speaker)
            .with_meta(meta::INTENDED, json!("other::senator::alice")),
        )
    };
    let original = make_item();
    let rejected = make_item();
    h.start().await;
    let socket = h.socket();
    let detail: DeskItemDetail = call(
        &socket,
        &Request::DeskItem {
            committee: original.committee.clone(),
            seq: original.seq,
        },
    )
    .await
    .unwrap();
    assert_eq!(detail.item.body, "A full request");
    assert_eq!(detail.events.len(), 1);
    assert_eq!(detail.destination.as_deref(), Some("other::senator::alice"));
    assert_eq!(detail.blocked, None);

    let stale = Request::DeskDecide {
        committee: original.committee.clone(),
        seq: original.seq,
        item_id: EventId::new(),
        id: EventId::new(),
        decision: DeskDecision::Forward,
    };
    assert!(
        call::<DeskOutcome>(&socket, &stale)
            .await
            .unwrap_err()
            .to_string()
            .contains("has changed")
    );
    assert!(h.server.events("other").is_empty());

    let forward = Request::DeskDecide {
        committee: original.committee.clone(),
        seq: original.seq,
        item_id: original.id.clone(),
        id: EventId::new(),
        decision: DeskDecision::Forward,
    };
    // The destination is already durable when the audit append fails.
    // Retrying the same decision must reuse its original delivery ID.
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .reject_append_containing = Some("approved forwarding".into());
    let failed_audit = call::<DeskOutcome>(&socket, &forward)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        failed_audit.contains("Delivered to other::senator::alice #1"),
        "{failed_audit}"
    );
    assert_eq!(h.server.events("other").len(), 1);
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .reject_append_containing = None;
    let receipt: DeskOutcome = call(&socket, &forward).await.unwrap();
    let retry: DeskOutcome = call(&socket, &forward).await.unwrap();
    assert_eq!(retry, receipt);
    let DeskOutcome::Delivered { seq, committee, .. } = receipt else {
        panic!("delivery receipt expected")
    };
    let destination = h.server.events(committee.as_str());
    assert_eq!(destination.len(), 1);
    assert_eq!(destination[0].seq, seq);
    assert_eq!(
        senate_core::Origin::of(&destination[0]).unwrap().thread,
        original.seq
    );
    assert_eq!(destination[0].meta[meta::DESK_PERSON], "alice");
    let audit: Vec<_> = h
        .server
        .events(original.committee.as_str())
        .into_iter()
        .filter(|event| event.thread == Some(original.seq))
        .collect();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].meta[meta::ACTED_BY], "alice");
    assert_eq!(audit[0].meta[meta::DESK_OUTCOME]["seq"], seq.0);

    let refused = Request::DeskDecide {
        committee: original.committee.clone(),
        seq: rejected.seq,
        item_id: rejected.id.clone(),
        id: EventId::new(),
        decision: DeskDecision::Refuse {
            reason: "Outside policy".into(),
        },
    };
    let receipt: DeskOutcome = call(&socket, &refused).await.unwrap();
    assert!(matches!(receipt, DeskOutcome::Refused { .. }));
    assert_eq!(
        call::<DeskOutcome>(&socket, &refused).await.unwrap(),
        receipt
    );
    assert_eq!(h.server.events("other").len(), 1);
    h.stop().await;
}

#[tokio::test]
async fn an_agent_peer_is_refused_even_when_its_person_is_speaker() {
    if let (Some(socket), Some(ready)) = (std::env::var_os(SOCKET_ENV), std::env::var_os(GO_ENV)) {
        tokio::time::timeout(WAIT, async {
            while !Path::new(&ready).exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        for decision in [
            DeskDecision::Forward,
            DeskDecision::Answer { body: "x".into() },
            DeskDecision::Close { reason: None },
        ] {
            let err = call::<DeskOutcome>(
                Path::new(&socket),
                &Request::DeskDecide {
                    committee: CommitteeId::new("refund-v2").unwrap(),
                    seq: senate_core::Seq(1),
                    item_id: EventId::new(),
                    id: EventId::new(),
                    decision,
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(err, ipc::ClientError::Remote(error)
                if error.code == ipc::codes::NOT_A_PERSON && error.message.contains("senator::claude")));
        }
        return;
    }
    let mut h = Harness::new().await;
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(Some("senator::alice".parse().unwrap()))
        .unwrap();
    h.start().await;
    let ready = h.dir.path().join("peer-ready");
    let mut agent = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "desk_decision::an_agent_peer_is_refused_even_when_its_person_is_speaker",
            "--nocapture",
        ])
        .env(SOCKET_ENV, h.socket())
        .env(GO_ENV, &ready)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    h.store()
        .upsert_session(&SessionUpsert {
            session_id: "desk-guard-peer".into(),
            cwd: h.dir.path().into(),
            git_root: None,
            branch: None,
            inferred_committee: None,
            socket: None,
            token: None,
            claude_pid: agent.id(),
            transcript_path: None,
            ancestor_pids: Vec::new(),
            runtime: Runtime::Claude,
            agent: path("claude"),
            agent_source: AgentSource::Session,
        })
        .unwrap();
    std::fs::write(ready, "").unwrap();
    assert!(
        tokio::time::timeout(WAIT, agent.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    h.stop().await;
}

#[tokio::test]
async fn hosted_session_caller_is_refused_for_speaker_and_prompt_free_requests() {
    use senate_core::alias::AliasName;
    use senate_core::{CommitteeSeatAction, Seq};
    use senate_observe::{AdminRequest, CreateGrant, Preview, Scope};

    let mut h = Harness::new().await;
    h.start().await;
    let mut agent = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let agent_pid = agent.id();
    h.store()
        .upsert_session(&SessionUpsert {
            session_id: "hosted-guard-peer".into(),
            cwd: h.dir.path().into(),
            git_root: None,
            branch: None,
            inferred_committee: None,
            socket: None,
            token: None,
            claude_pid: agent_pid,
            transcript_path: None,
            ancestor_pids: Vec::new(),
            runtime: Runtime::Claude,
            agent: path("claude-hosted"),
            agent_source: AgentSource::Session,
        })
        .unwrap();

    let committee = CommitteeId::new("refund-v2").unwrap();
    let senator = path("codex-1");
    let alias = path("reviewer");
    let bare = AliasName::bare(senator.clone());
    let person = ipc::SessionSelector::default();
    let hosted_session = ipc::SessionSelector {
        session_id: Some("hosted-guard-peer".into()),
        claude_pid: None,
    };

    let requests: Vec<Request> = vec![
        // Speaker-level requests
        Request::DeskDecide {
            committee: committee.clone(),
            seq: Seq(1),
            item_id: EventId::new(),
            id: EventId::new(),
            decision: DeskDecision::Forward,
        },
        Request::Forward {
            session: person.clone(),
            committee: committee.clone(),
            seq: Seq(1),
            authorize: false,
            note: None,
            id: None,
        },
        Request::Refuse {
            session: person.clone(),
            committee: committee.clone(),
            seq: Seq(1),
            reason: "no".into(),
            id: None,
        },
        Request::Desk {
            set: Some(vec![senator.clone()]),
        },
        Request::Secretaries {
            session: person.clone(),
            action: ipc::SecretaryOp::Add,
            for_: None,
            secretary: Some(senator.clone()),
            expires_at: None,
        },
        Request::SetSecretarySession {
            target_session: Some("s".into()),
            senator: None,
            principal: None,
            appointed: true,
        },
        Request::EnsureSecretary,
        Request::SetChair {
            session: person.clone(),
            committee: Some(committee.clone()),
            senator: senator.clone(),
        },
        Request::Spawn {
            attempt_id: None,
            session: person.clone(),
            runtime: Some(ipc::HostedRuntime::Claude),
            profile: None,
            committee: committee.clone(),
            chair: true,
            secretary: false,
            alias: None,
            cwd: h.dir.path().into(),
            prompt: None,
            model: ipc::HostedModel::default(),
        },
        Request::Spawn {
            attempt_id: None,
            session: hosted_session,
            runtime: Some(ipc::HostedRuntime::Claude),
            profile: None,
            committee: committee.clone(),
            chair: false,
            secretary: true,
            alias: None,
            cwd: h.dir.path().into(),
            prompt: None,
            model: ipc::HostedModel::default(),
        },
        Request::DeskOpenCommittee {
            session: person.clone(),
            committee: committee.clone(),
            cwd: h.dir.path().into(),
            runtime: None,
            profile: None,
            alias: None,
            prompt: None,
        },
        Request::SpeakerAlias {
            session: person.clone(),
            committee: committee.clone(),
            senator: senator.clone(),
            alias: alias.clone(),
        },
        Request::SpeakerUnseat {
            session: person.clone(),
            committee: committee.clone(),
            senator: senator.clone(),
            id: None,
        },
        Request::ArchiveCommittee {
            committee: committee.clone(),
            archived: true,
            id: None,
        },
        Request::DeskWorker {
            command: senate_core::worker::Admin::Issue {
                worker: "bot".parse().unwrap(),
                committees: [committee.clone()].into_iter().collect(),
                desk_inbox: None,
            },
        },
        Request::DeskWorker {
            command: senate_core::worker::Admin::Rotate { id: EventId::new() },
        },
        Request::AuthorizeNativeAuthority {
            session_id: "native-1".into(),
            approval: EventId::new(),
        },
        Request::Approve {
            id: "approval-1".into(),
            decision: senate_bridge::approvals::Decision::Allow,
        },
        Request::ApproveSession {
            senator: bare.clone(),
            decision: senate_bridge::approvals::Decision::Allow,
        },
        Request::RemoteControl {
            command: senate_bridge::remote::Control::PrepareEnable {
                listen: "127.0.0.1:9000".parse().unwrap(),
            },
        },
        Request::RemoteControl {
            command: senate_bridge::remote::Control::Pair {
                scopes: senate_bridge::remote::Scopes {
                    watch: true,
                    attach: false,
                },
                valid_secs: 60,
            },
        },
        Request::RemoteControl {
            command: senate_bridge::remote::Control::PrepareEnroll,
        },
        Request::ObserveAdmin {
            command: AdminRequest::Create {
                grant: CreateGrant {
                    name: "scene".into(),
                    committees: Some(vec![committee.clone()]),
                    all_committees: false,
                    scope: Scope::Metadata,
                    lines: None,
                    origins: Vec::new(),
                    topology: false,
                    expires_in_days: None,
                },
            },
        },
        Request::ObserveAdmin {
            command: AdminRequest::Publish {
                committee: committee.clone(),
                seq: Seq(1),
                reviewed: Preview {
                    committee: committee.clone(),
                    event_id: EventId::new(),
                    line: "line".into(),
                },
            },
        },
        // Newly prompt-free requests
        Request::Attach {
            senator: bare.clone(),
        },
        Request::OpenViewer {
            host: EventId::new(),
            writable: false,
        },
        Request::OpenViewer {
            host: EventId::new(),
            writable: true,
        },
        Request::Stop {
            session: person.clone(),
            senator: bare.clone(),
            force: false,
        },
        Request::Restore {
            session: person.clone(),
            senator: bare.clone(),
            model: ipc::HostedModel::default(),
        },
        Request::Replace {
            senator: bare.clone(),
            prompt: None,
            model: ipc::HostedModel::default(),
        },
        Request::Pause {
            senator: bare.clone(),
            force: false,
            timeout_secs: None,
        },
        Request::Resume {
            senator: bare.clone(),
        },
        Request::RestartHost {
            senator: bare.clone(),
            host: EventId::new(),
            force: false,
        },
        Request::PruneHosts,
        Request::SessionForget {
            session_id: "s-1".into(),
        },
        Request::RebindHost {
            host: EventId::new().to_string(),
            to: bare.clone(),
        },
        Request::SetAlias {
            senator: bare.clone(),
            alias: alias.clone(),
            committee: Some(committee.clone()),
        },
        Request::RemoveAlias {
            primary: senator.clone(),
            alias: alias.clone(),
            committee: Some(committee.clone()),
        },
        Request::SetCommitteeSeat {
            committee: committee.clone(),
            request: senate_core::api::SetCommitteeSeat {
                senator: senator.clone(),
                action: CommitteeSeatAction::Unseat,
                id: Default::default(),
                acted_by: None,
            },
        },
        Request::SetCard {
            session: person.clone(),
            target: ipc::CardTarget::Own,
            document: "{}".into(),
        },
        Request::DeskWorker {
            command: senate_core::worker::Admin::Revoke { id: EventId::new() },
        },
        Request::ObserveAdmin {
            command: AdminRequest::Revoke {
                id: "grant-1".into(),
            },
        },
        Request::ObserveAdmin {
            command: AdminRequest::Withdraw {
                committee: committee.clone(),
                seq: Seq(1),
            },
        },
        Request::Secretaries {
            session: person,
            action: ipc::SecretaryOp::Remove,
            for_: None,
            secretary: Some(senator),
            expires_at: None,
        },
    ];

    let daemon = h.daemon.as_ref().unwrap();
    for request in &requests {
        let error = daemon
            .check_caller_for_test(agent_pid, request)
            .await
            .expect_err(&format!("hosted session must be refused for {request:?}"));
        assert_eq!(error.code, ipc::codes::NOT_A_PERSON, "{request:?}");
        assert!(
            error.message.contains("senator::claude-hosted"),
            "refusal should name the hosted session for {request:?}: {}",
            error.message
        );
        let lower = error.message.to_ascii_lowercase();
        assert!(
            lower.contains("mcp") && lower.contains("ask the person"),
            "refusal must direct the agent to MCP tools or the person for {request:?}: {}",
            error.message
        );
    }

    let _ = agent.kill().await;
    let _ = agent.wait().await;
    h.stop().await;
}

/// alice's daemon with alice in the Speaker's seat (unless `speaker` says
/// otherwise), and roots put on her desk, each waiting as `awaiting` says.
async fn desk(
    speaker: Option<&str>,
    roots: Vec<(NewEvent, Option<senate_core::Awaiting>)>,
) -> (Harness, Vec<senate_core::Event>) {
    let mut h = Harness::new().await;
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(speaker.map(|s| s.parse().unwrap()))
        .unwrap();
    let mut events = Vec::new();
    for (root, awaiting) in roots {
        let event = h.server.append(root);
        if let Some(awaiting) = awaiting {
            h.server
                .state
                .inner
                .lock()
                .unwrap()
                .desk_awaiting
                .insert((event.committee.to_string(), event.seq), awaiting);
        }
        events.push(event);
    }
    h.start().await;
    (h, events)
}

fn question(to: Address, body: &str) -> NewEvent {
    NewEvent::new(
        common::committee(),
        Kind::Question,
        Address::senior(&common::name("bob")),
        body,
    )
    .to(to)
}

fn decide(item: &senate_core::Event, id: &EventId, decision: DeskDecision) -> Request {
    Request::DeskDecide {
        committee: item.committee.clone(),
        seq: item.seq,
        item_id: item.id.clone(),
        id: id.clone(),
        decision,
    }
}

async fn detail(socket: &Path, item: &senate_core::Event) -> DeskItemDetail {
    call(
        socket,
        &Request::DeskItem {
            committee: item.committee.clone(),
            seq: item.seq,
        },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn the_speaker_answers_each_kind_of_waiting_item_in_its_thread_once() {
    use ipc::DeskAction::{Answer, Close, Forward, ForwardAndAuthorize, Refuse};
    use senate_core::Awaiting;
    let (mut h, items) = desk(
        Some("senator::alice"),
        vec![
            (
                question(Address::Speaker, "Which quarter?"),
                Some(Awaiting::Addressed),
            ),
            (
                question(Address::senator(&path("codex")), "Is the ledger done?"),
                Some(Awaiting::Away {
                    agents: vec![path("codex")],
                }),
            ),
            (
                question(Address::Speaker, "Who looks at refunds?"),
                Some(Awaiting::Pick {
                    owner: None,
                    candidates: vec![path("claude"), path("codex")],
                }),
            ),
            (
                NewEvent::new(
                    common::committee(),
                    Kind::Request,
                    Address::senior(&common::name("bob")),
                    "A request for elsewhere",
                )
                .to(Address::Speaker)
                .with_meta(meta::INTENDED, json!("other::senator::alice")),
                None,
            ),
        ],
    )
    .await;
    let socket = h.socket();
    let [addressed, away, pick, forwardable] = items.as_slice() else {
        unreachable!()
    };
    for item in [addressed, away, pick] {
        let d = detail(&socket, item).await;
        assert_eq!(d.actions, [Answer, Close], "{}", item.body);
        assert_eq!(d.waits_on, None);
    }
    assert_eq!(
        detail(&socket, forwardable).await.actions,
        [ForwardAndAuthorize, Forward, Refuse, Answer, Close]
    );

    // An empty answer is refused and posts nothing.
    let empty = call::<DeskOutcome>(
        &socket,
        &decide(
            addressed,
            &EventId::new(),
            DeskDecision::Answer { body: "  ".into() },
        ),
    )
    .await
    .unwrap_err();
    assert!(empty.to_string().contains("empty"), "{empty}");

    for (item, body) in [
        (addressed, "From Q3."),
        (away, "Yes, merged."),
        (pick, "Me."),
    ] {
        let id = EventId::new();
        let answer = decide(item, &id, DeskDecision::Answer { body: body.into() });
        let receipt: DeskOutcome = call(&socket, &answer).await.unwrap();
        let DeskOutcome::Answered { seq, .. } = receipt.clone() else {
            panic!("an answer receipt expected: {receipt:?}")
        };
        // A retry with the same id is the same answer, posted once.
        assert_eq!(
            call::<DeskOutcome>(&socket, &answer).await.unwrap(),
            receipt
        );
        let thread: Vec<_> = h
            .server
            .events(item.committee.as_str())
            .into_iter()
            .filter(|e| e.thread == Some(item.seq))
            .collect();
        assert_eq!(thread.len(), 1);
        let posted = &thread[0];
        assert_eq!(posted.seq, seq);
        assert_eq!(posted.kind, Kind::Answer);
        assert_eq!(posted.from, Address::Speaker);
        assert_eq!(
            posted.to,
            [Address::senior(&common::name("bob"))],
            "the asker has it"
        );
        assert_eq!(posted.body, body);
        assert_eq!(posted.meta[meta::DESK_OUTCOME]["kind"], "answered");
        assert_eq!(posted.meta[meta::DESK_OUTCOME]["by"], "alice");
        assert_eq!(posted.meta[meta::ACTED_BY], "alice");

        // Settled: nothing more to do here, and a second answer (another
        // window, or a secretary at the desk) cannot land as well.
        let d = detail(&socket, item).await;
        assert!(d.actions.is_empty());
        assert!(d.waits_on.unwrap().contains("no longer on the desk"));
        let again = call::<DeskOutcome>(
            &socket,
            &decide(
                item,
                &EventId::new(),
                DeskDecision::Answer {
                    body: "Twice".into(),
                },
            ),
        )
        .await
        .unwrap_err();
        assert!(again.to_string().contains("settled already"), "{again}");
    }
    h.stop().await;
}

#[tokio::test]
async fn closing_takes_an_item_off_the_desk_with_its_reason() {
    let (mut h, items) = desk(
        Some("senator::alice"),
        vec![
            (question(Address::Speaker, "Still needed?"), None),
            (question(Address::Speaker, "And this?"), None),
        ],
    )
    .await;
    for item in &items {
        h.server.state.inner.lock().unwrap().desk_awaiting.insert(
            (item.committee.to_string(), item.seq),
            senate_core::Awaiting::Addressed,
        );
    }
    let socket = h.socket();
    let docket = || async {
        let reply: ipc::DocketReply = call(
            &socket,
            &Request::Docket {
                session: Default::default(),
                committee: Some(common::committee()),
            },
        )
        .await
        .unwrap();
        reply
            .senates
            .iter()
            .flat_map(|s| &s.pages)
            .flat_map(|p| &p.items)
            .map(|i| i.event.seq)
            .collect::<Vec<_>>()
    };
    assert_eq!(docket().await, [items[0].seq, items[1].seq]);

    let id = EventId::new();
    let close = decide(
        &items[0],
        &id,
        DeskDecision::Close {
            reason: Some(" Settled in chat. ".into()),
        },
    );
    let receipt: DeskOutcome = call(&socket, &close).await.unwrap();
    assert!(
        matches!(&receipt, DeskOutcome::Closed { reason: Some(r), .. } if r == "Settled in chat."),
        "{receipt:?}"
    );
    assert_eq!(call::<DeskOutcome>(&socket, &close).await.unwrap(), receipt);
    let thread: Vec<_> = h
        .server
        .events(items[0].committee.as_str())
        .into_iter()
        .filter(|e| e.thread == Some(items[0].seq))
        .collect();
    assert_eq!(thread.len(), 1, "closed once");
    let status = senate_core::StatusPayload::of(&thread[0]).unwrap();
    assert_eq!(status.state, senate_core::StatusState::Archived);
    assert_eq!(thread[0].from, Address::Speaker);
    assert_eq!(thread[0].meta[meta::DESK_OUTCOME]["kind"], "closed");
    assert_eq!(
        thread[0].meta[meta::DESK_OUTCOME]["reason"],
        "Settled in chat."
    );
    assert_eq!(
        docket().await,
        [items[1].seq],
        "the closed one is off the desk"
    );

    // Without a reason too.
    let receipt: DeskOutcome = call(
        &socket,
        &decide(
            &items[1],
            &EventId::new(),
            DeskDecision::Close { reason: None },
        ),
    )
    .await
    .unwrap();
    assert!(matches!(receipt, DeskOutcome::Closed { reason: None, .. }));
    assert!(docket().await.is_empty());
    h.stop().await;
}

#[tokio::test]
async fn only_the_speaker_answers_and_not_what_waits_elsewhere() {
    use senate_core::Awaiting;
    // bob holds the seat: alice may read the item but not decide it.
    let (mut h, items) = desk(
        Some("senator::bob"),
        vec![(
            question(Address::Speaker, "Which quarter?"),
            Some(Awaiting::Addressed),
        )],
    )
    .await;
    let socket = h.socket();
    let d = detail(&socket, &items[0]).await;
    assert!(d.actions.is_empty());
    assert!(d.waits_on.unwrap().contains("Only the Speaker"));
    for decision in [
        DeskDecision::Answer { body: "Q3".into() },
        DeskDecision::Close { reason: None },
    ] {
        let err = call::<DeskOutcome>(&socket, &decide(&items[0], &EventId::new(), decision))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ipc::ClientError::Remote(e) if e.code == ipc::codes::NOT_PRESIDING),
            "{err}"
        );
    }
    h.stop().await;

    // An empty seat: nobody here decides.
    let (mut h, items) = desk(
        None,
        vec![(
            question(Address::Speaker, "Anyone?"),
            Some(Awaiting::Addressed),
        )],
    )
    .await;
    let err = call::<DeskOutcome>(
        &h.socket(),
        &decide(
            &items[0],
            &EventId::new(),
            DeskDecision::Answer { body: "x".into() },
        ),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("Only the Speaker"), "{err}");
    h.stop().await;

    // A child Senate's question while it is offline, and an agent's
    // permission prompt: neither is answered from here.
    let prompt = NewEvent::new(
        common::committee(),
        Kind::Question,
        Address::senator(&path("claude")),
        "Allow `rm -rf target`?",
    )
    .to(Address::Speaker)
    .with_meta(meta::HOSTED_APPROVAL, json!(true));
    let (mut h, items) = desk(
        Some("senator::alice"),
        vec![
            (
                question(Address::Speaker, "For the child"),
                Some(Awaiting::ChildOffline {
                    child: common::name("eu"),
                }),
            ),
            (prompt, Some(Awaiting::Addressed)),
        ],
    )
    .await;
    let socket = h.socket();
    for (item, why) in [(&items[0], "offline"), (&items[1], "Approvals")] {
        let d = detail(&socket, item).await;
        assert!(d.actions.is_empty());
        assert!(
            d.waits_on.as_deref().unwrap().contains(why),
            "{:?}",
            d.waits_on
        );
        let err = call::<DeskOutcome>(
            &socket,
            &decide(
                item,
                &EventId::new(),
                DeskDecision::Answer { body: "x".into() },
            ),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains(why), "{err}");
    }
    assert!(
        h.server
            .events(common::committee().as_str())
            .iter()
            .all(|e| e.thread.is_none()),
        "nothing posted"
    );
    h.stop().await;
}

#[tokio::test]
async fn forward_and_authorize_attaches_direct_execution_auth_and_distinguishes_idempotency() {
    let mut h = Harness::new().await;
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(Some("senator::alice".parse().unwrap()))
        .unwrap();
    let item = h.server.append(
        NewEvent::new(
            common::committee(),
            Kind::Request,
            Address::senior(&common::name("bob")),
            "Deploy v0.4.0 to staging.",
        )
        .to(Address::Speaker)
        .with_meta(meta::INTENDED, json!("other::senator::alice")),
    );
    h.start().await;
    let socket = h.socket();

    let action_id = EventId::new();
    let authorize = Request::DeskDecide {
        committee: item.committee.clone(),
        seq: item.seq,
        item_id: item.id.clone(),
        id: action_id.clone(),
        decision: DeskDecision::ForwardAndAuthorize {
            note: Some("  staging only  ".into()),
        },
    };
    let receipt: DeskOutcome = call(&socket, &authorize).await.unwrap();
    let retry: DeskOutcome = call(&socket, &authorize).await.unwrap();
    assert_eq!(retry, receipt);

    // Reusing the same id for plain Forward rejects as a mismatched retry.
    let mismatch = Request::DeskDecide {
        committee: item.committee.clone(),
        seq: item.seq,
        item_id: item.id,
        id: action_id,
        decision: DeskDecision::Forward,
    };
    assert!(call::<DeskOutcome>(&socket, &mismatch).await.is_err());

    let delivered = h.server.events("other");
    assert_eq!(delivered.len(), 1);
    let auth = senate_core::ExecutionAuthPayload::of(&delivered[0])
        .expect("ForwardAndAuthorize must attach meta::EXECUTION_AUTH");
    assert_eq!(auth.authorized_by, common::name("alice"));
    assert_eq!(auth.mode, senate_core::ExecutionAuthMode::Direct);
    assert_eq!(auth.note.as_deref(), Some("staging only"));
    h.stop().await;
}
