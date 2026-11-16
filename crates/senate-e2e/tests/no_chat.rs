//! `senated` with no chat platform mounted, as the binary runs it: agents
//! deliberate through the log alone, and the Speaker still triages and
//! covers for an absent agent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use senate_core::{Address, Kind, PresenceState, meta};

/// An agent asks another; the question is triaged, delivered and answered,
/// and the answer reaches the asker.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_asks_another_and_the_answer_returns() {
    let world = Setup::new().without_chat().start().await;
    assert!(world.daemon().has_speaker());
    let mut alice = world.laptop("alice");
    let mut bob = world.laptop("bob");
    alice.open().await;
    bob.open().await;
    let alice_inbox = alice.inbox("s-alice");
    let bob_inbox = bob.inbox("s-bob");
    alice.session_start("s-alice", &alice_inbox).await;
    bob.session_start("s-bob", &bob_inbox).await;
    world.wait_connected("alice/claude", true).await;
    world.wait_connected("bob/claude", true).await;

    let asked = alice
        .ask("s-alice", claude("bob"), "网关幂等用哪个 header？")
        .await;
    let push = bob_inbox.wait_for("网关幂等用哪个 header").await;
    assert_eq!(thread_of(&push, "网关幂等用哪个 header"), asked.seq);
    bob.answer(
        "s-bob",
        asked.seq,
        "Idempotency-Key，见 gateway/src/idem.rs",
    )
    .await;
    alice_inbox.wait_for("Idempotency-Key，见").await;

    let events = world.events().await;
    assert!(
        events
            .iter()
            .any(|e| e.thread == Some(asked.seq) && e.meta.contains_key(meta::TRIAGE)),
        "the Speaker judged the question"
    );
    assert!(world.posts().is_empty(), "there is no platform to post to");
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// Alice is away; the Speaker answers Bob's question from her checkpoint,
/// in the log, and hands her mailbox back when she returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_absent_agent_is_covered_through_the_log() {
    let world = Setup::new()
        .without_chat()
        .server(|config| {
            config.presence_disconnected_secs = 1;
            config.lease_secs = 2;
        })
        .start()
        .await;
    let mut alice = world.laptop("alice");
    alice.git_init("feature/refund-v2/alice");
    alice.open().await;
    let alice_inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &alice_inbox).await;
    let checkpoint = alice
        .checkpoint(
            "s-alice",
            Some("## 完成\n- 网关幂等键用 Idempotency-Key header"),
            None,
            None,
        )
        .await
        .posted
        .seq;
    alice.close().await;
    eventually_async("alice/claude absent", || async {
        world
            .events()
            .await
            .iter()
            .filter_map(senate_core::PresencePayload::of)
            .any(|p| p.agent == path("alice/claude") && p.state == PresenceState::Absent)
            .then_some(())
    })
    .await;

    let mut bob = world.laptop("bob");
    bob.open().await;
    let bob_inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &bob_inbox).await;
    world.brain.answers(Ok(format!(
        "用 Idempotency-Key header（检查点 seq {checkpoint}）"
    )));
    let asked = bob
        .ask("s-bob", claude("alice"), "幂等键用哪个 header？")
        .await;
    let push = bob_inbox.wait_for("Idempotency-Key header（检查点").await;
    assert!(push.contains("[speaker::alice/claude] 接管"), "{push}");
    let answer = world
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Answer && e.thread == Some(asked.seq))
        .expect("the answer in the log");
    assert_eq!(answer.from, Address::Speaker);
    assert_eq!(answer.on_behalf_of, Some(claude("alice")));

    alice.open().await;
    let handback = alice_inbox.wait_for("代你回答了").await;
    assert!(
        handback.contains(&format!("seq {}", asked.seq)),
        "{handback}"
    );
    alice.close().await;
    bob.close().await;
    world.stop().await;
}
