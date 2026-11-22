//! The Speaker's memory across a parent and a child Senate: the parent's
//! digest is drafted from its own log only, so a child's discussion reaches
//! it only as it crossed up (an answer to a relay), never what was said
//! below.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_bridge::ipc::Registration;
use senate_core::{Address, CommitteeId, DigestPayload, DigestPoint, Hub, Kind, Locale, Seq};
use senate_speaker::{DigestOut, MemoryDraft, ScriptedBrain};

fn refund() -> CommitteeId {
    committee()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_child_secretary_digests_private_chat_and_only_the_answer_crosses_up() {
    let world = Setup::new()
        .server(|c| c.name = Some(name("acme")))
        .speaker(|s| {
            s.memory.busy_messages = 3;
            s.memory.quiet_mins = 0;
        })
        .start()
        .await;
    world.bind().await;
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        &under(
            &world,
            "tok-alice",
            "secretaries = [\"claude\", \"secretary\"]\n[model]\nmodel = \"scripted-model\"",
        ),
    );
    std::fs::write(
        desk.paths.home().join("secretary.toml"),
        "[memory]\nbusy_messages = 2\nquiet_mins = 0\n",
    )
    .unwrap();
    let child_brain = ScriptedBrain::new();
    desk.machine = Some(
        senated::Machine::start_with_brain(desk.paths.clone(), fast_timing(), child_brain.clone())
            .await
            .unwrap(),
    );
    let child = &desk.senate().daemon;
    assert_eq!(child.built_in_session(), Some(&path("secretary")));
    let office = child.hub.office().unwrap();
    assert_eq!(office.kind, senate_core::SenatorKind::Person);
    assert!(office.desk.contains(&path("secretary")));
    assert_eq!(
        desk.wait_registration().await.registration,
        Registration::Registered
    );
    let claude = desk.inbox("s-claude");
    desk.session_start("s-claude", &claude).await.unwrap();
    desk.wait_connected("claude", true).await;
    eventually("claude's seat declared above", || {
        world.daemon().hub.children().into_iter().find(|c| {
            c.seats.iter().any(|s| {
                s.senator == path("claude")
                    && s.presence == senate_core::continuity::PresenceState::Online
            })
        })
    })
    .await;

    // Below, in alice's own Senate: what stays there.
    desk.post("s-claude", "Internal: the retry rewrite is half done")
        .await;

    // Append the two sourced chat events as a gateway would on the child's hub.
    let chat = |who: &str, body: &str| {
        senate_core::NewEvent::new(refund(), Kind::Chat, senator(who), body).with_source(
            senate_core::Source {
                platform: "chat".into(),
                group_id: "child-private".into(),
                group_name: None,
                simulated: false,
                simulated_by: None,
            },
        )
    };
    let private = child
        .hub
        .append(chat("alice", "Internal: the retry audit is due Friday"))
        .await
        .unwrap()
        .event;
    let summary = "Internal: Friday audit";
    child_brain.digests(Ok(DigestOut {
        points: vec![DigestPoint {
            text: summary.into(),
            refs: vec![private.seq],
        }],
        memory: vec![MemoryDraft {
            text: summary.into(),
            refs: vec![private.seq],
            supersedes: vec![],
        }],
    }));
    child
        .hub
        .append(chat("claude", "Internal: I will review it"))
        .await
        .unwrap();
    let brief = eventually_async("the child's private memory", || async {
        let brief = child.hub.brief(&refund()).await.unwrap();
        (!brief.memory.is_empty()).then_some(brief)
    })
    .await;
    assert_eq!(brief.memory[0].text, summary);
    assert_eq!(brief.memory[0].refs, [private.seq]);
    assert_eq!(brief.memory[0].by, Address::Speaker);
    assert_eq!(brief.digest.unwrap().payload.points[0].refs, [private.seq]);
    assert!(
        child_brain.digest_inputs()[0]
            .events
            .iter()
            .any(|e| e.body == private.body)
    );

    // Above: a question crosses down and its answer comes up.
    world
        .say(senated::testing::Message::mention(
            GROUP,
            "u_pm",
            "m_mem1",
            "/tell alice Which retry limit does the callback use?",
        ))
        .await;
    let relay = eventually_async("the relay", || async {
        world
            .events_of(&refund())
            .await
            .into_iter()
            .find(|e| e.from == Address::Speaker && e.kind == Kind::Question)
    })
    .await;
    let below = eventually_async("the question below", || async {
        desk.senate()
            .daemon
            .hub
            .read(&refund(), Seq::ZERO, 200)
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.body.contains("Which retry limit") && e.kind == Kind::Question)
    })
    .await;
    desk.answer("s-claude", below.seq, "Three times, see retry.rs")
        .await;
    let answer = eventually_async("the answer above", || async {
        world
            .events_of(&refund())
            .await
            .into_iter()
            .find(|e| e.body == "Three times, see retry.rs")
    })
    .await;
    assert_eq!(answer.thread, Some(relay.seq));

    // People above talk it over: busy, and quiet at once.
    world.brain.digests(Ok(DigestOut {
        points: vec![DigestPoint {
            text: "Retries stay at three".into(),
            refs: vec![answer.seq],
        }],
        memory: vec![],
    }));
    for (who, n, text) in [
        ("u_pm", 2, "three retries then"),
        ("u_bob", 3, "fine by me, three"),
    ] {
        world
            .say(senated::testing::Message::ambient(
                GROUP,
                who,
                &format!("m_mem{n}"),
                text,
            ))
            .await;
    }
    let digest = eventually_async("the parent's digest", || async {
        world
            .events_of(&refund())
            .await
            .into_iter()
            .find_map(|e| DigestPayload::of(&e))
    })
    .await;
    assert_eq!(digest.points[0].refs, [answer.seq]);

    let inputs = world.brain.digest_inputs();
    assert_eq!(inputs.len(), 1);
    let shown: Vec<&str> = inputs[0].events.iter().map(|e| e.body.as_str()).collect();
    assert!(
        shown.contains(&"Three times, see retry.rs"),
        "what crossed up is shown: {shown:?}"
    );
    assert!(shown.contains(&"fine by me, three"), "{shown:?}");
    assert!(
        inputs[0]
            .events
            .iter()
            .all(|e| !e.body.contains("Internal:")),
        "nothing said below: {shown:?}"
    );
    assert!(
        inputs[0].events.iter().all(|e| e.committee == refund()),
        "the parent's own log"
    );
    assert!(
        world
            .events_of(&refund())
            .await
            .iter()
            .all(|e| !e.body.contains("Internal:"))
    );
    assert!(
        world
            .hub()
            .brief(&refund())
            .await
            .unwrap()
            .memory
            .is_empty()
    );
    desk.stop().await;
    world.stop().await;
}
