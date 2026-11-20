//! A question for a child Senate that is offline, with a person presiding
//! and no built-in session: the Senate puts it on the Speaker's docket,
//! and the session working the desk answers it on the child's behalf or
//! tells the one who asked that it is held. Either way it leaves the
//! docket, and the question itself still waits for the child.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_bridge::client::ApiClient;
use senate_bridge::ipc::{DocketReply, Posted, Request};
use senate_bridge::mcp::SenateMcp;
use senate_core::{Address, Awaiting, ChildSenate, Event, Kind, NewEvent, Seq, meta};

fn north_desk() -> Address {
    Address::child(name("north-desk"), committee())
}

/// The questions for an offline child Senate on the desk's docket, by seq.
async fn awaiting_the_office(laptop: &Laptop) -> Option<Vec<Seq>> {
    let reply: DocketReply = laptop
        .try_ipc(Request::Docket {
            session: Laptop::session("s-claude"),
            committee: None,
        })
        .await
        .ok()?;
    Some(
        reply
            .senates
            .iter()
            .flat_map(|senate| &senate.pages)
            .flat_map(|page| &page.items)
            .filter(|item| {
                matches!(&item.awaiting, Awaiting::ChildOffline { child } if child.as_str() == "north-desk")
            })
            .map(|item| item.event.seq)
            .collect(),
    )
}

async fn event_at(world: &World, seq: Seq) -> Event {
    world
        .events_of(&committee())
        .await
        .into_iter()
        .find(|e| e.seq == seq)
        .expect("the event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_desk_answers_or_holds_a_question_for_an_offline_child_senate() {
    // bob, a person, presides; his claude works his desk. carol's child
    // Senate was last seen ten minutes ago.
    let world = team_above().await;
    let mut child = ChildSenate::new(name("north-desk"), path("carol"));
    child.last_seen = Some(chrono::Utc::now() - chrono::Duration::minutes(10));
    world.hub().register_child(child).await.unwrap();
    let mut bob = world.laptop("bob");
    bob.open().await;
    let inbox = bob.inbox("s-claude");
    bob.session_start("s-claude", &inbox).await;
    world.wait_connected(BOB_CLAUDE, true).await;
    ApiClient::new(&world.server_url(), "tok-bob")
        .unwrap()
        .set_secretaries(&path("bob"), vec![path(BOB_CLAUDE)], None)
        .await
        .unwrap();
    eventually_async("bob's claude to work the desk", || async {
        awaiting_the_office(&bob).await
    })
    .await;

    // alice asks north-desk two things; nothing answers there.
    let ask = |body: &'static str| {
        let event =
            NewEvent::new(committee(), Kind::Question, person("alice"), body).to(north_desk());
        let url = world.server_url();
        async move { speak(&url, "tok-alice", &event).await.unwrap() }
    };
    let first = ask("Is the retry cap 3?").await;
    let second = ask("Which header carries the idempotency key?").await;
    assert_eq!(first.to, [north_desk()]);

    // Both are on the desk's docket, and the desk has the tool for each.
    let waiting = eventually_async("both on the docket", || async {
        awaiting_the_office(&bob)
            .await
            .filter(|seqs| seqs.len() == 2)
    })
    .await;
    assert_eq!(waiting, [first.seq, second.seq]);
    let tools = SenateMcp::new(bob.paths.socket(), Laptop::session("s-claude"))
        .tools()
        .await;
    for tool in ["speaker_cover", "speaker_hold"] {
        assert!(tools.iter().any(|t| t.name == tool), "{tool}");
    }

    // The desk answers the first on the child's behalf.
    let posted: Posted = bob
        .ipc(Request::Cover {
            session: Laptop::session("s-claude"),
            committee: committee(),
            thread: first.seq,
            body: "3, per the decision in the brief.".into(),
            agent: None,
            id: None,
        })
        .await;
    let answer = event_at(&world, posted.seq).await;
    assert_eq!(answer.kind, Kind::Answer);
    assert_eq!(answer.from, Address::Speaker);
    assert_eq!(answer.thread, Some(first.seq));
    assert_eq!(answer.to, [person("alice")]);
    assert_eq!(answer.on_behalf_of, Some(north_desk()));
    assert_eq!(answer.meta[meta::CHILD_OFFLINE], "north-desk");
    assert_eq!(answer.meta[meta::ACTED_BY], BOB_CLAUDE);
    assert!(
        answer.body.contains("[north-desk]")
            && answer.body.ends_with("3, per the decision in the brief."),
        "{}",
        answer.body
    );

    // It says the second is held.
    let posted: Posted = bob
        .ipc(Request::Hold {
            session: Laptop::session("s-claude"),
            committee: committee(),
            seq: second.seq,
            id: None,
        })
        .await;
    let held = event_at(&world, posted.seq).await;
    assert_eq!(held.kind, Kind::Note);
    assert_eq!(held.from, Address::Speaker);
    assert_eq!(held.thread, Some(second.seq));
    assert_eq!(held.to, [person("alice")]);
    assert_eq!(held.on_behalf_of, None);
    assert_eq!(held.meta[meta::CHILD_OFFLINE], "north-desk");
    assert_eq!(held.meta[meta::ACTED_BY], BOB_CLAUDE);
    assert!(held.body.contains("10"), "since when: {}", held.body);

    // Neither awaits the office any more; both questions still stand for
    // the child, untouched.
    assert_eq!(awaiting_the_office(&bob).await, Some(Vec::new()));
    for asked in [&first, &second] {
        assert_eq!(event_at(&world, asked.seq).await.to, [north_desk()]);
    }
    // Nothing is held twice.
    let again = bob
        .try_ipc::<Posted>(Request::Hold {
            session: Laptop::session("s-claude"),
            committee: committee(),
            seq: second.seq,
            id: None,
        })
        .await;
    assert!(again.is_err(), "{again:?}");
    bob.close().await;
    world.stop().await;
}
