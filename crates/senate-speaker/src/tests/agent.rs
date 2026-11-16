//! Agent mode: [`SpeakerMode::Agent`] hands each mention to
//! [`Brain::converse`](crate::Brain::converse); the [`AgentTurn`]'s
//! state-changing actions are restated as one numbered list and confirmed
//! with one yes, its relays run at once; replay and restart boundaries.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use senate_core::{Address, Hub, Kind, Locale, OutputLevel, PresenceState, Seq, meta};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::*;
use super::surface::{Caps, FakeSurface};
use crate::{AgentTurn, BrainError, Intent, RelayKind, ScriptedBrain, SpeakerMode};

async fn rig() -> Rig {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP)).await.unwrap();
    start(hub, FakeSurface::new(), ScriptedBrain::new()).await
}

async fn start(hub: Arc<MemoryHub>, surface: Arc<FakeSurface>, brain: Arc<ScriptedBrain>) -> Rig {
    let mut config = config();
    config.mode = SpeakerMode::Agent;
    Rig::start_config(hub, surface, brain, config).await
}

/// `event` as a guest (someone not on the roster) sends it.
fn from_guest(mut event: senate_core::NewEvent) -> senate_core::NewEvent {
    event.from = guest(STRANGER);
    event
}

fn decide(body: &str) -> Intent {
    Intent::Decide {
        body: body.into(),
        supersedes: None,
    }
}

async fn confirm(rig: &Rig, root: Seq) {
    let plan = rig
        .surface
        .posts()
        .into_iter()
        .find(|p| p.markdown.contains("1. ") && p.markdown.contains("回复「对」"))
        .unwrap_or_else(|| panic!("missing plan: {:?}", rig.surface.texts()));
    rig.append(in_thread(
        "pm",
        "对",
        "yes",
        root,
        plan.thread_ref,
        &plan.message_ref,
    ))
    .await;
    rig.settle().await;
}

#[tokio::test]
async fn multiple_actions_wait_for_one_confirmation_and_have_distinct_ids() {
    let rig = rig().await;
    rig.brain.converses(Ok(AgentTurn::act(
        "好的，先确认一下。",
        vec![
            Intent::Bind {
                committee: "refund-v3".into(),
            },
            decide("重试上限 3 次"),
            decide("指数退避"),
        ],
    )));
    let root = rig.append(mention("pm", "改绑后记两条决策", "ask")).await;
    rig.settle().await;
    assert_eq!(rig.surface.posts().len(), 2);
    let plan = rig.surface.texts().pop().unwrap();
    assert!(plan.contains("1. ") && plan.contains("2. ") && plan.contains("3. "));
    assert_eq!(plan.matches("回复「对」").count(), 1);
    assert_eq!(
        rig.hub
            .events(&refund())
            .iter()
            .filter(|e| e.kind == Kind::Binding)
            .count(),
        1
    );
    assert!(
        !rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.kind == Kind::Decision)
    );
    // Every line names the committee it acts on; steps after the Bind act on
    // refund-v3.
    assert!(
        plan.contains("把本群从委员会 refund-v2 改绑到委员会 refund-v3"),
        "{plan}"
    );
    assert!(
        plan.contains("2. 在 refund-v3 记录决策「重试上限 3 次」"),
        "{plan}"
    );
    assert!(
        plan.contains("3. 在 refund-v3 记录决策「指数退避」"),
        "{plan}"
    );
    confirm(&rig, root.seq).await;
    let events = rig.hub.events(&refund());
    assert!(events.iter().any(|e| {
        e.binding()
            .is_some_and(|b| b.action == senate_core::BindingAction::Unbind)
    }));
    assert!(!events.iter().any(|e| e.kind == Kind::Decision));
    let moved = rig.hub.events(&committee("refund-v3"));
    let bind = moved.iter().find(|e| e.kind == Kind::Binding).unwrap();
    let decisions: Vec<_> = moved.iter().filter(|e| e.kind == Kind::Decision).collect();
    assert_eq!(decisions.len(), 2);
    assert!(bind.seq < decisions[0].seq && decisions[0].seq < decisions[1].seq);
    assert_eq!(decisions[0].body, "重试上限 3 次");
    assert_eq!(decisions[1].body, "指数退避");
    assert_eq!(decisions[0].on_behalf_of, Some(user("pm")));
    assert_ne!(decisions[0].id, decisions[1].id);
    // Said in refund-v2's log: no cross-committee link, acknowledged in the
    // plan's result instead.
    assert!(
        decisions
            .iter()
            .all(|d| !d.meta.contains_key(meta::RELAYED_FROM))
    );
    let result = rig
        .surface
        .texts()
        .into_iter()
        .find(|t| t.contains("1. 已执行"))
        .unwrap();
    assert!(
        result.contains(&format!("已记录决策（seq {}）", decisions[0].seq)),
        "{result}"
    );
    assert_eq!(rig.surface.conflicts(), 0);
    rig.stop().await;
}

#[tokio::test]
async fn steps_after_a_bind_wait_for_the_confirmation_and_run_on_the_new_committee() {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP)).await.unwrap();
    // A decision only refund-v2 has: superseding it on refund-v3 is refused.
    let old = hub
        .append(senate_core::NewEvent::new(
            refund(),
            Kind::Decision,
            agent("alice"),
            "退款上限 30 天",
        ))
        .await
        .unwrap()
        .event;
    let rig = start(hub, FakeSurface::new(), ScriptedBrain::new()).await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            Intent::Relay {
                targets: vec!["alice".into()],
                kind: RelayKind::Note,
                body: "先别合并".into(),
            },
            Intent::Bind {
                committee: "refund-v3".into(),
            },
            Intent::Decide {
                body: "退款上限 7 天".into(),
                supersedes: Some(old.seq),
            },
            Intent::Relay {
                targets: vec!["bob".into()],
                kind: RelayKind::Note,
                body: "改到 v3 了".into(),
            },
            Intent::Status,
        ],
    )));
    let root = rig.append(mention("pm", "转告、改绑、再转告", "ask")).await;
    rig.settle().await;

    // Before the Bind: the relay ran at once, on refund-v2.
    assert!(
        rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.from == Address::Speaker && e.body == "先别合并")
    );
    // The supersedes was checked against refund-v3, where seq N is no
    // decision: that step is reported and left out.
    let texts = rig.surface.texts();
    assert!(
        texts
            .iter()
            .any(|t| t.contains(&format!("seq {}", old.seq))),
        "{texts:?}"
    );
    // After the Bind: nothing on refund-v3 yet, the relay and status wait.
    assert!(rig.hub.events(&committee("refund-v3")).is_empty());
    let plan = texts
        .iter()
        .find(|t| t.contains("回复「对」"))
        .unwrap()
        .clone();
    assert!(
        plan.contains("1. 把本群从委员会 refund-v2 改绑到委员会 refund-v3"),
        "{plan}"
    );
    assert!(
        plan.contains("2. 在 refund-v3 转告 [senator::bob/claude]：「改到 v3 了」"),
        "{plan}"
    );
    assert!(plan.contains("3. 在 refund-v3 查看本群的绑定"), "{plan}");
    assert!(!plan.contains("退款上限 7 天"), "{plan}");

    confirm(&rig, root.seq).await;
    let moved = rig.hub.events(&committee("refund-v3"));
    let bind = moved.iter().find(|e| e.kind == Kind::Binding).unwrap();
    let relay = moved
        .iter()
        .find(|e| e.from == Address::Speaker && e.body == "改到 v3 了")
        .unwrap();
    assert!(bind.seq < relay.seq);
    assert_eq!(relay.to, vec![agent("bob")]);
    assert!(!relay.meta.contains_key(meta::RELAYED_FROM));
    let result = rig
        .surface
        .texts()
        .into_iter()
        .find(|t| t.contains("1. 已执行"))
        .unwrap();
    assert!(result.contains("已转给 [senator::bob/claude]"), "{result}");
    assert!(result.contains("3. 已执行"), "{result}");
    rig.stop().await;
}

#[tokio::test]
async fn steps_after_an_unbind_are_left_out_except_a_new_binding() {
    let rig = rig().await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            Intent::Unbind,
            decide("解绑后记录"),
            Intent::Bind {
                committee: "refund-v4".into(),
            },
            decide("在 v4 记录"),
        ],
    )));
    let root = rig.append(mention("pm", "解绑、记录、再绑", "ask")).await;
    rig.settle().await;
    let texts = rig.surface.texts();
    assert!(
        texts.iter().any(|t| t.contains("这一步排在解绑之后")),
        "{texts:?}"
    );
    let plan = texts
        .iter()
        .find(|t| t.contains("回复「对」"))
        .unwrap()
        .clone();
    assert!(
        plan.contains("1. 解除本群与委员会 refund-v2 的绑定"),
        "{plan}"
    );
    assert!(plan.contains("2. 把本群绑定到委员会 refund-v4"), "{plan}");
    assert!(
        plan.contains("3. 在 refund-v4 记录决策「在 v4 记录」"),
        "{plan}"
    );
    assert!(!plan.contains("解绑后记录"), "{plan}");
    confirm(&rig, root.seq).await;
    assert!(
        !rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.kind == Kind::Decision)
    );
    let v4 = rig.hub.events(&committee("refund-v4"));
    assert!(v4.iter().any(|e| e.kind == Kind::Binding));
    assert!(
        v4.iter()
            .any(|e| e.kind == Kind::Decision && e.body == "在 v4 记录")
    );
    rig.stop().await;
}

#[tokio::test]
async fn relays_run_immediately_and_each_question_gets_its_own_triage() {
    let rig = rig().await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            Intent::Relay {
                targets: vec!["alice".into()],
                kind: RelayKind::Question,
                body: "一号问题".into(),
            },
            Intent::Relay {
                targets: vec!["bob".into()],
                kind: RelayKind::Question,
                body: "二号问题".into(),
            },
            Intent::AdoptAgentGroup { committee: None },
        ],
    )));
    let root = rig
        .append(mention("pm", "问他们，再设 agent 群", "ask"))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let relays: Vec<_> = events
        .iter()
        .filter(|e| e.from == Address::Speaker && e.kind == Kind::Question)
        .collect();
    assert_eq!(relays.len(), 2);
    assert_eq!(relays[0].body, "一号问题");
    assert_eq!(relays[1].body, "二号问题");
    assert_eq!(rig.brain.triage_inputs().len(), 2);
    assert!(!events.iter().any(|e| {
        e.binding()
            .is_some_and(|b| b.role == senate_core::GroupRole::Agents)
    }));
    confirm(&rig, root.seq).await;
    assert!(rig.hub.events(&refund()).iter().any(|e| {
        e.binding()
            .is_some_and(|b| b.role == senate_core::GroupRole::Agents)
    }));
    rig.stop().await;
}

#[tokio::test]
async fn clarification_is_scoped_to_speaker_group_and_thread_and_contains_history() {
    let rig = rig().await;
    rig.brain
        .converses(Ok(AgentTurn::ask("要把谁的工作交给 bob？")))
        .converses(Ok(AgentTurn::act(
            "",
            vec![Intent::Reassign {
                from: "alice".into(),
                to: "bob".into(),
            }],
        )));
    let root = rig.append(mention("pm", "把她的工作交给 bob", "ask")).await;
    rig.settle().await;
    let reply = rig.surface.posts()[0].clone();
    rig.append(in_thread(
        "bob",
        "alice",
        "wrong-person",
        root.seq,
        reply.thread_ref.clone(),
        &reply.message_ref,
    ))
    .await;
    rig.append(in_thread(
        "pm",
        "alice",
        "wrong-thread",
        Seq(999),
        Some("other".into()),
        "other",
    ))
    .await;
    let mut other = in_thread(
        "pm",
        "alice",
        "wrong-group",
        root.seq,
        reply.thread_ref.clone(),
        &reply.message_ref,
    );
    other.source.as_mut().unwrap().group_id = "other".into();
    rig.append(other).await;
    rig.settle().await;
    assert_eq!(rig.brain.conversations().len(), 1);
    let follow = rig
        .append(in_thread(
            "pm",
            "alice",
            "answer",
            root.seq,
            reply.thread_ref,
            &reply.message_ref,
        ))
        .await;
    rig.settle().await;
    let conversations = rig.brain.conversations();
    assert_eq!(conversations.len(), 2);
    assert_eq!(conversations[1].utterance.text, "alice");
    assert_eq!(conversations[1].turns.len(), 2);
    assert_eq!(conversations[1].turns[0].speaker, user("pm"));
    assert_eq!(conversations[1].turns[1].speaker, Address::Speaker);
    assert_eq!(conversations[1].turns[1].text, "要把谁的工作交给 bob？");
    confirm(&rig, follow.seq).await;
    let presence = rig.hub.presence(&refund()).await.unwrap();
    assert!(
        presence
            .agents
            .iter()
            .any(|p| p.agent == agent_path("alice")
                && p.state == PresenceState::Reassigned
                && p.new_owner == Some(agent_path("bob")))
    );
    rig.stop().await;
}

#[tokio::test]
async fn limits_members_validation_and_fixed_syntax_keep_their_boundaries() {
    let rig = rig().await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![decide("陌生人的决策"), Intent::Status],
    )));
    // A guest asks: the decision is refused per step, the status answered.
    rig.append(from_guest(mention("pm", "记一下", "stranger")))
        .await;
    rig.settle().await;
    assert!(
        rig.surface
            .texts()
            .iter()
            .any(|t| t == &crate::render::guest_refused(Locale::Zh))
    );
    assert!(rig.surface.texts().iter().any(|t| t.contains("refund-v2")));
    rig.brain
        .converses(Ok(AgentTurn::act("不应发出", vec![decide("x"); 6])));
    rig.append(mention("pm", "太多操作", "too-many")).await;
    rig.settle().await;
    assert!(!rig.surface.texts().iter().any(|t| t == "不应发出"));
    assert!(rig.surface.texts().last().unwrap().contains("最多"));
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            Intent::Bind {
                committee: "bad committee".into(),
            },
            decide(" "),
            Intent::Decide {
                body: "取代不存在的决策".into(),
                supersedes: Some(Seq(999)),
            },
            Intent::Reassign {
                from: "nobody".into(),
                to: "bob".into(),
            },
            decide("有效"),
        ],
    )));
    let root = rig.append(mention("pm", "部分有效", "valid")).await;
    rig.settle().await;
    let plan = rig.surface.texts().pop().unwrap();
    assert!(plan.contains("有效") && !plan.contains("2. "));
    confirm(&rig, root.seq).await;
    assert_eq!(
        rig.hub
            .events(&refund())
            .iter()
            .filter(|e| e.kind == Kind::Decision)
            .count(),
        1
    );
    rig.brain
        .converses(Err(BrainError::Unavailable("offline".into())));
    rig.append(mention("pm", "自然语言", "failed")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last(),
        Some(&crate::render::not_understood(Locale::Zh))
    );
    let calls = rig.brain.conversations().len();
    rig.append(mention("pm", "/bind refund-v3", "fixed")).await;
    rig.settle().await;
    assert_eq!(rig.brain.conversations().len(), calls);
    assert!(
        rig.hub
            .events(&committee("refund-v3"))
            .iter()
            .any(|e| e.kind == Kind::Binding)
    );
    rig.stop().await;
}

#[tokio::test]
async fn invites_and_output_level_also_wait_and_a_failure_stops_the_plan() {
    let hub = MemoryHub::new();
    hub.append(agents_bind_event(&refund(), GROUP))
        .await
        .unwrap();
    let rig = start(hub, FakeSurface::new(), ScriptedBrain::new()).await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            decide("先记录"),
            Intent::InviteToAgentGroup {
                who: vec!["alice".into()],
            },
            Intent::SetOutputLevel {
                level: OutputLevel::All,
            },
        ],
    )));
    let root = rig.append(mention("pm", "记录、邀请、改输出", "ask")).await;
    rig.settle().await;
    assert!(
        !rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.meta.contains_key(meta::OUTPUT_LEVEL))
    );
    confirm(&rig, root.seq).await;
    assert_eq!(
        rig.hub
            .events(&refund())
            .iter()
            .filter(|e| e.kind == Kind::Decision)
            .count(),
        1
    );
    assert!(
        !rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.meta.contains_key(meta::OUTPUT_LEVEL))
    );
    let result = rig
        .surface
        .texts()
        .into_iter()
        .find(|t| t.contains("执行失败"))
        .unwrap();
    assert!(result.contains("1. 已执行") && result.contains("后续 1 项未执行"));
    rig.stop().await;
}

#[tokio::test]
async fn output_and_invite_succeed_together_after_confirmation() {
    let hub = MemoryHub::new();
    hub.append(agents_bind_event(&refund(), GROUP))
        .await
        .unwrap();
    let surface = FakeSurface::with(Caps::ALL);
    let rig = start(hub, surface.clone(), ScriptedBrain::new()).await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            Intent::InviteToAgentGroup {
                who: vec!["alice".into()],
            },
            Intent::SetOutputLevel {
                level: OutputLevel::All,
            },
        ],
    )));
    let root = rig.append(mention("pm", "邀请并改输出", "ask")).await;
    rig.settle().await;
    assert!(surface.invites().is_empty());
    confirm(&rig, root.seq).await;
    assert!(!surface.invites().is_empty());
    assert!(
        rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.meta.get(meta::OUTPUT_LEVEL) == Some(&json!("all")))
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn conversations_expire_and_confirmations_cancel() {
    let rig = rig().await;
    rig.brain.converses(Ok(AgentTurn::ask("谁？")));
    let root = rig.append(mention("pm", "改派", "ask")).await;
    rig.settle().await;
    let reply = rig.surface.posts()[0].clone();
    tokio::time::advance(Duration::from_secs(601)).await;
    rig.append(in_thread(
        "pm",
        "alice",
        "late",
        root.seq,
        reply.thread_ref,
        &reply.message_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(rig.brain.conversations().len(), 1);
    rig.brain
        .converses(Ok(AgentTurn::act("", vec![decide("取消的决策")])));
    let root = rig.append(mention("pm", "记录", "plan")).await;
    rig.settle().await;
    let reply = rig.surface.posts().pop().unwrap();
    rig.append(in_thread(
        "pm",
        "等等",
        "no",
        root.seq,
        reply.thread_ref,
        &reply.message_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last(),
        Some(&crate::render::cancelled(Locale::Zh))
    );
    assert!(
        !rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.kind == Kind::Decision)
    );
    rig.stop().await;
}

#[tokio::test]
async fn replay_does_not_call_the_model_or_repeat_actions_and_restart_loses_clarification() {
    let rig = rig().await;
    rig.brain.converses(Ok(AgentTurn::ask("谁？")));
    let root = rig.append(mention("pm", "改派", "ask")).await;
    rig.settle().await;
    let reply = rig.surface.posts()[0].clone();
    let hub = rig.hub.clone();
    let surface = rig.surface.clone();
    let brain = rig.brain.clone();
    rig.stop().await;
    hub.set_cursor(&refund(), crate::CURSOR, Seq(1))
        .await
        .unwrap();
    let rig = start(hub, surface, brain).await;
    rig.append(in_thread(
        "pm",
        "alice",
        "after-restart",
        root.seq,
        reply.thread_ref,
        &reply.message_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(rig.brain.conversations().len(), 1);
    assert_eq!(rig.surface.posts().len(), 1);
    rig.stop().await;
}

#[tokio::test]
async fn unbound_agent_clarification_and_batched_binding_use_the_same_limits() {
    let rig = start(MemoryHub::new(), FakeSurface::new(), ScriptedBrain::new()).await;
    rig.brain
        .converses(Ok(AgentTurn::ask("哪个 committee？")))
        .converses(Ok(AgentTurn::act(
            "",
            vec![
                Intent::Bind {
                    committee: "refund-v2".into(),
                },
                Intent::Status,
            ],
        )));
    rig.mention_unbound(unbound("pm", "绑定", "ask")).await;
    let reply = rig.surface.posts()[0].clone();
    let mut follow = unbound("pm", "refund-v2", "follow");
    follow.addressing = "thread".into();
    follow.reply_to_message_ref = Some(reply.message_ref.clone());
    let mut stranger = follow.clone();
    stranger.sender = user("bob");
    stranger.message_ref = "wrong-speaker".into();
    rig.mention_unbound(stranger).await;
    assert_eq!(rig.brain.conversations().len(), 1);
    rig.mention_unbound(follow).await;
    assert_eq!(rig.brain.conversations().len(), 2);
    assert_eq!(rig.brain.conversations()[1].turns.len(), 2);
    assert!(rig.hub.events(&refund()).is_empty());
    let plan = rig.surface.posts().pop().unwrap();
    let mut yes = unbound("pm", "对", "yes");
    yes.addressing = "thread".into();
    yes.thread_ref = plan.thread_ref;
    yes.reply_to_message_ref = Some(plan.message_ref);
    rig.mention_unbound(yes).await;
    assert!(
        rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.binding().is_some_and(|b| b.group_id == "random"))
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_stall_reuses_the_turn_and_its_completed_actions() {
    let rig = rig().await;
    rig.brain
        .converses(Ok(AgentTurn::act("", vec![decide("一"), decide("二")])));
    let root = rig.append(mention("pm", "记两条", "ask")).await;
    rig.settle().await;
    let plan = rig.surface.posts().pop().unwrap();
    rig.surface
        .fail_next(senate_core::SurfaceError::Unavailable("offline".into()));
    rig.append(in_thread(
        "pm",
        "对",
        "yes",
        root.seq,
        plan.thread_ref,
        &plan.message_ref,
    ))
    .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    rig.settle().await;
    assert_eq!(rig.brain.conversations().len(), 1);
    assert_eq!(
        rig.hub
            .events(&refund())
            .iter()
            .filter(|e| e.kind == Kind::Decision)
            .count(),
        2
    );
    assert_eq!(
        rig.surface
            .texts()
            .iter()
            .filter(|t| t.contains("1. 已执行"))
            .count(),
        1
    );
    assert_eq!(rig.surface.conflicts(), 0);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_transient_hub_failure_resumes_at_the_failed_step_even_after_confirmation_ttl() {
    let rig = rig().await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![decide("先完成"), decide("稍后重试"), decide("最后完成")],
    )));
    let root = rig.append(mention("pm", "记录三条", "ask")).await;
    rig.settle().await;
    rig.hub.fail_append(
        "稍后重试",
        senate_core::HubError::Unavailable("internal database connection details".into()),
    );
    confirm(&rig, root.seq).await;
    assert_eq!(rig.hub.append_attempts("先完成"), 1);
    assert_eq!(rig.hub.append_attempts("稍后重试"), 1);
    assert_eq!(rig.hub.append_attempts("最后完成"), 0);
    assert!(
        !rig.surface
            .texts()
            .iter()
            .any(|t| t.contains("执行失败") || t.contains("internal database"))
    );
    // Confirmation has already been given: its retry must survive the pending TTL.
    tokio::time::advance(Duration::from_secs(601)).await;
    rig.settle().await;
    assert_eq!(
        rig.hub.append_attempts("先完成"),
        1,
        "a completed step is skipped"
    );
    assert_eq!(rig.hub.append_attempts("稍后重试"), 2);
    assert_eq!(rig.hub.append_attempts("最后完成"), 1);
    let decisions: Vec<_> = rig
        .hub
        .events(&refund())
        .into_iter()
        .filter(|e| e.kind == Kind::Decision)
        .map(|e| e.body)
        .collect();
    assert_eq!(decisions, ["先完成", "稍后重试", "最后完成"]);
    assert!(rig.surface.texts().iter().any(|t| t.contains("3. 已执行")));
    assert_eq!(rig.brain.conversations().len(), 1);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_transient_surface_failure_retries_the_same_operation_without_rerunning_completed_steps()
{
    let hub = MemoryHub::new();
    hub.append(agents_bind_event(&refund(), GROUP))
        .await
        .unwrap();
    let rig = start(hub, FakeSurface::with(Caps::ALL), ScriptedBrain::new()).await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            decide("先完成"),
            Intent::InviteToAgentGroup {
                who: vec!["alice".into()],
            },
            decide("最后完成"),
        ],
    )));
    let root = rig.append(mention("pm", "记录并邀请", "ask")).await;
    rig.settle().await;
    rig.surface
        .fail_next_invite(senate_core::SurfaceError::Unavailable(
            "internal platform endpoint".into(),
        ));
    confirm(&rig, root.seq).await;
    assert_eq!(rig.hub.append_attempts("先完成"), 1);
    assert_eq!(rig.hub.append_attempts("最后完成"), 0);
    assert!(
        !rig.surface
            .texts()
            .iter()
            .any(|t| t.contains("执行失败") || t.contains("internal platform"))
    );
    tokio::time::advance(Duration::from_secs(3)).await;
    rig.settle().await;
    assert_eq!(rig.hub.append_attempts("先完成"), 1);
    assert_eq!(rig.hub.append_attempts("最后完成"), 1);
    let attempts = rig.surface.invite_attempts();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0], attempts[1],
        "the retry keeps its idempotency key"
    );
    assert_eq!(rig.surface.invites().len(), 1);
    assert!(rig.surface.texts().iter().any(|t| t.contains("3. 已执行")));
    rig.stop().await;
}

#[tokio::test]
async fn permanent_hub_refusals_stop_the_plan_without_exposing_internal_errors() {
    for error in [
        senate_core::HubError::Rejected("private validation details".into()),
        senate_core::HubError::NotFound("private committee storage key".into()),
    ] {
        let rig = rig().await;
        rig.brain.converses(Ok(AgentTurn::act(
            "",
            vec![decide("先完成"), decide("被拒绝"), decide("不应执行")],
        )));
        let root = rig.append(mention("pm", "记录三条", "ask")).await;
        rig.settle().await;
        rig.hub.fail_append("被拒绝", error);
        confirm(&rig, root.seq).await;
        assert_eq!(rig.hub.append_attempts("先完成"), 1);
        assert_eq!(rig.hub.append_attempts("被拒绝"), 1);
        assert_eq!(rig.hub.append_attempts("不应执行"), 0);
        let text = rig
            .surface
            .texts()
            .into_iter()
            .find(|t| t.contains("执行失败"))
            .unwrap();
        assert!(text.contains("1. 已执行") && text.contains("后续 1 项未执行"));
        assert!(!rig.surface.texts().iter().any(|t| t.contains("private")));
        rig.stop().await;
    }
}

#[tokio::test]
async fn a_permanent_surface_refusal_stops_the_plan_without_exposing_internal_errors() {
    let hub = MemoryHub::new();
    hub.append(agents_bind_event(&refund(), GROUP))
        .await
        .unwrap();
    let rig = start(hub, FakeSurface::with(Caps::ALL), ScriptedBrain::new()).await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            Intent::InviteToAgentGroup {
                who: vec!["alice".into()],
            },
            decide("不应执行"),
        ],
    )));
    let root = rig.append(mention("pm", "邀请再记录", "ask")).await;
    rig.settle().await;
    rig.surface
        .fail_next_invite(senate_core::SurfaceError::Rejected(
            "private platform error".into(),
        ));
    confirm(&rig, root.seq).await;
    assert_eq!(rig.hub.append_attempts("不应执行"), 0);
    assert!(
        rig.surface
            .texts()
            .iter()
            .any(|t| t.contains("执行失败") && t.contains("后续 1 项未执行"))
    );
    assert!(!rig.surface.texts().iter().any(|t| t.contains("private")));
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_unbound_followup_refreshes_expiry_before_the_model_runs() {
    let rig = start(MemoryHub::new(), FakeSurface::new(), ScriptedBrain::new()).await;
    rig.brain
        .converses(Ok(AgentTurn::ask("哪位？")))
        .converses_after(Duration::from_secs(2), Ok(AgentTurn::ask("要交给谁？")))
        .converses(Ok(AgentTurn::say("明白了")));
    rig.mention_unbound(unbound("pm", "交接", "ask")).await;
    let first = rig.surface.posts()[0].clone();
    tokio::time::advance(Duration::from_secs(599)).await;
    let mut follow = unbound("pm", "alice", "follow");
    follow.addressing = "thread".into();
    follow.thread_ref = first.thread_ref.clone();
    follow.thread_root_message_ref = Some("ask".into());
    rig.mention_unbound(follow).await;
    let mut last = unbound("pm", "bob", "last");
    last.addressing = "thread".into();
    last.thread_ref = first.thread_ref;
    last.thread_root_message_ref = Some("ask".into());
    rig.mention_unbound(last).await;
    let conversations = rig.brain.conversations();
    assert_eq!(conversations.len(), 3);
    assert_eq!(conversations[2].turns.len(), 4);
    assert_eq!(conversations[2].turns[3].text, "要交给谁？");
    rig.stop().await;
}

#[tokio::test]
async fn a_guests_plan_is_refused_step_by_step_and_nothing_waits_for_a_yes() {
    let rig = rig().await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![
            Intent::Bind {
                committee: "refund-v3".into(),
            },
            Intent::Status,
        ],
    )));
    rig.append(from_guest(mention("pm", "改绑到 v3", "ask")))
        .await;
    rig.settle().await;
    let texts = rig.surface.texts();
    assert_eq!(
        texts
            .iter()
            .filter(|t| **t == crate::render::guest_refused(Locale::Zh))
            .count(),
        1,
        "{texts:?}"
    );
    // The read-only step is answered; no plan is restated.
    assert!(texts.iter().any(|t| t.contains("refund-v2")), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("回复「对」")), "{texts:?}");
    assert!(rig.hub.events(&committee("refund-v3")).is_empty());
    rig.stop().await;
}

/// alice has a second agent; neither of hers is connected.
async fn rig_with_two_agents_of_alice() -> Rig {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP)).await.unwrap();
    hub.seat(&path("alice/codex"));
    start(hub, FakeSurface::new(), ScriptedBrain::new()).await
}

#[tokio::test]
async fn a_relay_to_a_person_with_several_agents_asks_which_and_the_answer_carries_it_on() {
    let rig = rig_with_two_agents_of_alice().await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![Intent::Relay {
            targets: vec!["alice".into()],
            kind: RelayKind::Question,
            body: "重试上限是多少？".into(),
        }],
    )));
    let root = rig
        .append(mention("pm", "问问 alice 重试上限", "ask"))
        .await;
    rig.settle().await;
    let ask = rig.surface.posts().pop().unwrap();
    assert!(
        ask.markdown
            .starts_with("[senator::alice] 有 2 个 agent，你指的是哪一个？"),
        "{}",
        ask.markdown
    );
    assert!(ask.markdown.contains("1. [senator::alice/claude] 不在线"));
    assert!(ask.markdown.contains("2. [senator::alice/codex] 不在线"));
    assert!(
        matches!(
            &ask.target,
            senate_core::PostTarget::Reply {
                in_thread: true,
                ..
            }
        ),
        "asked in the thread"
    );
    let relayed = |rig: &Rig| {
        rig.hub
            .events(&refund())
            .into_iter()
            .filter(|e| e.from == Address::Speaker && e.body == "重试上限是多少？")
            .collect::<Vec<_>>()
    };
    assert!(
        relayed(&rig).is_empty(),
        "nothing relayed before the answer"
    );

    rig.append(in_thread(
        "pm",
        "2",
        "pick",
        root.seq,
        ask.thread_ref.clone(),
        &ask.message_ref,
    ))
    .await;
    rig.settle().await;
    let relays = relayed(&rig);
    assert_eq!(relays.len(), 1);
    assert_eq!(relays[0].to, vec![agent_named("alice", "codex")]);
    assert_eq!(relays[0].on_behalf_of, Some(user("pm")));
    assert!(
        rig.surface
            .texts()
            .iter()
            .any(|t| t.contains("已转给 [senator::alice/codex]")),
        "{:?}",
        rig.surface.texts()
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
    rig.stop().await;
}

#[tokio::test]
async fn a_relay_to_one_agent_named_outright_goes_straight_there() {
    let rig = rig_with_two_agents_of_alice().await;
    rig.brain.converses(Ok(AgentTurn::act(
        "",
        vec![Intent::Relay {
            targets: vec!["alice/codex".into()],
            kind: RelayKind::Question,
            body: "重试上限是多少？".into(),
        }],
    )));
    rig.append(mention("pm", "问问 alice 的 codex", "ask"))
        .await;
    rig.settle().await;
    let texts = rig.surface.texts();
    assert!(!texts.iter().any(|t| t.contains("哪一个")), "{texts:?}");
    let relay = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.from == Address::Speaker && e.body == "重试上限是多少？")
        .unwrap();
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);
    rig.stop().await;
}

#[tokio::test]
async fn in_english_a_plan_is_restated_run_after_yes_and_too_many_actions_are_refused() {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP)).await.unwrap();
    let mut config = config_in(Locale::En);
    config.mode = SpeakerMode::Agent;
    let rig = Rig::start_config(hub, FakeSurface::new(), ScriptedBrain::new(), config).await;
    rig.brain.converses(Ok(AgentTurn::act(
        "Sure, let me check first.",
        vec![
            Intent::Bind {
                committee: "refund-v3".into(),
            },
            decide("retry at most 3 times"),
        ],
    )));
    let root = rig
        .append(mention(
            "pm",
            "move us to v3 and record the retry cap",
            "ask",
        ))
        .await;
    rig.settle().await;
    let plan = rig.surface.posts().pop().unwrap();
    assert_eq!(
        plan.markdown,
        "I understand:\n\
         1. Move this group from committee refund-v2 to committee refund-v3.\n\
         2. On refund-v3: Record the decision \"retry at most 3 times\".\n\
         Reply \"yes\" to confirm."
    );
    assert!(rig.hub.events(&committee("refund-v3")).is_empty());

    rig.append(in_thread(
        "pm",
        "yes",
        "yes",
        root.seq,
        plan.thread_ref.clone(),
        &plan.message_ref,
    ))
    .await;
    rig.settle().await;
    let moved = rig.hub.events(&committee("refund-v3"));
    let decision = moved.iter().find(|e| e.kind == Kind::Decision).unwrap();
    assert_eq!(decision.body, "retry at most 3 times");
    let result = rig
        .surface
        .texts()
        .into_iter()
        .find(|t| t.starts_with("1. Done: "))
        .unwrap();
    assert!(
        result.starts_with(
            "1. Done: Move this group from committee refund-v2 to committee refund-v3."
        ),
        "{result}"
    );
    assert!(
        result.contains(&format!(
            "2. Done: On refund-v3: Record the decision \"retry at most 3 times\".\nDecision recorded (seq {})",
            decision.seq
        )),
        "{result}"
    );

    rig.brain
        .converses(Ok(AgentTurn::act("not said", vec![decide("x"); 6])));
    rig.append(mention("pm", "too much at once", "too-many"))
        .await;
    rig.settle().await;
    let texts = rig.surface.texts();
    assert!(!texts.iter().any(|t| t == "not said"));
    assert_eq!(
        texts.last().map(String::as_str),
        Some("I run at most 5 operations at a time; nothing in this plan was run.")
    );
    for text in &texts {
        assert_senate_vocabulary(text);
    }
    rig.stop().await;
}
