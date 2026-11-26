//! What waits on `chair` follows the chair: when the chair moves, the new
//! chair is handed every root addressed to `chair` that is still open, and
//! the old chair's session is no longer pushed it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::path::PathBuf;
use std::time::Duration;

use common::*;
use senate_bridge::hook::HookKind;
use senate_bridge::state::ItemStatus;
use senate_core::{Address, Event, Kind, NewEvent};

/// A worktree on `refund-v2` whose sessions speak as `agent`.
fn worktree(h: &Harness, agent: &str) -> PathBuf {
    let wd = h.workdir(agent, Some("refund-v2"));
    std::fs::write(wd.join(".senate/agent"), format!("{agent}\n")).unwrap();
    wd
}

/// `agent` chairs `refund-v2` from now on: the server says so, and the log.
fn appoint(h: &Harness, agent: &str) -> Event {
    h.server.state.inner.lock().unwrap().chair = Some((path(agent), true));
    h.server.append(
        NewEvent::new(committee(), Kind::Note, Address::Committee, "chair set").with_meta(
            senate_core::meta::CHAIR,
            senate_core::chair::ChairPayload {
                senator: path(agent),
            }
            .value(),
        ),
    )
}

fn to_chair(body: &str) -> NewEvent {
    question(person("pm"), Address::Chair, body)
}

/// The item `agent` has for `seq`, once it is in `status`.
async fn item_in(h: &Harness, agent: &str, seq: u64, status: ItemStatus) {
    eventually(&format!("{agent}'s item {seq} {status:?}"), WAIT, || {
        (h.item_of(agent, seq)?.status == status).then_some(())
    })
    .await;
}

/// The bug: alpha chairs and is offline (its session here ended); a
/// message to `chair` comes; the chair moves to beta, online. beta's
/// stream passed the message while alpha chaired, so it was never beta's.
/// Now it is handed over: beta is pushed it. When the chair goes back to
/// alpha, online again, what is still open is alpha's again, and beta is
/// not pushed it twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_to_the_chair_follows_the_chair_to_the_new_one() {
    let mut h = Harness::new().await;
    appoint(&h, "alpha");
    h.start().await;
    let wd_alpha = worktree(&h, "alpha");
    let inbox_alpha = h.inbox("a");
    h.hook(HookKind::SessionStart, "a", &wd_alpha, Some(&inbox_alpha))
        .await;
    h.wait_stream_of("refund-v2", "alpha").await;
    let wd_beta = worktree(&h, "beta");
    let inbox_beta = h.inbox("b");
    h.hook(HookKind::SessionStart, "b", &wd_beta, Some(&inbox_beta))
        .await;
    h.wait_stream_of("refund-v2", "beta").await;
    h.hook(HookKind::SessionEnd, "a", &wd_alpha, Some(&inbox_alpha))
        .await;

    let message = h.server.append(to_chair("Who reviews the refund cap?"));
    h.wait_delivered_cursor_of("refund-v2", "beta", message.seq.0)
        .await;
    assert!(h.item_of("beta", message.seq.0).is_none());

    appoint(&h, "beta");
    item_in(&h, "beta", message.seq.0, ItemStatus::Delivered).await;
    let pushed = inbox_beta.wait_messages(1).await;
    assert!(
        pushed[0].contains("Who reviews the refund cap?"),
        "{pushed:?}"
    );
    assert!(pushed[0].contains("seq 2"), "{pushed:?}");
    assert!(inbox_alpha.messages().is_empty());

    let inbox_alpha2 = h.inbox("a2");
    h.hook(HookKind::SessionStart, "a2", &wd_alpha, Some(&inbox_alpha2))
        .await;
    h.wait_stream_of("refund-v2", "alpha").await;
    appoint(&h, "alpha");
    let back = inbox_alpha2.wait_messages(1).await;
    assert!(back[0].contains("Who reviews the refund cap?"), "{back:?}");
    item_in(&h, "alpha", message.seq.0, ItemStatus::Delivered).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(inbox_beta.messages().len(), 1);
    h.stop().await;
}

/// The old chair's session holds a message to `chair` it has not been
/// shown (it has no push channel; the item waits for its next hook). The
/// chair moves: the item is withdrawn from it, and its next hook does not
/// show it. The new chair has it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_old_chair_was_not_shown_is_withdrawn_from_it() {
    let mut h = Harness::new().await;
    appoint(&h, "alpha");
    h.start().await;
    let wd_alpha = worktree(&h, "alpha");
    h.hook(HookKind::SessionStart, "a", &wd_alpha, None).await;
    h.wait_stream_of("refund-v2", "alpha").await;
    let wd_beta = worktree(&h, "beta");
    let inbox_beta = h.inbox("b");
    h.hook(HookKind::SessionStart, "b", &wd_beta, Some(&inbox_beta))
        .await;
    h.wait_stream_of("refund-v2", "beta").await;

    let message = h.server.append(to_chair("Held for alpha's hook"));
    item_in(&h, "alpha", message.seq.0, ItemStatus::Queued).await;

    appoint(&h, "beta");
    item_in(&h, "alpha", message.seq.0, ItemStatus::Cancelled).await;
    let pushed = inbox_beta.wait_messages(1).await;
    assert!(pushed[0].contains("Held for alpha's hook"), "{pushed:?}");
    let next = h
        .hook(HookKind::UserPromptSubmit, "a", &wd_alpha, None)
        .await;
    assert!(
        next.as_ref()
            .is_none_or(|out| !out.to_string().contains("Held for alpha's hook")),
        "{next:?}"
    );
    h.stop().await;
}

/// What the old chair answered before the move is settled: the new chair
/// is not handed it. What it did not is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn what_the_old_chair_answered_is_not_handed_over() {
    let mut h = Harness::new().await;
    appoint(&h, "alpha");
    h.start().await;
    let wd_beta = worktree(&h, "beta");
    let inbox_beta = h.inbox("b");
    h.hook(HookKind::SessionStart, "b", &wd_beta, Some(&inbox_beta))
        .await;
    h.wait_stream_of("refund-v2", "beta").await;

    let answered = h.server.append(to_chair("Settled already?"));
    h.server.append(
        NewEvent::new(committee(), Kind::Answer, senator("alpha"), "Yes.")
            .to(person("pm"))
            .in_thread(answered.seq),
    );
    let open = h.server.append(to_chair("Still open?"));
    h.wait_delivered_cursor_of("refund-v2", "beta", open.seq.0)
        .await;

    appoint(&h, "beta");
    let pushed = inbox_beta.wait_messages(1).await;
    assert!(pushed[0].contains("Still open?"), "{pushed:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.item_of("beta", answered.seq.0).is_none());
    assert!(
        inbox_beta
            .messages()
            .iter()
            .all(|m| !m.contains("Settled already?"))
    );
    h.stop().await;
}
