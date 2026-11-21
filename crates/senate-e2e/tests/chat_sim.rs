//! Simulated chat (`senate chat send` / `senate chat thread`): a message
//! injected at the gateway as a person's, interpreted and answered by the
//! Speaker exactly as a platform message is, with everything it posts kept
//! off the platform unless `--live`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::time::Duration;

use common::*;
use senate_bridge::ipc::{Request, SessionSelector};
use senate_bridge::speak;
use senate_core::chat_sim::{SimTranscript, SimulateChat, SimulatedChat};
use senate_core::{Address, Hub, Kind};
use senate_speaker::{Intent, RelayKind};

fn send(text: &str) -> SimulateChat {
    SimulateChat {
        text: text.into(),
        ..SimulateChat::default()
    }
}

fn chat_send(session: SessionSelector, request: SimulateChat) -> Request {
    Request::ChatSend {
        session,
        committee: Some(committee()),
        request,
    }
}

/// `senate chat thread --wait`: the conversation once the Speaker has
/// posted after the first `since` entries.
async fn wait_thread(laptop: &Laptop, message_ref: &str, since: Option<usize>) -> SimTranscript {
    let (transcript, arrived) = speak::chat_thread(
        &laptop.paths.socket(),
        committee(),
        message_ref.into(),
        Some((since, WAIT)),
    )
    .await
    .unwrap();
    assert!(arrived, "the Speaker posted nothing: {transcript:#?}");
    transcript
}

/// A person's simulated question is relayed to an agent, the agent's
/// answer comes back into the conversation, and `chat thread --wait` reads
/// it, while the platform (people group and Agent group alike) receives
/// nothing at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_simulated_question_is_relayed_and_answered_in_the_log_only() {
    let world = World::start(60).await;
    world.with_agent_group().await;
    let mut bob = world.laptop("bob");
    bob.open().await;
    let inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &inbox).await;
    world.wait_connected(BOB_CLAUDE, true).await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    world.settle().await;
    let posts_before = world.posts();

    world.brain.interprets(Ok(Intent::Relay {
        targets: vec!["bob".into()],
        kind: RelayKind::Question,
        body: "退款多久到账？".into(),
    }));
    let sent: SimulatedChat = alice
        .ipc(chat_send(
            SessionSelector::default(),
            send("问一下 bob 的 agent，退款多久到账？"),
        ))
        .await;
    assert!(sent.message_ref.starts_with("sim:"), "{sent:?}");
    assert_eq!(sent.group_id, GROUP, "the committee's people group");
    assert!(!sent.live);

    // Stored as the person's message, marked simulated.
    let chat = world
        .hub()
        .get(&committee(), sent.seq.expect("recorded"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chat.kind, Kind::Chat);
    assert_eq!(chat.from, person("alice"), "from the person, not a guest");
    assert!(chat.is_simulated());

    // The relay reaches the agent as any relay would, marked.
    let push = inbox.wait_for("退款多久到账").await;
    assert!(
        push.contains("(simulated)") || push.contains("（模拟）"),
        "{push}"
    );
    let thread = thread_of(&push, "退款多久到账");
    let relay = world
        .hub()
        .get(&committee(), thread)
        .await
        .unwrap()
        .unwrap();
    assert!(relay.is_simulated(), "the relay carries the chat's source");
    assert_eq!(relay.on_behalf_of, Some(person("alice")));

    // The Speaker acknowledges in the conversation.
    let first = wait_thread(&alice, &sent.message_ref, None).await;
    assert_eq!(first.entries[0].from, person("alice"));
    assert_eq!(first.entries[0].seq, sent.seq);

    bob.answer("s-bob", thread, "一般 3 个工作日。").await;
    let seen = first.entries.len();
    let answered = eventually_async("the answer in the conversation", || async {
        let t = wait_thread(&alice, &sent.message_ref, Some(seen)).await;
        t.entries
            .iter()
            .any(|e| e.from == Address::Speaker && e.text.contains("一般 3 个工作日"))
            .then_some(t)
    })
    .await;
    // The relay's task card went to the Agent group of the conversation,
    // not of the platform.
    assert!(
        answered
            .entries
            .iter()
            .any(|e| e.card && e.group_id.as_deref() == Some(AGENTS)),
        "{answered:#?}"
    );

    // Nothing reached the platform: not the acknowledgement, not the card,
    // not the answer.
    world.settle().await;
    let after = world.posts();
    assert_eq!(
        after.len(),
        posts_before.len(),
        "posts made for a simulated message: {:#?}",
        &after[posts_before.len()..]
    );
    // And what people read leaves it out.
    let brief = world.hub().brief(&committee()).await.unwrap();
    assert!(
        brief.open_questions.iter().all(|q| q.seq != thread),
        "{brief:#?}"
    );

    alice.close().await;
    bob.close().await;
    world.stop().await;
}

/// With `--live`, the message is posted into the real group, marked, and
/// the Speaker's answer goes to the platform under it, recorded in the
/// conversation as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_simulated_message_posts_through_the_surface() {
    let world = World::start(60).await;
    world.bind().await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    world.settle().await;

    world.brain.interprets(Ok(Intent::Recall));
    let sent: SimulatedChat = alice
        .ipc(chat_send(
            SessionSelector::default(),
            SimulateChat {
                live: true,
                ..send("定了什么？")
            },
        ))
        .await;
    assert!(sent.live);
    assert!(!sent.message_ref.starts_with("sim:"), "the platform's ref");
    let root = world
        .wait_post("the simulated message in the group", |p| {
            p.message_ref == sent.message_ref
        })
        .await;
    assert!(root.is_root_in(GROUP), "{root:?}");
    assert!(root.markdown.contains("(simulated)"), "{}", root.markdown);
    assert!(root.markdown.contains("定了什么"), "{}", root.markdown);

    let reply = world
        .wait_post("the Speaker answers under it", |p| {
            p.replies_to(&sent.message_ref)
        })
        .await;
    let transcript = wait_thread(&alice, &sent.message_ref, None).await;
    assert!(transcript.live);
    assert!(
        transcript
            .entries
            .iter()
            .any(|e| e.from == Address::Speaker && e.message_ref == reply.message_ref),
        "{transcript:#?}"
    );
    alice.close().await;
    world.stop().await;
}

/// Only a person, as themselves, may simulate: not an agent, not as
/// someone else without the Speaker's office, and never so that the
/// message would come from a guest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agents_and_impersonation_are_refused() {
    let world = Setup::new().person("dave", false).start().await;
    world.bind().await;
    let mut bob = world.laptop("bob");
    bob.open().await;
    let inbox = bob.inbox("s-bob");
    bob.session_start("s-bob", &inbox).await;
    world.wait_connected(BOB_CLAUDE, true).await;
    world.settle().await;
    let chats = |events: Vec<senate_core::Event>| {
        events
            .into_iter()
            .filter(|e| e.kind == Kind::Chat && e.is_simulated())
            .count()
    };

    // From inside bob's coding session: the caller is the agent.
    let refused = bob
        .try_ipc::<SimulatedChat>(chat_send(Laptop::session("s-bob"), send("hi")))
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("is an agent"), "{refused}");

    // Bob as alice, without the office.
    let refused = bob
        .try_ipc::<SimulatedChat>(chat_send(
            SessionSelector::default(),
            SimulateChat {
                as_person: Some(name("alice")),
                ..send("hi")
            },
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("only their own"), "{refused}");

    // Bob is not the Speaker's office.
    let refused = bob
        .try_ipc::<SimulatedChat>(chat_send(
            SessionSelector::default(),
            SimulateChat {
                office: true,
                as_person: Some(name("alice")),
                ..send("hi")
            },
        ))
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("office"), "{refused}");

    // Dave has no account on the platform: his message would come from a
    // guest, so it is not simulated at all.
    let mut dave = world.laptop("dave");
    dave.open().await;
    let refused = dave
        .try_ipc::<SimulatedChat>(chat_send(SessionSelector::default(), send("hi")))
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("guest"), "{refused}");

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(chats(world.events().await), 0, "nothing was injected");
    dave.close().await;
    bob.close().await;
    world.stop().await;
}
