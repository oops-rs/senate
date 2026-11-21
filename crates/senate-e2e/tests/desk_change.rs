//! The Speaker's desk changed from somewhere else: a machine whose agent
//! is put on the desk (or taken off it) knows at once, with no request of
//! its own and no restart of its daemon. The Senate says the office on the
//! machine's streams; nothing in a committee's log does.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use common::*;
use senate_bridge::client::ApiClient;
use senate_bridge::ipc::{ClientError, DocketReply, IpcClient, Request, WatchEvent, codes};
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
    // What the session's MCP server follows, to tell the client when its
    // tools change.
    let mut watch = IpcClient::connect(&alice.paths.socket(), std::time::Duration::from_secs(2))
        .await
        .unwrap()
        .watch(false, std::time::Duration::from_secs(5))
        .await
        .unwrap();

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
    // And says so to whoever watches it.
    loop {
        let event = tokio::time::timeout(WAIT, watch.next()).await;
        match event
            .expect("the roles to be said changed in time")
            .unwrap()
        {
            Some(WatchEvent::Roles) => break,
            Some(_) => continue,
            None => panic!("the daemon closed the watch"),
        }
    }
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

/// `senate secretary desk [set]`: the person who presides reads and sets
/// the desk workers from the command line, in order, through their own
/// daemon; someone else is refused by the Senate; a list naming the
/// Speaker itself, or one senator twice, is refused before it is sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_desk_workers_are_shown_and_set_from_the_command_line() {
    use senate_bridge::ipc::DeskReply;
    let world = team_above().await;
    let mut bob = world.laptop("bob");
    bob.open().await;
    let shown: DeskReply = bob.ipc(Request::Desk { set: None }).await;
    assert_eq!(shown.speaker, path("bob"));
    assert!(shown.secretaries.is_empty(), "{shown:?}");

    let set: DeskReply = bob
        .ipc(Request::Desk {
            set: Some(vec![path("claude"), path(BOB_CLAUDE)]),
        })
        .await;
    assert_eq!(set.secretaries, [path("claude"), path(BOB_CLAUDE)]);
    // In that order on the Senate, and in the office the daemon now knows.
    let all = ApiClient::new(&world.server_url(), "tok-bob")
        .unwrap()
        .secretaries()
        .await
        .unwrap();
    assert_eq!(
        all.get(&path("bob")).unwrap(),
        &[path("claude"), path(BOB_CLAUDE)]
    );
    let first = set.desk.iter().position(|w| w == &path("claude"));
    let second = set.desk.iter().position(|w| w == &path(BOB_CLAUDE));
    assert!(first.is_some() && first < second, "{set:?}");
    // The order is the order given.
    let swapped: DeskReply = bob
        .ipc(Request::Desk {
            set: Some(vec![path(BOB_CLAUDE), path("claude")]),
        })
        .await;
    assert_eq!(swapped.secretaries, [path(BOB_CLAUDE), path("claude")]);

    for bad in [vec![path("bob")], vec![path("claude"), path("claude")]] {
        let refused = bob
            .try_ipc::<DeskReply>(Request::Desk {
                set: Some(bad.clone()),
            })
            .await;
        assert!(
            matches!(&refused, Err(ClientError::Remote(e)) if e.code == codes::BAD_REQUEST),
            "{bad:?}: {refused:?}"
        );
    }

    // Alice neither presides nor runs the Speaker: the Senate refuses her.
    let mut alice = world.laptop("alice");
    alice.open().await;
    let refused = alice
        .try_ipc::<DeskReply>(Request::Desk {
            set: Some(vec![path("claude")]),
        })
        .await;
    assert!(refused.is_err(), "{refused:?}");
    let shown: DeskReply = alice.ipc(Request::Desk { set: None }).await;
    assert_eq!(shown.secretaries, [path(BOB_CLAUDE), path("claude")]);

    // Set to nothing: no desk workers.
    let cleared: DeskReply = bob
        .ipc(Request::Desk {
            set: Some(Vec::new()),
        })
        .await;
    assert!(cleared.secretaries.is_empty(), "{cleared:?}");
    alice.close().await;
    bob.close().await;
    world.stop().await;
}
