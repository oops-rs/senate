//! The log on SQLite, the store for a single machine: the core loop runs
//! on it, and a restart of the daemon keeps the log, seats again the agents
//! it has seen, and picks up where every reader left off.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use senate_core::api::Me;
use senate_core::{Hub, Kind};
use senate_speaker::{Intent, RelayKind};
use senated::testing::Message;

async fn me(world: &World, token: &str) -> Me {
    reqwest::Client::new()
        .get(format!("{}/v1/me", world.server_url()))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_log_and_seats_the_agents_again() {
    let mut world = Setup::new().sqlite().start().await;
    assert!(world.config.server.database_url.starts_with("sqlite:"));
    world.bind().await;
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
        .ask("s-alice", claude("bob"), "q1: 网关幂等用哪个 header？")
        .await;
    bob_inbox.wait_for("q1:").await;
    bob.answer("s-bob", asked.seq, "a1: Idempotency-Key").await;
    alice_inbox.wait_for("a1:").await;
    world.settle().await;
    let before = world.events().await;
    let epoch = world.hub().epoch().await.unwrap();
    let (alice_seen, bob_seen) = (alice_inbox.messages().len(), bob_inbox.messages().len());

    // Bob's machine is off across the restart: only the log knows his agent.
    bob.close().await;
    world.restart().await;

    assert_eq!(world.hub().epoch().await.unwrap(), epoch, "the same log");
    assert_eq!(world.events().await, before, "every event kept");
    let roster = world.hub().roster().await.unwrap();
    assert!(
        roster.agents().contains(&path("bob/claude")),
        "seated again from the log: {:?}",
        roster.agents()
    );
    assert_eq!(me(&world, "tok-bob").await.agents, [path("bob/claude")]);
    assert_eq!(
        me(&world, "tok-alice").await.committees,
        [committee()],
        "the committee is still there"
    );
    // The gateway routes the group from the log's bindings.
    assert_eq!(
        world.gateway.as_ref().unwrap().committee_of(GROUP),
        Some(committee())
    );
    // Alice's bridge finds the server again on its own.
    world.wait_connected("alice/claude", true).await;

    // The Speaker resumes from its cursor: nothing old is posted again.
    world.settle().await;
    assert!(
        world.posts().is_empty(),
        "nothing re-posted: {:?}",
        world.posts()
    );

    // And everything carries on, with seq continuing.
    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["bob".into()],
        kind: RelayKind::Question,
        body: "q2: 重试上限是几次？".into(),
    }));
    world
        .say(Message::mention(
            GROUP,
            "u_pm",
            "m_pm_q2",
            "问一下 bob 的 agent，q2: 重试上限是几次？",
        ))
        .await;
    bob.open().await;
    bob_inbox.wait_for("q2:").await;
    let relayed = world
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Question && e.body.contains("q2:"))
        .unwrap();
    assert!(relayed.seq > before.last().unwrap().seq);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        alice_inbox.messages().len(),
        alice_seen,
        "nothing delivered twice to alice"
    );
    assert!(
        bob_inbox.messages()[bob_seen..]
            .iter()
            .all(|m| !m.contains("q1:")),
        "nor to bob"
    );
    alice.close().await;
    bob.close().await;
    world.stop().await;
}
