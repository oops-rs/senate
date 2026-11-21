//! Questions put to a seat that is gone since (thread 797, items 7 and 9):
//! the office closes them with one note to the asker, the brief stops
//! listing them, and the Brain is told those relays are not duplicates.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use senate_core::{
    Address, BriefBuilder, CommitteeSeatAction, CommitteeSeatChange, Event, EventId, Hub, Kind,
    NewEvent, PresenceState, meta,
};
use serde_json::json;

use super::hub::MemoryHub;
use super::rig::*;

/// The child Senate `wyak` declares `codex` on refund-v2, and the Speaker
/// relayed pm's question to that seat.
async fn relayed_to_a_child_seat() -> (Rig, senate_core::Seq) {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP)).await.unwrap();
    hub.add_child("wyak", "pm");
    hub.declare_seats("wyak", &refund(), &[("codex", PresenceState::Online)]);
    let question = hub
        .append(
            NewEvent::new(
                refund(),
                Kind::Question,
                Address::Speaker,
                "Commons 怎么部署？",
            )
            .to(Address::Child {
                senate: name("wyak"),
                committee: refund(),
            })
            .on_behalf_of(user("pm"))
            .with_meta(meta::INTENDED, json!("wyak::refund-v2::senator::codex")),
        )
        .await
        .unwrap()
        .event
        .seq;
    let rig = Rig::start(hub).await;
    rig.settle().await;
    (rig, question)
}

fn seat_notes(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| e.meta.contains_key(meta::SEAT_REMOVED))
        .collect()
}

fn open_questions(events: &[Event]) -> Vec<senate_core::Seq> {
    let mut brief = BriefBuilder::new(refund());
    for event in events {
        brief.observe(event);
    }
    brief.open_questions().map(|q| q.seq).collect()
}

/// A note anyone writes: something new for the Speaker to step over.
fn something_new() -> NewEvent {
    NewEvent::new(refund(), Kind::Note, user("bob"), "下午开会")
}

#[tokio::test]
async fn a_question_to_a_child_seat_closes_once_the_child_stops_declaring_it() {
    let (rig, question) = relayed_to_a_child_seat().await;
    assert!(seat_notes(&rig.hub.events(&refund())).is_empty());

    // The child's next report no longer has the seat.
    rig.hub.declare_seats("wyak", &refund(), &[]);
    rig.append(something_new()).await;
    rig.settle().await;
    rig.append(something_new()).await;
    rig.settle().await;

    let events = rig.hub.events(&refund());
    let notes = seat_notes(&events);
    assert_eq!(notes.len(), 1, "one note, however often it is seen");
    let note = notes[0];
    assert_eq!(note.from, Address::Speaker);
    assert_eq!(note.kind, Kind::Note);
    assert_eq!(note.thread, Some(question));
    assert_eq!(note.to, vec![user("pm")], "to the asker");
    assert_eq!(
        note.meta.get(meta::SEAT_REMOVED),
        Some(&json!("wyak::refund-v2::senator::codex"))
    );
    assert!(
        note.body.contains("[wyak::refund-v2::senator::codex]"),
        "{}",
        note.body
    );
    assert!(note.body.contains("没有转给别人"), "{}", note.body);
    assert!(!open_questions(&events).contains(&question));
    // Not re-routed: nothing else went out for it.
    assert!(
        !events
            .iter()
            .any(|e| e.seq > question && e.kind == Kind::Question),
        "no question relayed again"
    );
    rig.stop().await;
}

#[tokio::test]
async fn a_child_that_has_not_reported_since_the_start_closes_nothing() {
    let (rig, question) = relayed_to_a_child_seat().await;
    rig.hub.declare_seats("wyak", &refund(), &[]);
    rig.hub.child_seen("wyak", None);
    rig.append(something_new()).await;
    rig.settle().await;
    let events = rig.hub.events(&refund());
    assert!(seat_notes(&events).is_empty());
    assert!(open_questions(&events).contains(&question));
    rig.stop().await;
}

#[tokio::test]
async fn a_question_to_an_unseated_agent_closes_with_a_note_to_the_asker() {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP)).await.unwrap();
    // Connected: the Speaker waits for its answer, and says nothing for it.
    hub.connect(&refund(), &agent_path("bob"));
    let rig = Rig::start(Arc::clone(&hub)).await;
    let to_bob = rig
        .append(question(user("alice"), agent("bob"), "重试几次？"))
        .await
        .seq;
    let to_pm = rig
        .append(question(user("alice"), user("pm"), "上限多少？"))
        .await
        .seq;
    rig.settle().await;
    let unseat = CommitteeSeatChange {
        senator: agent_path("bob"),
        action: CommitteeSeatAction::Unseat,
        by: name("pm"),
    };
    rig.append(unseat.event(refund(), EventId::new())).await;
    rig.settle().await;

    let events = rig.hub.events(&refund());
    let notes = seat_notes(&events);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert_eq!(notes[0].thread, Some(to_bob));
    assert_eq!(notes[0].to, vec![user("alice")]);
    assert!(
        notes[0].body.contains("[senator::copilot]"),
        "{}",
        notes[0].body
    );
    let open = open_questions(&events);
    assert!(!open.contains(&to_bob));
    // Said in chat, under the question's copy.
    assert!(
        rig.surface
            .texts()
            .iter()
            .any(|t| t.contains("已不在本委员会")),
        "{:?}",
        rig.surface.texts()
    );
    assert!(open.contains(&to_pm), "a question to a person stays open");
    rig.stop().await;
}
