//! What a committee decided reaches every agent: a decision said in chat is
//! in the next session's brief, agents propose and people decide, and what
//! one group said is found from any machine.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_bridge::client::ApiClient;
use senate_core::{Kind, NewEvent, meta};
use senate_speaker::Intent;
use senated::testing::Message;

/// A decision said in one of two bound groups reaches Alice's next session
/// start through the brief, with its seq, and the Speaker's top three.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_decision_in_chat_is_in_the_next_sessions_brief() {
    let world = World::start(60).await;
    world.bind().await;
    world.bind_product().await;
    let mut alice = world.laptop("alice");
    alice.open().await;

    world.brain.interprets(Ok(Intent::Decide {
        body: "退款上限 30 天".into(),
        supersedes: None,
    }));
    world.brain.writes_top(Ok("1. 退款上限是 30 天".into()));
    world
        .say(
            Message::mention(PRODUCT_GROUP, "u_pm", "m_pm_d", "记一下：退款上限 30 天")
                .titled(PRODUCT_NAME),
        )
        .await;
    let restated = world
        .wait_post("the Speaker restates the decision", |p| {
            p.replies_to("m_pm_d") && p.markdown.contains("我理解为")
        })
        .await;
    world
        .reply_to(PRODUCT_GROUP, "u_pm", "m_pm_ok", "对", &restated, "m_pm_d")
        .await;
    world
        .wait_post("the Speaker acknowledges the decision", |p| {
            p.replies_to("m_pm_d") && p.markdown.contains("已记录决策")
        })
        .await;

    let events = world.events().await;
    let decision = events
        .iter()
        .find(|e| e.kind == Kind::Decision)
        .expect("the decision is in the log");
    assert_eq!(decision.on_behalf_of, Some(person("pm")));
    assert_eq!(
        decision.source.as_ref().map(|s| s.group_id.as_str()),
        Some(PRODUCT_GROUP)
    );

    // The top three follow shortly after the decision.
    eventually_async("the Speaker's top three", || async {
        world
            .events()
            .await
            .iter()
            .any(|e| e.meta.get(meta::BRIEF_TOP).is_some())
            .then_some(())
    })
    .await;

    let inbox = alice.inbox("s-alice");
    let out = alice
        .session_start("s-alice", &inbox)
        .await
        .expect("the hook injects context");
    let context = context(&out);
    assert!(context.contains("退款上限 30 天"), "{context}");
    assert!(
        context.contains(&format!("seq {}", decision.seq)),
        "{context}"
    );
    assert!(context.contains("退款上限是 30 天"), "the top: {context}");
    assert!(
        !world
            .posts()
            .iter()
            .any(|p| p.markdown.contains("退款上限是 30 天")),
        "the top is never mirrored to chat"
    );

    alice.close().await;
    world.stop().await;
}

/// An agent proposes a decision; a person confirms it in the chat thread;
/// every agent's brief has it. An agent cannot decide directly.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_propose_people_decide() {
    let world = World::start(60).await;
    world.bind().await;
    let mut alice = world.laptop("alice");
    let mut bob = world.laptop("bob");
    alice.open().await;
    bob.open().await;
    let alice_inbox = alice.inbox("s-alice");
    let bob_inbox = bob.inbox("s-bob");
    alice.session_start("s-alice", &alice_inbox).await;
    bob.session_start("s-bob", &bob_inbox).await;

    // Straight to the server, an agent's decision is refused.
    let client = ApiClient::new(&world.server_url(), "tok-alice").unwrap();
    let direct = NewEvent::new(committee(), Kind::Decision, senator("claude"), "我说了算");
    assert!(client.append(&direct).await.is_err());

    let body = "幂等键统一用 Idempotency-Key";
    let proposed = alice.propose_decision("s-alice", body).await;
    let root = world
        .wait_post("the proposal as a root in the mirror group", |p| {
            p.is_root_in(GROUP) && p.markdown.contains("提议记为决策")
        })
        .await;
    assert!(
        root.markdown.contains(&format!(
            "[senator::claude] 提议记为决策：{body}（seq {}）",
            proposed.seq
        )),
        "{}",
        root.markdown
    );
    assert!(
        bob.brief("s-bob").await.decisions.is_empty(),
        "not yet a decision"
    );

    world
        .reply_to(
            GROUP,
            "u_carol",
            "m_carol_ok",
            "对",
            &root,
            &root.message_ref,
        )
        .await;
    world
        .wait_post("the Speaker acknowledges the decision", |p| {
            p.markdown.contains("已记录决策")
        })
        .await;

    let events = world.events().await;
    let decision = events
        .iter()
        .find(|e| e.kind == Kind::Decision)
        .expect("the confirmed decision is in the log");
    assert_eq!(decision.thread, Some(proposed.seq));
    assert_eq!(decision.on_behalf_of, Some(person("carol")));
    assert_eq!(
        decision
            .meta
            .get(meta::PROPOSED_BY)
            .and_then(|v| v.as_str()),
        Some("senator::claude")
    );
    for (laptop, session) in [(&alice, "s-alice"), (&bob, "s-bob")] {
        let brief = laptop.brief(session).await;
        assert_eq!(
            brief.decisions.iter().map(|d| d.seq).collect::<Vec<_>>(),
            [decision.seq]
        );
        assert_eq!(brief.decisions[0].body, body);
    }

    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// What was said in the backend group is found by Bob's agent, though
/// nothing of it was ever pushed to him.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_finds_what_another_group_said() {
    let world = World::start(60).await;
    world.bind().await;
    world.bind_product().await;
    let mut bob = world.laptop("bob");
    bob.open().await;
    let inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &inbox).await;

    let said = "网关重试改成指数退避，最多 5 次";
    world
        .say(Message::ambient(GROUP, "u_carol", "m_carol_1", said).titled(GROUP_NAME))
        .await;
    world
        .say(Message::ambient(PRODUCT_GROUP, "u_pm", "m_pm_2", "下周三评审").titled(PRODUCT_NAME))
        .await;
    let chats = world
        .events()
        .await
        .iter()
        .filter(|e| e.kind == Kind::Chat)
        .count();
    assert!(chats >= 2, "both chats recorded");

    let found = bob.search("s-bob", "指数退避 重试").await;
    assert_eq!(found.events.len(), 1, "{:?}", found.events);
    let hit = &found.events[0];
    assert_eq!(hit.body, said);
    assert_eq!(hit.from, person("carol"));
    assert_eq!(
        hit.source.as_ref().map(|s| s.group_id.as_str()),
        Some(GROUP)
    );
    assert!(
        !inbox.messages().iter().any(|m| m.contains("指数退避")),
        "undirected chat is found, not pushed"
    );

    bob.close().await;
    world.stop().await;
}
