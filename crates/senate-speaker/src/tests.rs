//! Speaker tests: the flows with a [`ScriptedBrain`](crate::ScriptedBrain),
//! an in-memory Hub and a fake chat surface; the basis Brain with a scripted
//! provider.

mod agent;
mod agents;
mod agreement;
mod announcement;
mod basis_brain;
mod continuity;
mod decisions;
mod desk;
mod flows;
mod gone;
pub(crate) mod hub;
mod memory;
mod model_check;
mod model_down;
mod review;
mod rig;
mod secretary;
mod surface;
mod tasks;
mod work;

/// A short debounce exposes the interval between startup and the uplink's
/// next report. One child's cached report predates this Speaker.
async fn startup_seats_rig() -> rig::Rig {
    use senate_core::Hub;
    let hub = hub::MemoryHub::new();
    hub.append(rig::bind_event(&rig::refund(), rig::GROUP))
        .await
        .unwrap();
    hub.add_child("east", "alice");
    hub.add_child("west", "bob");
    hub.child_seen("west", Some(chrono::DateTime::UNIX_EPOCH));
    let mut config = rig::config();
    config.card_debounce_ms = 100;
    rig::Rig::start_config(
        hub,
        surface::FakeSurface::with(surface::Caps::ADOPTED),
        crate::ScriptedBrain::new(),
        config,
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn first_announcement_waits_for_each_fresh_report_then_throttles() {
    use crate::engine::announcement::ANNOUNCEMENT_INTERVAL;
    use senate_core::{Kind, NewEvent, PresenceState};
    use std::time::Duration;
    let rig = startup_seats_rig().await;
    let debounce = Duration::from_millis(100);
    announcement::pass(&rig, debounce).await;
    assert!(rig.surface.announcements().is_empty());
    rig.hub.declare_seats(
        "east",
        &rig::refund(),
        &[("delegate", PresenceState::Online)],
    );
    announcement::pass(&rig, Duration::from_secs(8)).await;
    assert!(
        rig.surface.announcements().is_empty(),
        "west has only a pre-start report"
    );
    rig.hub.declare_seats("west", &rig::refund(), &[]);
    announcement::pass(&rig, debounce).await;
    let sent = rig.surface.announcements();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.markdown.contains("delegate"));
    rig.append(NewEvent::new(
        rig::refund(),
        Kind::Decision,
        rig::user("pm"),
        "approved",
    ))
    .await;
    announcement::pass(&rig, ANNOUNCEMENT_INTERVAL - debounce).await;
    assert_eq!(rig.surface.announcements().len(), 1);
    announcement::pass(&rig, debounce).await;
    let sent = rig.surface.announcements();
    assert_eq!(sent.len(), 2);
    assert!(sent[1].1.markdown.contains("approved"));
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn first_announcement_uses_available_reports_at_the_uplink_deadline() {
    use senate_core::{
        PresenceState,
        api::{MAX_BACKOFF, UPLINK_POLL},
    };
    use std::time::Duration;
    let rig = startup_seats_rig().await;
    rig.hub.declare_seats(
        "east",
        &rig::refund(),
        &[("delegate", PresenceState::Online)],
    );
    let tick = Duration::from_millis(1);
    announcement::pass(&rig, MAX_BACKOFF + UPLINK_POLL - tick).await;
    assert!(rig.surface.announcements().is_empty());
    announcement::pass(&rig, tick).await;
    let sent = rig.surface.announcements();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].1.markdown.contains("delegate"));
    rig.stop().await;
}
