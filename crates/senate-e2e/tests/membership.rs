//! Bind = join, end to end: a group the platform has no configuration for
//! is bound with `/bind`, the gateway has the platform adopt it, and from
//! then on its unmentioned chat reaches the log; `/agents` adopts the Agent
//! group the same way, and unbinding the last people group releases both.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use senate_core::Kind;
use senated::Delivered;
use senated::MembershipPolicy;
use senated::testing::{Caps, MembershipKind, MembershipOutcome, Message};

/// A platform app that may adopt groups, configured for none: only
/// mentions arrive from a group until the bot has joined it.
fn joining() -> Setup {
    Setup::new().platform(|chat| {
        chat.set_caps(Caps {
            membership: true,
            ..Caps::PLAIN
        });
        chat.gate_ambient(&[]);
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_an_unconfigured_group_joins_it_and_unbinding_leaves_it() {
    let world = joining().start().await;

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
            .joined()
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
        "nothing from before the join"
    );

    // The Agent group, adopted with `/agents`, is joined too.
    world.with_agent_group().await;
    eventually("the Agent group to be adopted", || {
        world
            .fake()
            .joined()
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
        world.fake().joined().is_empty().then_some(())
    })
    .await;
    let released = world.fake().release_calls();
    assert!(released.contains(&GROUP.to_owned()), "{released:?}");
    assert!(released.contains(&AGENTS.to_owned()), "{released:?}");
    world.stop().await;
}

/// The platform answers adopts slowly: the Speaker's first root posts into
/// groups it just bound (the "no Agent group" hint, the Agent group's
/// pinned cards) do not wait for the join, and the joins still land.
///
/// Refusing a post into a group not yet joined, and healing it before
/// posting again, is a property of one platform's transport, not of the
/// platform-neutral gateway; this test does not depend on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn posts_right_after_a_bind_do_not_wait_for_a_slow_join() {
    let world = joining().start().await;
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
    eventually("both groups joined", || {
        let joined = world.fake().joined();
        (joined.contains(&GROUP.to_owned()) && joined.contains(&AGENTS.to_owned())).then_some(())
    })
    .await;
    world.stop().await;
}

/// A restarted gateway rebuilds its table from the log and releases a
/// group the log no longer binds, without waiting for a message from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_releases_what_the_log_unbound() {
    // The log must outlive the daemon.
    let mut world = joining().sqlite().start().await;
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

/// A short grace and short pauses, for tests that wait in real time.
fn quick(grace_ms: u64) -> MembershipPolicy {
    MembershipPolicy {
        release_grace: Duration::from_millis(grace_ms),
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(100),
        call_timeout: Duration::from_secs(5),
    }
}

/// A platform that fails twice before it takes an adopt, and twice before it
/// takes a release: both are retried until they land.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_platform_that_fails_joins_is_asked_until_it_takes_them() {
    let world = joining()
        .membership_policy(quick(200))
        .platform(|chat| {
            chat.set_caps(Caps {
                membership: true,
                ..Caps::PLAIN
            });
            chat.gate_ambient(&[]);
            chat.fail_adopts(2);
            chat.fail_releases(2);
        })
        .start()
        .await;

    world.bind().await;
    eventually("the group to be adopted after two failures", || {
        world
            .fake()
            .joined()
            .contains(&GROUP.to_owned())
            .then_some(())
    })
    .await;
    let outcomes: Vec<_> = world
        .fake()
        .membership_calls_of(MembershipKind::Adopt)
        .iter()
        .map(|c| c.outcome)
        .collect();
    assert_eq!(
        outcomes,
        [
            MembershipOutcome::Failed,
            MembershipOutcome::Failed,
            MembershipOutcome::Done
        ]
    );

    let unbound = tokio::time::Instant::now();
    world
        .say(Message::mention(GROUP, "u_pm", "m_unbind", "/unbind"))
        .await;
    eventually("the group to be released after two failures", || {
        world.fake().joined().is_empty().then_some(())
    })
    .await;
    let releases = world.fake().membership_calls_of(MembershipKind::Release);
    assert_eq!(
        releases.iter().map(|c| c.outcome).collect::<Vec<_>>(),
        [
            MembershipOutcome::Failed,
            MembershipOutcome::Failed,
            MembershipOutcome::Done
        ]
    );
    assert!(
        releases[0].at - unbound >= Duration::from_millis(200),
        "not before the grace"
    );
    world.stop().await;
}

/// A group moved from one committee to another (unbound, then bound again
/// within the grace) stays joined: no release, no second adopt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_group_moved_to_another_committee_stays_joined() {
    let world = joining().membership_policy(quick(3_000)).start().await;
    world.bind().await;
    eventually("the group to be adopted", || {
        world
            .fake()
            .joined()
            .contains(&GROUP.to_owned())
            .then_some(())
    })
    .await;

    world
        .say(Message::mention(GROUP, "u_pm", "m_unbind", "/unbind"))
        .await;
    eventually("the group to be unbound", || {
        (world.gateway.as_ref()?.committee_of(GROUP).is_none()).then_some(())
    })
    .await;
    world
        .say(Message::mention(
            GROUP,
            "u_pm",
            "m_rebind",
            "/bind refund-v3",
        ))
        .await;
    eventually("the group to be bound to the other committee", || {
        (world.gateway.as_ref()?.committee_of(GROUP)
            == Some(senate_core::CommitteeId::new("refund-v3").unwrap()))
        .then_some(())
    })
    .await;

    tokio::time::sleep(Duration::from_millis(3_500)).await;
    assert!(
        world
            .fake()
            .membership_calls_of(MembershipKind::Release)
            .is_empty(),
        "never released"
    );
    assert_eq!(world.fake().adopt_calls().len(), 1, "never adopted twice");
    assert!(world.fake().joined().contains(&GROUP.to_owned()));
    world.stop().await;
}

/// A restarted daemon adopts a bound group again, by a message of its own
/// log, without waiting for anyone to speak.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_adopts_a_bound_group_again_from_the_log() {
    let mut world = joining()
        .sqlite()
        .platform(|chat| {
            chat.set_caps(Caps {
                membership: true,
                ..Caps::PLAIN
            });
            chat.gate_ambient(&[]);
            // What the platform delivered before the restart it still knows.
            chat.know_message("m_after", GROUP);
        })
        .start()
        .await;
    world.bind().await;
    eventually("the group to be adopted", || {
        world
            .fake()
            .joined()
            .contains(&GROUP.to_owned())
            .then_some(())
    })
    .await;
    world
        .say(Message::ambient(GROUP, "u_pm", "m_after", "绑定之后"))
        .await;

    world.restart().await;
    eventually("the group to be adopted again", || {
        world
            .fake()
            .joined()
            .contains(&GROUP.to_owned())
            .then_some(())
    })
    .await;
    assert_eq!(world.fake().adopt_calls(), ["m_after"]);
    world.stop().await;
}

/// A release the platform kept refusing is still owed after a restart: the
/// log says the group is bound to nothing, so the new run releases it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_release_the_platform_refused_is_performed_after_a_restart() {
    let starts = Arc::new(AtomicUsize::new(0));
    let seen = starts.clone();
    let mut world = joining()
        .sqlite()
        .membership_policy(quick(100))
        .platform(move |chat| {
            chat.set_caps(Caps {
                membership: true,
                ..Caps::PLAIN
            });
            chat.gate_ambient(&[]);
            if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                chat.fail_releases(1_000_000);
            }
        })
        .start()
        .await;
    world.bind().await;
    world
        .say(Message::mention(GROUP, "u_pm", "m_unbind", "/unbind"))
        .await;
    eventually("the release to be tried and refused", || {
        (world
            .fake()
            .membership_calls_of(MembershipKind::Release)
            .len()
            >= 2)
            .then_some(())
    })
    .await;
    assert!(world.fake().release_calls().is_empty(), "never taken");

    world.restart().await;
    eventually("the owed release to be made after the restart", || {
        world
            .fake()
            .release_calls()
            .contains(&GROUP.to_owned())
            .then_some(())
    })
    .await;
    world.stop().await;
}

/// The Speaker's first post into a group it has just bound is refused by a
/// platform that has not let the bot join the group yet: the gateway adopts it then
/// and the post goes through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_post_refused_for_want_of_membership_rejoins_and_lands() {
    let world = joining()
        .platform(|chat| {
            chat.set_caps(Caps {
                membership: true,
                ..Caps::PLAIN
            });
            chat.gate_ambient(&[]);
            chat.delay_adopts(Duration::from_millis(300));
            chat.require_membership_to_post(true);
        })
        .start()
        .await;
    world.bind().await;
    world
        .wait_post("the no-Agent-group hint in the people group", |p| {
            p.is_root_in(GROUP)
        })
        .await;
    assert!(world.fake().joined().contains(&GROUP.to_owned()));
    world.stop().await;
}

/// The Speaker confirms a bind in the thread of the bind message, as a
/// reply, which names a message and not a group. The platform refuses it
/// until the group's (slow) join has landed; the gateway finds the group
/// from the bind message, adopts, and the confirmation lands, with no heal
/// of the platform layer's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_confirmation_in_the_bind_messages_thread_is_refused_once_and_then_lands() {
    let world = joining()
        .platform(|chat| {
            chat.set_caps(Caps {
                membership: true,
                ..Caps::PLAIN
            });
            chat.gate_ambient(&[]);
            chat.delay_adopts(Duration::from_secs(3));
            chat.require_membership_to_reply(true);
        })
        .start()
        .await;
    world.bind().await;
    let confirmation = world
        .wait_post(
            "the Speaker's confirmation in the bind message's thread",
            |p| p.replies_to("m_bind"),
        )
        .await;
    assert_eq!(confirmation.group.as_deref(), Some(GROUP));
    assert!(
        world.fake().joined().contains(&GROUP.to_owned()),
        "the confirmation landed because the group had been adopted"
    );
    assert_eq!(
        world.fake().rejected_posts(),
        1,
        "refused once, healed by the gateway, then accepted"
    );
    assert_eq!(
        world.fake().adopt_calls(),
        ["m_bind"],
        "one adopt, however many posts waited for it"
    );
    world.stop().await;
}
