//! Decisions said in chat or proposed by agents, the brief as a reply and
//! its Speaker-written top, and the mirror group with several bound groups.

use std::sync::Arc;
use std::time::Duration;

use senate_core::{
    Address, BindingAction, BindingPayload, Event, Hub, Kind, Locale, NewEvent, PROPOSAL_DECISION,
    PostTarget, Seq, meta,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::{
    GROUP, Rig, STRANGER, agent, agent_path, answer, assert_senate_vocabulary, bind_event,
    config_in, in_thread, mention, mention_in, question, refund, source, user,
};
use super::surface::{FakeSurface, PLATFORM};
use crate::{BrainError, Intent, ScriptedBrain, render};

const PRODUCT: &str = "product";

async fn bound_hub() -> Arc<MemoryHub> {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub
}

fn reply_to(message_ref: &str) -> PostTarget {
    PostTarget::Reply {
        message_ref: message_ref.into(),
        in_thread: true,
    }
}

fn root_in(group: &str) -> PostTarget {
    PostTarget::NewRoot {
        group_id: group.into(),
    }
}

fn decisions(events: &[Event]) -> Vec<Event> {
    events
        .iter()
        .filter(|e| e.kind == Kind::Decision)
        .cloned()
        .collect()
}

fn proposal(from: &str, body: &str) -> NewEvent {
    NewEvent::new(refund(), Kind::Note, agent(from), body)
        .with_meta(meta::PROPOSAL, json!(PROPOSAL_DECISION))
}

fn unbind_event(group: &str) -> NewEvent {
    NewEvent::new(refund(), Kind::Binding, Address::Speaker, "unbind").with_binding(
        &BindingPayload {
            action: BindingAction::Unbind,
            platform: PLATFORM.into(),
            group_id: group.into(),
            group_name: None,
            role: senate_core::GroupRole::People,
        },
    )
}

#[tokio::test]
async fn a_decision_said_to_the_speaker_is_restated_confirmed_and_acknowledged() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain.interprets(Ok(Intent::Decide {
        body: "退款上限 30 天".into(),
        supersedes: None,
    }));
    let ask = rig
        .append(mention("pm", "记一下：退款上限 30 天", "m_h1"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].target, reply_to("m_h1"));
    assert_eq!(
        posts[0].markdown,
        "我理解为：记录决策「退款上限 30 天」。回复「对」确认"
    );
    assert!(decisions(&rig.hub.events(&refund())).is_empty());

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

    // The decision, written by the Speaker on behalf of the person.
    let recorded = decisions(&rig.hub.events(&refund()));
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    let decision = &recorded[0];
    assert_eq!(decision.from, Address::Speaker);
    assert_eq!(decision.on_behalf_of, Some(user("pm")));
    assert_eq!(decision.body, "退款上限 30 天");
    assert_eq!(decision.thread, None);
    assert_eq!(decision.source, Some(source()));
    assert_eq!(
        decision.meta.get(meta::RELAYED_FROM),
        Some(&json!(ask.seq.0))
    );
    assert_eq!(decision.meta.get(meta::SUPERSEDES), None);

    // Acknowledged once, in the speaker's thread; nothing else is said.
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert_eq!(posts[1].target, reply_to("m_h1"));
    assert_eq!(
        posts[1].markdown,
        format!("已记录决策（seq {}）", decision.seq)
    );
    assert_eq!(posts[1].thread_ref, posts[0].thread_ref);
    let brief = rig.hub.brief(&refund()).await.expect("brief");
    assert_eq!(brief.decisions.len(), 1);
    assert_eq!(brief.decisions[0].seq, decision.seq);
    assert_eq!(brief.decisions[0].by, user("pm"));

    // Changing it names the decision it replaces; a seq that is not a
    // decision is refused.
    rig.brain
        .interprets(Ok(Intent::Decide {
            body: "退款上限改成 14 天".into(),
            supersedes: Some(ask.seq),
        }))
        .interprets(Ok(Intent::Decide {
            body: "退款上限改成 14 天".into(),
            supersedes: Some(decision.seq),
        }));
    rig.append(mention("pm", "记一下：上限改成 14 天", "m_h3"))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(format!("seq {} 不是本委员会的一条决策，没法取代它。", ask.seq).as_str())
    );
    let change = rig
        .append(mention(
            "pm",
            "记一下：上限改成 14 天，取代刚才那条",
            "m_h4",
        ))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(
            format!(
                "我理解为：记录决策「退款上限改成 14 天」，取代 seq {} 的决策。回复「对」确认",
                decision.seq
            )
            .as_str()
        )
    );
    let restated = rig.surface.posts().last().cloned().expect("restatement");
    rig.append(in_thread(
        "pm",
        "确认！",
        "m_h5",
        change.seq,
        restated.thread_ref.clone(),
        &restated.message_ref,
    ))
    .await;
    rig.settle().await;
    let recorded = decisions(&rig.hub.events(&refund()));
    assert_eq!(recorded.len(), 2);
    assert_eq!(
        recorded[1].meta.get(meta::SUPERSEDES),
        Some(&json!(decision.seq.0))
    );
    let brief = rig.hub.brief(&refund()).await.expect("brief");
    assert_eq!(brief.decisions.len(), 1, "the old one is superseded");
    assert_eq!(brief.decisions[0].body, "退款上限改成 14 天");
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

/// An English committee: the restatement and the ack in English, and any
/// yes-word confirms, whichever language it is in.
#[tokio::test]
async fn in_english_a_decision_is_restated_and_any_yes_word_confirms_it() {
    let rig = Rig::start_config(
        bound_hub().await,
        FakeSurface::new(),
        ScriptedBrain::new(),
        config_in(Locale::En),
    )
    .await;
    for (i, (body, yes)) in [
        ("Refund limit is 30 days", "yes"),
        ("Retries at most 3 times", "ok"),
        ("Idempotency key is Idempotency-Key", "对"),
    ]
    .into_iter()
    .enumerate()
    {
        rig.brain.interprets(Ok(Intent::Decide {
            body: body.into(),
            supersedes: None,
        }));
        let asked_ref = format!("m_e{i}");
        let ask = rig
            .append(mention("pm", &format!("note: {body}"), &asked_ref))
            .await;
        rig.settle().await;
        let restated = rig.surface.posts().last().cloned().expect("restatement");
        assert_eq!(restated.target, reply_to(&asked_ref));
        assert_eq!(
            restated.markdown,
            format!("I understand: Record the decision \"{body}\". Reply \"yes\" to confirm.")
        );
        assert_eq!(decisions(&rig.hub.events(&refund())).len(), i);

        rig.append(in_thread(
            "pm",
            yes,
            &format!("m_y{i}"),
            ask.seq,
            restated.thread_ref.clone(),
            &restated.message_ref,
        ))
        .await;
        rig.settle().await;
        let recorded = decisions(&rig.hub.events(&refund()));
        assert_eq!(recorded.len(), i + 1, "{yes:?} confirms");
        let decision = recorded.last().expect("decision");
        assert_eq!(decision.body, body);
        assert_eq!(decision.on_behalf_of, Some(user("pm")));
        assert_eq!(
            rig.surface.texts().last().map(String::as_str),
            Some(format!("Decision recorded (seq {})", decision.seq).as_str())
        );
    }

    // A guest's /decide is refused, in English.
    rig.append(mention(STRANGER, "/decide limit 90 days", "m_x1"))
        .await;
    rig.settle().await;
    assert_eq!(decisions(&rig.hub.events(&refund())).len(), 3);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(
            "Only people on the roster can have me change anything: bindings, decisions, invitations, the output level, work orders, reassignment. You can still ask me questions."
        )
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

#[tokio::test]
async fn slash_decide_records_directly_and_only_for_members() {
    let hub = bound_hub().await;
    let surface = FakeSurface::new();
    let brain = ScriptedBrain::new();
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    let ask = rig
        .append(mention(
            "pm",
            "/decide 幂等键统一用 Idempotency-Key",
            "m_h1",
        ))
        .await;
    rig.settle().await;
    let recorded = decisions(&hub.events(&refund()));
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body, "幂等键统一用 Idempotency-Key");
    assert_eq!(recorded[0].on_behalf_of, Some(user("pm")));
    assert_eq!(
        recorded[0].meta.get(meta::RELAYED_FROM),
        Some(&json!(ask.seq.0))
    );
    let posts = surface.posts();
    assert_eq!(posts.len(), 1, "no restatement: {posts:#?}");
    assert_eq!(posts[0].target, reply_to("m_h1"));
    assert_eq!(
        posts[0].markdown,
        format!("已记录决策（seq {}）", recorded[0].seq)
    );
    assert!(
        brain.utterances().is_empty(),
        "the fixed syntax needs no model"
    );

    // A guest gets the one refusal.
    rig.append(mention(STRANGER, "/decide 上限 90 天", "m_x1"))
        .await;
    rig.settle().await;
    assert_eq!(decisions(&hub.events(&refund())).len(), 1);
    assert_eq!(
        surface.texts().last().map(String::as_str),
        Some(
            "只有名册上的人可以让我改动东西：绑定、决策、邀请、输出等级、代码任务、改派。你仍然可以问我问题。"
        )
    );
    assert_eq!(
        surface.texts().last().cloned(),
        Some(render::guest_refused(Locale::Zh))
    );
    rig.append(mention(
        "pm",
        &format!("/decide --supersedes {} 新的决定", recorded[0].seq),
        "m_replace",
    ))
    .await;
    rig.settle().await;
    let history = decisions(&hub.events(&refund()));
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].meta[meta::SUPERSEDES], json!(recorded[0].seq));
    let brief = hub.brief(&refund()).await.expect("brief");
    assert_eq!(brief.decisions.len(), 1);
    assert_eq!(brief.decisions[0].body, "新的决定");
    rig.stop().await;

    // A replay from before the command records and says nothing twice.
    let before = surface.posts();
    let events = hub.events(&refund()).len();
    hub.set_cursor(&refund(), crate::CURSOR, Seq(1))
        .await
        .expect("rewind");
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain).await;
    assert_eq!(hub.events(&refund()).len(), events);
    assert_eq!(surface.posts(), before);
    rig.stop().await;
}

#[tokio::test]
async fn recall_replies_with_the_brief_in_chinese() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let decided = hub
        .append(
            NewEvent::new(refund(), Kind::Decision, Address::Speaker, "退款上限 30 天")
                .on_behalf_of(user("pm")),
        )
        .await
        .expect("decision")
        .event;
    let asked = hub
        .append(question(
            agent("alice"),
            agent("bob"),
            "网关用哪个 header？",
        ))
        .await
        .expect("question")
        .event;
    let top = hub
        .append(
            NewEvent::new(refund(), Kind::Note, Address::Speaker, "1. 退款上限 30 天")
                .with_meta(meta::BRIEF_TOP, json!(true)),
        )
        .await
        .expect("top")
        .event;
    let rig = Rig::start(hub).await;

    rig.append(mention("pm", "/brief", "m_h1")).await;
    rig.settle().await;
    let said = rig.surface.posts().last().cloned().expect("a reply");
    assert_eq!(said.target, reply_to("m_h1"));
    let text = said.markdown;
    assert!(
        text.starts_with("委员会 refund-v2 的简报（截至 seq "),
        "{text}"
    );
    assert!(
        text.contains(&format!("当前重点（seq {}）：\n1. 退款上限 30 天", top.seq)),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "已定决策：\n- seq {} · [senator::pm] · ",
            decided.seq
        )),
        "{text}"
    );
    assert!(text.contains("：退款上限 30 天\n"), "{text}");
    assert!(
        text.contains(&format!(
            "待回答的问题：\n- seq {} · [senator::claude] → [senator::copilot]：网关用哪个 header？",
            asked.seq
        )),
        "{text}"
    );

    // Said in words, through the Brain; anyone in the group may ask, a
    // guest too.
    rig.brain.interprets(Ok(Intent::Recall));
    rig.append(mention(STRANGER, "现在定了哪些？", "m_h2"))
        .await;
    rig.settle().await;
    let said = rig.surface.posts().last().cloned().expect("a reply");
    assert_eq!(said.target, reply_to("m_h2"));
    assert!(said.markdown.contains("已定决策："), "{}", said.markdown);
}

#[tokio::test]
async fn an_agent_proposal_is_posted_and_a_members_yes_records_it_once() {
    let rig = Rig::start(bound_hub().await).await;
    let proposed = rig
        .append(proposal("alice", "幂等键统一用 Idempotency-Key"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].target, root_in(GROUP));
    assert_eq!(
        posts[0].markdown,
        format!(
            "[senator::claude] 提议记为决策：幂等键统一用 Idempotency-Key（seq {}）回复「对」或「是」确认",
            proposed.seq
        )
    );
    let copy = rig
        .hub
        .mirror_of_message(PLATFORM, &posts[0].message_ref)
        .await
        .expect("read")
        .expect("the root maps back to the proposal");
    assert_eq!(copy.seq, proposed.seq);
    let root_ref = posts[0].message_ref.clone();
    let thread_ref = Some(format!("t_{root_ref}"));

    // Talk in the thread is just talk; a guest's 「对」 or "yes" decides
    // nothing.
    for (who, text, message_ref) in [
        ("bob", "再想想，X-Request-Id 也行", "m_t1"),
        (STRANGER, "对", "m_t2"),
        (STRANGER, "yes", "m_t2b"),
    ] {
        rig.append(
            in_thread(
                who,
                text,
                message_ref,
                proposed.seq,
                thread_ref.clone(),
                &root_ref,
            )
            .to(agent("alice")),
        )
        .await;
    }
    rig.settle().await;
    assert!(decisions(&rig.hub.events(&refund())).is_empty());
    assert_eq!(rig.surface.posts().len(), 1, "nothing said");

    let yes = rig
        .append(
            in_thread(
                "pm",
                "是",
                "m_t3",
                proposed.seq,
                thread_ref.clone(),
                &root_ref,
            )
            .to(agent("alice")),
        )
        .await;
    rig.settle().await;
    let recorded = decisions(&rig.hub.events(&refund()));
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    let decision = &recorded[0];
    assert_eq!(decision.from, Address::Speaker);
    assert_eq!(decision.on_behalf_of, Some(user("pm")));
    assert_eq!(decision.thread, Some(proposed.seq));
    assert_eq!(decision.body, "幂等键统一用 Idempotency-Key");
    assert_eq!(
        decision.meta.get(meta::PROPOSED_BY),
        Some(&json!("senator::claude"))
    );
    assert_eq!(
        decision.meta.get(meta::RELAYED_FROM),
        Some(&json!(yes.seq.0))
    );
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert_eq!(posts[1].target, reply_to(&root_ref));
    assert_eq!(
        posts[1].markdown,
        format!("已记录决策（seq {}）", decision.seq)
    );
    assert_eq!(posts[1].thread_ref, thread_ref, "in the proposal's thread");

    // The proposal is closed: a second yes is ordinary chat.
    rig.append(
        in_thread("bob", "ok", "m_t4", proposed.seq, thread_ref, &root_ref).to(agent("alice")),
    )
    .await;
    rig.settle().await;
    assert_eq!(decisions(&rig.hub.events(&refund())).len(), 1);
    assert_eq!(rig.surface.posts().len(), 2);
    let brief = rig.hub.brief(&refund()).await.expect("brief");
    assert_eq!(brief.decisions[0].seq, decision.seq);
}

#[tokio::test]
async fn a_proposal_can_be_confirmed_after_a_restart() {
    let hub = bound_hub().await;
    let surface = FakeSurface::new();
    let brain = ScriptedBrain::new();
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    let proposed = rig.append(proposal("alice", "重试最多 3 次")).await;
    rig.settle().await;
    rig.stop().await;
    let root = surface.posts()[0].clone();

    // Nothing about the proposal is held in memory: a new Speaker, much
    // later, still takes the yes.
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain).await;
    rig.append(
        in_thread(
            "bob",
            "是",
            "m_t1",
            proposed.seq,
            root.thread_ref.clone(),
            &root.message_ref,
        )
        .to(agent("alice")),
    )
    .await;
    rig.settle().await;
    let recorded = decisions(&hub.events(&refund()));
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].thread, Some(proposed.seq));
    assert_eq!(recorded[0].on_behalf_of, Some(user("bob")));
    assert_eq!(
        surface.texts().last().map(String::as_str),
        Some(format!("已记录决策（seq {}）", recorded[0].seq).as_str())
    );
    rig.stop().await;
}

#[tokio::test]
async fn in_english_a_proposal_is_posted_and_a_members_yes_records_it() {
    let rig = Rig::start_config(
        bound_hub().await,
        FakeSurface::new(),
        ScriptedBrain::new(),
        config_in(Locale::En),
    )
    .await;
    let proposed = rig
        .append(proposal("alice", "Idempotency key is Idempotency-Key"))
        .await;
    rig.settle().await;
    let root = rig.surface.posts()[0].clone();
    assert_eq!(root.target, root_in(GROUP));
    assert_eq!(
        root.markdown,
        format!(
            "[senator::claude] proposes a decision: Idempotency key is Idempotency-Key (seq {}). Reply \"yes\" to confirm.",
            proposed.seq
        )
    );
    let thread_ref = Some(format!("t_{}", root.message_ref));

    // A guest's yes decides nothing.
    rig.append(
        in_thread(
            STRANGER,
            "yes",
            "m_t1",
            proposed.seq,
            thread_ref.clone(),
            &root.message_ref,
        )
        .to(agent("alice")),
    )
    .await;
    rig.settle().await;
    assert!(decisions(&rig.hub.events(&refund())).is_empty());
    assert_eq!(rig.surface.posts().len(), 1, "nothing said");

    rig.append(
        in_thread(
            "bob",
            "OK",
            "m_t2",
            proposed.seq,
            thread_ref.clone(),
            &root.message_ref,
        )
        .to(agent("alice")),
    )
    .await;
    rig.settle().await;
    let recorded = decisions(&rig.hub.events(&refund()));
    assert_eq!(recorded.len(), 1, "{recorded:#?}");
    assert_eq!(recorded[0].on_behalf_of, Some(user("bob")));
    assert_eq!(recorded[0].thread, Some(proposed.seq));
    let ack = rig.surface.posts().last().cloned().expect("ack");
    assert_eq!(ack.target, reply_to(&root.message_ref));
    assert_eq!(ack.thread_ref, thread_ref);
    assert_eq!(
        ack.markdown,
        format!("Decision recorded (seq {})", recorded[0].seq)
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

#[tokio::test(start_paused = true)]
async fn the_brief_top_is_written_once_per_debounce_and_skipped_without_a_model() {
    let hub = bound_hub().await;
    let surface = FakeSurface::new();
    let brain = ScriptedBrain::new();
    brain.writes_top(Ok(
        "退款上限 30 天（seq 2）\n\n幂等键用 Idempotency-Key（seq 4）\n第三行\n第四行不要".into(),
    ));
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;

    // Two decisions ten seconds apart: one rewrite, 30 s after the first.
    rig.append(mention("pm", "/decide 退款上限 30 天", "m_h1"))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(10)).await;
    rig.append(mention("pm", "/decide 幂等键用 Idempotency-Key", "m_h2"))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(19)).await;
    rig.settle().await;
    assert!(brain.briefs().is_empty(), "too early");
    tokio::time::advance(Duration::from_secs(2)).await;
    rig.settle().await;
    let briefs = brain.briefs();
    assert_eq!(briefs.len(), 1);
    assert_eq!(briefs[0].decisions.len(), 2, "both decisions in one brief");

    let events = hub.events(&refund());
    let top = events.last().expect("the top");
    assert_eq!(top.kind, Kind::Note);
    assert_eq!(top.from, Address::Speaker);
    assert!(top.to.is_empty());
    assert_eq!(top.meta.get(meta::BRIEF_TOP), Some(&json!(true)));
    assert_eq!(
        top.body,
        "退款上限 30 天（seq 2）\n幂等键用 Idempotency-Key（seq 4）\n第三行"
    );
    let brief = hub.brief(&refund()).await.expect("brief");
    assert_eq!(brief.top.map(|t| t.seq), Some(top.seq));
    let said = surface.texts();
    assert_eq!(said.len(), 2, "two acks, the top unsaid: {said:?}");
    rig.settle().await;
    rig.stop().await;

    // A replay of those decisions does not rewrite the top again.
    hub.set_cursor(&refund(), crate::CURSOR, Seq(1))
        .await
        .expect("rewind");
    let rig = Rig::start_with(hub.clone(), surface.clone(), brain.clone()).await;
    tokio::time::advance(Duration::from_secs(31)).await;
    rig.settle().await;
    assert_eq!(brain.briefs().len(), 1, "no refresh: the top is current");

    // The model is down: no top, nothing said, nothing stuck.
    brain.writes_top(Err(BrainError::Unavailable("provider 503".into())));
    rig.append(mention("pm", "/decide 重试最多 3 次", "m_h3"))
        .await;
    rig.settle().await;
    let events = hub.events(&refund()).len();
    tokio::time::advance(Duration::from_secs(31)).await;
    rig.settle().await;
    assert_eq!(brain.briefs().len(), 2, "it was asked");
    assert_eq!(hub.events(&refund()).len(), events, "and nothing appended");
    assert_eq!(surface.texts().len(), 3);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_focus_rebuild_cannot_restore_a_corrected_claim_from_its_decision() {
    let hub = bound_hub().await;
    let brain = ScriptedBrain::new();
    let rig = Rig::start_with(hub.clone(), FakeSurface::new(), brain.clone()).await;
    let decision = rig
        .append(NewEvent::new(
            refund(),
            Kind::Decision,
            user("pm"),
            "Use the flash model",
        ))
        .await;
    let stale = format!("Speaker uses flash (seq {})", decision.seq);
    let focus = rig
        .append(
            NewEvent::new(refund(), Kind::Note, Address::Speaker, &stale)
                .with_meta(meta::BRIEF_TOP, json!(true)),
        )
        .await;
    rig.append(mention(
        "pm",
        &format!("/correct {} Speaker uses pro", focus.seq),
        "m_correct",
    ))
    .await;
    rig.settle().await;
    let corrected = hub.brief(&refund()).await.unwrap().top.unwrap();
    assert_eq!(corrected.body, "Speaker uses pro");

    for (message, draft, expected) in [
        ("m_next", "Speaker uses pro", "Speaker uses pro"),
        ("m_again", stale.as_str(), "Speaker uses pro"),
    ] {
        brain.writes_top(Ok(draft.to_owned()));
        rig.append(mention("pm", "/decide Keep the weekly release", message))
            .await;
        rig.settle().await;
        tokio::time::advance(Duration::from_secs(31)).await;
        rig.settle().await;
        let brief = hub.brief(&refund()).await.unwrap();
        assert!(brief.decisions.iter().any(|d| d.seq == decision.seq));
        assert_eq!(brief.top.unwrap().body, expected);
    }
    assert_eq!(brain.briefs().len(), 2);
    for edits in brain.top_edits() {
        assert!(edits.contains(&stale));
        assert!(edits.contains("Corrected wording: Speaker uses pro"));
    }
    let generated = hub
        .events(&refund())
        .into_iter()
        .filter(|e| e.seq > corrected.seq && e.meta.contains_key(meta::BRIEF_TOP))
        .collect::<Vec<_>>();
    assert_eq!(
        generated.len(),
        1,
        "the valid rebuild lands, the stale one does not"
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_brief_top_is_never_mirrored() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "用哪个 header？"))
        .await;
    rig.settle().await;
    assert_eq!(rig.surface.posts().len(), 1);

    // Not as a root, and not even inside a mirrored thread.
    for top in [
        NewEvent::new(refund(), Kind::Note, Address::Speaker, "1. 上限 30 天"),
        NewEvent::new(refund(), Kind::Note, Address::Speaker, "1. 上限 30 天")
            .in_thread(asked.seq)
            .to(agent("alice")),
    ] {
        rig.append(top.with_meta(meta::BRIEF_TOP, json!(true)))
            .await;
    }
    rig.append(answer(
        agent("bob"),
        asked.seq,
        agent("alice"),
        "Idempotency-Key",
    ))
    .await;
    rig.settle().await;
    let said = rig.surface.texts();
    assert_eq!(said.len(), 2, "{said:?}");
    assert_eq!(said[1], "[senator::copilot] Idempotency-Key");
}

#[tokio::test]
async fn with_two_groups_agents_talk_in_the_first_still_bound_and_people_hear_back_where_they_asked()
 {
    let hub = bound_hub().await;
    hub.append(bind_event(&refund(), PRODUCT))
        .await
        .expect("bind product");
    hub.connect(&refund(), &agent_path("alice"));
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;

    // Agent to agent: the first-bound group.
    rig.append(question(agent("alice"), agent("bob"), "一号问题"))
        .await;
    rig.settle().await;
    assert_eq!(rig.surface.posts()[0].target, root_in(GROUP));

    // A person in the second group: the ack and the answer go back there.
    rig.append(mention_in(
        &refund(),
        PRODUCT,
        "pm",
        "/tell alice 退款上限多少天？",
        "m_p1",
    ))
    .await;
    rig.settle().await;
    let relay = rig.hub.events(&refund()).into_iter().last().expect("relay");
    assert_eq!(relay.kind, Kind::Question);
    rig.append(answer(agent("alice"), relay.seq, user("pm"), "30 天"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 3, "{posts:#?}");
    assert_eq!(posts[1].target, reply_to("m_p1"));
    assert_eq!(posts[2].target, reply_to("m_p1"));
    assert_eq!(posts[2].markdown, "[senator::claude] 30 天");
    let copies = rig
        .hub
        .mirrors_of_seq(&refund(), relay.seq, PLATFORM)
        .await
        .expect("read");
    assert_eq!(copies[0].target, PRODUCT);

    // The first group unbinds: agent traffic and proposals move to the
    // one still bound, and stay there when the first comes back.
    rig.append(unbind_event(GROUP)).await;
    rig.append(question(agent("bob"), agent("alice"), "二号问题"))
        .await;
    rig.append(proposal("bob", "重试最多 3 次")).await;
    rig.append(bind_event(&refund(), GROUP)).await;
    rig.append(question(agent("alice"), agent("bob"), "三号问题"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 6, "{posts:#?}");
    for post in &posts[3..] {
        assert_eq!(post.target, root_in(PRODUCT), "{}", post.markdown);
    }
}
