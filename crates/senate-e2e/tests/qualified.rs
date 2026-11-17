//! A qualified name reaches a seat on another committee: a person on
//! `refund-v2` writes `senate ask ibot::codex "…"`, and the question lands
//! on committee `ibot`, addressed to `senator::codex`, at codex's session
//! there. Nothing of it is written on `refund-v2`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use senate_bridge::ipc::SayKind;
use senate_core::{CommitteeId, Hub, Kind};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_asks_an_agent_on_another_committee_by_its_qualified_name() {
    let world = Setup::new().without_chat().start().await;
    let ibot = CommitteeId::new("ibot").unwrap();

    // Bob's machine runs `codex` (a Claude Code session named so) in a
    // checkout bound to `ibot`.
    let mut bob = world.laptop("bob");
    bob.config.agents.claude = path("codex");
    std::fs::write(bob.work.join(".senate/committee"), "ibot\n").unwrap();
    bob.open().await;
    let codex = bob.inbox("s-codex");
    bob.session_start("s-codex", &codex).await.unwrap();
    eventually_async("codex on ibot", || async {
        world
            .hub()
            .is_agent_connected(&ibot, &path("codex"))
            .await
            .then_some(())
    })
    .await;
    // Something is said there, so the committee is in the log.
    bob.post("s-codex", "ibot 的重试在 retry.rs").await;

    // Alice, on refund-v2, asks it by its qualified name.
    let mut alice = world.laptop("alice");
    alice.open().await;
    let sent = senate_bridge::speak::send(
        &alice.paths.socket(),
        committee(),
        SayKind::Question,
        "ibot::codex",
        "ibot 的重试策略是什么？".into(),
    )
    .await
    .unwrap();
    assert_eq!(sent.wrote.to_string(), "ibot::senator::codex");
    assert_eq!(sent.posted.to, [senator("codex")]);

    // It is on ibot, from alice, to codex; and at codex's session.
    let events = world.events_of(&ibot).await;
    let question = events
        .iter()
        .find(|e| e.body == "ibot 的重试策略是什么？")
        .expect("on ibot");
    assert_eq!(question.kind, Kind::Question);
    assert_eq!(question.from, person("alice"));
    assert_eq!(question.to, [senator("codex")]);
    assert_eq!(question.seq, sent.posted.seq);
    let push = codex.wait_for("ibot 的重试策略").await;
    assert_eq!(thread_of(&push, "ibot 的重试策略"), sent.posted.seq);
    assert!(
        world
            .events()
            .await
            .iter()
            .all(|e| !e.body.contains("ibot 的重试策略")),
        "nothing on refund-v2"
    );
    alice.close().await;
    bob.close().await;
    world.stop().await;
}
