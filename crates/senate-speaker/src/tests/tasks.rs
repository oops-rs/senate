//! Task threads: triage, a thread parked on its person, approval (the
//! hand-off to one of the person's agents), a person stepping in,
//! `escalate`, and the signature on answers.

use std::sync::Arc;
use std::time::Duration;

use senate_core::{
    Address, Event, Hub, Kind, Locale, NewEvent, PostTarget, Seq, StatusPayload, StatusState,
    TriagePayload, TriageVerdict, meta,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::{
    GROUP, Rig, STRANGER, agent, agent_named, agent_path, agents_bind_event, answer,
    assert_senate_vocabulary, bind_event, config_in, in_thread, mention, name, path, question,
    refund, roster, user,
};
use super::surface::{Caps, FakeSurface};
use crate::{BrainError, Intent, RelayKind, ScriptedBrain, render};

pub(crate) const AGENTS: &str = "agents";

async fn bound_hub() -> Arc<MemoryHub> {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub
}

/// A people group and an adopted Agent group.
pub(crate) async fn agents_hub() -> Arc<MemoryHub> {
    let hub = bound_hub().await;
    hub.append(agents_bind_event(&refund(), AGENTS))
        .await
        .expect("bind agents");
    hub
}

fn reply_to(message_ref: &str) -> PostTarget {
    PostTarget::Reply {
        message_ref: message_ref.into(),
        in_thread: true,
    }
}

fn request(from: Address, to: Address, body: &str) -> NewEvent {
    NewEvent::new(refund(), Kind::Request, from, body).to(to)
}

fn status(from: Address, root: Seq, status: &StatusPayload) -> NewEvent {
    NewEvent::new(refund(), Kind::Status, from, "")
        .in_thread(root)
        .with_meta(meta::STATUS, status.value())
}

/// The Speaker's verdict notes in thread `root`.
fn verdicts(events: &[Event], root: Seq) -> Vec<TriagePayload> {
    events
        .iter()
        .filter(|e| e.thread == Some(root) && e.from == Address::Speaker && e.kind == Kind::Note)
        .filter_map(TriagePayload::of)
        .collect()
}

fn statuses(events: &[Event], root: Seq) -> Vec<(Address, StatusPayload)> {
    events
        .iter()
        .filter(|e| e.thread == Some(root))
        .filter_map(|e| StatusPayload::of(e).map(|s| (e.from.clone(), s)))
        .collect()
}

/// The approval a person's hand-off appended in thread `root` (the root
/// again, as their own task), if any.
fn approval(events: &[Event], root: Seq) -> Option<Event> {
    events
        .iter()
        .find(|e| {
            matches!(e.kind, Kind::Request | Kind::Question)
                && e.thread == Some(root)
                && TriagePayload::of(e).and_then(|t| t.rule).as_deref() == Some("approved")
        })
        .cloned()
}

// ----- 1. triage -------------------------------------------------------------

#[tokio::test]
async fn a_root_question_gets_one_triage_note_with_its_probabilities() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    rig.brain.triages(Ok(TriagePayload {
        verdict: TriageVerdict::Unsure, // recomputed from the probabilities
        p_needs_human: 0.1,
        p_agent: 0.8,
        p_unsure: 0.1,
        rule: None,
        reason: Some("代码里能查到".into()),
    }));
    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "网关幂等用哪个 header？",
        ))
        .await;
    rig.settle().await;

    let events = rig.hub.events(&refund());
    let notes = verdicts(&events, asked.seq);
    assert_eq!(notes.len(), 1, "{events:#?}");
    assert_eq!(notes[0].verdict, TriageVerdict::Agent);
    assert!((notes[0].p_agent - 0.8).abs() < 1e-6);
    let note = events.iter().find(|e| e.kind == Kind::Note).expect("note");
    assert!(note.body.starts_with("分流：交给 agent"), "{}", note.body);
    assert!(statuses(&events, asked.seq).is_empty(), "nothing parked");

    let inputs = rig.brain.triage_inputs();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].seq, Some(asked.seq));
    assert_eq!(inputs[0].asker, agent("alice"));
    assert_eq!(inputs[0].to, vec![agent("bob")]);
    // Still mirrored as before (no cards on this surface).
    assert!(
        rig.surface.texts()[0].starts_with("[senator::claude] → [senator::copilot]"),
        "{:?}",
        rig.surface.texts()
    );
    assert_eq!(rig.cursor(&refund()).await, note.seq);
}

#[tokio::test]
async fn every_request_goes_to_the_person_without_asking_the_model() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(request(
            agent("bob"),
            agent("alice"),
            "把网关 header 改成 Idempotency-Key",
        ))
        .await;
    rig.settle().await;

    assert!(rig.brain.triage_inputs().is_empty(), "a hard rule");
    let events = rig.hub.events(&refund());
    let notes = verdicts(&events, asked.seq);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].verdict, TriageVerdict::NeedsHuman);
    assert_eq!(notes[0].rule.as_deref(), Some("request"));
    assert_eq!(notes[0].p_needs_human, 0.0, "a rule has no probabilities");

    // 2. Parked on alice, the person behind the agent: suspended, waiting
    // on her, then the mention.
    let parked = statuses(&events, asked.seq);
    assert_eq!(parked.len(), 1);
    let (from, s) = &parked[0];
    assert_eq!(*from, Address::Speaker);
    assert_eq!(s.state, StatusState::Suspended);
    assert_eq!(s.escalated_to, Some(name("alice")));
    assert_eq!(s.takeover, None);

    let posts = rig.surface.posts();
    assert_eq!(posts.len(), 2, "{posts:#?}");
    assert_eq!(posts[1].target, reply_to(&posts[0].message_ref));
    assert_eq!(
        posts[1].markdown,
        "@alice 这条需要你本人：改动请求要先经过本人（规则 request）。回复「交给我的 agent」或直接回答"
    );
    assert_eq!(posts[1].mention, None, "no native mentions here");
    assert!(rig.brain.questions().is_empty());
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

#[tokio::test]
async fn a_persons_own_request_to_their_own_agent_is_not_parked() {
    let rig = Rig::start(bound_hub().await).await;
    let asked = rig
        .append(request(user("alice"), agent("alice"), "把 header 改了"))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let notes = verdicts(&events, asked.seq);
    assert_eq!(notes[0].verdict, TriageVerdict::Agent);
    assert_eq!(notes[0].rule.as_deref(), Some("own_agent"));
    assert!(statuses(&events, asked.seq).is_empty());
    assert!(rig.brain.triage_inputs().is_empty());
}

#[tokio::test]
async fn own_agent_holds_for_any_of_the_persons_agents_and_not_for_someone_elses() {
    let hub = bound_hub().await;
    hub.seat(&path("codex"), "alice");
    let rig = Rig::start(hub).await;

    // alice → codex: one of hers, though not `claude`.
    let own = rig
        .append(request(
            user("alice"),
            agent_named("alice", "codex"),
            "把 header 改了",
        ))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let notes = verdicts(&events, own.seq);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].verdict, TriageVerdict::Agent);
    assert_eq!(notes[0].rule.as_deref(), Some("own_agent"));
    assert!(statuses(&events, own.seq).is_empty());

    // alice → copilot: someone else's agent, so the request rule.
    let other = rig
        .append(request(user("alice"), agent("bob"), "把 header 改了"))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let notes = verdicts(&events, other.seq);
    assert_eq!(notes[0].verdict, TriageVerdict::NeedsHuman);
    assert_eq!(notes[0].rule.as_deref(), Some("request"));
    let parked = statuses(&events, other.seq);
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].1.escalated_to, Some(name("bob")));
    assert!(rig.brain.triage_inputs().is_empty());
}

#[tokio::test]
async fn a_brain_failure_is_unsure() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    rig.brain
        .triages(Err(BrainError::Unavailable("model down".into())));
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "还在吗？"))
        .await;
    rig.settle().await;
    let notes = verdicts(&rig.hub.events(&refund()), asked.seq);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].verdict, TriageVerdict::Unsure);
    assert!(statuses(&rig.hub.events(&refund()), asked.seq).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_slow_triage_is_given_up_on_after_its_deadline() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    rig.brain.triages_after(
        Duration::from_secs(600),
        Ok(crate::scripted_verdict(TriageVerdict::NeedsHuman, None)),
    );
    let started = tokio::time::Instant::now();
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "还在吗？"))
        .await;
    rig.settle().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(5) && elapsed < Duration::from_secs(10),
        "{elapsed:?}"
    );
    let notes = verdicts(&rig.hub.events(&refund()), asked.seq);
    assert_eq!(notes[0].verdict, TriageVerdict::Unsure);
}

#[tokio::test]
async fn a_relay_carries_its_verdict_inline() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: "网关幂等用哪个 header？".into(),
    }));
    rig.append(mention(
        "pm",
        "问一下 alice 的 agent，网关幂等用哪个 header",
        "m_h1",
    ))
    .await;
    rig.settle().await;

    let events = rig.hub.events(&refund());
    let relay = events
        .iter()
        .find(|e| e.kind == Kind::Question)
        .expect("relay");
    assert_eq!(
        TriagePayload::of(relay).map(|t| t.verdict),
        Some(TriageVerdict::Agent)
    );
    assert!(verdicts(&events, relay.seq).is_empty(), "no separate note");
    let inputs = rig.brain.triage_inputs();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].seq, None);
    assert_eq!(inputs[0].asker, user("pm"));
    assert_eq!(
        rig.surface.texts(),
        vec![format!("已转给 [senator::claude]（seq {}）", relay.seq)]
    );
}

#[tokio::test]
async fn a_relay_that_needs_its_person_is_still_relayed_and_says_so() {
    let rig = Rig::start(bound_hub().await).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: "下周能上线吗？".into(),
    }));
    rig.brain
        .judges(TriageVerdict::NeedsHuman, "上线时间要本人承诺");
    rig.append(mention("pm", "问一下 alice 的 agent 下周能上线吗", "m_h1"))
        .await;
    rig.settle().await;

    let events = rig.hub.events(&refund());
    let relay = events
        .iter()
        .find(|e| e.kind == Kind::Question)
        .expect("relay");
    let parked = statuses(&events, relay.seq);
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].1.escalated_to, Some(name("alice")));
    let texts = rig.surface.texts();
    assert_eq!(
        texts[0],
        format!(
            "已转给 [senator::claude]（seq {}）。Speaker 判断这条需要 [senator::alice] 本人处理，已提醒 [senator::alice]",
            relay.seq
        )
    );
    assert_eq!(
        texts[1],
        "@alice 这条需要你本人：上线时间要本人承诺。回复「交给我的 agent」或直接回答"
    );
    assert!(
        rig.brain.questions().is_empty(),
        "no answer on its behalf while parked"
    );
    for text in &texts {
        assert_senate_vocabulary(text);
    }
}

// ----- 2. a native mention -----------------------------------------------------

#[tokio::test]
async fn the_mention_is_native_where_the_surface_has_mentions() {
    let surface = FakeSurface::with(Caps {
        mentions: true,
        ..Caps::default()
    });
    let rig = Rig::start_with(bound_hub().await, surface, ScriptedBrain::new()).await;
    rig.append(request(agent("bob"), agent("alice"), "改 header"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    // The person behind the agent, by their own account on this platform.
    assert_eq!(posts[1].mention.as_deref(), Some("u_alice"));
    assert_eq!(
        posts[1].markdown,
        "这条需要你本人：改动请求要先经过本人（规则 request）。回复「交给我的 agent」或直接回答"
    );
}

#[tokio::test]
async fn parking_on_another_of_the_persons_agents_still_waits_on_the_person() {
    let hub = bound_hub().await;
    hub.seat(&path("codex"), "alice");
    let surface = FakeSurface::with(Caps {
        mentions: true,
        ..Caps::default()
    });
    let rig = Rig::start_with(hub, surface, ScriptedBrain::new()).await;
    let asked = rig
        .append(request(
            agent("bob"),
            agent_named("alice", "codex"),
            "改 header",
        ))
        .await;
    rig.settle().await;
    let parked = statuses(&rig.hub.events(&refund()), asked.seq);
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].1.escalated_to, Some(name("alice")));
    let posts = rig.surface.posts();
    assert_eq!(posts[1].mention.as_deref(), Some("u_alice"));
}

// ----- 3. approval ---------------------------------------------------------------

#[tokio::test]
async fn the_person_hands_a_parked_request_to_their_own_agent() {
    let rig = Rig::start(bound_hub().await).await;
    let asked = rig
        .append(request(
            agent("bob"),
            agent("alice"),
            "把网关 header 改成 Idempotency-Key",
        ))
        .await;
    rig.settle().await;
    let root_ref = rig.surface.posts()[0].message_ref.clone();

    // Someone else's 交给我的 agent is not an approval, nor a takeover.
    rig.append(in_thread(
        "pm",
        "交给我的 agent",
        "m_p1",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(statuses(&rig.hub.events(&refund()), asked.seq).len(), 1);

    let ok = rig
        .append(in_thread(
            "alice",
            "交给我的 agent",
            "m_a1",
            asked.seq,
            None,
            &root_ref,
        ))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let task = approval(&events, asked.seq).expect("her own task");
    assert_eq!(task.from, user("alice"));
    assert_eq!(task.to, vec![agent("alice")]);
    assert_eq!(task.body, "把网关 header 改成 Idempotency-Key");
    assert_eq!(task.meta.get(meta::RELAYED_FROM), Some(&json!(ok.seq.0)));
    let verdict = TriagePayload::of(&task).expect("inline verdict");
    assert_eq!(verdict.verdict, TriageVerdict::Agent);
    assert_eq!(verdict.rule.as_deref(), Some("approved"));
    let all = statuses(&events, asked.seq);
    assert_eq!(
        all.last().map(|(_, s)| s.state),
        Some(StatusState::Processing)
    );
    let board = rig.hub.tasks(&refund()).await.expect("board");
    assert_eq!(
        board.get(asked.seq).map(|t| t.state),
        Some(StatusState::Processing)
    );
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("好的，已交给 [senator::claude]。")
    );
    // The approval is not echoed into the thread.
    assert!(
        !rig.surface
            .texts()
            .iter()
            .any(|t| t.starts_with("[senator::alice]"))
    );

    // Saying it again changes nothing.
    let count = events.len();
    rig.append(in_thread(
        "alice",
        "交给我的agent。",
        "m_a2",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(rig.hub.events(&refund()).len(), count + 1, "only the chat");
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("这条已经交给 agent 了。")
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

/// A request put to `codex` while alice also has `claude`.
async fn parked_on_codex() -> (Rig, Seq, String) {
    let hub = bound_hub().await;
    hub.seat(&path("codex"), "alice");
    // Her other agent is the one online: it still must not get the thread.
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(request(
            agent("bob"),
            agent_named("alice", "codex"),
            "把网关 header 改成 Idempotency-Key",
        ))
        .await;
    rig.settle().await;
    let root_ref = rig.surface.posts()[0].message_ref.clone();
    (rig, asked.seq, root_ref)
}

#[tokio::test]
async fn hand_it_to_my_agent_goes_back_to_the_agent_the_root_was_put_to() {
    let (rig, root, root_ref) = parked_on_codex().await;
    rig.append(in_thread(
        "alice",
        "交给我的 agent",
        "m_a1",
        root,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let task = approval(&rig.hub.events(&refund()), root).expect("approved");
    assert_eq!(
        task.to,
        vec![agent_named("alice", "codex")],
        "never the sibling"
    );
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("好的，已交给 [senator::codex]。")
    );
    assert!(rig.brain.pick_inputs().is_empty(), "nothing to pick");
}

#[tokio::test]
async fn hand_it_to_my_claude_goes_to_the_agent_named() {
    let (rig, root, root_ref) = parked_on_codex().await;
    rig.append(in_thread(
        "alice",
        "交给我的 claude",
        "m_a1",
        root,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let task = approval(&rig.hub.events(&refund()), root).expect("approved");
    assert_eq!(task.to, vec![agent("alice")]);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("好的，已交给 [senator::claude]。")
    );
}

#[tokio::test]
async fn hand_it_to_an_agent_she_does_not_have_is_refused_and_changes_nothing() {
    let (rig, root, root_ref) = parked_on_codex().await;
    let before = rig.hub.events(&refund()).len();
    rig.append(in_thread(
        "alice",
        "交给我的 gemini",
        "m_a1",
        root,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    assert_eq!(events.len(), before + 1, "only her chat");
    assert!(approval(&events, root).is_none());
    let reply = rig.surface.posts().last().cloned().expect("a reply");
    assert_eq!(
        reply.markdown,
        "[senator::alice] 在这里没有叫 gemini 的 agent。"
    );
    assert_eq!(
        reply.markdown,
        render::no_agent_named(Locale::Zh, &name("alice"), "gemini")
    );
    assert_eq!(reply.target, reply_to("m_a1"));
    // The thread still waits on her: no takeover either.
    let all = statuses(&events, root);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].1.state, StatusState::Suspended);
}

/// bob's agent asks pm's agent, which escalates to alice: the root names
/// none of alice's agents.
async fn escalated_to_alice(hub: Arc<MemoryHub>) -> (Rig, Seq, String) {
    hub.connect(&refund(), &agent_path("pm"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(question(agent("bob"), agent("pm"), "退款上限要不要改？"))
        .await;
    rig.settle().await;
    let root_ref = rig.surface.posts()[0].message_ref.clone();
    let mut esc = StatusPayload::new(StatusState::Suspended);
    esc.escalated_to = Some(name("alice"));
    esc.reason = Some("要 alice 拍板".into());
    rig.append(status(agent("pm"), asked.seq, &esc)).await;
    rig.settle().await;
    (rig, asked.seq, root_ref)
}

#[tokio::test]
async fn when_the_root_names_none_of_hers_the_speaker_picks_her_one_connected_agent() {
    let hub = bound_hub().await;
    hub.seat(&path("codex"), "alice");
    hub.connect(&refund(), &path("codex"));
    let (rig, root, root_ref) = escalated_to_alice(hub).await;
    rig.append(in_thread(
        "alice",
        "交给我的 agent",
        "m_a1",
        root,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let task = approval(&rig.hub.events(&refund()), root).expect("approved");
    assert_eq!(task.to, vec![agent_named("alice", "codex")]);
    assert_eq!(task.from, user("alice"));
    let picks = rig.brain.pick_inputs();
    assert_eq!(picks.len(), 1);
    assert_eq!(picks[0].owner, Some(name("alice")));
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("好的，已交给 [senator::codex]。")
    );
}

#[tokio::test]
async fn when_the_speaker_cannot_tell_it_asks_in_the_thread_and_her_answer_carries_the_hand_off() {
    let hub = bound_hub().await;
    hub.seat(&path("codex"), "alice");
    let (rig, root, root_ref) = escalated_to_alice(hub).await;
    rig.append(in_thread(
        "alice",
        "交给我的 agent",
        "m_a1",
        root,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    assert!(approval(&rig.hub.events(&refund()), root).is_none());
    let ask = rig.surface.posts().last().cloned().expect("the question");
    assert_eq!(ask.target, reply_to("m_a1"));
    assert!(
        ask.markdown
            .starts_with("[senator::alice] 有 2 个 agent，你指的是哪一个？回复编号或名字："),
        "{}",
        ask.markdown
    );
    assert!(ask.markdown.contains("[senator::claude] 不在线"));
    assert!(ask.markdown.contains("[senator::codex] 不在线"));
    assert_senate_vocabulary(&ask.markdown);

    // Her answer, in the thread, is not a takeover: it carries the hand-off.
    rig.append(in_thread(
        "alice",
        "codex",
        "m_a2",
        root,
        ask.thread_ref.clone(),
        &ask.message_ref,
    ))
    .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let task = approval(&events, root).expect("approved");
    assert_eq!(task.to, vec![agent_named("alice", "codex")]);
    assert!(
        statuses(&events, root)
            .iter()
            .all(|(_, s)| s.takeover.is_none())
    );
    // The reply to her answer; codex is offline, so the Speaker's
    // provisional answer for it may follow in the thread.
    let texts = rig.surface.texts();
    assert!(
        texts.iter().any(|t| t == "好的，已交给 [senator::codex]。"),
        "{texts:#?}"
    );
}

#[tokio::test]
async fn a_person_with_no_agent_is_told_it_is_theirs_to_handle() {
    let hub = bound_hub().await;
    let mut without = roster();
    assert!(without.unseat(&agent_path("alice")));
    hub.set_roster(without);
    let (rig, root, root_ref) = escalated_to_alice(hub).await;
    let before = rig.hub.events(&refund()).len();
    rig.append(in_thread(
        "alice",
        "交给我的 agent",
        "m_a1",
        root,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    assert_eq!(events.len(), before + 1, "only her chat");
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("你在这里没有 agent，这条只能由你本人处理。")
    );
    assert_eq!(
        rig.surface.texts().last().cloned(),
        Some(render::no_agent_of_yours(Locale::Zh))
    );
}

// ----- 4. human first ------------------------------------------------------------

#[tokio::test]
async fn a_member_replying_before_the_answer_takes_the_thread_over() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "网关幂等用哪个 header？",
        ))
        .await;
    rig.settle().await;
    let root_ref = rig.surface.posts()[0].message_ref.clone();

    rig.append(in_thread(
        "pm",
        "我来答：用 Idempotency-Key",
        "m_p1",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.append(in_thread(
        "pm",
        "补充一下",
        "m_p2",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let taken = statuses(&events, asked.seq);
    assert_eq!(taken.len(), 1, "one takeover: {taken:#?}");
    let (from, s) = &taken[0];
    assert_eq!(*from, Address::Speaker);
    assert_eq!(s.state, StatusState::Suspended);
    assert_eq!(s.takeover, Some(name("pm")));
    assert_eq!(s.reason.as_deref(), Some("人工处理中"));
    assert!(
        rig.brain.questions().is_empty(),
        "no answer on its behalf once a person has it"
    );
}

#[tokio::test]
async fn a_guest_replying_in_a_task_thread_does_not_take_it_over() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "网关幂等用哪个 header？",
        ))
        .await;
    rig.settle().await;
    let root_ref = rig.surface.posts()[0].message_ref.clone();
    for (text, message_ref) in [
        ("我来答：用 Idempotency-Key", "m_x1"),
        ("交给我的 agent", "m_x2"),
    ] {
        rig.append(in_thread(
            STRANGER,
            text,
            message_ref,
            asked.seq,
            None,
            &root_ref,
        ))
        .await;
    }
    rig.settle().await;
    let events = rig.hub.events(&refund());
    assert!(statuses(&events, asked.seq).is_empty(), "{events:#?}");
    assert!(approval(&events, asked.seq).is_none());

    // A member after the guest still can.
    rig.append(in_thread(
        "pm",
        "我来答",
        "m_p1",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let taken = statuses(&rig.hub.events(&refund()), asked.seq);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].1.takeover, Some(name("pm")));
}

/// A live trial's thread 49: the agent was away, the Speaker found nothing
/// and said so after `answer_fallback_secs`, and a person replied after
/// that. The non-answer is provisional, so the person still takes it over.
#[tokio::test(start_paused = true)]
async fn a_member_replying_after_a_provisional_fallback_still_takes_the_thread_over() {
    let hub = bound_hub().await;
    let rig = Rig::start(hub).await;
    rig.brain
        .answers(Err(BrainError::Unavailable("nothing in the log".into())));
    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "presence_disconnected_secs 默认是多少？",
        ))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(61)).await;
    rig.settle().await;

    let events = rig.hub.events(&refund());
    let fallback = events
        .iter()
        .find(|e| e.kind == Kind::Answer && e.thread == Some(asked.seq))
        .expect("the fallback");
    assert!(senate_core::is_provisional(fallback), "{fallback:#?}");
    assert!(fallback.body.contains("现在不在线"), "{}", fallback.body);
    // The asker still sees it in the platform thread.
    let texts = rig.surface.texts();
    assert!(texts.iter().any(|t| t.contains("现在不在线")), "{texts:#?}");

    let root_ref = rig.surface.posts()[0].message_ref.clone();
    rig.append(in_thread(
        "pm",
        "我来：默认 120",
        "m_p1",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let taken = statuses(&rig.hub.events(&refund()), asked.seq);
    assert_eq!(taken.len(), 1, "one takeover: {taken:#?}");
    assert_eq!(taken[0].1.takeover, Some(name("pm")));
    assert_eq!(taken[0].1.reason.as_deref(), Some("人工处理中"));
}

#[tokio::test]
async fn after_the_answer_a_reply_is_a_correction_and_the_asker_is_not_a_takeover() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "网关幂等用哪个 header？",
        ))
        .await;
    rig.append(answer(
        agent("bob"),
        asked.seq,
        agent("alice"),
        "X-Idem-Key",
    ))
    .await;
    rig.settle().await;
    let root_ref = rig.surface.posts()[0].message_ref.clone();
    rig.append(in_thread(
        "pm",
        "不对，用 Idempotency-Key",
        "m_p1",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    assert!(statuses(&rig.hub.events(&refund()), asked.seq).is_empty());

    // A person's own relayed question: their follow-up is not a takeover.
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: "退款上限多少天？".into(),
    }));
    rig.append(mention("pm", "问 alice 的 agent 退款上限多少天", "m_h2"))
        .await;
    rig.settle().await;
    let relay = rig
        .hub
        .events(&refund())
        .into_iter()
        .rfind(|e| e.kind == Kind::Question)
        .expect("relay");
    rig.append(in_thread(
        "pm",
        "最好今天回我",
        "m_h3",
        relay.seq,
        None,
        "m_h2",
    ))
    .await;
    rig.settle().await;
    assert!(statuses(&rig.hub.events(&refund()), relay.seq).is_empty());
}

// ----- 5. escalate -----------------------------------------------------------------

#[tokio::test]
async fn an_escalation_mentions_the_person_and_tells_the_asking_agent() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(hub).await;
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "这个需求要不要做？"))
        .await;
    rig.settle().await;
    let root_ref = rig.surface.posts()[0].message_ref.clone();

    let mut esc = StatusPayload::new(StatusState::Suspended);
    esc.escalated_to = Some(name("bob"));
    esc.reason = Some("要产品拍板".into());
    let escalated = rig.append(status(agent("bob"), asked.seq, &esc)).await;
    rig.settle().await;

    let posts = rig.surface.posts();
    let mention = posts.last().expect("mention");
    assert_eq!(mention.target, reply_to(&root_ref));
    assert_eq!(
        mention.markdown,
        "@bob [senator::copilot] 把这条交给你：要产品拍板"
    );
    let events = rig.hub.events(&refund());
    let ack = events
        .iter()
        .find(|e| e.seq > escalated.seq && e.kind == Kind::Note)
        .expect("ack");
    assert_eq!(ack.from, Address::Speaker);
    assert_eq!(ack.to, vec![agent("alice")]);
    assert_eq!(ack.thread, Some(asked.seq));
    assert_eq!(ack.body, "已转给 [senator::bob] 本人");
    assert_eq!(posts.len(), 2, "the note to the agent is not mirrored");
}

#[tokio::test]
async fn an_escalated_relay_is_acknowledged_in_the_askers_thread() {
    let hub = bound_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start(hub).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: "能延期吗？".into(),
    }));
    rig.append(mention("pm", "问 alice 的 agent 能延期吗", "m_h1"))
        .await;
    rig.settle().await;
    let relay = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.kind == Kind::Question)
        .expect("relay");
    let mut esc = StatusPayload::new(StatusState::Suspended);
    esc.escalated_to = Some(name("alice"));
    rig.append(status(agent("alice"), relay.seq, &esc)).await;
    rig.settle().await;
    let posts = rig.surface.posts();
    let texts: Vec<_> = posts.iter().map(|p| p.markdown.as_str()).collect();
    assert_eq!(
        texts[1..],
        [
            "@alice [senator::claude] 把这条交给你",
            "已转给 [senator::alice] 本人"
        ],
        "{posts:#?}"
    );
    assert!(posts[1..].iter().all(|p| p.target == reply_to("m_h1")));
}

// ----- 6. the signature ------------------------------------------------------------

#[tokio::test]
async fn an_answer_back_in_a_people_group_is_signed_and_the_agent_group_has_the_thread() {
    let hub = agents_hub().await;
    hub.connect(&refund(), &agent_path("alice"));
    let rig = Rig::start_with(hub, FakeSurface::with(Caps::ADOPTED), ScriptedBrain::new()).await;
    rig.brain.interprets(Ok(Intent::Relay {
        targets: vec!["alice".into()],
        kind: RelayKind::Question,
        body: "退款上限多少天？".into(),
    }));
    rig.append(mention("pm", "问 alice 的 agent 退款上限多少天", "m_h1"))
        .await;
    rig.settle().await;
    let relay = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.kind == Kind::Question)
        .expect("relay");
    let card = rig
        .surface
        .cards()
        .into_iter()
        .find(|c| {
            c.target
                == PostTarget::NewRoot {
                    group_id: AGENTS.into(),
                }
                && c.first.title.starts_with("退款上限")
        })
        .expect("a task card in the Agent group");
    assert_eq!(card.first.state, StatusState::Processing);
    assert!(
        card.first
            .markdown
            .contains("[senator::pm]（经 Speaker） → [senator::claude]（seq"),
        "{}",
        card.first.markdown
    );

    rig.append(answer(agent("alice"), relay.seq, user("pm"), "30 天"))
        .await;
    rig.settle().await;
    let posts = rig.surface.posts();
    let signed = posts
        .iter()
        .find(|p| p.target == reply_to("m_h1") && p.markdown.contains("30 天"))
        .expect("the answer in the asker's thread");
    assert_eq!(
        signed.markdown,
        "[senator::claude] 30 天\n\n由 [senator::claude] 回答 · 来自 refund-v2 的 Agent 群"
    );
    let plain = posts
        .iter()
        .find(|p| p.target == reply_to(&card.message_ref))
        .expect("the answer in the Agent group thread");
    assert_eq!(plain.markdown, "[senator::claude] 30 天");
}

#[tokio::test]
async fn someone_else_saying_hand_it_to_my_agent_is_told_whom_it_waits_on() {
    let rig = Rig::start(bound_hub().await).await;
    let asked = rig
        .append(request(
            agent("bob"),
            agent("alice"),
            "把网关 header 改成 Idempotency-Key",
        ))
        .await;
    rig.settle().await;
    let root_ref = rig.surface.posts()[0].message_ref.clone();
    let before = rig.hub.events(&refund()).len();

    rig.append(in_thread(
        "pm",
        "交给我的 agent",
        "m_p1",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;

    let reply = rig.surface.posts().last().cloned().expect("a reply");
    assert_eq!(
        reply.markdown,
        "这条在等 [senator::alice] 本人，只有 TA 能交给自己的 agent。"
    );
    assert!(matches!(
        reply.target,
        PostTarget::Reply { ref message_ref, in_thread: true } if message_ref == "m_p1"
    ));
    // Nothing changes in the log: no approval, no takeover.
    let events = rig.hub.events(&refund());
    assert_eq!(events.len(), before + 1, "only pm's chat");
    assert_eq!(statuses(&events, asked.seq).len(), 1);
    rig.stop().await;
}

// ----- 7. in English -----------------------------------------------------------------

#[tokio::test]
async fn in_english_a_parked_request_mentions_the_person_and_her_hand_off_names_its_agent() {
    let hub = bound_hub().await;
    hub.seat(&path("codex"), "alice");
    let rig = Rig::start_config(
        hub,
        FakeSurface::with(Caps {
            mentions: true,
            ..Caps::default()
        }),
        ScriptedBrain::new(),
        config_in(Locale::En),
    )
    .await;
    let asked = rig
        .append(request(
            agent("bob"),
            agent_named("alice", "codex"),
            "Rename the gateway header to Idempotency-Key",
        ))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let note = verdicts(&events, asked.seq);
    assert_eq!(note[0].rule.as_deref(), Some("request"));
    let posts = rig.surface.posts();
    assert_eq!(
        posts[0].markdown,
        format!(
            "[senator::copilot] → [senator::codex] Rename the gateway header to Idempotency-Key (seq {})",
            asked.seq
        )
    );
    assert_eq!(posts[1].mention.as_deref(), Some("u_alice"));
    assert_eq!(
        posts[1].markdown,
        "This needs you in person: A change request goes to the person first (rule request). Reply \"hand it to my agent\" or answer it directly."
    );
    let root_ref = posts[0].message_ref.clone();

    // Someone else is told whom it waits on.
    rig.append(in_thread(
        "pm",
        "hand it to my agent",
        "m_p1",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some(
            "This is waiting on [senator::alice] in person; only they can hand it to their own agent."
        )
    );

    // An agent she does not have.
    rig.append(in_thread(
        "alice",
        "hand it to my gemini",
        "m_a0",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[senator::alice] has no agent called gemini here.")
    );

    // "my claude" is that agent, not the one the root was put to.
    rig.append(in_thread(
        "alice",
        "Hand it to my claude.",
        "m_a1",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let task = approval(&rig.hub.events(&refund()), asked.seq).expect("approved");
    assert_eq!(task.to, vec![agent("alice")]);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("OK, handed to [senator::claude].")
    );
    rig.append(in_thread(
        "alice",
        "hand it to my agent",
        "m_a2",
        asked.seq,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("This has already been handed to the agent.")
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}

#[tokio::test]
async fn in_english_hand_it_to_my_agent_goes_to_the_roots_own_agent_and_an_escalation_is_acked() {
    let (rig, root, root_ref) = {
        let hub = bound_hub().await;
        hub.seat(&path("codex"), "alice");
        hub.connect(&refund(), &agent_path("alice"));
        let rig = Rig::start_config(
            hub,
            FakeSurface::new(),
            ScriptedBrain::new(),
            config_in(Locale::En),
        )
        .await;
        let asked = rig
            .append(request(
                agent("bob"),
                agent_named("alice", "codex"),
                "Rename the header",
            ))
            .await;
        rig.settle().await;
        let root_ref = rig.surface.posts()[0].message_ref.clone();
        (rig, asked.seq, root_ref)
    };
    assert_eq!(
        rig.surface.texts()[1],
        "@alice This needs you in person: A change request goes to the person first (rule request). Reply \"hand it to my agent\" or answer it directly."
    );
    rig.append(in_thread(
        "alice",
        "hand it to my agent",
        "m_a1",
        root,
        None,
        &root_ref,
    ))
    .await;
    rig.settle().await;
    let task = approval(&rig.hub.events(&refund()), root).expect("approved");
    assert_eq!(task.to, vec![agent_named("alice", "codex")]);
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("OK, handed to [senator::codex].")
    );

    // An agent's escalation, in English.
    let asked = rig
        .append(question(
            agent("alice"),
            agent("bob"),
            "Should we build this?",
        ))
        .await;
    rig.settle().await;
    let mut esc = StatusPayload::new(StatusState::Suspended);
    esc.escalated_to = Some(name("bob"));
    esc.reason = Some("needs a product call".into());
    let escalated = rig.append(status(agent("bob"), asked.seq, &esc)).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("@bob [senator::copilot] handed this to you: needs a product call")
    );
    let ack = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.seq > escalated.seq && e.kind == Kind::Note)
        .expect("ack");
    assert_eq!(ack.body, "Passed to [senator::bob] in person");
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
}
