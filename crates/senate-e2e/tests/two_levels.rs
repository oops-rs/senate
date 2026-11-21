//! A team's Senate with its built-in Speaker (a scripted brain) and two
//! people, alice and bob; alice's machine runs `senated --machine` with no
//! Senate of its own: its bridge talks to the team's server directly, and
//! the agents it runs, `claude` (a Claude Code session) and `codex` (a
//! Codex session), are seated there under flat names, run by alice. Bob's
//! Claude Code agent there is `copilot`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::time::Duration;

use common::fake_codex::{FakeCodex, input_text};
use common::*;
use senate_bridge::ipc::{InboxReply, Request, SessionSelector};
use senate_core::{Address, Hub, Kind, Locale, NewEvent, SpeakerSeat, meta};

/// `senate.toml` of a machine on the team's Senate; `extra`: more of its
/// top-level keys.
fn on_the_team(world: &World, extra: &str) -> String {
    format!(
        "server_url = \"{}\"\ntoken = \"tok-alice\"\n{extra}",
        world.server_url()
    )
}

fn codex_pushes(codex: &FakeCodex) -> Vec<String> {
    codex
        .pushes()
        .iter()
        .map(|(_, params)| input_text(params).to_owned())
        .collect()
}

/// Alice's machine with its two sessions on the team's committee.
async fn alice_at_her_desk(
    world: &World,
    dir: &std::path::Path,
    extra: &str,
) -> (Desk, FakeInbox, FakeCodex) {
    let mut desk = Desk::new(dir, "alice", Locale::Zh, &on_the_team(world, extra));
    desk.git_init("senate/refund-v2/alice");
    let codex = desk.codex_app_server();
    desk.start().await;
    assert!(desk.machine().senate().is_none(), "it hosts no Senate");
    let claude = desk.inbox("s-claude");
    let start = desk.session_start("s-claude", &claude).await.unwrap();
    assert!(
        context(&start).contains("[senator::claude]"),
        "{}",
        context(&start)
    );
    assert!(
        !context(&start).contains("由这个会话主持"),
        "the team's Speaker is built in: {}",
        context(&start)
    );
    let start = desk.codex_start("thread-codex").await.unwrap();
    assert!(context(&start).contains("[senator::codex]"));
    codex.idle("thread-codex");
    world.wait_connected("claude", true).await;
    world.wait_connected("codex", true).await;
    (desk, claude, codex)
}

async fn bob_at_work(world: &World) -> (Laptop, FakeInbox) {
    let mut bob = world.laptop("bob");
    bob.open().await;
    let inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &inbox).await;
    world.wait_connected(BOB_CLAUDE, true).await;
    (bob, inbox)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_question_from_the_team_is_answered_by_alices_agent() {
    let world = Setup::new().without_chat().start().await;
    assert_eq!(
        me_at(&world.server_url(), "tok-alice").await.speaker,
        Some(SpeakerSeat::BuiltIn)
    );
    let dir = tempfile::tempdir().unwrap();
    let (mut desk, claude, codex) = alice_at_her_desk(&world, dir.path(), "").await;
    let (mut bob, bob_inbox) = bob_at_work(&world).await;

    // Seated flat, each run by the person whose machine runs it.
    let roster = world.hub().roster().await.unwrap();
    assert_eq!(roster.owner_of(&path("claude")), Some(&name("alice")));
    assert_eq!(roster.owner_of(&path("codex")), Some(&name("alice")));
    assert_eq!(roster.owner_of(&path(BOB_CLAUDE)), Some(&name("bob")));

    // Bob's agent asks alice's claude.
    let asked = bob
        .ask("s-bob", senator("claude"), "退款回调重试几次？")
        .await;
    claude.wait_for("退款回调重试几次").await;

    // claude consults codex, its sibling on the same committee: the same
    // person's agent, delivered at once.
    let consulted = desk
        .ask(
            "s-claude",
            senator("codex"),
            "回调重试的上限在代码里是多少？",
        )
        .await;
    eventually("codex is asked", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("回调重试的上限"))
    })
    .await;
    desk.answer("thread-codex", consulted.seq, "3 次，见 refund/retry.rs")
        .await;
    claude.wait_for("3 次，见 refund/retry.rs").await;

    // It answers Bob, in its own name.
    desk.answer("s-claude", asked.seq, "重试 3 次（refund/retry.rs）")
        .await;
    bob_inbox.wait_for("重试 3 次（refund/retry.rs）").await;
    let events = world.events().await;
    let answer = events
        .iter()
        .find(|e| e.body == "重试 3 次（refund/retry.rs）")
        .unwrap();
    assert_eq!(answer.from, senator("claude"));

    // Authority: bob may not speak as an agent alice runs, nor alice as
    // bob's; nobody writes as the built-in Speaker.
    for (token, from) in [("tok-bob", "claude"), ("tok-alice", BOB_CLAUDE)] {
        let (status, _) = speak(
            &world.server_url(),
            token,
            &NewEvent::new(committee(), Kind::Note, senator(from), "我是谁？"),
        )
        .await
        .unwrap_err();
        assert_eq!(status, 403, "{token} as {from}");
    }
    let as_speaker = NewEvent::new(committee(), Kind::Note, Address::Speaker, "x")
        .with_meta(meta::BRIEF_TOP, serde_json::json!(true));
    for token in ["tok-alice", "tok-bob"] {
        let (status, _) = speak(&world.server_url(), token, &as_speaker)
            .await
            .unwrap_err();
        assert_eq!(status, 403, "{token}");
    }

    bob.close().await;
    desk.stop().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_team_and_the_machine_survive_a_restart() {
    let mut world = Setup::new().without_chat().sqlite().start().await;
    let dir = tempfile::tempdir().unwrap();
    let (mut desk, claude, codex) = alice_at_her_desk(&world, dir.path(), "").await;

    // claude asks codex; codex leaves a checkpoint.
    let asked = desk.ask("s-claude", senator("codex"), "覆盖了吗？").await;
    eventually("codex is asked", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("覆盖了吗"))
    })
    .await;
    desk.checkpoint("thread-codex", Some("回调测试已覆盖"), None, None)
        .await;
    let hub = world.hub().clone();
    assert!(
        hub.latest_checkpoint(&committee(), &path("codex"))
            .await
            .unwrap()
            .is_some()
    );
    let head = hub.head(&committee()).await.unwrap();
    // claude's delivered cursor is kept on the team's server.
    eventually_async("claude's cursor", || {
        let hub = hub.clone();
        async move {
            let seq = hub
                .owned_cursor(&committee(), &path("claude"), "delivered")
                .await
                .ok()?;
            (seq == head).then_some(())
        }
    })
    .await;

    // Both restart: the team's server, then alice's machine.
    desk.stop().await;
    world.restart().await;
    desk.start().await;
    let hub = world.hub().clone();
    assert!(hub.head(&committee()).await.unwrap() >= head);
    let roster = hub.roster().await.unwrap();
    for agent in ["claude", "codex"] {
        assert_eq!(
            roster.owner_of(&path(agent)),
            Some(&name("alice")),
            "{agent} seated again, run by alice"
        );
    }
    assert!(
        hub.latest_checkpoint(&committee(), &path("codex"))
            .await
            .unwrap()
            .is_some()
    );

    // The sessions come back through their hooks; nothing is pushed twice.
    desk.prompt("s-claude", &claude, None).await;
    desk.codex_start("thread-codex").await.unwrap();
    codex.idle("thread-codex");
    world.wait_connected("claude", true).await;
    world.wait_connected("codex", true).await;
    desk.answer("thread-codex", asked.seq, "覆盖了，见 tests/callback.rs")
        .await;
    claude.wait_for("覆盖了，见 tests/callback.rs").await;
    let asked_pushes = codex_pushes(&codex)
        .iter()
        .filter(|m| m.contains("覆盖了吗？"))
        .count();
    assert_eq!(asked_pushes, 1, "the question was pushed once");
    desk.stop().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_alices_session_away_her_other_agent_works_and_she_sees_what_waits() {
    let world = Setup::new().without_chat().start().await;
    let dir = tempfile::tempdir().unwrap();
    // The committee's stream stays open with no session on it, so what is
    // addressed to her agents waits on her machine.
    let (mut desk, claude, codex) =
        alice_at_her_desk(&world, dir.path(), "committees = [\"refund-v2\"]\n").await;
    let (mut bob, _bob_inbox) = bob_at_work(&world).await;

    // claude's session closes.
    desk.session_end("s-claude", &claude).await;

    // A question for codex is delivered at once.
    let started = std::time::Instant::now();
    bob.ask("s-bob", senator("codex"), "日志格式定了吗？").await;
    eventually("codex is asked", || {
        codex_pushes(&codex)
            .into_iter()
            .find(|p| p.contains("日志格式定了吗"))
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "delivered in {:?}",
        started.elapsed()
    );

    // A question for claude waits, and alice sees it in her inbox.
    let waiting = bob.ask("s-bob", senator("claude"), "谁来看重试？").await;
    let inbox: InboxReply = eventually_async("alice's inbox lists it", || async {
        let reply: InboxReply = desk
            .ipc(Request::Inbox {
                session: SessionSelector::default(),
                committee: Some(committee()),
                agent: None,
                all: false,
            })
            .await;
        reply
            .threads
            .iter()
            .any(|t| t.root == waiting.seq)
            .then_some(reply)
    })
    .await;
    let shown =
        senate_bridge::person::render_inbox(&inbox, Locale::Zh, senate_bridge::mcp::ShowAll::Flag);
    assert!(shown.contains("谁来看重试？"), "{shown}");

    // When claude's session is back, it is handed the question as it starts.
    let back = desk.session_start("s-claude", &claude).await.unwrap();
    assert!(
        context(&back).contains("谁来看重试？"),
        "{}",
        context(&back)
    );
    bob.close().await;
    desk.stop().await;
    world.stop().await;
}
