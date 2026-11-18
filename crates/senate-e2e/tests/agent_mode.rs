//! Agent mode end to end: the Speaker in conversation with people in a
//! chat thread, planning several operations, confirmed with one yes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_core::{
    Address, BindingAction, CommitteeId, GroupRole, Hub, Kind, PresenceState, Seq, meta,
};
use senate_speaker::{AgentTurn, BrainError, CURSOR, Intent, RelayKind, SpeakerMode};
use senated::testing::{Message, Post};

fn decide(body: &str) -> Intent {
    Intent::Decide {
        body: body.into(),
        supersedes: None,
    }
}

/// A world whose Speaker converses, planning at most `max_actions`.
async fn agent_world(max_actions: usize) -> World {
    Setup::new()
        .speaker(move |speaker| {
            speaker.mode = SpeakerMode::Agent;
            speaker.agent_max_actions = max_actions;
        })
        .start()
        .await
}

async fn say(world: &World, sender: &str, message: &str, text: &str, needle: &str) -> Post {
    world
        .say(Message::mention(GROUP, sender, message, text))
        .await;
    world
        .wait_post(needle, |p| {
            p.replies_to(message) && p.markdown.contains(needle)
        })
        .await
}

/// A plain message in the thread of `post`, rooted at `root`: no mention,
/// no quote.
async fn reply(world: &World, sender: &str, message: &str, text: &str, post: &Post, root: &str) {
    world
        .say(
            Message::ambient(GROUP, sender, message, text)
                .in_thread(post.thread_ref.as_deref().unwrap(), root),
        )
        .await;
}

async fn caught_up(world: &World, body: &str) {
    eventually_async("chat processed by the Speaker", || async {
        let chat = world
            .events()
            .await
            .into_iter()
            .find(|e| e.kind == Kind::Chat && e.body == body)?;
        (world.hub().cursor(&committee(), CURSOR).await.unwrap() >= chat.seq).then_some(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn several_operations_change_nothing_until_one_confirmation_then_run_in_order() {
    let world = agent_world(5).await;
    world.bind().await;
    world.brain.converses(Ok(AgentTurn::act(
        "请确认这两步。",
        vec![
            Intent::Bind {
                committee: "refund-v3".into(),
            },
            decide("重试上限 3 次"),
        ],
    )));
    let plan = say(
        &world,
        "u_pm",
        "m_plan",
        "把这个群改绑到 refund-v3，再记一下：重试上限 3 次",
        "回复「对」确认",
    )
    .await;
    assert!(plan.markdown.contains("1. ") && plan.markdown.contains("2. "));
    assert_eq!(plan.markdown.matches("回复「对」确认").count(), 1);
    let before = world.events().await;
    assert!(!before.iter().any(|e| e.kind == Kind::Decision));
    assert!(!before.iter().any(|e| {
        e.binding()
            .is_some_and(|b| b.action == BindingAction::Unbind)
    }));
    let target = CommitteeId::new("refund-v3").unwrap();
    assert!(
        world
            .hub()
            .read(&target, Seq::ZERO, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        world
            .posts()
            .iter()
            .filter(|p| p.replies_to("m_plan") && p.markdown.contains("回复「对」确认"))
            .count(),
        1
    );

    // A plain thread message, without a mention or a quoted bot message.
    reply(&world, "u_pm", "m_yes", "对", &plan, "m_plan").await;
    // Steps after a bind act on the new committee, in order.
    let decision = eventually_async("the confirmed decision on refund-v3", || async {
        world
            .events_of(&target)
            .await
            .into_iter()
            .find(|e| e.kind == Kind::Decision)
    })
    .await;
    let rebound = world
        .events_of(&target)
        .await
        .into_iter()
        .find(|e| {
            e.binding()
                .is_some_and(|b| b.group_id == GROUP && b.action == BindingAction::Bind)
        })
        .unwrap();
    assert!(world.events().await.iter().any(|e| {
        e.binding()
            .is_some_and(|b| b.group_id == GROUP && b.action == BindingAction::Unbind)
    }));
    assert!(
        rebound.seq < decision.seq,
        "rebound first, then the decision"
    );
    assert!(
        !world
            .events()
            .await
            .iter()
            .any(|e| e.kind == Kind::Decision),
        "nothing recorded on the old committee"
    );
    assert!(
        plan.markdown.contains("在 refund-v3 记录决策"),
        "{}",
        plan.markdown
    );
    assert_eq!(decision.body, "重试上限 3 次");
    assert_eq!(decision.on_behalf_of, Some(person("pm")));
    assert_eq!(
        world.brain.conversations().len(),
        1,
        "confirmation bypasses the Brain"
    );
    assert!(world.brain.utterances().is_empty());
    // The gateway follows the rebinding: the group's chat now goes to
    // refund-v3.
    eventually("the group rebound at the gateway", || {
        (world.gateway.as_ref()?.committee_of(GROUP) == Some(target.clone())).then_some(())
    })
    .await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_reaches_the_live_agent_before_group_adoption_is_confirmed() {
    let world = agent_world(5).await;
    world.bind().await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    let inbox = alice.inbox("s-alice");
    alice.session_start("s-alice", &inbox).await;
    world.wait_connected("claude", true).await;
    world.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            Intent::Relay {
                targets: vec!["alice".into()],
                kind: RelayKind::Note,
                body: "先别合并".into(),
            },
            Intent::AdoptAgentGroup { committee: None },
        ],
    )));
    let plan = say(
        &world,
        "u_pm",
        "m_plan",
        "告诉 alice 的 agent 先别合并，然后把这个群设为 agent 群",
        "回复「对」确认",
    )
    .await;
    let pushed = inbox.wait_for("先别合并").await;
    assert!(pushed.contains("[senator::pm]"), "{pushed}");
    let events = world.events().await;
    let relay = events
        .iter()
        .find(|e| e.kind == Kind::Note && e.body == "先别合并")
        .unwrap();
    assert_eq!(relay.to, vec![senator("claude")]);
    assert_eq!(relay.on_behalf_of, Some(person("pm")));
    assert!(relay.meta.contains_key(meta::RELAYED_FROM));
    assert!(!events.iter().any(|e| {
        e.binding()
            .is_some_and(|b| b.group_id == GROUP && b.role == GroupRole::Agents)
    }));
    assert!(plan.markdown.contains("1. ") && !plan.markdown.contains("2. "));
    reply(&world, "u_pm", "m_yes", "对", &plan, "m_plan").await;
    eventually_async("the group adopted after confirmation", || async {
        world
            .events()
            .await
            .into_iter()
            .any(|e| {
                e.binding()
                    .is_some_and(|b| b.group_id == GROUP && b.role == GroupRole::Agents)
            })
            .then_some(())
    })
    .await;
    alice.close().await;
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clarification_continues_without_a_mention_only_for_the_original_speaker() {
    let world = agent_world(5).await;
    world.bind().await;
    // Both agents must be seated for "alice" and "bob" to name one each.
    for (who, agent) in [("alice", "claude"), ("bob", BOB_CLAUDE)] {
        let mut laptop = world.laptop(who);
        laptop.open().await;
        let inbox = laptop.inbox(&format!("s-{who}"));
        laptop.session_start(&format!("s-{who}"), &inbox).await;
        world.wait_connected(agent, true).await;
        laptop.close().await;
    }
    world
        .brain
        .converses(Ok(AgentTurn::ask("要把谁的工作交给 bob？")))
        .converses(Ok(AgentTurn::act(
            "",
            vec![Intent::Reassign {
                from: "alice".into(),
                to: "bob".into(),
            }],
        )));
    let question = say(&world, "u_pm", "m_question", "把她的工作交给 bob", "要把谁").await;
    reply(
        &world,
        "u_bob",
        "m_wrong",
        "我来代答 alice",
        &question,
        "m_question",
    )
    .await;
    caught_up(&world, "我来代答 alice").await;
    assert_eq!(world.brain.conversations().len(), 1);
    reply(&world, "u_pm", "m_alice", "alice", &question, "m_question").await;
    let plan = world
        .wait_post("a reassignment restatement", |p| {
            p.replies_to("m_alice") && p.markdown.contains("回复「对」确认")
        })
        .await;
    assert!(
        plan.markdown.contains("[senator::claude]") && plan.markdown.contains("[senator::copilot]"),
        "{}",
        plan.markdown
    );
    let conversations = world.brain.conversations();
    assert_eq!(conversations.len(), 2);
    assert_eq!(conversations[1].utterance.text, "alice");
    assert_eq!(conversations[1].turns.len(), 2);
    assert_eq!(conversations[1].turns[0].text, "把她的工作交给 bob");
    assert_eq!(conversations[1].turns[1].speaker, Address::Speaker);
    assert_eq!(conversations[1].turns[1].text, "要把谁的工作交给 bob？");
    assert!(
        !world
            .hub()
            .presence(&committee())
            .await
            .unwrap()
            .agents
            .iter()
            .any(|p| p.state == PresenceState::Reassigned)
    );
    reply(&world, "u_pm", "m_yes", "对", &plan, "m_question").await;
    eventually_async("alice's work reassigned to bob", || async {
        world
            .hub()
            .presence(&committee())
            .await
            .unwrap()
            .agents
            .into_iter()
            .find(|p| {
                p.agent == path("claude")
                    && p.state == PresenceState::Reassigned
                    && p.new_owner == Some(path("copilot"))
            })
    })
    .await;
    assert_eq!(world.brain.conversations().len(), 2);
    world.stop().await;
}

/// A guest's state change is refused; a plan longer than the limit runs
/// nothing; a model failure falls back to the fixed syntax, which always
/// works.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn guest_and_action_limits_model_failure_and_fixed_syntax_hold() {
    let world = agent_world(2).await;
    world.bind().await;
    world
        .brain
        .converses(Ok(AgentTurn::act("", vec![decide("陌生人的决策")])));
    let refusal = say(&world, "u_stranger", "m_stranger", "记一下", "名册上的人").await;
    assert!(!refusal.markdown.contains("回复「对」确认"));
    assert!(
        !world
            .events()
            .await
            .iter()
            .any(|e| e.kind == Kind::Decision)
    );

    world.brain.converses(Ok(AgentTurn::act(
        "这句也不该发出",
        vec![
            Intent::Relay {
                targets: vec!["alice".into()],
                kind: RelayKind::Note,
                body: "不该转发".into(),
            },
            decide("不该记录"),
            Intent::Unbind,
        ],
    )));
    let refusal = say(&world, "u_pm", "m_many", "做三件事", "最多").await;
    assert!(refusal.markdown.contains('2'));
    assert_eq!(
        world
            .posts()
            .iter()
            .filter(|p| p.replies_to("m_many"))
            .count(),
        1
    );
    assert!(
        !world
            .events()
            .await
            .iter()
            .any(|e| e.body == "不该转发" || e.kind == Kind::Decision)
    );
    assert!(!world.events().await.iter().any(|e| {
        e.binding()
            .is_some_and(|b| b.action == BindingAction::Unbind)
    }));

    world
        .brain
        .converses(Err(BrainError::Unavailable("model offline".into())));
    let fallback = say(&world, "u_pm", "m_failed", "记个决策", "/bind").await;
    assert!(fallback.markdown.contains("/tell"));
    let calls = world.brain.conversations().len();
    say(&world, "u_pm", "m_fixed", "/bind refund-v3", "refund-v3").await;
    let target = CommitteeId::new("refund-v3").unwrap();
    assert!(world.events_of(&target).await.iter().any(|e| {
        e.binding()
            .is_some_and(|b| b.group_id == GROUP && b.action == BindingAction::Bind)
    }));
    assert_eq!(world.brain.conversations().len(), calls);
    assert!(world.brain.utterances().is_empty());
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unbound_group_clarifies_and_confirms_a_bind_in_the_thread() {
    let world = agent_world(5).await;
    world
        .brain
        .converses(Ok(AgentTurn::ask("要绑定哪个委员会？")))
        .converses(Ok(AgentTurn::act(
            "",
            vec![
                Intent::Bind {
                    committee: COMMITTEE.into(),
                },
                decide("没有绑定时不能记录"),
            ],
        )));
    let question = say(&world, "u_pm", "m_question", "把群绑上", "哪个委员会").await;
    assert_eq!(world.brain.conversations()[0].utterance.committee, None);
    reply(
        &world,
        "u_pm",
        "m_committee",
        COMMITTEE,
        &question,
        "m_question",
    )
    .await;
    let plan = world
        .wait_post("unbound group plan", |p| {
            p.replies_to("m_committee") && p.markdown.contains("回复「对」确认")
        })
        .await;
    assert_eq!(world.brain.conversations().len(), 2);
    assert_eq!(world.brain.conversations()[1].turns.len(), 2);
    assert!(world.events().await.is_empty());
    assert!(world.posts().iter().any(|p| p.replies_to("m_committee")
        && p.markdown.contains("绑定")
        && !p.markdown.contains("回复「对」确认")));
    reply(&world, "u_pm", "m_yes", "对", &plan, "m_question").await;
    eventually_async("confirmed unbound group binding", || async {
        world.events().await.into_iter().find(|e| {
            e.binding()
                .is_some_and(|b| b.group_id == GROUP && b.action == BindingAction::Bind)
        })
    })
    .await;
    assert!(
        !world
            .events()
            .await
            .iter()
            .any(|e| e.kind == Kind::Decision)
    );
    world.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_intent_mode_still_interprets_one_action() {
    let world = World::start(60).await;
    world.bind().await;
    world
        .brain
        .interprets(Ok(decide("intent 模式决策")))
        .converses(Ok(AgentTurn::say("agent 模式不会调用")));
    let plan = say(&world, "u_pm", "m_intent", "记一下", "回复「对」确认").await;
    assert!(!plan.markdown.contains("1. "));
    reply(&world, "u_pm", "m_yes", "对", &plan, "m_intent").await;
    eventually_async("intent decision", || async {
        world
            .events()
            .await
            .into_iter()
            .find(|e| e.kind == Kind::Decision)
    })
    .await;
    assert_eq!(world.brain.utterances().len(), 1);
    assert!(world.brain.conversations().is_empty());
    world.stop().await;
}
