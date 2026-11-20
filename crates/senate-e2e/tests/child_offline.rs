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
use senate_bridge::ipc::Registration;
use senate_bridge::ipc::{DocketReply, Posted, Request};
use senate_bridge::mcp::SenateMcp;
use senate_core::{Address, Awaiting, ChildSenate, Event, Kind, Locale, NewEvent, Seq, meta};
use senate_speaker::Offline;

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
    assert_eq!(
        answer.to,
        [person("alice"), north_desk()],
        "to the asker, and to the child for when it is back"
    );
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

/// The parent's built-in Speaker answers a question for a child Senate
/// that is offline. When the child's machine is back, its uplink brings
/// the question down and, under it, the answer that was given in the
/// child's name, once; and it posts nothing of that back up as the
/// child's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_answer_given_for_an_offline_child_senate_reaches_it_once_and_is_not_echoed() {
    let world = Setup::new()
        .without_chat()
        .server(|c| c.name = Some(name("acme")))
        .start()
        .await;
    let alice_desk = Address::child(name("alice-desk"), committee());
    // alice's machine registered its Senate and was last seen ten minutes
    // ago.
    let mut child = ChildSenate::new(name("alice-desk"), path("alice"));
    child.last_seen = Some(chrono::Utc::now() - chrono::Duration::minutes(10));
    world.hub().register_child(child).await.unwrap();

    world
        .brain
        .decides_offline(Ok(Offline::Answer("Three times, per the brief.".into())));
    let asked = speak(
        &world.server_url(),
        "tok-carol",
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("carol"),
            "Which retry limit does the refund callback use?",
        )
        .to(alice_desk.clone()),
    )
    .await
    .unwrap();
    let answer = eventually_async("the office's answer for the child", || async {
        world
            .events_of(&committee())
            .await
            .into_iter()
            .find(|e| e.thread == Some(asked.seq) && e.meta.contains_key(meta::CHILD_OFFLINE))
    })
    .await;
    assert_eq!(answer.kind, Kind::Answer);
    assert_eq!(answer.from, Address::Speaker);
    assert_eq!(answer.on_behalf_of, Some(alice_desk.clone()));
    assert_eq!(answer.to, [person("carol"), alice_desk.clone()]);
    assert!(answer.body.ends_with("Three times, per the brief."));

    // The machine is back.
    let dir = tempfile::tempdir().unwrap();
    let mut desk = Desk::new(
        dir.path(),
        "alice",
        Locale::En,
        &under(&world, "tok-alice", ""),
    );
    desk.start().await;
    assert_eq!(
        desk.wait_registration().await.registration,
        Registration::Registered
    );
    let below = || async { desk.events_of(&committee()).await };
    let root = eventually_async("the question on alice's desk", || async {
        below()
            .await
            .into_iter()
            .find(|e| e.body == "Which retry limit does the refund callback use?")
    })
    .await;
    assert_eq!(root.thread, None);
    let brought = eventually_async("the answer given for it, under the question", || async {
        below()
            .await
            .into_iter()
            .find(|e| e.thread == Some(root.seq) && e.body.ends_with("Three times, per the brief."))
    })
    .await;
    assert_eq!(brought.kind, Kind::Answer);
    assert_eq!(brought.from, Address::Committee);
    assert_eq!(brought.to, [Address::Speaker]);
    // The desk item is answered: it does not await alice's desk, so no
    // secretary of hers covers or forwards it.
    let docket = desk.senate().daemon.hub.docket(&committee()).await.unwrap();
    assert!(
        docket.items.iter().all(|item| item.event.seq != root.seq),
        "answered above, yet on the desk: {:#?}",
        docket.items
    );

    // A few more polls: it came down once, and nothing went back up as the
    // child's own answer.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let in_thread: Vec<Event> = below()
        .await
        .into_iter()
        .filter(|e| e.thread == Some(root.seq))
        .collect();
    assert_eq!(in_thread.len(), 1, "{in_thread:#?}");
    let above: Vec<Event> = world
        .events_of(&committee())
        .await
        .into_iter()
        .filter(|e| e.thread == Some(asked.seq))
        .collect();
    assert_eq!(above.len(), 1, "only the office's answer: {above:#?}");
    assert!(
        world
            .events_of(&committee())
            .await
            .iter()
            .all(|e| e.from != alice_desk),
        "the child said nothing"
    );
    desk.stop().await;
    world.stop().await;
}
