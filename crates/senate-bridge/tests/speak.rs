//! The person's own commands (`senate ask`, `tell`, `request`, `reply`,
//! `thread`): the real binary against a real daemon and the fake server.
//! Every spawn gets a temp HOME and SENATE_HOME.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use common::*;
use senate_bridge::hook::HookKind;
use senate_core::{Address, Kind, Locale, NewEvent, Seq};
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_senate");

/// `senate <args>` on `h`'s daemon, with `stdin` when given.
async fn senate(h: &Harness, args: &[&str], stdin: Option<&str>, cwd: Option<&Path>) -> Output {
    let mut cmd = Command::new(BIN);
    for var in [
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_PID",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "RUST_LOG",
        "SENATE_AGENT",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("HOME", h.dir.path())
        .env("SENATE_HOME", h.paths.home())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let stdin = stdin.unwrap_or_default().to_string();
    tokio::task::spawn_blocking(move || {
        let mut child = cmd.spawn().unwrap();
        let mut input = child.stdin.take().unwrap();
        // The command may exit before reading it.
        let _ = input.write_all(stdin.as_bytes());
        drop(input);
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap()
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A daemon with alice's Claude session registered on refund-v2, so
/// `claude` is one of her agents seated there.
async fn machine(zh: bool) -> Harness {
    let mut h = Harness::new().await;
    if zh {
        h.config.locale = Locale::Zh;
    }
    h.start().await;
    let wd = h.workdir("s", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    h.wait_stream("refund-v2").await;
    h
}

fn last(h: &Harness) -> senate_core::Event {
    h.server.last_event("refund-v2")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_names_an_agent_relative_to_you_and_says_where_it_went() {
    let h = machine(false).await;
    let o = senate(
        &h,
        &[
            "ask",
            "claude",
            "which",
            "header?",
            "--committee",
            "refund-v2",
        ],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    let sent = last(&h);
    assert_eq!(sent.kind, Kind::Question);
    assert_eq!(
        sent.from,
        person("alice"),
        "as the person, never as an agent"
    );
    assert_eq!(sent.to, [claude("alice")]);
    assert_eq!(sent.body, "which header?");
    assert_eq!(
        out(&o).lines().next().unwrap(),
        format!(
            "Sent a question on refund-v2: thread {}, to [senator::alice/claude].",
            sent.seq
        )
    );
    assert!(
        out(&o).contains(&format!("`senate thread {}`", sent.seq)),
        "{}",
        out(&o)
    );
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Answer,
            claude("alice"),
            "Idempotency-Key",
        )
        .to(person("alice"))
        .in_thread(sent.seq),
    );
    let t = senate(
        &h,
        &["thread", &sent.seq.to_string(), "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(t.status.success(), "{}", err(&t));
    let text = out(&t);
    assert!(
        text.starts_with(&format!("Thread {} on refund-v2, 2 event(s):", sent.seq)),
        "{text}"
    );
    assert!(
        text.contains("[senator::alice] · ")
            && text.contains(" UTC · question · to [senator::alice/claude]"),
        "{text}"
    );
    assert!(
        text.contains("[senator::alice/claude] · ") && text.contains(" · answer"),
        "{text}"
    );
    assert!(
        text.contains("\n  which header?") && text.contains("\n  Idempotency-Key"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_committee_defaults_to_the_one_of_the_working_directory() {
    let h = machine(false).await;
    let wd = h.workdir("s", Some("refund-v2"));
    let o = senate(&h, &["tell", "claude", "heads up"], None, Some(&wd)).await;
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(last(&h).kind, Kind::Note);
    // None here: no committee to speak on.
    let elsewhere = h.workdir("none", None);
    let o = senate(&h, &["tell", "claude", "x"], None, Some(&elsewhere)).await;
    assert!(!o.status.success());
    assert!(err(&o).contains("--committee"), "{}", err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tell_is_a_note_and_request_is_a_request() {
    let h = machine(false).await;
    let o = senate(
        &h,
        &[
            "tell",
            "alice/claude",
            "the",
            "retry",
            "is",
            "fixed",
            "--committee",
            "refund-v2",
        ],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    let note = last(&h);
    assert_eq!(
        (note.kind, note.from.clone()),
        (Kind::Note, person("alice"))
    );
    assert_eq!(note.to, [claude("alice")]);
    assert_eq!(
        out(&o).trim(),
        format!(
            "Sent a note on refund-v2 (seq {}) to [senator::alice/claude].",
            note.seq
        )
    );

    let o = senate(
        &h,
        &[
            "request",
            "bob/claude",
            "bump",
            "the",
            "SDK",
            "--committee",
            "refund-v2",
        ],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    let req = last(&h);
    assert_eq!(
        (req.kind, req.from.clone()),
        (Kind::Request, person("alice"))
    );
    assert_eq!(req.to, [claude("bob")]);
    assert!(
        out(&o).starts_with("Sent a request on refund-v2: thread "),
        "{}",
        out(&o)
    );

    // A person is a person.
    let o = senate(
        &h,
        &["ask", "bob", "lunch?", "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(last(&h).to, [person("bob")]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whichever_agent_resolves_and_says_what_it_resolved_to() {
    let h = machine(false).await;
    let o = senate(
        &h,
        &["ask", "*", "anyone?", "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(last(&h).to, [claude("alice")]);
    assert!(
        out(&o)
            .lines()
            .next()
            .unwrap()
            .ends_with("to [senator::alice/claude] (you wrote [senator::alice/*])."),
        "{}",
        out(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ambiguous_recipient_lists_the_candidates_and_fails() {
    for zh in [false, true] {
        let h = machine(zh).await;
        h.server.seat("alice/codex");
        let before = h.server.events("refund-v2").len();
        for to in ["*", "alice/*"] {
            let o = senate(
                &h,
                &["ask", to, "which?", "--committee", "refund-v2"],
                None,
                None,
            )
            .await;
            assert!(!o.status.success());
            let text = err(&o);
            assert!(
                text.contains("[senator::alice/claude], [senator::alice/codex]"),
                "{text}"
            );
            if zh {
                assert!(
                    text.contains("在这里有好几个 agent")
                        && text.contains("senate ask alice/claude"),
                    "{text}"
                );
            } else {
                assert!(
                    text.contains("has several agents here")
                        && text.contains("`senate ask alice/claude …`"),
                    "{text}"
                );
            }
            assert!(out(&o).is_empty(), "{}", out(&o));
        }
        assert_eq!(h.server.events("refund-v2").len(), before, "nothing stored");
        // Naming one lands.
        let o = senate(
            &h,
            &["ask", "codex", "which?", "--committee", "refund-v2"],
            None,
            None,
        )
        .await;
        assert!(o.status.success(), "{}", err(&o));
        assert_eq!(last(&h).to, [codex("alice")]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_comes_from_stdin_when_it_is_a_dash() {
    let h = machine(false).await;
    let o = senate(
        &h,
        &["ask", "claude", "-", "--committee", "refund-v2"],
        Some("line one\nline two\n"),
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    assert_eq!(last(&h).body, "line one\nline two");
    let o = senate(
        &h,
        &["ask", "claude", "-", "--committee", "refund-v2"],
        Some("  \n"),
        None,
    )
    .await;
    assert!(!o.status.success());
    assert!(err(&o).contains("no text"), "{}", err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_target_says_why_and_stores_nothing() {
    let h = machine(false).await;
    let before = h.server.events("refund-v2").len();
    for (to, why) in [
        ("committee", "is not someone you can write to"),
        ("alice", "that is you"),
        ("Bad Name", "cannot read"),
    ] {
        let o = senate(
            &h,
            &["ask", to, "?", "--committee", "refund-v2"],
            None,
            None,
        )
        .await;
        assert!(!o.status.success(), "{to}");
        assert!(err(&o).contains(why), "{to}: {}", err(&o));
    }
    assert_eq!(h.server.events("refund-v2").len(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reply_speaks_in_the_thread_to_whoever_took_part() {
    let h = machine(false).await;
    let o = senate(
        &h,
        &[
            "ask",
            "claude",
            "which header?",
            "--committee",
            "refund-v2",
            "--json",
        ],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    let posted: Value = serde_json::from_str(&out(&o)).unwrap();
    let root = posted["seq"].as_u64().unwrap();
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Answer,
            claude("alice"),
            "Idempotency-Key",
        )
        .to(person("alice"))
        .in_thread(Seq(root)),
    );
    let o = senate(
        &h,
        &[
            "reply",
            &root.to_string(),
            "no,",
            "the",
            "retry",
            "header",
            "--committee",
            "refund-v2",
        ],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    let said = last(&h);
    assert_eq!(
        (
            said.kind.clone(),
            said.from.clone(),
            said.thread,
            said.to.clone()
        ),
        (
            Kind::Note,
            person("alice"),
            Some(Seq(root)),
            vec![claude("alice")]
        )
    );
    assert_eq!(said.body, "no, the retry header");
    assert_eq!(
        out(&o).trim(),
        format!(
            "Replied in thread {root} on refund-v2 (seq {}), to [senator::alice/claude].",
            said.seq
        )
    );
    // The same thread read by an event inside it.
    let o = senate(
        &h,
        &[
            "thread",
            &said.seq.to_string(),
            "--committee",
            "refund-v2",
            "--json",
        ],
        None,
        None,
    )
    .await;
    let v: Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["root"].as_u64(), Some(root));
    assert_eq!(v["events"].as_array().unwrap().len(), 3);
    // An unknown thread is refused with the daemon's words.
    let o = senate(
        &h,
        &["reply", "999", "x", "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(!o.status.success());
    assert!(err(&o).contains("no event with seq 999"), "{}", err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_stays_until_the_answer_arrives_and_prints_it_with_its_byline() {
    let h = machine(false).await;
    let args = [
        "ask",
        "claude",
        "which header?",
        "--committee",
        "refund-v2",
        "--wait=20",
    ];
    let waiting = senate(&h, &args, None, None);
    let answering = async {
        let q = eventually("the question", WAIT, || {
            h.server
                .events("refund-v2")
                .into_iter()
                .find(|e| e.kind == Kind::Question && e.from == person("alice"))
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        // A provisional answer is not the answer.
        h.server.append(
            NewEvent::new(committee(), Kind::Answer, Address::Speaker, "looking")
                .on_behalf_of(claude("alice"))
                .in_thread(q.seq)
                .with_meta(senate_core::meta::PROVISIONAL, true.into()),
        );
        tokio::time::sleep(Duration::from_millis(600)).await;
        h.server.append(
            NewEvent::new(
                committee(),
                Kind::Answer,
                claude("alice"),
                "Idempotency-Key",
            )
            .to(person("alice"))
            .in_thread(q.seq),
        );
        q.seq
    };
    let (o, root) = tokio::join!(waiting, answering);
    assert!(o.status.success(), "{}", err(&o));
    let text = out(&o);
    assert!(
        text.starts_with(&format!("Sent a question on refund-v2: thread {root}")),
        "{text}"
    );
    assert!(
        !text.contains("senate thread"),
        "waiting does not say how to look later: {text}"
    );
    assert!(
        text.trim_end().ends_with(&format!(
            "[senator::alice/claude] answered in thread {root}:\nIdempotency-Key"
        )),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_gives_up_after_its_timeout_and_says_how_to_look_later() {
    for zh in [false, true] {
        let h = machine(zh).await;
        let started = std::time::Instant::now();
        let o = senate(
            &h,
            &[
                "ask",
                "claude",
                "anyone?",
                "--committee",
                "refund-v2",
                "--wait=1",
            ],
            None,
            None,
        )
        .await;
        assert!(!o.status.success());
        assert!(started.elapsed() < Duration::from_secs(8));
        let root = last(&h).seq;
        let text = err(&o);
        if zh {
            assert!(
                text.contains(&format!("thread {root} 里 1 秒内没有回答")),
                "{text}"
            );
        } else {
            assert!(
                text.contains(&format!("no answer in thread {root} after 1s")),
                "{text}"
            );
        }
        assert!(text.contains(&format!("senate thread {root}")), "{text}");
        // The question itself was sent, and said so.
        assert!(out(&o).contains(&format!("thread {root}")), "{}", out(&o));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn everything_is_said_in_the_daemons_language() {
    let h = machine(true).await;
    let o = senate(
        &h,
        &["ask", "*", "有人吗？", "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    let root = last(&h).seq;
    let text = out(&o);
    assert!(
        text.starts_with(&format!(
            "已发出 question 到 refund-v2：thread {root}，给 [senator::alice/claude]（你写的是 [senator::alice/*]）。"
        )),
        "{text}"
    );
    assert!(text.contains(&format!("`senate thread {root}`")), "{text}");
    let o = senate(
        &h,
        &["thread", &root.to_string(), "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(
        out(&o).starts_with(&format!("refund-v2 的 thread {root}，共 1 条：")),
        "{}",
        out(&o)
    );
    let o = senate(
        &h,
        &[
            "reply",
            &root.to_string(),
            "算了",
            "--committee",
            "refund-v2",
        ],
        None,
        None,
    )
    .await;
    assert!(
        out(&o).starts_with(&format!("已在 refund-v2 的 thread {root} 里回复")),
        "{}",
        out(&o)
    );
    let o = senate(
        &h,
        &["tell", "claude", "好", "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(out(&o).starts_with("已发出 note（seq "), "{}", out(&o));
}
