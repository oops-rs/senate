//! The built-in session in its roles: working the Speaker's desk only when
//! it is the first present worker, signing what it writes for the office
//! (`acted_by`), deciding crossings (forward, leave, refuse), and covering
//! a question, never a request, for the agents whose secretary it is.

use std::sync::Arc;

use senate_core::{
    Address, CheckpointFiles, CheckpointPayload, CheckpointTrigger, CommitteeId, Event, Hub, Kind,
    Locator, NewEvent, Origin, PresencePayload, PresenceState, Roster, SenatorId, SenatorName, Seq,
    SpeakerName, meta, presence_event,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::{
    GROUP, Rig, agent, agent_path, bind_event, committee, name, path, refund, roster,
};
use crate::{Crossing, TAKEOVER_META};

/// The built-in session's name in these tests: the default.
fn me() -> SenatorName {
    path("secretary")
}

fn billing() -> CommitteeId {
    committee("billing")
}

/// pm presides; `desk` are the Speaker's secretaries, in order; each
/// `(agent, secretaries)` registers an agent's own.
fn office(desk: &[&str], agents: &[(&str, &[&str])]) -> Roster {
    let mut roster = roster()
        .with_speaker(SpeakerName::id(SenatorId::here(path("pm"))))
        .expect("pm presides")
        .with_built_in(me())
        .expect("the built-in session");
    roster.seat(path("codex"), &name("pm")).expect("codex");
    roster
        .set_secretaries(path("pm"), desk.iter().map(|s| path(s)).collect())
        .expect("the Speaker's secretaries");
    for (principal, secretaries) in agents {
        roster
            .set_secretaries(
                path(principal),
                secretaries.iter().map(|s| path(s)).collect(),
            )
            .expect("an agent's secretaries");
    }
    roster
}

async fn hub_with(roster: Roster) -> Arc<MemoryHub> {
    let hub = MemoryHub::new();
    hub.set_roster(roster);
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub
}

/// What `from` (an agent on refund-v2) means for `intended`, put on the
/// desk on the way, as the server writes it.
fn crossing(kind: Kind, from: Address, intended: &Locator, body: &str) -> NewEvent {
    NewEvent::new(refund(), kind, from, body)
        .to(Address::Speaker)
        .with_meta(meta::INTENDED, json!(intended.to_string()))
}

fn copilot_on_billing() -> Locator {
    Locator::on(billing(), Address::senator(&path("copilot")))
}

fn by_office(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| e.from == Address::Speaker && e.kind != Kind::Binding)
        .collect()
}

fn acted_by(event: &Event) -> Option<&str> {
    event.meta.get(meta::ACTED_BY).and_then(|v| v.as_str())
}

#[tokio::test]
async fn it_acts_on_the_desk_only_as_the_first_present_worker() {
    // codex is the Speaker's first secretary, the built-in session second.
    let hub = hub_with(office(&["codex", "secretary"], &[])).await;
    hub.connect(&refund(), &path("codex"));
    hub.connect(&billing(), &path("copilot"));
    let rig = Rig::start(hub).await;

    rig.append(crossing(
        Kind::Question,
        agent("alice"),
        &copilot_on_billing(),
        "账单接口的超时是多少？",
    ))
    .await;
    rig.settle().await;
    assert!(
        rig.brain.crossing_inputs().is_empty(),
        "left to codex, which is present"
    );
    assert!(by_office(&rig.hub.events(&refund())).is_empty());
    assert!(by_office(&rig.hub.events(&billing())).is_empty());

    // codex gone: the built-in session is the first present worker.
    rig.hub.disconnect(&refund(), &path("codex"));
    let asked = rig
        .append(crossing(
            Kind::Question,
            agent("alice"),
            &copilot_on_billing(),
            "账单的重试次数是多少？",
        ))
        .await;
    rig.settle().await;
    assert_eq!(rig.brain.crossing_inputs().len(), 1);

    // Forwarded: a new root at the destination, from the office, with the
    // asker's qualified name and thread.
    let billing_events = rig.hub.events(&billing());
    let forwarded = by_office(&billing_events);
    assert_eq!(forwarded.len(), 1, "{billing_events:#?}");
    let root = forwarded[0];
    assert_eq!(root.kind, Kind::Question);
    assert_eq!(root.thread, None);
    assert_eq!(root.to, vec![Address::senator(&path("copilot"))]);
    assert_eq!(root.body, "账单的重试次数是多少？");
    let origin = Origin::of(root).expect("origin");
    assert_eq!(
        origin.locator,
        Locator::on(refund(), agent("alice")),
        "the asker, qualified"
    );
    assert_eq!(origin.thread, asked.seq);
    assert_eq!(acted_by(root), Some("secretary"));

    // And a note in the asker's thread saying where it went.
    let refund_events = rig.hub.events(&refund());
    let notes = by_office(&refund_events);
    assert_eq!(notes.len(), 1, "{refund_events:#?}");
    let note = notes[0];
    assert_eq!(note.kind, Kind::Note);
    assert_eq!(note.thread, Some(asked.seq));
    assert_eq!(note.to, vec![agent("alice")]);
    assert_eq!(
        note.meta.get(meta::FORWARDED_TO),
        Some(&json!(copilot_on_billing().to_string()))
    );
    assert_eq!(acted_by(note), Some("secretary"));
    assert!(
        note.body.contains("[billing::senator::copilot]"),
        "{}",
        note.body
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_request_crossing_is_left_for_the_speaker() {
    let hub = hub_with(office(&["secretary"], &[])).await;
    let rig = Rig::start(hub).await;
    rig.append(crossing(
        Kind::Request,
        agent("alice"),
        &copilot_on_billing(),
        "把账单的重试次数改成 5",
    ))
    .await;
    rig.settle().await;
    assert_eq!(rig.brain.crossing_inputs().len(), 1, "it was considered");
    assert!(by_office(&rig.hub.events(&refund())).is_empty());
    assert!(rig.hub.events(&billing()).is_empty(), "nothing delivered");
    rig.stop().await;
}

#[tokio::test]
async fn a_refused_crossing_is_answered_with_the_reason() {
    let hub = hub_with(office(&["secretary"], &[])).await;
    let rig = Rig::start(hub).await;
    rig.brain
        .decides_crossing(Ok(Crossing::Refuse("账单的事请先问你的 chair".into())));
    let asked = rig
        .append(crossing(
            Kind::Question,
            agent("alice"),
            &copilot_on_billing(),
            "账单接口的超时是多少？",
        ))
        .await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let answers = by_office(&events);
    assert_eq!(answers.len(), 1, "{events:#?}");
    let answer = answers[0];
    assert_eq!(answer.kind, Kind::Answer);
    assert_eq!(answer.thread, Some(asked.seq));
    assert_eq!(answer.to, vec![agent("alice")]);
    assert!(
        answer.body.contains("账单的事请先问你的 chair"),
        "{}",
        answer.body
    );
    assert_eq!(acted_by(answer), Some("secretary"));
    assert!(rig.hub.events(&billing()).is_empty());
    rig.stop().await;
}

#[tokio::test]
async fn a_duty_the_configuration_leaves_out_stays_on_the_desk() {
    let hub = hub_with(office(&["secretary"], &[])).await;
    let mut config = super::rig::config();
    config.locale = senate_core::Locale::Zh;
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join("refund-v2.secretary.toml"),
        "duties = [\"route\", \"cover\", \"triage\"]\n",
    )
    .expect("write");
    let secretary = Arc::new(crate::SecretaryConfigs::new(
        Some(dir.path().into()),
        crate::BaseLayer::of_config(&config),
    ));
    let (unbound, rx) = tokio::sync::mpsc::channel(4);
    let surface = super::surface::FakeSurface::new();
    let brain = crate::ScriptedBrain::new();
    let args = crate::SpeakerArgs::new(hub.clone(), surface.clone(), brain.clone(), config, rx)
        .with_secretary(secretary);
    let rig = Rig::spawn(hub, surface, brain, args, unbound).await;
    rig.append(crossing(
        Kind::Question,
        agent("alice"),
        &copilot_on_billing(),
        "账单接口的超时是多少？",
    ))
    .await;
    rig.settle().await;
    assert_eq!(rig.brain.crossing_inputs().len(), 1, "decided: forward");
    assert!(
        by_office(&rig.hub.events(&refund())).is_empty(),
        "no forward duty"
    );
    assert!(rig.hub.events(&billing()).is_empty());
    rig.stop().await;
}

// ----- secretary to agents --------------------------------------------------------

const COMMIT: &str = "a41f9c2e0000000000000000000000000000beef";

fn away(who: &SenatorName, offset_ms: i64) -> NewEvent {
    let since =
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(1_800_000_000_000 + offset_ms)
            .expect("a time");
    presence_event(
        refund(),
        &PresencePayload {
            paused: false,
            agent: who.clone(),
            state: PresenceState::Absent,
            since,
            since_seq: Seq(0),
            previous: None,
            last_seen: None,
            cursor: None,
            lag: None,
            new_owner: None,
        },
    )
}

fn checkpoint(who: &SenatorName, progress: &str) -> NewEvent {
    let payload = CheckpointPayload {
        branch: format!("work/{who}"),
        commit: COMMIT.into(),
        pushed: true,
        remote_url: None,
        files: CheckpointFiles {
            progress: Some(progress.into()),
            decisions: None,
            open: None,
        },
        session_transcript: false,
        trigger: CheckpointTrigger::Tool,
    };
    NewEvent::new(
        refund(),
        Kind::Checkpoint,
        Address::senator(who),
        "checkpoint",
    )
    .with_meta(meta::CHECKPOINT, payload.value())
}

/// `agents` away, each with a checkpoint of its own.
async fn away_with_notes(roster: Roster, agents: &[(&str, &str)]) -> Arc<MemoryHub> {
    let hub = hub_with(roster).await;
    for (i, (who, progress)) in agents.iter().enumerate() {
        let who = path(who);
        hub.append(away(&who, i as i64)).await.expect("absent");
        hub.append(checkpoint(&who, progress))
            .await
            .expect("checkpoint");
    }
    hub
}

fn by_secretary(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| e.kind == Kind::Answer && e.from == Address::senator(&me()))
        .collect()
}

#[tokio::test]
async fn as_an_agents_secretary_it_covers_a_question_never_a_request() {
    // The Speaker has no secretary: the built-in session does not work the
    // desk, only claude's.
    let roster = office(&[], &[("claude", &["secretary"])]);
    let hub = away_with_notes(roster, &[("claude", "- 幂等键用 Idempotency-Key")]).await;
    let rig = Rig::start(hub).await;

    rig.append(
        NewEvent::new(refund(), Kind::Request, agent("bob"), "把 header 改了").to(agent("alice")),
    )
    .await;
    rig.settle().await;
    assert!(
        rig.brain.questions().is_empty(),
        "a request waits for claude"
    );
    assert!(by_secretary(&rig.hub.events(&refund())).is_empty());

    rig.brain
        .answers(Ok("据检查点，用 Idempotency-Key。".into()));
    let asked = rig
        .append(
            NewEvent::new(
                refund(),
                Kind::Question,
                agent("bob"),
                "幂等键用哪个 header？",
            )
            .to(agent("alice")),
        )
        .await;
    rig.settle().await;
    let questions = rig.brain.questions();
    assert_eq!(questions.len(), 1);
    let takeover = questions[0].takeover.as_ref().expect("from its notes");
    assert_eq!(takeover.checkpoint.agent, agent_path("alice"));

    let events = rig.hub.events(&refund());
    let answers = by_secretary(&events);
    assert_eq!(answers.len(), 1, "{events:#?}");
    let answer = answers[0];
    assert_eq!(answer.on_behalf_of, Some(agent("alice")));
    assert_eq!(answer.thread, Some(asked.seq));
    assert_eq!(answer.to, vec![agent("bob")]);
    assert_eq!(answer.meta.get(TAKEOVER_META), Some(&json!(true)));
    assert!(
        by_office(&events).is_empty(),
        "nothing written as the office: it is not on the desk"
    );
    rig.stop().await;
}

#[tokio::test]
async fn it_may_be_the_secretary_of_two_agents() {
    let roster = office(
        &[],
        &[("claude", &["secretary"]), ("copilot", &["secretary"])],
    );
    let hub = away_with_notes(
        roster,
        &[
            ("claude", "- 退款：幂等键做完了"),
            ("copilot", "- 账单：重试做了一半"),
        ],
    )
    .await;
    let rig = Rig::start(hub).await;

    rig.brain.answers(Ok("幂等键做完了。".into()));
    rig.append(
        NewEvent::new(refund(), Kind::Question, agent("pm"), "幂等键做完了吗？").to(agent("alice")),
    )
    .await;
    rig.settle().await;
    rig.brain.answers(Ok("重试做了一半。".into()));
    rig.append(
        NewEvent::new(refund(), Kind::Question, agent("pm"), "重试做完了吗？").to(agent("bob")),
    )
    .await;
    rig.settle().await;

    let questions = rig.brain.questions();
    assert_eq!(questions.len(), 2);
    let notes_of = |i: usize| {
        questions[i]
            .takeover
            .as_ref()
            .expect("a cover")
            .checkpoint
            .agent
            .clone()
    };
    assert_eq!(notes_of(0), agent_path("alice"), "claude's own notes");
    assert_eq!(notes_of(1), agent_path("bob"), "copilot's own notes");
    let events = rig.hub.events(&refund());
    let answers = by_secretary(&events);
    assert_eq!(answers.len(), 2, "{events:#?}");
    assert_eq!(answers[0].on_behalf_of, Some(agent("alice")));
    assert_eq!(answers[1].on_behalf_of, Some(agent("bob")));
    rig.stop().await;
}

#[tokio::test]
async fn a_connected_secretary_earlier_in_the_list_covers_first() {
    // codex before the built-in session in claude's list, and present.
    let roster = office(&[], &[("claude", &["codex", "secretary"])]);
    let hub = away_with_notes(roster, &[("claude", "- 幂等键用 Idempotency-Key")]).await;
    hub.connect(&refund(), &path("codex"));
    let rig = Rig::start(hub).await;
    rig.append(
        NewEvent::new(
            refund(),
            Kind::Question,
            agent("bob"),
            "幂等键用哪个 header？",
        )
        .to(agent("alice")),
    )
    .await;
    rig.settle().await;
    assert!(rig.brain.questions().is_empty(), "left to codex");
    assert!(by_secretary(&rig.hub.events(&refund())).is_empty());
    rig.stop().await;
}

// ----- configuration errors, told to the Speaker ----------------------------------

fn notes_to_the_speaker(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| {
            e.kind == Kind::Note
                && e.from == Address::Speaker
                && e.to == [Address::senator(&path("pm"))]
        })
        .collect()
}

#[tokio::test]
async fn the_speaker_is_told_once_per_change_of_the_configuration_errors() {
    let hub = hub_with(office(&["secretary"], &[])).await;
    let dir = tempfile::tempdir().expect("dir");
    let file = dir.path().join("secretary.toml");
    std::fs::write(&file, "mode = \"loud\"\n").expect("write");
    let mut config = super::rig::config();
    config.locale = senate_core::Locale::En;
    let secretary = Arc::new(crate::SecretaryConfigs::new(
        Some(dir.path().into()),
        crate::BaseLayer::of_config(&config),
    ));
    let (unbound, rx) = tokio::sync::mpsc::channel(4);
    let surface = super::surface::FakeSurface::new();
    let brain = crate::ScriptedBrain::new();
    let args = crate::SpeakerArgs::new(hub.clone(), surface.clone(), brain.clone(), config, rx)
        .with_secretary(secretary);
    let rig = Rig::spawn(hub, surface, brain, args, unbound).await;
    let say = |body: &'static str| NewEvent::new(refund(), Kind::Note, agent("alice"), body);

    rig.append(say("one")).await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    let told = notes_to_the_speaker(&events);
    assert_eq!(told.len(), 1, "{events:#?}");
    assert!(
        told[0].body.contains("secretary.toml:1"),
        "{}",
        told[0].body
    );
    assert!(
        told[0]
            .body
            .starts_with("The built-in session's configuration files have errors"),
        "{}",
        told[0].body
    );
    assert_eq!(acted_by(told[0]), Some("secretary"));

    // The same errors again: nothing more.
    rig.append(say("two")).await;
    rig.settle().await;
    assert_eq!(notes_to_the_speaker(&rig.hub.events(&refund())).len(), 1);

    // A different error is a change.
    std::fs::write(
        &file,
        "# edited\nmode = \"loud\"\nanswer_fallback_secs = \"x\"\n",
    )
    .expect("write");
    rig.append(say("three")).await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    assert_eq!(notes_to_the_speaker(&events).len(), 2, "{events:#?}");

    // Fixed: nothing is told.
    std::fs::write(&file, "answer_fallback_secs = 30\n").expect("write");
    rig.append(say("four")).await;
    rig.settle().await;
    assert_eq!(notes_to_the_speaker(&rig.hub.events(&refund())).len(), 2);
    rig.stop().await;
}

#[test]
fn the_configuration_notice_reads_in_both_languages() {
    use senate_core::Locale;
    let problems = [crate::Problem {
        file: "/h/secretary.toml".into(),
        line: Some(3),
        message: "mode must be one of".into(),
    }];
    let zh = crate::render::config_problems_note(Locale::Zh, &problems);
    let en = crate::render::config_problems_note(Locale::En, &problems);
    assert_ne!(zh, en);
    assert!(zh.starts_with("内置会话的配置文件有错误"), "{zh}");
    assert!(
        en.starts_with("The built-in session's configuration files"),
        "{en}"
    );
    for text in [&zh, &en] {
        assert!(
            text.contains("/h/secretary.toml:3: mode must be one of"),
            "{text}"
        );
    }
}

// ----- a child Senate that is offline ---------------------------------------------------

/// pm's child Senate, last seen ten minutes ago.
fn offline_child(hub: &MemoryHub) -> Address {
    hub.add_child("north-desk", "pm");
    hub.declare_seats(
        "north-desk",
        &refund(),
        &[("responder", PresenceState::Disconnected)],
    );
    hub.child_seen(
        "north-desk",
        Some(chrono::Utc::now() - chrono::Duration::minutes(10)),
    );
    Address::Child {
        senate: name("north-desk"),
        committee: refund(),
    }
}

/// With a person presiding, what to do about a question for an offline
/// child Senate falls to the desk in its order: the built-in session
/// decides only as the first present worker, and signs what it writes.
#[tokio::test]
async fn for_an_offline_child_senate_it_decides_only_as_the_first_present_worker() {
    let hub = hub_with(office(&["codex", "secretary"], &[])).await;
    let desk = offline_child(&hub);
    hub.connect(&refund(), &path("codex"));
    let rig = Rig::start(hub).await;

    rig.append(
        NewEvent::new(refund(), Kind::Question, agent("alice"), "退款上限是多少？")
            .to(desk.clone()),
    )
    .await;
    rig.settle().await;
    assert!(
        rig.brain.offline_inputs().is_empty(),
        "left to codex, which is present"
    );
    assert!(by_office(&rig.hub.events(&refund())).is_empty());

    // codex gone: the built-in session is the first present worker.
    rig.hub.disconnect(&refund(), &path("codex"));
    let asked = rig
        .append(
            NewEvent::new(
                refund(),
                Kind::Question,
                agent("alice"),
                "网关用哪个 header？",
            )
            .to(desk),
        )
        .await;
    rig.settle().await;
    assert_eq!(rig.brain.offline_inputs().len(), 1);
    let events = rig.hub.events(&refund());
    let said = by_office(&events);
    assert_eq!(said.len(), 1, "{said:#?}");
    assert_eq!(said[0].kind, Kind::Note, "the rules hold it");
    assert_eq!(said[0].thread, Some(asked.seq));
    assert_eq!(said[0].to, vec![agent("alice")]);
    assert_eq!(said[0].meta[meta::CHILD_OFFLINE], json!("north-desk"));
    assert_eq!(acted_by(said[0]), Some("secretary"));
    rig.stop().await;
}

/// Answering for an absent senator is the cover duty: without it the
/// built-in session holds the question and does not ask the Brain.
#[tokio::test]
async fn without_the_cover_duty_a_question_for_an_offline_child_senate_is_held_unasked() {
    let hub = hub_with(office(&["secretary"], &[])).await;
    let desk = offline_child(&hub);
    let mut config = super::rig::config();
    config.locale = senate_core::Locale::Zh;
    let dir = tempfile::tempdir().expect("dir");
    std::fs::write(
        dir.path().join("refund-v2.secretary.toml"),
        "duties = [\"route\", \"forward\", \"triage\"]\n",
    )
    .expect("write");
    let secretary = Arc::new(crate::SecretaryConfigs::new(
        Some(dir.path().into()),
        crate::BaseLayer::of_config(&config),
    ));
    let (unbound, rx) = tokio::sync::mpsc::channel(4);
    let surface = super::surface::FakeSurface::new();
    let brain = crate::ScriptedBrain::new();
    brain.decides_offline(Ok(crate::Offline::Answer("不该用到".into())));
    let args = crate::SpeakerArgs::new(hub.clone(), surface.clone(), brain.clone(), config, rx)
        .with_secretary(secretary);
    let rig = Rig::spawn(hub, surface, brain, args, unbound).await;
    rig.append(
        NewEvent::new(refund(), Kind::Question, agent("alice"), "退款上限是多少？").to(desk),
    )
    .await;
    rig.settle().await;
    assert!(rig.brain.offline_inputs().is_empty(), "no cover duty");
    let events = rig.hub.events(&refund());
    let said = by_office(&events);
    assert_eq!(said.len(), 1, "{said:#?}");
    assert_eq!(said[0].kind, Kind::Note);
    rig.stop().await;
}

// ----- a desk item answered above -------------------------------------------------------

/// An item that came down from the Senate above, with the answer the
/// office there gave in this Senate's name already under it (this Senate
/// was offline): the desk has nothing to do with it. It is not forwarded
/// to the seat it was meant for, and the Brain is not asked.
#[tokio::test]
async fn a_desk_item_answered_above_is_not_forwarded() {
    let hub = hub_with(office(&["secretary"], &[])).await;
    hub.connect(&billing(), &path("copilot"));
    let from_above = Origin {
        locator: "/acme::refund-v2::senator::carol".parse().expect("locator"),
        thread: Seq(7),
    }
    .value();
    let item = hub
        .append(
            crossing(
                Kind::Question,
                Address::Committee,
                &copilot_on_billing(),
                "账单接口的超时是多少？",
            )
            .with_meta(meta::ORIGIN, from_above.clone()),
        )
        .await
        .expect("the item")
        .event;
    hub.append(
        NewEvent::new(
            refund(),
            Kind::Answer,
            Address::Committee,
            "30 秒，见 brief。",
        )
        .to(Address::Speaker)
        .in_thread(item.seq)
        .with_meta(meta::ORIGIN, from_above),
    )
    .await
    .expect("the answer from above");
    let rig = Rig::start(hub).await;
    rig.settle().await;
    assert!(
        rig.brain.crossing_inputs().is_empty(),
        "answered above: nothing to decide"
    );
    assert!(by_office(&rig.hub.events(&refund())).is_empty());
    assert!(rig.hub.events(&billing()).is_empty(), "not forwarded");
    rig.stop().await;
}
