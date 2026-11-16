//! A Senate one level down: `senated` presided over by one of a person's
//! agents (`alice/claude`), where that agent's own subagents sit
//! (`alice/claude/reviewer`). There is no built-in Speaker here: the
//! presiding senator is the Speaker, and speaks upward in its own name.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use senate_bridge::ipc::ClientError;
use senate_core::Hub;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subagents_deliberate_one_level_down() {
    let world = Setup::new()
        .without_speaker()
        .server(|config| {
            config.roster.presider = Some(path("alice/claude"));
        })
        .start()
        .await;
    let roster = world.hub().roster().await.unwrap();
    assert_eq!(roster.presider(), Some(&path("alice/claude")));

    let mut alice = world.laptop("alice");
    alice.open().await;
    let reviewer = alice.inbox("s-reviewer");
    let tester = alice.inbox("s-tester");
    let start = alice
        .session_start_as("s-reviewer", &reviewer, "reviewer")
        .await
        .expect("orientation");
    assert!(
        context(&start).contains("[senator::alice/claude/reviewer]"),
        "seated under the presider: {}",
        context(&start)
    );
    alice
        .session_start_as("s-tester", &tester, "tester")
        .await
        .expect("orientation");
    world.wait_connected("alice/claude/reviewer", true).await;
    world.wait_connected("alice/claude/tester", true).await;

    let asked = alice
        .ask(
            "s-reviewer",
            senator("alice/claude/tester"),
            "回调的边界用例覆盖了吗？",
        )
        .await;
    let push = tester.wait_for("回调的边界用例").await;
    assert_eq!(thread_of(&push, "回调的边界用例"), asked.seq);
    alice
        .answer("s-tester", asked.seq, "覆盖了，见 tests/callback.rs")
        .await;
    reviewer.wait_for("tests/callback.rs").await;

    // "Alice's agent" here means one of the presider's subagents: both
    // are candidates, so an agent must name one.
    let refused = alice
        .try_ask("s-reviewer", agent_of("alice"), "谁在看回调？")
        .await
        .expect_err("two subagents");
    let ClientError::Remote(refused) = refused else {
        panic!("{refused}")
    };
    assert_eq!(
        refused.candidates,
        [path("alice/claude/reviewer"), path("alice/claude/tester")]
    );
    alice.close().await;
    world.stop().await;
}
