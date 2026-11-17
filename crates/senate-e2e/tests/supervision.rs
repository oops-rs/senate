//! The Speaker under the daemon's supervision: a failure while it works
//! does not lose what it was doing, and nothing it did is done twice.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use senate_core::Kind;
use senate_speaker::{Intent, RelayKind};
use senated::testing::Message;

/// A platform outage and then a bug on the platform side, while the Speaker
/// acknowledges a relay. The outage stalls the Speaker, which retries; the
/// bug (a panic inside the surface) takes the Speaker down, and the daemon's
/// supervisor starts it again from its cursor. The relay is appended once,
/// interpreted once, and the acknowledgement lands once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_platform_neither_loses_nor_repeats_a_relay() {
    let world = World::start(60).await;
    world.bind().await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &inbox).await;
    world.wait_connected("claude", true).await;

    world.fake().fail_next_posts(2);
    world.fake().panic_next_posts(1);
    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Note,
        body: "先别合并".into(),
    }));
    world
        .say(Message::mention(
            GROUP,
            "u_pm",
            "m_pm_1",
            "告诉 alice 的 agent，先别合并",
        ))
        .await;
    inbox.wait_for("先别合并").await;
    world
        .wait_post("the acknowledgement, in the end", |p| {
            p.replies_to("m_pm_1") && p.markdown.contains("[senator::claude]")
        })
        .await;
    world.settle().await;
    let relays = world
        .events()
        .await
        .into_iter()
        .filter(|e| e.kind == Kind::Note && e.body == "先别合并")
        .count();
    assert_eq!(relays, 1, "relayed once");
    let acks = world
        .posts()
        .into_iter()
        .filter(|p| p.replies_to("m_pm_1"))
        .count();
    assert_eq!(acks, 1, "acknowledged once");
    assert_eq!(world.brain.utterances().len(), 1, "interpreted once");
    alice.close().await;
    world.stop().await;
}
