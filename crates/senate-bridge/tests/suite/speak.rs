//! The person's own commands (`senate ask`, `tell`, `request`, `reply`,
//! `thread`): the real binary against a real daemon and the fake server.
//! Every spawn gets a temp HOME and SENATE_HOME.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

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
    senate_bridge::testenv::isolate(&mut cmd, h.dir.path());
    cmd.env("HOME", h.dir.path())
        .env("SENATE_HOME", h.paths.home())
        .env("SENATE_VERBOSE", "1")
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
            "Sent a question on refund-v2: thread {}, to [senator::claude].",
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
            && text.contains(" UTC · question · to [senator::claude]"),
        "{text}"
    );
    assert!(
        text.contains("[senator::claude] · ") && text.contains(" · answer"),
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
            "claude",
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
            "Sent a note on refund-v2 (seq {}) to [senator::claude].",
            note.seq
        )
    );

    // bob's agent has connected before: a senator of this Senate.
    h.server.seat("copilot", "bob");
    let o = senate(
        &h,
        &[
            "request",
            "copilot",
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

/// A name that is no senator here (a mistyped name, or a prefix of a
/// session's id) is refused before anything is posted: the question would
/// otherwise wait for someone who does not exist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_that_is_no_senator_is_refused_and_nothing_is_posted() {
    let h = machine(false).await;
    let before = h.server.events("refund-v2").len();
    for wrote in ["clau", "claude-7ddea704", "nobody"] {
        let o = senate(
            &h,
            &["ask", wrote, "which header?", "--committee", "refund-v2"],
            None,
            None,
        )
        .await;
        assert!(!o.status.success(), "{wrote}: {}", out(&o));
        assert!(
            err(&o).contains(&format!("[senator::{wrote}] cannot be reached"))
                && err(&o).contains(&format!("has no senator {wrote}")),
            "{wrote}: {}",
            err(&o)
        );
        assert_eq!(
            h.server.events("refund-v2").len(),
            before,
            "{wrote}: nothing was posted"
        );
    }
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
            .ends_with("to [senator::claude] (you wrote [senator::*])."),
        "{}",
        out(&o)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ambiguous_recipient_lists_the_candidates_and_fails() {
    for zh in [false, true] {
        let h = machine(zh).await;
        h.server.seat("codex", "alice");
        let before = h.server.events("refund-v2").len();
        for to in ["*", "senator::*"] {
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
                text.contains("[senator::claude], [senator::codex]"),
                "{text}"
            );
            if zh {
                assert!(
                    text.contains("[senator::*] 在这个委员会上有好几个 agent")
                        && text.contains("senate ask claude"),
                    "{text}"
                );
            } else {
                assert!(
                    text.contains("[senator::*] could reach several agents on this committee")
                        && text.contains("`senate ask claude …`"),
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
            "Replied in thread {root} on refund-v2 (seq {}), to [senator::claude].",
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
            "[senator::claude] answered in thread {root}:\nIdempotency-Key"
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
            "已发出 question 到 refund-v2：thread {root}，给 [senator::claude]（你写的是 [senator::*]）。"
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

// ---- what comes back to the person --------------------------------------

use senate_core::{StatusPayload, StatusState, Step, meta};

fn status_ev(root: Seq, p: StatusPayload) -> NewEvent {
    NewEvent::new(committee(), Kind::Status, claude("alice"), "")
        .in_thread(root)
        .with_meta(meta::STATUS, p.value())
}

/// What the first real task run did: processing with a title and steps, an
/// answer to the person, a suspension escalated to them with nine decisions
/// wanted, a checkpoint, then every step ticked off with `active`.
fn the_real_sequence(h: &Harness) -> Seq {
    let root = h.server.append(
        NewEvent::new(committee(), Kind::Request, person("alice"), "do the thing")
            .to(claude("alice")),
    );
    let mut s = StatusPayload::new(StatusState::Processing);
    s.title = Some("The plan".into());
    s.steps = Some(vec![Step {
        id: "a".into(),
        label: "read".into(),
        done: false,
    }]);
    h.server.append(status_ev(root.seq, s));
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Answer,
            claude("alice"),
            "The result is in the doc.\nSecond line.",
        )
        .to(person("alice"))
        .in_thread(root.seq),
    );
    let mut esc = StatusPayload::new(StatusState::Suspended);
    esc.escalated_to = Some(senate_core::Name::new("alice").unwrap());
    esc.reason = Some("1. which header?\n2. which table?".into());
    h.server.append(status_ev(root.seq, esc));
    h.server.append(
        NewEvent::new(committee(), Kind::Checkpoint, claude("alice"), "cp").in_thread(root.seq),
    );
    let mut done = StatusPayload::new(StatusState::Active);
    done.steps = Some(vec![Step {
        id: "a".into(),
        label: "read".into(),
        done: true,
    }]);
    h.server.append(status_ev(root.seq, done));
    root.seq
}

async fn inbox_text(h: &Harness, extra: &[&str]) -> String {
    let mut args = vec!["inbox", "--committee", "refund-v2"];
    args.extend_from_slice(extra);
    let o = senate(h, &args, None, None).await;
    assert!(o.status.success(), "{}", err(&o));
    out(&o)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_inbox_lists_what_came_back_to_the_person_until_they_read_it() {
    for zh in [false, true] {
        let h = machine(zh).await;
        let root = the_real_sequence(&h);
        let text = inbox_text(&h, &[]).await;
        let first = text.lines().next().unwrap();
        if zh {
            assert_eq!(
                first,
                "refund-v2 上有 1 个 thread 在等你（[senator::alice]），其中 1 个有新内容："
            );
            assert!(text.contains("对你是新的"), "{text}");
            assert!(
                text.contains("[senator::claude] 把它转给了你：\n      1. which header?\n      2. which table?"),
                "{text}"
            );
            assert!(text.contains("读：`senate thread "), "{text}");
        } else {
            assert_eq!(
                first,
                "1 thread(s) on refund-v2 wait on you ([senator::alice]); 1 with news:"
            );
            assert!(
                text.contains(&format!("- thread {root} · The plan · new for you")),
                "{text}"
            );
            assert!(
                text.contains("[senator::claude] escalated this to you:\n      1. which header?\n      2. which table?"),
                "{text}"
            );
            assert!(text.contains("read: `senate thread "), "{text}");
        }
        assert!(
            !text.contains("settled") && !text.contains("Nothing open waits"),
            "{text}"
        );

        // Showing the thread reads it: no longer new, still waiting on the
        // person because an escalation lasts until they act.
        let t = senate(
            &h,
            &["thread", &root.to_string(), "--committee", "refund-v2"],
            None,
            None,
        )
        .await;
        assert!(t.status.success(), "{}", err(&t));
        let text = inbox_text(&h, &[]).await;
        if zh {
            assert!(
                !text.contains("对你是新的") && text.contains("你已看过，等你处理"),
                "{text}"
            );
            assert!(text.contains("其中 0 个有新内容"), "{text}");
        } else {
            assert!(
                !text.contains("new for you") && text.contains("seen; it waits until you act"),
                "{text}"
            );
            assert!(text.contains("0 with news"), "{text}");
        }
        assert!(text.contains("1. which header?"), "{text}");

        // Replying hands it back to the agent.
        let r = senate(
            &h,
            &[
                "reply",
                &root.to_string(),
                "decided",
                "--committee",
                "refund-v2",
            ],
            None,
            None,
        )
        .await;
        assert!(r.status.success(), "{}", err(&r));
        let text = inbox_text(&h, &[]).await;
        assert!(
            !text.contains("wait on you") && !text.contains("在等你"),
            "{text}"
        );
        // A new note from the agent is news again.
        h.server.append(
            NewEvent::new(committee(), Kind::Note, claude("alice"), "Done, see PR")
                .to(person("alice"))
                .in_thread(root),
        );
        let text = inbox_text(&h, &[]).await;
        assert!(text.contains("Done, see PR"), "{text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_nobody_has_read_is_not_settled_for_the_person_and_mark_read_clears_it() {
    for zh in [false, true] {
        let h = machine(zh).await;
        let root = h.server.append(
            NewEvent::new(
                committee(),
                Kind::Question,
                person("alice"),
                "which header?",
            )
            .to(claude("alice")),
        );
        h.server.append(
            NewEvent::new(
                committee(),
                Kind::Answer,
                claude("alice"),
                "Idempotency-Key",
            )
            .to(person("alice"))
            .in_thread(root.seq),
        );
        let text = inbox_text(&h, &[]).await;
        assert!(text.contains("Idempotency-Key"), "{text}");
        assert!(
            text.contains(if zh { "回答了：" } else { "answered:" }),
            "{text}"
        );
        assert!(
            !text.contains("not shown") && !text.contains("未显示"),
            "{text}"
        );

        let text = inbox_text(&h, &["--mark-read"]).await;
        assert!(
            text.contains("Idempotency-Key"),
            "listed before it is cleared: {text}"
        );
        assert!(
            text.contains(if zh {
                "已把 1 个 thread 标为已读"
            } else {
                "Marked 1 thread(s) as read"
            }),
            "{text}"
        );
        // Now it is the settled thread the agents' rule says it is.
        let text = inbox_text(&h, &[]).await;
        assert!(!text.contains("Idempotency-Key"), "{text}");
        assert!(
            text.contains(if zh {
                "已答复的 1 个 thread 未显示"
            } else {
                "1 answered thread(s) not shown"
            }),
            "{text}"
        );
        // And the agents' own inbox never changed: settled for them
        // throughout.
    }
}

// ---- being told: --wait for a request, escalations, watch, notifications ---

use senate_bridge::notify::{Desktop, Recording};

fn escalation_from_claude(root: Seq, reason: &str) -> NewEvent {
    let mut esc = StatusPayload::new(StatusState::Suspended);
    esc.escalated_to = Some(senate_core::Name::new("alice").unwrap());
    esc.reason = Some(reason.into());
    status_ev(root, esc)
}

async fn first_thread(h: &Harness, kind: Kind) -> senate_core::Event {
    eventually("the person's message", WAIT, || {
        h.server
            .events("refund-v2")
            .into_iter()
            .find(|e| e.kind == kind && e.from == person("alice"))
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_wait_returns_on_an_answer_or_an_escalation_and_otherwise_times_out() {
    // An answer.
    let h = machine(false).await;
    let args = [
        "request",
        "claude",
        "do it",
        "--committee",
        "refund-v2",
        "--wait=20",
    ];
    let waiting = senate(&h, &args, None, None);
    let answering = async {
        let r = first_thread(&h, Kind::Request).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        h.server.append(
            NewEvent::new(committee(), Kind::Answer, claude("alice"), "Done")
                .to(person("alice"))
                .in_thread(r.seq),
        );
        r.seq
    };
    let (o, root) = tokio::join!(waiting, answering);
    assert!(o.status.success(), "{}", err(&o));
    assert!(
        out(&o).trim_end().ends_with(&format!(
            "[senator::claude] answered in thread {root}:\nDone"
        )),
        "{}",
        out(&o)
    );

    // An escalation back to the asker ends the wait for `request` and `ask`,
    // with its reason, in either language, and counts as seen.
    for (zh, kind) in [(false, "request"), (true, "ask")] {
        let h = machine(zh).await;
        let args = [
            kind,
            "claude",
            "do it",
            "--committee",
            "refund-v2",
            "--wait=20",
        ];
        let waiting = senate(&h, &args, None, None);
        let escalating = async {
            let k = if kind == "ask" {
                Kind::Question
            } else {
                Kind::Request
            };
            let r = first_thread(&h, k).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            h.server.append(escalation_from_claude(
                r.seq,
                "1. which header?\n2. which table?",
            ));
            r.seq
        };
        let (o, root) = tokio::join!(waiting, escalating);
        assert!(o.status.success(), "{}", err(&o));
        let text = out(&o);
        let expected = if zh {
            format!(
                "[senator::claude] 把 thread {root} 转给了你：\n1. which header?\n2. which table?\n用 `senate reply {root} <文字>` 回复。"
            )
        } else {
            format!(
                "[senator::claude] escalated thread {root} to you:\n1. which header?\n2. which table?\nReply with `senate reply {root} <text>`."
            )
        };
        assert!(text.trim_end().ends_with(&expected), "{text}");
        // It was shown, so it is not news; it still waits until they act.
        let inbox = inbox_text(&h, &[]).await;
        assert!(
            inbox.contains(if zh {
                "其中 0 个有新内容"
            } else {
                "0 with news"
            }),
            "{inbox}"
        );
    }

    // Nothing in time.
    let h = machine(false).await;
    let o = senate(
        &h,
        &[
            "request",
            "claude",
            "do it",
            "--committee",
            "refund-v2",
            "--wait=1",
        ],
        None,
        None,
    )
    .await;
    assert!(!o.status.success());
    assert!(err(&o).contains("no answer in thread"), "{}", err(&o));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_wait_ask_and_request_say_how_the_person_will_hear_back() {
    for zh in [false, true] {
        let h = machine(zh).await;
        for kind in ["ask", "request"] {
            let o = senate(
                &h,
                &[kind, "claude", "x", "--committee", "refund-v2"],
                None,
                None,
            )
            .await;
            let text = out(&o);
            for needle in if zh {
                ["桌面通知", "`senate inbox`", "`senate inbox --follow`"]
            } else {
                [
                    "desktop notification",
                    "`senate inbox`",
                    "`senate inbox --follow`",
                ]
            } {
                assert!(text.contains(needle), "{needle}: {text}");
            }
        }
        // `tell` expects no answer and promises nothing.
        let o = senate(
            &h,
            &["tell", "claude", "x", "--committee", "refund-v2"],
            None,
            None,
        )
        .await;
        assert!(!out(&o).contains("senate inbox --follow"), "{}", out(&o));
    }
}

/// A running `senate inbox --follow`, its stdout read on a thread.
struct Watcher {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<String>,
    subscribed: std::sync::mpsc::Receiver<()>,
}

impl Watcher {
    fn start(h: &Harness, args: &[&str]) -> Self {
        let mut cmd = Command::new(BIN);
        senate_bridge::testenv::isolate(&mut cmd, h.dir.path());
        cmd.env("HOME", h.dir.path())
            .env("SENATE_HOME", h.paths.home())
            .env("SENATE_VERBOSE", "1")
            .args(["inbox", "--follow"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        // The command says "watching" on stderr once the daemon has
        // subscribed it: that is the signal to wait for, not a delay.
        let stderr = child.stderr.take().unwrap();
        let (subscribed_tx, subscribed) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut lines = std::io::BufReader::new(stderr).lines();
            if lines.next().is_some() {
                let _ = subscribed_tx.send(());
            }
            // Keep draining so the command never blocks on a full pipe.
            for _ in lines {}
        });
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            lines,
            subscribed,
        }
    }

    /// Wait until the daemon has subscribed this watcher: anything appended
    /// after this is printed.
    fn ready(&self) {
        self.subscribed
            .recv_timeout(Duration::from_secs(30))
            .expect("senate inbox --follow never said it was watching");
    }

    /// Lines until `n` blocks (each ends at a blank line) or the timeout.
    fn blocks(&self, n: usize) -> Vec<String> {
        let mut blocks = Vec::new();
        let mut current = String::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while blocks.len() < n {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) if line.is_empty() => blocks.push(std::mem::take(&mut current)),
                Ok(line) => {
                    if !current.is_empty() {
                        current.push('\n');
                    }
                    current.push_str(&line);
                }
                Err(_) => break,
            }
        }
        blocks
    }

    fn json(&self, n: usize) -> Vec<Value> {
        let mut out = Vec::new();
        while out.len() < n {
            match self.lines.recv_timeout(Duration::from_secs(10)) {
                Ok(line) => out.push(serde_json::from_str(&line).unwrap()),
                Err(_) => break,
            }
        }
        out
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_prints_each_kind_addressed_to_the_person_and_nothing_else() {
    for zh in [false, true] {
        let h = machine(zh).await;
        let w = Watcher::start(&h, &["--committee", "refund-v2"]);
        // The watcher must be subscribed before events arrive.
        w.ready();
        let root = h.server.append(
            NewEvent::new(committee(), Kind::Request, person("alice"), "do it").to(claude("alice")),
        );
        // Not for the person: their own words, an agent's progress, what is
        // put to the Speaker.
        h.server.append(status_ev(
            root.seq,
            StatusPayload::new(StatusState::Processing),
        ));
        h.server.append(
            NewEvent::new(committee(), Kind::Question, claude("alice"), "pick one?")
                .to(Address::Speaker),
        );
        h.server.append(
            NewEvent::new(committee(), Kind::Answer, claude("alice"), "Plan\nline two")
                .to(person("alice"))
                .in_thread(root.seq),
        );
        h.server.append(
            NewEvent::new(committee(), Kind::Note, claude("alice"), "FYI the PR is up")
                .to(person("alice"))
                .in_thread(root.seq),
        );
        h.server
            .append(escalation_from_claude(root.seq, "1. which header?"));
        let blocks = w.blocks(3);
        assert_eq!(blocks.len(), 3, "{blocks:#?}");
        let r = root.seq;
        let (answer, note, esc) = (&blocks[0], &blocks[1], &blocks[2]);
        if zh {
            assert!(
                answer.starts_with("[senator::claude] 回答了 · "),
                "{answer}"
            );
            assert!(
                note.starts_with("[senator::claude] 留了一条 note · "),
                "{note}"
            );
            assert!(
                esc.starts_with("[senator::claude] 把它转给了你 · "),
                "{esc}"
            );
            assert!(
                answer.contains(&format!("读：`senate thread {r} --committee refund-v2`")),
                "{answer}"
            );
        } else {
            assert!(
                answer.starts_with("[senator::claude] answered · "),
                "{answer}"
            );
            assert!(
                note.starts_with("[senator::claude] left a note · "),
                "{note}"
            );
            assert!(
                esc.starts_with("[senator::claude] escalated this to you · "),
                "{esc}"
            );
            assert!(
                answer.contains(&format!("read: `senate thread {r} --committee refund-v2`")),
                "{answer}"
            );
            assert!(
                answer.contains(&format!(
                    "reply: `senate reply {r} --committee refund-v2 <text>`"
                )),
                "{answer}"
            );
        }
        assert!(
            answer.contains(&format!("refund-v2 · thread {r}")),
            "{answer}"
        );
        assert!(answer.contains("\n  Plan\n  line two\n"), "{answer}");
        assert!(note.contains("\n  FYI the PR is up\n"), "{note}");
        assert!(esc.contains("\n  1. which header?\n"), "{esc}");
        // It marked nothing as read.
        let text = inbox_text(&h, &[]).await;
        assert!(
            text.contains(if zh { "对你是新的" } else { "new for you" }),
            "{text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn watch_json_is_one_object_per_line_and_a_named_committee_filters() {
    let h = machine(false).await;
    let w = Watcher::start(&h, &["--committee", "refund-v2", "--json"]);
    let other = Watcher::start(&h, &["--committee", "other"]);
    w.ready();
    other.ready();
    let root = h.server.append(
        NewEvent::new(committee(), Kind::Request, person("alice"), "do it").to(claude("alice")),
    );
    h.server.append(
        NewEvent::new(committee(), Kind::Answer, claude("alice"), "Done")
            .to(person("alice"))
            .in_thread(root.seq),
    );
    let lines = w.json(1);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["event"], "for_person");
    assert_eq!(lines[0]["escalated"], false);
    assert_eq!(lines[0]["text"], "Done");
    assert_eq!(lines[0]["news"]["kind"], "answer");
    assert_eq!(lines[0]["news"]["thread"], root.seq.0);
    assert!(
        other.blocks(1).is_empty(),
        "another committee's watch prints nothing"
    );
}

/// A machine whose daemon posts through `rec` with a short burst window.
async fn machine_notifying(rec: &std::sync::Arc<Recording>, config: &str) -> Harness {
    let mut h = Harness::new().await;
    h.desktop = Some(Desktop {
        notifier: rec.clone(),
        window: Duration::from_millis(300),
    });
    if !config.is_empty() {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(h.paths.config())
            .unwrap();
        writeln!(f, "\n{config}").unwrap();
    }
    h.start().await;
    let wd = h.workdir("s", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    h.wait_stream("refund-v2").await;
    h
}

fn answer_to_alice(root: Seq, body: &str) -> NewEvent {
    NewEvent::new(committee(), Kind::Answer, claude("alice"), body)
        .to(person("alice"))
        .in_thread(root)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_desktop_is_told_once_per_burst_and_says_who_and_the_first_line() {
    let rec = Recording::new();
    let h = machine_notifying(&rec, "").await;
    let root = h.server.append(
        NewEvent::new(committee(), Kind::Request, person("alice"), "do it").to(claude("alice")),
    );
    h.server.append(answer_to_alice(
        root.seq,
        "Plan is ready for [senator::alice]\nsecond line",
    ));
    eventually("one notification", WAIT, || {
        (!rec.posted().is_empty()).then_some(())
    })
    .await;
    assert_eq!(
        rec.posted(),
        [(
            "Senate · refund-v2".to_string(),
            // Named as the app names them, in the byline and the line:
            // never a bracketed id.
            "claude answered: Plan is ready for alice".to_string()
        )]
    );
    // A burst: several events within the window are one notification.
    h.server.append(answer_to_alice(root.seq, "one"));
    h.server.append(
        NewEvent::new(committee(), Kind::Note, claude("alice"), "two")
            .to(person("alice"))
            .in_thread(root.seq),
    );
    h.server.append(escalation_from_claude(root.seq, "three"));
    eventually("the burst", WAIT, || {
        (rec.posted().len() >= 2).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    let posted = rec.posted();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(
        posted[1].1,
        "3 new for you, the latest: claude escalated this to you: three"
    );
    // Not for the person: nothing.
    h.server.append(
        NewEvent::new(committee(), Kind::Question, claude("alice"), "pick?").to(Address::Speaker),
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rec.posted().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_stays_quiet_while_the_app_is_connected_and_when_it_is_turned_off() {
    // An app client that shows notifications itself.
    let rec = Recording::new();
    let h = machine_notifying(&rec, "").await;
    let root = h.server.append(
        NewEvent::new(committee(), Kind::Request, person("alice"), "do it").to(claude("alice")),
    );
    let app = senate_bridge::ipc::IpcClient::connect(&h.socket(), Duration::from_secs(1))
        .await
        .unwrap()
        .watch(true, Duration::from_secs(5))
        .await
        .unwrap();
    h.server
        .append(answer_to_alice(root.seq, "while the app is open"));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(rec.posted().is_empty(), "{:?}", rec.posted());
    // A plain `senate inbox --follow` does not show notifications: it does not count.
    drop(app);
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.server
        .append(answer_to_alice(root.seq, "after the app is gone"));
    eventually("the notification", WAIT, || {
        (!rec.posted().is_empty()).then_some(())
    })
    .await;
    assert!(rec.posted()[0].1.ends_with("after the app is gone"));

    // `[notify] desktop = false`.
    let rec = Recording::new();
    let h = machine_notifying(&rec, "[notify]\ndesktop = false").await;
    let root = h.server.append(
        NewEvent::new(committee(), Kind::Request, person("alice"), "do it").to(claude("alice")),
    );
    h.server.append(answer_to_alice(root.seq, "quietly"));
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(rec.posted().is_empty());
    // It is still in the inbox.
    assert!(inbox_text(&h, &[]).await.contains("quietly"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_notifier_never_disturbs_delivery() {
    let rec = Recording::new().failing();
    let h = machine_notifying(&rec, "").await;
    let root = h.server.append(
        NewEvent::new(committee(), Kind::Request, person("alice"), "do it").to(claude("alice")),
    );
    let w = Watcher::start(&h, &["--committee", "refund-v2", "--json"]);
    w.ready();
    h.server.append(answer_to_alice(root.seq, "first"));
    eventually("an attempt", WAIT, || {
        (!rec.posted().is_empty()).then_some(())
    })
    .await;
    h.server.append(answer_to_alice(root.seq, "second"));
    let lines = w.json(2);
    assert_eq!(lines.len(), 2, "both still delivered");
    eventually("another attempt", WAIT, || {
        (rec.posted().len() >= 2).then_some(())
    })
    .await;
    assert!(inbox_text(&h, &[]).await.contains("second"));
}

// ---- a reply that crossed an escalation -----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reply_written_before_the_escalation_was_read_does_not_end_it() {
    for zh in [false, true] {
        let h = machine(zh).await;
        let root = h.server.append(
            NewEvent::new(committee(), Kind::Request, person("alice"), "do the thing")
                .to(claude("alice")),
        );
        let root = root.seq;
        h.server.append(
            NewEvent::new(
                committee(),
                Kind::Answer,
                claude("alice"),
                "First part done.",
            )
            .to(person("alice"))
            .in_thread(root),
        );
        // The person reads the thread: they have seen up to the answer.
        let shown = senate(
            &h,
            &["thread", &root.to_string(), "--committee", "refund-v2"],
            None,
            None,
        )
        .await;
        assert!(shown.status.success(), "{}", err(&shown));
        let answer_seq = last(&h).seq;

        // The agent escalates; before the person has seen it, they reply
        // about something else.
        let esc = h.server.append(escalation_from_claude(
            root,
            "1. which header?\n2. which table?\n3. which key?",
        ));
        let r = senate(
            &h,
            &[
                "reply",
                &root.to_string(),
                "unrelated:",
                "use",
                "the",
                "staging",
                "db",
                "--committee",
                "refund-v2",
            ],
            None,
            None,
        )
        .await;
        assert!(r.status.success(), "{}", err(&r));
        let sent = last(&h);
        assert_eq!(sent.from, person("alice"));
        assert_eq!(
            sent.meta.get(meta::SEEN),
            Some(&serde_json::json!(answer_seq.0)),
            "the reply says what had been shown to the person when it was written"
        );
        let text = out(&r);
        // The confirmation comes first, as for any reply; then the note,
        // as information.
        let confirmed = text.lines().next().unwrap_or_default();
        let replied = if zh {
            format!("的 thread {root} 里回复（seq {}）", sent.seq)
        } else {
            format!("Replied in thread {root} on refund-v2 (seq {})", sent.seq)
        };
        assert!(confirmed.contains(&replied), "{text}");
        let line = text.lines().nth(1).unwrap_or_default();
        if zh {
            assert!(
                line.starts_with(&format!(
                    "注意：thread {root} 有你还没读的 seq {}（转给你的，仍有效）",
                    esc.seq
                )) && line.contains(&format!("`senate thread {root}`")),
                "{text}"
            );
        } else {
            assert!(
                line.starts_with(&format!(
                    "Note: thread {root} has seq {}, which you have not read yet",
                    esc.seq
                )) && line.contains("(escalated to you, still in force)")
                    && line.contains(&format!("`senate thread {root}`")),
                "{text}"
            );
        }
        assert_eq!(text.lines().count(), 2, "one line, no more: {text}");

        // The escalation is still in force, still new, with its reason.
        let text = inbox_text(&h, &[]).await;
        if zh {
            assert!(text.contains("对你是新的"), "{text}");
            assert!(text.contains("把它转给了你"), "{text}");
        } else {
            assert!(text.contains("new for you"), "{text}");
            assert!(text.contains("escalated this to you"), "{text}");
        }
        assert!(text.contains("3. which key?"), "{text}");

        // Reading it, then replying, ends it, and says nothing more.
        let shown = senate(
            &h,
            &["thread", &root.to_string(), "--committee", "refund-v2"],
            None,
            None,
        )
        .await;
        assert!(shown.status.success(), "{}", err(&shown));
        let r = senate(
            &h,
            &[
                "reply",
                &root.to_string(),
                "decided",
                "--committee",
                "refund-v2",
            ],
            None,
            None,
        )
        .await;
        assert!(r.status.success(), "{}", err(&r));
        assert_eq!(out(&r).lines().count(), 1, "{}", out(&r));
        assert!(
            last(&h).meta.get(meta::SEEN).and_then(Value::as_u64) >= Some(esc.seq.0),
            "{:?}",
            last(&h).meta
        );
        let text = inbox_text(&h, &[]).await;
        assert!(
            !text.contains("wait on you")
                && !text.contains("在等你")
                && !text.contains("which key"),
            "{text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_apps_answer_says_what_the_person_had_seen_too() {
    use senate_bridge::ipc::{self, Request};
    let h = machine(false).await;
    let root = h
        .server
        .append(
            NewEvent::new(
                committee(),
                Kind::Question,
                person("alice"),
                "which header?",
            )
            .to(claude("alice")),
        )
        .seq;
    let seen = h.server.append(
        NewEvent::new(committee(), Kind::Note, claude("alice"), "looking")
            .to(person("alice"))
            .in_thread(root),
    );
    let o = senate(
        &h,
        &["thread", &root.to_string(), "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    let esc = h.server.append(escalation_from_claude(root, "which one?"));
    let answer = |body: &str| Request::Answer {
        session: ipc::SessionSelector::default(),
        thread: root,
        body: body.into(),
        id: None,
        committee: Some(committee()),
        agent: None,
        as_person: true,
    };
    let posted: ipc::Posted = ipc::request(&h.socket(), &answer("unrelated"), WAIT, WAIT)
        .await
        .unwrap();
    assert_eq!(posted.unseen_escalation, Some(esc.seq));
    assert_eq!(
        last(&h).meta.get(meta::SEEN),
        Some(&serde_json::json!(seen.seq.0))
    );
    // Read, then answer: it ends, and the reply says nothing about it.
    let o = senate(
        &h,
        &["thread", &root.to_string(), "--committee", "refund-v2"],
        None,
        None,
    )
    .await;
    assert!(o.status.success(), "{}", err(&o));
    let posted: ipc::Posted = ipc::request(&h.socket(), &answer("chosen"), WAIT, WAIT)
        .await
        .unwrap();
    assert_eq!(posted.unseen_escalation, None);
    assert!(last(&h).meta.get(meta::SEEN).and_then(Value::as_u64) >= Some(esc.seq.0));
}

/// `senate#145`–`#147`: a request the desk forwarded here (`from` the
/// Speaker, its origin on another committee), the assignee's escalation to
/// the person, and the person's reply as themselves in that thread.
mod forwarded_reply {
    use super::*;
    use senate_bridge::ipc::{self, Request};

    fn forwarded(h: &Harness) -> Seq {
        let origin = senate_core::Origin {
            locator: "/top::commons::senator::agy".parse().unwrap(),
            thread: Seq(109),
        };
        h.server
            .append(
                NewEvent::new(
                    committee(),
                    Kind::Request,
                    Address::Speaker,
                    "merge and release?",
                )
                .to(claude("alice"))
                .with_meta(meta::ORIGIN, origin.value())
                .with_meta(meta::ACTED_BY, serde_json::json!("alice")),
            )
            .seq
    }

    async fn read(h: &Harness, root: Seq) {
        let o = senate(
            h,
            &["thread", &root.to_string(), "--committee", "refund-v2"],
            None,
            None,
        )
        .await;
        assert!(o.status.success(), "{}", err(&o));
    }

    async fn reply(h: &Harness, root: Seq, body: &str) -> senate_core::Event {
        let _: ipc::Posted = ipc::request(
            &h.socket(),
            &Request::Answer {
                session: ipc::SessionSelector::default(),
                thread: root,
                body: body.into(),
                id: None,
                committee: Some(committee()),
                agent: None,
                as_person: true,
            },
            WAIT,
            WAIT,
        )
        .await
        .unwrap();
        let stored = last(h);
        assert_eq!(stored.body, body);
        assert_eq!(stored.from, person("alice"), "her own words");
        assert!(stored.on_behalf_of.is_none());
        stored
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_decision_goes_to_the_asker_and_the_assignee_waiting_on_it() {
        let h = machine(false).await;
        let root = forwarded(&h);
        h.server
            .append(escalation_from_claude(root, "1. version?\n2. scope?"));
        read(&h, root).await;
        let stored = reply(&h, root, "1. 0.14.3\n2. yes, main").await;
        assert_eq!(stored.to, [Address::Speaker, claude("alice")]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn with_nothing_escalated_it_goes_to_the_asker_only() {
        let h = machine(false).await;
        let root = forwarded(&h);
        read(&h, root).await;
        let stored = reply(&h, root, "not yet").await;
        assert_eq!(stored.to, [Address::Speaker]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_reply_that_crossed_the_escalation_does_not_answer_it() {
        let h = machine(false).await;
        let root = forwarded(&h);
        read(&h, root).await;
        // Escalated after she last read it: her reply was not to that.
        h.server.append(escalation_from_claude(root, "which one?"));
        let stored = reply(&h, root, "unrelated").await;
        assert_eq!(stored.to, [Address::Speaker]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_withdrawn_escalation_waits_on_no_one() {
        let h = machine(false).await;
        let root = forwarded(&h);
        h.server.append(escalation_from_claude(root, "which one?"));
        let mut back = StatusPayload::new(StatusState::Processing);
        back.withdraw = true;
        h.server.append(status_ev(root, back));
        read(&h, root).await;
        let stored = reply(&h, root, "fine").await;
        assert_eq!(stored.to, [Address::Speaker]);
    }

    /// The control: a local question from the agent that also escalated
    /// it is answered to it, once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_local_question_is_answered_to_its_asker_once() {
        let h = machine(false).await;
        let root = h
            .server
            .append(
                NewEvent::new(committee(), Kind::Question, claude("alice"), "which env?")
                    .to(person("alice")),
            )
            .seq;
        h.server.append(escalation_from_claude(root, "which env?"));
        read(&h, root).await;
        let stored = reply(&h, root, "staging").await;
        assert_eq!(stored.to, [claude("alice")]);
    }
}
