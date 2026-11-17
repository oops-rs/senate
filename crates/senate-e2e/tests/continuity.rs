//! A developer on leave is covered: a checkpoint outlives the laptop,
//! presence is visible, the Speaker answers for an absent agent from that
//! agent's own checkpoint (or sends the question to people), hands the
//! mailbox back, and a companion session answers when no session is open.
//!
//! Presence timings are shrunk through the server config
//! (`presence_disconnected_secs` / `lease_secs`), so "after 2 minutes" and
//! "after 15 minutes" are a second or two here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::*;
use senate_core::{
    Address, CheckpointPayload, CheckpointTrigger, Event, Hub, Kind, PresencePayload,
    PresenceState, meta,
};
use senated::testing::{Message, Post};

const BRANCH: &str = "feature/refund-v2/alice";

/// Presence in seconds rather than minutes: `disconnected` after 1 s
/// without a heartbeat, `absent` after `lease`.
fn quick_presence(lease: u64) -> Setup {
    Setup::new().server(move |config| {
        config.presence_disconnected_secs = 1;
        config.lease_secs = lease;
    })
}

/// The server's presence transitions for `agent`, in log order.
async fn presence_of(world: &World, agent: &str) -> Vec<(PresencePayload, Event)> {
    let agent = path(agent);
    world
        .events()
        .await
        .into_iter()
        .filter_map(|e| Some((PresencePayload::of(&e)?, e)))
        .filter(|(p, _)| p.agent == agent)
        .collect()
}

async fn wait_presence(world: &World, agent: &str, state: PresenceState) -> Event {
    eventually_async(&format!("{agent} {state:?} in the log"), || async {
        presence_of(world, agent)
            .await
            .into_iter()
            .rev()
            .find(|(p, _)| p.state == state)
            .map(|(_, e)| e)
    })
    .await
}

/// A root line in the Agent group that says `text`.
async fn agent_line(world: &World, text: &str) -> Post {
    world
        .wait_post(&format!("{text:?} in the Agent group"), |p| {
            p.is_root_in(AGENTS) && p.markdown.contains(text)
        })
        .await
}

/// The PROGRESS.md Alice's agent keeps: it covers the idempotency header.
const PROGRESS: &str = "## 完成\n- 网关幂等键用 Idempotency-Key header，已合入\n\n## 进行中\n- 退款回调\n\n## 下一步\n- 对账脚本";

/// Alice's laptop, a git checkout on her branch with one commit, open, with
/// session `s-alice` running. Returns the laptop, its inbox and `HEAD`.
async fn alice_at_work(world: &World) -> (Laptop, FakeInbox, String) {
    let mut alice = world.laptop("alice");
    let head = alice.git_init(BRANCH);
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &inbox).await;
    (alice, inbox, head)
}

/// A checkpoint survives the laptop: the `checkpoint` tool sends the
/// branch, commit and the three files; the brief lists the latest; the
/// Agent group gets one line, for the hand-off only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_survives_the_laptop() {
    let world = World::start(60).await;
    world.with_agent_group().await;
    let (mut alice, _inbox, head) = alice_at_work(&world).await;

    let reply = alice
        .checkpoint(
            "s-alice",
            Some(PROGRESS),
            Some("- 幂等键用 header 而不是 body 字段（seq 3）"),
            Some("- 等 bob：回调签名用哪个密钥"),
        )
        .await;
    assert_eq!(reply.branch, BRANCH);
    assert_eq!(reply.commit, head);
    assert!(!reply.pushed, "no remote");
    assert_eq!(reply.files, ["PROGRESS.md", "DECISIONS.md", "OPEN.md"]);
    assert!(
        std::fs::read_to_string(alice.work.join(".senate/PROGRESS.md"))
            .unwrap()
            .contains("Idempotency-Key"),
        "the files live in the checkout"
    );
    // Then a hand-off (no remote here: the push fails, the checkpoint is
    // sent anyway).
    let handoff = alice.handoff("s-alice", PROGRESS).await;
    assert!(!handoff.pushed, "no remote");

    // The log has it, files and all: it outlives the laptop.
    alice.close().await;
    let event = world
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Checkpoint)
        .expect("a checkpoint event");
    assert_eq!(event.seq, reply.posted.seq);
    assert_eq!(event.from, senator("claude"));
    let payload = CheckpointPayload::of(&event).expect("a payload");
    assert_eq!(payload.branch, BRANCH);
    assert_eq!(payload.commit, head);
    assert_eq!(payload.trigger, CheckpointTrigger::Tool);
    assert!(
        payload
            .files
            .progress
            .as_deref()
            .unwrap()
            .contains("Idempotency-Key")
    );
    assert!(
        payload
            .files
            .decisions
            .as_deref()
            .unwrap()
            .contains("seq 3")
    );
    assert!(payload.files.open.as_deref().unwrap().contains("回调签名"));

    // The brief lists the latest, for anyone.
    let brief = world.hub().brief(&committee()).await.unwrap();
    let listed = brief
        .checkpoints
        .iter()
        .find(|c| c.agent == path("claude"))
        .expect("alice's checkpoint in the brief");
    assert_eq!(listed.seq, handoff.posted.seq);
    let markdown = brief.to_markdown(world.locale, 10_000);
    let short = &head[..7];
    assert!(
        markdown.contains(&format!("[senator::claude] 最近检查点 · {BRANCH}@{short}")),
        "{markdown}"
    );

    // One line in the Agent group, for the hand-off: the Speaker has read
    // the tool's checkpoint before it, and posted nothing for it.
    agent_line(
        &world,
        &format!(
            "[senator::claude] 检查点 · {BRANCH}@{short}（未推送）· seq {}",
            handoff.posted.seq
        ),
    )
    .await;
    assert_eq!(
        world
            .posts()
            .iter()
            .filter(|p| p.markdown.contains("检查点 ·"))
            .count(),
        1,
        "only the hand-off is a line, and only in the Agent group"
    );
    world.stop().await;
}

/// Presence is visible: closing the laptop is `失联`, then `缺席`, in the
/// log and the Agent group, and `/status` agrees; reopening is `在线`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presence_is_visible() {
    let world = quick_presence(3).start().await;
    world.with_agent_group().await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &inbox).await;
    eventually_async("alice online on the server", || async {
        let page = world.hub().presence(&committee()).await.ok()?;
        page.agents
            .iter()
            .any(|p| p.agent == path("claude") && p.state == PresenceState::Online)
            .then_some(())
    })
    .await;
    assert!(
        presence_of(&world, "claude").await.is_empty(),
        "coming online the first time writes nothing"
    );

    alice.close().await;
    wait_presence(&world, "claude", PresenceState::Absent).await;
    let states: Vec<PresenceState> = presence_of(&world, "claude")
        .await
        .into_iter()
        .map(|(p, _)| p.state)
        .collect();
    assert_eq!(
        states,
        [PresenceState::Disconnected, PresenceState::Absent],
        "disconnected first, then absent"
    );
    let gone = agent_line(&world, "[senator::claude] 失联").await;
    let absent = agent_line(&world, "[senator::claude] 缺席（租约过期）").await;
    let posts = world.posts();
    let at = |p: &Post| posts.iter().position(|q| q.message_ref == p.message_ref);
    assert!(at(&gone) < at(&absent), "in order");

    // `/status` in chat agrees.
    world
        .say(Message::mention(GROUP, "u_pm", "m_status", "/status"))
        .await;
    let status = world
        .wait_post("the /status reply", |p| {
            p.markdown.contains("本群绑定在委员会") && p.markdown.contains("[senator::claude]")
        })
        .await;
    assert!(
        status
            .markdown
            .contains("[senator::claude] 缺席（租约过期）"),
        "{}",
        status.markdown
    );

    // She reopens.
    alice.open().await;
    let online = wait_presence(&world, "claude", PresenceState::Online).await;
    assert_eq!(
        PresencePayload::of(&online).unwrap().previous,
        Some(PresenceState::Absent)
    );
    agent_line(&world, "[senator::claude] 在线").await;
    alice.close().await;
    world.stop().await;
}

/// Covered while away: with Alice absent the Speaker answers from her
/// checkpoint as `[speaker::claude] 接管`; a question her files do not
/// cover goes to Bob; when she is back, her session gets one message
/// listing what was answered for her.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn covered_while_away() {
    let world = quick_presence(2).start().await;
    world.with_agent_group().await;
    let (mut alice, alice_inbox, _) = alice_at_work(&world).await;
    let checkpoint = alice
        .checkpoint("s-alice", Some(PROGRESS), None, None)
        .await
        .posted
        .seq;
    let mut bob = world.laptop("bob");
    bob.open().await;
    let bob_inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &bob_inbox).await;

    alice.close().await;
    wait_presence(&world, "claude", PresenceState::Absent).await;

    // Covered by PROGRESS.md.
    world.brain.answers(Ok(format!(
        "用 Idempotency-Key header，已合入（检查点 seq {checkpoint}）"
    )));
    let covered = bob
        .ask("s-bob", senator("claude"), "网关幂等键用哪个 header？")
        .await;
    let post = world
        .wait_post("the takeover answer", |p| {
            p.markdown.contains("[speaker::claude] 接管")
        })
        .await;
    assert!(
        post.markdown.contains(&format!(
            "Idempotency-Key header，已合入（检查点 seq {checkpoint}）"
        )),
        "{}",
        post.markdown
    );
    let events = world.events().await;
    let answer = events
        .iter()
        .find(|e| e.kind == Kind::Answer && e.thread == Some(covered.seq))
        .expect("the answer in the log");
    assert_eq!(answer.from, Address::Speaker);
    assert_eq!(answer.on_behalf_of, Some(senator("claude")));
    let asked = world.brain.questions();
    let takeover = asked[0].takeover.as_ref().expect("asked as a takeover");
    assert_eq!(takeover.checkpoint.seq, checkpoint);
    wait_presence(&world, "claude", PresenceState::TakenOver).await;
    agent_line(&world, "[senator::claude] 由 Speaker 接管").await;
    bob_inbox.wait_for("已合入").await;

    // Not covered: sent to the committee's other developer, bob.
    world
        .brain
        .answers_uncovered("检查点里没有回调签名的信息。");
    let uncovered = bob
        .ask("s-bob", senator("claude"), "回调签名用哪个密钥？")
        .await;
    let escalation = world
        .wait_post("bob mentioned about the uncovered question", |p| {
            p.mention.as_deref() == Some("u_bob") && p.markdown.contains("回调签名用哪个密钥")
        })
        .await;
    assert!(
        escalation
            .markdown
            .contains("[senator::claude] 不在，检查点没有覆盖这个问题，这条需要人"),
        "{}",
        escalation.markdown
    );
    assert!(
        !world
            .events()
            .await
            .iter()
            .any(|e| e.kind == Kind::Answer && e.thread == Some(uncovered.seq)),
        "no guessed answer"
    );

    // Alice comes back: one message hands the mailbox back.
    alice.open().await;
    let handback = alice_inbox.wait_for("代你回答了").await;
    assert!(handback.contains("代你回答了 1 条"), "{handback}");
    assert!(
        handback.contains(&format!("seq {}", covered.seq)) && handback.contains("网关幂等键"),
        "{handback}"
    );
    agent_line(&world, "[senator::claude] 回来了，Speaker 交还 1 条代答").await;
    let notes = alice_inbox
        .messages()
        .iter()
        .filter(|m| m.contains("代你回答了"))
        .count();
    assert_eq!(notes, 1, "one message");
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// A `claude` stand-in for the companion: records its session id (the
/// companion's, from the environment) under `dir`, then waits for
/// `dir/done` (at most ~20 s) and exits like `claude -p` does.
fn fake_claude(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let bin = dir.join("claude");
    let d = dir.display();
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > '{d}/args'\n\
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

/// The companion answers: Alice's bridge runs, her session is closed; a
/// question to her agent goes to a fork of her last session, whose answer
/// is labelled `[senator::claude] 伴随`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_companion_answers() {
    let world = World::start(60).await;
    world.with_agent_group().await;
    let mut alice = world.laptop("alice");
    let dump = world.dir.path().join("claude-dump");
    alice.enable_companion(&fake_claude(&dump), 30);
    alice.open().await;
    let old = alice.inbox("s-old");
    alice.session_start("s-old", &old).await;
    alice.session_end("s-old", &old).await;

    let mut bob = world.laptop("bob");
    bob.open().await;
    let bob_inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &bob_inbox).await;
    let asked = bob
        .ask("s-bob", senator("claude"), "幂等键用哪个 header？")
        .await;

    // The fork of s-old gets the question...
    let sid = eventually("a companion session", || {
        std::fs::read_to_string(dump.join("sid")).ok()
    })
    .await;
    assert!(sid.starts_with("companion-"), "{sid}");
    let args = std::fs::read_to_string(dump.join("args")).unwrap();
    assert_eq!(
        args.lines().take(4).collect::<Vec<_>>(),
        ["-p", "--resume", "s-old", "--fork-session"]
    );
    let prompt = std::fs::read_to_string(dump.join("prompt")).unwrap();
    assert!(prompt.contains("幂等键用哪个 header？"), "{prompt}");

    // ...and answers through its MCP shim, as the companion.
    let posted = alice
        .answer(&sid, asked.seq, "Idempotency-Key（见 gateway/src/idem.rs）")
        .await;
    std::fs::write(dump.join("done"), "").unwrap();
    let answer = world
        .events()
        .await
        .into_iter()
        .find(|e| e.seq == posted.seq)
        .expect("the answer in the log");
    assert_eq!(answer.from, senator("claude"));
    assert_eq!(answer.thread, Some(asked.seq));
    assert_eq!(
        answer.meta.get(meta::COMPANION),
        Some(&serde_json::json!(true))
    );
    world
        .wait_post("the companion's answer mirrored", |p| {
            p.markdown.contains("[senator::claude] 伴随") && p.markdown.contains("Idempotency-Key")
        })
        .await;
    bob_inbox.wait_for("gateway/src/idem.rs").await;
    assert!(
        old.messages().iter().all(|m| !m.contains("幂等键用哪个")),
        "the closed session is never pushed to"
    );
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// `.senate/` follows the branch: a session on branch A checkpoints; the
/// worktree switches to B, which has another committee and its own notes;
/// a session there is bound to B's committee and its checkpoint carries B's
/// notes; switching back restores A and the next checkpoint matches A's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_carries_the_notes_and_committee_of_the_branch_checked_out() {
    use senate_bridge::binding::{self, Settings};
    let world = World::start(60).await;
    let (mut alice, _inbox, head_a) = alice_at_work(&world).await;
    let other = senate_core::CommitteeId::new("other-work").unwrap();
    let settings = Settings::default();

    // Branch A, bound to refund-v2 by its worktree file: a checkpoint.
    let on_a = alice
        .checkpoint(
            "s-alice",
            Some("## A\n- retry limit\n"),
            Some("- A decided"),
            Some("- A waits"),
        )
        .await;
    assert_eq!(on_a.branch, BRANCH);
    assert_eq!(on_a.posted.committee, committee());
    let a_files = std::fs::read(alice.work.join(".senate/PROGRESS.md")).unwrap();

    // The worktree moves to a new branch B, and B is bound to another
    // committee (`senate join`, which swaps first).
    git(&alice.work, &["switch", "-q", "-c", "other-work/alice"]);
    binding::join(&alice.paths, &alice.work, &other, &settings)
        .await
        .unwrap();
    assert!(
        !alice.work.join(".senate/PROGRESS.md").exists(),
        "A's notes are not on B"
    );
    // A session starting there is bound to B's committee at its first hook.
    let inbox_b = alice.inbox("s-b");
    alice.session_start("s-b", &inbox_b).await;
    let on_b = alice
        .checkpoint(
            "s-b",
            Some("## B\n- other work\n"),
            Some("- B decided"),
            Some("- B waits"),
        )
        .await;
    assert_eq!(on_b.branch, "other-work/alice");
    assert_eq!(on_b.posted.committee, other, "B's committee");
    let b_event = world
        .events_of(&other)
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Checkpoint)
        .expect("a checkpoint on B's committee");
    let b_payload = CheckpointPayload::of(&b_event).unwrap();
    assert_eq!(b_payload.branch, "other-work/alice");
    assert_eq!(
        b_payload.files.progress.as_deref(),
        Some("## B\n- other work\n")
    );
    assert_eq!(b_payload.files.open.as_deref(), Some("- B waits\n"));
    assert_ne!(b_payload.commit, "", "a commit");

    // Back to A with no command run: the first session's next checkpoint is
    // A's, on A's committee, with A's notes exactly as they were.
    git(&alice.work, &["switch", "-q", BRANCH]);
    let again = alice.checkpoint("s-alice", None, None, None).await;
    assert_eq!(again.branch, BRANCH);
    assert_eq!(again.commit, head_a);
    assert_eq!(again.posted.committee, committee());
    assert_eq!(
        std::fs::read(alice.work.join(".senate/PROGRESS.md")).unwrap(),
        a_files
    );
    let event = world
        .events()
        .await
        .into_iter()
        .rfind(|e| e.kind == Kind::Checkpoint)
        .unwrap();
    let payload = CheckpointPayload::of(&event).unwrap();
    assert_eq!(
        payload.files.progress.as_deref(),
        Some("## A\n- retry limit\n")
    );
    assert_eq!(payload.files.decisions.as_deref(), Some("- A decided\n"));
    // And none of B's ever reached A's committee.
    for e in world.events().await {
        if let Some(p) = CheckpointPayload::of(&e) {
            assert!(!p.files.progress.unwrap_or_default().contains("## B"));
        }
    }
    alice.close().await;
    world.stop().await;
}
