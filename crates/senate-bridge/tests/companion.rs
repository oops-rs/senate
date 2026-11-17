//! Companion sessions, with a fake `claude`: never a real one.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::*;
use senate_bridge::hook::HookKind;
use senate_bridge::ipc::{self, CatchUpReply, Posted, Request, SessionSelector};
use senate_bridge::state::ItemStatus;
use senate_core::{Kind, meta};

/// A `claude` stand-in: records its arguments, environment, cwd and stdin
/// under `dir`, then waits for `dir/done` (at most ~20 s).
fn fake_claude(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let bin = dir.join("claude");
    let d = dir.display();
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > '{d}/args'\n\
             pwd > '{d}/cwd'\n\
             printf '%s' \"$SENATE_COMPANION\" > '{d}/flag'\n\
             printf '%s' \"$SENATE_HOME\" > '{d}/home'\n\
             cat > '{d}/prompt'\n\
             printf '%s' \"$SENATE_SESSION_ID\" > '{d}/sid.tmp' && mv '{d}/sid.tmp' '{d}/sid'\n\
             i=0\n\
             while [ ! -f '{d}/done' ] && [ $i -lt 200 ]; do sleep 0.1; i=$((i+1)); done\n\
             echo '{{\"type\":\"result\",\"is_error\":false,\"result\":\"ok\"}}'\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn enable_companion(h: &Harness, bin: &Path, max_secs: u64) {
    let mut text = std::fs::read_to_string(h.paths.config()).unwrap();
    text.push_str(&format!(
        "\n[companion]\nenabled = true\nclaude_bin = \"{}\"\nmax_secs = {max_secs}\n",
        bin.display()
    ));
    std::fs::write(h.paths.config(), text).unwrap();
}

fn selector(sid: &str) -> SessionSelector {
    SessionSelector {
        session_id: Some(sid.into()),
        claude_pid: None,
    }
}

/// alice worked on refund-v2 in session `s-old`, then closed it.
async fn left_a_session(h: &Harness) -> PathBuf {
    let wd = h.workdir("w1", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s-old", &wd, None).await;
    h.wait_stream("refund-v2").await;
    h.hook(HookKind::SessionEnd, "s-old", &wd, None).await;
    wd
}

async fn wait_file(path: &Path) -> String {
    eventually(&format!("{}", path.display()), WAIT, || {
        std::fs::read_to_string(path).ok()
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_with_no_session_is_answered_by_a_companion() {
    let mut h = Harness::new().await;
    let dump = h.dir.path().join("claude-dump");
    enable_companion(&h, &fake_claude(&dump), 30);
    h.start().await;
    let wd = left_a_session(&h).await;

    // A request is never given to a companion.
    let mut request = question(claude("bob"), claude("alice"), "把超时改成 5 秒");
    request.kind = Kind::Request;
    h.server.append(request);
    let q = h.server.append(question(
        claude("bob"),
        claude("alice"),
        "幂等键用哪个 header？",
    ));
    let sid = wait_file(&dump.join("sid")).await;
    assert!(sid.starts_with("companion-"), "{sid}");
    let args = std::fs::read_to_string(dump.join("args")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert_eq!(&args[..4], ["-p", "--resume", "s-old", "--fork-session"]);
    assert!(args.windows(2).any(|w| w == ["--output-format", "json"]));
    let allowed = args
        .iter()
        .position(|a| *a == "--allowedTools")
        .map(|i| args[i + 1])
        .unwrap();
    assert!(allowed.contains("mcp__senate__answer"));
    assert!(!allowed.contains("Bash"));
    assert_eq!(std::fs::read_to_string(dump.join("flag")).unwrap(), "1");
    assert_eq!(
        Path::new(std::fs::read_to_string(dump.join("cwd")).unwrap().trim())
            .canonicalize()
            .unwrap(),
        wd.canonicalize().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(dump.join("home")).unwrap(),
        h.paths.home().display().to_string()
    );
    let prompt = std::fs::read_to_string(dump.join("prompt")).unwrap();
    assert!(prompt.contains("幂等键用哪个 header？"), "{prompt}");
    assert!(prompt.contains(&format!("thread={}", q.seq)), "{prompt}");
    assert_eq!(h.item(q.seq.0).unwrap().status, ItemStatus::Inflight);

    // The fork's MCP shim speaks as the companion: it may read and answer
    // its question, nothing else.
    let socket = h.socket();
    let call = |r: Request| {
        let socket = socket.clone();
        async move { ipc::request::<Posted>(&socket, &r, WAIT, WAIT).await }
    };
    let refused = call(Request::Post {
        session: selector(&sid),
        body: "note".into(),
        proposal: false,
        committee: None,
        agent: None,
        id: None,
    })
    .await
    .unwrap_err();
    assert!(refused.to_string().contains("companion"), "{refused}");
    let read_before = h.server.cursor("refund-v2", "claude", "read");
    let caught: CatchUpReply = ipc::request(
        &h.socket(),
        &Request::CatchUp {
            session: selector(&sid),
            limit: None,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    assert!(!caught.items.is_empty());
    assert_eq!(
        h.server.cursor("refund-v2", "claude", "read"),
        read_before,
        "a companion does not move the read cursor"
    );
    let posted = call(Request::Answer {
        session: selector(&sid),
        thread: q.seq,
        body: "Idempotency-Key".into(),
        id: None,
        committee: None,
        agent: None,
        as_person: false,
    })
    .await
    .unwrap();
    let answer = h.server.last_event("refund-v2");
    assert_eq!(answer.seq, posted.seq);
    assert_eq!(answer.thread, Some(q.seq));
    assert_eq!(
        answer.meta.get(meta::COMPANION),
        Some(&serde_json::json!(true))
    );

    std::fs::write(dump.join("done"), "").unwrap();
    eventually("the question delivered via the companion", WAIT, || {
        let item = h.item(q.seq.0).unwrap();
        (item.status == ItemStatus::Delivered).then_some(())
    })
    .await;
    // Its session is gone with it.
    let gone = call(Request::Answer {
        session: selector(&sid),
        thread: q.seq,
        body: "again".into(),
        id: None,
        committee: None,
        agent: None,
        as_person: false,
    })
    .await
    .unwrap_err();
    assert!(gone.to_string().contains("not registered"), "{gone}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_companion_that_does_not_answer_is_killed_and_hands_the_question_back() {
    let mut h = Harness::new().await;
    let dump = h.dir.path().join("claude-dump");
    enable_companion(&h, &fake_claude(&dump), 1);
    h.start().await;
    let wd = left_a_session(&h).await;
    let q = h
        .server
        .append(question(claude("bob"), claude("alice"), "重试几次？"));
    wait_file(&dump.join("sid")).await;
    eventually(
        "the question back in the queue",
        Duration::from_secs(5),
        || {
            let item = h.item(q.seq.0).unwrap();
            (item.status == ItemStatus::Pending).then_some(())
        },
    )
    .await;
    // One try per question: no second companion.
    std::fs::remove_file(dump.join("sid")).unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!dump.join("sid").exists(), "not respawned");

    // A session that opens gets it.
    let out = h
        .hook(HookKind::SessionStart, "s2", &wd, None)
        .await
        .unwrap();
    assert!(context(&out).contains("重试几次？"), "{out}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn companions_are_off_by_default() {
    // The committee stays joined through bridge.toml, so the question arrives.
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    let dump = h.dir.path().join("claude-dump");
    fake_claude(&dump);
    h.start().await;
    left_a_session(&h).await;
    let q = h
        .server
        .append(question(claude("bob"), claude("alice"), "在吗？"));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!dump.join("sid").exists());
    assert_eq!(h.item(q.seq.0).unwrap().status, ItemStatus::Pending);
    h.stop().await;
}
