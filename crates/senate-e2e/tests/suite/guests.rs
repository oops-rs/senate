//! Guests: someone in chat who is not on the roster arrives as
//! `guest::chat/<id>`. A guest may ask and be answered; nothing a guest
//! says changes state.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use common::*;
use senate_core::{Address, Guest, Hub, Kind};
use senate_speaker::{Intent, RelayKind};
use senated::testing::Message;

fn stranger() -> Address {
    Address::Guest(Guest::new("chat", "u_stranger").unwrap())
}

/// The guest's question reaches an agent through the Speaker, and the
/// agent's answer comes back into the guest's thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guest_asks_and_is_answered() {
    let world = World::start(60).await;
    world.bind().await;
    let mut bob = world.laptop("bob");
    bob.open().await;
    let inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &inbox).await;
    world.wait_connected(BOB_CLAUDE, true).await;

    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["bob".into()],
        kind: RelayKind::Question,
        body: "退款多久到账？".into(),
    }));
    world
        .say(
            Message::mention(
                GROUP,
                "u_stranger",
                "m_guest_q",
                "问一下 bob 的 agent，退款多久到账？",
            )
            .titled(GROUP_NAME),
        )
        .await;
    let chat = world
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Chat)
        .unwrap();
    assert_eq!(chat.from, stranger(), "not on the roster: a guest");

    let push = inbox.wait_for("退款多久到账").await;
    assert!(push.contains("[guest::chat/u_stranger]"), "{push}");
    let thread = thread_of(&push, "退款多久到账");
    let relay = world
        .events()
        .await
        .into_iter()
        .find(|e| e.seq == thread)
        .unwrap();
    assert_eq!(relay.on_behalf_of, Some(stranger()));
    let posted = bob.answer("s-bob", thread, "一般 3 个工作日。").await;
    assert_eq!(posted.to, [stranger()], "the answer goes to the guest");
    world
        .wait_post("the answer in the guest's thread", |p| {
            p.replies_to("m_guest_q") && p.markdown.contains("一般 3 个工作日")
        })
        .await;

    // Asking the Speaker itself works too.
    world.brain.interprets(Ok(Intent::Recall));
    world
        .say(Message::mention(
            GROUP,
            "u_stranger",
            "m_guest_recall",
            "定了什么？",
        ))
        .await;
    world
        .wait_post("the Speaker answers the guest", |p| {
            p.replies_to("m_guest_recall")
        })
        .await;
    bob.close().await;
    world.stop().await;
}

/// A guest cannot bind a group, record a decision, or order work, in fixed
/// syntax or in words; nothing reaches the log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_guest_cannot_bind_decide_or_order_work() {
    let world = World::start(60).await;

    // Bind, in a group bound to nothing.
    world
        .say(Message::mention(
            "random",
            "u_stranger",
            "m_guest_bind",
            &format!("/bind {COMMITTEE}"),
        ))
        .await;
    world
        .wait_post("the refused bind", |p| {
            p.replies_to("m_guest_bind") && p.markdown.contains("只有名册上的人可以让我改动东西")
        })
        .await;
    assert!(world.hub().committees().await.unwrap().is_empty());

    world.bind().await;
    // Decide, in words.
    world.brain.interprets(Ok(Intent::Decide {
        body: "退款上限 90 天".into(),
        supersedes: None,
    }));
    world
        .say(Message::mention(
            GROUP,
            "u_stranger",
            "m_guest_d",
            "记一下：退款上限 90 天",
        ))
        .await;
    let refused = world
        .wait_post("the refused decision", |p| p.replies_to("m_guest_d"))
        .await;
    assert!(
        refused.markdown.contains("只有名册上的人可以让我改动东西"),
        "{}",
        refused.markdown
    );
    assert!(!refused.markdown.contains("回复「对」确认"));
    // Decide and order work, in fixed syntax.
    for (message_ref, text) in [
        ("m_guest_d2", "/decide 退款上限 90 天"),
        ("m_guest_w", "/work alice 把重试路径做完"),
        ("m_guest_r", "/reassign alice bob"),
    ] {
        world
            .say(Message::mention(GROUP, "u_stranger", message_ref, text))
            .await;
        world
            .wait_post(&format!("the refusal of {text}"), |p| {
                p.replies_to(message_ref) && p.markdown.contains("只有名册上的人可以让我改动东西")
            })
            .await;
    }
    world.settle().await;
    let events = world.events().await;
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.kind, Kind::Decision | Kind::Request)),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .filter(|e| e.kind == Kind::Binding)
            .all(|e| e.binding().is_some_and(|b| b.group_id == GROUP)),
        "only the PM's binding"
    );
    // And a guest is never seated: the log the server keeps refuses state
    // changes from one even through the Hub.
    let forged = senate_core::NewEvent::new(committee(), Kind::Decision, stranger(), "x");
    assert!(world.hub().append(forged).await.is_err());
    world.stop().await;
}
