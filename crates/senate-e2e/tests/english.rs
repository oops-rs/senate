//! The core loop in English: the deployment's `locale = "en"` reaches the
//! Speaker and the bridges, every text people and agents read is English,
//! and the labels are the same bracketed mentions as in Chinese.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use senate_core::{Kind, Locale};
use senate_speaker::{Intent, RelayKind};
use senated::testing::Message;

/// Any CJK character.
fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_to_terminal_and_back_in_english() {
    let world = Setup::new().locale(Locale::En).start().await;
    world.bind().await;
    let bound = world
        .wait_post("the bind acknowledged", |p| p.replies_to("m_bind"))
        .await;
    assert!(!has_cjk(&bound.markdown), "{}", bound.markdown);
    assert!(bound.markdown.contains("refund-v2"), "{}", bound.markdown);

    let mut alice = world.laptop("alice");
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    let start = alice.session_start("s-alice", &inbox).await.unwrap();
    assert!(!has_cjk(context(&start)), "{}", context(&start));
    world.wait_connected("claude", true).await;

    let body = "The refund window is 30 days, not 14.";
    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Note,
        body: body.into(),
    }));
    world
        .say(
            Message::mention(
                GROUP,
                "u_pm",
                "m_pm_1",
                &format!("tell alice's agent: {body}"),
            )
            .titled("Refund v2 backend"),
        )
        .await;

    let push = inbox.wait_for(body).await;
    assert!(!has_cjk(&push), "{push}");
    assert!(push.contains("[speaker::pm]"), "the same label: {push}");
    assert!(push.contains("[senator::pm]"), "{push}");
    let thread = thread_of(&push, body);
    let relay = world
        .events()
        .await
        .into_iter()
        .find(|e| e.seq == thread)
        .unwrap();
    assert_eq!(relay.kind, Kind::Note);
    assert_eq!(relay.to, [senator("claude")]);
    assert_eq!(relay.on_behalf_of, Some(person("pm")));

    let ack = world
        .wait_post("the relay ack", |p| {
            p.replies_to("m_pm_1") && p.markdown.contains("[senator::claude]")
        })
        .await;
    assert!(!has_cjk(&ack.markdown), "{}", ack.markdown);

    alice
        .answer("s-alice", thread, "Done: 30 days, see refund/policy.rs.")
        .await;
    let reply = world
        .wait_post("alice's answer mirrored into the thread", |p| {
            p.replies_to("m_pm_1") && p.markdown.contains("Done: 30 days")
        })
        .await;
    assert!(
        reply.markdown.contains("[senator::claude]"),
        "{}",
        reply.markdown
    );
    assert!(!has_cjk(&reply.markdown), "{}", reply.markdown);
    // Everything the Speaker posted is English.
    for post in world.posts() {
        assert!(!has_cjk(&post.markdown), "{}", post.markdown);
    }
    alice.close().await;
    world.stop().await;
}
