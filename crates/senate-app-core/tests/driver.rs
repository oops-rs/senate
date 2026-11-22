//! The app's driver against a fake daemon on a temp unix socket: the `watch`
//! stream, reconnecting, and the inbox / take / close / approve / docket
//! round trips, all in the bridge's own wire types.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use senate_app_core::AppState;
use senate_app_core::link::{Backoff, Driver, Link, Sink};
use senate_app_core::model::{Action, Failure, Model};
use senate_bridge::approvals::{Approval, Decision, ResolvedBy};
use senate_bridge::ipc::{Request, WatchEvent};
use senate_core::{Locale, Seq, StatusState};

use common::{FakeDaemon, World, alice, approval, committee, path, setup, suspended};

#[derive(Default, Clone)]
struct Recorder {
    notified: Arc<Mutex<Vec<String>>>,
}

impl Sink for Recorder {
    fn changed(&self, _model: &Model) {}
    fn approval_arrived(&self, approval: &Approval) {
        self.notified.lock().unwrap().push(approval.id.clone());
    }
}

const FAST: Backoff = Backoff {
    min: Duration::from_millis(20),
    max: Duration::from_millis(100),
};

fn driver(socket: &std::path::Path, locale: Locale) -> (Arc<Driver<Recorder>>, Recorder) {
    let sink = Recorder::default();
    let driver = Driver::new(Link::new(socket), sink.clone(), FAST, locale);
    tokio::spawn(Arc::clone(&driver).run());
    (driver, sink)
}

fn state<S: Sink>(driver: &Driver<S>) -> AppState {
    AppState::from_model(&driver.model(), Utc::now())
}

async fn until<S: Sink>(
    driver: &Driver<S>,
    what: &str,
    mut ok: impl FnMut(&AppState) -> bool,
) -> AppState {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let s = state(driver);
        if ok(&s) {
            return s;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}; last state: {s:#?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn offline_then_online_with_every_part_of_the_model() {
    let (_dir, socket, world) = setup();
    let (driver, sink) = driver(&socket, Locale::En);

    let s = until(&driver, "offline with a reason", |s| {
        !s.online && s.offline_reason.is_some()
    })
    .await;
    assert_eq!(s.menu.title, "Senate: offline");
    assert!(s.offline_reason.unwrap().starts_with("cannot connect to "));
    assert!(s.senates.is_empty() && s.agents.is_empty());

    let _daemon = FakeDaemon::start(&socket, world);
    let s = until(&driver, "inbox, approvals and docket", |s| {
        s.online && s.inbox.len() == 2 && s.approvals.len() == 1 && s.counts.docket == 2
    })
    .await;
    assert_eq!(s.person.as_deref(), Some("alice"));

    // The one Senate the daemon serves, who the person is in it, and the
    // agent of hers presiding.
    assert_eq!(s.senates.len(), 1);
    let senate = &s.senates[0];
    assert_eq!(senate.key, "alice-desk");
    assert_eq!(senate.senate.as_deref(), Some("alice-desk"));
    assert!(senate.primary);
    assert_eq!(senate.me, "[senator::alice]");
    assert_eq!(senate.title, "Senate alice-desk");
    assert_eq!(senate.speaker.as_deref(), Some("[senator::claude]"));
    assert_eq!(senate.presiding_agent.as_deref(), Some("claude"));
    assert_eq!(senate.committees[0].stream, "live");

    // The person's agents, with what waits on each: those with a stream on
    // this machine (not `reviewer` or `tester`, which only the docket names).
    let agents: Vec<_> = s
        .agents
        .iter()
        .map(|a| {
            (
                a.path.as_str(),
                a.senate.as_str(),
                a.pending,
                a.presiding,
                a.inbox,
            )
        })
        .collect();
    assert_eq!(
        agents,
        vec![
            ("claude", "alice-desk", 1, true, true),
            ("codex", "alice-desk", 1, false, true),
        ]
    );
    assert_eq!(s.agents[1].runtime.as_deref(), Some("codex"));

    // The merged inbox lists each thread once, with its mentions bracketed.
    assert_eq!(s.inbox[0].from, "[speaker::copilot]");
    assert_eq!(s.inbox[0].for_agents, vec!["claude".to_string()]);
    assert_eq!(s.inbox[1].for_agents, vec!["codex".to_string()]);
    assert_eq!(s.inbox[0].state, "suspended");
    assert_eq!(s.inbox[0].triage_label.as_deref(), Some("needs a person"));

    // The Codex panel and its approval.
    assert_eq!(s.codex.len(), 1);
    assert_eq!(s.codex[0].agent, "codex");
    assert_eq!(s.codex[0].approvals, 1);
    assert_eq!(
        *sink.notified.lock().unwrap(),
        vec!["019a-thread:7".to_string()]
    );

    // The docket of the Senate claude presides over, read-only.
    assert_eq!(s.docket.len(), 1);
    let d = &s.docket[0];
    assert_eq!(d.senate, "alice-desk");
    assert_eq!(d.title, "Senate alice-desk");
    assert_eq!(d.speaker.as_deref(), Some("[senator::claude]"));
    assert_eq!(d.committees[0].items.len(), 2);
    let pick = &d.committees[0].items[0];
    assert_eq!(pick.awaiting, "pick");
    assert_eq!(pick.by, "[senator::alice]");
    assert!(pick.why.starts_with("route to one of [senator::reviewer]"));
    assert_eq!(d.committees[0].items[1].awaiting, "away");
    assert_eq!(
        d.committees[0].seats[0].checkpoint.as_deref(),
        Some("lint config moved to the repo root")
    );

    // The menu: threads and approvals wait on the person, the docket on the
    // presiding agent.
    assert_eq!(s.menu.title, "Senate: online as alice");
    assert_eq!(s.menu.pending, "2 pending threads, 1 approval");
    assert_eq!(s.menu.badge.as_deref(), Some("3"));
    assert_eq!(
        s.menu.docket.as_deref(),
        Some("Docket: 2 awaiting the Speaker")
    );
    assert_eq!(s.menu.lines[0], "claude · refund-v2 · live · seq 120");
    assert_eq!(s.counts.pending, 3);
}

#[tokio::test]
async fn the_docket_is_read_only_and_only_asked_for_while_presiding() {
    let (_dir, socket, world) = setup();
    world.lock().unwrap().status = common::status_alone(vec![]);
    let daemon = FakeDaemon::start(&socket, world.clone());
    let (driver, _) = driver(&socket, Locale::En);
    let s = until(&driver, "online", |s| s.online && s.inbox.len() == 2).await;
    assert!(s.docket.is_empty());
    assert_eq!(s.menu.docket, None);
    assert_eq!(s.senates.len(), 1);

    // Nobody presides: no docket is ever requested.
    assert!(
        !daemon
            .requests()
            .iter()
            .any(|r| matches!(r, Request::Docket { .. }))
    );

    // Whatever the app does, it never routes, covers or refers.
    driver
        .act(Action::Answer {
            committee: committee(),
            thread: Seq(118),
            body: "ok".into(),
        })
        .await
        .unwrap();
    assert!(daemon.requests().iter().all(|r| !matches!(
        r,
        Request::Route { .. } | Request::Cover { .. } | Request::Refer { .. }
    )));
}

#[tokio::test]
async fn an_escalated_thread_is_listed_once_and_belongs_to_every_agent() {
    let (_dir, socket, world) = setup();
    {
        let mut w = world.lock().unwrap();
        let mut t = suspended(150, "ship it?", "claude");
        t.escalated_to = Some(alice());
        w.threads = vec![t];
    }
    let _daemon = FakeDaemon::start(&socket, world);
    let (driver, _) = driver(&socket, Locale::En);
    let s = until(&driver, "the escalated thread", |s| s.inbox.len() == 1).await;
    let t = &s.inbox[0];
    assert!(t.escalated);
    assert_eq!(t.escalated_to.as_deref(), Some("[senator::alice]"));
    assert_eq!(t.for_agents, vec!["claude".to_string()]);
    // It waits on the person whichever agent it was for: both agents count it.
    let pending: Vec<_> = s.agents.iter().map(|a| a.pending).collect();
    assert_eq!(&pending[..2], &[1, 1]);
    assert_eq!(s.counts.threads, 1);
}

#[tokio::test]
async fn reply_take_and_close_round_trip() {
    let (_dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, world);
    let (driver, _) = driver(&socket, Locale::En);
    until(&driver, "inbox", |s| s.inbox.len() == 2).await;

    // Reply to the suspended thread, as the person.
    let posted = driver
        .act(Action::Answer {
            committee: committee(),
            thread: Seq(118),
            body: "走审批。".into(),
        })
        .await
        .unwrap();
    assert_eq!(posted["thread"], 118);
    // The daemon still lists it, `active`; it leaves the list and the
    // counts together.
    let s = until(&driver, "answered thread gone", |s| s.inbox.len() == 1).await;
    assert_eq!(s.inbox[0].root, 131);
    assert_eq!(s.menu.pending, "1 pending thread, 1 approval");
    assert_eq!(s.menu.badge.as_deref(), Some("2"));
    assert!(daemon.requests().iter().any(|r| matches!(
        r,
        Request::Answer { thread: Seq(118), body, committee: Some(c), as_person: true, agent: None, .. }
            if body == "走审批。" && *c == committee()
    )));

    // Take it, then close it.
    driver
        .act(Action::Take {
            committee: committee(),
            thread: Seq(131),
        })
        .await
        .unwrap();
    until(&driver, "taken", |s| {
        s.inbox[0].takeover.as_deref() == Some("[senator::alice]")
    })
    .await;
    driver
        .act(Action::Close {
            committee: committee(),
            thread: Seq(131),
        })
        .await
        .unwrap();
    until(&driver, "closed", |s| s.inbox.is_empty()).await;
    assert!(daemon.requests().iter().any(|r| matches!(
        r,
        Request::Task { thread: Seq(131), status, as_person: true, committee: Some(_), .. }
            if status.state == StatusState::Archived
    )));
    assert_eq!(state(&driver).last_error, None);

    // An empty reply never reaches the daemon, and is worded in the language.
    let before = daemon.requests().len();
    let err = driver
        .act(Action::Answer {
            committee: committee(),
            thread: Seq(1),
            body: "  ".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(err, Failure::EmptyReply);
    assert_eq!(daemon.requests().len(), before);
}

#[tokio::test]
async fn deny_wins_once_and_resolutions_elsewhere_clear_it() {
    let (_dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, world.clone());
    let (driver, _) = driver(&socket, Locale::En);
    until(&driver, "approval", |s| s.approvals.len() == 1).await;

    let reply = driver
        .act(Action::Approve {
            id: "019a-thread:7".into(),
            decision: Decision::Deny,
        })
        .await
        .unwrap();
    assert_eq!(reply["decision"], "deny");
    until(&driver, "approval gone", |s| s.approvals.is_empty()).await;

    // A second answer loses, and says why, in the daemon's words.
    let err = driver
        .act(Action::Approve {
            id: "019a-thread:7".into(),
            decision: Decision::Allow,
        })
        .await
        .unwrap_err();
    let Failure::Daemon(message) = &err else {
        panic!("{err:?}")
    };
    assert!(message.contains("already answered"), "{message}");
    assert_eq!(state(&driver).last_error.as_deref(), Some(message.as_str()));

    // One answered in the TUI disappears from the app.
    let next = approval("019a-thread:9");
    world.lock().unwrap().approvals.push(next.clone());
    daemon.emit(WatchEvent::ApprovalRequested(Box::new(next)));
    until(&driver, "new approval", |s| s.approvals.len() == 1).await;
    world.lock().unwrap().approvals.clear();
    daemon.emit(WatchEvent::ApprovalResolved {
        id: "019a-thread:9".into(),
        session_id: "019a-thread".into(),
        by: ResolvedBy::Elsewhere,
        decision: None,
    });
    until(&driver, "resolved elsewhere", |s| s.approvals.is_empty()).await;
}

#[tokio::test]
async fn reconnects_after_the_daemon_restarts() {
    let (_dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, world.clone());
    let (driver, _) = driver(&socket, Locale::En);
    until(&driver, "online", |s| s.online && s.approvals.len() == 1).await;

    daemon.stop();
    let s = until(&driver, "offline", |s| !s.online).await;
    assert!(s.approvals.is_empty(), "approvals are not shown offline");
    assert!(s.inbox.is_empty() && s.docket.is_empty() && s.agents.is_empty());
    assert_eq!(
        s.menu.badge, None,
        "nothing is claimed to wait while offline"
    );

    world.lock().unwrap().approvals.clear();
    let _again = FakeDaemon::start(&socket, world);
    let s = until(&driver, "online again", |s| s.online && s.inbox.len() == 2).await;
    assert!(s.approvals.is_empty());
    assert_eq!(s.docket.len(), 1);
}

#[tokio::test]
async fn an_inbox_event_rereads_that_agents_inbox_and_the_docket() {
    let (_dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, world.clone());
    let (driver, _) = driver(&socket, Locale::En);
    until(&driver, "inbox", |s| s.inbox.len() == 2).await;

    world
        .lock()
        .unwrap()
        .threads
        .push(suspended(160, "new one", "codex"));
    daemon.emit(WatchEvent::Inbox {
        agent: path("codex"),
        committee: committee(),
        seq: Seq(160),
        kind: senate_core::Kind::Question,
    });
    until(&driver, "the new thread", |s| s.inbox.len() == 3).await;

    // An event names an agent with no stream on this machine, whose inbox
    // the app cannot read: no inbox request for it, but the docket is read
    // again.
    let before = daemon.requests().len();
    daemon.emit(WatchEvent::Inbox {
        agent: path("reviewer"),
        committee: committee(),
        seq: Seq(161),
        kind: senate_core::Kind::Question,
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    let after: Vec<_> = daemon.requests().split_off(before);
    assert!(
        after.iter().all(|r| matches!(r, Request::Docket { .. })),
        "{after:?}"
    );
    assert!(!after.is_empty());
}

#[tokio::test]
async fn lagged_rereads_approvals() {
    let (_dir, socket, world) = setup();
    let daemon = FakeDaemon::start(&socket, world.clone());
    let (driver, _) = driver(&socket, Locale::En);
    until(&driver, "approval", |s| s.approvals.len() == 1).await;

    world
        .lock()
        .unwrap()
        .approvals
        .push(approval("019a-thread:8"));
    daemon.emit(WatchEvent::Lagged { missed: 3 });
    until(&driver, "both approvals", |s| s.approvals.len() == 2).await;
}

#[tokio::test]
async fn the_language_can_change_while_running() {
    let (_dir, socket, world) = setup();
    let _daemon = FakeDaemon::start(&socket, world);
    let (driver, _) = driver(&socket, Locale::En);
    until(&driver, "inbox, approval and docket", |s| {
        s.online && s.inbox.len() == 2 && s.approvals.len() == 1 && s.counts.docket == 2
    })
    .await;

    driver.set_locale(Locale::Zh);
    let s = state(&driver);
    assert_eq!(s.menu.title, "Senate：已作为 alice 在线");
    assert_eq!(s.menu.pending, "2 个待处理线程，1 个审批");
    assert_eq!(s.menu.docket.as_deref(), Some("议程：2 项等待 Speaker"));
    assert_eq!(
        s.senates[0].title, "Senate alice-desk",
        "Senate and its path read the same in every language"
    );
    assert_eq!(s.inbox[0].state_label, "等待中");
    assert_eq!(
        s.inbox[0].from, "[speaker::copilot]",
        "ids read the same in every language"
    );
    assert!(
        s.docket[0].committees[0].items[0]
            .why
            .starts_with("要在 [senator::reviewer]")
    );
    assert_eq!(
        s.docket[0].committees[0].items[0].body, "谁来看一下 [senator::reviewer] 的补丁？",
        "what people wrote is shown as written"
    );
}

#[test]
fn world_is_the_one_the_tests_describe() {
    let w = World::new();
    assert_eq!(
        w.status.senate.as_ref().map(ToString::to_string).as_deref(),
        Some("alice-desk")
    );
    assert!(w.status.people.is_person(&path("alice")));
    assert!(!w.status.people.is_person(&path("claude")));
    assert_eq!(
        w.status.speaker,
        Some(senate_core::SpeakerSeat::Senator(path("claude")))
    );
    assert_eq!(w.threads.len(), 2);
}

#[tokio::test]
async fn reconnect_now_cuts_a_long_wait_short() {
    let (_dir, socket, world) = setup();
    // A minute between tries: only reconnect_now gets it online in time.
    let slow = Backoff {
        min: Duration::from_secs(60),
        max: Duration::from_secs(60),
    };
    let driver = Driver::new(Link::new(&socket), Recorder::default(), slow, Locale::En);
    tokio::spawn(Arc::clone(&driver).run());
    until(&driver, "offline", |s| {
        !s.online && s.offline_reason.is_some()
    })
    .await;
    let _daemon = FakeDaemon::start(&socket, world);
    driver.reconnect_now();
    until(&driver, "online at once", |s| s.online).await;
}
