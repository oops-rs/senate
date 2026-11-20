//! The Speaker's desk changed from somewhere else: a machine whose agent
//! is put on the desk (or taken off it) knows at once, with no request of
//! its own and no restart of its daemon. The Senate says the office on the
//! machine's streams; nothing in a committee's log does.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_bridge::client::ApiClient;
use senate_bridge::ipc::{ClientError, DocketReply, Request, codes};
use senate_core::{Address, Kind, NewEvent};

/// claude's session asks for the desk's docket, as `speaker_docket` does.
/// The daemon answers from the office it knows: it asks the Senate nothing.
async fn docket(laptop: &Laptop) -> Result<DocketReply, ClientError> {
    laptop
        .try_ipc(Request::Docket {
            session: Laptop::session("s-claude"),
            committee: None,
        })
        .await
}

fn not_presiding(reply: &Result<DocketReply, ClientError>) -> bool {
    matches!(reply, Err(ClientError::Remote(e)) if e.code == codes::NOT_PRESIDING)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_desk_changed_elsewhere_is_known_on_the_machine_at_once() {
    // bob, a person, presides; alice's claude is a member like any other.
    let world = team_above().await;
    let mut alice = world.laptop("alice");
    alice.open().await;
    let inbox = alice.inbox("s-claude");
    alice.session_start("s-claude", &inbox).await;
    world.wait_connected("claude", true).await;
    let refused = docket(&alice).await;
    assert!(not_presiding(&refused), "{refused:?}");

    // Bob puts claude on his desk, from his own machine: nothing of it
    // passes through alice's daemon, and no committee's log says so.
    let bob = ApiClient::new(&world.server_url(), "tok-bob").unwrap();
    bob.set_secretaries(&path("bob"), vec![path("claude")], None)
        .await
        .unwrap();

    // Alice's daemon knows: claude's session presides.
    let reply = eventually_async("claude's session to preside", || async {
        docket(&alice).await.ok()
    })
    .await;
    assert_eq!(reply.speaker, Some(path("claude")));
    // What is put to the Speaker is pushed to it.
    speak(
        &world.server_url(),
        "tok-carol",
        &NewEvent::new(
            committee(),
            Kind::Question,
            person("carol"),
            "Who decides the retry limit?",
        )
        .to(Address::Speaker),
    )
    .await
    .unwrap();
    inbox.wait_for("Who decides the retry limit?").await;

    // Taken off the desk again, it stops presiding as promptly.
    bob.set_secretaries(&path("bob"), Vec::new(), None)
        .await
        .unwrap();
    eventually_async("claude's session to stop presiding", || async {
        not_presiding(&docket(&alice).await).then_some(())
    })
    .await;
    alice.close().await;
    world.stop().await;
}
