//! The FFI facade, from Rust, against the fake daemon: what a native app
//! sees through [`CoreListener`] and what its calls do.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use senate_app_core::{
    AppCore, AppState, ApprovalDecision, ApprovalRequest, CoreError, CoreListener, HostedSession,
};
use senate_bridge::approvals::Decision;
use senate_bridge::ipc::Request;
use senate_core::Seq;

use common::{FakeDaemon, setup};

#[derive(Default)]
struct Listener {
    last: Mutex<Option<AppState>>,
    approvals: Mutex<Vec<ApprovalRequest>>,
    notes: Mutex<Vec<senate_app_core::ffi::PersonNote>>,
    prompts: Mutex<Vec<HostedSession>>,
}

impl CoreListener for Listener {
    fn on_state(&self, state: AppState) {
        *self.last.lock().unwrap() = Some(state);
    }
    fn on_approval_requested(&self, approval: ApprovalRequest) {
        self.approvals.lock().unwrap().push(approval);
    }
    fn on_for_person(&self, note: senate_app_core::ffi::PersonNote) {
        self.notes.lock().unwrap().push(note);
    }
    fn on_prompt_waiting(&self, session: HostedSession) {
        self.prompts.lock().unwrap().push(session);
    }
}

async fn until(listener: &Listener, what: &str, ok: impl Fn(&AppState) -> bool) -> AppState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(s) = listener.last.lock().unwrap().clone()
            && ok(&s)
        {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; last: {:#?}",
            listener.last.lock().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn personal_stats_round_trip_through_the_core() {
    let (dir, socket, world) = setup();
    let _daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    core.start(Arc::new(Listener::default())).unwrap();
    let report = core
        .stats(
            "2026-11-01".into(),
            "2026-11-14".into(),
            "Asia/Taipei".into(),
        )
        .await
        .unwrap();
    assert_eq!(report.days[0].prompts, 5);
    assert_eq!(report.days[0].date, "2026-11-14");
    assert_eq!(report.top_repos[0].name, "refund-service");
}

#[tokio::test]
async fn session_drop_uses_the_upload_wire_and_preserves_refusals() {
    let (dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    core.start(Arc::new(Listener::default())).unwrap();
    let saved = core
        .session_drop("fixture".into(), "image.png".into(), vec![0, 255])
        .await
        .unwrap();
    assert_eq!(saved, "/tmp/senate-fake/drops/fixture/image.png");
    let problem = core
        .session_drop("unknown".into(), "image.png".into(), vec![])
        .await
        .unwrap_err();
    assert!(
        matches!(problem, senate_app_core::CoreError::Failed { message }
        if message == "no registered session with that ID")
    );
    drop(daemon);
}

#[tokio::test]
async fn the_desk_action_returns_its_receipt_and_preserves_a_daemon_refusal() {
    let (dir, socket, world) = setup();
    common::desk_fixture(&mut world.lock().unwrap());
    let daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    core.start(Arc::new(Listener::default())).unwrap();

    let committee = "refund-v2".to_owned();
    let full = core.desk_item(committee.clone(), 60).await.unwrap();
    assert!(full.item.body.contains("revised refund threshold"));
    assert_eq!(full.thread.root, 60);
    assert!(full.blocked.is_none());
    let id = core.new_desk_action_id();
    let forwarded = core
        .decide_desk_item(
            committee.clone(),
            60,
            full.event_id.clone(),
            id.clone(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(forwarded.seq, 75);
    assert!(forwarded.delivered);
    assert_eq!(
        core.decide_desk_item(committee.clone(), 60, full.event_id, id, None)
            .await
            .unwrap(),
        forwarded
    );
    let original = core.thread(committee.clone(), 60).await.unwrap();
    assert_eq!(original.events[0].body, full.item.body);
    let failed_item = core.desk_item(committee.clone(), 61).await.unwrap();
    let fail = core
        .decide_desk_item(
            committee.clone(),
            61,
            failed_item.event_id.clone(),
            core.new_desk_action_id(),
            None,
        )
        .await
        .unwrap_err();
    assert!(
        fail.to_string().contains("temporarily unavailable"),
        "{fail}"
    );
    let refused = core
        .decide_desk_item(
            committee.clone(),
            61,
            failed_item.event_id,
            core.new_desk_action_id(),
            Some("Outside policy".into()),
        )
        .await
        .unwrap();
    assert!(!refused.delivered);
    let blocked = core.desk_item(committee.clone(), 62).await.unwrap();
    assert!(blocked.blocked.unwrap().contains("Only the Speaker"));
    assert!(blocked.actions.is_empty());
    assert!(blocked.waits_on.unwrap().contains("Only the Speaker"));
    daemon.stop();
}

#[tokio::test]
async fn the_speaker_answers_or_closes_a_desk_item_through_one_choice() {
    use senate_app_core::{DeskAction, DeskChoice, DeskOutcomeKind};
    let (dir, socket, world) = setup();
    common::desk_fixture(&mut world.lock().unwrap());
    let daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    core.start(Arc::new(Listener::default())).unwrap();
    let committee = "refund-v2".to_owned();

    let forwardable = core.desk_item(committee.clone(), 60).await.unwrap();
    assert_eq!(
        forwardable.actions,
        [
            DeskAction::Forward,
            DeskAction::Refuse,
            DeskAction::Answer,
            DeskAction::Close
        ]
    );
    let item = core.desk_item(committee.clone(), 63).await.unwrap();
    assert_eq!(item.actions, [DeskAction::Answer, DeskAction::Close]);
    assert_eq!(item.waits_on, None);

    let id = core.new_desk_action_id();
    let answer = DeskChoice::Answer {
        body: "From Q3.".into(),
    };
    let answered = core
        .desk_decide(
            committee.clone(),
            63,
            item.event_id.clone(),
            id.clone(),
            answer.clone(),
        )
        .await
        .unwrap();
    assert_eq!(answered.kind, DeskOutcomeKind::Answered);
    assert_eq!(answered.seq, 77);
    assert!(!answered.delivered);
    assert_eq!(
        core.desk_decide(committee.clone(), 63, item.event_id.clone(), id, answer)
            .await
            .unwrap(),
        answered,
        "a retry with the same id is the same answer"
    );

    let closed = core
        .desk_decide(
            committee.clone(),
            61,
            core.desk_item(committee.clone(), 61)
                .await
                .unwrap()
                .event_id,
            core.new_desk_action_id(),
            DeskChoice::Close {
                reason: Some("Settled in chat.".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(closed.kind, DeskOutcomeKind::Closed);
    assert_eq!(closed.reason.as_deref(), Some("Settled in chat."));
    daemon.stop();
}

#[tokio::test]
async fn snapshots_and_actions_through_the_facade() {
    let (dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en-US".into())).unwrap();
    assert_eq!(
        core.reply("refund-v2".into(), 118, "x".into()).await,
        Err(CoreError::NotStarted)
    );

    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();
    assert_eq!(core.start(listener.clone()), Err(CoreError::AlreadyStarted));

    let s = until(&listener, "online with inbox, approval and docket", |s| {
        s.online && s.inbox.len() == 2 && s.approvals.len() == 1 && !s.docket.is_empty()
    })
    .await;
    assert_eq!(s.person.as_deref(), Some("alice"));
    assert_eq!(s.menu.title, "Senate: online as alice");
    // Two threads, one approval, two items awaiting the Speaker.
    assert_eq!(s.menu.badge.as_deref(), Some("5"));
    assert_eq!(s.counts.pending, 5);
    assert_eq!(s.counts.docket, 2);
    assert_eq!(s.senates.len(), 1, "the daemon serves one Senate");
    assert_eq!(s.inbox[0].state, "suspended");
    assert_eq!(s.inbox[0].triage.as_deref(), Some("needs_human"));
    assert_eq!(s.codex.len(), 1);
    assert_eq!(s.codex[0].approvals, 1);
    assert_eq!(listener.approvals.lock().unwrap()[0].id, "019a-thread:7");

    // The Codex thread's terminal command resumes it in its directory.
    let cmd = core.codex_terminal(Some("019a-thread".into())).unwrap();
    assert_eq!(cmd.cwd.as_deref(), Some("/Users/alice/dev/refund"));
    assert_eq!(cmd.argv[1], "resume");

    // Mentions read as names: `claude` is `lead` on refund-v2 (once the
    // aliases are in), an id elsewhere.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while senate_app_core::ffi::mention_names_version() == 0
        || senate_app_core::ffi::split_mentions(
            "[senator::claude]".into(),
            Some("refund-v2".into()),
        )[0]
        .display
        .as_deref()
            != Some("lead")
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "aliases never arrived"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let parts = senate_app_core::ffi::split_mentions(
        "ask [speaker::claude] or [senator::alice]".into(),
        Some("refund-v2".into()),
    );
    assert_eq!(parts[1].display.as_deref(), Some("lead (Speaker)"));
    assert_eq!(parts[1].id.as_deref(), Some("speaker::claude"));
    assert_eq!(parts[3].display.as_deref(), Some("alice"));

    // Empty replies never reach the daemon, and a bad committee is refused
    // in the language the core was given.
    assert!(matches!(
        core.reply("refund-v2".into(), 118, "  ".into()).await,
        Err(CoreError::Invalid { message }) if message == "the reply is empty"
    ));
    core.set_language(Some("zh-Hans".into()));
    assert!(matches!(
        core.reply("refund-v2".into(), 118, "  ".into()).await,
        Err(CoreError::Invalid { message }) if message == "回复内容为空"
    ));
    assert!(matches!(
        core.take("Not A Committee".into(), 1).await,
        Err(CoreError::Invalid { .. })
    ));
    until(&listener, "Chinese menu", |s| {
        s.menu.title.starts_with("Senate：")
    })
    .await;
    core.set_language(Some("en".into()));

    core.reply("refund-v2".into(), 118, "走审批。".into())
        .await
        .unwrap();
    // The answered thread leaves the list, the counts and the menu bar in
    // the same snapshot.
    let s = until(&listener, "answered", |s| s.inbox.len() == 1).await;
    assert_eq!(s.counts.threads, 1);
    assert_eq!(s.counts.pending, 4);
    assert_eq!(s.menu.badge.as_deref(), Some("4"));
    assert_eq!(s.menu.pending, "1 pending thread, 1 approval");
    core.close("refund-v2".into(), 131).await.unwrap();
    until(&listener, "closed", |s| s.inbox.is_empty()).await;

    core.approve("019a-thread:7".into(), ApprovalDecision::DenyAndStop)
        .await
        .unwrap();
    until(&listener, "approval gone", |s| s.approvals.is_empty()).await;
    let late = core
        .approve("019a-thread:7".into(), ApprovalDecision::Allow)
        .await
        .unwrap_err();
    assert!(
        matches!(&late, CoreError::Failed { message } if message.contains("already answered")),
        "{late:?}"
    );

    let requests = daemon.requests();
    assert!(requests.iter().any(|r| matches!(
        r,
        Request::Approve {
            decision: Decision::Cancel,
            ..
        }
    )));
    assert!(requests.iter().any(|r| matches!(
        r,
        Request::Answer {
            thread: Seq(118),
            as_person: true,
            ..
        }
    )));
}

#[tokio::test]
async fn reading_a_thread_returns_its_events_and_marks_it_seen() {
    let (dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();
    until(&listener, "online", |s| s.online && s.inbox.len() == 2).await;

    let view = core.thread("refund-v2".into(), 131).await.unwrap();
    assert_eq!(view.root, 131);
    assert_eq!(view.committee, "refund-v2");
    assert_eq!(view.events[0].body, "which env?");
    assert_eq!(view.events[0].kind_label, "question");
    assert_eq!(view.events[1].kind, "note");
    assert!(view.events.iter().all(|e| !e.mine));

    assert!(matches!(
        core.thread("refund-v2".into(), 999).await,
        Err(CoreError::Failed { message }) if message.contains("no thread 999")
    ));
    assert!(daemon.requests().iter().any(|r| matches!(
        r,
        Request::Thread {
            thread: Seq(131),
            mark_seen: true,
            ..
        }
    )));
}

#[tokio::test]
async fn hosted_sessions_show_their_waiting_prompt_and_stop_and_restore() {
    let (dir, socket, world) = setup();
    world.lock().unwrap().sessions = common::hosted_sessions();
    let daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();
    let s = until(&listener, "sessions read", |s| s.sessions.len() == 2).await;
    let lead = &s.sessions[0];
    assert_eq!(lead.label, "refund-v2::lead");
    assert!(lead.running);
    assert_eq!(lead.prompt_tool.as_deref(), Some("cargo publish"));
    assert_eq!(lead.model.as_deref(), Some("opus · high"));
    assert_eq!(s.counts.prompts, 1);
    // Two threads, one approval, one waiting terminal, two docket items.
    assert_eq!(s.menu.badge.as_deref(), Some("6"));
    assert_eq!(
        s.menu.prompts.as_deref(),
        Some("1 terminal waiting for your permission")
    );
    assert_eq!(listener.prompts.lock().unwrap().len(), 1, "notified once");

    // Attaching is a terminal command; the decision is the person's, there.
    let attach = core.attach_terminal(lead.label.clone()).unwrap();
    assert_eq!(attach.argv, ["senate", "attach", "refund-v2::lead"]);
    assert!(attach.shell_line.starts_with("SENATE_HOME="));
    assert!(attach.shell_line.ends_with("senate attach refund-v2::lead"));
    let exact = core.attach_terminal_for_session(lead.session_id.clone());
    assert_eq!(
        exact.argv,
        [
            "senate",
            "attach",
            &format!("--session={}", lead.session_id)
        ]
    );
    assert_eq!(core.watch_terminal().argv, ["senate", "session", "watch"]);

    // The prompt is a permission prompt: the app may answer it, as the
    // person, through the daemon.
    assert!(lead.prompt_answerable);
    core.approve_session("refund-v2::lead".into(), ApprovalDecision::AllowForSession)
        .await
        .unwrap();
    until(&listener, "answered", |s| s.counts.prompts == 0).await;
    assert!(matches!(
        core.approve_session("refund-v2::lead".into(), ApprovalDecision::Deny).await,
        Err(CoreError::Failed { message }) if message.contains("no permission prompt waiting")
    ));
    assert!(daemon.requests().iter().any(|r| matches!(
        r,
        Request::ApproveSession { senator, decision: Decision::AllowForSession }
            if senator.to_string() == "refund-v2::lead"
    )));

    core.stop_session("refund-v2::lead".into()).await.unwrap();
    let s = until(&listener, "stopped", |s| {
        s.sessions.iter().all(|x| !x.running) && s.counts.prompts == 0
    })
    .await;
    assert_eq!(s.menu.prompts, None);
    core.restore_session("reviewer".into()).await.unwrap();
    until(&listener, "restored", |s| {
        s.sessions
            .iter()
            .any(|x| x.label == "reviewer" && x.running)
    })
    .await;
    assert!(matches!(
        core.stop_session("nobody-here".into()).await,
        Err(CoreError::Failed { message }) if message.contains("no session")
    ));
    assert_eq!(listener.prompts.lock().unwrap().len(), 1, "never twice");
    assert!(
        daemon
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Stop { force: false, .. }))
    );
}

#[tokio::test]
async fn the_senate_shows_its_chairs_children_and_uplink() {
    let (dir, socket, world) = setup();
    world.lock().unwrap().status.roles = Some(common::roles());
    let _daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();
    let s = until(&listener, "roles", |s| {
        s.senates.first().is_some_and(|x| !x.chairs.is_empty())
    })
    .await;
    let senate = &s.senates[0];
    let claude = senate
        .committees
        .iter()
        .find(|c| c.agent == "claude")
        .unwrap();
    assert_eq!(claude.duty.as_deref(), Some("reviews refunds"));
    assert_eq!(senate.chairs[0].committee, "refund-v2");
    assert_eq!(senate.chairs[0].chair.as_deref(), Some("[senator::claude]"));
    let child = &senate.children[0];
    assert_eq!(child.name, "carol-desk");
    assert!(child.online);
    assert_eq!(child.seats[0].name, "refund-dev (carol-desk)");
    assert_eq!(child.seats[0].duty.as_deref(), Some("reviews refunds"));
    assert_eq!(
        senate.uplink.as_deref(),
        Some("registered as alice-desk with /acme")
    );
}

/// What a timeline watch saw.
#[derive(Default)]
struct Timeline {
    deltas: Mutex<Vec<senate_app_core::TimelineDelta>>,
    ended: Mutex<Option<Option<String>>>,
}

impl senate_app_core::TimelineListener for Timeline {
    fn on_delta(&self, delta: senate_app_core::TimelineDelta) {
        self.deltas.lock().unwrap().push(delta);
    }
    fn on_end(&self, error: Option<String>) {
        *self.ended.lock().unwrap() = Some(error);
    }
}

#[tokio::test]
async fn sessions_by_committee_their_timelines_and_the_composer() {
    use senate_app_core::{EntryBody, SessionStatus};

    let (dir, socket, world) = setup();
    {
        let mut w = world.lock().unwrap();
        common::busy_senate(&mut w);
        common::session_tree(&mut w);
        w.grow_every = Some(Duration::from_millis(50));
    }
    let _daemon = FakeDaemon::start(&socket, Arc::clone(&world));
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en-US".into())).unwrap();
    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();
    let s = until(&listener, "the session tree", |s| {
        s.groups.iter().map(|g| g.sessions.len()).sum::<usize>() == 9
    })
    .await;

    // Every committee, by name; the sessions of no committee last.
    let names: Vec<_> = s.groups.iter().map(|g| g.title.as_str()).collect();
    assert_eq!(
        names,
        ["docs", "infra", "payments", "refund-v2", "No committee"]
    );
    let group = |name: &str| s.groups.iter().find(|g| g.title == name).unwrap();
    // refund-v2: its chair first, waiting at a prompt; the committee shows it.
    let refund = group("refund-v2");
    assert_eq!(refund.sessions[0].display_name, "lead");
    assert!(refund.sessions[0].chair);
    assert_eq!(refund.sessions[0].status, SessionStatus::NeedsYou);
    assert_eq!(refund.status, Some(SessionStatus::NeedsYou));
    assert!(
        refund
            .sessions
            .iter()
            .any(|x| x.hosted.is_none() && x.status == SessionStatus::Working)
    );
    assert!(
        refund
            .sessions
            .iter()
            .any(|x| x.status == SessionStatus::Stopped)
    );
    // payments: the chair works; the request it put to alice counts there.
    let payments = group("payments");
    assert_eq!(payments.sessions[0].display_name, "ledger");
    assert_eq!(payments.sessions[0].status, SessionStatus::Working);
    assert!(payments.waiting >= 2, "the request and the docket item");
    // docs: its chair is paused, but what it escalated to alice is unread,
    // which comes first.
    let scribe = &group("docs").sessions[0];
    assert_eq!(scribe.status, SessionStatus::Unread);
    assert!(scribe.hosted.as_ref().unwrap().paused);
    let oncall = group("infra")
        .sessions
        .iter()
        .find(|x| x.display_name == "oncall")
        .unwrap();
    assert!(oncall.readable, "Antigravity timelines are supported");
    // A request put to alice is hers to decide; a plain question is not.
    let request = s.inbox.iter().find(|t| t.root == 210).unwrap();
    assert!(request.to_decide);

    // lead's conversation: the tail, then older pages back to the start.
    let page = core
        .timeline("s-claude".into(), None, Some(20))
        .await
        .unwrap();
    assert!(page.available && page.has_older);
    assert_eq!(page.items.len(), 20);
    let last = page.items.last().unwrap();
    assert!(matches!(&last.body, EntryBody::Tool { call } if call.status == "running"));
    let older = core
        .timeline("s-claude".into(), page.first_seq, Some(300))
        .await
        .unwrap();
    assert!(!older.has_older);
    assert_eq!(older.last_seq.unwrap() + 1, page.first_seq.unwrap());
    // This Antigravity fixture has not created a conversation database yet.
    let none = core.timeline("ops-1".into(), None, None).await.unwrap();
    assert!(!none.available);
    assert!(none.unavailable.unwrap().contains("No transcript"));

    // Watched, the running call completes in place, then more is said.
    let seen = Arc::new(Timeline::default());
    core.watch_timeline("s-claude".into(), page.last_seq.unwrap(), seen.clone())
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while seen.deltas.lock().unwrap().len() < 4 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the timeline did not grow"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    core.stop_timeline("s-claude".into());
    let deltas = seen.deltas.lock().unwrap().clone();
    assert!(deltas[0].items.is_empty(), "nothing after the tail yet");
    let done = &deltas[1].items[0];
    assert_eq!(done.seq, last.seq, "the same item, updated");
    assert!(matches!(&done.body, EntryBody::Tool { call } if call.status == "completed"));
    assert!(
        matches!(&deltas[2].items[0].body, EntryBody::Senate { message } if message.thread == Some(140))
    );
    assert!(!deltas[3].working);

    // The composer: a note to lead on refund-v2, as alice.
    // It comes back as the log took it: the card that will show it in the
    // session's conversation carries the same committee and seq.
    let sent = core
        .say("s-claude".into(), "Ship it.".into())
        .await
        .unwrap();
    assert_eq!(sent.committee, "refund-v2");
    assert_eq!(sent.seq, 300);
    let said = world
        .lock()
        .unwrap()
        .requests
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::Say {
                committee,
                to,
                body,
                kind,
                ..
            } => Some((committee.clone(), to.clone(), body.clone(), *kind)),
            _ => None,
        });
    let (committee, to, body, kind) = said.expect("a say request");
    assert_eq!(committee.unwrap().as_str(), "refund-v2");
    assert_eq!(
        to,
        vec![senate_core::Address::senator(&common::path("claude"))]
    );
    assert_eq!(body, "Ship it.");
    assert_eq!(kind, senate_bridge::ipc::SayKind::Note);
    assert!(
        matches!(
            core.say("scratch-1".into(), "hi".into()).await,
            Err(CoreError::Invalid { .. })
        ),
        "a session on no committee has nowhere to be spoken to"
    );

    // Pause: stopped and recorded paused.
    let lead = refund.sessions[0].hosted.clone().unwrap();
    core.pause_session(lead.label.clone()).await.unwrap();
    until(&listener, "lead paused", |s| {
        s.groups
            .iter()
            .flat_map(|g| &g.sessions)
            .any(|x| x.session_id == "s-claude" && x.status == SessionStatus::Paused)
    })
    .await;
}

#[tokio::test]
async fn a_settled_thread_reads_with_its_title_without_an_inbox_row() {
    let (dir, socket, world) = setup();
    {
        let mut w = world.lock().unwrap();
        common::busy_senate(&mut w);
        common::session_tree(&mut w);
    }
    let _daemon = FakeDaemon::start(&socket, Arc::clone(&world));
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en-US".into())).unwrap();
    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();
    until(&listener, "online", |s| s.online).await;
    let view = core.thread("refund-v2".into(), 140).await.unwrap();
    assert!(!view.events.is_empty());
    assert_eq!(view.title, "Reviewed the threshold change; fine by me.");
    assert!(!view.kind_label.is_empty());
}

#[tokio::test]
async fn the_default_secretary_is_read_then_ensured_and_a_stopped_one_refused() {
    use senate_app_core::ffi::SecretaryState;
    let (dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, Arc::clone(&world));
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();

    // Read: none yet, and reading changes nothing.
    let none = core.default_secretary().await.unwrap();
    assert_eq!(
        (none.committee.as_str(), none.alias.as_str()),
        ("office", "secretary")
    );
    assert_eq!(none.state, SecretaryState::NotRecorded);
    assert_eq!((none.senator, none.action), (None, None));
    assert_eq!(
        core.default_secretary().await.unwrap().state,
        SecretaryState::NotRecorded
    );

    // Ensure: started, then seated where the sidebar finds it.
    let started = core.ensure_secretary().await.unwrap();
    assert_eq!(started.state, SecretaryState::Running);
    assert_eq!(started.action.as_deref(), Some("started"));
    assert_eq!(started.senator.as_deref(), Some(common::SECRETARY));
    until(&listener, "the secretary's session", |s| {
        s.groups.iter().any(|g| {
            g.sessions
                .iter()
                .any(|l| l.committee.as_deref() == Some("office") && l.display_name == "secretary")
        })
    })
    .await;
    assert_eq!(
        core.ensure_secretary().await.unwrap().action.as_deref(),
        Some("already_running")
    );
    assert!(daemon.requests().contains(&Request::EnsureSecretary));

    // Stopped on purpose: refused, with the daemon's words.
    common::seat_secretary(
        &mut world.lock().unwrap(),
        senate_bridge::ipc::DefaultSecretaryState::Stopped,
    );
    match core.ensure_secretary().await {
        Err(e) => assert!(
            e.to_string()
                .contains("senate session restore office::secretary"),
            "{e}"
        ),
        Ok(s) => panic!("a stopped secretary was ensured: {s:?}"),
    }
}

#[tokio::test]
async fn history_pages_filters_searches_forgets_and_restores() {
    use senate_app_core::history::SearchKind;
    let (dir, socket, world) = setup();
    {
        let mut w = world.lock().unwrap();
        w.sessions = common::hosted_sessions();
        common::busy_senate(&mut w);
        common::session_tree(&mut w);
        common::history(&mut w, 20);
    }
    let daemon = FakeDaemon::start(&socket, Arc::clone(&world));
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();

    // Newest first, a page at a time, to the end.
    let first = core.history(None, None, 8).await.unwrap();
    assert_eq!(first.rows.len(), 8);
    assert!(
        first
            .rows
            .windows(2)
            .all(|w| w[0].last_activity >= w[1].last_activity)
    );
    let mut all = first.rows.clone();
    let mut next = first.next.clone();
    while let Some(cursor) = next {
        let page = core.history(None, Some(cursor), 8).await.unwrap();
        all.extend(page.rows);
        next = page.next;
    }
    assert_eq!(all.len(), 20);
    let gone = all.iter().find(|r| !r.readable).expect("an unreadable one");
    assert!(!gone.restorable);
    let named = &all[0];
    assert!(
        named.label.contains("::"),
        "committee::alias: {}",
        named.label
    );

    // On one committee.
    let payments = core
        .history(Some("payments".into()), None, 50)
        .await
        .unwrap();
    assert!(!payments.rows.is_empty());
    assert!(
        payments
            .rows
            .iter()
            .all(|r| r.committee.as_deref() == Some("payments"))
    );

    // Search: past conversations, and live ones.
    let hits = core
        .search_sessions("ledger migration".into(), None, None, 50)
        .await
        .unwrap();
    assert!(!hits.indexing);
    assert!(
        hits.hits
            .iter()
            .any(|h| !h.live && h.session_id.starts_with("past-")),
        "{hits:?}"
    );
    let hit = &hits.hits[0];
    assert!(
        hit.snippet.to_lowercase().contains("ledger migration"),
        "{}",
        hit.snippet
    );
    let live = core
        .search_sessions("cargo publish".into(), None, None, 50)
        .await
        .unwrap();
    assert!(
        live.hits
            .iter()
            .any(|h| h.live && h.kind == SearchKind::Tool),
        "{live:?}"
    );

    // Forget: the record goes; a live session cannot be forgotten.
    let forgotten = all.iter().find(|r| r.readable).unwrap().session_id.clone();
    core.forget_session(forgotten.clone()).await.unwrap();
    let after = core.history(None, None, 50).await.unwrap();
    assert!(!after.rows.iter().any(|r| r.session_id == forgotten));
    assert!(core.forget_session("s-claude".into()).await.is_err());

    // Restore: it leaves History and runs again, in the sidebar.
    let back = after.rows.iter().find(|r| r.restorable).unwrap().clone();
    core.restore_session(back.label.clone()).await.unwrap();
    until(&listener, "the restored session in the sidebar", |s| {
        s.groups
            .iter()
            .any(|g| g.sessions.iter().any(|l| l.session_id == back.session_id))
    })
    .await;
    let later = core.history(None, None, 50).await.unwrap();
    assert!(!later.rows.iter().any(|r| r.session_id == back.session_id));
    drop(daemon);
}

#[tokio::test]
async fn archived_committees_are_listed_and_unarchived_through_the_facade() {
    let (dir, socket, world) = setup();
    world
        .lock()
        .unwrap()
        .archived
        .push(senate_core::CommitteeId::new("api-test").unwrap());
    let daemon = FakeDaemon::start(&socket, Arc::clone(&world));
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    let listener = Arc::new(Listener::default());
    core.start(listener.clone()).unwrap();

    assert_eq!(core.archived_committees().await.unwrap(), ["api-test"]);
    core.unarchive_committee("api-test".into()).await.unwrap();
    assert!(core.archived_committees().await.unwrap().is_empty());
    let requests = daemon.requests();
    assert!(requests.iter().any(|r| matches!(
        r,
        Request::ArchiveCommittee { committee, archived: false, .. } if committee.as_str() == "api-test"
    )));
    // The roster is read again after.
    let unarchived_at = requests
        .iter()
        .position(|r| matches!(r, Request::ArchiveCommittee { .. }))
        .unwrap();
    assert!(
        requests[unarchived_at..]
            .iter()
            .any(|r| matches!(r, Request::Roster)),
        "{requests:?}"
    );
    assert!(matches!(
        core.unarchive_committee("Not A Name".into()).await,
        Err(senate_app_core::ffi::CoreError::Invalid { .. })
    ));
}

#[tokio::test]
async fn committee_activity_records_are_exposed_through_the_facade() {
    use senate_app_core::activity::{ActivityBatch, ActivityListener};
    struct Activity(tokio::sync::mpsc::UnboundedSender<ActivityBatch>);
    impl ActivityListener for Activity {
        fn on_batch(&self, batch: ActivityBatch) {
            let _ = self.0.send(batch);
        }
        fn on_end(&self, error: Option<String>) {
            assert!(error.is_none(), "{error:?}");
        }
    }

    let (dir, socket, world) = setup();
    let _daemon = FakeDaemon::start(&socket, world);
    let core = AppCore::new(Some(dir.path().display().to_string()), Some("en".into())).unwrap();
    core.start(Arc::new(Listener::default())).unwrap();
    let page = core
        .committee_threads("refund-v2".into(), true, Some(1), None)
        .await
        .unwrap();
    assert_eq!(page.rows[0].other_committee.as_deref(), Some("payments"));
    assert_eq!(page.rows[0].last_author.as_deref(), Some("senator::tester"));
    assert_eq!(page.next_cursor.as_deref(), Some("430"));

    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let id = core
        .watch_activity(
            Some(vec!["refund-v2".into()]),
            [("refund-v2".into(), 501)].into_iter().collect(),
            Arc::new(Activity(send)),
        )
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), receive.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.cursors["refund-v2"], 501);
    let update = tokio::time::timeout(Duration::from_secs(5), receive.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(update.events[0].seq, 502);
    assert_eq!(update.events[0].thread, Some(502));
    core.stop_activity(id);
}
