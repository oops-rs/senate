//! The gateway's group memberships over a fake platform, with the clock paused: what a
//! bind and an unbind ask of the platform, when, and how often.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use senate_core::{
    Address, Appended, BindingAction, BindingPayload, ChatSurface, CommitteeId, Event, GroupRole,
    Hub, HubError, Kind, Mirror, MirrorRef, NewEvent, PostTarget, Roster, SenatorName, Seq, Source,
};
use senated::testing::{Caps, FakeChat, MembershipKind, MembershipOutcome, Message};
use senated::{ChatGateway, Component, ComponentContext, MembershipPolicy};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

const GRACE: Duration = Duration::from_secs(3);
const GROUP: &str = "g_backend";

/// A log in memory, as the gateway needs one.
struct MemoryHub {
    logs: Mutex<BTreeMap<CommitteeId, Vec<Event>>>,
    cursors: Mutex<HashMap<(CommitteeId, String), Seq>>,
    mirrors: Mutex<Vec<MirrorRef>>,
    tx: broadcast::Sender<Event>,
}

impl MemoryHub {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            logs: Mutex::default(),
            cursors: Mutex::default(),
            mirrors: Mutex::default(),
            tx: broadcast::channel(1024).0,
        })
    }

    fn logs(&self) -> MutexGuard<'_, BTreeMap<CommitteeId, Vec<Event>>> {
        self.logs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn len(&self, committee: &CommitteeId) -> usize {
        self.logs().get(committee).map_or(0, Vec::len)
    }
}

#[async_trait]
impl Hub for MemoryHub {
    async fn append(&self, event: NewEvent) -> Result<Appended, HubError> {
        let stored = {
            let mut logs = self.logs();
            let log = logs.entry(event.committee.clone()).or_default();
            if let Some(existing) = log.iter().find(|e| e.id == event.id) {
                return Ok(Appended {
                    event: existing.clone(),
                    duplicate: true,
                });
            }
            let mut ts = Utc::now();
            if let Some(last) = log.last()
                && ts <= last.ts
            {
                ts = last.ts + chrono::Duration::microseconds(1);
            }
            let stored = event.into_event(Seq(log.len() as u64 + 1), ts);
            log.push(stored.clone());
            stored
        };
        let _ = self.tx.send(stored.clone());
        Ok(Appended {
            event: stored,
            duplicate: false,
        })
    }

    async fn read(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, HubError> {
        Ok(self
            .logs()
            .get(committee)
            .map(|log| {
                log.iter()
                    .filter(|e| e.seq > after)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, HubError> {
        Ok(self
            .logs()
            .get(committee)
            .and_then(|log| log.iter().find(|e| e.seq == seq).cloned()))
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    async fn committees(&self) -> Result<Vec<CommitteeId>, HubError> {
        Ok(self.logs().keys().cloned().collect())
    }

    async fn roster(&self) -> Result<Roster, HubError> {
        Ok(Roster::default())
    }

    async fn is_agent_connected(&self, _committee: &CommitteeId, _agent: &SenatorName) -> bool {
        false
    }

    async fn cursor(&self, committee: &CommitteeId, name: &str) -> Result<Seq, HubError> {
        Ok(self
            .cursors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(committee.clone(), name.to_owned()))
            .copied()
            .unwrap_or_default())
    }

    async fn set_cursor(
        &self,
        committee: &CommitteeId,
        name: &str,
        seq: Seq,
    ) -> Result<(), HubError> {
        self.cursors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((committee.clone(), name.to_owned()), seq);
        Ok(())
    }

    async fn epoch(&self) -> Result<String, HubError> {
        Ok("test".into())
    }

    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), HubError> {
        self.mirrors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(mirror);
        Ok(())
    }

    async fn mirrors_of_seq(
        &self,
        _committee: &CommitteeId,
        _seq: Seq,
        _platform: &str,
    ) -> Result<Vec<MirrorRef>, HubError> {
        Ok(Vec::new())
    }

    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, HubError> {
        Ok(self
            .mirrors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|m| m.platform == platform && m.message_ref == message_ref)
            .min_by_key(|m| m.seq)
            .cloned())
    }
}

fn committee(name: &str) -> CommitteeId {
    CommitteeId::new(name).unwrap()
}

async fn binding(hub: &MemoryHub, to: &str, group: &str, action: BindingAction) {
    hub.append(
        NewEvent::new(committee(to), Kind::Binding, Address::Speaker, "binding")
            .with_source(Source {
                platform: "chat".into(),
                group_id: group.into(),
                group_name: None,
                simulated: false,
            })
            .with_binding(&BindingPayload {
                action,
                platform: "chat".into(),
                group_id: group.into(),
                group_name: None,
                role: GroupRole::People,
            }),
    )
    .await
    .unwrap();
}

/// A person's chat in a group, as a gateway recorded it before a restart.
async fn chat(hub: &MemoryHub, to: &str, group: &str, message_ref: &str) {
    hub.append(
        NewEvent::new(
            committee(to),
            Kind::Chat,
            "guest::chat/u_1".parse().unwrap(),
            "hello",
        )
        .with_source(Source {
            platform: "chat".into(),
            group_id: group.into(),
            group_name: None,
            simulated: false,
        })
        .with_mirror(Mirror {
            platform: "chat".into(),
            message_ref: message_ref.into(),
            thread_ref: None,
        }),
    )
    .await
    .unwrap();
}

struct Rig {
    hub: Arc<MemoryHub>,
    fake: Arc<FakeChat>,
    gateway: ChatGateway,
    stop: CancellationToken,
    running: JoinHandle<anyhow::Result<()>>,
}

fn policy() -> MembershipPolicy {
    MembershipPolicy {
        release_grace: GRACE,
        retry_initial: Duration::from_secs(1),
        retry_max: Duration::from_secs(8),
        call_timeout: Duration::from_secs(30),
    }
}

async fn rig() -> Rig {
    rig_on(MemoryHub::new(), |_| {}).await
}

async fn rig_on(hub: Arc<MemoryHub>, platform: impl FnOnce(&FakeChat)) -> Rig {
    let (fake, gateway) = FakeChat::new();
    fake.set_caps(Caps {
        membership: true,
        ..Caps::PLAIN
    });
    platform(&fake);
    let gateway = gateway.with_membership_policy(policy());
    let stop = CancellationToken::new();
    let running = tokio::spawn({
        let gateway = gateway.clone();
        let ctx = ComponentContext {
            hub: hub.clone(),
            shutdown: stop.clone(),
        };
        async move { gateway.run(ctx).await }
    });
    let rig = Rig {
        hub,
        fake,
        gateway,
        stop,
        running,
    };
    settle().await;
    rig
}

impl Rig {
    /// A message from the group, so the gateway can name it for an adopt.
    async fn hear(&self, message_ref: &str) {
        self.fake
            .deliver(Message::ambient(GROUP, "u_1", message_ref, "hi"))
            .await
            .unwrap();
    }

    async fn bind(&self, to: &str) {
        binding(&self.hub, to, GROUP, BindingAction::Bind).await;
        settle().await;
    }

    async fn unbind(&self, from: &str) {
        binding(&self.hub, from, GROUP, BindingAction::Unbind).await;
        settle().await;
    }

    fn adopts(&self) -> Vec<senated::testing::MembershipCall> {
        self.fake.membership_calls_of(MembershipKind::Adopt)
    }

    fn releases(&self) -> Vec<senated::testing::MembershipCall> {
        self.fake.membership_calls_of(MembershipKind::Release)
    }

    async fn stop(self) {
        self.stop.cancel();
        self.running.await.unwrap().unwrap();
    }
}

/// Let every task that can run, run.
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

/// Move the paused clock on, running whatever falls due on the way.
async fn pass(time: Duration) {
    tokio::time::sleep(time).await;
    settle().await;
}

#[tokio::test(start_paused = true)]
async fn the_default_grace_is_three_seconds() {
    let policy = MembershipPolicy::default();
    assert_eq!(policy.release_grace, Duration::from_secs(3));
}

#[tokio::test(start_paused = true)]
async fn an_unbind_then_a_bind_within_the_grace_neither_releases_nor_adopts_again() {
    let rig = rig().await;
    rig.hear("m1").await;
    rig.bind("alpha").await;
    pass(Duration::from_secs(1)).await;
    assert_eq!(rig.adopts().len(), 1);
    assert!(rig.fake.joined().contains(&GROUP.to_owned()));

    // The group moves from one committee to another.
    rig.unbind("alpha").await;
    pass(Duration::from_secs(1)).await;
    rig.bind("beta").await;
    pass(Duration::from_secs(60)).await;

    assert_eq!(rig.releases(), [], "never released");
    assert_eq!(rig.adopts().len(), 1, "never adopted twice");
    assert!(rig.fake.joined().contains(&GROUP.to_owned()));
    assert_eq!(rig.gateway.committee_of(GROUP), Some(committee("beta")));
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_unbind_alone_releases_once_after_the_grace() {
    let rig = rig().await;
    rig.hear("m1").await;
    rig.bind("alpha").await;
    pass(Duration::from_secs(1)).await;
    assert_eq!(rig.adopts().len(), 1);

    let unbound = Instant::now();
    rig.unbind("alpha").await;
    pass(GRACE - Duration::from_millis(100)).await;
    assert_eq!(rig.releases(), [], "still within the grace");
    pass(Duration::from_millis(200)).await;

    let releases = rig.releases();
    assert_eq!(releases.len(), 1);
    assert_eq!(releases[0].subject, GROUP);
    assert_eq!(releases[0].at - unbound, GRACE);
    assert_eq!(releases[0].outcome, MembershipOutcome::Done);
    assert!(rig.fake.joined().is_empty());
    // Nothing more, however long it waits.
    pass(Duration::from_secs(600)).await;
    assert_eq!(rig.releases().len(), 1);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_release_that_fails_twice_is_retried_with_backoff_until_it_lands() {
    let rig = rig().await;
    rig.hear("m1").await;
    rig.bind("alpha").await;
    pass(Duration::from_secs(1)).await;
    rig.fake.fail_releases(2);

    let unbound = Instant::now();
    rig.unbind("alpha").await;
    pass(Duration::from_secs(60)).await;

    let releases = rig.releases();
    let outcomes: Vec<_> = releases.iter().map(|c| c.outcome).collect();
    assert_eq!(
        outcomes,
        [
            MembershipOutcome::Failed,
            MembershipOutcome::Failed,
            MembershipOutcome::Done
        ]
    );
    // After the grace, then 1s, then 2s.
    let at: Vec<_> = releases.iter().map(|c| c.at - unbound).collect();
    assert_eq!(
        at,
        [
            GRACE,
            GRACE + Duration::from_secs(1),
            GRACE + Duration::from_secs(3)
        ]
    );
    assert!(rig.fake.joined().is_empty());
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn the_pause_between_tries_is_bounded() {
    let rig = rig().await;
    rig.hear("m1").await;
    rig.fake.fail_adopts(1_000);
    rig.bind("alpha").await;
    pass(Duration::from_secs(120)).await;

    let calls = rig.adopts();
    let gaps: Vec<_> = calls
        .windows(2)
        .map(|w| (w[1].at - w[0].at).as_secs())
        .collect();
    assert_eq!(&gaps[..5], [1, 2, 4, 8, 8]);
    assert!(gaps.iter().all(|g| *g <= 8), "{gaps:?}");
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_adopt_that_fails_is_retried_until_it_lands() {
    let rig = rig().await;
    rig.hear("m1").await;
    rig.fake.fail_adopts(2);
    rig.bind("alpha").await;
    assert!(rig.fake.joined().is_empty());
    pass(Duration::from_secs(10)).await;

    let outcomes: Vec<_> = rig.adopts().iter().map(|c| c.outcome).collect();
    assert_eq!(
        outcomes,
        [
            MembershipOutcome::Failed,
            MembershipOutcome::Failed,
            MembershipOutcome::Done
        ]
    );
    assert!(rig.fake.joined().contains(&GROUP.to_owned()));
    pass(Duration::from_secs(600)).await;
    assert_eq!(rig.adopts().len(), 3, "settled: no more asking");
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn an_unbind_during_an_adopt_retry_ends_the_retries_and_releases() {
    let rig = rig().await;
    rig.hear("m1").await;
    rig.fake.fail_adopts(1_000);
    rig.bind("alpha").await;
    pass(Duration::from_secs(5)).await;
    let tried = rig.adopts().len();
    assert!(tried >= 2, "retrying: {tried}");

    rig.unbind("alpha").await;
    pass(Duration::from_secs(120)).await;
    assert!(
        rig.adopts().len() <= tried + 1,
        "no adopts after the unbind: {} then {}",
        tried,
        rig.adopts().len()
    );
    // The platform may still hold something from before: the release goes.
    assert_eq!(rig.releases().len(), 1);
    assert_eq!(rig.releases()[0].outcome, MembershipOutcome::Done);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_restart_adopts_a_bound_group_again_by_a_message_the_log_holds() {
    let hub = MemoryHub::new();
    // What the log says: bound, and someone spoke there.
    binding(&hub, "alpha", GROUP, BindingAction::Bind).await;
    chat(&hub, "alpha", GROUP, "m_old").await;
    chat(&hub, "alpha", GROUP, "m_newer").await;

    for run in 0..2 {
        let rig = rig_on(hub.clone(), |fake| {
            fake.know_message("m_newer", GROUP);
            fake.know_message("m_old", GROUP);
        })
        .await;
        pass(Duration::from_secs(1)).await;
        let adopts = rig.adopts();
        assert_eq!(adopts.len(), 1, "run {run}");
        assert_eq!(adopts[0].subject, "m_newer", "the latest message");
        assert_eq!(adopts[0].outcome, MembershipOutcome::Done);
        assert!(rig.fake.joined().contains(&GROUP.to_owned()));
        assert_eq!(rig.releases(), []);
        rig.stop().await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_restart_adopts_a_bound_group_no_one_has_spoken_in_on_its_first_message() {
    let hub = MemoryHub::new();
    binding(&hub, "alpha", GROUP, BindingAction::Bind).await;
    let rig = rig_on(hub, |_| {}).await;
    pass(Duration::from_secs(1)).await;
    assert_eq!(rig.adopts(), [], "nothing names the group yet");

    rig.hear("m_first").await;
    pass(Duration::from_secs(1)).await;
    assert_eq!(rig.adopts().len(), 1);
    assert_eq!(rig.adopts()[0].subject, "m_first");
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_restart_performs_a_release_the_log_says_is_owed() {
    let hub = MemoryHub::new();
    binding(&hub, "alpha", GROUP, BindingAction::Bind).await;
    binding(&hub, "alpha", GROUP, BindingAction::Unbind).await;
    let rig = rig_on(hub, |fake| {
        // The bot is still in the group from before the restart.
        fake.know_message("m1", GROUP);
    })
    .await;
    rig.fake
        .deliver(Message::ambient(GROUP, "u_1", "m1", "x"))
        .await
        .unwrap();
    // Held on the platform from before: adopt it as the earlier run did.
    rig.fake.adopt_group("m1").await.unwrap();
    assert!(rig.fake.joined().contains(&GROUP.to_owned()));
    pass(GRACE + Duration::from_secs(1)).await;
    assert_eq!(rig.releases().len(), 1);
    assert!(rig.fake.joined().is_empty());
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_surface_that_cannot_join_is_asked_once_and_not_again() {
    let rig = rig_on(MemoryHub::new(), |fake| fake.set_caps(Caps::PLAIN)).await;
    rig.hear("m1").await;
    rig.bind("alpha").await;
    pass(Duration::from_secs(600)).await;
    assert_eq!(rig.adopts().len(), 1);
    rig.hear("m2").await;
    rig.unbind("alpha").await;
    rig.bind("beta").await;
    rig.unbind("beta").await;
    pass(Duration::from_secs(3_600)).await;
    assert_eq!(rig.adopts().len(), 1, "no retry storm");
    assert!(rig.releases().len() <= 1, "no retry storm");
    assert_eq!(rig.adopts()[0].outcome, MembershipOutcome::Unsupported);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_hanging_adopt_blocks_neither_the_log_nor_the_gateway() {
    let rig = rig().await;
    rig.fake.hang_adopts(true);
    rig.hear("m1").await;
    let began = Instant::now();
    rig.bind("alpha").await;

    // Appends and deliveries go on, with no time passing at all.
    for n in 0..10 {
        let delivered = rig
            .fake
            .deliver(Message::ambient(GROUP, "u_1", &format!("m_{n}"), "talk"))
            .await
            .unwrap();
        assert!(matches!(delivered, senated::Delivered::Recorded(_)));
        chat(&rig.hub, "alpha", "g_other", &format!("x{n}")).await;
    }
    assert_eq!(Instant::now() - began, Duration::ZERO);
    assert!(rig.hub.len(&committee("alpha")) >= 20);
    assert_eq!(rig.gateway.committee_of(GROUP), Some(committee("alpha")));

    // The call is given up on at its timeout and asked again.
    pass(Duration::from_secs(100)).await;
    assert!(rig.adopts().len() >= 2);
    // An unbind still gets its release through, once the hung call timed out.
    rig.fake.hang_adopts(false);
    rig.unbind("alpha").await;
    pass(Duration::from_secs(120)).await;
    assert_eq!(
        rig.releases().last().unwrap().outcome,
        MembershipOutcome::Done
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_root_post_refused_for_want_of_membership_joins_and_goes_through() {
    let rig = rig().await;
    rig.fake.require_membership_to_post(true);
    rig.fake.delay_adopts(Duration::from_secs(2));
    let mount = rig.gateway.mount().unwrap();
    rig.hear("m1").await;
    rig.bind("alpha").await;

    // The Speaker posts as the bind lands, before the adopt has returned.
    let posted = mount
        .surface
        .post(
            PostTarget::NewRoot {
                group_id: GROUP.into(),
            },
            "hello",
            "op-1",
        )
        .await
        .unwrap();
    assert!(!posted.message_ref.is_empty());
    assert_eq!(rig.fake.posts().len(), 1);
    assert_eq!(rig.adopts().len(), 1, "the adopt under way is waited for");

    // A refusal for another reason is not retried: the bot is in the group.
    rig.fake.require_membership_to_post(false);
    let _ = mount
        .surface
        .post(
            PostTarget::NewRoot {
                group_id: GROUP.into(),
            },
            "again",
            "op-1",
        )
        .await;
    assert_eq!(rig.adopts().len(), 1);
    rig.stop().await;
}

// ---- a reply is healed too ---------------------------------------------------

fn reply_to(message_ref: &str) -> PostTarget {
    PostTarget::Reply {
        message_ref: message_ref.into(),
        in_thread: true,
    }
}

#[tokio::test(start_paused = true)]
async fn a_reply_refused_for_want_of_membership_joins_its_group_and_goes_through() {
    let rig = rig().await;
    rig.fake.require_membership_to_reply(true);
    rig.fake.delay_adopts(Duration::from_secs(2));
    let mount = rig.gateway.mount().unwrap();
    // The bind message, then the bind: the Speaker confirms in the thread of
    // the bind message while the slow adopt is still under way.
    rig.hear("m_bind").await;
    rig.bind("alpha").await;
    let started = Instant::now();
    let posted = mount
        .surface
        .post(reply_to("m_bind"), "bound", "op-1")
        .await
        .unwrap();
    assert!(!posted.message_ref.is_empty());
    assert_eq!(rig.fake.posts().len(), 1);
    assert_eq!(rig.fake.rejected_posts(), 1, "refused once, then it landed");
    assert_eq!(rig.adopts().len(), 1, "the adopt under way is waited for");
    assert!(
        Instant::now() - started <= Duration::from_secs(2),
        "it waited for that adopt and no longer"
    );
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_to_a_message_the_gateway_cannot_place_is_not_healed() {
    let rig = rig().await;
    rig.fake.require_membership_to_reply(true);
    rig.fake.delay_adopts(Duration::from_secs(2));
    let mount = rig.gateway.mount().unwrap();
    // The platform knows the message, the gateway never saw it and the log
    // holds no copy of it.
    rig.fake.know_message("m_old", GROUP);
    rig.hear("m_bind").await;
    rig.bind("alpha").await;
    let refused = mount.surface.post(reply_to("m_old"), "x", "op-1").await;
    assert!(
        matches!(refused, Err(senate_core::SurfaceError::Rejected(_))),
        "{refused:?}"
    );
    assert_eq!(rig.fake.rejected_posts(), 1, "not tried again");
    assert_eq!(rig.adopts().len(), 1, "only the bind's own adopt");
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_into_a_group_that_is_not_bound_is_not_healed() {
    let rig = rig().await;
    rig.fake.require_membership_to_reply(true);
    let mount = rig.gateway.mount().unwrap();
    rig.hear("m1").await;
    let refused = mount.surface.post(reply_to("m1"), "x", "op-1").await;
    assert!(
        matches!(refused, Err(senate_core::SurfaceError::Rejected(_))),
        "{refused:?}"
    );
    assert_eq!(rig.fake.rejected_posts(), 1);
    assert!(
        rig.adopts().is_empty(),
        "never an adopt for an unbound group"
    );
    // Bound, then unbound and released: refused again, and not adopted
    // again by the refusal.
    rig.bind("alpha").await;
    pass(Duration::from_secs(1)).await;
    rig.unbind("alpha").await;
    pass(GRACE + Duration::from_secs(1)).await;
    assert_eq!(rig.releases().len(), 1);
    let adopts = rig.adopts().len();
    let refused = mount.surface.post(reply_to("m1"), "y", "op-2").await;
    assert!(refused.is_err());
    assert_eq!(rig.adopts().len(), adopts);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_is_retried_once_and_never_in_a_loop() {
    let rig = rig().await;
    rig.fake.delay_adopts(Duration::from_secs(1));
    let mount = rig.gateway.mount().unwrap();
    rig.hear("m1").await;
    rig.bind("alpha").await;
    // The platform refuses for a reason no join mends.
    rig.fake.reject_next_posts(10);
    let refused = mount.surface.post(reply_to("m1"), "x", "op-1").await;
    assert!(refused.is_err());
    assert_eq!(rig.fake.rejected_posts(), 2, "the post and its one retry");
    assert_eq!(rig.adopts().len(), 1);
    // Joined now: another refusal is not healed at all.
    let refused = mount.surface.post(reply_to("m1"), "y", "op-2").await;
    assert!(refused.is_err());
    assert_eq!(rig.fake.rejected_posts(), 3);
    assert_eq!(rig.adopts().len(), 1);
    pass(Duration::from_secs(60)).await;
    assert_eq!(rig.fake.rejected_posts(), 3, "nothing keeps trying");
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_is_not_delayed_when_membership_is_already_confirmed() {
    let rig = rig().await;
    rig.fake.delay_adopts(Duration::from_secs(2));
    let mount = rig.gateway.mount().unwrap();
    rig.hear("m1").await;
    rig.bind("alpha").await;
    pass(Duration::from_secs(5)).await;
    assert_eq!(rig.adopts()[0].outcome, MembershipOutcome::Done);
    rig.fake.require_membership_to_reply(true);
    let started = Instant::now();
    mount
        .surface
        .post(reply_to("m1"), "x", "op-1")
        .await
        .unwrap();
    assert_eq!(Instant::now(), started, "no waiting");
    assert_eq!(rig.fake.rejected_posts(), 0);
    assert_eq!(rig.adopts().len(), 1);
    rig.stop().await;
}

#[tokio::test(start_paused = true)]
async fn a_reply_to_something_the_speaker_posted_finds_its_group_in_the_logs_mirrors() {
    let rig = rig().await;
    rig.fake.require_membership_to_reply(true);
    rig.fake.delay_adopts(Duration::from_secs(2));
    let mount = rig.gateway.mount().unwrap();
    rig.hear("m_bind").await;
    rig.bind("alpha").await;
    // A copy the Speaker posted into the group earlier (another run of the
    // daemon, say): the platform knows it, the gateway only has the log.
    rig.fake.know_message("m_copy", GROUP);
    rig.hub
        .put_mirror(MirrorRef {
            committee: committee("alpha"),
            seq: Seq(1),
            platform: "chat".into(),
            target: format!("{GROUP}#ack"),
            message_ref: "m_copy".into(),
            thread_ref: None,
        })
        .await
        .unwrap();
    mount
        .surface
        .post(reply_to("m_copy"), "x", "op-1")
        .await
        .unwrap();
    assert_eq!(rig.fake.rejected_posts(), 1);
    assert_eq!(rig.adopts().len(), 1);
    rig.stop().await;
}
