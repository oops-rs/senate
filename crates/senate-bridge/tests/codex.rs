//! Codex sessions: the app-server adapter against a fake app-server, alone
//! and inside the daemon. No real Codex, no model turns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::fake_codex::{FakeCodex, FakeThread, fake_codex_bin, input_text};
use common::*;
use senate_bridge::SessionState;
use senate_bridge::delivery::codex::{AppServer, CodexError, Pushed, Transport, push_once};
use senate_bridge::hook::HookKind;
use senate_bridge::session::Runtime;
use senate_bridge::state::ItemStatus;
use senate_core::{CommitteeId, Kind, NewEvent};

const PUSH: Duration = Duration::from_secs(5);

fn fake(dir: &tempfile::TempDir) -> FakeCodex {
    FakeCodex::bind(dir.path().join("cx.sock"))
}

fn unix(codex: &FakeCodex) -> Transport {
    Transport::Unix(codex.path.clone())
}

#[tokio::test]
async fn an_idle_thread_gets_a_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let codex = fake(&dir);
    codex.idle("t1");
    let pushed = push_once(&unix(&codex), "t1", "[senate · x] hi", PUSH)
        .await
        .unwrap();
    assert_eq!(
        pushed,
        Pushed::Started {
            turn: "turn-1".into()
        }
    );
    assert_eq!(codex.methods(), ["initialize", "thread/read", "turn/start"]);
    let (_, params) = &codex.pushes()[0];
    assert_eq!(params["threadId"], "t1");
    assert_eq!(params["input"][0]["type"], "text");
    assert_eq!(input_text(params), "[senate · x] hi");
    let init = &codex.calls()[0].1;
    assert_eq!(init["clientInfo"]["name"], "senate");
}

#[tokio::test]
async fn a_busy_thread_is_steered_with_its_active_turn() {
    let dir = tempfile::tempdir().unwrap();
    let codex = fake(&dir);
    codex.busy("t1", "turn-users");
    let pushed = push_once(&unix(&codex), "t1", "steer me", PUSH)
        .await
        .unwrap();
    assert_eq!(
        pushed,
        Pushed::Steered {
            turn: "turn-users".into()
        }
    );
    assert_eq!(
        codex.methods(),
        [
            "initialize",
            "thread/read",
            "thread/turns/list",
            "turn/steer"
        ]
    );
    let (_, params) = &codex.pushes()[0];
    assert_eq!(params["expectedTurnId"], "turn-users");
    assert_eq!(input_text(params), "steer me");
}

#[tokio::test]
async fn a_turn_that_changed_under_the_steer_is_read_again_once() {
    let dir = tempfile::tempdir().unwrap();
    let codex = fake(&dir);
    codex.busy("t1", "turn-a");
    codex.state.lock().unwrap().rotate_steers = 1;
    let mut server = AppServer::connect(&unix(&codex)).await.unwrap();
    let pushed = server.push("t1", "again").await.unwrap();
    let now = codex.active_turn("t1").unwrap();
    assert_ne!(now, "turn-a");
    assert_eq!(pushed, Pushed::Steered { turn: now.clone() });
    let steers: Vec<String> = codex
        .pushes()
        .iter()
        .map(|(_, p)| p["expectedTurnId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(steers, ["turn-a".to_string(), now]);
    assert_eq!(
        codex
            .methods()
            .iter()
            .filter(|m| *m == "thread/read")
            .count(),
        2
    );

    // A steer refused twice is an error, not a loop.
    codex.state.lock().unwrap().rotate_steers = 2;
    let before = codex.pushes().len();
    let err = server.push("t1", "x").await.unwrap_err();
    assert!(
        matches!(&err, CodexError::Rpc { message, .. } if message.contains("expected active turn id")),
        "{err}"
    );
    assert_eq!(codex.pushes().len() - before, 2);
}

#[tokio::test]
async fn a_turn_that_ended_before_the_steer_becomes_a_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let codex = fake(&dir);
    codex.busy("t1", "turn-a");
    let mut server = AppServer::connect(&unix(&codex)).await.unwrap();
    // The bridge reads `active`; the turn ends before it steers.
    assert!(matches!(
        server.thread_state("t1").await.unwrap(),
        senate_bridge::delivery::codex::ThreadState::Active { .. }
    ));
    codex.idle("t1");
    let err = server.turn_steer("t1", "turn-a", "late").await.unwrap_err();
    assert!(err.to_string().contains("no active turn to steer"), "{err}");
    // A full push reads again and starts a turn.
    let pushed = server.push("t1", "late").await.unwrap();
    assert!(matches!(pushed, Pushed::Started { .. }), "{pushed:?}");
}

#[tokio::test]
async fn unloaded_and_unknown_threads_are_not_loaded() {
    let dir = tempfile::tempdir().unwrap();
    let codex = fake(&dir);
    codex.thread(
        "gone",
        FakeThread {
            loaded: false,
            active: None,
        },
    );
    for t in ["gone", "never-seen"] {
        let err = push_once(&unix(&codex), t, "x", PUSH).await.unwrap_err();
        assert!(
            matches!(&err, CodexError::NotLoaded(id) if id == t),
            "{err}"
        );
    }
    assert!(codex.pushes().is_empty());
}

#[tokio::test]
async fn no_app_server_is_a_connect_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = push_once(
        &Transport::Unix(dir.path().join("nobody.sock")),
        "t1",
        "x",
        PUSH,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, CodexError::Connect { .. }), "{err}");
    let Some(down) = fake_codex_bin(dir.path(), None) else {
        eprintln!("skipping the proxy half: no nc on this machine");
        return;
    };
    let err = push_once(&Transport::Proxy { program: down }, "t1", "x", PUSH)
        .await
        .unwrap_err();
    assert!(matches!(err, CodexError::Connect { .. }), "{err}");
}

#[tokio::test]
async fn the_proxy_transport_speaks_through_codex_app_server_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let codex = fake(&dir);
    let Some(bin) = fake_codex_bin(dir.path(), Some(&codex.path)) else {
        eprintln!("skipping: no nc on this machine");
        return;
    };
    codex.idle("t1");
    let pushed = push_once(&Transport::Proxy { program: bin }, "t1", "via proxy", PUSH)
        .await
        .unwrap();
    assert!(matches!(pushed, Pushed::Started { .. }));
    assert_eq!(input_text(&codex.pushes()[0].1), "via proxy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_smoke_reads_then_starts_or_steers() {
    let dir = tempfile::tempdir().unwrap();
    let codex = fake(&dir);
    codex.idle("t1");
    let listen = format!("unix://{}", codex.path.display());
    let smoke = |args: Vec<String>| {
        let home = dir.path().join("senate");
        async move {
            let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_senate"))
                .env("SENATE_HOME", home)
                .arg("codex-smoke")
                .args(args)
                .output()
                .await
                .unwrap();
            (
                out.status.success(),
                String::from_utf8_lossy(&out.stdout).into_owned()
                    + &String::from_utf8_lossy(&out.stderr),
            )
        }
    };
    let args = |extra: &[&str]| {
        let mut a = vec!["--listen".to_string(), listen.clone()];
        a.extend(extra.iter().map(|s| s.to_string()));
        a
    };

    let (ok, out) = smoke(args(&["--thread", "t1"])).await;
    assert!(ok, "{out}");
    assert!(out.contains("thread t1: Idle"), "{out}");
    assert!(out.contains("nothing sent"), "{out}");
    assert!(codex.pushes().is_empty());

    let (ok, out) = smoke(args(&["--thread", "t1", "--text", "hello"])).await;
    assert!(ok, "{out}");
    assert!(out.contains("turn/start: new turn turn-1"), "{out}");

    let (ok, out) = smoke(args(&["--thread", "t1", "--text", "more"])).await;
    assert!(ok, "{out}");
    assert!(out.contains("active turn: turn-1"), "{out}");
    assert!(
        out.contains("turn/steer: 4 chars into turn turn-1"),
        "{out}"
    );

    let (ok, out) = smoke(args(&["--thread", "nope", "--text", "x"])).await;
    assert!(!ok);
    assert!(out.contains("not loaded"), "{out}");
}

// ---- inside the daemon ---------------------------------------------------

/// A directed event reaches a Codex thread as a new turn
/// when it is idle and steers the running turn when it is busy; a Claude
/// session next to it still gets its inbox push.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_starts_then_steers_a_codex_thread() {
    let mut h = Harness::new().await;
    let codex = h.codex();
    h.start().await;
    let wd = h.workdir("cx", Some("refund-v2"));
    let start = h
        .hook_codex(HookKind::SessionStart, "thread-1", &wd)
        .await
        .expect("a Codex session gets orientation through its SessionStart hook");
    assert!(context(&start).contains(
        "joined to the committee `refund-v2` of the Senate [/senate] as [senator::codex]"
    ));
    let s = h.store().session("thread-1").unwrap().unwrap();
    assert_eq!(s.runtime, Runtime::Codex);
    assert_eq!(s.agent, path("codex"));
    h.wait_stream("refund-v2").await;
    codex.idle("thread-1");

    // A Claude session on another committee, pushed through its inbox.
    let inbox = h.inbox("claude");
    let other = h.workdir("cl", Some("other"));
    h.hook(HookKind::SessionStart, "claude-1", &other, Some(&inbox))
        .await;
    h.wait_stream("other").await;

    h.server.append(relayed_to(
        common::codex("alice"),
        "网关幂等用哪个 header？",
    ));
    let pushes = eventually("a turn/start", WAIT, || {
        let p = codex.pushes();
        (!p.is_empty()).then_some(p)
    })
    .await;
    let (method, params) = &pushes[0];
    assert_eq!(method, "turn/start");
    assert_eq!(params["threadId"], "thread-1");
    let text = input_text(params);
    assert!(text.contains("[senate · refund-v2 · seq 1"), "{text}");
    assert!(text.contains("网关幂等用哪个 header？"), "{text}");
    eventually("seq 1 delivered", WAIT, || {
        let i = h.item_of("codex", 1)?;
        (i.status == ItemStatus::Delivered).then_some(())
    })
    .await;
    let turn = codex.active_turn("thread-1").unwrap();

    // Bob's agent is busy with that turn: the next one steers it.
    h.server
        .append(relayed_to(common::codex("alice"), "还有一个问题"));
    let pushes = eventually("a turn/steer", WAIT, || {
        let p = codex.pushes();
        (p.len() >= 2).then_some(p)
    })
    .await;
    let (method, params) = &pushes[1];
    assert_eq!(method, "turn/steer");
    assert_eq!(params["expectedTurnId"], turn.as_str());
    assert!(input_text(params).contains("还有一个问题"));

    // The Claude session is untouched by the Codex adapter.
    h.server.append(
        NewEvent::new(
            CommitteeId::new("other").unwrap(),
            Kind::Question,
            claude("bob"),
            "for claude",
        )
        .to(claude("alice")),
    );
    let msgs = inbox.wait_messages(1).await;
    assert!(msgs[0].contains("for claude"), "{msgs:?}");
    assert!(
        codex
            .pushes()
            .iter()
            .all(|(_, p)| !input_text(p).contains("for claude"))
    );
    assert_eq!(codex.pushes().len(), 2);
    h.stop().await;
}

/// No app-server: the batch is held for the thread (fail closed). A hook
/// does not hand it over as context, which Codex would leave unanswered;
/// it retries the push, and once the app-server is there the batch arrives
/// as a turn, once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_an_app_server_items_stay_held_until_a_push_lands() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("cx", Some("refund-v2"));
    h.hook_codex(HookKind::SessionStart, "thread-1", &wd).await;
    h.wait_stream("refund-v2").await;

    h.server
        .append(relayed_to(common::codex("alice"), "anyone there?"));
    let held = || {
        let i = h.item_of("codex", 1)?;
        (i.status == ItemStatus::Queued).then_some(())
    };
    eventually("held", WAIT, held).await;
    assert_eq!(
        h.store().session("thread-1").unwrap().unwrap().state,
        SessionState::Live,
        "a missing app-server says nothing about the thread"
    );
    let out = h
        .hook_codex(HookKind::UserPromptSubmit, "thread-1", &wd)
        .await;
    assert!(
        out.as_ref()
            .is_none_or(|o| !context(o).contains("anyone there?")),
        "{out:?}"
    );
    // The hook's retry fails too: the item is still held, not lost.
    tokio::time::sleep(Duration::from_millis(600)).await;
    eventually("still held", WAIT, held).await;

    let codex = h.codex();
    codex.busy("thread-1", "turn-users");
    h.hook_codex(HookKind::UserPromptSubmit, "thread-1", &wd)
        .await;
    let pushes = eventually("a turn/steer", WAIT, || {
        let p = codex.pushes();
        (!p.is_empty()).then_some(p)
    })
    .await;
    assert_eq!(pushes[0].0, "turn/steer");
    assert!(input_text(&pushes[0].1).contains("anyone there?"));
    eventually("delivered", WAIT, || {
        let i = h.item_of("codex", 1)?;
        (i.status == ItemStatus::Delivered).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(codex.pushes().len(), 1);
    h.stop().await;
}

/// The thread is no longer loaded (its TUI quit): the session goes stale
/// and the item waits until the thread is resumed, then arrives as a turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unloaded_thread_goes_stale_until_it_is_resumed() {
    let mut h = Harness::new().await;
    let codex = h.codex();
    h.start().await;
    let wd = h.workdir("cx", Some("refund-v2"));
    h.hook_codex(HookKind::SessionStart, "thread-1", &wd).await;
    h.wait_stream("refund-v2").await;
    codex.thread(
        "thread-1",
        FakeThread {
            loaded: false,
            active: None,
        },
    );

    h.server
        .append(relayed_to(common::codex("alice"), "still there?"));
    eventually("thread-1 stale", WAIT, || {
        let s = h.store().session("thread-1").unwrap()?;
        (s.state == SessionState::Stale).then_some(())
    })
    .await;
    assert_eq!(h.item_of("codex", 1).unwrap().status, ItemStatus::Pending);
    assert!(codex.pushes().is_empty());

    codex.idle("thread-1");
    let out = h
        .hook_codex(HookKind::SessionStart, "thread-1", &wd)
        .await
        .unwrap();
    // The brief lists it as an open question; the message itself is not here.
    assert!(pos(context(&out), 1).is_none(), "{}", context(&out));
    let pushes = eventually("a turn/start", WAIT, || {
        let p = codex.pushes();
        (!p.is_empty()).then_some(p)
    })
    .await;
    assert_eq!(pushes[0].0, "turn/start");
    assert!(input_text(&pushes[0].1).contains("still there?"));
    h.stop().await;
}

/// Items that waited for a Codex session before it registered
/// (Codex was closed when the question came) are pushed through the
/// app-server as a turn after the user's prompt, not handed to the
/// UserPromptSubmit hook as context Codex would leave unanswered. Once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_backlog_from_before_the_session_arrives_as_a_turn_not_hook_context() {
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    let codex = h.codex();
    h.start().await;
    // An earlier Codex session, now closed, puts codex on record, so
    // the configured committee stays open for it.
    let wd = h.workdir("cx", Some("refund-v2"));
    h.hook_codex(HookKind::SessionStart, "thread-0", &wd).await;
    h.hook_codex(HookKind::SessionEnd, "thread-0", &wd).await;
    h.wait_stream_of("refund-v2", "codex").await;
    h.server.append(relayed_to(
        common::codex("alice"),
        "网关幂等用哪个 header？",
    ));
    eventually("pending with no session", WAIT, || {
        let i = h.item_of("codex", 1)?;
        (i.status == ItemStatus::Pending).then_some(())
    })
    .await;

    // The user opens Codex and types: the hook runs inside their turn.
    codex.busy("thread-1", "turn-users");
    let out = h
        .hook_codex(HookKind::UserPromptSubmit, "thread-1", &wd)
        .await;
    assert!(
        out.as_ref()
            .is_none_or(|o| !context(o).contains("网关幂等用哪个 header？")),
        "the backlog must not ride on the hook: {out:?}"
    );
    let pushes = eventually("a push through the app-server", WAIT, || {
        let p = codex.pushes();
        (!p.is_empty()).then_some(p)
    })
    .await;
    let (method, params) = &pushes[0];
    assert_eq!(method, "turn/steer");
    assert_eq!(params["threadId"], "thread-1");
    assert_eq!(params["expectedTurnId"], "turn-users");
    let text = input_text(params);
    assert!(text.contains("[senate · refund-v2 · seq 1"), "{text}");
    assert!(text.contains("网关幂等用哪个 header？"), "{text}");
    eventually("seq 1 delivered", WAIT, || {
        let i = h.item_of("codex", 1)?;
        (i.status == ItemStatus::Delivered && i.session_id.as_deref() == Some("thread-1"))
            .then_some(())
    })
    .await;

    // A later hook neither hands it over nor pushes it again.
    let out = h
        .hook_codex(HookKind::UserPromptSubmit, "thread-1", &wd)
        .await;
    assert!(
        out.as_ref()
            .is_none_or(|o| !context(o).contains("网关幂等用哪个 header？"))
    );
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(codex.pushes().len(), 1);
    h.stop().await;
}
