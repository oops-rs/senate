//! Task threads: people find agents from chat, agents find each other, and
//! people can take over in a thread. Agent traffic goes to the Agent group
//! as cards; a request goes to the person first; an agent escalates; a
//! person who answers first takes the thread over.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use senate_core::{GroupRole, Kind, StatusPayload, StatusState, Step, TriageVerdict, meta};
use senated::testing::{Caps, Message, Post};

/// The task card posted as a root in the Agent group whose content mentions
/// `needle`. The pinned "waiting on people" and "today" cards are roots
/// there too, and the summary quotes open questions.
async fn task_card(world: &World, needle: &str) -> Post {
    world
        .wait_post(&format!("the task card for {needle}"), |p| {
            p.is_root_in(AGENTS)
                && world.fake().card(&p.message_ref).is_some_and(|c| {
                    !c.title.starts_with("等人处理")
                        && !c.title.starts_with("今天的摘要")
                        && format!("{}{}", c.title, c.markdown).contains(needle)
                })
        })
        .await
}

/// Agent traffic goes to the Agent group as a task card that moves in
/// place; the answer lands there and the card ends archived.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn task_threads_live_in_the_agent_group_as_cards() {
    let world = World::start(60).await;
    world.with_agent_group().await;
    let mut alice = world.laptop("alice");
    let mut bob = world.laptop("bob");
    alice.open().await;
    bob.open().await;
    let alice_inbox = alice.inbox("s-alice");
    let bob_inbox = bob.inbox("s-bob");
    alice.session_start("s-alice", &alice_inbox).await;
    bob.session_start("s-bob", &bob_inbox).await;

    world.brain.judges(TriageVerdict::Agent, "查代码就能答");
    let asked = alice
        .ask("s-alice", senator(BOB_CLAUDE), "网关幂等用哪个 header？")
        .await;
    let root = task_card(&world, "网关幂等").await;
    assert!(
        !world
            .posts()
            .iter()
            .any(|p| p.is_root_in(GROUP) && p.markdown.contains("幂等")),
        "the people group does not see agent traffic"
    );
    bob_inbox.wait_for("网关幂等").await;

    let mut working = StatusPayload::new(StatusState::Processing);
    working.title = Some("查幂等 header".into());
    working.steps = Some(vec![Step {
        id: "grep".into(),
        label: "查 gateway/src/idem.rs".into(),
        done: true,
    }]);
    bob.status("s-bob", asked.seq, working).await;
    eventually("the card to show the task", || {
        let card = world.fake().card(&root.message_ref)?;
        (card.title.contains("查幂等 header") && card.steps.len() == 1).then_some(())
    })
    .await;

    bob.answer("s-bob", asked.seq, "Idempotency-Key").await;
    bob.status(
        "s-bob",
        asked.seq,
        StatusPayload::new(StatusState::Archived),
    )
    .await;
    eventually("the card to end archived", || {
        let card = world.fake().card(&root.message_ref)?;
        (card.state == StatusState::Archived).then_some(())
    })
    .await;
    let triaged = world
        .events()
        .await
        .into_iter()
        .any(|e| e.thread == Some(asked.seq) && e.meta.contains_key(meta::TRIAGE));
    assert!(triaged, "the triage verdict is in the log");
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// Where the platform lets the bot create groups, binding the people group
/// creates `[Agent 群] <committee>` with the binder in it, and agent traffic
/// goes there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_creates_the_agent_group() {
    let world = World::start(60).await;
    world.fake().set_caps(Caps::ALL);
    let mut alice = world.laptop("alice");
    alice.open().await;
    let alice_inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &alice_inbox).await;
    world.wait_connected("claude", true).await;
    world.bind().await;

    let created = eventually("the Agent group to be created", || {
        world.fake().groups().into_iter().next()
    })
    .await;
    assert_eq!(created.name, format!("[Agent 群] {COMMITTEE}"));
    assert!(
        created.members.contains(&user_id("pm")),
        "the binder is in it: {:?}",
        created.members
    );
    assert!(
        created.members.contains(&user_id("alice")),
        "so is a person whose agent is connected: {:?}",
        created.members
    );
    eventually_async("the Agent group to be bound", || async {
        world
            .events()
            .await
            .iter()
            .any(|e| {
                e.binding()
                    .is_some_and(|b| b.group_id == created.group_id && b.role == GroupRole::Agents)
            })
            .then_some(())
    })
    .await;
    assert!(
        !world
            .posts()
            .iter()
            .any(|p| p.markdown.contains("还没有 Agent 群")),
        "no adopt hint when the group could be created"
    );

    world.brain.judges(TriageVerdict::Agent, "查代码就能答");
    let mut bob = world.laptop("bob");
    bob.open().await;
    let bob_inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &bob_inbox).await;
    alice
        .ask("s-alice", senator(BOB_CLAUDE), "网关幂等用哪个 header？")
        .await;
    world
        .wait_post("the task card in the created group", |p| {
            p.is_root_in(&created.group_id)
                && world
                    .fake()
                    .card(&p.message_ref)
                    .is_some_and(|c| format!("{}{}", c.title, c.markdown).contains("网关幂等"))
        })
        .await;
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// Another agent's request goes to the person first; once she hands it to
/// her own agent, it reaches her session as her task.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_goes_to_the_person_first() {
    let world = World::start(60).await;
    world.with_agent_group().await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &inbox).await;

    let mut bob = world.laptop("bob");
    bob.open().await;
    let bob_inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &bob_inbox).await;
    // Bob's agent asks Alice's to *do* something: `ask(request=true)`.
    let request = bob
        .request(
            "s-bob",
            senator("claude"),
            "把网关 header 改成 Idempotency-Key",
        )
        .await;
    let mention = world
        .wait_post("the Speaker mentions alice", |p| {
            p.mention.as_deref() == Some("u_alice")
        })
        .await;
    assert!(
        mention.markdown.contains("交给我的 agent"),
        "{}",
        mention.markdown
    );
    let root = task_card(&world, "Idempotency").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        inbox
            .messages()
            .iter()
            .all(|m| !m.contains("Idempotency-Key")),
        "not pushed"
    );

    // Alice hands it to her own agent in the thread.
    let thread = world.thread_of(&root.message_ref);
    world
        .say(
            Message::ambient(AGENTS, "u_alice", "m_alice_ok", "交给我的 agent")
                .in_thread(&thread, &root.message_ref),
        )
        .await;
    let push = inbox.wait_for("Idempotency-Key").await;
    assert!(
        push.contains("来自 [senator::alice]"),
        "her own task: {push}"
    );
    let events = world.events().await;
    assert!(events.iter().any(|e| e.kind == Kind::Request
        && e.from == person("alice")
        && e.to == [senator("claude")]
        && e.thread == Some(request.seq)));
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// The agent escalates: the person is mentioned and the asker told.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_agent_escalates() {
    let world = World::start(60).await;
    world.with_agent_group().await;
    let mut alice = world.laptop("alice");
    let mut bob = world.laptop("bob");
    alice.open().await;
    bob.open().await;
    let alice_inbox = alice.inbox("s-alice");
    let bob_inbox = bob.inbox("s-bob");
    alice.session_start("s-alice", &alice_inbox).await;
    bob.session_start("s-bob", &bob_inbox).await;

    world.brain.judges(TriageVerdict::Agent, "事实");
    let asked = bob
        .ask("s-bob", senator("claude"), "这个能延到下个迭代吗？")
        .await;
    alice_inbox.wait_for("下个迭代").await;
    alice
        .escalate("s-alice", asked.seq, "排期要 alice 定")
        .await;
    world
        .wait_post("alice is mentioned", |p| {
            p.mention.as_deref() == Some("u_alice") && p.markdown.contains("排期要 alice 定")
        })
        .await;
    bob_inbox.wait_for("已转给 [senator::alice] 本人").await;
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// A person replying in the thread before any answer takes it over: the
/// queued delivery is cancelled and never pushed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_gets_there_first() {
    let world = World::start(60).await;
    world.with_agent_group().await;
    let mut alice = world.laptop("alice");
    let mut bob = world.laptop("bob");
    alice.open().await;
    bob.open().await;
    let alice_inbox = alice.inbox("s-alice");
    let bob_inbox = bob.inbox("s-bob");
    alice.session_start("s-alice", &alice_inbox).await;
    bob.session_start("s-bob", &bob_inbox).await;

    // The verdict is slow, so the question is still waiting at Bob's bridge.
    world.brain.triages_after(
        Duration::from_secs(3),
        Ok(senate_speaker::scripted_verdict(TriageVerdict::Agent, None)),
    );
    alice
        .ask("s-alice", senator(BOB_CLAUDE), "回调签名用哪个密钥？")
        .await;
    let root = task_card(&world, "回调签名").await;
    let thread = world.thread_of(&root.message_ref);
    world
        .say(
            Message::ambient(
                AGENTS,
                "u_pm",
                "m_pm_first",
                "我来答：用 KMS 里的 refund-callback",
            )
            .in_thread(&thread, &root.message_ref),
        )
        .await;
    eventually_async("the thread taken over", || async {
        world
            .events()
            .await
            .iter()
            .filter_map(StatusPayload::of)
            .any(|s| s.takeover == Some(name("pm")))
            .then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        bob_inbox.messages().iter().all(|m| !m.contains("回调签名")),
        "never pushed"
    );
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// People are invited into the Agent group by mentioning them: the gateway
/// records whom a message mentions as their addresses, and the Speaker
/// invites them by their accounts on the platform.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn people_are_invited_to_the_agent_group_by_mention() {
    let world = World::start(60).await;
    world.fake().set_caps(Caps::ALL);
    world.bind().await;
    let created = eventually("the Agent group to be created", || {
        world.fake().groups().into_iter().next()
    })
    .await;

    world
        .say(Message::mention(GROUP, "u_pm", "m_invite", "/invite @Bob").mentioning("u_bob", "Bob"))
        .await;
    let invited =
        eventually("bob invited", || {
            world.fake().invites().into_iter().find(|(group, users)| {
                *group == created.group_id && users.contains(&user_id("bob"))
            })
        })
        .await;
    assert_eq!(invited.1, [user_id("bob")]);
    let chat = world
        .events()
        .await
        .into_iter()
        .find(|e| e.kind == Kind::Chat && e.body.starts_with("/invite"))
        .unwrap();
    assert_eq!(
        chat.meta[meta::MENTIONS],
        serde_json::json!([{ "address": "senator::bob", "name": "Bob" }]),
        "platform ids stay out of the log"
    );
    world.stop().await;
}
