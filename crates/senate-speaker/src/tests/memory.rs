//! Digests of the discussion and the memory points they add, with a
//! scripted Brain: when a digest is written, what it may cite, what it may
//! supersede, and what it never sees.

use std::sync::Arc;
use std::time::Duration;

use senate_core::{
    Address, CommitteeId, DigestPayload, DigestPoint, DigestTrigger, Event, Hub, Kind,
    MemoryPayload, Mirror, NewEvent, Seq, Source, meta,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::{GROUP, Rig, bind_event, committee, config_in, sender, source};
use super::surface::{FakeSurface, PLATFORM};
use crate::{DigestOut, MemoryDraft, ScriptedBrain, SpeakerConfig, SpeakerMsg};

fn orchard() -> CommitteeId {
    committee("orchard")
}

/// A message said in the group, not to the Speaker, as the gateway records
/// it.
fn said(from: &str, text: &str, n: u32) -> NewEvent {
    NewEvent::new(orchard(), Kind::Chat, sender(from), text)
        .with_source(source())
        .with_mirror(Mirror {
            platform: PLATFORM.into(),
            message_ref: format!("m_said_{n}"),
            thread_ref: None,
        })
        .with_meta("addressing", json!("ambient"))
}

fn simulated(from: &str, text: &str, n: u32) -> NewEvent {
    said(from, text, n).with_source(Source {
        simulated: true,
        ..source()
    })
}

fn point(text: &str, refs: &[Seq]) -> DigestPoint {
    DigestPoint {
        text: text.into(),
        refs: refs.to_vec(),
    }
}

fn draft(text: &str, refs: &[Seq], supersedes: &[Seq]) -> MemoryDraft {
    MemoryDraft {
        text: text.into(),
        refs: refs.to_vec(),
        supersedes: supersedes.to_vec(),
    }
}

fn digests(hub: &MemoryHub) -> Vec<(Event, DigestPayload)> {
    hub.events(&orchard())
        .into_iter()
        .filter_map(|e| DigestPayload::of(&e).map(|d| (e, d)))
        .collect()
}

fn memory_points(hub: &MemoryHub) -> Vec<(Event, MemoryPayload)> {
    hub.events(&orchard())
        .into_iter()
        .filter_map(|e| MemoryPayload::of(&e).map(|m| (e, m)))
        .collect()
}

async fn bound(config: SpeakerConfig) -> Rig {
    let hub = MemoryHub::new();
    hub.append(bind_event(&orchard(), GROUP))
        .await
        .expect("bind");
    Rig::start_config(hub, FakeSurface::new(), ScriptedBrain::new(), config).await
}

async fn due(rig: &Rig, trigger: DigestTrigger) {
    rig.speaker
        .cast(SpeakerMsg::DigestDue {
            committee: orchard(),
            trigger,
        })
        .expect("speaker listens");
    rig.settle().await;
}

#[tokio::test]
async fn the_daily_digest_cites_only_what_it_was_shown_and_sees_no_simulated_chat() {
    let rig = bound(config_in(senate_core::Locale::En)).await;
    let a = rig
        .append(said("alice", "Shall we ship on Monday?", 1))
        .await;
    let test = rig
        .append(simulated("bob", "This is only a rehearsal", 2))
        .await;
    let b = rig.append(said("bob", "Monday works for me", 3)).await;
    rig.settle().await;
    rig.brain.digests(Ok(DigestOut {
        points: vec![
            point("Release planned for Monday", &[a.seq, b.seq]),
            point("A rehearsal happened", &[test.seq]),
            point("Something from elsewhere", &[Seq(9_999)]),
        ],
        memory: vec![draft(
            "The release is on Monday",
            &[a.seq, b.seq, test.seq],
            &[],
        )],
    }));
    let posts_before = rig.surface.posts().len();
    due(&rig, DigestTrigger::Daily).await;

    let inputs = rig.brain.digest_inputs();
    assert_eq!(inputs.len(), 1);
    let shown: Vec<Seq> = inputs[0].events.iter().map(|e| e.seq).collect();
    assert_eq!(
        shown,
        [a.seq, b.seq],
        "the simulated message is never shown"
    );
    assert_eq!(inputs[0].trigger, DigestTrigger::Daily);

    let written = digests(&rig.hub);
    assert_eq!(written.len(), 1);
    let (event, digest) = &written[0];
    assert_eq!(event.from, Address::Speaker);
    assert_eq!(event.body, "- Release planned for Monday");
    assert_eq!(
        digest.points,
        [point("Release planned for Monday", &[a.seq, b.seq])]
    );
    assert_eq!((digest.from, digest.through), (a.seq, b.seq));
    let points = memory_points(&rig.hub);
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].0.body, "The release is on Monday");
    assert_eq!(
        points[0].1,
        MemoryPayload::Point {
            refs: vec![a.seq, b.seq],
            supersedes: vec![],
            digest: Some(event.seq),
        }
    );
    assert_eq!(
        rig.surface.posts().len(),
        posts_before,
        "neither the digest nor memory is posted"
    );
    let brief = rig.hub.brief(&orchard()).await.unwrap();
    assert_eq!(brief.memory.len(), 1);
    assert_eq!(brief.digest.unwrap().seq, event.seq);

    // Nothing new since: no digest, not even a Brain call.
    rig.append(simulated("bob", "Another rehearsal", 4)).await;
    due(&rig, DigestTrigger::Daily).await;
    assert_eq!(rig.brain.digest_inputs().len(), 1);
    assert_eq!(digests(&rig.hub).len(), 1);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_busy_stretch_gets_a_digest_once_it_goes_quiet() {
    let mut config = config_in(senate_core::Locale::En);
    config.memory.busy_messages = 3;
    config.memory.quiet_mins = 1;
    let rig = bound(config).await;

    // One person alone is not a busy discussion.
    for n in 0..3 {
        rig.append(said("alice", &format!("thinking aloud {n}"), n))
            .await;
    }
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(61)).await;
    rig.settle().await;
    assert!(rig.brain.digest_inputs().is_empty());

    let reply = rig
        .append(said("bob", "the cache should be shared", 10))
        .await;
    rig.settle().await;
    // Still talking: a newer message moves the quiet time on.
    tokio::time::advance(Duration::from_secs(30)).await;
    let last = rig.append(said("alice", "agreed, shared", 11)).await;
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(45)).await;
    rig.settle().await;
    assert!(
        rig.brain.digest_inputs().is_empty(),
        "not quiet long enough"
    );

    rig.brain.digests(Ok(DigestOut {
        points: vec![point("The cache will be shared", &[reply.seq, last.seq])],
        memory: vec![],
    }));
    tokio::time::advance(Duration::from_secs(20)).await;
    rig.settle().await;
    let inputs = rig.brain.digest_inputs();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].trigger, DigestTrigger::Quiet);
    assert_eq!(inputs[0].events.len(), 5);
    let written = digests(&rig.hub);
    assert_eq!(written.len(), 1);
    assert_eq!(written[0].1.trigger, DigestTrigger::Quiet);
    assert_eq!(written[0].1.through, last.seq);
    rig.stop().await;
}

#[tokio::test]
async fn a_newer_point_supersedes_the_speakers_but_never_a_persons() {
    let rig = bound(config_in(senate_core::Locale::En)).await;
    let point_of = |text: &str| {
        NewEvent::new(orchard(), Kind::Note, Address::Speaker, text).with_meta(
            meta::MEMORY,
            MemoryPayload::Point {
                refs: vec![Seq(1)],
                supersedes: vec![],
                digest: None,
            }
            .to_value(),
        )
    };
    let noon = rig.append(point_of("Deploys happen at noon")).await;
    let owner = rig
        .append(point_of("Billing is owned by the platform team"))
        .await;
    let corrected = rig
        .append(
            NewEvent::new(
                orchard(),
                Kind::Note,
                sender("alice"),
                "Billing is owned by the payments team",
            )
            .with_meta(
                meta::MEMORY,
                MemoryPayload::Correct {
                    target: owner.seq,
                    refs: vec![],
                }
                .to_value(),
            ),
        )
        .await;
    let a = rig
        .append(said("alice", "we deploy at night from now on", 1))
        .await;
    let b = rig
        .append(said("bob", "ok, nights. and billing moves to us", 2))
        .await;
    rig.settle().await;
    rig.brain.digests(Ok(DigestOut {
        points: vec![point("Deploys move to night", &[a.seq, b.seq])],
        memory: vec![
            draft("Deploys happen at night", &[a.seq], &[noon.seq]),
            draft(
                "Billing is owned by the core team",
                &[b.seq],
                &[corrected.seq],
            ),
        ],
    }));
    due(&rig, DigestTrigger::Daily).await;

    let shown = &rig.brain.digest_inputs()[0].memory;
    assert_eq!(
        shown.iter().map(|p| p.seq).collect::<Vec<_>>(),
        [noon.seq, corrected.seq],
        "the Brain sees the points in force"
    );
    assert!(shown[1].by_person());
    let brief = rig.hub.brief(&orchard()).await.unwrap();
    let texts: Vec<&str> = brief.memory.iter().map(|p| p.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "Billing is owned by the payments team",
            "Deploys happen at night",
        ],
        "noon is replaced; the person's correction stands, and nothing replaces it"
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_restarted_speaker_does_not_digest_the_same_discussion_again() {
    let hub = MemoryHub::new();
    hub.append(bind_event(&orchard(), GROUP))
        .await
        .expect("bind");
    let brain = ScriptedBrain::new();
    let rig = Rig::start_config(
        Arc::clone(&hub),
        FakeSurface::new(),
        Arc::clone(&brain),
        config_in(senate_core::Locale::En),
    )
    .await;
    let a = rig.append(said("alice", "the schema is final", 1)).await;
    rig.settle().await;
    brain.digests(Ok(DigestOut {
        points: vec![point("The schema is final", &[a.seq])],
        memory: vec![],
    }));
    due(&rig, DigestTrigger::Daily).await;
    rig.stop().await;

    let rig = Rig::start_config(
        Arc::clone(&hub),
        FakeSurface::new(),
        Arc::clone(&brain),
        config_in(senate_core::Locale::En),
    )
    .await;
    due(&rig, DigestTrigger::Daily).await;
    due(&rig, DigestTrigger::Quiet).await;
    assert_eq!(brain.digest_inputs().len(), 1);
    assert_eq!(digests(&hub).len(), 1);
    rig.stop().await;
}

#[tokio::test]
async fn a_brain_that_fails_writes_nothing_and_the_next_time_tries_again() {
    let rig = bound(config_in(senate_core::Locale::En)).await;
    let a = rig.append(said("alice", "the schema is final", 1)).await;
    rig.settle().await;
    due(&rig, DigestTrigger::Daily).await; // unscripted: unavailable
    assert!(digests(&rig.hub).is_empty());
    rig.brain.digests(Ok(DigestOut {
        points: vec![point("The schema is final", &[a.seq])],
        memory: vec![],
    }));
    due(&rig, DigestTrigger::Daily).await;
    assert_eq!(rig.brain.digest_inputs().len(), 2);
    assert_eq!(digests(&rig.hub).len(), 1);
    rig.stop().await;
}
