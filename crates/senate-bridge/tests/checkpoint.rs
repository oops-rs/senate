//! Checkpoints in the daemon: the tool, handoff pushes, and the automatic
//! triggers (new commit, interval, session end), each one agent's.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use common::*;
use senate_bridge::hook::HookKind;
use senate_bridge::ipc::{self, CheckpointReply, Request, SessionSelector};
use senate_core::{CheckpointPayload, CheckpointTrigger, Event, Kind};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A worktree on `feature/refund-v2/alice` with one commit.
fn repo(h: &Harness, name: &str) -> std::path::PathBuf {
    let wd = h.workdir(name, Some("refund-v2"));
    git(&wd, &["init", "-q", "-b", "feature/refund-v2/alice"]);
    std::fs::write(wd.join("README.md"), "hi\n").unwrap();
    git(&wd, &["add", "README.md"]);
    git(&wd, &["commit", "-q", "-m", "first"]);
    wd
}

fn commit(wd: &Path, file: &str) -> String {
    std::fs::write(wd.join(file), file).unwrap();
    git(wd, &["add", file]);
    git(wd, &["commit", "-q", "-m", file]);
    git(wd, &["rev-parse", "HEAD"])
}

fn checkpoints(h: &Harness) -> Vec<(CheckpointPayload, Event)> {
    h.server
        .events("refund-v2")
        .into_iter()
        .filter(|e| e.kind == Kind::Checkpoint)
        .map(|e| (CheckpointPayload::of(&e).expect("a payload"), e))
        .collect()
}

async fn checkpoint(h: &Harness, sid: &str, progress: Option<&str>, push: bool) -> CheckpointReply {
    ipc::request(
        &h.socket(),
        &Request::Checkpoint {
            session: SessionSelector {
                session_id: Some(sid.into()),
                claude_pid: None,
            },
            summary: None,
            progress: progress.map(Into::into),
            decisions: None,
            open: Some("- seq 3: 等 bob 的回答".into()),
            push,
            id: None,
        },
        Duration::from_secs(1),
        Duration::from_secs(30),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tool_writes_the_files_and_a_handoff_pushes() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = repo(&h, "w1");
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    let head = git(&wd, &["rev-parse", "HEAD"]);

    let reply = checkpoint(&h, "s1", Some("## 完成\n- 幂等键"), false).await;
    assert_eq!(reply.commit, head);
    assert_eq!(reply.branch, "feature/refund-v2/alice");
    assert!(!reply.pushed);
    assert_eq!(reply.written, ["PROGRESS.md", "OPEN.md"]);
    assert_eq!(reply.files, ["PROGRESS.md", "OPEN.md"]);
    assert_eq!(
        std::fs::read_to_string(wd.join(".senate/PROGRESS.md")).unwrap(),
        "## 完成\n- 幂等键\n"
    );
    let (payload, event) = checkpoints(&h).pop().unwrap();
    assert_eq!(event.from, claude("alice"));
    assert_eq!(event.body, "完成", "the first line of PROGRESS.md");
    assert_eq!(payload.trigger, CheckpointTrigger::Tool);
    assert_eq!(
        payload.files.open.as_deref(),
        Some("- seq 3: 等 bob 的回答\n")
    );
    assert_eq!(
        event.refs.as_ref().unwrap().commit.as_deref(),
        Some(head.as_str())
    );
    assert!(
        !payload.session_transcript,
        "the hook's transcript path does not exist"
    );

    // A handoff with no origin: refused, reported, checkpointed anyway.
    let reply = checkpoint(&h, "s1", None, true).await;
    assert!(
        reply.push_error.as_deref().unwrap().contains("origin"),
        "{reply:?}"
    );
    assert_eq!(
        checkpoints(&h).pop().unwrap().0.trigger,
        CheckpointTrigger::Handoff
    );

    // With an origin it pushes, and the commit is then on the remote.
    let remote = h.dir.path().join("origin.git");
    git(
        h.dir.path(),
        &["init", "-q", "--bare", remote.to_str().unwrap()],
    );
    git(&wd, &["remote", "add", "origin", remote.to_str().unwrap()]);
    let reply = checkpoint(&h, "s1", None, true).await;
    assert_eq!(reply.push_error, None, "{reply:?}");
    assert!(reply.pushed, "{reply:?}");
    let (payload, _) = checkpoints(&h).pop().unwrap();
    assert!(payload.pushed);
    assert_eq!(payload.remote_url.as_deref(), remote.to_str());
    assert_eq!(
        git(&remote, &["rev-parse", "feature/refund-v2/alice"]),
        head,
        "the branch is on origin"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commits_intervals_and_session_ends_checkpoint_on_their_own() {
    let mut h = Harness::new().await;
    h.start_with(senate_bridge::Timing {
        checkpoint_poll: Duration::from_millis(150),
        checkpoint_interval: Duration::from_millis(1500),
        agy_nudge_quiet: Duration::from_secs(10),
        ..fast_timing()
    })
    .await;
    let wd = repo(&h, "w1");
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    // A second session in the same worktree does not double anything.
    h.hook(HookKind::SessionStart, "s2", &wd, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(checkpoints(&h).is_empty(), "the first look is a baseline");

    let second = commit(&wd, "a.txt");
    let (payload, _) = eventually("a commit checkpoint", WAIT, || checkpoints(&h).pop()).await;
    assert_eq!(payload.trigger, CheckpointTrigger::Commit);
    assert_eq!(payload.commit, second);
    assert!(!payload.pushed, "nothing is pushed automatically");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(checkpoints(&h).len(), 1, "one per worktree");

    // Nothing changed: the interval passes without a checkpoint.
    tokio::time::sleep(Duration::from_millis(1800)).await;
    assert_eq!(checkpoints(&h).len(), 1, "unchanged, skipped");
    // A file changes: the next interval sends it.
    std::fs::write(wd.join(".senate/PROGRESS.md"), "- 写重试\n").unwrap();
    let (payload, _) = eventually("an interval checkpoint", WAIT, || {
        checkpoints(&h)
            .pop()
            .filter(|(p, _)| p.trigger == CheckpointTrigger::Interval)
    })
    .await;
    assert_eq!(payload.files.progress.as_deref(), Some("- 写重试\n"));

    // The session ends after another change.
    std::fs::write(wd.join(".senate/OPEN.md"), "- seq 9\n").unwrap();
    h.hook(HookKind::SessionEnd, "s1", &wd, None).await;
    let (payload, _) = eventually("a session-end checkpoint", WAIT, || {
        checkpoints(&h)
            .pop()
            .filter(|(p, _)| p.trigger == CheckpointTrigger::SessionEnd)
    })
    .await;
    assert_eq!(payload.files.open.as_deref(), Some("- seq 9\n"));
    // Ending the other session changes nothing: skipped.
    let before = checkpoints(&h).len();
    h.hook(HookKind::SessionEnd, "s2", &wd, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(checkpoints(&h).len(), before);
    h.stop().await;
}

/// Two agents of one person in one worktree (both on the person's branch)
/// each write their own checkpoint of a new commit, and each one's "already
/// said" is judged against its own latest checkpoint, not the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_agents_in_one_worktree_checkpoint_as_themselves() {
    let mut h = Harness::new().await;
    h.start_with(senate_bridge::Timing {
        checkpoint_poll: Duration::from_millis(150),
        checkpoint_interval: Duration::from_secs(3600),
        agy_nudge_quiet: Duration::from_secs(10),
        ..fast_timing()
    })
    .await;
    let wd = repo(&h, "w1");
    h.hook(HookKind::SessionStart, "cc", &wd, None).await;
    let mut env = h.env_for("rv", None);
    env.agent = Some("reviewer".into());
    h.hook_with(HookKind::SessionStart, "rv", &wd, &env).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The tool, from one agent: only that agent's checkpoint.
    let reply = checkpoint(&h, "rv", Some("- review\n"), false).await;
    let (_, event) = checkpoints(&h).pop().unwrap();
    assert_eq!(event.seq, reply.posted.seq);
    assert_eq!(event.from, senator("reviewer"));

    let head = commit(&wd, "b.txt");
    let both = eventually("a commit checkpoint from each agent", WAIT, || {
        let from: Vec<String> = checkpoints(&h)
            .into_iter()
            .filter(|(p, _)| p.trigger == CheckpointTrigger::Commit && p.commit == head)
            .map(|(_, e)| e.from.to_string())
            .collect();
        (from.len() == 2).then_some(from)
    })
    .await;
    assert!(both.contains(&"senator::claude".to_string()), "{both:?}");
    assert!(both.contains(&"senator::reviewer".to_string()), "{both:?}");
    // Both sessions run on the person's one branch.
    for (p, _) in checkpoints(&h) {
        assert_eq!(p.branch, "feature/refund-v2/alice");
    }
    h.stop().await;
}
