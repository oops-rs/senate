//! The machine's default secretary: ensured on the person's ask, never
//! duplicated, resumed, restored, refused when stopped on purpose or when
//! another seat holds its alias.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use common::{FakeServer, Harness, RECORD_HOSTED_ARGV, fast_timing, path};
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::ipc::{
    self, DefaultSecretary, DefaultSecretaryState, EnsureAction, HostedRuntime, Request, SpawnReply,
};
use senate_core::{CommitteeId, SenatorId};

const BIN: &str = env!("CARGO_BIN_EXE_senate");
const TIMEOUT: Duration = Duration::from_secs(10);

/// alice's machine, where alice is the Speaker; Claude Code is a fixture.
async fn machine() -> Harness {
    machine_presided_by("senator::alice", None).await
}

/// A machine whose Senate's Speaker is `speaker` (a senator id: with a
/// path, a senator of the Senate above), registered under `parent` when
/// given: that server is its uplink.
async fn machine_presided_by(speaker: &str, parent: Option<&FakeServer>) -> Harness {
    let mut h = Harness::new().await;
    h.config.hosted_session_limit = Some(4);
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let fixture = h.dir.path().join("fixture-claude");
    std::fs::write(
        &fixture,
        format!("#!/bin/sh\n{RECORD_HOSTED_ARGV}\nwhile IFS= read -r line; do :; done\n"),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[companion]\nclaude_bin = {:?}\n",
            fixture.to_str().unwrap()
        ),
    )
    .unwrap();
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(Some(senate_core::SpeakerName::id(
            speaker.parse::<SenatorId>().unwrap(),
        )))
        .unwrap();
    if let Some(parent) = parent {
        h.server.state.inner.lock().unwrap().parent = Some("acme".parse().unwrap());
        h.config.uplink = Some(senate_bridge::config::Uplink {
            url: parent.url.clone(),
            token: "tok-alice".into(),
        });
    }
    h.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: h.paths.clone(),
                config: h.config.clone(),
                timing: fast_timing(),
                desktop: None,
            },
            BIN.into(),
        )
        .await
        .unwrap(),
    );
    h
}

async fn call<T: serde::de::DeserializeOwned>(h: &Harness, request: &Request) -> T {
    ipc::request(&h.socket(), request, TIMEOUT, TIMEOUT)
        .await
        .unwrap_or_else(|error| panic!("{request:?}: {error}"))
}

async fn refused(h: &Harness, request: &Request) -> String {
    match ipc::request::<serde_json::Value>(&h.socket(), request, TIMEOUT, TIMEOUT).await {
        Err(ipc::ClientError::Remote(error)) => error.message,
        other => panic!("{request:?}: expected a refusal, got {other:?}"),
    }
}

fn office() -> CommitteeId {
    CommitteeId::new("office").unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_secretary_is_started_once_and_brought_back_as_it_was_left() {
    let mut h = machine().await;
    let before: DefaultSecretary = call(&h, &Request::DefaultSecretary).await;
    assert_eq!(before.state, DefaultSecretaryState::NotRecorded);
    assert!(before.senator.is_none());

    // The first ask: `office` opened, the secretary started on the desk in
    // a directory Senate owns, its brief sent as its first request.
    let started: DefaultSecretary = call(&h, &Request::EnsureSecretary).await;
    assert_eq!(started.action, Some(EnsureAction::Started));
    assert_eq!(started.state, DefaultSecretaryState::Running);
    let senator = started.senator.clone().unwrap();
    {
        let server = h.server.state.inner.lock().unwrap();
        assert_eq!(
            server.aliases.get(&office(), &path("secretary")),
            Some(&senator)
        );
        let desk = server.roster.office().unwrap();
        assert!(desk.agent_workers().any(|w| w == &senator), "{desk:?}");
    }
    let brief = h
        .server
        .events("office")
        .into_iter()
        .find(|e| {
            e.kind == senate_core::Kind::Request
                && e.to.iter().any(|a| a.as_senator() == Some(&senator))
        })
        .expect("its brief");
    assert!(brief.body.contains("open_committee"), "{}", brief.body);
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(rows.rows.len(), 1);
    assert_eq!(
        rows.rows[0].dir,
        h.paths.home().join("secretary").canonicalize().unwrap()
    );

    // Asked again: nothing new.
    let again: DefaultSecretary = call(&h, &Request::EnsureSecretary).await;
    assert_eq!(again.action, Some(EnsureAction::AlreadyRunning));
    assert_eq!(again.senator.as_ref(), Some(&senator));
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(rows.rows.len(), 1, "never two");

    // Paused: the next ask resumes it.
    let _: serde_json::Value = call(
        &h,
        &Request::Pause {
            senator: senator.clone().into(),
            force: true,
            timeout_secs: Some(1),
        },
    )
    .await;
    let paused: DefaultSecretary = call(&h, &Request::DefaultSecretary).await;
    assert_eq!(paused.state, DefaultSecretaryState::Paused);
    let resumed: DefaultSecretary = call(&h, &Request::EnsureSecretary).await;
    assert_eq!(resumed.action, Some(EnsureAction::Resumed));

    // Stopped on purpose: refused, saying how to bring it back.
    let _: serde_json::Value = call(
        &h,
        &Request::Stop {
            session: Default::default(),
            senator: senator.clone().into(),
            force: true,
        },
    )
    .await;
    let stopped: DefaultSecretary = call(&h, &Request::DefaultSecretary).await;
    assert_eq!(stopped.state, DefaultSecretaryState::Stopped);
    let message = refused(&h, &Request::EnsureSecretary).await;
    assert!(
        message.contains("stopped on purpose")
            && message.contains("senate session restore office::secretary"),
        "{message}"
    );

    // Restored by hand, then its terminal ends unasked (a restart): the
    // next ask restores it.
    let _: SpawnReply = call(
        &h,
        &Request::Restore {
            session: Default::default(),
            senator: senator.clone().into(),
            model: Default::default(),
        },
    )
    .await;
    senate_bridge::host::kill_all(&h.paths);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let state: DefaultSecretary = call(&h, &Request::DefaultSecretary).await;
        if state.state == DefaultSecretaryState::Ended {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "still {:?}",
            state.state
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let restored: DefaultSecretary = call(&h, &Request::EnsureSecretary).await;
    assert_eq!(restored.action, Some(EnsureAction::Restored));
    assert_eq!(restored.senator.as_ref(), Some(&senator));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_seat_aliased_secretary_is_never_taken_over() {
    let mut h = machine().await;
    let _: serde_json::Value = call(
        &h,
        &Request::OpenCommittee {
            committee: office(),
        },
    )
    .await;
    let _: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: office(),
            alias: Some(path("secretary")),
            cwd: h.dir.path().to_path_buf(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    let message = refused(&h, &Request::EnsureSecretary).await;
    assert!(
        message.contains("office::secretary is already the alias of")
            && message.contains("will not take it over"),
        "{message}"
    );
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(rows.rows.len(), 1, "nothing started");
    h.stop().await;
}

fn as_session(
    rows: &ipc::SessionsReply,
    senator: &senate_core::SenatorName,
) -> ipc::SessionSelector {
    let row = rows
        .rows
        .iter()
        .find(|row| &row.primary == senator)
        .expect("its session");
    ipc::SessionSelector {
        session_id: Some(row.session_id.clone()),
        claude_pid: None,
    }
}

fn open(session: ipc::SessionSelector, committee: &str, cwd: &std::path::Path) -> Request {
    Request::DeskOpenCommittee {
        session,
        committee: CommitteeId::new(committee).unwrap(),
        cwd: cwd.to_path_buf(),
        runtime: Some(HostedRuntime::Claude),
        profile: None,
        alias: Some(path("main")),
        prompt: Some("Fix the refund rounding bug; report to the secretary.".into()),
    }
}

/// `open_committee`: only a desk worker; refused for a committee the
/// office did not open; a failed chair is finished by calling again, with
/// one opening note; a chair is never replaced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_desk_opens_a_committee_and_a_retry_finishes_it() {
    let mut h = machine().await;
    let secretary: DefaultSecretary = call(&h, &Request::EnsureSecretary).await;
    let secretary = secretary.senator.unwrap();
    // A chair and a member of refund-v2, for the refusals.
    let spawn = |chair: bool, alias: &str| Request::Spawn {
        attempt_id: None,
        session: Default::default(),
        runtime: Some(HostedRuntime::Claude),
        profile: None,
        committee: common::committee(),
        alias: Some(path(alias)),
        cwd: h.dir.path().to_path_buf(),
        chair,
        secretary: false,
        prompt: None,
        model: Default::default(),
    };
    let chair: SpawnReply = call(&h, &spawn(true, "lead")).await;
    let member: SpawnReply = call(&h, &spawn(false, "helper")).await;
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    let project = h.dir.path().join("ledger");

    for who in [&chair.senator, &member.senator] {
        let message = match ipc::request::<serde_json::Value>(
            &h.socket(),
            &open(as_session(&rows, who), "ledger-fix", &project),
            TIMEOUT,
            TIMEOUT,
        )
        .await
        {
            Err(ipc::ClientError::Remote(error)) => {
                assert!(
                    [ipc::codes::NOT_PRESIDING, ipc::codes::NOT_CHAIRING]
                        .contains(&error.code.as_str()),
                    "{who}: {error:?}"
                );
                error.message
            }
            other => panic!("{who}: expected a refusal, got {other:?}"),
        };
        assert!(!message.is_empty());
    }
    assert!(h.server.events("ledger-fix").is_empty(), "nothing opened");
    // This test server keeps one chair for all committees: forget refund-v2's.
    h.server.state.inner.lock().unwrap().chair = None;

    let desk = as_session(&rows, &secretary);
    // A committee the office did not open.
    let message = refused(&h, &open(desk.clone(), "refund-v2", &project)).await;
    assert!(
        message.contains("add a chair or members to it with `spawn`"),
        "{message}"
    );

    // The project directory does not exist yet: opened, chair not started.
    let message = refused(&h, &open(desk.clone(), "ledger-fix", &project)).await;
    assert!(
        message.contains("committee ledger-fix is open, but its chair did not start")
            && message.contains("Call open_committee again"),
        "{message}"
    );
    let opening = h.server.events("ledger-fix");
    assert_eq!(opening.len(), 1);
    assert_eq!(
        opening[0].meta.get(senate_core::meta::ACTED_BY),
        Some(&serde_json::json!(secretary)),
        "recorded as the secretary acting for the office"
    );
    assert_eq!(opening[0].from, senate_core::Address::Speaker);

    // Called again once the directory is there: no second note, a chair.
    std::fs::create_dir_all(&project).unwrap();
    let done: ipc::DeskOpenReply = call(&h, &open(desk.clone(), "ledger-fix", &project)).await;
    assert_eq!(done.opened, None);
    let started = done.chair.expect("the chair started");
    let notes = h
        .server
        .events("ledger-fix")
        .into_iter()
        .filter(|e| e.id == opening[0].id)
        .count();
    assert_eq!(notes, 1, "opened once");
    assert_eq!(
        h.server
            .state
            .inner
            .lock()
            .unwrap()
            .chair
            .as_ref()
            .map(|(c, _)| c),
        Some(&started.senator),
    );
    let brief = h
        .server
        .events("ledger-fix")
        .into_iter()
        .find(|e| e.kind == senate_core::Kind::Request)
        .expect("the chair's first request");
    assert!(brief.body.contains("refund rounding"));

    // The chair's session launched but its appointment never reached the
    // server: called again, the same spawn attempt finds that session (no
    // second one) and the appointment is made.
    h.server.state.inner.lock().unwrap().chair = None;
    let confirmed: ipc::DeskOpenReply = call(&h, &open(desk.clone(), "ledger-fix", &project)).await;
    assert_eq!(
        confirmed.chair.as_ref().map(|c| &c.senator),
        Some(&started.senator),
        "the same session"
    );
    assert_eq!(
        h.server
            .state
            .inner
            .lock()
            .unwrap()
            .chair
            .as_ref()
            .map(|(c, _)| c),
        Some(&started.senator),
    );
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(rows.rows.len(), 4, "no second chair session");

    // Again: the chair stands; nothing new starts.
    let again: ipc::DeskOpenReply = call(&h, &open(desk, "ledger-fix", &project)).await;
    assert!(again.chair.is_none());
    assert_eq!(again.existing_chair.as_ref(), Some(&started.senator));
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(rows.rows.len(), 4, "secretary, lead, helper, main");
    h.stop().await;
}

/// An agent cannot start the default secretary: `senate secretary ask` from
/// inside a hosted session's process tree is refused, and nothing starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_cannot_start_the_default_secretary() {
    let mut h = machine().await;
    let project = h.dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    // The hosted TUI itself asks, as an agent would from a tool.
    std::fs::write(
        h.dir.path().join("fixture-claude"),
        format!(
            "#!/bin/sh\n{RECORD_HOSTED_ARGV}\n\
             while [ ! -e .go ]; do sleep 0.05; done\n\
             {BIN:?} secretary ask 'open a committee' > .guard.tmp 2>&1; echo \"exit $?\" >> .guard.tmp; mv .guard.tmp .guard\n\
             while IFS= read -r line; do :; done\n"
        ),
    )
    .unwrap();
    let _: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: common::committee(),
            alias: Some(path("worker")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    std::fs::write(project.join(".go"), "").unwrap();
    let out = common::eventually("the TUI's ask came back", TIMEOUT, || {
        std::fs::read_to_string(project.join(".guard")).ok()
    })
    .await;
    assert!(out.contains("exit 1"), "{out}");
    assert!(
        out.contains("inside the process tree of")
            && out.contains("Only the person starts the default secretary"),
        "{out}"
    );
    let state: DefaultSecretary = call(&h, &Request::DefaultSecretary).await;
    assert_eq!(state.state, DefaultSecretaryState::NotRecorded);
    h.stop().await;
}

/// A machine's Senate registered as a child, presided over by its person as
/// a senator of the Senate above: its desk is kept here, under the person's
/// handle, and the default secretary works it. Nothing of it is written to
/// the Senate above.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_presided_over_by_its_person_keeps_its_own_desk() {
    let parent = FakeServer::start(&[("tok-alice", "alice")], &["refund-v2"]).await;
    let mut h = machine_presided_by("/acme::senator::alice", Some(&parent)).await;
    let office = h
        .server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .office()
        .unwrap();
    assert_eq!(office.kind, senate_core::SenatorKind::Person);
    assert_eq!(
        office.speaker_here(),
        None,
        "the Speaker is the parent's senator"
    );

    let started: DefaultSecretary = call(&h, &Request::EnsureSecretary).await;
    assert_eq!(started.action, Some(EnsureAction::Started));
    let senator = started.senator.clone().unwrap();
    {
        let child = h.server.state.inner.lock().unwrap();
        assert_eq!(
            child.secretaries.get(&path("alice")),
            Some(&vec![senator.clone()])
        );
        assert!(
            child
                .roster
                .office()
                .unwrap()
                .agent_workers()
                .any(|w| w == &senator)
        );
    }
    // The desk lists it, as this Senate's desk.
    let desk: ipc::DeskReply = call(&h, &Request::Desk { set: None }).await;
    assert_eq!(desk.speaker, path("alice"));
    assert_eq!(desk.secretaries, std::slice::from_ref(&senator));
    // Asked again: the same one.
    let again: DefaultSecretary = call(&h, &Request::EnsureSecretary).await;
    assert_eq!(again.action, Some(EnsureAction::AlreadyRunning));
    // What `senate secretary ask` sends after the ensure: a request to it.
    let sent = senate_bridge::speak::send(
        &h.socket(),
        CommitteeId::new("office").unwrap(),
        ipc::SayKind::Request,
        senator.as_str(),
        "Make agy the chair of ibot".into(),
    )
    .await
    .unwrap();
    assert_eq!(sent.posted.committee, CommitteeId::new("office").unwrap());
    let asked = h.server.last_event("office");
    assert_eq!(asked.body, "Make agy the chair of ibot");
    assert!(
        asked.to.iter().any(|a| a.as_senator() == Some(&senator)),
        "{:?}",
        asked.to
    );

    // Its desk tools act for this Senate's office: it opens a committee
    // for a project here.
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    let project = h.dir.path().join("ibot");
    std::fs::create_dir_all(&project).unwrap();
    let opened: ipc::DeskOpenReply =
        call(&h, &open(as_session(&rows, &senator), "ibot", &project)).await;
    assert!(
        opened.opened.is_some() && opened.chair.is_some(),
        "{opened:?}"
    );
    assert_eq!(
        h.server.events("ibot")[0].from,
        senate_core::Address::Speaker
    );

    // Nothing crossed upward: no desk change, no event in the Senate above.
    {
        let above = parent.state.inner.lock().unwrap();
        assert!(above.secretaries.is_empty(), "{:?}", above.secretaries);
        assert!(
            above.events.values().all(Vec::is_empty),
            "{:?}",
            above.events.keys().collect::<Vec<_>>()
        );
    }
    h.stop().await;
}

/// A child whose Speaker is an agent of the Senate above has no desk here:
/// the default secretary and a desk appointment are refused, saying where
/// that desk is kept.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_presided_over_by_an_agent_above_has_no_desk_here() {
    let parent = FakeServer::start(&[("tok-alice", "alice")], &["refund-v2"]).await;
    let mut h = machine_presided_by("/acme::senator::claude-up", Some(&parent)).await;
    let message = refused(&h, &Request::EnsureSecretary).await;
    assert!(
        message.contains("[/acme::senator::claude-up], is an agent of the Senate /acme")
            && message.contains("appoint its secretaries there"),
        "{message}"
    );
    let message = refused(&h, &Request::Desk { set: None }).await;
    assert!(
        message.contains("is an agent of the Senate /acme"),
        "{message}"
    );
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert!(rows.rows.is_empty(), "nothing started");
    h.stop().await;
}

/// `senate senator chair`: the person names a chair (the server's own check
/// decides who may); from inside a coding session it is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_person_names_a_chair_and_an_agent_cannot_in_their_name() {
    let mut h = machine().await;
    let project = h.dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let worker: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: common::committee(),
            alias: Some(path("main")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    let info: senate_core::api::ChairInfo = call(
        &h,
        &Request::SetChair {
            session: Default::default(),
            committee: Some(common::committee()),
            senator: worker.senator.clone(),
        },
    )
    .await;
    assert_eq!(info.chair.as_ref(), Some(&worker.senator));

    // The same, typed by an agent in its terminal.
    std::fs::write(
        h.dir.path().join("fixture-claude"),
        format!(
            "#!/bin/sh\n{RECORD_HOSTED_ARGV}\n\
             while [ ! -e .go ]; do sleep 0.05; done\n\
             {BIN:?} senator chair {} --committee refund-v2 > .guard.tmp 2>&1; echo \"exit $?\" >> .guard.tmp; mv .guard.tmp .guard\n\
             while IFS= read -r line; do :; done\n",
            worker.senator
        ),
    )
    .unwrap();
    let agent_dir = h.dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    let _: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: common::committee(),
            alias: Some(path("other")),
            cwd: agent_dir.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    std::fs::write(agent_dir.join(".go"), "").unwrap();
    let out = common::eventually("the TUI's chair command came back", TIMEOUT, || {
        std::fs::read_to_string(agent_dir.join(".guard")).ok()
    })
    .await;
    assert!(out.contains("exit 1"), "{out}");
    assert!(
        out.contains("inside the process tree of") && out.contains("senate senator chair"),
        "{out}"
    );
    h.stop().await;
}
