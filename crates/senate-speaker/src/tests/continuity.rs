//! Continuity: presence and checkpoint lines, `/status`, the Speaker taking
//! over an away agent's questions, escalation, hand-back, and the companion
//! byline. Everything is per agent: a person's other agents never stand in.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use senate_core::{
    Address, CheckpointFiles, CheckpointPayload, CheckpointTrigger, Event, Hub, Kind, Locale,
    NewEvent, PresencePayload, PresenceState, Principal, Roster, SeatedAgent, SenatorName, Seq,
    StatusPayload, StatusState, meta, presence_event,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::{
    GROUP, Rig, agent, agent_named, agent_path, agents_bind_event, answer,
    assert_senate_vocabulary, bind_event, config, config_in, mention, name, path, question, refund,
    roster, runner_of,
};
use super::surface::{Caps, FakeSurface};
use crate::{ScriptedBrain, SpeakerConfig, TAKEOVER_META};

const COMMIT: &str = "a41f9c2e0000000000000000000000000000beef";
pub(crate) const AGENTS: &str = "agents";

/// A people group and an adopted Agent group.
pub(crate) async fn agents_hub() -> Arc<MemoryHub> {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub.append(agents_bind_event(&refund(), AGENTS))
        .await
        .expect("bind agents");
    hub
}

/// The server's presence transition for `who` (a senator path such as
/// `claude`), `offset_ms` after a fixed base so each one has its own
/// derived id.
pub(crate) fn presence_of(who: &SenatorName, state: PresenceState, offset_ms: i64) -> NewEvent {
    let since = chrono::DateTime::<Utc>::from_timestamp_millis(1_800_000_000_000 + offset_ms)
        .expect("a time");
    presence_event(
        refund(),
        &PresencePayload {
            paused: false,
            agent: who.clone(),
            state,
            since,
            since_seq: Seq(0),
            previous: None,
            last_seen: None,
            cursor: None,
            lag: Some(3),
            new_owner: None,
        },
    )
}

/// `<handle>/claude`'s presence transition.
fn presence(handle: &str, state: PresenceState, offset_ms: i64) -> NewEvent {
    presence_of(&agent_path(handle), state, offset_ms)
}

fn checkpoint(handle: &str, progress: &str, pushed: bool) -> NewEvent {
    checkpoint_by(handle, progress, pushed, CheckpointTrigger::Tool)
}

fn checkpoint_by(
    handle: &str,
    progress: &str,
    pushed: bool,
    trigger: CheckpointTrigger,
) -> NewEvent {
    checkpoint_of(&agent_path(handle), progress, pushed, trigger)
}

/// A checkpoint of the agent `who` on its person's branch.
fn checkpoint_of(
    who: &SenatorName,
    progress: &str,
    pushed: bool,
    trigger: CheckpointTrigger,
) -> NewEvent {
    let payload = CheckpointPayload {
        branch: format!("senate/refund-v2/{}", runner_of(who)),
        commit: COMMIT.into(),
        pushed,
        remote_url: None,
        files: CheckpointFiles {
            progress: Some(progress.into()),
            decisions: None,
            open: None,
        },
        session_transcript: false,
        trigger,
    };
    NewEvent::new(
        refund(),
        Kind::Checkpoint,
        Address::senator(who),
        "幂等键做完了",
    )
    .with_meta(meta::CHECKPOINT, payload.value())
}

fn speaker_answers(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| e.kind == Kind::Answer && e.from == Address::Speaker)
        .collect()
}

fn presence_states(events: &[Event], handle: &str) -> Vec<PresenceState> {
    presence_states_of(events, &agent_path(handle))
}

fn presence_states_of(events: &[Event], who: &SenatorName) -> Vec<PresenceState> {
    events
        .iter()
        .filter_map(PresencePayload::of)
        .filter(|p| p.agent == *who)
        .map(|p| p.state)
        .collect()
}

/// An Agent group and Alice's agent away (`absent`) with a checkpoint.
async fn alice_away() -> Arc<MemoryHub> {
    let hub = agents_hub().await;
    for event in [
        presence("alice", PresenceState::Disconnected, 0),
        presence("alice", PresenceState::Absent, 1),
        checkpoint("alice", "- 完成：幂等键用 Idempotency-Key", true),
    ] {
        hub.append(event).await.expect("append");
    }
    hub
}

// ----- presence lines ---------------------------------------------------------

#[tokio::test]
async fn presence_transitions_are_lines_in_the_agent_group() {
    let rig = Rig::start(agents_hub().await).await;
    for (state, n) in [
        (PresenceState::Disconnected, 0),
        (PresenceState::Absent, 1),
        (PresenceState::TakenOver, 2),
        (PresenceState::Online, 3),
    ] {
        rig.append(presence("alice", state, n)).await;
    }
    let mut reassigned = PresencePayload::of(
        &presence("bob", PresenceState::Reassigned, 4).into_event(Seq(1), Utc::now()),
    )
    .expect("payload");
    reassigned.new_owner = Some(agent_path("pm"));
    rig.append(presence_event(refund(), &reassigned)).await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts(),
        vec![
            "[senator::claude] 失联",
            "[senator::claude] 缺席（租约过期）",
            "[senator::claude] 由 Speaker 接管",
            "[senator::claude] 在线",
            "[senator::copilot] 的工作已交给 [senator::aider]",
        ]
    );
    for text in rig.surface.texts() {
        assert_senate_vocabulary(&text);
    }
    assert!(rig.surface.posts().iter().all(|p| p.target
        == senate_core::PostTarget::NewRoot {
            group_id: AGENTS.into()
        }));

    // A replay posts nothing twice.
    let surface = rig.surface.clone();
    let hub = rig.hub.clone();
    rig.stop().await;
    hub.set_cursor(&refund(), crate::CURSOR, Seq(2))
        .await
        .expect("rewind");
    let again = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;
    assert_eq!(surface.texts().len(), 5);
    again.stop().await;
}

#[tokio::test]
async fn at_answers_only_just_absent_taken_over_and_back_online_are_posted() {
    let rig = Rig::start(agents_hub().await).await;
    rig.append(mention("pm", "/output answers", "m_h1")).await;
    rig.settle().await;
    let before = rig.surface.texts().len();
    for (state, n) in [
        (PresenceState::Disconnected, 0),
        (PresenceState::Absent, 1),
        (PresenceState::TakenOver, 2),
        (PresenceState::Online, 3),
        (PresenceState::Disconnected, 4),
    ] {
        rig.append(presence("alice", state, n)).await;
    }
    rig.append(checkpoint_by(
        "alice",
        "进度",
        true,
        CheckpointTrigger::Handoff,
    ))
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts()[before..].to_vec(),
        vec![
            "[senator::claude] 缺席（租约过期）",
            "[senator::claude] 由 Speaker 接管",
            "[senator::claude] 在线",
        ]
    );
}

#[tokio::test]
async fn a_presence_not_from_the_server_is_ignored() {
    let rig = Rig::start(agents_hub().await).await;
    let mut forged = presence("alice", PresenceState::Absent, 0);
    forged.from = agent("alice");
    rig.append(forged).await;
    rig.settle().await;
    assert!(rig.surface.texts().is_empty());
}

// ----- checkpoint lines ---------------------------------------------------------

#[tokio::test]
async fn only_a_handoff_checkpoint_is_a_line_in_the_agent_group() {
    let rig = Rig::start(agents_hub().await).await;
    for trigger in [
        CheckpointTrigger::Commit,
        CheckpointTrigger::Interval,
        CheckpointTrigger::SessionEnd,
        CheckpointTrigger::Tool,
    ] {
        rig.append(checkpoint_by("alice", "进度", true, trigger))
            .await;
    }
    let pushed = rig
        .append(checkpoint_by(
            "alice",
            "进度",
            true,
            CheckpointTrigger::Handoff,
        ))
        .await;
    let local = rig
        .append(checkpoint_by(
            "bob",
            "进度",
            false,
            CheckpointTrigger::Handoff,
        ))
        .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts(),
        vec![
            format!(
                "[senator::claude] 检查点 · senate/refund-v2/alice@a41f9c2（已推送）· seq {}",
                pushed.seq
            ),
            format!(
                "[senator::copilot] 检查点 · senate/refund-v2/bob@a41f9c2（未推送）· seq {}",
                local.seq
            ),
        ]
    );
}

// ----- /status and the pinned card ------------------------------------------------

#[tokio::test]
async fn status_shows_presence_lag_and_checkpoint_age() {
    let hub = alice_away().await;
    hub.append(presence("bob", PresenceState::Disconnected, 5))
        .await
        .expect("append");
    let rig = Rig::start(hub).await;
    let cp = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.kind == Kind::Checkpoint)
        .expect("checkpoint");
    rig.append(mention("pm", "/status --all", "m_s1")).await;
    rig.settle().await;
    let said = rig.surface.texts().last().cloned().expect("a reply");
    assert!(said.starts_with("本群绑定在委员会 refund-v2。"), "{said}");
    assert!(
        said.contains(&format!(
            "\n- [senator::claude] 缺席（租约过期） · 落后 3 条事件 · 检查点 0 分钟前（seq {}）",
            cp.seq
        )),
        "{said}"
    );
    assert!(
        said.contains("\n- [senator::copilot] 失联 · 落后 3 条事件 · 没有检查点"),
        "{said}"
    );
    assert_senate_vocabulary(&said);
}

#[tokio::test]
async fn status_lists_each_agent_by_its_own_connection() {
    // Alice has two agents: codex is connected, claude is not. Neither
    // stands for the other. Agents are listed in the order they were seated.
    let hub = alice_away().await;
    let codex = path("codex");
    hub.connect(&refund(), &codex);
    hub.connect(&refund(), &agent_path("bob"));
    hub.append(presence_of(&codex, PresenceState::Online, 2))
        .await
        .expect("append");
    let rig = Rig::start(hub).await;
    rig.append(mention("pm", "/status --all", "m_s1")).await;
    rig.settle().await;
    let said = rig.surface.texts().last().cloned().expect("a reply");
    assert!(
        said.contains("\n在线 agent：[senator::copilot]、[senator::codex]\n"),
        "{said}"
    );
    assert!(said.contains("\n离线 agent：[senator::claude]\n"), "{said}");
    // Every seat with its presence; aider, on the roster but never on the
    // committee, is no seat of it.
    assert!(
        said.contains("\n席位：\n- [senator::claude] 缺席（租约过期）\n- [senator::copilot] 在线\n- [senator::codex] 在线\n"),
        "{said}"
    );
    assert!(!said.contains("aider"), "{said}");
    assert!(
        said.contains("\n- [senator::claude] 缺席（租约过期）"),
        "{said}"
    );
    assert!(said.contains("\n- [senator::codex] 在线"), "{said}");
    rig.append(mention("pm", "/status", "m_current")).await;
    rig.settle().await;
    let current = rig.surface.texts().last().cloned().expect("a reply");
    assert!(!current.contains("[senator::claude]"), "{current}");
    assert!(current.contains("[senator::codex]"), "{current}");
    rig.stop().await;

    // The same in English.
    let hub = alice_away().await;
    hub.connect(&refund(), &codex);
    let rig = Rig::start_config(
        hub,
        FakeSurface::new(),
        ScriptedBrain::new(),
        config_in(Locale::En),
    )
    .await;
    rig.append(mention("pm", "/status --all", "m_s1")).await;
    rig.settle().await;
    let said = rig.surface.texts().last().cloned().expect("a reply");
    assert!(
        said.starts_with("This group is bound to committee refund-v2.\nAgents online: [senator::codex]\nAgents offline: [senator::claude]\nSeats:\n- [senator::claude] absent (lease expired)\n- [senator::codex] online\n"),
        "{said}"
    );
    assert!(
        said.contains("\n- [senator::claude] absent (lease expired) · 3 events behind · checkpoint 0 minutes ago"),
        "{said}"
    );
    assert_senate_vocabulary(&said);
}

#[tokio::test(start_paused = true)]
async fn the_today_card_has_a_presence_line_per_agent() {
    let hub = agents_hub().await;
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;
    rig.append(presence("alice", PresenceState::Absent, 0))
        .await;
    rig.append(presence("bob", PresenceState::Disconnected, 1))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_millis(2_100)).await;
    rig.settle().await;
    let today = surface
        .cards()
        .into_iter()
        .find(|c| c.first.title.starts_with("今天的摘要"))
        .expect("today card");
    let text = today.now.markdown;
    assert!(text.contains("**各 agent 在场**"), "{text}");
    assert!(
        text.contains("- [senator::claude] 缺席（租约过期）"),
        "{text}"
    );
    assert!(text.contains("- [senator::copilot] 失联"), "{text}");
}

/// Thread 797 item 7b: "各 agent 最近在做" lists the seats in child
/// Senates, as /status shows them (alias, presence, duty), with the latest
/// relay to them the log here has; a quiet seat is listed too.
#[tokio::test(start_paused = true)]
async fn the_today_card_lists_child_seats_with_their_latest_activity() {
    let hub = agents_hub().await;
    hub.add_child("bob-desk", "pm");
    hub.declare_seats(
        "bob-desk",
        &refund(),
        &[
            ("codex", PresenceState::Online),
            ("archivist", PresenceState::Absent),
        ],
    );
    hub.declare_duty("bob-desk", &refund(), "codex", "账单的开发");
    hub.declare_alias("bob-desk", &refund(), "codex", "billing-dev");
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;
    let relayed = rig
        .append(
            NewEvent::new(
                refund(),
                Kind::Question,
                senate_core::Address::Speaker,
                "退款服务怎么部署？",
            )
            .to(senate_core::Address::Child {
                senate: super::rig::name("bob-desk"),
                committee: refund(),
            })
            .on_behalf_of(super::rig::user("pm"))
            .with_meta(
                senate_core::meta::INTENDED,
                json!("bob-desk::refund-v2::senator::codex"),
            ),
        )
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_millis(2_100)).await;
    rig.settle().await;
    let today = surface
        .cards()
        .into_iter()
        .find(|c| c.first.title.starts_with("今天的摘要"))
        .expect("today card");
    let text = today.now.markdown;
    let section = text
        .split("**各 agent 最近在做**\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .unwrap_or_default();
    assert_eq!(
        section,
        format!(
            "- billing-dev（bob-desk） 在线 · 职责：账单的开发 · seq {} · 提问：退款服务怎么部署？\n\
             - [bob-desk::archivist] 缺席（租约过期）",
            relayed.seq
        ),
        "{text}"
    );
}

// ----- takeover ----------------------------------------------------------------

#[tokio::test]
async fn an_absent_agents_question_is_answered_from_its_checkpoint() {
    let rig = Rig::start(alice_away().await).await;
    rig.brain.answers(Ok(
        "据检查点 seq 5 的 PROGRESS.md，用 Idempotency-Key。".into()
    ));
    let asked = rig
        .append(question(
            agent("bob"),
            agent("alice"),
            "幂等键用哪个 header？",
        ))
        .await;
    rig.settle().await;

    let asked_brain = &rig.brain.questions()[0];
    let takeover = asked_brain.takeover.as_ref().expect("a takeover");
    assert_eq!(takeover.checkpoint.agent, agent_path("alice"));
    assert_eq!(
        takeover.checkpoint.checkpoint.files.progress.as_deref(),
        Some("- 完成：幂等键用 Idempotency-Key")
    );
    assert!(takeover.brief.is_some());

    let events = rig.hub.events(&refund());
    let answers = speaker_answers(&events);
    assert_eq!(answers.len(), 1);
    let answered = answers[0];
    assert_eq!(answered.on_behalf_of, Some(agent("alice")));
    assert_eq!(answered.thread, Some(asked.seq));
    assert_eq!(answered.to, vec![agent("bob")]);
    assert_eq!(answered.meta.get(TAKEOVER_META), Some(&json!(true)));
    // Recorded as taken over first (once), then answered.
    let taken: Vec<&Event> = events
        .iter()
        .filter(|e| PresencePayload::of(e).is_some_and(|p| p.state == PresenceState::TakenOver))
        .collect();
    assert_eq!(taken.len(), 1);
    assert!(taken[0].seq < answered.seq);

    let texts = rig.surface.texts();
    assert!(
        texts.contains(&"[senator::claude] 由 Speaker 接管".to_owned()),
        "{texts:?}"
    );
    assert_eq!(
        texts.last().map(String::as_str),
        Some("[speaker::claude] 接管 据检查点 seq 5 的 PROGRESS.md，用 Idempotency-Key。")
    );
    for text in &texts {
        assert_senate_vocabulary(text);
        assert!(!text.contains("[speaker 代"), "{text}");
    }
    assert!(!answered.body.contains("[speaker"), "{}", answered.body);

    // A second question while taken over: answered too, no second mark.
    rig.brain.answers(Ok("还没做，见 PROGRESS.md。".into()));
    rig.append(question(agent("bob"), agent("alice"), "退款回调呢？"))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    assert_eq!(speaker_answers(&events).len(), 2);
    assert_eq!(
        presence_states(&events, "alice"),
        vec![
            PresenceState::Disconnected,
            PresenceState::Absent,
            PresenceState::TakenOver
        ]
    );
}

/// Alice with two agents: claude away (`absent`) and codex online with a
/// checkpoint of its own. `claude_checkpoint` decides whether claude left
/// one.
async fn alice_with_two_agents(claude_checkpoint: bool) -> Arc<MemoryHub> {
    let hub = agents_hub().await;
    let codex = path("codex");
    hub.connect(&refund(), &codex);
    let mut events = vec![
        presence("alice", PresenceState::Disconnected, 0),
        presence("alice", PresenceState::Absent, 1),
        presence_of(&codex, PresenceState::Online, 2),
        checkpoint_of(
            &codex,
            "- 完成：codex 在做退款回调",
            true,
            CheckpointTrigger::Tool,
        ),
    ];
    if claude_checkpoint {
        events.push(checkpoint(
            "alice",
            "- 完成：claude 定了幂等键用 Idempotency-Key",
            true,
        ));
    }
    for event in events {
        hub.append(event).await.expect("append");
    }
    hub
}

#[tokio::test]
async fn an_absent_agent_is_taken_over_from_its_own_checkpoint_never_its_siblings() {
    let rig = Rig::start(alice_with_two_agents(true).await).await;
    rig.brain.answers(Ok("用 Idempotency-Key。".into()));
    let asked = rig
        .append(question(
            agent("bob"),
            agent("alice"),
            "幂等键用哪个 header？",
        ))
        .await;
    rig.settle().await;

    let questions = rig.brain.questions();
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].agent, agent_path("alice"));
    let takeover = questions[0].takeover.as_ref().expect("a takeover");
    assert_eq!(takeover.checkpoint.agent, agent_path("alice"));
    assert_eq!(
        takeover.checkpoint.checkpoint.files.progress.as_deref(),
        Some("- 完成：claude 定了幂等键用 Idempotency-Key"),
        "never codex's checkpoint"
    );

    let events = rig.hub.events(&refund());
    let answers = speaker_answers(&events);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].on_behalf_of, Some(agent("alice")));
    assert_eq!(answers[0].thread, Some(asked.seq));
    // Nothing went to codex: not rerouted, not marked.
    let codex = agent_named("alice", "codex");
    assert!(
        !events.iter().any(|e| e.to.contains(&codex)),
        "never rerouted to codex"
    );
    assert_eq!(
        presence_states_of(&events, &path("codex")),
        vec![PresenceState::Online]
    );
    assert!(presence_states(&events, "alice").contains(&PresenceState::TakenOver));
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[speaker::claude] 接管 用 Idempotency-Key。")
    );
}

#[tokio::test]
async fn an_absent_agent_without_its_own_checkpoint_is_escalated_though_a_sibling_has_one() {
    let rig = Rig::start(alice_with_two_agents(false).await).await;
    rig.append(question(agent("bob"), agent("alice"), "q1 多少天？"))
        .await;
    rig.settle().await;

    assert!(rig.brain.questions().is_empty(), "no answer from codex's");
    let events = rig.hub.events(&refund());
    assert!(speaker_answers(&events).is_empty());
    assert_eq!(
        escalations(&events)[0].1.reason.as_deref(),
        Some("[senator::claude] 不在，检查点没有，这条需要人：q1 多少天？")
    );
    assert!(
        !events
            .iter()
            .any(|e| e.to.contains(&agent_named("alice", "codex"))),
        "never rerouted to codex"
    );
}

#[tokio::test(start_paused = true)]
async fn a_disconnected_agent_keeps_the_answer_window() {
    let hub = agents_hub().await;
    hub.append(presence("alice", PresenceState::Disconnected, 0))
        .await
        .expect("append");
    hub.append(checkpoint("alice", "进度", true))
        .await
        .expect("append");
    let rig = Rig::start(hub).await;
    rig.brain.answers(Ok("记录里没有。".into()));
    rig.append(question(agent("bob"), agent("alice"), "多少天？"))
        .await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    rig.settle().await;
    assert!(rig.brain.questions().is_empty(), "within the window");
    tokio::time::advance(Duration::from_secs(31)).await;
    rig.settle().await;
    let questions = rig.brain.questions();
    assert_eq!(questions.len(), 1);
    assert!(
        questions[0].takeover.is_none(),
        "a plain answer for the agent"
    );
    let events = rig.hub.events(&refund());
    let answered = speaker_answers(&events)[0];
    assert!(!answered.meta.contains_key(TAKEOVER_META));
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[speaker::claude] 记录里没有。")
    );
}

#[tokio::test(start_paused = true)]
async fn going_absent_during_the_window_takes_over_at_once() {
    let hub = agents_hub().await;
    hub.append(presence("alice", PresenceState::Disconnected, 0))
        .await
        .expect("append");
    hub.append(checkpoint("alice", "进度", true))
        .await
        .expect("append");
    let rig = Rig::start(hub).await;
    rig.brain.answers(Ok("见 PROGRESS.md。".into()));
    rig.append(question(agent("bob"), agent("alice"), "多少天？"))
        .await;
    rig.settle().await;
    assert!(rig.brain.questions().is_empty());
    rig.append(presence("alice", PresenceState::Absent, 1))
        .await;
    rig.settle().await;
    let questions = rig.brain.questions();
    assert_eq!(questions.len(), 1);
    assert!(questions[0].takeover.is_some());
}

#[tokio::test]
async fn a_request_is_never_taken_over() {
    let rig = Rig::start(alice_away().await).await;
    rig.append(
        NewEvent::new(refund(), Kind::Request, agent("bob"), "把 header 改了").to(agent("alice")),
    )
    .await;
    rig.settle().await;
    assert!(rig.brain.questions().is_empty());
    let events = rig.hub.events(&refund());
    assert!(speaker_answers(&events).is_empty());
    assert!(!presence_states(&events, "alice").contains(&PresenceState::TakenOver));
}

// ----- escalation ----------------------------------------------------------------

/// The Speaker's takeover escalations.
fn escalations(events: &[Event]) -> Vec<(&Event, StatusPayload)> {
    events
        .iter()
        .filter(|e| e.from == Address::Speaker && e.meta.get(TAKEOVER_META) == Some(&json!(true)))
        .filter_map(|e| StatusPayload::of(e).map(|s| (e, s)))
        .collect()
}

#[tokio::test]
async fn a_stale_checkpoint_is_escalated_to_the_other_developers() {
    let hub = alice_away().await;
    // Carol's agent has been active on the committee too (she is on the
    // roster with no account on the platform); pm's has not.
    let mut principals = roster().principals().to_vec();
    principals.push(Principal::new(name("carol")));
    let mut seated = roster().seated().to_vec();
    seated.push(SeatedAgent::new(agent_path("carol"), name("carol")));
    hub.set_roster(Roster::new(principals, seated).expect("roster"));
    hub.append(NewEvent::new(
        refund(),
        Kind::Note,
        Address::Senator(agent_path("carol")),
        "我在看退款",
    ))
    .await
    .expect("append");
    let config = SpeakerConfig {
        takeover_stale_hours: 0,
        ..config()
    };
    let surface = FakeSurface::with(Caps {
        mentions: true,
        ..Caps::default()
    });
    let rig = Rig::start_config(hub, surface.clone(), ScriptedBrain::new(), config).await;
    let asked = rig
        .append(question(
            agent("bob"),
            agent("alice"),
            "幂等键用哪个 header？",
        ))
        .await;
    rig.settle().await;

    assert!(rig.brain.questions().is_empty(), "no guessing");
    let events = rig.hub.events(&refund());
    assert!(speaker_answers(&events).is_empty());
    assert!(!presence_states(&events, "alice").contains(&PresenceState::TakenOver));
    let escalated = escalations(&events);
    assert_eq!(escalated.len(), 1);
    let (status_event, status) = &escalated[0];
    assert_eq!(status.state, StatusState::Suspended);
    assert_eq!(status_event.thread, Some(asked.seq));
    assert_eq!(status_event.on_behalf_of, Some(agent("alice")));
    let reason = "[senator::claude] 不在，检查点太旧，这条需要人：幂等键用哪个 header？";
    assert_eq!(status.reason.as_deref(), Some(reason));

    // A native mention of bob, and carol's written out, under the question.
    let root_copy = surface
        .posts()
        .into_iter()
        .find(|p| p.markdown.contains("幂等键用哪个 header？（seq"))
        .expect("the question's copy")
        .message_ref;
    let mentions: Vec<_> = surface
        .posts()
        .into_iter()
        .filter(|p| p.mention.is_some())
        .collect();
    assert_eq!(
        mentions
            .iter()
            .map(|p| p.mention.clone().unwrap_or_default())
            .collect::<Vec<_>>(),
        vec!["u_bob".to_owned()],
        "carol has no account on the platform: her mention is written out"
    );
    assert!(mentions.iter().all(|p| p.markdown == reason));
    assert!(mentions.iter().all(|p| p.target
        == senate_core::PostTarget::Reply {
            message_ref: root_copy.clone(),
            in_thread: true
        }));
    assert!(
        surface.texts().contains(&format!("@carol {reason}")),
        "{:?}",
        surface.texts()
    );
}

#[tokio::test]
async fn no_checkpoint_or_an_uncovered_question_is_escalated() {
    let hub = agents_hub().await;
    hub.append(presence("alice", PresenceState::Absent, 0))
        .await
        .expect("append");
    let rig = Rig::start(hub).await;
    rig.append(question(agent("bob"), agent("alice"), "q1 多少天？"))
        .await;
    rig.settle().await;
    assert!(rig.brain.questions().is_empty());
    let events = rig.hub.events(&refund());
    assert_eq!(
        escalations(&events)[0].1.reason.as_deref(),
        Some("[senator::claude] 不在，检查点没有，这条需要人：q1 多少天？")
    );
    assert!(
        rig.surface.texts().contains(
            &"@bob [senator::claude] 不在，检查点没有，这条需要人：q1 多少天？".to_owned()
        ),
        "{:?}",
        rig.surface.texts()
    );

    rig.append(checkpoint("alice", "只写了进度", true)).await;
    rig.brain.answers_uncovered("检查点里没有回调签名的信息。");
    rig.append(question(agent("bob"), agent("alice"), "q2 回调签名？"))
        .await;
    rig.settle().await;
    assert_eq!(rig.brain.questions().len(), 1);
    let events = rig.hub.events(&refund());
    assert!(speaker_answers(&events).is_empty());
    assert_eq!(
        escalations(&events)
            .last()
            .and_then(|(_, s)| s.reason.clone())
            .as_deref(),
        Some("[senator::claude] 不在，检查点没有覆盖这个问题，这条需要人：q2 回调签名？")
    );
    assert!(!presence_states(&events, "alice").contains(&PresenceState::TakenOver));
}

// ----- hand back -------------------------------------------------------------------

#[tokio::test]
async fn coming_back_hands_the_mailbox_back_once() {
    let rig = Rig::start(alice_away().await).await;
    rig.brain.answers(Ok("用 Idempotency-Key。".into()));
    rig.brain.answers(Ok("还没做。".into()));
    let q1 = rig
        .append(question(
            agent("bob"),
            agent("alice"),
            "幂等键用哪个 header？",
        ))
        .await;
    rig.settle().await;
    let q2 = rig
        .append(question(agent("bob"), agent("alice"), "退款回调做了吗？"))
        .await;
    rig.settle().await;
    let answers: Vec<Seq> = speaker_answers(&rig.hub.events(&refund()))
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(answers.len(), 2);

    let back = rig
        .append(presence("alice", PresenceState::Online, 10))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let notes: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == Kind::Note && e.from == Address::Speaker && e.to == [agent("alice")])
        .collect();
    assert_eq!(notes.len(), 1);
    let body = &notes[0].body;
    assert!(body.contains("代你回答了 2 条"), "{body}");
    assert!(
        body.contains(&format!(
            "- seq {} · 问：幂等键用哪个 header？ → 答（seq {}）：用 Idempotency-Key。",
            q1.seq, answers[0]
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "- seq {} · 问：退款回调做了吗？ → 答（seq {}）：还没做。",
            q2.seq, answers[1]
        )),
        "{body}"
    );
    let texts = rig.surface.texts();
    let tail = texts[texts.len() - 2..].to_vec();
    assert_eq!(
        tail,
        vec![
            "[senator::claude] 在线",
            "[senator::claude] 回来了，Speaker 交还 2 条代答"
        ]
    );
    // The hand-back note itself is never mirrored.
    assert!(!texts.iter().any(|t| t.contains("代你回答了")));

    // Replaying the return hands back nothing twice.
    let surface = rig.surface.clone();
    let hub = rig.hub.clone();
    rig.stop().await;
    hub.set_cursor(&refund(), crate::CURSOR, Seq(back.seq.0 - 1))
        .await
        .expect("rewind");
    let again = Rig::start_with(hub.clone(), surface.clone(), ScriptedBrain::new()).await;
    let notes = hub
        .events(&refund())
        .into_iter()
        .filter(|e| e.kind == Kind::Note && e.from == Address::Speaker && e.to == [agent("alice")])
        .count();
    assert_eq!(notes, 1);
    assert_eq!(surface.texts().len(), texts.len());

    // Going away and back again without a takeover hands back nothing.
    again
        .append(presence("alice", PresenceState::Disconnected, 20))
        .await;
    again
        .append(presence("alice", PresenceState::Online, 21))
        .await;
    again.settle().await;
    assert!(
        !surface.texts()[texts.len()..]
            .iter()
            .any(|t| t.contains("交还")),
        "{:?}",
        surface.texts()
    );
    again.stop().await;
}

#[tokio::test]
async fn a_restart_mid_takeover_answers_once() {
    // The Speaker recorded `taken_over` and died before appending its
    // answer: the restarted one answers, without a second mark.
    let hub = alice_away().await;
    let asked = hub
        .append(question(
            agent("bob"),
            agent("alice"),
            "幂等键用哪个 header？",
        ))
        .await
        .expect("append")
        .event;
    hub.mark_taken_over(&refund(), &agent_path("alice"))
        .await
        .expect("mark")
        .expect("newly taken over");
    let brain = ScriptedBrain::new();
    brain.answers(Ok("用 Idempotency-Key。".into()));
    let rig = Rig::start_with(hub.clone(), FakeSurface::new(), brain).await;
    let events = hub.events(&refund());
    let answers = speaker_answers(&events);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].thread, Some(asked.seq));
    assert_eq!(answers[0].meta.get(TAKEOVER_META), Some(&json!(true)));
    assert_eq!(
        presence_states(&events, "alice")
            .iter()
            .filter(|s| **s == PresenceState::TakenOver)
            .count(),
        1
    );

    // And a restart after the answer landed changes nothing.
    let surface = rig.surface.clone();
    let posted = surface.texts().len();
    rig.stop().await;
    hub.set_cursor(&refund(), crate::CURSOR, Seq::ZERO)
        .await
        .expect("rewind");
    let again = Rig::start_with(hub.clone(), surface.clone(), ScriptedBrain::new()).await;
    assert_eq!(speaker_answers(&hub.events(&refund())).len(), 1);
    assert!(again.brain.questions().is_empty(), "nothing asked again");
    assert_eq!(surface.texts().len(), posted);
    again.stop().await;
}

#[tokio::test]
async fn coming_back_while_the_brain_drafts_drops_the_takeover() {
    let rig = Rig::start(alice_away().await).await;
    rig.brain.answers_after(
        Duration::from_millis(300),
        Ok("用 Idempotency-Key。".into()),
    );
    rig.append(question(
        agent("bob"),
        agent("alice"),
        "幂等键用哪个 header？",
    ))
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rig.brain.questions().len(), 1, "drafting");
    rig.append(presence("alice", PresenceState::Online, 10))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    assert!(speaker_answers(&events).is_empty(), "her session has it");
    assert!(!presence_states(&events, "alice").contains(&PresenceState::TakenOver));
}

// ----- the companion ------------------------------------------------------------------

#[tokio::test]
async fn a_companion_answer_is_labelled() {
    let rig = Rig::start(agents_hub().await).await;
    let asked = rig
        .append(question(agent("alice"), agent("bob"), "哪个 header？"))
        .await;
    rig.settle().await;
    rig.append(
        answer(agent("bob"), asked.seq, agent("alice"), "Idempotency-Key")
            .with_meta(meta::COMPANION, json!(true)),
    )
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[senator::copilot] 伴随 Idempotency-Key")
    );
    // A forged takeover flag on an agent's own answer is just its answer.
    rig.append(
        answer(agent("bob"), asked.seq, agent("alice"), "再补一句")
            .with_meta(TAKEOVER_META, json!(true)),
    )
    .await;
    rig.settle().await;
    assert_eq!(
        rig.surface.texts().last().map(String::as_str),
        Some("[senator::copilot] 再补一句")
    );
}

// ----- English ----------------------------------------------------------------------

#[tokio::test]
async fn in_english_the_takeover_escalation_and_hand_back_read_in_english() {
    let hub = alice_away().await;
    let rig = Rig::start_config(
        hub,
        FakeSurface::new(),
        ScriptedBrain::new(),
        config_in(Locale::En),
    )
    .await;
    rig.brain
        .answers(Ok("Idempotency-Key, per PROGRESS.md.".into()));
    rig.append(question(
        agent("bob"),
        agent("alice"),
        "Which header for idempotency?",
    ))
    .await;
    rig.settle().await;
    let texts = rig.surface.texts();
    assert!(
        texts.contains(&"[senator::claude] taken over by the Speaker".to_owned()),
        "{texts:?}"
    );
    assert_eq!(
        texts.last().map(String::as_str),
        Some("[speaker::claude] takeover Idempotency-Key, per PROGRESS.md.")
    );

    // Not covered: escalated, in English.
    rig.brain.answers_uncovered("Nothing about callbacks.");
    rig.append(question(
        agent("bob"),
        agent("alice"),
        "Is the callback signed?",
    ))
    .await;
    rig.settle().await;
    let reason = "[senator::claude] is away and its checkpoint does not cover this, so this needs a person: Is the callback signed?";
    assert_eq!(
        escalations(&rig.hub.events(&refund()))
            .last()
            .and_then(|(_, s)| s.reason.clone())
            .as_deref(),
        Some(reason)
    );
    assert!(
        rig.surface.texts().contains(&format!("@bob {reason}")),
        "{:?}",
        rig.surface.texts()
    );

    // Back online: one answer handed back.
    rig.append(presence("alice", PresenceState::Online, 10))
        .await;
    rig.settle().await;
    let texts = rig.surface.texts();
    assert_eq!(
        texts[texts.len() - 2..].to_vec(),
        vec![
            "[senator::claude] online",
            "[senator::claude] is back; the Speaker handed back 1 answer",
        ]
    );
    let note = rig
        .hub
        .events(&refund())
        .into_iter()
        .find(|e| e.kind == Kind::Note && e.from == Address::Speaker && e.to == [agent("alice")])
        .expect("the hand-back note");
    assert!(
        note.body.contains("answered 1 question for you:\n- seq "),
        "{}",
        note.body
    );
    for text in rig.surface.texts().iter().chain([&note.body]) {
        assert_senate_vocabulary(text);
    }
    // Who it was for is a field, not words in the body.
    let answer = speaker_answers(&rig.hub.events(&refund()))[0].clone();
    assert_eq!(answer.body, "Idempotency-Key, per PROGRESS.md.");
    assert_eq!(answer.on_behalf_of, Some(agent("alice")));
}
