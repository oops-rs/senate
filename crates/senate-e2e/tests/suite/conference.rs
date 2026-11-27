#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common::*;
use senate_bridge::ipc::{Request, SessionSelector};
use senate_core::conference::{
    ConferenceDetail, ConferenceList, ConferenceRsvpAction, ConferenceSeatInput, ConferenceState,
    ConferenceSummary, PostConferenceEventResponse, SeatStatus,
};
use senate_core::{Address, CommitteeId, Hub, Kind, Locator, NewEvent};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_committee_conference_convenes_rsvps_posts_and_adjourns_over_bridge() {
    let world = World::start(60).await;
    let senate_c = CommitteeId::new("senate").unwrap();
    let commons_c = CommitteeId::new("commons").unwrap();

    world
        .hub()
        .append(NewEvent::new(
            senate_c.clone(),
            Kind::Request,
            person("bob"),
            "Open senate committee",
        ))
        .await
        .unwrap();
    world.hub().set_chair(&senate_c, path("bob")).await.unwrap();
    world
        .hub()
        .append(NewEvent::new(
            commons_c.clone(),
            Kind::Request,
            person("carol"),
            "Open commons committee",
        ))
        .await
        .unwrap();
    world
        .hub()
        .set_chair(&commons_c, path("carol"))
        .await
        .unwrap();

    let mut alice = world.laptop("alice");
    alice.open().await;
    let mut bob = world.laptop("bob");
    bob.open().await;
    let mut carol = world.laptop("carol");
    carol.open().await;
    world.settle().await;

    // 1. Alice convenes across senate::chair and commons::chair.
    let summary: ConferenceSummary = alice
        .ipc(Request::ConferenceConvene {
            session: SessionSelector::default(),
            topic: "Cross-committee release coordination".into(),
            seats: vec![
                ConferenceSeatInput {
                    target: Locator::on(senate_c.clone(), Address::Chair),
                },
                ConferenceSeatInput {
                    target: Locator::on(commons_c.clone(), Address::Chair),
                },
            ],
            linked_threads: vec![],
        })
        .await;
    assert_eq!(summary.state, ConferenceState::Convening);

    // 2. Bob attends, Carol delegates an ephemeral agent -> quorum InSession.
    let after_bob: ConferenceSummary = bob
        .ipc(Request::ConferenceRsvp {
            session: SessionSelector::default(),
            id: summary.id.clone(),
            action: ConferenceRsvpAction::Attend {
                target: Some(Locator::on(senate_c.clone(), Address::Chair)),
                participant: None,
            },
        })
        .await;
    assert_eq!(after_bob.state, ConferenceState::Convening);

    let after_carol: ConferenceSummary = carol
        .ipc(Request::ConferenceRsvp {
            session: SessionSelector::default(),
            id: summary.id.clone(),
            action: ConferenceRsvpAction::Delegate {
                target: Some(Locator::on(commons_c.clone(), Address::Chair)),
                senator: senator("commons-del"),
                delegated_by: None,
                brief: Some("Carry commons constraints".into()),
                ephemeral: true,
            },
        })
        .await;
    assert_eq!(after_carol.state, ConferenceState::InSession);
    assert!(matches!(
        &after_carol.seats[1].status,
        SeatStatus::Seated {
            ephemeral: true,
            ..
        }
    ));

    // 3. Alice posts in the room; agent receives 1 push opportunity.
    let post: PostConferenceEventResponse = alice
        .ipc(Request::ConferencePost {
            session: SessionSelector::default(),
            id: summary.id.clone(),
            body: "Align on Friday cutoff".into(),
            to: vec![],
        })
        .await;
    assert_eq!(post.floor.round, 1);
    assert_eq!(post.push_targets, vec![senator("commons-del")]);

    // 4. Alice adjourns; verify summary, list visibility, and write-back.
    let adjourned: ConferenceSummary = alice
        .ipc(Request::ConferenceAdjourn {
            session: SessionSelector::default(),
            id: summary.id.clone(),
            summary: "Cutoff set to Friday 17:00".into(),
            decisions: vec!["Friday 17:00 freeze".into()],
            action_items: vec![],
        })
        .await;
    assert_eq!(adjourned.state, ConferenceState::Adjourned);

    let list: ConferenceList = bob
        .ipc(Request::ConferenceList {
            session: SessionSelector::default(),
        })
        .await;
    assert!(list.conferences.iter().any(|c| c.id == summary.id));

    let detail: ConferenceDetail = alice
        .ipc(Request::ConferenceGet {
            session: SessionSelector::default(),
            id: summary.id.clone(),
        })
        .await;
    assert_eq!(detail.summary.state, ConferenceState::Adjourned);
}
