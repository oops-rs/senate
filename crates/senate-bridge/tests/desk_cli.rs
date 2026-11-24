//! `senate desk`: the Speaker's desk from the terminal, through the real
//! binary and daemon.

#![allow(clippy::unwrap_used)]
#[path = "support/common.rs"]
mod common;

use common::Harness;
use senate_core::{Address, Kind, NewEvent, meta};
use serde_json::json;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_senate");

fn desk(h: &Harness, args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    senate_bridge::testenv::isolate(&mut cmd, h.dir.path());
    cmd.env("HOME", h.dir.path())
        .env("SENATE_HOME", h.paths.home())
        .arg("desk")
        .args(args)
        .output()
        .unwrap()
}

/// `senate desk …` as a person runs it from their own terminal: not
/// inside the test's process tree, where the in-process daemon lives (its
/// person-only guard refuses a caller there). The outer shell exits at
/// once, so the command's parent is reparented away before it connects.
fn decide(h: &Harness, args: &[&str]) -> Output {
    let run = tempfile::tempdir_in(h.dir.path()).unwrap();
    let mut cmd = Command::new("/bin/sh");
    senate_bridge::testenv::isolate(&mut cmd, h.dir.path());
    let status = cmd
        .env("HOME", h.dir.path())
        .env("SENATE_HOME", h.paths.home())
        .current_dir(run.path())
        .arg("-c")
        .arg(r#"( sleep 0.3; "$0" desk "$@" > out 2> err; echo $? > code.tmp; mv code.tmp code ) < /dev/null > /dev/null 2>&1 &"#)
        .arg(BIN)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
    let code = run.path().join("code");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !code.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "senate desk {args:?} never finished"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let code: i32 = std::fs::read_to_string(&code)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    use std::os::unix::process::ExitStatusExt as _;
    Output {
        status: std::process::ExitStatus::from_raw(code << 8),
        stdout: std::fs::read(run.path().join("out")).unwrap(),
        stderr: std::fs::read(run.path().join("err")).unwrap(),
    }
}

fn ok(out: &Output) -> String {
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn failed(out: &Output) -> String {
    assert!(!out.status.success(), "{out:?}");
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn question(body: &str) -> NewEvent {
    NewEvent::new(
        common::committee(),
        Kind::Question,
        Address::senior(&common::name("bob")),
        body,
    )
    .to(Address::Speaker)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_speaker_lists_reads_and_decides_desk_items_from_the_terminal() {
    let mut h = Harness::new().await;
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(Some("senator::alice".parse().unwrap()))
        .unwrap();
    let mut items = Vec::new();
    for body in ["Which quarter does it start?", "Is this still needed?"] {
        let event = h.server.append(question(body));
        h.server.state.inner.lock().unwrap().desk_awaiting.insert(
            (event.committee.to_string(), event.seq),
            senate_core::Awaiting::Addressed,
        );
        items.push(event);
    }
    let crossing = h.server.append(
        NewEvent::new(
            common::committee(),
            Kind::Request,
            Address::senior(&common::name("bob")),
            "Please send the audit to the ledger owner.",
        )
        .to(Address::Speaker)
        .with_meta(meta::INTENDED, json!("other::senator::alice")),
    );
    let refused = h.server.append(
        NewEvent::new(
            common::committee(),
            Kind::Request,
            Address::senior(&common::name("bob")),
            "Please delete the audit log.",
        )
        .to(Address::Speaker)
        .with_meta(meta::INTENDED, json!("other::senator::alice")),
    );
    h.start().await;
    let c = common::committee();
    let name = |seq: senate_core::Seq| format!("{c}#{}", seq.0);

    // The list: every item, as `committee#seq`, who asked and what.
    let list = ok(&desk(&h, &[]));
    for item in [&items[0], &items[1], &crossing, &refused] {
        assert!(list.contains(&name(item.seq)), "{list}");
    }
    assert!(list.contains("Which quarter does it start?"), "{list}");
    assert!(list.contains("for other::senator::alice"), "{list}");

    // One item: its thread and the commands that decide it.
    let show = ok(&desk(&h, &["show", &name(items[0].seq)]));
    assert!(show.contains("Which quarter does it start?"), "{show}");
    assert!(show.contains(&format!("senate desk answer {}", name(items[0].seq))));
    assert!(
        !show.contains("senate desk forward"),
        "only for a crossing: {show}"
    );
    let show = ok(&desk(&h, &["show", &crossing.seq.0.to_string()]));
    assert!(
        show.contains("senate desk forward"),
        "the seq alone: {show}"
    );

    // Answer: in its thread, from the Speaker, to the asker.
    let out = ok(&decide(&h, &["answer", &name(items[0].seq), "From", "Q3."]));
    assert!(out.contains("answered as the Speaker"), "{out}");
    let answer = h
        .server
        .events(c.as_str())
        .into_iter()
        .find(|e| e.thread == Some(items[0].seq))
        .unwrap();
    assert_eq!(
        (answer.kind, answer.from.clone()),
        (Kind::Answer, Address::Speaker)
    );
    assert_eq!(answer.body, "From Q3.");
    assert_eq!(answer.to, [Address::senior(&common::name("bob"))]);
    // Settled: a second decision says so.
    let again = failed(&decide(&h, &["close", &name(items[0].seq)]));
    assert!(again.contains("no longer on the desk"), "{again}");

    // Close, forward, refuse.
    let out = ok(&decide(
        &h,
        &["close", &name(items[1].seq), "--reason", "Settled in chat"],
    ));
    assert!(out.contains("closed"), "{out}");
    let out = ok(&decide(&h, &["forward", &name(crossing.seq)]));
    assert!(out.contains("forwarded to other::senator::alice"), "{out}");
    assert_eq!(h.server.events("other").len(), 1);
    let out = ok(&decide(
        &h,
        &["refuse", &name(refused.seq), "Outside", "policy"],
    ));
    assert!(out.contains("refused"), "{out}");
    assert!(ok(&desk(&h, &["list"])).contains("Nothing awaits the Speaker."));

    // Names that are not items.
    assert!(failed(&desk(&h, &["show", "999"])).contains("no item #999"));
    assert!(failed(&desk(&h, &["answer", "ledger", "x"])).contains("committee#seq"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn someone_not_in_the_seat_reads_the_desk_but_is_told_why_not() {
    let mut h = Harness::new().await;
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .roster
        .name_speaker(Some("senator::bob".parse().unwrap()))
        .unwrap();
    let item = h.server.append(question("Which quarter?"));
    h.server.state.inner.lock().unwrap().desk_awaiting.insert(
        (item.committee.to_string(), item.seq),
        senate_core::Awaiting::Addressed,
    );
    h.start().await;
    let item = format!("{}#{}", item.committee, item.seq.0);
    let show = ok(&desk(&h, &["show", &item]));
    assert!(show.contains("Only the Speaker"), "{show}");
    let refused = failed(&decide(&h, &["answer", &item, "Q3"]));
    assert!(refused.contains("Only the Speaker"), "{refused}");
    h.stop().await;
}
