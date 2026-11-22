//! The Speaker's main flows: relays and their answers, bindings, the fixed
//! syntax, answers on an agent's behalf, restarts and outages; which of a
//! person's agents a relay reaches; what a guest may ask; English.

use std::time::Duration;

use senate_core::{
    Account, Address, BindingAction, Event, Hub, Kind, Locale, Mirror, NewEvent, Origin,
    PostTarget, Principal, Roster, Seq, SurfaceError, UnboundMention, meta,
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
    let mut principals = base.principals().to_vec();
    principals.push(Principal {
        kind: senate_core::PrincipalKind::Person,
        handle: name("carol"),
        names: vec!["Carol".into()],
        accounts: vec![Account {
            platform: PLATFORM.into(),
            id: "u_carol".into(),
        }],
    });
    Roster::new(principals, base.seated().to_vec()).expect("roster")
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
    assert_eq!(utterance.roster.principals().len(), 3);

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

    // The model is down: the Speaker says so in one line, without the
    // fixed-syntax dump (nothing the asker wrote was wrong).
    rig.brain
        .interprets(Err(BrainError::Unavailable("provider 503".into())));
    rig.append(mention("pm", "帮我看看这个", "m_h1")).await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert_eq!(
        said[0],
        render::model_unavailable(Locale::Zh, None),
        "{said:?}"
    );

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
            "本群绑定在委员会 refund-v2。\n在线 agent：[senator::claude]\n离线 agent：无\n席位：\n- [senator::claude] 在线"
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
async fn a_direct_chair_question_stays_with_its_recipient_after_speaker_replay() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain
        .answers(Ok("an answer for an ordinary question".into()));
    let origin = Origin {
        locator: "/senate::other::senator::copilot".parse().expect("locator"),
        thread: Seq(3),
    };
    let direct = rig
        .append(
            question(Address::Committee, agent("alice"), "direct question")
                .on_behalf_of(agent("bob"))
                .with_meta(meta::ORIGIN, origin.value())
                .with_meta(meta::CHAIR_DISCUSSION, json!(true)),
        )
        .await;
    rig.settle().await;
    assert_eq!(rig.cursor(&refund()).await, direct.seq);
    assert!(rig.surface.posts().is_empty());
    assert!(rig.brain.questions().is_empty());

    let (hub, surface, brain) = (rig.hub.clone(), rig.surface.clone(), rig.brain.clone());
    rig.stop().await;
    let rig = Rig::start_with(hub, surface, brain).await;
    tokio::time::advance(Duration::from_secs(120)).await;
    rig.settle().await;
    assert_eq!(rig.cursor(&refund()).await, direct.seq);
    assert!(rig.surface.posts().is_empty());
    assert!(rig.brain.questions().is_empty());

    let normal = rig
        .append(question(agent("bob"), agent("alice"), "ordinary question"))
        .await;
    rig.settle().await;
    assert_eq!(rig.brain.questions()[0].event.seq, normal.seq);
    assert_eq!(rig.surface.posts().len(), 2);
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
            "This group is bound to committee refund-v2.\nAgents online: [senator::claude]\nAgents offline: none\nSeats:\n- [senator::claude] online"
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

    // The model's failure reads in English too; with no model at all, the
    // fixed syntax's usage does.
    rig.brain
        .interprets(Err(BrainError::Unavailable("provider 503".into())));
    rig.append(in_random("pm", "have a look at this", "m_h4"))
        .await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert_eq!(
        said.last(),
        Some(&render::model_unavailable(Locale::En, None)),
        "{said:?}"
    );
    rig.brain.interprets(Err(BrainError::NoModel));
    rig.append(in_random("pm", "have a look at this", "m_h5"))
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

// ----- a person's agent one level up: their child Senates ---------------------------

/// The child Senate's desk for refund-v2, as the parent's log names it.
fn desk_of(child: &str) -> Address {
    Address::Child {
        senate: name(child),
        committee: refund(),
    }
}

/// carol runs no agent here and is the Speaker of one child Senate: her
/// agent is that Senate's Speaker's desk, on the same committee; written
/// out, `senator::carol` is still carol.
#[tokio::test]
async fn a_persons_agent_with_one_child_senate_is_that_senates_desk() {
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    hub.add_child("north-desk", "carol");
    // Someone else's child Senate changes nothing for carol.
    hub.add_child("south-desk", "bob");
    let rig = Rig::start(hub).await;

    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["carol".into()],
        kind: RelayKind::Question,
        body: "退款文档更新了吗？".into(),
    }));
    rig.append(mention(
        "pm",
        "问一下 carol 的 agent 退款文档更新了吗",
        "m_c1",
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(relay.to, vec![desk_of("north-desk")]);
    assert_eq!(relay.on_behalf_of, Some(user("pm")));
    assert!(rig.brain.pick_inputs().is_empty(), "nothing to pick from");

    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["senator::carol".into()],
        kind: RelayKind::Note,
        body: "记得更新退款文档".into(),
    }));
    rig.append(mention(
        "pm",
        "告诉 senator::carol 记得更新退款文档",
        "m_c2",
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![user("carol")], "the person, written out");
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

/// Two child Senates: the Speaker asks which, and the answer carries the
/// relay on to that Senate's desk.
#[tokio::test]
async fn a_persons_agent_with_two_child_senates_is_asked_and_picked() {
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    hub.add_child("north-desk", "carol");
    hub.add_child("south-desk", "carol");
    let rig = Rig::start(hub).await;

    let ask = rig
        .append(mention("pm", "/tell carol 记得更新退款文档", "m_c1"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.hub.events(&refund()).len(),
        2,
        "nothing relayed while it asks"
    );
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].markdown,
        "[senator::carol] 是 2 个 Senate 的 Speaker，你指的是哪一个？回复编号或名字：\n\
         1. [north-desk]\n\
         2. [south-desk]"
    );
    let picked = rig.brain.pick_inputs();
    assert_eq!(picked.len(), 1, "the same pick as among agents");
    assert_eq!(picked[0].owner, Some(name("carol")));

    rig.append(in_thread(
        "pm",
        "south-desk",
        "m_c2",
        ask.seq,
        posts[0].thread_ref.clone(),
        &posts[0].message_ref,
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Note);
    assert_eq!(relay.to, vec![desk_of("south-desk")]);
    assert_eq!(relay.body, "记得更新退款文档");

    // In English, the same question names the Senates.
    assert_eq!(
        render::which_senate(
            Locale::En,
            Some(&name("carol")),
            &[path("north-desk"), path("south-desk")]
        ),
        "[senator::carol] is the Speaker of 2 Senates. Which one do you mean? Reply with its number or name:\n\
         1. [north-desk]\n\
         2. [south-desk]"
    );
}

/// The Brain's pick among the Senates is honoured.
#[tokio::test]
async fn the_brain_may_pick_one_of_a_persons_child_senates() {
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    hub.add_child("north-desk", "carol");
    hub.add_child("south-desk", "carol");
    let rig = Rig::start(hub).await;
    rig.brain.picks(Ok(Pick::Agent(path("north-desk"))));
    rig.append(mention("pm", "/tell carol 记得更新退款文档", "m_c1"))
        .await;
    rig.settle().await;
    assert_eq!(last_relay(&rig).to, vec![desk_of("north-desk")]);
}

/// A person who runs an agent here keeps the lookup here, child Senates
/// or not.
#[tokio::test]
async fn a_person_with_an_agent_here_keeps_it_over_a_child_senate() {
    let hub = bound_hub().await;
    hub.add_child("north-desk", "alice");
    let rig = Rig::start(hub).await;
    rig.append(mention("pm", "/tell alice 记得更新退款文档", "m_c1"))
        .await;
    rig.settle().await;
    assert_eq!(last_relay(&rig).to, vec![agent("alice")]);
}

// ----- a seat in a child Senate ---------------------------------------------------------

use senate_core::PresenceState;

/// The seat a relay is meant for, as its `meta.intended` names it.
fn intended(relay: &Event) -> Option<String> {
    senate_core::locator::intended(relay).map(|seat| seat.to_string())
}

/// carol's child Senate declares one seat on the committee: a relay for
/// carol's agent goes to that Senate's desk and names the seat, online or
/// not. Nothing is picked and nothing is asked.
#[tokio::test]
async fn a_relay_to_a_persons_child_senate_names_its_only_seat() {
    for presence in [PresenceState::Online, PresenceState::Absent] {
        let hub = bound_hub().await;
        hub.set_roster(roster_with_carol());
        hub.add_child("north-desk", "carol");
        hub.declare_seats("north-desk", &refund(), &[("responder", presence)]);
        // A seat on another committee is not one of this committee's.
        let other = senate_core::CommitteeId::new("billing").expect("committee");
        hub.declare_seats("north-desk", &other, &[("auditor", PresenceState::Online)]);
        let rig = Rig::start(hub).await;

        rig.append(mention("pm", "/tell carol 退款文档更新了吗？", "m_s1"))
            .await;
        rig.settle().await;
        let relay = last_relay(&rig);
        assert_eq!(relay.kind, Kind::Question);
        assert_eq!(relay.to, vec![desk_of("north-desk")], "by way of the desk");
        assert_eq!(
            intended(&relay).as_deref(),
            Some("north-desk::refund-v2::senator::responder"),
            "{presence:?}"
        );
        assert!(
            rig.brain.pick_inputs().is_empty(),
            "one seat: nothing to pick"
        );
        assert_eq!(
            rig.surface.texts().last().map(String::as_str),
            Some(
                format!(
                    "已转给 [north-desk::refund-v2::senator::responder]（seq {}）",
                    relay.seq
                )
                .as_str()
            )
        );
    }
}

/// Several seats on the committee: the same pick as among a person's
/// agents, with what the child declared of each seat. A pick names the
/// seat; a pick that cannot tell names none, and nothing is asked up here.
#[tokio::test]
async fn among_several_seats_of_a_child_senate_the_brain_picks_or_the_desk_decides() {
    let seats = [
        ("responder", PresenceState::Online),
        ("reviewer", PresenceState::Online),
        ("archivist", PresenceState::Absent),
    ];
    // The Brain picks one, online or not.
    for picked in ["reviewer", "archivist"] {
        let hub = bound_hub().await;
        hub.set_roster(roster_with_carol());
        hub.add_child("north-desk", "carol");
        hub.declare_seats("north-desk", &refund(), &seats);
        hub.declare_duty("north-desk", &refund(), "reviewer", "审阅退款文档");
        let rig = Rig::start(hub).await;
        rig.brain.picks(Ok(Pick::Agent(path(picked))));
        rig.append(mention("pm", "/tell carol 退款文档更新了吗？", "m_s1"))
            .await;
        rig.settle().await;
        let relay = last_relay(&rig);
        assert_eq!(relay.to, vec![desk_of("north-desk")]);
        assert_eq!(
            intended(&relay),
            Some(format!("north-desk::refund-v2::senator::{picked}"))
        );
        let asked = rig.brain.pick_inputs();
        assert_eq!(
            asked.len(),
            1,
            "asked once, though the relay waits on triage"
        );
        assert_eq!(asked[0].owner, Some(name("carol")));
        assert_eq!(asked[0].task, "退款文档更新了吗？");
        let seen: Vec<(String, bool, Option<&str>)> = asked[0]
            .candidates
            .iter()
            .map(|c| (c.agent.to_string(), c.present, c.duty.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                ("responder".to_owned(), true, None),
                ("reviewer".to_owned(), true, Some("审阅退款文档")),
                ("archivist".to_owned(), false, None),
            ],
            "each seat as the child declared it: presence and duty"
        );
    }

    // The Brain cannot tell (so do the rules, with two seats online): the
    // desk, no seat named, and no question about seats only the child knows.
    for pick in [Some(Ok(Pick::Ask)), None] {
        let hub = bound_hub().await;
        hub.set_roster(roster_with_carol());
        hub.add_child("north-desk", "carol");
        hub.declare_seats("north-desk", &refund(), &seats);
        let rig = Rig::start(hub).await;
        if let Some(pick) = pick {
            rig.brain.picks(pick);
        }
        rig.append(mention("pm", "/tell carol 退款文档更新了吗？", "m_s1"))
            .await;
        rig.settle().await;
        let relay = last_relay(&rig);
        assert_eq!(relay.to, vec![desk_of("north-desk")]);
        assert_eq!(intended(&relay), None);
        let texts = rig.surface.texts();
        assert_eq!(
            texts,
            [format!(
                "已转给 [north-desk::refund-v2::speaker]（seq {}）",
                relay.seq
            )],
            "only the acknowledgement"
        );
    }

    // The rules alone: the only seat online.
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    hub.add_child("north-desk", "carol");
    hub.declare_seats(
        "north-desk",
        &refund(),
        &[
            ("responder", PresenceState::Disconnected),
            ("reviewer", PresenceState::Online),
        ],
    );
    let rig = Rig::start(hub).await;
    rig.append(mention("pm", "/tell carol 退款文档更新了吗？", "m_s1"))
        .await;
    rig.settle().await;
    assert_eq!(
        intended(&last_relay(&rig)).as_deref(),
        Some("north-desk::refund-v2::senator::reviewer")
    );
}

/// Two child Senates: the person says which, and the relay then names the
/// seat of the one chosen.
#[tokio::test]
async fn a_chosen_child_senate_is_reached_at_its_seat() {
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    hub.add_child("north-desk", "carol");
    hub.add_child("south-desk", "carol");
    hub.declare_seats(
        "north-desk",
        &refund(),
        &[("responder", PresenceState::Online)],
    );
    hub.declare_seats("south-desk", &refund(), &[("clerk", PresenceState::Online)]);
    let rig = Rig::start(hub).await;
    let ask = rig
        .append(mention("pm", "/tell carol 记得更新退款文档", "m_s1"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1, "which Senate is asked, as before");
    rig.append(in_thread(
        "pm",
        "2",
        "m_s2",
        ask.seq,
        posts[0].thread_ref.clone(),
        &posts[0].message_ref,
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![desk_of("south-desk")]);
    assert_eq!(
        intended(&relay).as_deref(),
        Some("south-desk::refund-v2::senator::clerk")
    );
}

/// A seat a child Senate declares is named outright in three ways; each
/// resolves to that seat when the name is no senator here.
#[tokio::test]
async fn a_child_senates_seat_named_outright_is_that_seat() {
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    hub.add_child("north-desk", "carol");
    hub.add_child("south-desk", "bob");
    hub.declare_seats(
        "north-desk",
        &refund(),
        &[
            ("responder", PresenceState::Online),
            ("reviewer", PresenceState::Online),
        ],
    );
    hub.declare_seats(
        "south-desk",
        &refund(),
        &[("reviewer", PresenceState::Online)],
    );
    hub.declare_alias("north-desk", &refund(), "responder", "north-dev");
    hub.declare_alias("south-desk", &refund(), "reviewer", "south-dev");
    let rig = Rig::start(hub).await;
    let relayed = |n: usize, target: &str| {
        let text = format!("/tell {target} 记得更新退款文档 {n}");
        let message_ref = format!("m_n{n}");
        let rig = &rig;
        async move {
            let before = rig.hub.events(&refund()).len();
            rig.append(mention("pm", &text, &message_ref)).await;
            rig.settle().await;
            let events = rig.hub.events(&refund());
            // The chat and, when the target was understood, the relay.
            (events.len() > before + 1).then(|| last_relay(rig))
        }
    };
    for (n, target) in [
        "responder",
        "senator::responder",
        "north-desk::responder",
        "north-desk::refund-v2::senator::responder",
    ]
    .into_iter()
    .enumerate()
    {
        let relay = relayed(n, target)
            .await
            .unwrap_or_else(|| panic!("{target}"));
        assert_eq!(relay.to, vec![desk_of("north-desk")], "{target}");
        assert_eq!(
            intended(&relay).as_deref(),
            Some("north-desk::refund-v2::senator::responder"),
            "{target}"
        );
    }
    assert!(rig.brain.pick_inputs().is_empty(), "named: nothing to pick");

    // Two Senates declare `reviewer`: alone it names neither; with its
    // Senate it is that one's.
    assert_eq!(relayed(10, "reviewer").await, None);
    let relay = relayed(11, "south-desk::reviewer").await.expect("named");
    assert_eq!(relay.to, vec![desk_of("south-desk")]);
    assert_eq!(
        intended(&relay).as_deref(),
        Some("south-desk::refund-v2::senator::reviewer")
    );
    // A seat the Senate does not declare is nobody; in full it is taken
    // as said, and the desk there deals with it.
    assert_eq!(relayed(12, "north-desk::stranger").await, None);
    let relay = relayed(13, "north-desk::refund-v2::senator::stranger")
        .await
        .expect("named in full");
    assert_eq!(
        intended(&relay).as_deref(),
        Some("north-desk::refund-v2::senator::stranger")
    );
    // An explicit desk stays a desk. Explicit seats each get their own thread.
    let relay = relayed(14, "north-desk::speaker").await.expect("the desk");
    assert_eq!(relay.to, vec![desk_of("north-desk")]);
    assert_eq!(intended(&relay), None);
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec![
            "north-desk::responder".into(),
            "south-desk::reviewer".into(),
        ],
        kind: RelayKind::Question,
        body: "谁负责群公告？".into(),
    }));
    rig.append(mention("pm", "请两边核对群公告", "m_n16")).await;
    rig.settle().await;
    let relays: Vec<_> = rig
        .hub
        .events(&refund())
        .into_iter()
        .filter(|e| e.kind == Kind::Question && e.body == "谁负责群公告？")
        .collect();
    assert_eq!(relays.len(), 2);
    for (event, child, seat) in [
        (&relays[0], "north-desk", "responder"),
        (&relays[1], "south-desk", "reviewer"),
    ] {
        assert_eq!(event.to, vec![desk_of(child)]);
        assert_eq!(
            intended(event),
            Some(format!("{child}::refund-v2::senator::{seat}"))
        );
    }
    let receipts: Vec<_> = rig
        .surface
        .posts()
        .into_iter()
        .filter(|p| p.target == reply_to("m_n16"))
        .map(|p| p.markdown)
        .collect();
    assert_eq!(
        receipts,
        ["已转给 north-dev（north-desk）、south-dev（south-desk）"]
    );
    for (event, child, seat) in [
        (&relays[0], "north-desk", "responder"),
        (&relays[1], "south-desk", "reviewer"),
    ] {
        rig.append(
            NewEvent::new(
                refund(),
                Kind::Answer,
                desk_of(child),
                format!("answer from {seat}"),
            )
            .in_thread(event.seq)
            .with_meta(
                meta::ANSWERED_BY,
                json!(format!("{child}::refund-v2::senator::{seat}")),
            ),
        )
        .await;
    }
    rig.settle().await;
    let replies: Vec<_> = rig
        .surface
        .posts()
        .into_iter()
        .filter(|p| p.target == reply_to("m_n16"))
        .map(|p| p.markdown)
        .collect();
    assert_eq!(
        &replies[1..],
        [
            "north-dev（north-desk） answer from responder",
            "south-dev（south-desk） answer from reviewer"
        ]
    );
}

/// A senator of this Senate keeps its name: a child Senate's seat of the
/// same name is reached by saying whose it is.
#[tokio::test]
async fn a_senator_here_wins_over_a_child_senates_seat_of_the_same_name() {
    let hub = bound_hub().await;
    hub.add_child("north-desk", "alice");
    hub.declare_seats(
        "north-desk",
        &refund(),
        &[("claude", PresenceState::Online)],
    );
    let rig = Rig::start(hub).await;
    rig.append(mention("pm", "/tell claude 记得更新退款文档", "m_w1"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![agent("alice")]);
    assert_eq!(intended(&relay), None);

    rig.append(mention(
        "pm",
        "/tell north-desk::claude 记得更新退款文档",
        "m_w2",
    ))
    .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![desk_of("north-desk")]);
    assert_eq!(
        intended(&relay).as_deref(),
        Some("north-desk::refund-v2::senator::claude")
    );
}

// ----- a child Senate that is offline -----------------------------------------------------

use crate::Offline;

/// carol's child Senate with one seat, last seen `minutes` ago (`None`:
/// never since this Senate started).
async fn hub_with_child_seen(minutes: Option<i64>) -> std::sync::Arc<MemoryHub> {
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    hub.add_child("north-desk", "carol");
    hub.declare_seats(
        "north-desk",
        &refund(),
        &[("responder", PresenceState::Disconnected)],
    );
    hub.child_seen(
        "north-desk",
        minutes.map(|m| chrono::Utc::now() - chrono::Duration::minutes(m)),
    );
    hub
}

/// What the office wrote in the relay's thread about the child being
/// offline.
fn offline_lines(rig: &Rig, relay: &Event) -> Vec<Event> {
    rig.hub
        .events(&refund())
        .into_iter()
        .filter(|e| e.thread == Some(relay.seq) && e.meta.contains_key(meta::CHILD_OFFLINE))
        .collect()
}

/// The relay the Speaker wrote for `text`, which must be to north-desk.
async fn relayed_to_north(rig: &Rig, text: &str, message_ref: &str) -> Event {
    rig.append(mention("pm", text, message_ref)).await;
    rig.settle().await;
    let relay = rig
        .hub
        .events(&refund())
        .into_iter()
        .rev()
        .find(|e| e.thread.is_none() && e.to == vec![desk_of("north-desk")])
        .expect("a relay to north-desk");
    assert_eq!(relay.from, Address::Speaker);
    relay
}

#[tokio::test]
async fn child_escalation_and_verified_reply_share_the_original_chat_thread() {
    let rig = Rig::start(hub_with_child_seen(Some(0)).await).await;
    let root = relayed_to_north(&rig, "/tell carol check the docs?", "m_root").await;
    let child = desk_of("north-desk");
    rig.append(
        NewEvent::new(refund(), Kind::Note, child.clone(), "Waiting for approval")
            .in_thread(root.seq)
            .with_meta(meta::APPROVAL_WAIT, json!(true)),
    )
    .await;
    rig.settle().await;
    rig.append(in_thread(
        "pm",
        "yes",
        "approval_chat",
        root.seq,
        None,
        "m_root",
    ))
    .await;
    rig.settle().await;
    let informational = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.from == Address::Speaker && e.body == "yes")
        .unwrap();
    assert_eq!(informational.kind, Kind::Note);
    assert!(!informational.meta.contains_key(meta::APPROVAL_WAIT));
    assert!(!informational.meta.contains_key(meta::HOSTED_APPROVAL));
    rig.append(
        NewEvent::new(refund(), Kind::Note, child.clone(), "Which branch?")
            .in_thread(root.seq)
            .with_meta(meta::RELAY_ASKED_PERSON, json!("pm")),
    )
    .await;
    rig.settle().await;
    assert!(
        rig.surface
            .posts()
            .iter()
            .any(|p| p.target == reply_to("m_root") && p.markdown.contains("Which branch?"))
    );
    for (who, simulated, kind) in [
        ("bob", false, Kind::Note),
        ("pm", true, Kind::Note),
        ("pm", false, Kind::Answer),
    ] {
        let body = format!("reply {who} {simulated}");
        let mut chat = mention(who, &body, &body).in_thread(root.seq);
        chat.source.as_mut().unwrap().simulated = simulated;
        rig.append(chat).await;
        rig.settle().await;
        let reply = rig
            .hub
            .events(&refund())
            .into_iter()
            .find(|e| e.from == Address::Speaker && e.body == body)
            .unwrap();
        assert_eq!(
            (reply.kind, reply.thread, reply.to),
            (kind, Some(root.seq), vec![child.clone()])
        );
        assert_eq!(reply.on_behalf_of, (!simulated).then(|| user(who)));
    }
}

/// A question relayed to a child Senate that is offline is still written
/// for it, seat and all; the rules hold it, and the one who asked is told
/// it is offline, once.
#[tokio::test]
async fn a_question_for_an_offline_child_senate_is_held_and_the_asker_told() {
    let rig = Rig::start(hub_with_child_seen(Some(5)).await).await;
    let relay = relayed_to_north(&rig, "/tell carol 退款文档更新了吗？", "m_o1").await;
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(
        intended(&relay).as_deref(),
        Some("north-desk::refund-v2::senator::responder"),
        "an offline seat is still named"
    );

    let lines = offline_lines(&rig, &relay);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let held = &lines[0];
    assert_eq!(held.kind, Kind::Note);
    assert_eq!(held.from, Address::Speaker);
    assert_eq!(
        held.to,
        vec![user("pm")],
        "the one the relay was written for"
    );
    assert_eq!(held.on_behalf_of, None);
    assert_eq!(held.meta[meta::CHILD_OFFLINE], json!("north-desk"));
    assert_eq!(
        held.body,
        "[north-desk] 目前离线（最后一次连接在 5 分钟前）。这条消息已保留，它重新连接后会收到。"
    );
    assert!(
        rig.surface.texts().iter().any(|t| t.contains("目前离线")),
        "said where it was asked: {:#?}",
        rig.surface.texts()
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }

    // The Brain was asked, with what this Senate knows of the child.
    let asked = rig.brain.offline_inputs();
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0].event.seq, relay.seq);
    assert_eq!(asked[0].child.name, name("north-desk"));
    assert_eq!(asked[0].child.seats.len(), 1);
    assert_eq!(
        asked[0]
            .intended
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("north-desk::refund-v2::senator::responder")
    );
    assert!(asked[0].brief.is_some());
}

/// The Brain may answer on the child's behalf: its answer goes to the one
/// who asked, in the child's name, and the question still waits for the
/// child. A Brain that fails holds.
#[tokio::test]
async fn the_brain_may_answer_for_an_offline_child_senate() {
    let rig = Rig::start(hub_with_child_seen(Some(120)).await).await;
    rig.brain
        .decides_offline(Ok(Offline::Answer("已更新，见 brief 的检查点。".into())));
    let relay = relayed_to_north(&rig, "/tell carol 退款文档更新了吗？", "m_o1").await;
    let lines = offline_lines(&rig, &relay);
    assert_eq!(lines.len(), 1, "{lines:#?}");
    let answer = &lines[0];
    assert_eq!(answer.kind, Kind::Answer);
    assert_eq!(answer.from, Address::Speaker);
    assert_eq!(answer.on_behalf_of, Some(desk_of("north-desk")));
    assert_eq!(
        answer.to,
        vec![user("pm"), desk_of("north-desk")],
        "to the asker, and to the child for when it is back"
    );
    assert_eq!(
        answer.body,
        "[north-desk] 目前离线，以下由 Speaker 的办公室代答；它重新连接后仍会收到这个问题。\n\
         已更新，见 brief 的检查点。"
    );
    assert_eq!(relay.to, vec![desk_of("north-desk")], "the question stands");

    rig.brain
        .decides_offline(Err(BrainError::Unavailable("down".into())));
    let relay = relayed_to_north(&rig, "/tell carol 网关用哪个 header？", "m_o2").await;
    let lines = offline_lines(&rig, &relay);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].kind, Kind::Note, "no decision: held");
}

/// A note is never answered for the child: it is held, and the Brain is
/// not asked.
#[tokio::test]
async fn a_note_for_an_offline_child_senate_is_only_held() {
    let rig = Rig::start(hub_with_child_seen(Some(5)).await).await;
    rig.brain
        .decides_offline(Ok(Offline::Answer("不该用到".into())));
    let relay = relayed_to_north(&rig, "/tell carol 记得更新退款文档", "m_o1").await;
    assert_eq!(relay.kind, Kind::Note);
    let lines = offline_lines(&rig, &relay);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].kind, Kind::Note);
    assert!(rig.brain.offline_inputs().is_empty());
}

/// A child Senate that is online, or that has not declared itself since
/// this Senate started, is not known to be offline: nothing is said.
#[tokio::test]
async fn a_child_senate_not_known_to_be_offline_gets_no_line() {
    for seen in [Some(0), None] {
        let rig = Rig::start(hub_with_child_seen(seen).await).await;
        let relay = relayed_to_north(&rig, "/tell carol 退款文档更新了吗？", "m_o1").await;
        assert!(offline_lines(&rig, &relay).is_empty(), "{seen:?}");
        assert!(rig.brain.offline_inputs().is_empty(), "{seen:?}");
    }
}

// ----- an unaddressed question, relayed by duty --------------------------------------

const DEPLOY_DUTY: &str = "负责回答 退款服务的部署、架构和使用问题";

/// The seats an utterance was interpreted with, as `(target, duty, online)`.
fn seats_seen(rig: &Rig) -> Vec<(String, Option<String>, bool)> {
    let utterances = rig.brain.utterances();
    let utterance = utterances.last().expect("interpreted");
    utterance
        .seats
        .iter()
        .map(|s| (s.target.clone(), s.duty.clone(), s.online))
        .collect()
}

/// A hub with codex (alice's second agent) connected on the committee with
/// a duty, and carol's child Senate `carol-desk` declaring `seats` there.
async fn duty_hub(seats: &[(&str, PresenceState, Option<&str>)]) -> std::sync::Arc<MemoryHub> {
    let hub = bound_hub().await;
    hub.set_roster(roster_with_carol());
    hub.seat(&path("codex"), "alice");
    hub.connect(&refund(), &path("codex"));
    hub.append(
        senate_core::DutyPayload {
            senator: path("codex"),
            duty: "owns the retry path".into(),
        }
        .note(refund(), Address::senator(&path("codex"))),
    )
    .await
    .expect("duty");
    hub.add_child("carol-desk", "carol");
    let declared: Vec<(&str, PresenceState)> = seats.iter().map(|(s, p, _)| (*s, *p)).collect();
    hub.declare_seats("carol-desk", &refund(), &declared);
    for (seat, _, duty) in seats {
        if let Some(duty) = duty {
            hub.declare_duty("carol-desk", &refund(), seat, duty);
        }
    }
    // A seat on another committee is not one of this committee's.
    let other = senate_core::CommitteeId::new("billing").expect("committee");
    hub.declare_seats("carol-desk", &other, &[("auditor", PresenceState::Online)]);
    hub
}

/// Thread 749: "@Speaker 退款服务是怎么部署的？", addressed to nobody. The
/// model sees every seat on the committee with its duty, here and in the
/// child Senate, and its relay to the one whose duty fits, written as the
/// seats list it, reaches the child's desk naming that seat.
#[tokio::test]
async fn an_unaddressed_question_reaches_the_child_seat_whose_duty_fits() {
    let hub = duty_hub(&[
        ("responder", PresenceState::Online, Some(DEPLOY_DUTY)),
        ("archivist", PresenceState::Absent, None),
    ])
    .await;
    let rig = Rig::start(hub).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["carol-desk::responder".into()],
        kind: RelayKind::Question,
        body: "退款服务是怎么部署的？".into(),
    }));
    rig.append(mention("pm", "退款服务是怎么部署的？", "m_d1"))
        .await;
    rig.settle().await;

    assert_eq!(
        seats_seen(&rig),
        [
            (
                "codex".to_owned(),
                Some("owns the retry path".to_owned()),
                true
            ),
            (
                "carol-desk::responder".to_owned(),
                Some(DEPLOY_DUTY.to_owned()),
                true
            ),
            ("carol-desk::archivist".to_owned(), None, false),
        ],
        "every seat on the committee, as a relay names it, with duty and presence"
    );
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(relay.to, vec![desk_of("carol-desk")], "by way of the desk");
    assert_eq!(
        intended(&relay).as_deref(),
        Some("carol-desk::refund-v2::senator::responder")
    );
    assert_eq!(relay.body, "退款服务是怎么部署的？");
    assert!(
        rig.brain.pick_inputs().is_empty(),
        "named outright: no pick"
    );
}

/// Thread 797 item 8: an unaddressed change request, where this Senate
/// does no coding work itself. The Brain is not offered `work`, and its
/// relay by duty goes to the child seat as a `request`, through the desk.
#[tokio::test]
async fn an_unaddressed_change_request_reaches_the_duty_seat_as_a_request() {
    let hub = duty_hub(&[("responder", PresenceState::Online, Some(DEPLOY_DUTY))]).await;
    let rig = Rig::start(hub).await;
    let asked = "请在 refund 仓库的 deploy/README.md 末尾加一节，开分支、本地提交";
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["carol-desk::responder".into()],
        kind: RelayKind::Request,
        body: asked.into(),
    }));
    rig.append(mention("pm", asked, "m_d9")).await;
    rig.settle().await;
    let utterances = rig.brain.utterances();
    assert!(
        !utterances.last().expect("interpreted").work,
        "no [speaker.work] here: work is not offered"
    );
    let relay = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.from == Address::Speaker && e.kind == Kind::Request)
        .expect("a request relayed");
    assert_eq!(relay.to, vec![desk_of("carol-desk")]);
    assert_eq!(
        intended(&relay).as_deref(),
        Some("carol-desk::refund-v2::senator::responder")
    );
    assert_eq!(relay.body, asked);
    assert_eq!(relay.on_behalf_of, Some(user("pm")));
}

/// The same for a seat of this Senate: a relay to `codex` by its duty
/// reaches codex itself.
#[tokio::test]
async fn an_unaddressed_question_reaches_the_local_seat_whose_duty_fits() {
    let hub = duty_hub(&[("responder", PresenceState::Online, Some(DEPLOY_DUTY))]).await;
    let rig = Rig::start(hub).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["codex".into()],
        kind: RelayKind::Question,
        body: "重试路径现在是怎么做的？".into(),
    }));
    rig.append(mention("pm", "重试路径现在是怎么做的？", "m_d2"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.kind, Kind::Question);
    assert_eq!(relay.to, vec![Address::senator(&path("codex"))]);
    assert_eq!(intended(&relay), None);
    assert!(
        rig.brain.pick_inputs().is_empty(),
        "named outright: no pick"
    );
}

/// A cross-product implementation question goes to both responsible seats,
/// including a child whose last declaration has gone stale.
#[tokio::test]
async fn a_question_requiring_two_duties_reaches_both_seats() {
    let hub = duty_hub(&[
        (
            "responder",
            PresenceState::Online,
            Some("Senate announcements"),
        ),
        (
            "operator",
            PresenceState::Online,
            Some("Commons integration"),
        ),
    ])
    .await;
    hub.child_seen(
        "carol-desk",
        Some(chrono::Utc::now() - chrono::Duration::hours(1)),
    );
    let rig = Rig::start(hub).await;
    let question = "群公告里显示的席位列表，是 Senate 那边生成的还是 commons 那边生成的？";
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec![
            "carol-desk::responder".into(),
            "carol-desk::operator".into(),
        ],
        kind: RelayKind::Question,
        body: question.into(),
    }));
    rig.append(mention("pm", question, "m_d3")).await;
    rig.settle().await;
    let seen = seats_seen(&rig);
    for (seat, duty) in [
        ("responder", "Senate announcements"),
        ("operator", "Commons integration"),
    ] {
        assert!(seen.contains(&(format!("carol-desk::{seat}"), Some(duty.into()), false)));
        assert!(
            rig.hub
                .events(&refund())
                .iter()
                .any(|e| e.kind == Kind::Question
                    && e.body == question
                    && intended(e) == Some(format!("carol-desk::refund-v2::senator::{seat}")))
        );
    }
}

/// Thread 767: people are shown seats by alias, never by primary id. The
/// seats the model sees carry the name people are shown (the alias here,
/// `<alias>（<child>）` for a child's seat), the relay still names the
/// target, and the acknowledgement in chat names the seat as shown.
#[tokio::test]
async fn seats_are_shown_to_people_by_alias() {
    let hub = duty_hub(&[
        ("responder", PresenceState::Online, Some(DEPLOY_DUTY)),
        ("archivist", PresenceState::Absent, None),
    ])
    .await;
    hub.declare_alias("carol-desk", &refund(), "responder", "refund-dev");
    hub.set_alias(&refund(), "retry-desk", "codex");
    let rig = Rig::start(hub).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["carol-desk::responder".into()],
        kind: RelayKind::Question,
        body: "退款服务是怎么部署的？".into(),
    }));
    rig.append(mention("pm", "退款服务是怎么部署的？", "m_a1"))
        .await;
    rig.settle().await;

    let utterances = rig.brain.utterances();
    let shown: Vec<(String, Option<String>)> = utterances
        .last()
        .expect("interpreted")
        .seats
        .iter()
        .map(|s| (s.target.clone(), s.shown.clone()))
        .collect();
    assert_eq!(
        shown,
        [
            ("codex".to_owned(), Some("retry-desk".to_owned())),
            (
                "carol-desk::responder".to_owned(),
                Some("refund-dev（carol-desk）".to_owned())
            ),
            ("carol-desk::archivist".to_owned(), None),
        ]
    );
    let relay = last_relay(&rig);
    assert_eq!(
        intended(&relay).as_deref(),
        Some("carol-desk::refund-v2::senator::responder"),
        "the relay still names the seat's id"
    );
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("已转给 refund-dev（carol-desk）（seq {}）", relay.seq).as_str())
    );

    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["codex".into()],
        kind: RelayKind::Question,
        body: "重试路径现在是怎么做的？".into(),
    }));
    rig.append(mention("pm", "重试路径现在是怎么做的？", "m_a2"))
        .await;
    rig.settle().await;
    let relay = last_relay(&rig);
    assert_eq!(relay.to, vec![Address::senator(&path("codex"))]);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("已转给 [senator::retry-desk]（seq {}）", relay.seq).as_str())
    );
}

/// Thread 773: in a group bound to a committee whose seats are all in
/// child Senates, `/status` lists every seat online by the name people
/// see, with its duty, and never the built-in session, connected here as
/// it is.
#[tokio::test]
async fn status_lists_the_child_seats_and_never_the_built_in_session() {
    let hub = bound_hub().await;
    hub.set_roster(
        roster_with_carol()
            .with_built_in(path("secretary"))
            .expect("the built-in session"),
    );
    hub.connect(&refund(), &path("secretary"));
    hub.append(super::continuity::presence_of(
        &path("secretary"),
        PresenceState::Online,
        1,
    ))
    .await
    .expect("presence");
    for (child, seat, alias, duty) in [
        ("carol-desk", "claude-11a084c4", "refund-dev", "退款开发"),
        ("bob-desk", "codex-01a1fc99", "billing-dev", "账单的开发"),
    ] {
        hub.add_child(child, "carol");
        hub.declare_seats(child, &refund(), &[(seat, PresenceState::Online)]);
        hub.declare_duty(child, &refund(), seat, duty);
        hub.declare_alias(child, &refund(), seat, alias);
    }
    let rig = Rig::start(hub).await;
    rig.append(mention("pm", "/status", "m_st1")).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(
            "本群绑定在委员会 refund-v2。\n\
             在线 agent：refund-dev（carol-desk）、billing-dev（bob-desk）\n\
             离线 agent：无\n\
             席位：\n\
             - refund-dev（carol-desk） 在线 · 职责：退款开发\n\
             - billing-dev（bob-desk） 在线 · 职责：账单的开发"
        )
    );
}
