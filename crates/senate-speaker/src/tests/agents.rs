//! The Agent group: created, adopted, invited into, archived; task cards
//! updated in place per output level; the two pinned cards; restarts.

use std::time::Duration;

use senate_core::{
    Address, BindingAction, GroupRole, Hub, Kind, Locale, NewEvent, OutputLevel, PostTarget, Seq,
    StatusPayload, StatusState, Step, UnboundMention, meta,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::{
    GROUP, Rig, STRANGER, agent, agent_named, agent_path, agents_bind_event, answer,
    assert_senate_vocabulary, bind_event, config_in, guest, in_thread, mention, mention_in, path,
    question, refund, unbound, user,
};
use super::surface::{Caps, FakeSurface};
use crate::{Intent, ScriptedBrain};

/// The adopted Agent group's id.
const AGENTS: &str = "agents";

/// A people group and an adopted Agent group.
async fn agents_hub() -> std::sync::Arc<MemoryHub> {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub.append(agents_bind_event(&refund(), AGENTS))
        .await
        .expect("bind agents");
    hub
}

/// `event` as a guest (someone not on the roster) sends it.
fn from_guest(mut event: NewEvent) -> NewEvent {
    event.from = guest(STRANGER);
    event
}

fn root_in(group: &str) -> PostTarget {
    PostTarget::NewRoot {
        group_id: group.into(),
    }
}

fn status(from: Address, root: Seq, status: &StatusPayload) -> NewEvent {
    NewEvent::new(refund(), Kind::Status, from, "")
        .in_thread(root)
        .with_meta(meta::STATUS, status.value())
}

fn progress(title: &str, steps: &[(&str, bool)]) -> StatusPayload {
    let mut s = StatusPayload::new(StatusState::Processing);
    s.title = Some(title.into());
    s.steps = Some(
        steps
            .iter()
            .map(|(label, done)| Step {
                id: (*label).into(),
                label: (*label).into(),
                done: *done,
            })
            .collect(),
    );
    s
}

/// Let the card debounce (2 s by default) run out, on a paused clock.
async fn after_debounce(rig: &Rig) {
    rig.settle().await;
    tokio::time::advance(Duration::from_millis(2_100)).await;
    rig.settle().await;
}

fn agents_bindings(rig: &Rig) -> Vec<(BindingAction, String)> {
    rig.hub
        .events(&refund())
        .iter()
        .filter_map(|e| e.binding())
        .filter(|b| b.role == GroupRole::Agents)
        .map(|b| (b.action, b.group_id))
        .collect()
}

// ----- the Agent group -------------------------------------------------------------

#[tokio::test]
async fn a_group_created_without_its_people_is_bound_and_says_who_is_missing() {
    let hub = MemoryHub::new();
    hub.connect(&refund(), &agent_path("alice"));
    let surface = FakeSurface::with(Caps {
        members: false,
        ..Caps::ALL
    });
    let rig = Rig::start_with(hub, surface, ScriptedBrain::new()).await;
    rig.mention_unbound(unbound("pm", "/bind refund-v2", "m_m1"))
        .await;

    let created = rig.surface.groups()[0].1.clone();
    assert_eq!(
        agents_bindings(&rig),
        vec![(BindingAction::Bind, created)],
        "the group exists, so it is the Agent group"
    );
    let texts = rig.surface.texts();
    let missing = texts
        .iter()
        .find(|t| t.contains("没能把"))
        .unwrap_or_else(|| panic!("{texts:#?}"));
    assert!(missing.contains("[Agent 群] refund-v2"), "{missing}");
    // Accounts not added are reported as the people they belong to.
    assert!(
        missing.contains("[senator::pm]、[senator::alice]"),
        "{missing}"
    );
    assert!(!texts.iter().any(|t| t.contains("还没有 Agent 群")));
}

#[tokio::test]
async fn the_first_binding_creates_the_agent_group_with_its_people() {
    let hub = MemoryHub::new();
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start_with(hub, FakeSurface::with(Caps::ALL), ScriptedBrain::new()).await;
    rig.mention_unbound(unbound("pm", "/bind refund-v2", "m_m1"))
        .await;

    let groups = rig.surface.groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].0.name, "[Agent 群] refund-v2");
    assert_eq!(
        groups[0].0.invite,
        vec!["u_pm", "u_alice"],
        "binder, then connected"
    );
    let created = groups[0].1.clone();
    assert_eq!(
        agents_bindings(&rig),
        vec![(BindingAction::Bind, created.clone())]
    );
    let events = rig.hub.events(&refund());
    let binding = events.last().expect("agents binding");
    assert_eq!(binding.on_behalf_of, Some(user("pm")));
    assert!(
        !rig.surface
            .texts()
            .iter()
            .any(|t| t.contains("还没有 Agent 群")),
        "no hint when it could be created"
    );

    // Its pinned cards, posted and pinned.
    let cards = rig.surface.cards();
    assert_eq!(cards.len(), 2, "{cards:#?}");
    assert!(cards.iter().all(|c| c.target == root_in(&created)));
    assert_eq!(cards[0].first.title, "等人处理");
    assert!(cards[1].first.title.starts_with("今天的摘要"));
    assert_eq!(rig.surface.pins().len(), 2);

    // From now on agent traffic goes there, as a card.
    rig.append(question(
        agent("alice"),
        agent("bob"),
        "网关幂等用哪个 header？",
    ))
    .await;
    rig.settle().await;
    let task = rig.surface.cards().pop().expect("task card");
    assert_eq!(task.target, root_in(&created));
    assert_eq!(task.first.title, "网关幂等用哪个 header？");
    assert!(
        rig.surface
            .posts()
            .iter()
            .all(|p| p.target != root_in(GROUP))
    );
}

#[tokio::test]
async fn without_group_creation_the_hint_is_posted_once_even_after_a_restart() {
    let hub = MemoryHub::new();
    let surface = FakeSurface::new();
    let brain = ScriptedBrain::new();
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    rig.mention_unbound(unbound("pm", "/bind refund-v2", "m_m1"))
        .await;
    let posts = surface.posts();
    assert_eq!(
        posts
            .iter()
            .map(|p| p.markdown.as_str())
            .collect::<Vec<_>>(),
        vec![
            "已绑定到委员会 refund-v2。",
            "还没有 Agent 群：建一个群，把 @Speaker 拉进去，说「这个群是 refund-v2 的 agent 群」",
        ]
    );
    assert_eq!(posts[1].target, root_in("random"));
    rig.stop().await;

    hub.set_cursor(&refund(), crate::CURSOR, Seq::ZERO)
        .await
        .expect("rewind");
    let rig = Rig::start_with(hub, surface.clone(), brain).await;
    assert_eq!(surface.posts(), posts);
    rig.stop().await;
}

#[tokio::test]
async fn a_group_is_adopted_with_the_fixed_syntax_or_restated_and_confirmed() {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;

    // In a group bound to no committee, the committee must be named.
    rig.mention_unbound(unbound("pm", "/agents", "m_m1")).await;
    assert!(
        rig.surface
            .texts()
            .last()
            .is_some_and(|t| t.starts_with("要设为哪个委员会的 Agent 群")),
        "{:?}",
        rig.surface.texts()
    );
    rig.mention_unbound(unbound("pm", "/agents refund-v2", "m_m2"))
        .await;
    assert_eq!(
        agents_bindings(&rig),
        vec![(BindingAction::Bind, "random".to_owned())]
    );
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("本群现在是委员会 refund-v2 的 Agent 群。")
    );
    // A guest may not.
    rig.mention_unbound(UnboundMention {
        group_id: "elsewhere".into(),
        sender: guest(STRANGER),
        ..unbound("pm", "/agents refund-v2", "m_u3")
    })
    .await;
    assert_eq!(agents_bindings(&rig).len(), 1);
    assert_eq!(
        rig.surface.texts().last(),
        Some(&crate::render::guest_refused(Locale::Zh))
    );

    // The mirror target is now that group.
    rig.append(question(agent("alice"), agent("bob"), "去哪个群？"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.posts().last().map(|p| p.target.clone()),
        Some(root_in("random"))
    );

    // In words, in the people group: restated, then confirmed.
    rig.brain
        .interprets(Ok(Intent::AdoptAgentGroup { committee: None }));
    let ask = rig.append(mention("pm", "这个群是 agent 群", "m_h1")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(
            "我理解为：把本群设为委员会 refund-v2 的 Agent 群（agent 之间的往来和任务卡片都发在这里）。回复「对」确认"
        )
    );
    assert_eq!(agents_bindings(&rig).len(), 1, "not yet");
    rig.append(in_thread("pm", "对", "m_h2", ask.seq, None, "m_s0"))
        .await;
    rig.settle().await;
    assert_eq!(
        agents_bindings(&rig).last(),
        Some(&(BindingAction::Bind, GROUP.to_owned())),
        "the latest Agent group wins"
    );
}

#[tokio::test]
async fn people_are_invited_into_the_agent_group() {
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(agents_hub().await, surface.clone(), ScriptedBrain::new()).await;
    rig.append(mention("pm", "/invite alice", "m_h1")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("我没有权限直接拉人进群，请群主手动邀请 alice。")
    );

    surface.set_caps(Caps::ALL);
    rig.append(mention("pm", "/invite @Bob 爱丽丝", "m_h2"))
        .await;
    rig.settle().await;
    assert_eq!(
        surface.invites(),
        vec![(
            AGENTS.to_owned(),
            vec!["u_bob".to_owned(), "u_alice".to_owned()]
        )]
    );
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("已邀请 bob、alice 进 Agent 群。")
    );

    // An agent active on the committee gets its person invited, once.
    rig.append(question(agent("pm"), agent("bob"), "在吗？"))
        .await;
    rig.append(question(agent("pm"), agent("bob"), "还在吗？"))
        .await;
    rig.settle().await;
    let invites = surface.invites();
    assert_eq!(invites.len(), 2, "{invites:?}");
    assert_eq!(invites[1], (AGENTS.to_owned(), vec!["u_pm".to_owned()]));
}

#[tokio::test]
async fn the_last_people_group_going_archives_the_agent_group() {
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(agents_hub().await, surface.clone(), ScriptedBrain::new()).await;
    surface.set_caps(Caps::ALL);
    rig.append(mention("pm", "/unbind", "m_h1")).await;
    rig.settle().await;
    assert_eq!(
        surface.renames(),
        vec![(AGENTS.to_owned(), "[已归档] refund-v2".to_owned())]
    );
    assert_eq!(
        agents_bindings(&rig),
        vec![
            (BindingAction::Bind, AGENTS.to_owned()),
            (BindingAction::Unbind, AGENTS.to_owned()),
        ]
    );
    // Archived: agent traffic has nowhere to go.
    let before = surface.posts().len();
    rig.append(question(agent("alice"), agent("bob"), "还有人吗？"))
        .await;
    rig.settle().await;
    assert_eq!(surface.posts().len(), before);
}

#[tokio::test]
async fn a_person_is_invited_once_by_account_however_many_of_their_agents_show_up() {
    let hub = agents_hub().await;
    hub.seat(&path("codex"), "alice");
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;
    surface.set_caps(Caps::ALL);
    rig.append(question(agent("alice"), agent("bob"), "在吗？"))
        .await;
    rig.append(NewEvent::new(
        refund(),
        Kind::Note,
        agent_named("alice", "codex"),
        "我也在",
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        surface.invites(),
        vec![(AGENTS.to_owned(), vec!["u_alice".to_owned()])],
        "alice's account on the platform, once"
    );
}

#[tokio::test]
async fn naming_an_agent_invites_its_person() {
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(agents_hub().await, surface.clone(), ScriptedBrain::new()).await;
    surface.set_caps(Caps::ALL);
    rig.append(mention("pm", "/invite claude", "m_h1")).await;
    rig.settle().await;
    assert_eq!(
        surface.invites(),
        vec![(AGENTS.to_owned(), vec!["u_alice".to_owned()])]
    );
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("已邀请 alice 进 Agent 群。")
    );
}

#[tokio::test]
async fn people_the_chat_mentions_are_invited_and_a_guest_is_not() {
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(agents_hub().await, surface.clone(), ScriptedBrain::new()).await;
    surface.set_caps(Caps::ALL);
    rig.append(mention("pm", "/invite @Bob @Stranger", "m_h1").with_meta(
        meta::MENTIONS,
        json!([
            {"address": "senator::bob", "name": "Bob"},
            {"address": guest(STRANGER).to_string(), "name": "Stranger"},
        ]),
    ))
    .await;
    rig.settle().await;
    // The guest's name as typed is nobody on the roster: nothing is
    // invited, and the Speaker says whom it knows.
    let texts = rig.surface.texts();
    assert!(
        surface.invites().is_empty(),
        "an unknown name stops the invite: {texts:?}"
    );
    assert!(
        texts
            .last()
            .is_some_and(|t| t.starts_with("我不认识 Stranger")),
        "{texts:?}"
    );

    // Mentioned only: the gateway's record is what counts.
    rig.append(mention("pm", "/invite", "m_h2").with_meta(
        meta::MENTIONS,
        json!([
            {"address": "senator::bob", "name": "Bob"},
            {"address": guest(STRANGER).to_string(), "name": "Stranger"},
        ]),
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        surface.invites(),
        vec![(AGENTS.to_owned(), vec!["u_bob".to_owned()])]
    );
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("已邀请 bob 进 Agent 群。")
    );
}

#[tokio::test]
async fn a_guest_may_not_invite_set_the_output_level_or_adopt() {
    let hub = agents_hub().await;
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;
    surface.set_caps(Caps::ALL);
    for (i, text) in ["/invite alice", "/output all", "/agents"]
        .into_iter()
        .enumerate()
    {
        rig.append(from_guest(mention("pm", text, &format!("m_g{i}"))))
            .await;
        rig.settle().await;
        assert_eq!(
            rig.surface.texts().last(),
            Some(&crate::render::guest_refused(Locale::Zh)),
            "{text}"
        );
    }
    assert!(surface.invites().is_empty());
    assert!(
        !rig.hub
            .events(&refund())
            .iter()
            .any(|e| e.meta.contains_key(meta::OUTPUT_LEVEL))
    );
    assert_eq!(
        agents_bindings(&rig),
        vec![(BindingAction::Bind, AGENTS.to_owned())]
    );
}

#[tokio::test]
async fn the_hint_names_the_bot_as_configured() {
    let mut config = config_in(Locale::En);
    config.bot_name = "Helper".into();
    let rig = Rig::start_config(
        MemoryHub::new(),
        FakeSurface::new(),
        ScriptedBrain::new(),
        config,
    )
    .await;
    rig.mention_unbound(unbound("pm", "/bind refund-v2", "m_u1"))
        .await;
    assert_eq!(
        rig.surface.texts(),
        vec![
            "Bound to committee refund-v2.",
            "There is no Agent group yet: create a group, add @Helper to it, and say \"/agents refund-v2\" there.",
        ]
    );
    rig.stop().await;
}

#[tokio::test]
async fn in_english_the_agent_group_is_created_and_archived() {
    let hub = MemoryHub::new();
    hub.connect(&refund(), &agent_path("alice"));
    let surface = FakeSurface::with(Caps {
        members: false,
        ..Caps::ALL
    });
    let rig = Rig::start_config(
        hub,
        surface.clone(),
        ScriptedBrain::new(),
        config_in(Locale::En),
    )
    .await;
    rig.mention_unbound(unbound("pm", "/bind refund-v2", "m_u1"))
        .await;
    let (group, created) = surface.groups()[0].clone();
    assert_eq!(group.name, "[Agents] refund-v2");
    assert_eq!(group.invite, vec!["u_pm", "u_alice"]);
    let cards = surface.cards();
    assert_eq!(cards[0].first.title, "Waiting on people");
    assert_eq!(cards[0].first.markdown, "Nothing is waiting on a person.");
    assert!(cards[1].first.title.starts_with("Today · "));
    let texts = surface.texts();
    assert_eq!(
        texts,
        vec![
            "Bound to committee refund-v2.".to_owned(),
            "Created [Agents] refund-v2, but could not add [senator::pm], [senator::alice] (the chat app may lack permission to add group members); please invite them by hand.".to_owned(),
        ]
    );

    // The people group goes: the Agent group is archived.
    rig.append(mention_in(&refund(), "random", "pm", "/unbind", "m_u2"))
        .await;
    rig.settle().await;
    assert_eq!(
        surface.renames(),
        vec![(created.clone(), "[Archived] refund-v2".to_owned())]
    );
    assert_eq!(
        agents_bindings(&rig),
        vec![
            (BindingAction::Bind, created.clone()),
            (BindingAction::Unbind, created),
        ]
    );
    assert_eq!(
        surface.texts().last().map(String::as_str),
        Some("This group is no longer bound to committee refund-v2.")
    );
    for text in surface.texts() {
        assert_senate_vocabulary(&text);
    }
    for card in surface.cards() {
        assert_senate_vocabulary(&card.now.title);
        assert_senate_vocabulary(&card.now.markdown);
    }
    rig.stop().await;
}

// ----- task cards ------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_task_card_is_updated_in_place_debounced_and_ends_with_the_answer() {
    let hub = agents_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start_with(hub, FakeSurface::with(Caps::ADOPTED), ScriptedBrain::new()).await;
    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "网关幂等用哪个 header？",
        ))
        .await;
    rig.settle().await;
    let card = rig.surface.card("网关幂等用哪个 header？").expect("card");
    assert_eq!(card.target, root_in(AGENTS));
    assert!(
        card.first.markdown.contains("**分流** 交给 agent"),
        "{}",
        card.first.markdown
    );
    let posts = rig.surface.posts().len();

    // Two statuses within the debounce: one update, showing the second.
    rig.append(status(
        agent("bob"),
        asked.seq,
        &progress("查幂等 header", &[("已读日志", true)]),
    ))
    .await;
    rig.append(status(
        agent("bob"),
        asked.seq,
        &progress("查幂等 header", &[("已读日志", true), ("起草回答", false)]),
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface
            .card("网关幂等用哪个 header？")
            .expect("card")
            .updates,
        0
    );
    after_debounce(&rig).await;
    let card = rig.surface.card("查幂等 header").expect("card");
    assert_eq!(card.updates, 1);
    assert_eq!(card.now.steps.len(), 2);
    assert_eq!(card.now.state, StatusState::Processing);
    assert_eq!(rig.surface.posts().len(), posts, "never a new message");

    rig.append(answer(
        agent("bob"),
        asked.seq,
        agent("alice"),
        "Idempotency-Key",
    ))
    .await;
    after_debounce(&rig).await;
    let card = rig.surface.card("查幂等 header").expect("card");
    assert_eq!(card.updates, 2);
    assert_eq!(card.now.state, StatusState::Active);
    assert!(
        card.now.markdown.contains("**最新回答** Idempotency-Key"),
        "{}",
        card.now.markdown
    );
}

#[tokio::test(start_paused = true)]
async fn output_levels_shape_the_agent_group() {
    let hub = agents_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start_with(hub, FakeSurface::with(Caps::ADOPTED), ScriptedBrain::new()).await;

    rig.append(mention("pm", "/output answers", "m_h1")).await;
    rig.settle().await;
    let note = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.meta.contains_key(meta::OUTPUT_LEVEL))
        .expect("the level note");
    assert_eq!(note.from, Address::Speaker);
    assert_eq!(note.meta.get(meta::OUTPUT_LEVEL), Some(&json!("answers")));
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("好的，Agent 群以后只回答案。")
    );
    assert_eq!(
        rig.hub
            .tasks(&refund())
            .await
            .expect("board")
            .output_level(),
        OutputLevel::Answers
    );

    // Answers only: a status leaves the card alone; the answer updates it.
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "哪个 header？"))
        .await;
    rig.append(status(
        agent("bob"),
        asked.seq,
        &progress("查 header", &[("读代码", false)]),
    ))
    .await;
    after_debounce(&rig).await;
    assert_eq!(rig.surface.card("哪个 header？").expect("card").updates, 0);
    rig.append(answer(
        agent("bob"),
        asked.seq,
        agent("alice"),
        "Idempotency-Key",
    ))
    .await;
    after_debounce(&rig).await;
    assert_eq!(rig.surface.card("查 header").expect("card").updates, 1);

    // Everything: each status is also a thread reply.
    rig.brain.interprets(Ok(Intent::SetOutputLevel {
        level: OutputLevel::All,
    }));
    rig.append(mention("pm", "agent 群全部往返", "m_h2")).await;
    rig.settle().await;
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "第二个问题"))
        .await;
    rig.append(status(
        agent("bob"),
        asked.seq,
        &progress("第二个", &[("读代码", true)]),
    ))
    .await;
    after_debounce(&rig).await;
    let card = rig.surface.card("第二个").expect("card");
    assert_eq!(card.updates, 1);
    let reply = rig.surface.posts().pop().expect("status reply");
    assert_eq!(
        reply.target,
        PostTarget::Reply {
            message_ref: card.message_ref,
            in_thread: true
        }
    );
    assert_eq!(
        reply.markdown,
        "[senator::copilot] 处理中 · 第二个 · ✓ 读代码"
    );
}

#[tokio::test(start_paused = true)]
async fn a_card_update_cut_short_by_a_restart_still_lands_once() {
    let hub = agents_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let surface = FakeSurface::with(Caps::ADOPTED);
    let brain = ScriptedBrain::new();
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "哪个 header？"))
        .await;
    rig.append(status(
        agent("bob"),
        asked.seq,
        &progress("查 header", &[("读代码", true)]),
    ))
    .await;
    rig.settle().await;
    rig.stop().await; // before the debounce ran out
    assert_eq!(surface.card("哪个 header？").expect("card").updates, 0);

    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    after_debounce(&rig).await;
    assert_eq!(surface.card("查 header").expect("card").updates, 1);
    rig.stop().await;

    // A second restart finds the revision recorded and sends nothing.
    let rig = Rig::start_with(hub, surface.clone(), brain).await;
    after_debounce(&rig).await;
    assert_eq!(surface.card("查 header").expect("card").updates, 1);
    assert_eq!(
        surface.cards().len(),
        3,
        "two pinned cards and the task card"
    );
}

#[tokio::test]
async fn without_cards_the_task_is_a_plain_root_and_there_are_no_pinned_cards() {
    let hub = agents_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "哪个 header？"))
        .await;
    rig.settle().await;
    assert!(rig.surface.cards().is_empty());
    assert_eq!(
        rig.surface.texts(),
        vec![format!(
            "[senator::claude] → [senator::copilot] 哪个 header？（seq {}）",
            asked.seq
        )]
    );
    assert_eq!(rig.surface.posts()[0].target, root_in(AGENTS));
}

// ----- the pinned cards -----------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_pinned_cards_follow_whats_waiting_and_today() {
    let hub = agents_hub().await;
    let surface = FakeSurface::with(Caps::ADOPTED);
    let brain = ScriptedBrain::new();
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    assert_eq!(surface.pins().len(), 2);
    let waiting = || surface.card("等人处理").expect("waiting card");
    let today = || {
        surface
            .cards()
            .into_iter()
            .find(|c| c.first.title.starts_with("今天的摘要"))
            .expect("today card")
    };
    assert_eq!(waiting().now.markdown, "没有等人处理的话题。");

    let asked = rig
        .append(
            NewEvent::new(
                refund(),
                Kind::Request,
                agent("bob"),
                "把网关 header 改成 Idempotency-Key",
            )
            .to(agent("alice")),
        )
        .await;
    after_debounce(&rig).await;
    let listed = waiting().now.markdown;
    assert!(
        listed.starts_with(&format!("- seq {} · 把网关 header", asked.seq)),
        "{listed}"
    );
    assert!(listed.contains("等 [senator::alice]"), "{listed}");
    let summary = today().now.markdown;
    assert!(
        summary.contains(&format!("seq {} · 把网关 header", asked.seq)),
        "{summary}"
    );
    assert!(summary.contains("[senator::copilot] · seq"), "{summary}");

    // A decision today shows up; the approval drops the thread from waiting.
    rig.append(NewEvent::new(
        refund(),
        Kind::Decision,
        user("pm"),
        "幂等键统一用 Idempotency-Key",
    ))
    .await;
    let card = surface
        .card("把网关 header 改成 Idempotency-Key")
        .expect("task card");
    rig.append(in_thread(
        "alice",
        "交给我的 agent",
        "m_a1",
        asked.seq,
        None,
        &card.message_ref,
    ))
    .await;
    after_debounce(&rig).await;
    assert_eq!(waiting().now.markdown, "没有等人处理的话题。");
    assert!(
        today()
            .now
            .markdown
            .contains("幂等键统一用 Idempotency-Key")
    );
    rig.stop().await;

    // Their refs survive a restart: refreshed, never posted again.
    let updates = waiting().updates;
    let rig = Rig::start_with(hub, surface.clone(), brain).await;
    after_debounce(&rig).await;
    assert_eq!(surface.cards().len(), 3);
    assert_eq!(surface.pins().len(), 2);
    assert!(waiting().updates <= updates + 1, "at most one refresh");
    rig.stop().await;
}

// ----- restart ---------------------------------------------------------------------

#[tokio::test]
async fn a_parked_request_replayed_after_a_crash_changes_nothing() {
    let hub = agents_hub().await;
    let surface = FakeSurface::with(Caps::ADOPTED);
    let brain = ScriptedBrain::new();
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    let asked = rig
        .append(
            NewEvent::new(refund(), Kind::Request, agent("bob"), "改 header").to(agent("alice")),
        )
        .await;
    rig.settle().await;
    let card = surface.card("改 header").expect("task card");
    rig.append(in_thread(
        "bob",
        "我先看看",
        "m_b1",
        asked.seq,
        None,
        &card.message_ref,
    ))
    .await;
    rig.settle().await;
    let events = hub.events(&refund());
    let posts = surface.posts();
    let cards = surface.cards();
    assert_eq!(posts.len(), 1, "the mention: {posts:#?}");
    rig.stop().await;

    hub.set_cursor(&refund(), crate::CURSOR, Seq(2))
        .await
        .expect("rewind");
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    assert_eq!(hub.events(&refund()), events, "no event twice");
    assert_eq!(surface.posts(), posts, "no post twice");
    assert_eq!(surface.cards().len(), cards.len(), "no card twice");
    assert!(brain.triage_inputs().is_empty());
    rig.stop().await;
}
