//! The Speaker's section of each people group's announcement: people
//! groups only, sent when what it shows changed, coalesced and at most
//! hourly, and a surface that cannot set it told about once.

use std::time::Duration;

use senate_core::{Hub, Kind, NewEvent, PostTarget};

use super::hub::MemoryHub;
use super::rig::{GROUP, Rig, agent, agents_bind_event, bind_event, refund, user};
use super::surface::{Caps, FakeSurface};
use crate::ScriptedBrain;
use crate::engine::announcement::ANNOUNCEMENT_INTERVAL;

const AGENTS: &str = "agents";

/// The notice a group gets once when its announcement cannot be set.
const NOTICE: &str = "我没法更新本群的公告";

async fn hub_with_groups() -> std::sync::Arc<MemoryHub> {
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    hub.append(agents_bind_event(&refund(), AGENTS))
        .await
        .expect("bind agents");
    hub
}

/// Let `by` pass on a paused clock, with the Speaker caught up either side.
pub(super) async fn pass(rig: &Rig, by: Duration) {
    rig.settle().await;
    tokio::time::advance(by).await;
    rig.settle().await;
}

/// The card debounce (2 s by default), and a little more.
const DEBOUNCE: Duration = Duration::from_millis(2_100);

fn decision(body: &str) -> NewEvent {
    NewEvent::new(refund(), Kind::Decision, user("pm"), body)
}

fn notices(surface: &FakeSurface) -> usize {
    surface
        .posts()
        .iter()
        .filter(|p| p.markdown.starts_with(NOTICE))
        .count()
}

#[tokio::test(start_paused = true)]
async fn only_people_groups_get_it_and_a_burst_lands_once_an_hour_on() {
    let hub = hub_with_groups().await;
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;

    // Starting up sends the section once, to the people group only.
    pass(&rig, DEBOUNCE).await;
    let sent = surface.announcements();
    assert_eq!(sent.len(), 1, "{sent:?}");
    let (group, first, _) = &sent[0];
    assert_eq!(group, GROUP);
    assert_eq!(first.title, "委员会 refund-v2");
    assert!(first.markdown.starts_with("**怎么用**\n在群里 @Speaker"));
    assert!(
        first.markdown.contains("**最近的决策**\n无。"),
        "{}",
        first.markdown
    );

    // Three decisions in a burst, inside the hour: nothing yet.
    rig.append(decision("幂等键统一用 Idempotency-Key")).await;
    pass(&rig, DEBOUNCE).await;
    rig.append(decision("退款走异步队列")).await;
    rig.append(decision("重试最多三次")).await;
    pass(&rig, ANNOUNCEMENT_INTERVAL - DEBOUNCE * 3).await;
    assert_eq!(surface.announcements().len(), 1, "throttled to one an hour");

    // The hour is up: one update, with all three.
    pass(&rig, DEBOUNCE * 2).await;
    let sent = surface.announcements();
    assert_eq!(sent.len(), 2, "{sent:?}");
    let (group, latest, operation) = &sent[1];
    assert_eq!(group, GROUP);
    for decided in [
        "重试最多三次",
        "退款走异步队列",
        "幂等键统一用 Idempotency-Key",
    ] {
        assert!(latest.markdown.contains(decided), "{}", latest.markdown);
    }
    assert!(
        latest.markdown.find("重试最多三次") < latest.markdown.find("幂等键统一用"),
        "newest first: {}",
        latest.markdown
    );
    assert_ne!(operation, &sent[0].2, "new content is a new operation");
    assert!(surface.announcements().iter().all(|(g, _, _)| g != AGENTS));
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn it_is_sent_only_when_what_it_shows_changed() {
    let hub = hub_with_groups().await;
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;
    pass(&rig, DEBOUNCE).await;
    assert_eq!(surface.announcements().len(), 1);

    // A note from an agent changes nothing the section shows.
    rig.append(NewEvent::new(
        refund(),
        Kind::Note,
        agent("bob"),
        "看了一下日志",
    ))
    .await;
    pass(&rig, ANNOUNCEMENT_INTERVAL + DEBOUNCE).await;
    assert_eq!(surface.announcements().len(), 1, "no-op: same section");

    // Something waiting on a person does, and lands at once: the last one
    // was sent over an hour ago.
    let asked = rig
        .append(
            NewEvent::new(
                refund(),
                Kind::Request,
                agent("bob"),
                "把网关 header 改成 Idempotency-Key",
            )
            .to(agent("alice")),
        )
        .await;
    pass(&rig, DEBOUNCE).await;
    let sent = surface.announcements();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent[1].1.markdown.contains(&format!(
            "**等人处理**\n- seq {} · 把网关 header",
            asked.seq
        )),
        "{}",
        sent[1].1.markdown
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_surface_without_it_says_so_once_and_stops_until_a_rebind_or_restart() {
    let hub = hub_with_groups().await;
    let surface = FakeSurface::with(Caps {
        announcements: false,
        ..Caps::ADOPTED
    });
    let dir = tempfile::tempdir().expect("speaker data");
    let mut config = super::rig::config();
    config.data_dir = dir.path().into();
    let rig = Rig::start_config(
        hub.clone(),
        surface.clone(),
        ScriptedBrain::new(),
        config.clone(),
    )
    .await;
    pass(&rig, DEBOUNCE).await;
    assert_eq!(surface.announcements().len(), 1);
    assert_eq!(notices(&surface), 1);
    let notice = surface
        .posts()
        .into_iter()
        .find(|p| p.markdown.starts_with(NOTICE))
        .expect("notice");
    assert_eq!(
        notice.target,
        PostTarget::NewRoot {
            group_id: GROUP.into()
        }
    );

    // Changes after that are not tried, nor said again.
    rig.append(decision("幂等键统一用 Idempotency-Key")).await;
    pass(&rig, ANNOUNCEMENT_INTERVAL + DEBOUNCE).await;
    assert_eq!(surface.announcements().len(), 1, "no more tries");
    assert_eq!(notices(&surface), 1);

    // The new gateway has no deduplication history. The receipt survives both
    // the Speaker restart and a new binding event for the same group.
    rig.stop().await;
    let surface = FakeSurface::with(Caps {
        announcements: false,
        ..Caps::ADOPTED
    });
    let rig = Rig::start_config(hub, surface.clone(), ScriptedBrain::new(), config).await;
    pass(&rig, DEBOUNCE).await;
    assert_eq!(surface.announcements().len(), 1);
    assert_eq!(notices(&surface), 0);
    rig.append(bind_event(&refund(), GROUP)).await;
    pass(&rig, DEBOUNCE).await;
    assert_eq!(surface.announcements().len(), 2);
    assert_eq!(notices(&surface), 0);
    rig.append(bind_event(&refund(), "second-group")).await;
    pass(&rig, DEBOUNCE).await;
    let notices: Vec<_> = surface
        .posts()
        .into_iter()
        .filter(|p| p.markdown.starts_with(NOTICE))
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(
        notices[0].target,
        PostTarget::NewRoot {
            group_id: "second-group".into()
        }
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_success_rearms_the_notice_even_at_the_same_binding_and_log_head() {
    let dir = tempfile::tempdir().expect("speaker data");
    let mut config = super::rig::config();
    config.data_dir = dir.path().into();
    let hub = hub_with_groups().await;
    let unsupported = Caps {
        announcements: false,
        ..Caps::ADOPTED
    };
    let surface = FakeSurface::with(unsupported);
    for (caps, expected_notices) in [(unsupported, 1), (Caps::ADOPTED, 1), (unsupported, 2)] {
        surface.set_caps(caps);
        let rig = Rig::start_config(
            hub.clone(),
            surface.clone(),
            ScriptedBrain::new(),
            config.clone(),
        )
        .await;
        pass(&rig, DEBOUNCE).await;
        assert_eq!(notices(&surface), expected_notices);
        rig.stop().await;
    }
    let ids: Vec<_> = surface
        .posts()
        .into_iter()
        .filter(|p| p.markdown.starts_with(NOTICE))
        .map(|p| p.operation_id)
        .collect();
    assert_ne!(ids[0], ids[1]);
}

#[tokio::test(start_paused = true)]
async fn the_latest_digest_and_what_we_know_join_the_section() {
    use senate_core::{
        Address, DigestPayload, DigestPoint, DigestTrigger, MemoryPayload, Seq, meta,
    };
    let hub = hub_with_groups().await;
    let surface = FakeSurface::with(Caps::ADOPTED);
    let rig = Rig::start_with(hub, surface.clone(), ScriptedBrain::new()).await;
    pass(&rig, DEBOUNCE).await;
    assert!(!surface.announcements()[0].1.markdown.contains("近况"));

    let note = |body: &str| NewEvent::new(refund(), Kind::Note, Address::Speaker, body);
    rig.append(
        note("发布改到周一").with_meta(
            meta::MEMORY,
            MemoryPayload::Point {
                refs: vec![Seq(1)],
                supersedes: vec![],
                digest: None,
            }
            .to_value(),
        ),
    )
    .await;
    rig.append(
        note("- 讨论了发布日期").with_meta(
            meta::DIGEST,
            DigestPayload {
                trigger: DigestTrigger::Daily,
                from: Seq(1),
                through: Seq(2),
                points: vec![DigestPoint {
                    text: "讨论了发布日期，倾向周一".into(),
                    refs: vec![Seq(1)],
                }],
            }
            .to_value(),
        ),
    )
    .await;
    pass(&rig, ANNOUNCEMENT_INTERVAL + DEBOUNCE).await;
    let sent = surface.announcements();
    let latest = &sent.last().unwrap().1.markdown;
    assert!(
        latest.contains("**近况**\n- 讨论了发布日期，倾向周一"),
        "{latest}"
    );
    assert!(
        latest.contains("**我们知道的**\n- 发布改到周一"),
        "{latest}"
    );
    assert!(
        latest.find("**最近的决策**") < latest.find("**近况**"),
        "after the decisions: {latest}"
    );
    rig.stop().await;
}
