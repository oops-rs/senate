//! The core loop, end to end: a person in chat reaches an agent's terminal
//! and back; agents ask each other in the open and people can correct
//! them; a closed laptop catches up in order while the Speaker covered.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::time::{Duration, Instant};

use common::*;
use senate_core::{Address, Kind};
use senate_speaker::{Intent, RelayKind};
use senated::testing::Message;

/// From `@Speaker 告诉 alice 的 agent …` in chat to Alice's terminal and
/// back into the same chat thread, in well under a minute.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_to_terminal_and_back_within_a_minute() {
    let world = World::start(60).await;
    world.bind().await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &inbox).await;
    world.wait_connected("claude", true).await;

    // A statement, so the Speaker relays it as a note; the push still
    // invites Alice's agent to reply.
    let body = "退款上限是 30 天，不是 14 天";
    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Note,
        body: body.into(),
    }));
    let started = Instant::now();
    world
        .say(
            Message::mention(
                GROUP,
                "u_pm",
                "m_pm_1",
                &format!("告诉 alice 的 agent，{body}"),
            )
            .titled(GROUP_NAME),
        )
        .await;

    let push = inbox.wait_for(body).await;
    let thread = thread_of(&push, body);
    assert!(
        push.contains("来自 [speaker::pm]") && push.contains("[senator::pm] 通过 Speaker"),
        "the push names who is speaking through the Speaker: {push}"
    );
    let events = world.events().await;
    let chat = events
        .iter()
        .find(|e| e.kind == Kind::Chat && e.from == person("pm"))
        .expect("the PM's message is in the log");
    assert!(chat.is_directed_to(&Address::Speaker));
    let relay = events.iter().find(|e| e.seq == thread).unwrap();
    assert_eq!(relay.kind, Kind::Note);
    assert_eq!(relay.to, vec![senator("claude")]);
    assert_eq!(relay.on_behalf_of, Some(person("pm")));

    // The Speaker acknowledged in the PM's thread.
    world
        .wait_post("the relay ack", |p| {
            p.replies_to("m_pm_1") && p.markdown.contains("[senator::claude]")
        })
        .await;

    // Alice's agent answers with the answer tool; it lands in the same thread.
    let posted = alice
        .answer(
            "s-alice",
            thread,
            "收到，已改成 30 天，见 refund/policy.rs。",
        )
        .await;
    assert_eq!(posted.to, vec![person("pm")]);
    let reply = world
        .wait_post("alice's answer mirrored into the thread", |p| {
            p.replies_to("m_pm_1") && p.markdown.contains("已改成 30 天")
        })
        .await;
    assert!(
        reply.markdown.contains("[senator::claude]"),
        "{}",
        reply.markdown
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "round trip took {:?}",
        started.elapsed()
    );

    alice.close().await;
    world.stop().await;
}

/// Alice's agent asks Bob's; the exchange is one chat thread; a person's
/// reply in that thread, without a mention, reaches both agents.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_to_agent_is_visible_and_correctable() {
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
    world.wait_connected("claude", true).await;
    world.wait_connected("copilot", true).await;

    let question = "网关的幂等键用哪个 header？仓库里 X-Idem-Key 和 Idempotency-Key 都有。";
    let asked = alice.ask("s-alice", senator(BOB_CLAUDE), question).await;
    let root = world
        .wait_post("the question as a new root in the group", |p| {
            p.is_root_in(GROUP) && p.markdown.contains("网关的幂等键")
        })
        .await;
    // A task root is a card; who asks whom is on the card.
    let card = world
        .fake()
        .card(&root.message_ref)
        .expect("the root is a task card");
    let shown = format!("{}\n{}", card.title, card.markdown);
    assert!(
        shown.contains("[senator::claude]") && shown.contains("[senator::copilot]"),
        "{shown}"
    );

    let push = bob_inbox.wait_for("网关的幂等键").await;
    assert_eq!(thread_of(&push, "网关的幂等键"), asked.seq);
    bob.answer(
        "s-bob",
        asked.seq,
        "用 X-Idem-Key，见 gateway/src/idem.rs。",
    )
    .await;
    let answer = world
        .wait_post("bob's answer in the question's thread", |p| {
            p.replies_to(&root.message_ref) && p.markdown.contains("X-Idem-Key，见")
        })
        .await;
    assert!(
        answer.markdown.contains("[senator::copilot]"),
        "{}",
        answer.markdown
    );
    alice_inbox.wait_for("X-Idem-Key，见").await;

    // Carol corrects it in the chat thread, replying to Bob's answer without
    // mentioning anyone.
    let thread_ref = answer
        .thread_ref
        .clone()
        .unwrap_or_else(|| world.thread_of(&root.message_ref));
    let correction = "不对，统一用 Idempotency-Key，X-Idem-Key 是旧的。";
    world
        .say(
            Message::reply(
                GROUP,
                "u_carol",
                "m_carol_1",
                correction,
                &answer.message_ref,
            )
            .in_thread(&thread_ref, &root.message_ref),
        )
        .await;
    let to_alice = alice_inbox.wait_for(correction).await;
    let to_bob = bob_inbox.wait_for(correction).await;
    assert!(
        to_alice.contains("[senator::carol]") && to_bob.contains("[senator::carol]"),
        "{to_alice}\n{to_bob}"
    );

    let events = world.events().await;
    let chat = events
        .iter()
        .find(|e| e.kind == Kind::Chat && e.body.contains("Idempotency-Key，X-Idem-Key"))
        .expect("the correction is in the log");
    assert_eq!(
        chat.thread,
        Some(asked.seq),
        "same log thread as the question"
    );
    assert!(chat.is_directed_to(&senator("claude")) && chat.is_directed_to(&senator(BOB_CLAUDE)));

    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// Alice's laptop is closed while traffic happens; when it opens, her agent
/// gets what it missed, in order. Meanwhile the Speaker answered for her.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_laptop_catches_up_in_order() {
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
    world.wait_connected("claude", true).await;

    alice.close().await;
    world.wait_connected("claude", false).await;
    let before = alice_inbox.messages().len();

    // Bob's agent asks; Alice is away, so the Speaker answers for her.
    world.brain.answers(Ok(
        "根据日志 seq 3，退款上限是 30 天（Speaker 代答）。".into()
    ));
    let q1 = bob
        .ask("s-bob", senator("claude"), "q1: 退款上限的常量叫什么？")
        .await;
    let on_behalf = world
        .wait_post("the Speaker answering for alice", |p| {
            p.markdown.contains("Speaker 代答")
        })
        .await;
    assert!(
        on_behalf.markdown.contains("[speaker::claude]"),
        "{}",
        on_behalf.markdown
    );
    bob_inbox.wait_for("Speaker 代答").await;

    // The PM leaves her a note through the Speaker; Carol chats undirected.
    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["爱丽丝".into()],
        kind: RelayKind::Note,
        body: "n2: 测试环境明天上午重置。".into(),
    }));
    world
        .say(Message::mention(
            GROUP,
            "u_pm",
            "m_pm_2",
            "告诉爱丽丝的 agent，测试环境明天上午重置",
        ))
        .await;
    world
        .say(Message::ambient(
            GROUP,
            "u_carol",
            "m_carol_2",
            "c3: 午饭吃什么",
        ))
        .await;
    // The relay is asynchronous (the Speaker interprets, then triages); Bob
    // asks again only once it is in the log, so the order is the test's.
    let (n2, c3) = eventually_async("the note and the chatter in the log", || async {
        let events = world.events().await;
        let n2 = events
            .iter()
            .find(|e| e.body.contains("n2:") && e.kind == Kind::Note)?;
        let c3 = events.iter().find(|e| e.body.contains("c3:"))?;
        Some((n2.seq, c3.seq))
    })
    .await;
    world
        .brain
        .answers(Ok("日志里没有答案，等 alice 回来确认。".into()));
    let q4 = bob
        .ask("s-bob", senator("claude"), "q4: 回调签名用哪个密钥？")
        .await;
    assert!(q1.seq < n2 && n2 < q4.seq);
    assert_eq!(
        alice_inbox.messages().len(),
        before,
        "nothing reaches a closed laptop"
    );

    // Open the lid. The session never ended; the daemon resumes from its
    // delivered cursor and pushes the directed backlog as one message.
    alice.open().await;
    let push = alice_inbox.wait_for("q4:").await;
    let (p1, p2, p4) = (
        pos(&push, q1.seq).expect("q1 in the backlog"),
        pos(&push, n2).expect("n2 in the backlog"),
        pos(&push, q4.seq).expect("q4 in the backlog"),
    );
    assert!(p1 < p2 && p2 < p4, "seq order: {push}");
    assert!(
        pos(&push, c3).is_none(),
        "undirected chatter waits for catch_up"
    );
    assert_eq!(
        alice_inbox.messages().len(),
        before + 1,
        "one batched message for the backlog"
    );

    // catch_up shows her the rest, including what the Speaker said for her.
    let caught = alice.catch_up("s-alice").await;
    let seqs: Vec<_> = caught.items.iter().map(|i| i.event.seq).collect();
    assert!(seqs.contains(&c3), "chatter is there: {seqs:?}");
    assert!(
        caught
            .items
            .iter()
            .any(|i| i.event.on_behalf_of == Some(senator("claude"))),
        "and the Speaker's answers on her behalf"
    );

    alice.close().await;
    bob.close().await;
    world.stop().await;
}
