//! Decisions proposed from discussion, with a scripted Brain: who must
//! agree, how often the Speaker asks, and that a person's yes decides.

use std::time::Duration;

use senate_core::{
    Address, CommitteeId, Event, Hub, Kind, Mirror, NewEvent, PostTarget, Seq, Source, meta,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::{GROUP, Rig, STRANGER, bind_event, committee, config_in, sender, source, user};
use super::surface::{FakeSurface, PLATFORM};
use crate::{Agreement, ScriptedBrain};

fn orchard() -> CommitteeId {
    committee("orchard")
}

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

fn reply(from: &str, text: &str, n: u32, root: Seq, root_ref: &str) -> NewEvent {
    NewEvent::new(orchard(), Kind::Chat, sender(from), text)
        .in_thread(root)
        .with_source(source())
        .with_mirror(Mirror {
            platform: PLATFORM.into(),
            message_ref: format!("m_reply_{n}"),
            thread_ref: Some(format!("t_{root_ref}")),
        })
        .with_meta("addressing", json!("reply"))
        .with_meta("reply_to_message_ref", json!(root_ref))
}

fn agreed(statement: &str, topic: &str, refs: &[Seq], agreeing: &[&str]) -> Agreement {
    Agreement {
        statement: statement.into(),
        topic: topic.into(),
        refs: refs.to_vec(),
        agreeing: agreeing.iter().map(|h| user(h)).collect(),
    }
}

fn proposals(hub: &MemoryHub) -> Vec<Event> {
    hub.events(&orchard())
        .into_iter()
        .filter(|e| e.from == Address::Speaker && e.meta.contains_key(meta::SOURCES))
        .filter(|e| e.kind == Kind::Note)
        .collect()
}

fn decisions(hub: &MemoryHub) -> Vec<Event> {
    hub.events(&orchard())
        .into_iter()
        .filter(|e| e.kind == Kind::Decision)
        .collect()
}

async fn bound() -> Rig {
    let hub = MemoryHub::new();
    hub.append(bind_event(&orchard(), GROUP))
        .await
        .expect("bind");
    Rig::start_config(
        hub,
        FakeSurface::new(),
        ScriptedBrain::new(),
        config_in(senate_core::Locale::En),
    )
    .await
}

/// Let the burst go quiet for longer than `agreement_quiet_secs`.
async fn quiet(rig: &Rig) {
    rig.settle().await;
    tokio::time::advance(Duration::from_secs(121)).await;
    rig.settle().await;
}

#[tokio::test(start_paused = true)]
async fn two_people_agreeing_are_asked_and_a_persons_yes_records_it() {
    let rig = bound().await;
    let a = rig
        .append(said("alice", "Let's cap retries at three", 1))
        .await;
    let b = rig.append(said("bob", "Agreed, three retries", 2)).await;
    rig.brain.spots(Ok(Some(agreed(
        "Retries are capped at three",
        "retry cap",
        &[a.seq, b.seq],
        &["alice", "bob"],
    ))));
    quiet(&rig).await;

    let inputs = rig.brain.agreement_inputs();
    assert_eq!(inputs.len(), 1);
    assert_eq!(
        inputs[0].events.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [a.seq, b.seq]
    );
    let proposed = proposals(&rig.hub);
    assert_eq!(proposed.len(), 1);
    let proposal = &proposed[0];
    assert_eq!(proposal.body, "Retries are capped at three");
    assert_eq!(
        proposal.meta.get(meta::SOURCES),
        Some(&json!([a.seq.0, b.seq.0]))
    );
    let posts = rig.surface.posts();
    let asked = posts.last().expect("asked in the group");
    assert_eq!(
        asked.target,
        PostTarget::NewRoot {
            group_id: GROUP.into()
        }
    );
    assert_eq!(
        asked.markdown,
        format!(
            "Record this as a decision? Retries are capped at three (seq {}). Reply \"yes\" to confirm.",
            proposal.seq
        )
    );
    let root_ref = asked.message_ref.clone();

    // A guest's yes decides nothing; a person's does.
    rig.append(reply(STRANGER, "yes", 3, proposal.seq, &root_ref))
        .await;
    rig.settle().await;
    assert!(decisions(&rig.hub).is_empty());
    rig.append(reply("pm", "yes", 4, proposal.seq, &root_ref))
        .await;
    rig.settle().await;
    let recorded = decisions(&rig.hub);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].body, "Retries are capped at three");
    assert_eq!(recorded[0].on_behalf_of, Some(user("pm")));
    assert_eq!(recorded[0].thread, Some(proposal.seq));
    assert_eq!(
        recorded[0].meta.get(meta::SOURCES),
        Some(&json!([a.seq.0, b.seq.0]))
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn one_person_alone_or_simulated_chat_is_never_checked() {
    let rig = bound().await;
    rig.append(said("alice", "I think we should cap retries", 1))
        .await;
    rig.append(said("alice", "at three, I mean", 2)).await;
    quiet(&rig).await;
    let rehearsal = |from: &str, text: &str, n: u32| {
        said(from, text, n).with_source(Source {
            simulated: true,
            ..source()
        })
    };
    rig.append(rehearsal("bob", "agreed", 3)).await;
    rig.append(rehearsal("pm", "agreed too", 4)).await;
    quiet(&rig).await;
    assert!(rig.brain.agreement_inputs().is_empty());
    assert!(proposals(&rig.hub).is_empty());
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_agreement_that_does_not_hold_or_repeats_a_topic_is_not_proposed() {
    let rig = bound().await;
    let a = rig.append(said("alice", "Ship on Monday?", 1)).await;
    rig.append(said("bob", "Monday is fine", 2)).await;
    // Only one of the people the Brain names wrote what it cites.
    rig.brain.spots(Ok(Some(agreed(
        "Ship on Monday",
        "release day",
        &[a.seq],
        &["alice", "bob"],
    ))));
    quiet(&rig).await;
    assert!(proposals(&rig.hub).is_empty());

    // A new burst on the same topic, proposed once.
    let c = rig.append(said("alice", "So Monday, then", 3)).await;
    let d = rig.append(said("bob", "Yes, Monday", 4)).await;
    rig.brain.spots(Ok(Some(agreed(
        "Ship on Monday",
        "Release day",
        &[c.seq, d.seq],
        &["alice", "bob"],
    ))));
    quiet(&rig).await;
    assert_eq!(proposals(&rig.hub).len(), 1);

    let e = rig.append(said("alice", "Monday it is", 5)).await;
    let f = rig.append(said("bob", "Monday!", 6)).await;
    rig.brain.spots(Ok(Some(agreed(
        "We ship on Monday",
        "release  DAY",
        &[e.seq, f.seq],
        &["alice", "bob"],
    ))));
    quiet(&rig).await;
    assert_eq!(rig.brain.agreement_inputs().len(), 3);
    assert_eq!(proposals(&rig.hub).len(), 1, "at most once per topic");
    rig.stop().await;
}
