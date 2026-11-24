//! A live, bounded view of a single grant. The snapshot and replay journal
//! are published together under the view lock.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use futures::stream;
use senate_core::{CommitteeId, Event, Seq};
use senate_observe_protocol::{
    Cursor, ResetData, ResetFrame, ResetKind, ResetReason, Snapshot, V1,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, broadcast, watch};

use crate::audit::Audit;
use crate::diff::{change_hints, patches};
use crate::grants::{Authorized, GrantInfo, lock};
use crate::journal::{Hint, Journal, JournalError};

const VIEW_LIMIT: usize = 16;
const STREAM_LIMIT: usize = 16;
const LEASE: Duration = Duration::from_secs(30);
const BUILD_TIMEOUT: Duration = Duration::from_secs(10);
const COALESCE: Duration = Duration::from_millis(50);
const RECONCILE: Duration = Duration::from_secs(2);
const MAX_DURABLE_GAP: u64 = 512;
const MAX_STATE_TOTAL: u32 = 32 * 1024 * 1024;

/// Implemented by the embedded Senate server. No optional transports.
#[async_trait]
pub trait SenateSource: Send + Sync {
    fn subscribe(&self) -> broadcast::Receiver<Event>;

    async fn capture(&self, grant: &GrantInfo) -> Result<Capture, ProjectionError>;

    async fn read(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, ProjectionError>;

    async fn hint(&self, event: &Event, state: &Snapshot) -> Result<Option<Hint>, ProjectionError>;
}

pub struct Capture {
    pub snapshot: Snapshot,
    pub heads: BTreeMap<CommitteeId, Seq>,
}

#[derive(Clone, Copy, Debug)]
pub enum ProjectionError {
    NotReady,
    TooLarge,
    RateLimited,
    Reset,
    Unauthorized,
}

impl From<JournalError> for ProjectionError {
    fn from(error: JournalError) -> Self {
        match error {
            JournalError::Unavailable => Self::NotReady,
            JournalError::TooLarge => Self::TooLarge,
            JournalError::Lost => Self::Reset,
        }
    }
}

pub(crate) struct Projections {
    source: Mutex<Option<Arc<dyn SenateSource>>>,
    views: Mutex<BTreeMap<String, Arc<View>>>,
    builds: Semaphore,
    streams: Arc<Semaphore>,
    state_budget: Arc<Semaphore>,
    shutdown: watch::Sender<bool>,
}

struct View {
    journal: Mutex<Option<Journal>>,
    signal: watch::Sender<Signal>,
    activity: Mutex<Activity>,
    wake: Notify,
    stream_limit: Arc<Semaphore>,
}

struct Activity {
    streams: usize,
    until: Instant,
}

#[derive(Clone, Copy)]
enum Signal {
    Pending,
    Ready,
    Failed(ProjectionError),
}

impl Projections {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            source: Mutex::new(None),
            views: Mutex::new(BTreeMap::new()),
            builds: Semaphore::new(4),
            streams: Arc::new(Semaphore::new(STREAM_LIMIT)),
            state_budget: Arc::new(Semaphore::new(MAX_STATE_TOTAL as usize)),
            shutdown: watch::channel(false).0,
        })
    }

    pub(crate) fn set_source(&self, source: Arc<dyn SenateSource>) {
        *lock(&self.source) = Some(source);
    }

    pub(crate) fn shutdown(&self) {
        self.shutdown.send_replace(true);
        *lock(&self.source) = None;
        for view in lock(&self.views).values() {
            view.signal
                .send_replace(Signal::Failed(ProjectionError::Unauthorized));
        }
    }

    async fn view(self: &Arc<Self>, grant: &Authorized) -> Result<Arc<View>, ProjectionError> {
        let source = lock(&self.source)
            .clone()
            .ok_or(ProjectionError::NotReady)?;
        let view = {
            let mut views = lock(&self.views);
            if let Some(view) = views.get(&grant.info.id) {
                view.clone()
            } else {
                if views.len() >= VIEW_LIMIT {
                    return Err(ProjectionError::RateLimited);
                }
                let view = Arc::new(View {
                    journal: Mutex::new(None),
                    signal: watch::channel(Signal::Pending).0,
                    activity: Mutex::new(Activity {
                        streams: 0,
                        until: Instant::now() + LEASE,
                    }),
                    wake: Notify::new(),
                    stream_limit: Arc::new(Semaphore::new(2)),
                });
                let worker = self.clone();
                let view_for_task = view.clone();
                let grant_for_task = grant.clone();
                views.insert(grant.info.id.clone(), view.clone());
                tokio::spawn(async move {
                    let result = worker.run(&view_for_task, source, &grant_for_task).await;
                    worker.finish(&view_for_task, &grant_for_task.info.id, result);
                });
                view
            }
        };
        let mut signal = view.signal.subscribe();
        tokio::time::timeout(
            BUILD_TIMEOUT,
            signal.wait_for(|signal| !matches!(signal, Signal::Pending)),
        )
        .await
        .map_err(|_| ProjectionError::NotReady)?
        .map_err(|_| ProjectionError::NotReady)?;
        let current = *signal.borrow();
        match current {
            Signal::Ready if grant.is_live() => Ok(view),
            Signal::Ready => Err(ProjectionError::Unauthorized),
            Signal::Failed(error) => Err(error),
            Signal::Pending => Err(ProjectionError::NotReady),
        }
    }

    fn finish(&self, view: &Arc<View>, grant_id: &str, error: ProjectionError) {
        view.signal.send_replace(Signal::Failed(error));
        *lock(&view.journal) = None;
        let mut views = lock(&self.views);
        if views
            .get(grant_id)
            .is_some_and(|current| Arc::ptr_eq(current, view))
        {
            views.remove(grant_id);
        }
    }

    async fn run(
        &self,
        view: &View,
        source: Arc<dyn SenateSource>,
        grant: &Authorized,
    ) -> ProjectionError {
        let result = self.run_inner(view, &*source, grant).await;
        result.err().unwrap_or(ProjectionError::Reset)
    }

    async fn run_inner(
        &self,
        view: &View,
        source: &dyn SenateSource,
        grant: &Authorized,
    ) -> Result<(), ProjectionError> {
        let mut log = source.subscribe();
        let mut shutdown = self.shutdown.subscribe();
        let mut revoked = grant.revoked.clone();
        let deadline = (grant.info.expires_at - Utc::now())
            .to_std()
            .unwrap_or_default();
        let expiry = tokio::time::sleep(deadline);
        tokio::pin!(expiry);
        let mut budget = Budget::new(self.state_budget.clone());
        let mut previous = {
            let _build = self
                .builds
                .try_acquire()
                .map_err(|_| ProjectionError::RateLimited)?;
            let first = capture(source, &grant.info).await?;
            check_view(&first.snapshot)?;
            let journal = Journal::new(first.snapshot.clone())?;
            budget.resize(journal.state_bytes())?;
            *lock(&view.journal) = Some(journal);
            view.signal.send_replace(Signal::Ready);
            first
        };
        let mut reconcile = tokio::time::interval(RECONCILE);
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            if !grant.is_live() || *shutdown.borrow() {
                return Err(ProjectionError::Unauthorized);
            }
            let idle = {
                let activity = lock(&view.activity);
                (activity.streams == 0).then_some(activity.until)
            };
            if idle.is_some_and(|until| Instant::now() >= until) {
                return Ok(());
            }
            tokio::select! {
                _ = &mut expiry => return Err(ProjectionError::Unauthorized),
                _ = shutdown.changed() => return Err(ProjectionError::Unauthorized),
                _ = revoked.changed() => return Err(ProjectionError::Unauthorized),
                _ = idle_until(idle) => continue,
                _ = view.wake.notified() => continue,
                _ = reconcile.tick() => {}
                result = log.recv() => {
                    if matches!(result, Err(broadcast::error::RecvError::Closed)) {
                        return Err(ProjectionError::Reset);
                    }
                    if let Ok(event) = result
                        && grant.info.committees.as_ref().is_some_and(|allowed| !allowed.contains(&event.committee))
                    {
                        continue;
                    }
                    tokio::time::sleep(COALESCE).await;
                    for _ in 0..MAX_DURABLE_GAP {
                        if log.try_recv().is_err() { break; }
                    }
                }
            }

            let _build = self
                .builds
                .acquire()
                .await
                .map_err(|_| ProjectionError::NotReady)?;
            let current = capture(source, &grant.info).await?;
            check_view(&current.snapshot)?;
            let mut hints =
                tokio::time::timeout(BUILD_TIMEOUT, durable_hints(source, &previous, &current))
                    .await
                    .map_err(|_| ProjectionError::Reset)??;
            hints.extend(change_hints(&previous.snapshot, &current.snapshot));
            if current.snapshot.capabilities != previous.snapshot.capabilities
                || previous
                    .heads
                    .keys()
                    .any(|committee| !current.heads.contains_key(committee))
            {
                return Err(ProjectionError::Reset);
            }
            let patch = patches(&previous.snapshot, &current.snapshot);
            if patch != Default::default() {
                hints.push(Hint {
                    committee_id: None,
                    link: None,
                    event: senate_observe_protocol::Event::StatePatch(Box::new(patch)),
                });
            }
            if !hints.is_empty() && grant.is_live() && !*shutdown.borrow() {
                let mut journal = lock(&view.journal);
                let journal = journal.as_mut().ok_or(ProjectionError::Reset)?;
                let next = journal.prepare(current.snapshot.clone(), hints)?;
                budget.resize(next.state_bytes())?;
                journal.apply(next);
                view.signal.send_replace(Signal::Ready);
            }
            previous = current;
        }
    }

    pub(crate) async fn snapshot(
        self: &Arc<Self>,
        grant: &Authorized,
    ) -> Result<Arc<str>, ProjectionError> {
        let view = self.view(grant).await?;
        let journal = lock(&view.journal);
        let state = journal
            .as_ref()
            .ok_or(ProjectionError::NotReady)?
            .snapshot();
        drop(journal);
        view.extend_lease();
        Ok(state)
    }

    pub(crate) async fn events(
        self: &Arc<Self>,
        grant: Authorized,
        cursor: Option<Cursor>,
        audit: Arc<Audit>,
    ) -> Result<Response, ProjectionError> {
        let global = self
            .streams
            .clone()
            .try_acquire_owned()
            .map_err(|_| ProjectionError::RateLimited)?;
        let view = if cursor.is_some() {
            lock(&self.views)
                .get(&grant.info.id)
                .cloned()
                .ok_or(ProjectionError::Reset)?
        } else {
            self.view(&grant).await?
        };
        let local = view
            .stream_limit
            .clone()
            .try_acquire_owned()
            .map_err(|_| ProjectionError::RateLimited)?;
        let signal = view.signal.subscribe();
        let after = {
            let mut journal = lock(&view.journal);
            let journal = journal.as_mut().ok_or(ProjectionError::NotReady)?;
            journal.prune();
            journal
                .boundary(cursor.as_ref())
                .ok_or(ProjectionError::Reset)?
        };
        let expires_in = (grant.info.expires_at - Utc::now())
            .to_std()
            .unwrap_or_default();
        let stream = Stream {
            view: view.clone(),
            signal,
            audit: StreamAudit {
                audit,
                id: grant.info.id.clone(),
                frames: 0,
                bytes: 0,
            },
            grant,
            expiry: tokio::time::Instant::now() + expires_in,
            after,
            _reservation: Reservation::new(view, global, local),
            last_poll: Instant::now(),
        };
        Ok(Sse::new(stream::unfold(stream, next_frame))
            .keep_alive(
                KeepAlive::new()
                    .interval(Duration::from_secs(15))
                    .text("heartbeat"),
            )
            .into_response())
    }
}

async fn capture(source: &dyn SenateSource, grant: &GrantInfo) -> Result<Capture, ProjectionError> {
    tokio::time::timeout(BUILD_TIMEOUT, source.capture(grant))
        .await
        .map_err(|_| ProjectionError::NotReady)?
}

fn check_view(snapshot: &Snapshot) -> Result<(), ProjectionError> {
    if snapshot.committees.len() > 256 || snapshot.senators.len() > 4096 {
        return Err(ProjectionError::TooLarge);
    }
    Ok(())
}

async fn idle_until(until: Option<Instant>) {
    if let Some(until) = until {
        tokio::time::sleep_until(until.into()).await;
    } else {
        std::future::pending().await
    }
}

async fn durable_hints(
    source: &dyn SenateSource,
    previous: &Capture,
    current: &Capture,
) -> Result<Vec<Hint>, ProjectionError> {
    let mut hints = Vec::new();
    let mut gap = 0u64;
    for (committee, head) in &current.heads {
        let before = previous.heads.get(committee).copied().unwrap_or(Seq::ZERO);
        gap = gap.saturating_add(head.0.checked_sub(before.0).ok_or(ProjectionError::Reset)?);
        if gap > MAX_DURABLE_GAP {
            return Err(ProjectionError::Reset);
        }
        if head > &before {
            let target = usize::try_from(head.0 - before.0).map_err(|_| ProjectionError::Reset)?;
            let events = source.read(committee, before, target).await?;
            if events.len() != target || events.last().is_none_or(|event| event.seq != *head) {
                return Err(ProjectionError::Reset);
            }
            for event in &events {
                if let Some(hint) = source.hint(event, &current.snapshot).await? {
                    hints.push(hint);
                }
            }
        }
    }
    let known: BTreeSet<_> = previous
        .snapshot
        .committees
        .iter()
        .map(|committee| &committee.id)
        .collect();
    for committee in &current.snapshot.committees {
        if !known.contains(&committee.id) {
            hints.push(Hint {
                committee_id: Some(committee.id.clone()),
                link: committee.link.clone(),
                event: senate_observe_protocol::Event::CommitteeCreated(
                    senate_observe_protocol::CommitteeRef {
                        committee_id: committee.id.clone(),
                    },
                ),
            });
        }
    }
    Ok(hints)
}

impl View {
    fn extend_lease(&self) {
        lock(&self.activity).until = Instant::now() + LEASE;
        self.wake.notify_one();
    }
}

struct Reservation {
    view: Arc<View>,
    _global: OwnedSemaphorePermit,
    _local: OwnedSemaphorePermit,
}

impl Reservation {
    fn new(view: Arc<View>, global: OwnedSemaphorePermit, local: OwnedSemaphorePermit) -> Self {
        lock(&view.activity).streams += 1;
        view.wake.notify_one();
        Self {
            view,
            _global: global,
            _local: local,
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut activity = lock(&self.view.activity);
        activity.streams -= 1;
        activity.until = Instant::now() + LEASE;
        self.view.wake.notify_one();
    }
}

struct Stream {
    view: Arc<View>,
    signal: watch::Receiver<Signal>,
    grant: Authorized,
    audit: StreamAudit,
    expiry: tokio::time::Instant,
    after: u64,
    _reservation: Reservation,
    last_poll: Instant,
}

async fn next_frame(mut state: Stream) -> Option<(Result<SseEvent, Infallible>, Stream)> {
    if state.after == u64::MAX || !state.grant.is_live() {
        return None;
    }
    if state.last_poll.elapsed() >= Duration::from_secs(5) {
        return None;
    }
    loop {
        if !state.grant.is_live() {
            return None;
        }
        let signaled = *state.signal.borrow_and_update();
        let next = {
            let journal = lock(&state.view.journal);
            journal
                .as_ref()
                .map_or(Ok(None), |journal| journal.next(state.after))
        };
        let reset = match (signaled, next) {
            (Signal::Failed(ProjectionError::Unauthorized), _) => return None,
            (Signal::Failed(_), _) => Some(ResetReason::SourceReset),
            (_, Err(_)) => Some(ResetReason::Overrun),
            (_, Ok(Some(frame))) => {
                state.after = frame.position;
                state.last_poll = Instant::now();
                state.audit.frames += 1;
                state.audit.bytes += frame.data.len() as u64;
                let event = SseEvent::default()
                    .id(frame.cursor.as_str())
                    .data(&*frame.data);
                return Some((Ok(event), state));
            }
            _ => None,
        };
        if let Some(reason) = reset {
            let frame = ResetFrame {
                schema_version: V1,
                at: Utc::now(),
                kind: ResetKind::StreamReset,
                data: ResetData { reason },
            };
            let event = SseEvent::default().json_data(frame).ok()?;
            state.after = u64::MAX;
            // The next poll observes the terminal position and closes.
            return Some((Ok(event), state));
        }
        tokio::select! {
            _ = tokio::time::sleep_until(state.expiry) => return None,
            _ = state.grant.revoked.changed() => return None,
            result = state.signal.changed() => if result.is_err() { return None; }
        }
    }
}

struct StreamAudit {
    audit: Arc<Audit>,
    id: String,
    frames: u64,
    bytes: u64,
}

impl Drop for StreamAudit {
    fn drop(&mut self) {
        self.audit.stream_closed(&self.id, self.frames, self.bytes);
    }
}

struct Budget {
    pool: Arc<Semaphore>,
    held: u32,
}

impl Budget {
    fn new(pool: Arc<Semaphore>) -> Self {
        Self { pool, held: 0 }
    }

    fn resize(&mut self, bytes: usize) -> Result<(), ProjectionError> {
        let bytes = u32::try_from(bytes).map_err(|_| ProjectionError::TooLarge)?;
        if bytes > self.held {
            self.pool
                .try_acquire_many(bytes - self.held)
                .map_err(|_| ProjectionError::TooLarge)?
                .forget();
        } else {
            self.pool.add_permits((self.held - bytes) as usize);
        }
        self.held = bytes;
        Ok(())
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        self.pool.add_permits(self.held as usize);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{CreateGrant, Registry, Scope};
    use senate_observe_protocol::{Senate, Windows};

    struct Source {
        calls: AtomicUsize,
        log: broadcast::Sender<Event>,
    }

    #[async_trait]
    impl SenateSource for Source {
        fn subscribe(&self) -> broadcast::Receiver<Event> {
            self.log.subscribe()
        }

        async fn capture(&self, _: &GrantInfo) -> Result<Capture, ProjectionError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(Capture {
                snapshot: base_snapshot()?,
                heads: BTreeMap::new(),
            })
        }

        async fn read(
            &self,
            _: &CommitteeId,
            _: Seq,
            _: usize,
        ) -> Result<Vec<Event>, ProjectionError> {
            Ok(Vec::new())
        }

        async fn hint(&self, _: &Event, _: &Snapshot) -> Result<Option<Hint>, ProjectionError> {
            Ok(None)
        }
    }

    fn base_snapshot() -> Result<Snapshot, ProjectionError> {
        Ok(Snapshot {
            schema_version: V1,
            capabilities: vec![],
            as_of: Utc::now(),
            cursor: crate::journal::new_cursor()?,
            senate: Senate {
                id: "fixture".into(),
                name: "fixture".into(),
                link: None,
                person: None,
                desk_owner: None,
                desk_workers: vec![],
            },
            navigation: None,
            links: vec![],
            committees: vec![],
            projects: vec![],
            senators: vec![],
            threads: vec![],
            desk_items: vec![],
            approvals: vec![],
            windows: Windows {
                open_threads_per_committee: 40,
                settled_threads_per_committee: 10,
                desk_items_per_committee: 50,
                truncated: vec![],
            },
        })
    }

    #[tokio::test]
    async fn source_work_requires_a_live_projection_reservation() {
        let result = check_source_reservation().await;
        assert!(result.is_ok(), "{result:?}");
    }

    async fn check_source_reservation() -> Result<(), String> {
        let home = tempfile::tempdir().map_err(|e| e.to_string())?;
        let registry = Registry::open(
            home.path(),
            "127.0.0.1:9456"
                .parse::<std::net::SocketAddr>()
                .map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        let source = Arc::new(Source {
            calls: AtomicUsize::new(0),
            log: broadcast::channel(8).0,
        });
        registry.attach_senate(source.clone());
        assert_eq!(source.log.receiver_count(), 0);
        let issued = registry
            .create(
                CreateGrant {
                    name: "fixture scene".into(),
                    committees: None,
                    all_committees: true,
                    scope: Scope::Metadata,
                    origins: vec![],
                    topology: false,
                    expires_in_days: None,
                },
                BTreeSet::new(),
            )
            .map_err(|e| e.to_string())?;
        assert_eq!(source.calls.load(Ordering::Relaxed), 0);
        let grant = registry
            .authenticate(&issued.token, None, false)
            .map_err(|_| "failed auth")?;
        registry
            .projections
            .snapshot(&grant)
            .await
            .map_err(|e| format!("{e:?}"))?;
        let view = lock(&registry.projections.views)
            .get(&grant.info.id)
            .cloned()
            .ok_or("missing view")?;
        assert!(source.calls.load(Ordering::Relaxed) > 0);
        assert_eq!(source.log.receiver_count(), 1);
        lock(&view.activity).until = Instant::now();
        view.wake.notify_one();
        let mut signal = view.signal.subscribe();
        tokio::time::timeout(
            Duration::from_secs(1),
            signal.wait_for(|signal| matches!(signal, Signal::Failed(_))),
        )
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        assert_eq!(source.log.receiver_count(), 0);
        assert!(lock(&registry.projections.views).is_empty());
        Ok(())
    }

    #[test]
    fn an_oversized_update_keeps_the_prior_boundary_and_snapshot() {
        let result = check_big_update();
        assert!(result.is_ok(), "{result:?}");
    }

    fn check_big_update() -> Result<(), ProjectionError> {
        use senate_observe_protocol::{Event, MessageKind, MessagePosted};

        let snapshot = base_snapshot()?;
        let mut journal = Journal::new(snapshot.clone())?;
        let first = journal.snapshot();
        let seq = "42".parse().map_err(|_| ProjectionError::NotReady)?;
        let hint = |contents: String| Hint {
            committee_id: Some("senate".into()),
            link: None,
            event: Event::MessagePosted(MessagePosted {
                seq,
                message_kind: MessageKind::Note,
                from: senate_observe_protocol::Address::External,
                to: vec![],
                root_seq: None,
                line: Some(contents),
            }),
        };
        assert!(
            journal
                .prepare(snapshot.clone(), vec![hint("x".repeat(64 * 1024))])
                .is_err()
        );
        assert_eq!(journal.snapshot(), first);
        let next = journal.prepare(snapshot, vec![hint("hello".into())])?;
        journal.apply(next);
        assert_ne!(journal.snapshot(), first);
        assert!(journal.next(0)?.is_some());
        Ok(())
    }
}
