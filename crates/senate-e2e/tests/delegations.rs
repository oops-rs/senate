//! A person and their agents: a delegation. "This person's agent" is a
//! lookup with three outcomes, and an agent is never stood in for by a
//! sibling.
//!
//! Alice runs two agents on one machine, a Claude Code session
//! (`alice/claude`) and a Codex session (`alice/codex`); Bob runs one; Carol
//! runs none.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::fake_codex::{FakeCodex, input_text};
use common::*;
use senate_bridge::ipc::ClientError;
use senate_core::{Address, DelegationPresence, Hub, Kind, PresenceState};
use senate_speaker::{Intent, RelayKind};
use senated::testing::Message;

/// Alice's machine with both her sessions running: the laptop, the Claude
/// session's inbox and the fake Codex app-server.
async fn alice_with_two_agents(world: &World) -> (Laptop, FakeInbox, FakeCodex) {
    let mut alice = world.laptop("alice");
    alice.git_init("feature/refund-v2/alice");
    let codex = alice.codex_app_server();
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    let start = alice
        .session_start("s-alice", &inbox)
        .await
        .expect("orientation");
    assert!(
        context(&start).contains("[senator::alice/claude]"),
        "{}",
        context(&start)
    );
    let start = alice
        .codex_start("thread-alice")
        .await
        .expect("orientation");
    assert!(
        context(&start).contains("[senator::alice/codex]"),
        "{}",
        context(&start)
    );
    codex.idle("thread-alice");
    world.wait_connected("alice/claude", true).await;
    world.wait_connected("alice/codex", true).await;
    (alice, inbox, codex)
}

/// What the Codex session was pushed, in order.
fn codex_pushes(codex: &FakeCodex) -> Vec<String> {
    codex
        .pushes()
        .iter()
        .map(|(_, params)| input_text(params).to_owned())
        .collect()
}

async fn bob_at_work(world: &World) -> (Laptop, FakeInbox) {
    let mut bob = world.laptop("bob");
    bob.open().await;
    let inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &inbox).await;
    world.wait_connected("bob/claude", true).await;
    (bob, inbox)
}

/// Each of Alice's agents gets only what is addressed to it; a message for
/// "Alice's agent" from an agent is refused with the candidates, and goes
/// through once it names one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_agents_of_one_person_each_get_their_own() {
    let world = World::start(60).await;
    world.bind().await;
    let (mut alice, claude_inbox, app_server) = alice_with_two_agents(&world).await;
    let (mut bob, _bob_inbox) = bob_at_work(&world).await;

    let for_claude = bob
        .ask("s-bob", claude("alice"), "给 claude：幂等键用哪个 header？")
        .await;
    let for_codex = bob
        .ask("s-bob", codex("alice"), "给 codex：重试上限是几次？")
        .await;
    assert_eq!(for_claude.to, [claude("alice")]);
    assert_eq!(for_codex.to, [codex("alice")]);
    claude_inbox.wait_for("给 claude").await;
    eventually("the Codex session's push", || {
        codex_pushes(&app_server)
            .into_iter()
            .find(|t| t.contains("给 codex"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        claude_inbox
            .messages()
            .iter()
            .all(|m| !m.contains("给 codex")),
        "the Claude session never sees the Codex agent's message"
    );
    assert!(
        codex_pushes(&app_server)
            .iter()
            .all(|t| !t.contains("给 claude")),
        "and the other way round"
    );

    // Unnamed, from an agent: refused, with both of Alice's agents listed,
    // and nothing is stored.
    let head = world.events().await.len();
    let refused = bob
        .try_ask("s-bob", agent_of("alice"), "谁来看一下回调？")
        .await
        .expect_err("an agent must name one");
    let ClientError::Remote(refused) = refused else {
        panic!("{refused}")
    };
    assert_eq!(refused.code, "ambiguous_recipient", "{refused:?}");
    assert_eq!(
        refused.candidates,
        [path("alice/claude"), path("alice/codex")]
    );
    assert!(
        refused.message.contains("[senator::alice/claude]")
            && refused.message.contains("[senator::alice/codex]"),
        "{}",
        refused.message
    );
    assert_eq!(world.events().await.len(), head, "nothing appended");

    // Named, it goes through.
    let named = bob.ask("s-bob", codex("alice"), "谁来看一下回调？").await;
    assert_eq!(named.to, [codex("alice")]);
    eventually("the named one at the Codex session", || {
        codex_pushes(&app_server)
            .into_iter()
            .find(|t| t.contains("谁来看一下回调"))
    })
    .await;
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// The same request from a person in chat, who cannot know Alice's agents:
/// the Speaker asks which one in the thread, and proceeds once the person
/// answers there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_in_chat_is_asked_which_agent_and_the_relay_proceeds() {
    let world = World::start(60).await;
    world.bind().await;
    let (mut alice, claude_inbox, app_server) = alice_with_two_agents(&world).await;

    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: "回调签名用哪个密钥？".into(),
    }));
    world
        .say(
            Message::mention(
                GROUP,
                "u_pm",
                "m_pm_q",
                "问一下 alice 的 agent，回调签名用哪个密钥？",
            )
            .titled(GROUP_NAME),
        )
        .await;
    let which = world
        .wait_post("the Speaker asks which agent", |p| {
            p.replies_to("m_pm_q") && p.markdown.contains("你指的是哪一个")
        })
        .await;
    assert!(
        which.markdown.contains("1. [senator::alice/claude]")
            && which.markdown.contains("2. [senator::alice/codex]"),
        "{}",
        which.markdown
    );
    let picked = world.brain.pick_inputs();
    assert_eq!(picked.len(), 1, "the brain was asked to pick first");
    assert_eq!(
        picked[0]
            .candidates
            .iter()
            .map(|c| c.agent.clone())
            .collect::<Vec<_>>(),
        [path("alice/claude"), path("alice/codex")]
    );
    assert!(
        !world
            .events()
            .await
            .iter()
            .any(|e| e.kind == Kind::Question),
        "nothing relayed before the choice"
    );

    world
        .reply_to(GROUP, "u_pm", "m_pm_pick", "codex", &which, "m_pm_q")
        .await;
    let relayed = eventually_async("the relay to the chosen agent", || async {
        world
            .events()
            .await
            .into_iter()
            .find(|e| e.kind == Kind::Question && e.body.contains("回调签名"))
    })
    .await;
    assert_eq!(relayed.to, [codex("alice")]);
    assert_eq!(relayed.on_behalf_of, Some(person("pm")));
    eventually("the question at the Codex session", || {
        codex_pushes(&app_server)
            .into_iter()
            .find(|t| t.contains("回调签名"))
    })
    .await;
    assert!(
        claude_inbox
            .messages()
            .iter()
            .all(|m| !m.contains("回调签名")),
        "not the agent that was not chosen"
    );
    alice.close().await;
    world.stop().await;
}

/// The unnamed form for a person with one agent reaches that agent; for a
/// person with none, the person.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_agent_is_it_and_none_is_the_person() {
    let world = World::start(60).await;
    world.bind().await;
    let (mut alice, alice_inbox) = {
        let mut alice = world.laptop("alice");
        alice.open().await;
        let inbox = alice.inbox("s-alice");
        alice.session_start("s-alice", &inbox).await;
        world.wait_connected("alice/claude", true).await;
        (alice, inbox)
    };
    let (mut bob, bob_inbox) = bob_at_work(&world).await;

    // Bob has one agent: it is the one.
    let to_bob = alice
        .ask("s-alice", agent_of("bob"), "回调要不要重试？")
        .await;
    assert_eq!(to_bob.to, [claude("bob")]);
    bob_inbox.wait_for("回调要不要重试").await;

    // Carol has none: it goes to Carol herself, who is mentioned in chat.
    let to_carol = alice
        .ask("s-alice", agent_of("carol"), "对账脚本谁负责？")
        .await;
    assert_eq!(to_carol.to, [person("carol")]);
    let stored = world
        .events()
        .await
        .into_iter()
        .find(|e| e.seq == to_carol.seq)
        .unwrap();
    assert_eq!(stored.to, [person("carol")], "the log names the person");
    world
        .wait_post("the question shown to Carol in chat", |p| {
            p.markdown.contains("对账脚本谁负责")
        })
        .await;
    assert!(
        alice_inbox
            .messages()
            .iter()
            .chain(bob_inbox.messages().iter())
            .all(|m| !m.contains("对账脚本谁负责")),
        "no agent stands in for a person"
    );
    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// Alice's Claude agent is away while her Codex agent works. A question to
/// the Claude agent is answered by the Speaker from the Claude agent's own
/// checkpoint; nothing goes to the Codex agent; her delegation still reads
/// as present.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_absent_agent_is_covered_from_its_own_checkpoint_while_its_sibling_works() {
    let world = Setup::new()
        .server(|config| {
            config.presence_disconnected_secs = 1;
            config.lease_secs = 2;
        })
        .start()
        .await;
    world.bind().await;
    let (mut alice, _claude_inbox, app_server) = alice_with_two_agents(&world).await;
    let checkpoint = alice
        .checkpoint(
            "s-alice",
            Some("## 完成\n- 幂等键用 Idempotency-Key header"),
            None,
            None,
        )
        .await
        .posted
        .seq;
    // The Claude session ends; the Codex session stays.
    alice
        .hook(
            senate_bridge::hook::HookKind::SessionEnd,
            "s-alice",
            &_claude_inbox,
        )
        .await;
    eventually_async("alice/claude absent", || async {
        world
            .hub()
            .presence(&committee())
            .await
            .ok()?
            .agents
            .into_iter()
            .find(|p| p.agent == path("alice/claude") && p.state == PresenceState::Absent)
    })
    .await;
    assert!(
        world
            .hub()
            .is_agent_connected(&committee(), &path("alice/codex"))
            .await,
        "the sibling is still here"
    );
    assert_eq!(
        world
            .hub()
            .delegation_presence(&committee(), &name("alice"))
            .await
            .unwrap(),
        DelegationPresence::Present,
        "the delegation is present through its Codex agent"
    );

    let (mut bob, bob_inbox) = bob_at_work(&world).await;
    world.brain.answers(Ok(format!(
        "用 Idempotency-Key header（检查点 seq {checkpoint}）"
    )));
    let asked = bob
        .ask("s-bob", claude("alice"), "幂等键用哪个 header？")
        .await;
    let answer = eventually_async("the Speaker's answer for alice/claude", || async {
        world
            .events()
            .await
            .into_iter()
            .find(|e| e.kind == Kind::Answer && e.thread == Some(asked.seq))
    })
    .await;
    assert_eq!(answer.from, Address::Speaker);
    assert_eq!(answer.on_behalf_of, Some(claude("alice")));
    let questions = world.brain.questions();
    let takeover = questions[0].takeover.as_ref().expect("a takeover");
    assert_eq!(takeover.checkpoint.seq, checkpoint);
    assert_eq!(takeover.checkpoint.agent, path("alice/claude"));
    let push = bob_inbox.wait_for("Idempotency-Key header").await;
    assert!(push.contains("[speaker::alice/claude] 接管"), "{push}");

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        codex_pushes(&app_server)
            .iter()
            .all(|t| !t.contains("幂等键用哪个")),
        "nothing is rerouted to the sibling"
    );
    let events = world.events().await;
    let question = events.iter().find(|e| e.seq == asked.seq).unwrap();
    assert_eq!(
        question.to,
        [claude("alice")],
        "still addressed to the one asked"
    );
    alice.close().await;
    bob.close().await;
    world.stop().await;
}
