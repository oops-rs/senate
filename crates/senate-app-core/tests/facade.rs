//! The FFI facade, from Rust, against the fake daemon: what a native app
//! sees through [`CoreListener`] and what its calls do.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use senate_app_core::{
    AppCore, AppState, ApprovalDecision, ApprovalRequest, CoreError, CoreListener,
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
    assert_eq!(s.menu.badge.as_deref(), Some("3"));
    assert_eq!(s.counts.pending, 3);
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
    assert_eq!(s.counts.pending, 2);
    assert_eq!(s.menu.badge.as_deref(), Some("2"));
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
