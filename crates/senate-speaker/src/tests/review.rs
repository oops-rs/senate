//! Regression tests for findings of an early review (their ids lead the
//! names): operation ids across log resets, who may ask for what, stalls,
//! catch-up, and the Brain's concurrency.

use std::sync::Arc;
use std::time::Duration;

use senate_core::{
    Address, BindingAction, CommitteeId, Event, Hub, Kind, Locale, NewEvent, PostTarget, Seq,
    UnboundMention,
};
use tokio::sync::mpsc;

use super::hub::MemoryHub;
use super::rig::{
    GROUP, Rig, STRANGER, agent, agent_path, answer, assert_senate_vocabulary, bind_event,
    committee, config, guest, in_thread, mention, mention_in, question, refund, unbound, user,
};
use super::surface::FakeSurface;
use crate::render;
use crate::{Intent, RelayKind, ScriptedBrain, SpeakerArgs};

async fn bound_hub() -> Arc<MemoryHub> {
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

/// A mention by [`STRANGER`] in a group bound to nothing.
fn guest_unbound(text: &str, message_ref: &str) -> UnboundMention {
    UnboundMention {
        sender: guest(STRANGER),
        ..unbound("pm", text, message_ref)
    }
}

fn reply_to(message_ref: &str) -> PostTarget {
    PostTarget::Reply {
        message_ref: message_ref.into(),
        in_thread: true,
    }
}

fn kinds(events: &[Event], kind: &Kind) -> usize {
    events.iter().filter(|e| &e.kind == kind).count()
}

/// Yield (never advancing a paused clock) until `done`, or give up.
async fn spin_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..2_000 {
        if done() {
            return true;
        }
        tokio::task::yield_now().await;
    }
    done()
}

#[tokio::test]
async fn h1_a_reset_log_never_reuses_an_operation_id() {
    // botd remembers operation ids across our restart; the log does not.
    let surface = FakeSurface::new();
    let first = bound_hub().await;
    first.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start_with(first, surface.clone(), ScriptedBrain::new()).await;
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "第一个问题"))
        .await;
    rig.settle().await;
    rig.stop().await;

    // The `memory:` server restarted: a new epoch, and seq starts over.
    let second = bound_hub().await;
    second.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start_with(second, surface.clone(), ScriptedBrain::new()).await;
    let again = rig
        .append(question(agent("alice"), agent("bob"), "另一个问题"))
        .await;
    rig.settle().await;
    assert_eq!(again.seq, asked.seq, "the same seq in a new log");

    let posts = surface.posts();
    assert_eq!(surface.conflicts(), 0, "no key was reused");
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert!(posts[1].markdown.contains("另一个问题"));
    assert_ne!(posts[0].operation_id, posts[1].operation_id);
}

// Was: guests were refused every relay with one refusal. Now a guest may
// ask (a question relayed, the status), but not change state or push a note.
#[tokio::test]
async fn h2_only_members_may_bind_unbind_or_relay_a_note() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Note,
        body: "上限改成 90 天".into(),
    }));
    for (text, message_ref) in [
        ("/bind checkout", "m_x1"),
        ("/unbind", "m_x2"),
        ("/tell alice 上限改成 90 天", "m_x3"),
        ("告诉 alice 的 agent，上限改成 90 天", "m_x4"),
        ("/status", "m_x5"),
    ] {
        rig.append(guest_mention(text, message_ref)).await;
    }
    rig.settle().await;

    let refused = render::guest_refused(Locale::Zh);
    let note_refused = render::guest_note_refused(Locale::Zh);
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 5, "one reply each: {posts:#?}");
    for ((post, message_ref), refusal) in posts.iter().zip(["m_x1", "m_x2", "m_x3", "m_x4"]).zip([
        &refused,
        &refused,
        &note_refused,
        &note_refused,
    ]) {
        assert_eq!(post.target, reply_to(message_ref));
        assert_eq!(&post.markdown, refusal);
    }
    assert_eq!(
        refused,
        "只有名册上的人可以让我改动东西：绑定、决策、邀请、输出等级、代码任务、改派。你仍然可以问我问题。"
    );
    assert!(
        posts[4].markdown.starts_with("本群绑定在委员会 refund-v2"),
        "anyone may ask for the status: {}",
        posts[4].markdown
    );
    let events = rig.hub.events(&refund());
    assert_eq!(kinds(&events, &Kind::Binding), 1, "still just the one");
    assert_eq!(
        kinds(&events, &Kind::Note) + kinds(&events, &Kind::Question),
        0
    );
    assert_eq!(kinds(&events, &Kind::Chat), 5, "what they said is recorded");
    assert!(rig.hub.events(&committee("checkout")).is_empty());

    // In a group bound to nothing, too, by command or in words.
    rig.mention_unbound(guest_unbound("/bind refund-v2", "m_u1"))
        .await;
    rig.brain.interprets(Ok(Intent::Bind {
        committee: "refund-v2".into(),
    }));
    rig.mention_unbound(guest_unbound("把这个群绑到 refund-v2", "m_u2"))
        .await;
    let texts = rig.surface.texts();
    assert_eq!(&texts[5..], [refused.clone(), refused]);
    assert_eq!(
        rig.hub.events(&refund()).len(),
        events.len(),
        "nothing bound"
    );
    for text in &texts {
        assert_senate_vocabulary(text);
    }
}

#[tokio::test(start_paused = true)]
async fn h3_a_hung_post_stalls_its_committee_without_wedging_the_speaker() {
    let checkout = committee("checkout");
    let hub = bound_hub().await;
    hub.append(bind_event(&checkout, "frontend"))
        .await
        .expect("bind");
    hub.connect(&refund(), &agent_path("bob"));
    hub.connect(&checkout, &agent_path("bob"));
    let rig = Rig::start(hub).await;

    rig.surface.hang_next();
    let started = tokio::time::Instant::now();
    rig.append(question(agent("alice"), agent("bob"), "退款这边？"))
        .await;
    rig.append(
        NewEvent::new(
            checkout.clone(),
            Kind::Question,
            agent("alice"),
            "结账那边？",
        )
        .to(agent("bob")),
    )
    .await;
    rig.settle().await;
    assert!(
        started.elapsed() >= crate::engine::POST_TIMEOUT,
        "the hung post was given up on"
    );
    let texts = rig.surface.texts();
    assert!(
        texts.iter().any(|t| t.contains("结账那边？")),
        "the other committee went on: {texts:?}"
    );

    // The stalled committee retries the same operation and gets through.
    tokio::time::sleep(Duration::from_secs(3)).await;
    rig.settle().await;
    let texts = rig.surface.texts();
    assert_eq!(texts.len(), 2, "{texts:?}");
    assert!(texts.iter().any(|t| t.contains("退款这边？")));
    assert_eq!(
        rig.cursor(&refund()).await,
        Seq(3),
        "the question and its triage note"
    );
}

#[tokio::test(start_paused = true)]
async fn h3_a_brain_that_never_answers_is_given_up_on() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain
        .interprets_after(Duration::from_secs(3_600), Ok(Intent::Status));
    let started = tokio::time::Instant::now();
    rig.append(mention("pm", "现在什么情况？", "m_h1")).await;
    rig.settle().await;
    let elapsed = started.elapsed();
    let deadline = config().brain_deadline() + crate::engine::BRAIN_SLACK;
    assert!(
        elapsed >= deadline && elapsed < Duration::from_secs(3_600),
        "{elapsed:?}"
    );
    let said = rig.surface.texts();
    assert_eq!(said.len(), 1);
    assert!(
        said[0].contains("/tell <handle>[/<agent>] <内容>"),
        "{said:?}"
    );
}

#[tokio::test]
async fn m1_catching_up_does_not_answer_what_was_answered_further_down() {
    // While the Speaker was down: a question to alice, whose agent was
    // offline by the time we look, and her answer right after it.
    let hub = bound_hub().await;
    let asked = hub
        .append(question(agent("bob"), agent("alice"), "退款上限是多少天？"))
        .await
        .expect("question")
        .event;
    hub.append(answer(agent("alice"), asked.seq, agent("bob"), "30 天"))
        .await
        .expect("answer");
    let rig = Rig::start(hub).await;

    assert!(
        rig.brain.questions().is_empty(),
        "nothing answered on her behalf"
    );
    let events = rig.hub.events(&refund());
    assert_eq!(events.len(), 4, "the triage note too: {events:#?}");
    let texts = rig.surface.texts();
    assert_eq!(texts.len(), 2, "the question and her answer: {texts:?}");
    assert_eq!(texts[1], "[senator::alice/claude] 30 天");
}

#[tokio::test]
async fn m3_chat_without_a_platform_source_is_never_an_instruction() {
    let rig = Rig::start(bound_hub().await).await;
    for text in ["/bind checkout", "/tell alice 改成 90 天", "随便说点什么"] {
        let mut chat = mention("pm", text, "m_h1");
        chat.source = None;
        rig.append(chat).await;
    }
    rig.settle().await;
    assert!(rig.surface.posts().is_empty());
    assert!(rig.brain.utterances().is_empty());
    assert_eq!(rig.hub.events(&refund()).len(), 4, "nothing appended");
    assert!(rig.hub.events(&committee("checkout")).is_empty());
}

#[tokio::test]
async fn m6_an_answer_to_a_note_is_mirrored_like_an_answer_to_a_question() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    rig.brain
        .interprets(Ok(Intent::Relay {
            targets: vec!["alice".into()],
            kind: RelayKind::Note,
            body: "退款上限是 30 天".into(),
        }))
        .interprets(Ok(Intent::Relay {
            targets: vec!["alice".into()],
            kind: RelayKind::Question,
            body: "退款上限改成 30 天了，你那边改了吗？".into(),
        }));
    rig.append(mention(
        "pm",
        "告诉 alice 的 agent，退款上限是 30 天",
        "m_n",
    ))
    .await;
    rig.append(mention(
        "pm",
        "跟 alice 的 agent 说上限改成 30 天了，问她那边改了吗",
        "m_q",
    ))
    .await;
    rig.settle().await;
    let relays: Vec<Event> = rig
        .hub
        .events(&refund())
        .into_iter()
        .filter(|e| e.from == Address::Speaker && e.kind != Kind::Binding)
        .collect();
    assert_eq!(relays.len(), 2);
    let (note, asked) = (&relays[0], &relays[1]);
    assert_eq!(
        (note.kind.clone(), asked.kind.clone()),
        (Kind::Note, Kind::Question)
    );

    // Her agent answers both (the bridge's reply footer invites it).
    rig.append(answer(
        agent("alice"),
        note.seq,
        user("pm"),
        "收到，已按 30 天",
    ))
    .await;
    rig.append(answer(agent("alice"), asked.seq, user("pm"), "已经改了"))
        .await;
    rig.settle().await;

    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 4, "two acks, two answers: {posts:#?}");
    let (note_ack, question_ack) = (&posts[0], &posts[1]);
    let (note_answer, question_answer) = (&posts[2], &posts[3]);
    assert_eq!(note_answer.target, reply_to("m_n"));
    assert_eq!(question_answer.target, reply_to("m_q"));
    assert_eq!(
        note_answer.markdown,
        "[senator::alice/claude] 收到，已按 30 天"
    );
    assert_eq!(question_answer.markdown, "[senator::alice/claude] 已经改了");
    assert_eq!(note_answer.thread_ref, note_ack.thread_ref);
    assert_eq!(question_answer.thread_ref, question_ack.thread_ref);
    assert!(
        rig.brain.questions().is_empty(),
        "nothing owed: nothing answered on her behalf"
    );
}

#[tokio::test]
async fn m10_a_panicked_speaker_leaves_no_pump_behind() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let surface = FakeSurface::new();
    let brain = ScriptedBrain::new();
    let (tx, rx) = mpsc::channel(16);
    let args = SpeakerArgs::new(hub.clone(), surface.clone(), brain.clone(), config(), rx);
    let rig = Rig::spawn(
        hub.clone(),
        surface.clone(),
        brain.clone(),
        args.clone(),
        tx.clone(),
    )
    .await;

    // A bug on the posting path kills the actor; ractor skips post_stop.
    surface.panic_next();
    rig.append(question(agent("alice"), agent("bob"), "还在吗？"))
        .await;
    tokio::time::timeout(Duration::from_secs(5), rig.join)
        .await
        .expect("the speaker dies")
        .expect("joined");

    // Its successor, on the same arguments, gets every unbound mention.
    let rig = Rig::spawn(hub, surface.clone(), brain, args, tx).await;
    rig.mention_unbound(unbound("pm", "/status", "m_u1")).await;
    let texts = surface.texts();
    assert!(
        texts.iter().any(|t| t.contains("还在吗？")),
        "the question was mirrored on restart: {texts:?}"
    );
    assert_eq!(
        texts.last().map(String::as_str),
        Some("本群还没有绑定委员会。用 /bind <committee> 绑定。"),
        "the mention reached the live Speaker: {texts:?}"
    );
}

#[tokio::test]
async fn m12_only_the_requester_confirms_or_cancels() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain.interprets(Ok(Intent::Unbind));
    let ask = rig
        .append(mention("pm", "这个群不用了，解绑吧", "m_h1"))
        .await;
    rig.settle().await;
    let restated = rig.surface.posts()[0].clone();

    // Others in the thread neither confirm nor cancel.
    rig.append(in_thread(
        "bob",
        "对",
        "m_h2",
        ask.seq,
        restated.thread_ref.clone(),
        &restated.message_ref,
    ))
    .await;
    let mut cancel = in_thread(
        "pm",
        "取消",
        "m_h3",
        ask.seq,
        restated.thread_ref.clone(),
        &restated.message_ref,
    );
    cancel.from = guest(STRANGER);
    rig.append(cancel).await;
    rig.settle().await;
    assert_eq!(rig.surface.posts().len(), 1, "nothing said to them");
    assert_eq!(kinds(&rig.hub.events(&refund()), &Kind::Binding), 1);

    // The one who asked does.
    rig.append(in_thread(
        "pm",
        "对",
        "m_h4",
        ask.seq,
        restated.thread_ref.clone(),
        &restated.message_ref,
    ))
    .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let unbind = events.last().and_then(Event::binding).expect("a binding");
    assert_eq!(unbind.action, BindingAction::Unbind);
    assert_eq!(
        events.last().map(|e| e.on_behalf_of.clone()),
        Some(Some(user("pm")))
    );
    let last = rig.surface.posts().pop().expect("a reply");
    assert_eq!(last.target, reply_to("m_h4"));

    // Unbound groups: the same rule.
    rig.brain.interprets(Ok(Intent::Bind {
        committee: "checkout".into(),
    }));
    let ask = unbound("pm", "把这个群绑定到 checkout", "m_m1");
    rig.mention_unbound(ask.clone()).await;
    let thread = rig.surface.posts().pop().and_then(|p| p.thread_ref);
    let in_that_thread = |from: &str, text: &str, message_ref: &str| UnboundMention {
        message_ref: message_ref.into(),
        thread_ref: thread.clone(),
        thread_root_message_ref: Some("m_m1".into()),
        addressing: "ambient".into(),
        sender: user(from),
        text: text.into(),
        ..ask.clone()
    };
    let posted = rig.surface.posts().len();
    rig.mention_unbound(in_that_thread("bob", "对", "m_m2"))
        .await;
    rig.mention_unbound(in_that_thread("alice", "算了", "m_m3"))
        .await;
    assert_eq!(rig.surface.posts().len(), posted, "ignored");
    assert!(rig.hub.events(&committee("checkout")).is_empty());
    rig.mention_unbound(in_that_thread("pm", "对", "m_m4"))
        .await;
    let texts = rig.surface.texts();
    assert_eq!(
        texts.iter().rev().nth(1).map(String::as_str),
        Some("已绑定到委员会 checkout。")
    );
    assert!(
        texts
            .last()
            .is_some_and(|t| t.starts_with("还没有 Agent 群")),
        "the first binding of checkout says how to get an Agent group: {texts:?}"
    );
    assert_eq!(rig.hub.events(&committee("checkout")).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn m13_a_slow_interpretation_holds_up_only_its_own_committee() {
    let checkout = committee("checkout");
    let hub = bound_hub().await;
    hub.append(bind_event(&checkout, "frontend"))
        .await
        .expect("bind");
    hub.connect(&refund(), &agent_path("bob"));
    hub.connect(&refund(), &agent_path("alice"));
    hub.connect(&checkout, &agent_path("bob"));
    let rig = Rig::start(hub).await;
    rig.brain.interprets_after(
        Duration::from_secs(40),
        Ok(Intent::Relay {
            targets: vec!["alice".into()],
            kind: RelayKind::Note,
            body: "退款上限是 30 天".into(),
        }),
    );
    let started = tokio::time::Instant::now();

    // refund-v2: a chat to think about, then an agent question after it.
    rig.append(mention(
        "pm",
        "告诉 alice 的 agent，退款上限是 30 天",
        "m_h1",
    ))
    .await;
    rig.append(question(agent("alice"), agent("bob"), "退款这边的问题"))
        .await;
    // checkout: an agent question, meanwhile.
    rig.append(
        NewEvent::new(
            checkout.clone(),
            Kind::Question,
            agent("alice"),
            "结账那边的问题",
        )
        .to(agent("bob")),
    )
    .await;

    let surface = rig.surface.clone();
    assert!(
        spin_until(|| surface.texts().iter().any(|t| t.contains("结账那边的问题"))).await,
        "checkout was mirrored while refund-v2 was thinking"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(rig.brain.utterances().len(), 1, "the call is under way");
    assert_eq!(
        rig.surface.texts().len(),
        1,
        "refund-v2 waits at its chat: nothing after it is mirrored yet"
    );

    rig.settle().await;
    assert!(started.elapsed() >= Duration::from_secs(40));
    let texts = rig.surface.texts();
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert!(
        texts[1].contains("退款这边的问题"),
        "seq order kept: {texts:?}"
    );
    assert!(
        texts[2].starts_with("已转给 [senator::alice/claude]"),
        "{texts:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn m13_at_most_four_brain_calls_run_at_once() {
    let hub = MemoryHub::new();
    let groups: Vec<(CommitteeId, String)> = (1..=5)
        .map(|n| (committee(&format!("f{n}")), format!("g{n}")))
        .collect();
    for (committee_id, group) in &groups {
        hub.append(bind_event(committee_id, group))
            .await
            .expect("bind");
    }
    let rig = Rig::start(hub).await;
    for (n, (committee_id, group)) in groups.iter().enumerate() {
        rig.brain.interprets_after(
            Duration::from_secs(10),
            Ok(Intent::None {
                reply: format!("好的 {n}"),
            }),
        );
        rig.append(mention_in(
            committee_id,
            group,
            "pm",
            "在吗",
            &format!("m_{n}"),
        ))
        .await;
    }
    let brain = rig.brain.clone();
    assert!(spin_until(|| brain.utterances().len() >= 4).await);
    spin_until(|| false).await;
    assert_eq!(rig.brain.utterances().len(), 4, "the fifth waits its turn");

    rig.settle().await;
    assert_eq!(rig.brain.utterances().len(), 5);
    assert_eq!(rig.surface.posts().len(), 5);
}
