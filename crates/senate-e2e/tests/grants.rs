//! Bind = grant, end to end: a group the platform has no configuration for
//! is bound with `/bind`, the gateway has the platform adopt it, and from
//! then on its unmentioned chat reaches the log; `/agents` adopts the Agent
//! group the same way, and unbinding the last people group releases both.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use senate_core::Kind;
use senated::Delivered;
use senated::testing::{Caps, Message};

/// A platform app that may adopt groups, configured for none: only
/// mentions arrive from a group until it holds a grant.
fn granting() -> Setup {
    Setup::new().platform(|chat| {
        chat.set_caps(Caps {
            grants: true,
            ..Caps::PLAIN
        });
        chat.gate_ambient(&[]);
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_an_unconfigured_group_grants_it_and_unbinding_releases_it() {
    let world = granting().start().await;

    let before = world
        .say(Message::ambient(GROUP, "u_pm", "m_before", "绑定之前"))
        .await;
    assert_eq!(
        before,
        Delivered::Dropped,
        "the platform delivers nothing yet"
    );
    world.bind().await;
    eventually("the people group to be adopted", || {
        world
            .fake()
            .granted()
            .contains(&GROUP.to_owned())
            .then_some(())
    })
    .await;
    assert_eq!(world.fake().adopt_calls(), ["m_bind"]);

    world
        .say(Message::ambient(GROUP, "u_pm", "m_after", "绑定之后"))
        .await;
    assert!(
        world
            .events()
            .await
            .iter()
            .any(|e| e.kind == Kind::Chat && e.body == "绑定之后"),
        "the group's unmentioned chat is recorded"
    );
    assert!(
        !world.events().await.iter().any(|e| e.body == "绑定之前"),
        "nothing from before the grant"
    );

    // The Agent group, adopted with `/agents`, is granted too.
    world.with_agent_group().await;
    eventually("the Agent group to be adopted", || {
        world
            .fake()
            .granted()
            .contains(&AGENTS.to_owned())
            .then_some(())
    })
    .await;
    assert!(world.fake().adopt_calls().contains(&"m_adopt".to_owned()));

    // Unbinding the last people group (fixed syntax: no confirmation)
    // archives the Agent group: both are bound to nothing, so both are
    // released.
    world
        .say(Message::mention(GROUP, "u_pm", "m_unbind", "/unbind"))
        .await;
    eventually("both groups to be released", || {
        world.fake().granted().is_empty().then_some(())
    })
    .await;
    let released = world.fake().release_calls();
    assert!(released.contains(&GROUP.to_owned()), "{released:?}");
    assert!(released.contains(&AGENTS.to_owned()), "{released:?}");
    world.stop().await;
}

/// The platform answers adopts slowly: the Speaker's first root posts into
/// groups it just bound (the "no Agent group" hint, the Agent group's
/// pinned cards) do not wait for the grant, and the grants still land.
///
/// Replaces a scenario of the predecessor's own platform, where a post into
/// a group not yet granted was refused and its surface healed the grant
/// before posting again; that is a property of one platform's transport,
/// not of the platform-neutral gateway.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn posts_right_after_a_bind_do_not_wait_for_a_slow_grant() {
    let world = granting().start().await;
    world.fake().delay_adopts(Duration::from_millis(300));

    world.bind().await;
    world
        .wait_post("the no-Agent-group hint in the people group", |p| {
            p.is_root_in(GROUP)
        })
        .await;

    world
        .say(
            Message::mention(AGENTS, "u_pm", "m_adopt", &format!("/agents {COMMITTEE}"))
                .titled("[Agents] refund-v2"),
        )
        .await;
    eventually("the Agent group's cards to be pinned", || {
        let pins = world.fake().pins();
        (pins.len() >= 2).then_some(())
    })
    .await;
    eventually("both groups granted", || {
        let granted = world.fake().granted();
        (granted.contains(&GROUP.to_owned()) && granted.contains(&AGENTS.to_owned())).then_some(())
    })
    .await;
    world.stop().await;
}

/// A restarted gateway rebuilds its table from the log and releases a
/// group the log no longer binds, without waiting for a message from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_releases_what_the_log_unbound() {
    // The log must outlive the daemon.
    let mut world = granting().sqlite().start().await;
    world.bind().await;
    world
        .say(Message::mention(GROUP, "u_pm", "m_unbind", "/unbind"))
        .await;
    eventually("the group released", || {
        world
            .fake()
            .release_calls()
            .contains(&GROUP.to_owned())
            .then_some(())
    })
    .await;

    world.restart().await;
    eventually("the group released again after the restart", || {
        world
            .fake()
            .release_calls()
            .contains(&GROUP.to_owned())
            .then_some(())
    })
    .await;
    assert_eq!(world.gateway.as_ref().unwrap().committee_of(GROUP), None);
    world.stop().await;
}
