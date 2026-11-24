//! A fake daemon with canned threads for `claude` and `codex`, a Codex
//! thread, an approval, and the docket of the one Senate (`alice-desk`),
//! where `claude` presides, for working on the native app without a Senate
//! server. Its Senate is a busy one ([`common::busy_senate`]): four
//! committees and their chairs, agents under aliases beside long-gone ones,
//! a request and an escalation waiting on alice, and tasks her agents are on:
//!
//! ```sh
//! mkdir -p /tmp/sh && chmod 700 /tmp/sh
//! SENATE_HOME=/tmp/sh cargo run -p senate-app-core --example fake_daemon &
//! SENATE_HOME=/tmp/sh SENATE_APP_SHOW=1 apps/macos/build/Build/Products/Debug/Senate.app/Contents/MacOS/Senate
//! ```
//!
//! Every 20 s it asks for one more approval (`FAKE_APPROVAL_EVERY=<s>`,
//! 0 for never). Ctrl-C stops it.
//! `FAKE_THREADS=2000` adds that many threads (and a 5000-event thread 990,
//! `FAKE_EVENTS` to change it), to measure the app with a large inbox. It only ever
//! writes under `SENATE_HOME`, which this example requires to be set.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../tests/support/common.rs"]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use senate_bridge::Paths;
use senate_bridge::ipc::WatchEvent;

#[tokio::main]
async fn main() {
    assert!(
        std::env::var_os(senate_bridge::paths::HOME_ENV).is_some_and(|v| !v.is_empty()),
        "set SENATE_HOME to a scratch directory: this fake must not run in the real home"
    );
    let paths = Paths::from_env().expect("a Senate home");
    paths.ensure_home().expect("creating the Senate home");
    let socket = paths.socket();
    let mut w = common::World::new();
    w.sessions = common::hosted_sessions();
    // FAKE_THREADS=N: N more threads waiting on alice, and thread 990 a
    // long one (FAKE_EVENTS events, default 5000), to measure the app.
    if let Some(n) = std::env::var("FAKE_THREADS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        let events = std::env::var("FAKE_EVENTS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5000);
        common::scale(&mut w, n, events);
        println!("{n} more threads; thread 990 has {events} events");
    }
    w.status.roles = Some(common::roles());
    common::busy_senate(&mut w);
    // Every session state on every committee, and their timelines, which
    // grow while watched (`FAKE_TIMELINE_EVERY=<s>`, 0 for never).
    common::session_tree(&mut w);
    let grow = std::env::var("FAKE_TIMELINE_EVERY")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(6);
    w.grow_every = (grow > 0).then(|| Duration::from_secs(grow));
    // FAKE_TIMELINE_EVERY_MS=<ms>: grow that often instead (a busy session).
    if let Some(ms) = std::env::var("FAKE_TIMELINE_EVERY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        w.grow_every = (ms > 0).then(|| Duration::from_millis(ms));
    }
    // FAKE_BIG=N: one more session, `marathon`, with N items shaped like a
    // long chair session, growing while watched (`SENATE_APP_PERF=session`).
    if let Some(n) = std::env::var("FAKE_BIG")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        common::marathon(&mut w, n);
        println!("marathon-1: {n} items");
    }
    // FAKE_WORKING=N: N more sessions, all working (the sidebar's marks).
    if let Some(n) = std::env::var("FAKE_WORKING")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
    {
        common::working_sessions(&mut w, n);
        println!("{n} working sessions");
    }
    // FAKE_STREAM=1: one more session, `stream`, whose reply arrives as a
    // token stream while watched.
    if std::env::var("FAKE_STREAM").is_ok_and(|v| v == "1") {
        common::streaming(&mut w);
        println!("stream-1: a streamed reply");
    }
    // FAKE_SECRETARY=<state>: the default secretary already there
    // (running, paused, ended, stopped, elsewhere); none by default.
    if let Ok(v) = std::env::var("FAKE_SECRETARY") {
        use senate_bridge::ipc::DefaultSecretaryState as St;
        let state = match v.as_str() {
            "running" => St::Running,
            "paused" => St::Paused,
            "ended" => St::Ended,
            "stopped" => St::Stopped,
            "elsewhere" => St::Elsewhere,
            _ => St::NotRecorded,
        };
        common::seat_secretary(&mut w, state);
    }
    // FAKE_HISTORY=N: ended and stopped sessions in History (36 by default).
    let history = std::env::var("FAKE_HISTORY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(36);
    common::history(&mut w, history);
    let world = Arc::new(Mutex::new(w));
    let daemon = common::FakeDaemon::start(&socket, Arc::clone(&world));
    println!("fake daemon on {}", socket.display());
    // FAKE_APPROVAL_EVERY=<s>: how often another approval arrives (0:
    // never, which the UI tests use so the lists hold still).
    let every = std::env::var("FAKE_APPROVAL_EVERY")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20);
    let mut n = 8;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = tokio::time::sleep(Duration::from_secs(every.max(1))), if every > 0 => {
                let a = common::approval(&format!("019a-thread:{n}"));
                n += 1;
                world.lock().unwrap().approvals.push(a.clone());
                daemon.emit(WatchEvent::ApprovalRequested(Box::new(a)));
            }
        }
    }
    for r in daemon.requests() {
        println!("{}", serde_json::to_string(&r).unwrap());
    }
}
