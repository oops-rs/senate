//! The Speaker's main flows: relays and their answers, bindings, the fixed
//! syntax, answers on an agent's behalf, restarts and outages; which of a
//! person's agents a relay reaches; what a guest may ask; English.

use std::time::Duration;

use senate_core::{
    Account, Address, BindingAction, Event, Hub, Kind, Locale, Mirror, NewEvent, PostTarget,
    Roster, Senior, Seq, SurfaceError, UnboundMention,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::{
    GROUP, Rig, STRANGER, agent, agent_named, agent_path, answer, assert_senate_vocabulary,
    bind_event, committee, config_in, guest, in_thread, mention, name, path, question, refund,
    roster, source, unbound, user,
};
use super::surface::{FakeSurface, PLATFORM};
use crate::render;
use crate::{BrainError, Intent, Pick, RelayKind, ScriptedBrain};

async fn bound_hub() -> std::sync::Arc<MemoryHub> {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub
}

/// What the gateway records for a mention by [`STRANGER`], a guest.
fn guest_mention(text: &str, message_ref: &str) -> NewEvent {
    let mut chat = mention("pm", text, message_ref);
    chat.from = guest(STRANGER);
    chat
}

fn reply_to(message_ref: &str) -> PostTarget {
    PostTarget::Reply {
        message_ref: message_ref.into(),
        in_thread: true,
    }
}

/// The last event in the log, which must be the Speaker's relay.
fn last_relay(rig: &Rig) -> Event {
    let relay = rig.hub.events(&refund()).into_iter().last().expect("relay");
    assert_eq!(relay.from, Address::Speaker, "{relay:#?}");
    assert!(
        matches!(relay.kind, Kind::Note | Kind::Question),
        "{relay:#?}"
    );
    relay
}

/// The usual roster plus carol, who has no agent.
fn roster_with_carol() -> Roster {
    let base = roster();
    let mut seniors = base.seniors().to_vec();
    seniors.push(Senior {
        handle: name("carol"),
        names: vec!["Carol".into()],
        accounts: vec![Account {
            platform: PLATFORM.into(),
            id: "u_carol".into(),
        }],
    });
    Roster::new(seniors, base.seated().to_vec()).expect("roster")
}

/// A bound hub where alice has a second agent, `codex`, seated.
async fn hub_with_codex() -> std::sync::Arc<MemoryHub> {
    let hub = bound_hub().await;
    hub.seat(&path("codex"), "alice");
    hub
}

#[tokio::test]
async fn criterion_1_a_relay_is_acknowledged_and_the_answer_lands_in_the_same_thread() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: "退款上限是 30 天".into(),
    }));

    let chat = rig
        .append(mention(
            "pm",
            "告诉 alice 的 agent，退款上限是 30 天",
            "m_h1",
        ))
        .await;
    rig.settle().await;

    // The relayed question.
    let events = rig.hub.events(&refund());
    assert_eq!(events.len(), 3, "{events:#?}");
    let relay = &events[2];
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(relay.from, Address::Speaker);
    assert_eq!(relay.on_behalf_of, Some(user("pm")));
    assert_eq!(relay.to, vec![agent("alice")]);
    assert_eq!(relay.thread, None);
    assert_eq!(relay.source, Some(source()));
    assert_eq!(relay.body, "退款上限是 30 天");
    assert_eq!(relay.meta.get("relayed_from"), Some(&json!(chat.seq.0)));

    // The Brain saw the utterance in context.
    let utterance = &rig.brain.utterances()[0];
    assert_eq!(utterance.speaker, user("pm"));
    assert_eq!(utterance.committee, Some(refund()));
    assert_eq!(utterance.group.group_id, GROUP);
    assert_eq!(utterance.context.len(), 1, "the binding event");
    assert_eq!(utterance.roster.seniors().len(), 3);

    // The ack, in the human's thread, keyed to the relay's seq.
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].target, reply_to("m_h1"));
    assert_eq!(
        posts[0].markdown,
        format!("已转给 [senator::claude]（seq {}）", relay.seq)
    );
    assert_eq!(
        posts[0].operation_id,
        format!(
            "senate:{}:refund-v2:{}:{GROUP}",
            rig.hub.epoch_now(),
            relay.seq
        )
    );
    let copies = rig
        .hub
        .mirrors_of_seq(&refund(), relay.seq, PLATFORM)
        .await
        .expect("read");
    assert_eq!(copies.len(), 1, "{copies:?}");
    let mirror = &copies[0];
    assert_eq!(mirror.target, GROUP);
    assert_eq!(mirror.message_ref, "m_h1", "replies there map to the relay");
    assert_eq!(mirror.thread_ref, posts[0].thread_ref);

    // Alice's agent answers; it appears as a reply in the same thread.
    let reply = rig
        .append(answer(
            agent("alice"),
            relay.seq,
            user("pm"),
            "收到，已按 30 天处理",
        ))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1].target, reply_to("m_h1"));
    assert_eq!(posts[1].markdown, "[senator::claude] 收到，已按 30 天处理");
    assert_eq!(posts[1].thread_ref, posts[0].thread_ref);
    assert_eq!(rig.cursor(&refund()).await, reply.seq);
    assert!(
        rig.brain.questions().is_empty(),
        "no answer on her behalf: she answered"
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

#[tokio::test]
async fn criterion_2_agent_to_agent_is_one_thread_and_human_corrections_are_not_echoed() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;

    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "网关的幂等键用哪个 header？",
        ))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].target,
        PostTarget::NewRoot {
            group_id: GROUP.into()
        }
    );
    assert_eq!(
        posts[0].markdown,
        format!(
            "[senator::claude] → [senator::copilot] 网关的幂等键用哪个 header？（seq {}）",
            asked.seq
        )
    );
    let root_ref = posts[0].message_ref.clone();

    rig.append(answer(
        agent("bob"),
        asked.seq,
        agent("alice"),
        "Idempotency-Key",
    ))
    .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1].target, reply_to(&root_ref));
    assert_eq!(posts[1].markdown, "[senator::copilot] Idempotency-Key");

    // The gateway records a human reply in that thread as a chat to both
    // agents; it is already in the chat, so the Speaker says nothing.
    let correction = rig
        .append(
            NewEvent::new(refund(), Kind::Chat, user("pm"), "不对，用 X-Request-Id")
                .in_thread(asked.seq)
                .with_to(vec![agent("alice"), agent("bob")])
                .with_source(source())
                .with_mirror(Mirror {
                    platform: PLATFORM.into(),
                    message_ref: "m_h9".into(),
                    thread_ref: posts[0].thread_ref.clone(),
                })
                .with_meta("addressing", json!("ambient"))
                .with_meta("thread_root_message_ref", json!(root_ref)),
        )
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.posts().len(),
        2,
        "a chat from the platform is never mirrored"
    );
    assert!(rig.brain.utterances().is_empty());
    assert_eq!(rig.cursor(&refund()).await, correction.seq);
}

#[tokio::test]
async fn a_first_binding_from_an_unbound_group_is_restated_and_confirmed() {
    let rig = Rig::start(MemoryHub::new()).await;
    rig.brain.interprets(Ok(Intent::Bind {
        committee: "refund-v2".into(),
    }));
    let ask = unbound("pm", "把这个群绑定到 refund-v2", "m_m1");
    rig.mention_unbound(ask.clone()).await;

    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].target, reply_to("m_m1"));
    assert_eq!(
        posts[0].markdown,
        "我理解为：把本群绑定到委员会 refund-v2。回复「对」确认"
    );
    assert!(rig.hub.committees().await.expect("committees").is_empty());
    let thread = posts[0].thread_ref.clone();
    assert!(thread.is_some());

    // "对" typed in that thread binds, however the gateway classed it.
    rig.mention_unbound(UnboundMention {
        message_ref: "m_m2".into(),
        thread_ref: thread.clone(),
        thread_root_message_ref: Some("m_m1".into()),
        addressing: "ambient".into(),
        text: "对".into(),
        ..ask.clone()
    })
    .await;
    let events = rig.hub.events(&refund());
    assert_eq!(events.len(), 1);
    let binding = events[0].binding().expect("a binding payload");
    assert_eq!(binding.action, BindingAction::Bind);
    assert_eq!(binding.group_id, "random");
    assert_eq!(binding.group_name.as_deref(), Some("杂谈"));
    assert_eq!(events[0].from, Address::Speaker);
    assert_eq!(events[0].on_behalf_of, Some(user("pm")));
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 3);
    assert_eq!(posts[1].target, reply_to("m_m2"));
    assert_eq!(posts[1].markdown, "已绑定到委员会 refund-v2。");
    // The surface cannot create the Agent group: one hint, in the group.
    assert_eq!(
        posts[2].target,
        PostTarget::NewRoot {
            group_id: "random".into()
        }
    );
    assert!(
        posts[2].markdown.starts_with("还没有 Agent 群"),
        "{posts:#?}"
    );
    assert_eq!(
        rig.brain.utterances().len(),
        1,
        "the confirmation needs no model"
    );

    // Anything but a confirmation word cancels.
    rig.brain.interprets(Ok(Intent::Bind {
        committee: "checkout".into(),
    }));
    rig.mention_unbound(UnboundMention {
        group_id: "other".into(),
        message_ref: "m_o1".into(),
        ..ask.clone()
    })
    .await;
    let thread = rig.surface.posts()[3].thread_ref.clone();
    rig.mention_unbound(UnboundMention {
        group_id: "other".into(),
        message_ref: "m_o2".into(),
        thread_ref: thread,
        text: "等等，先不要".into(),
        ..ask
    })
    .await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("好的，已取消。")
    );
    assert!(rig.hub.events(&committee("checkout")).is_empty());
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

#[tokio::test]
async fn an_unbind_in_a_bound_group_waits_for_a_reply_in_the_thread() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain.interprets(Ok(Intent::Unbind));
    let ask = rig
        .append(mention("pm", "这个群不用了，解绑吧", "m_h1"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(
        posts[0].markdown,
        "我理解为：解除本群与委员会 refund-v2 的绑定。回复「对」确认"
    );
    assert_eq!(rig.hub.events(&refund()).len(), 2, "nothing unbound yet");

    // The gateway records "对" as a reply to the restatement (no @ needed).
    rig.append(in_thread(
        "pm",
        "对",
        "m_h2",
        ask.seq,
        posts[0].thread_ref.clone(),
        &posts[0].message_ref,
    ))
    .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let unbind = events.last().expect("an event").binding().expect("binding");
    assert_eq!(unbind.action, BindingAction::Unbind);
    assert_eq!(unbind.group_id, GROUP);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("已解除本群与委员会 refund-v2 的绑定。")
    );
}

/// The yes-words are the same in both languages: `yes`/`ok` confirm in a
/// Chinese committee, 「对」 in an English one.
#[tokio::test]
async fn yes_words_confirm_in_either_locale() {
    for locale in Locale::ALL {
        for yes in ["对", "yes", "ok", "OK!"] {
            let hub = bound_hub().await;
            let rig = Rig::start_config(
                hub,
                FakeSurface::new(),
                ScriptedBrain::new(),
                config_in(locale),
            )
            .await;
            rig.brain.interprets(Ok(Intent::Unbind));
            let ask = rig.append(mention("pm", "unbind this group", "m_h1")).await;
            rig.settle().await;
            let restated = rig.surface.posts()[0].clone();
            assert_eq!(
                restated.markdown,
                render::confirm_action(locale, &render::describe_unbind(locale, &refund()))
            );
            rig.append(in_thread(
                "pm",
                yes,
                "m_h2",
                ask.seq,
                restated.thread_ref.clone(),
                &restated.message_ref,
            ))
            .await;
            rig.settle().await;
            let events = rig.hub.events(&refund());
            assert_eq!(
                events.last().and_then(Event::binding).map(|b| b.action),
                Some(BindingAction::Unbind),
                "{locale:?} {yes}"
            );
            assert_eq!(
                rig.surface.texts().last().cloned(),
                Some(render::unbound(locale, &refund())),
                "{locale:?} {yes}"
            );
            rig.stop().await;
        }
    }
}

#[tokio::test]
async fn the_fixed_syntax_works_without_the_brain_and_covers_its_failures() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;

    // The model is down: the Speaker says so and points at the fixed syntax.
    rig.brain
        .interprets(Err(BrainError::Unavailable("provider 503".into())));
    rig.append(mention("pm", "帮我看看这个", "m_h1")).await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert!(said[0].contains("/tell <name> <内容>"), "{said:?}");

    // /tell relays without consulting the Brain.
    rig.append(mention("pm", "/tell alice 记得更新退款文档", "m_h2"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Note);
    assert_eq!(relay.to, vec![agent("alice")]);
    assert_eq!(relay.body, "记得更新退款文档");
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("已转给 [senator::claude]（seq {}）", relay.seq).as_str())
    );

    // /status is deterministic.
    rig.append(mention("pm", "/status", "m_h3")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(
            "本群绑定在委员会 refund-v2。\n在线 agent：[senator::claude]\n离线 agent：[senator::aider]、[senator::copilot]"
        )
    );

    // /bind moves the group directly, no confirmation.
    rig.append(mention("pm", "/bind checkout", "m_h4")).await;
    rig.settle().await;
    let checkout = rig.hub.events(&committee("checkout"));
    assert_eq!(checkout.len(), 1);
    assert_eq!(
        checkout[0].binding().map(|b| b.action),
        Some(BindingAction::Bind)
    );
    let last = rig
        .hub
        .events(&refund())
        .into_iter()
        .last()
        .expect("unbind");
    assert_eq!(
        last.binding().map(|b| b.action),
        Some(BindingAction::Unbind)
    );
    let texts = rig.surface.texts();
    assert_eq!(
        texts.iter().rev().nth(1).map(String::as_str),
        Some("已绑定到委员会 checkout。")
    );
    assert!(
        texts
            .last()
            .is_some_and(|t| t.starts_with("还没有 Agent 群")),
        "{texts:?}"
    );

    // Unknown names are refused with the roster.
    rig.append(mention("pm", "/tell carol 你好", "m_h5")).await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert!(
        said.last().is_some_and(|t| t.contains("我不认识 carol")),
        "{said:?}"
    );
    assert_eq!(
        rig.brain.utterances().len(),
        1,
        "only the free-text mention"
    );
}

#[tokio::test]
async fn a_question_to_an_offline_agent_is_answered_on_its_behalf_at_once() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain.answers(Ok("退款上限是 30 天（seq 1）".into()));

    let asked = rig
        .append(question(agent("bob"), agent("alice"), "退款上限是多少天？"))
        .await;
    rig.settle().await;

    let events = rig.hub.events(&refund());
    let fallback = events.last().expect("the answer on her behalf");
    assert_eq!(fallback.kind, Kind::Answer);
    assert_eq!(fallback.from, Address::Speaker);
    assert_eq!(fallback.on_behalf_of, Some(agent("alice")));
    assert_eq!(fallback.thread, Some(asked.seq));
    assert_eq!(fallback.to, vec![agent("bob")]);
    assert_eq!(fallback.body, "退款上限是 30 天（seq 1）");
    let question = &rig.brain.questions()[0];
    assert_eq!(question.agent, agent_path("alice"));
    assert_eq!(question.event.seq, asked.seq);

    let posts = rig.surface.posts();
    assert_eq!(
        posts.len(),
        2,
        "the question, then the answer on her behalf"
    );
    assert_eq!(posts[1].target, reply_to(&posts[0].message_ref));
    assert_eq!(
        posts[1].markdown,
        "[speaker::claude] 退款上限是 30 天（seq 1）"
    );

    // Alice comes back and answers herself: that is mirrored too.
    rig.append(answer(agent("alice"), asked.seq, agent("bob"), "对，30 天"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[senator::claude] 对，30 天")
    );
    assert_eq!(rig.brain.questions().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_question_is_answered_on_behalf_after_the_window() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    rig.brain
        .answers(Ok("记录里没有结论（seq 1 只有绑定）。".into()));

    // Answered in time: nothing on her behalf, however long we wait.
    let early = rig
        .append(question(agent("bob"), agent("alice"), "先问一个"))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    rig.append(answer(agent("alice"), early.seq, agent("bob"), "马上答"))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(120)).await;
    rig.settle().await;
    assert!(rig.brain.questions().is_empty(), "the timer must not fire");

    // Not answered: the window closes at 60 s.
    let asked = rig
        .append(question(agent("bob"), agent("alice"), "退款上限是多少天？"))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(59)).await;
    rig.settle().await;
    assert!(rig.brain.questions().is_empty(), "too early");
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.settle().await;
    let questions = rig.brain.questions();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].event.seq, asked.seq);
    let fallback = rig
        .hub
        .events(&refund())
        .into_iter()
        .last()
        .expect("the answer on her behalf");
    assert_eq!(fallback.on_behalf_of, Some(agent("alice")));
    assert_eq!(fallback.thread, Some(asked.seq));
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[speaker::claude] 记录里没有结论（seq 1 只有绑定）。")
    );

    // A late real answer is still mirrored.
    rig.append(answer(agent("alice"), asked.seq, agent("bob"), "30 天"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[senator::claude] 30 天")
    );
}

/// The Brain cannot answer: the fallback says where the question is, and
/// does not call a connected agent offline.
#[tokio::test(start_paused = true)]
async fn the_fallback_says_whether_the_agent_has_the_question() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    rig.brain
        .answers(Err(BrainError::Unavailable("provider 400".into())));
    rig.append(question(agent("bob"), agent("alice"), "退款上限是多少天？"))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(61)).await;
    rig.settle().await;
    let text = rig.surface.texts().last().cloned().unwrap_or_default();
    assert!(text.contains("已经收到这个问题"), "{text}");
    assert!(!text.contains("不在线"), "{text}");

    rig.hub.disconnect(&refund(), &agent_path("alice"));
    rig.brain
        .answers(Err(BrainError::Unavailable("provider 400".into())));
    rig.append(question(
        agent("bob"),
        agent("alice"),
        "幂等键用哪个 header？",
    ))
    .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(61)).await;
    rig.settle().await;
    let text = rig.surface.texts().last().cloned().unwrap_or_default();
    assert!(text.contains("现在不在线"), "{text}");
}

/// Who wrote a copy is its byline, never text in the stored body: an
/// agent's answer is headed `[senator::copilot]`, the Speaker's answer
/// for it `[speaker::copilot]`.
#[tokio::test]
async fn mirrors_are_headed_by_the_byline_and_bodies_carry_no_labels() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain.answers(Ok("用 Idempotency-Key（seq 1）".into()));

    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "网关的幂等键用哪个 header？",
        ))
        .await;
    rig.settle().await;
    rig.append(answer(
        agent("bob"),
        asked.seq,
        agent("alice"),
        "确认是 Idempotency-Key",
    ))
    .await;
    rig.settle().await;

    let texts = rig.surface.texts();
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert_eq!(texts[1], "[speaker::copilot] 用 Idempotency-Key（seq 1）");
    assert_eq!(texts[2], "[senator::copilot] 确认是 Idempotency-Key");
    for text in &texts {
        assert_senate_vocabulary(text);
    }

    let events = rig.hub.events(&refund());
    let on_behalf = events
        .iter()
        .find(|e| e.kind == Kind::Answer && e.from == Address::Speaker)
        .expect("the Speaker's answer");
    assert_eq!(on_behalf.on_behalf_of, Some(agent("bob")));
    assert_eq!(
        on_behalf.byline(Locale::Zh).as_deref(),
        Some("[speaker::copilot]")
    );
    assert_eq!(on_behalf.body, "用 Idempotency-Key（seq 1）");
    for event in events.iter().filter(|e| e.from == Address::Speaker) {
        assert!(
            !event.body.contains("[speaker"),
            "a label in a stored body: {event:#?}"
        );
    }
}

#[tokio::test]
async fn a_restart_replays_from_the_cursor_without_posting_twice() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    hub.connect(&refund(), &agent_path("bob"));
    let surface = FakeSurface::new();
    let brain = ScriptedBrain::new();
    brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Note,
        body: "退款上限是 30 天".into(),
    }));
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    rig.append(mention(
        "pm",
        "告诉 alice 的 agent，退款上限是 30 天",
        "m_h1",
    ))
    .await;
    rig.settle().await;
    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "幂等键用哪个 header？",
        ))
        .await;
    let replied = rig
        .append(answer(
            agent("bob"),
            asked.seq,
            agent("alice"),
            "Idempotency-Key",
        ))
        .await;
    rig.settle().await;
    let before = surface.posts();
    assert_eq!(before.len(), 3, "ack, root, reply");
    let events_before = hub.events(&refund()).len();
    rig.stop().await;

    // Crash before any cursor was persisted: everything is replayed. The
    // chat is re-interpreted (differently, even), but the relay it causes has
    // the same id, and every mirror is already recorded.
    hub.set_cursor(&refund(), crate::CURSOR, Seq(1))
        .await
        .expect("rewind");
    brain.interprets(Ok(Intent::Relay {
        targets: vec!["bob".into()],
        kind: RelayKind::Question,
        body: "another reading".into(),
    }));
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    assert_eq!(surface.posts(), before, "no post twice");
    assert_eq!(
        surface.duplicates(),
        0,
        "mirror refs short-circuit the posts"
    );
    assert_eq!(hub.events(&refund()).len(), events_before, "no event twice");
    assert_eq!(rig.cursor(&refund()).await, Seq(events_before as u64));
    rig.stop().await;

    // Crash between a post landing and its mirror being recorded: the
    // replay re-sends the same operation id, which the platform dedupes.
    let last = replied.seq;
    hub.forget_mirror(&refund(), last);
    hub.set_cursor(&refund(), crate::CURSOR, Seq(last.0 - 1))
        .await
        .expect("rewind");
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain).await;
    assert_eq!(surface.posts(), before);
    assert_eq!(surface.duplicates(), 1);
    assert!(
        !hub.mirrors_of_seq(&refund(), last, PLATFORM)
            .await
            .expect("read")
            .is_empty(),
        "the mirror is recorded again"
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_lagging_subscription_recovers_from_the_log_in_order() {
    let hub = MemoryHub::with_capacity(4);
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;

    // Ten appends without yielding overrun the 4-slot broadcast buffer.
    for n in 1..=10 {
        rig.append(question(agent("alice"), agent("bob"), &format!("问题 {n}")))
            .await;
    }
    // No Settle here: recovery must come from the lag/gap path alone.
    for _ in 0..200 {
        if rig.surface.posts().len() == 10 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let texts = rig.surface.texts();
    assert_eq!(texts.len(), 10, "{texts:?}");
    for (n, text) in texts.iter().enumerate() {
        assert!(
            text.starts_with(&format!(
                "[senator::claude] → [senator::copilot] 问题 {}（",
                n + 1
            )),
            "{text}"
        );
    }
    // Each question and its triage note.
    assert_eq!(rig.cursor(&refund()).await, Seq(21));
}

#[tokio::test]
async fn a_platform_outage_pauses_the_committee_and_retries_from_the_cursor() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let surface = FakeSurface::new();
    let rig = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;

    surface.fail_next(SurfaceError::Unavailable("gateway restarting".into()));
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "还在吗？"))
        .await;
    rig.settle().await;
    assert!(surface.posts().is_empty());
    assert_eq!(rig.cursor(&refund()).await, Seq(1), "not passed");

    // The retry fires after a short pause and posts it.
    for _ in 0..400 {
        if !surface.posts().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    rig.settle().await;
    assert_eq!(surface.posts().len(), 1);
    assert_eq!(
        rig.cursor(&refund()).await,
        asked.seq.next(),
        "the question, then its triage note"
    );
}

#[tokio::test]
async fn bindings_not_from_the_speaker_do_not_move_the_mirror_group() {
    let hub = bound_hub().await;
    let mut forged = bind_event(&refund(), "elsewhere");
    forged.from = user("mallory");
    hub.append(forged).await.expect("append");
    // Make "elsewhere" the first bound group if it were trusted: unbind
    // backend the same forged way.
    let mut forged = bind_event(&refund(), GROUP);
    forged.from = agent("mallory");
    forged.meta.insert(
        "binding".into(),
        json!({"action": "unbind", "platform": PLATFORM, "group_id": GROUP}),
    );
    hub.append(forged).await.expect("append");
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    rig.append(question(agent("alice"), agent("bob"), "去哪个群？"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.posts()[0].target,
        PostTarget::NewRoot {
            group_id: GROUP.into()
        }
    );
}

// ----- which agent a relay reaches ---------------------------------------------

/// A person with no agent gets what was meant for their agent in person,
/// whether by `/tell` or in words.
#[tokio::test]
async fn a_relay_to_a_person_with_no_agent_goes_to_the_person() {
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    let rig = Rig::start(hub).await;

    rig.append(mention("pm", "/tell carol 记得更新退款文档", "m_h1"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Note);
    assert_eq!(relay.to, vec![user("carol")]);
    assert_eq!(relay.on_behalf_of, Some(user("pm")));
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("已转给 [senator::carol]（seq {}）", relay.seq).as_str())
    );

    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["Carol".into()],
        kind: RelayKind::Question,
        body: "退款文档更新了吗？".into(),
    }));
    rig.append(mention("pm", "问一下 Carol 退款文档更新了吗", "m_h2"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(relay.to, vec![user("carol")]);
    assert!(rig.brain.pick_inputs().is_empty(), "nothing to pick from");
}

/// An agent named outright is that agent, by `/tell` or in words.
#[tokio::test]
async fn a_relay_to_an_agent_named_outright_reaches_that_one() {
    let hub = hub_with_codex().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;

    rig.append(mention("pm", "/tell codex 记得更新退款文档", "m_h1"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("已转给 [senator::codex]（seq {}）", relay.seq).as_str())
    );

    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["codex".into()],
        kind: RelayKind::Note,
        body: "退款上限是 30 天".into(),
    }));
    rig.append(mention(
        "pm",
        "告诉 alice 的 codex，退款上限是 30 天",
        "m_h2",
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);
    assert_eq!(relay.body, "退款上限是 30 天");
    assert!(
        rig.brain.pick_inputs().is_empty(),
        "a named agent needs no pick"
    );
}

/// `[senator::*]` is whichever agent on the committee: the only one there
/// gets it, nobody's in particular, and no pick is needed.
#[tokio::test]
async fn whichever_agent_is_the_only_one_on_the_committee() {
    let hub = hub_with_codex().await;
    hub.connect(&refund(), &path("codex"));
    let rig = Rig::start(hub).await;

    rig.append(mention("pm", "/tell [senator::*] 记得更新退款文档", "m_h1"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);
    assert!(rig.brain.pick_inputs().is_empty(), "one candidate, no pick");
}

/// Several agents on the committee: the Speaker picks among all of them,
/// for no person in particular.
#[tokio::test]
async fn whichever_agent_among_several_on_the_committee_is_picked() {
    let hub = hub_with_codex().await;
    hub.connect(&refund(), &path("codex"));
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    rig.brain.picks(Ok(Pick::Agent(agent_path("bob"))));

    rig.append(mention("pm", "/tell [senator::*] 记得更新退款文档", "m_h1"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent("bob")]);
    let picks = rig.brain.pick_inputs();
    assert_eq!(picks.len(), 1, "{picks:#?}");
    assert_eq!(picks[0].owner, None);
    let candidates: Vec<String> = picks[0]
        .candidates
        .iter()
        .map(|c| c.agent.to_string())
        .collect();
    assert_eq!(candidates, ["copilot", "codex"]);
}

/// No agent on the committee, and several people in the Senate: nobody
/// to give it to, so nothing is relayed.
#[tokio::test]
async fn whichever_agent_with_none_on_the_committee_is_nobody_among_several_people() {
    let rig = Rig::start(bound_hub().await).await;
    let before = rig.hub.events(&refund()).len();
    rig.append(mention("pm", "/tell [senator::*] 记得更新退款文档", "m_h1"))
        .await;
    rig.settle().await;
    assert_eq!(rig.hub.events(&refund()).len(), before + 1, "only the chat");
    assert!(
        rig.surface
            .texts()
            .last()
            .is_some_and(|t| t.starts_with("我不认识 [senator::*]")),
        "{:?}",
        rig.surface.texts()
    );
}

/// Unnamed, with exactly one of the person's agents connected: that one.
#[tokio::test]
async fn an_unnamed_relay_reaches_the_only_connected_agent() {
    let hub = hub_with_codex().await;
    hub.connect(&refund(), &path("codex"));
    let rig = Rig::start(hub).await;

    rig.append(mention("pm", "/tell alice 记得更新退款文档", "m_h1"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);

    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["爱丽丝".into()],
        kind: RelayKind::Question,
        body: "退款文档更新了吗？".into(),
    }));
    rig.append(mention(
        "pm",
        "问一下爱丽丝的 agent 退款文档更新了吗",
        "m_h2",
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);

    let picks = rig.brain.pick_inputs();
    assert_eq!(picks.len(), 2, "{picks:#?}");
    assert_eq!(picks[0].owner, Some(name("alice")));
    assert_eq!(picks[0].asker, user("pm"));
    let present: Vec<(String, bool)> = picks[0]
        .candidates
        .iter()
        .map(|c| (c.agent.to_string(), c.present))
        .collect();
    assert_eq!(present, [("claude".into(), false), ("codex".into(), true)]);
}

/// Unnamed, with none of them connected: the Speaker asks in the thread,
/// and the asker's number carries the relay on.
#[tokio::test]
async fn an_unnamed_relay_with_no_agent_connected_asks_which_and_a_number_answers() {
    let rig = Rig::start(hub_with_codex().await).await;

    let ask = rig
        .append(mention("pm", "/tell alice 记得更新退款文档", "m_h1"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.hub.events(&refund()).len(),
        2,
        "nothing relayed while it asks"
    );
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].target, reply_to("m_h1"));
    assert_eq!(
        posts[0].markdown,
        "[senator::alice] 有 2 个 agent，你指的是哪一个？回复编号或名字：\n\
         1. [senator::claude] 不在线\n\
         2. [senator::codex] 不在线"
    );

    rig.append(in_thread(
        "pm",
        "2",
        "m_h2",
        ask.seq,
        posts[0].thread_ref.clone(),
        &posts[0].message_ref,
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Note);
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);
    assert_eq!(relay.on_behalf_of, Some(user("pm")));
    assert_eq!(relay.body, "记得更新退款文档");
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("已转给 [senator::codex]（seq {}）", relay.seq).as_str())
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

/// Both connected: the Speaker asks; a reply that names none is told the
/// choices again, and the agent's name then carries the relay on.
#[tokio::test]
async fn an_unnamed_relay_with_both_connected_asks_and_a_name_answers() {
    let hub = hub_with_codex().await;
    hub.connect(&refund(), &agent_path("alice"));
    hub.connect(&refund(), &path("codex"));
    let rig = Rig::start(hub).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Note,
        body: "退款上限是 30 天".into(),
    }));

    let ask = rig
        .append(mention(
            "pm",
            "告诉 alice 的 agent，退款上限是 30 天",
            "m_h1",
        ))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].markdown,
        "[senator::alice] 有 2 个 agent，你指的是哪一个？回复编号或名字：\n\
         1. [senator::claude] 在线\n\
         2. [senator::codex] 在线"
    );
    let (thread_ref, question_ref) = (posts[0].thread_ref.clone(), posts[0].message_ref.clone());

    // Someone else's reply is not an answer.
    rig.append(in_thread(
        "bob",
        "codex",
        "m_h2",
        ask.seq,
        thread_ref.clone(),
        &question_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(rig.surface.posts().len(), 1, "nothing said to bob");

    rig.append(in_thread(
        "pm",
        "随便哪个",
        "m_h3",
        ask.seq,
        thread_ref.clone(),
        &question_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("没认出是哪一个。请回复编号或名字：[senator::claude]、[senator::codex]")
    );
    assert_eq!(rig.hub.events(&refund()).len(), 4, "still nothing relayed");

    rig.append(in_thread(
        "pm",
        "codex",
        "m_h4",
        ask.seq,
        thread_ref,
        &question_ref,
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Note);
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);
    assert_eq!(relay.body, "退款上限是 30 天");
    assert_eq!(relay.on_behalf_of, Some(user("pm")));
    assert_eq!(rig.brain.utterances().len(), 1, "the answer needs no model");
}

/// The Brain's pick is used as is; a pick of someone who is not a candidate
/// is not trusted, and the Speaker asks.
#[tokio::test]
async fn the_brains_pick_is_honoured_and_a_pick_of_a_stranger_asks() {
    let hub = hub_with_codex().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;

    // The rules would say claude, the only one connected.
    rig.brain.picks(Ok(Pick::Agent(path("codex"))));
    rig.append(mention("pm", "/tell alice 记得更新退款文档", "m_h1"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent_named("alice", "codex")]);

    rig.brain.picks(Ok(Pick::Agent(agent_path("bob"))));
    let before = rig.hub.events(&refund()).len();
    rig.append(mention("pm", "/tell alice 再确认一次退款上限", "m_h2"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.hub.events(&refund()).len(),
        before + 1,
        "only the chat: nothing relayed to bob"
    );
    let said = rig.surface.texts();
    assert!(
        said.last()
            .is_some_and(|t| t.starts_with("[senator::alice] 有 2 个 agent，你指的是哪一个？")),
        "{said:?}"
    );
}

// ----- guests ------------------------------------------------------------------

/// A guest may ask (a question relayed, the status, the brief) and nothing
/// more: no binding, no decision, no note pushed into an agent's session.
#[tokio::test]
async fn a_guest_may_ask_but_not_change_anything() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;

    for (text, message_ref) in [
        ("/bind checkout", "m_g1"),
        ("/decide 上限改成 90 天", "m_g2"),
        ("/tell alice 上限改成 90 天", "m_g3"),
    ] {
        rig.append(guest_mention(text, message_ref)).await;
    }
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 3, "{posts:#?}");
    assert_eq!(posts[0].target, reply_to("m_g1"));
    assert_eq!(posts[0].markdown, render::guest_refused(Locale::Zh));
    assert_eq!(posts[1].target, reply_to("m_g2"));
    assert_eq!(posts[1].markdown, render::guest_refused(Locale::Zh));
    assert_eq!(posts[2].target, reply_to("m_g3"));
    assert_eq!(posts[2].markdown, render::guest_note_refused(Locale::Zh));
    assert_eq!(
        posts[2].markdown,
        "我可以把你的问题转给 agent，但不能转达留言或指示。"
    );
    assert!(rig.hub.events(&committee("checkout")).is_empty());
    assert!(
        rig.hub
            .events(&refund())
            .iter()
            .all(|e| e.from != Address::Speaker || e.kind == Kind::Binding),
        "nothing recorded for them"
    );

    // A question is relayed, on the guest's behalf.
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: "退款上限是多少天？".into(),
    }));
    rig.append(guest_mention("问一下 alice 退款上限是多少天", "m_g4"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(relay.to, vec![agent("alice")]);
    assert_eq!(relay.on_behalf_of, Some(guest(STRANGER)));
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("已转给 [senator::claude]（seq {}）", relay.seq).as_str())
    );

    // /tell with a question is a question, and goes through too.
    rig.append(guest_mention("/tell alice 上限改了吗？", "m_g5"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(relay.body, "上限改了吗？");

    // The status and the brief are answered.
    rig.append(guest_mention("/status", "m_g6")).await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert!(
        said.last()
            .is_some_and(|t| t.starts_with("本群绑定在委员会 refund-v2。")),
        "{said:?}"
    );
    rig.append(guest_mention("/brief", "m_g7")).await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert!(
        said.last()
            .is_some_and(|t| t.starts_with("委员会 refund-v2 的简报")),
        "{said:?}"
    );
    for text in &said {
        assert_senate_vocabulary(text);
    }
}

// ----- English -----------------------------------------------------------------

/// The main flows in an English Senate: a binding restated and confirmed,
/// a relay acknowledged, the status, the brief.
#[tokio::test]
async fn the_main_flows_read_in_english() {
    let rig = Rig::start_config(
        MemoryHub::new(),
        FakeSurface::new(),
        ScriptedBrain::new(),
        config_in(Locale::En),
    )
    .await;

    // A first binding, restated, then confirmed with "yes".
    rig.brain.interprets(Ok(Intent::Bind {
        committee: "refund-v2".into(),
    }));
    let ask = unbound("pm", "bind this group to refund-v2", "m_m1");
    rig.mention_unbound(ask.clone()).await;
    let posts = rig.surface.posts();
    assert_eq!(
        posts[0].markdown,
        "I understand: Bind this group to committee refund-v2. Reply \"yes\" to confirm."
    );
    rig.mention_unbound(UnboundMention {
        message_ref: "m_m2".into(),
        thread_ref: posts[0].thread_ref.clone(),
        thread_root_message_ref: Some("m_m1".into()),
        addressing: "ambient".into(),
        text: "yes".into(),
        ..ask
    })
    .await;
    let texts = rig.surface.texts();
    assert_eq!(texts[1], "Bound to committee refund-v2.");
    assert!(
        texts[2].starts_with("There is no Agent group yet"),
        "{texts:?}"
    );
    let binding = rig.hub.events(&refund())[0].binding().expect("binding");
    assert_eq!(binding.action, BindingAction::Bind);

    // The group is bound to refund-v2 now; it is "random" in the log.
    rig.hub.connect(&refund(), &agent_path("alice"));
    let in_random = |from: &str, text: &str, message_ref: &str| {
        super::rig::mention_in(&refund(), "random", from, text, message_ref)
    };

    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Note,
        body: "the refund limit is 30 days".into(),
    }));
    rig.append(in_random(
        "pm",
        "tell alice's agent the refund limit is 30 days",
        "m_h1",
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent("alice")]);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("Passed to [senator::claude] (seq {})", relay.seq).as_str())
    );

    rig.append(in_random("pm", "/status", "m_h2")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(
            "This group is bound to committee refund-v2.\nAgents online: [senator::claude]\nAgents offline: [senator::aider], [senator::copilot]"
        )
    );

    rig.append(in_random("pm", "/brief", "m_h3")).await;
    rig.settle().await;
    let said = rig.surface.texts();
    let brief = said.last().cloned().unwrap_or_default();
    assert!(
        brief.starts_with("Brief for committee refund-v2 (as of seq"),
        "{brief}"
    );
    assert!(
        brief.contains("Decisions in force: none recorded yet."),
        "{brief}"
    );

    // The fixed syntax's usage is English too.
    rig.brain
        .interprets(Err(BrainError::Unavailable("provider 503".into())));
    rig.append(in_random("pm", "have a look at this", "m_h4"))
        .await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert!(
        said.last()
            .is_some_and(|t| t.contains("/tell <name> <text>")),
        "{said:?}"
    );
    for text in &said {
        assert_senate_vocabulary(text);
    }
}

/// An English Senate asks which agent in English, and an answer to an
/// on-behalf question is headed by the same byline as in Chinese.
#[tokio::test]
async fn which_agent_and_answers_on_behalf_read_in_english() {
    let hub = hub_with_codex().await;
    let rig = Rig::start_config(
        hub,
        FakeSurface::new(),
        ScriptedBrain::new(),
        config_in(Locale::En),
    )
    .await;
    let ask = rig
        .append(mention("pm", "/tell alice update the refund doc", "m_h1"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(
        posts[0].markdown,
        "[senator::alice] has 2 agents here. Which one do you mean? Reply with its number or name:\n\
         1. [senator::claude] not online\n\
         2. [senator::codex] not online"
    );
    rig.append(in_thread(
        "pm",
        "claude",
        "m_h2",
        ask.seq,
        posts[0].thread_ref.clone(),
        &posts[0].message_ref,
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent("alice")]);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("Passed to [senator::claude] (seq {})", relay.seq).as_str())
    );

    rig.brain.answers(Ok("30 days (seq 1)".into()));
    rig.append(question(
        agent("alice"),
        agent("bob"),
        "What is the refund limit?",
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[speaker::copilot] 30 days (seq 1)")
    );
}
