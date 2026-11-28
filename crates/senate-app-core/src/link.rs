//! The app's connection to the developer-machine daemon (`senated
//! --machine`) over `bridge.sock`, and the [`Driver`] that keeps a [`Model`]
//! current from it.
//!
//! Requests use the bridge's own client ([`ipc::request`]). The `watch`
//! stream is read here: the bridge's client reads one reply per request,
//! and `watch` answers once and then keeps writing [`WatchEvent`] lines.
//!
//! Nothing here knows about a UI; the FFI facade (and the tests) are a
//! [`Sink`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use senate_bridge::approvals::Approval;
use senate_bridge::ipc::{
    self, ActivityBatch, ApprovalsReply, ApproveReply, ClientError, DocketReply, InboxReply,
    MAX_LINE, Posted, Reply, Request, SessionRow, SessionSelector, SessionsReply, TimelineUpdate,
    WatchEvent,
};
use senate_bridge::local::Stream as UnixStream;
use senate_core::alias::KnownAliases;
use senate_core::{CommitteeId, Locale, SenatorName};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::model::{Action, Effects, Failure, Model, Notice, Offline, ReadOf, Refresh, message};

/// How often `sessions` is read at most while the daemon keeps saying it
/// changed.
const SESSIONS_EVERY: Duration = Duration::from_millis(300);
/// The least time between two snapshots handed to the sink while the model
/// keeps changing: one frame in two or three, sooner than a person sees.
pub const PUBLISH_EVERY: Duration = Duration::from_millis(40);
/// How often the aliases are read again on a status sweep.
const ALIASES_EVERY: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Local reads.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Ops that append to the server's log through the daemon.
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
/// A pause: the daemon's own wait for the checkpoint, and a margin.
const PAUSE_TIMEOUT: Duration = Duration::from_secs(senate_bridge::ipc::PAUSE_TIMEOUT_SECS + 15);
/// Ensuring the default secretary may start it: as long as a spawn.
const SPAWN_TIMEOUT: Duration =
    Duration::from_secs(senate_bridge::host::REQUEST_TIMEOUT.as_secs() + 15);

/// The daemon's socket.
#[derive(Debug, Clone)]
pub struct Link {
    socket: PathBuf,
}

impl Link {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// One request on a fresh connection.
    pub async fn call<T: DeserializeOwned>(&self, request: &Request) -> Result<T, ClientError> {
        let mut timeout = match request {
            Request::Answer { .. }
            | Request::Take { .. }
            | Request::Task { .. }
            | Request::Stop { .. }
            | Request::Restore { .. }
            | Request::Say { .. }
            | Request::ConferenceConvene { .. }
            | Request::ConferenceRsvp { .. }
            | Request::ConferenceStart { .. }
            | Request::ConferencePost { .. }
            | Request::ConferenceAdjourn { .. } => WRITE_TIMEOUT,
            // It waits for the agent's checkpoint (90 s at most).
            Request::Pause { .. } => PAUSE_TIMEOUT,
            Request::EnsureSecretary => SPAWN_TIMEOUT,
            _ => READ_TIMEOUT,
        };
        if senate_bridge::app_confirmation::required(request) {
            timeout += Duration::from_secs(65);
        }
        ipc::request(&self.socket, request, CONNECT_TIMEOUT, timeout).await
    }

    pub(crate) async fn session_drop(
        &self,
        session_id: String,
        name: String,
        bytes: &[u8],
    ) -> Result<String, ClientError> {
        ipc::IpcClient::connect(&self.socket, CONNECT_TIMEOUT)
            .await?
            .session_drop(session_id, name, bytes)
            .await
    }

    /// Open a `watch` stream.
    pub async fn watch(&self) -> Result<WatchStream, ClientError> {
        let stream =
            match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket)).await {
                Ok(Ok(s)) => s,
                Ok(Err(source)) => {
                    return Err(ClientError::Connect {
                        path: self.socket.clone(),
                        source,
                    });
                }
                Err(_) => return Err(ClientError::Timeout),
            };
        let mut stream = WatchStream {
            reader: BufReader::new(stream),
        };
        // Through `request_line`, like every other request: the daemon serves
        // a watch only to a caller it can identify, and that is where the
        // app's proof is attached.
        let line = ipc::request_line(&self.socket, &Request::Watch { app: true }).await?;
        stream.reader.get_mut().write_all(&line).await?;
        let first = tokio::time::timeout(READ_TIMEOUT, stream.line())
            .await
            .map_err(|_| ClientError::Timeout)??
            .ok_or_else(|| ClientError::Protocol("connection closed".into()))?;
        let reply: Reply =
            serde_json::from_str(&first).map_err(|e| ClientError::Protocol(e.to_string()))?;
        match (reply.ok, reply.error) {
            (true, _) => Ok(stream),
            (false, Some(e)) => Err(ClientError::Remote(e)),
            (false, None) => Err(ClientError::Protocol("error without detail".into())),
        }
    }
}

impl Link {
    /// Follow one session's timeline (`timeline_watch`) from after `after`:
    /// its own connection, whose lines are [`TimelineUpdate`]s, the first
    /// one the catch-up. A refusal comes as a reply instead.
    pub async fn timeline_watch(
        &self,
        session_id: &str,
        after: u64,
    ) -> Result<TimelineStream, ClientError> {
        let stream =
            match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket)).await {
                Ok(Ok(s)) => s,
                Ok(Err(source)) => {
                    return Err(ClientError::Connect {
                        path: self.socket.clone(),
                        source,
                    });
                }
                Err(_) => return Err(ClientError::Timeout),
            };
        let mut lines = WatchStream {
            reader: BufReader::new(stream),
        };
        let line = ipc::request_line(
            &self.socket,
            &Request::TimelineWatch {
                session_id: session_id.to_string(),
                after,
            },
        )
        .await?;
        lines.reader.get_mut().write_all(&line).await?;
        let first = tokio::time::timeout(READ_TIMEOUT, lines.line())
            .await
            .map_err(|_| ClientError::Timeout)??
            .ok_or_else(|| ClientError::Protocol("connection closed".into()))?;
        if let Ok(update) = serde_json::from_str::<TimelineUpdate>(&first) {
            return Ok(TimelineStream {
                lines,
                first: Some(update),
            });
        }
        let reply: Reply =
            serde_json::from_str(&first).map_err(|e| ClientError::Protocol(e.to_string()))?;
        Err(match reply.error {
            Some(e) => ClientError::Remote(e),
            None => ClientError::Protocol("timeline_watch: no update".into()),
        })
    }
}

impl Link {
    pub(crate) async fn activity_watch(
        &self,
        request: &Request,
    ) -> Result<ActivityStream, ClientError> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(&self.socket))
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(|source| ClientError::Connect {
                path: self.socket.clone(),
                source,
            })?;
        let mut lines = WatchStream {
            reader: BufReader::new(stream),
        };
        let line = ipc::request_line(&self.socket, request).await?;
        lines.reader.get_mut().write_all(&line).await?;
        let first = tokio::time::timeout(READ_TIMEOUT, lines.line())
            .await
            .map_err(|_| ClientError::Timeout)??
            .ok_or_else(|| ClientError::Protocol("connection closed".into()))?;
        if let Ok(batch) = serde_json::from_str::<ActivityBatch>(&first) {
            return Ok(ActivityStream {
                lines,
                first: Some(batch),
            });
        }
        Err(serde_json::from_str::<Reply>(&first)
            .ok()
            .and_then(|reply| reply.error)
            .map_or_else(
                || ClientError::Protocol("activity_watch: no batch or refusal".into()),
                ClientError::Remote,
            ))
    }
}

pub(crate) struct ActivityStream {
    lines: WatchStream,
    first: Option<ActivityBatch>,
}

impl ActivityStream {
    pub(crate) async fn next(&mut self) -> Result<Option<ActivityBatch>, ClientError> {
        if let Some(batch) = self.first.take() {
            return Ok(Some(batch));
        }
        self.lines
            .line()
            .await?
            .map(|line| {
                serde_json::from_str(&line).map_err(|e| ClientError::Protocol(e.to_string()))
            })
            .transpose()
    }
}

/// An open `timeline_watch` connection. Dropping it stops watching.
pub struct TimelineStream {
    lines: WatchStream,
    first: Option<TimelineUpdate>,
}

impl TimelineStream {
    /// The next update; `None` when the daemon closed the stream.
    pub async fn next(&mut self) -> Result<Option<TimelineUpdate>, ClientError> {
        if let Some(first) = self.first.take() {
            return Ok(Some(first));
        }
        loop {
            let Some(line) = self.lines.line().await? else {
                return Ok(None);
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<TimelineUpdate>(&line) {
                Ok(update) => return Ok(Some(update)),
                Err(e) => tracing::debug!("skipping timeline line: {e}"),
            }
        }
    }
}

/// An open `watch` connection.
pub struct WatchStream {
    reader: BufReader<UnixStream>,
}

impl WatchStream {
    /// The next event; `None` when the daemon closed the stream.
    pub async fn next(&mut self) -> Result<Option<WatchEvent>, ClientError> {
        loop {
            let Some(line) = self.line().await? else {
                return Ok(None);
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<WatchEvent>(&line) {
                Ok(event) => return Ok(Some(event)),
                // A newer daemon may add events; skip what this app does
                // not know rather than drop the stream.
                Err(e) => tracing::debug!("skipping watch line: {e}: {line}"),
            }
        }
    }

    async fn line(&mut self) -> Result<Option<String>, ClientError> {
        let mut buf = Vec::new();
        let limit = u64::try_from(MAX_LINE).unwrap_or(u64::MAX) + 1;
        let n = (&mut self.reader)
            .take(limit)
            .read_until(b'\n', &mut buf)
            .await?;
        if n == 0 {
            return Ok(None);
        }
        if buf.last() != Some(&b'\n') {
            return Err(ClientError::Protocol(if buf.len() > MAX_LINE {
                format!("line longer than {MAX_LINE} bytes")
            } else {
                "connection closed mid-line".into()
            }));
        }
        buf.pop();
        String::from_utf8(buf)
            .map(Some)
            .map_err(|e| ClientError::Protocol(e.to_string()))
    }
}

/// Where the driver's changes go: the tray and the window, or a test.
pub trait Sink: Send + Sync + 'static {
    /// The model changed; re-render. Called with one model at a time, each
    /// later than the one before; after any run of changes, with the model
    /// as the last of them left it.
    fn changed(&self, model: &Model);
    /// A Codex approval is waiting (first sight only).
    fn approval_arrived(&self, _approval: &Approval) {}
    /// Something came back to the person (the app notifies; the daemon
    /// stays quiet while it is connected).
    fn for_person(&self, _committee: &str, _thread: u64, _title: &str, _body: &str) {}
    /// A permission prompt waits in a hosted terminal (first sight only).
    fn prompt_waiting(&self, _row: &SessionRow) {}
}

/// Reconnect delays: doubling from `min` to `max`, reset by a connection.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub min: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            min: Duration::from_millis(500),
            max: Duration::from_secs(10),
        }
    }
}

impl Backoff {
    pub fn next(&self, current: Duration) -> Duration {
        (current * 2).clamp(self.min, self.max)
    }
}

/// Keeps a [`Model`] current from the daemon and runs the window's actions.
pub struct Driver<S: Sink> {
    link: Link,
    /// Cuts a reconnect's wait short ([`Driver::reconnect_now`]).
    wake: tokio::sync::Notify,
    /// When the aliases were last read.
    aliases_read: Mutex<Option<Instant>>,
    model: Mutex<Model>,
    sink: S,
    backoff: Backoff,
    /// A `sessions` read is scheduled ([`SESSIONS_EVERY`]).
    sessions_due: std::sync::atomic::AtomicBool,
    /// The model changed since the sink was last handed it.
    dirty: std::sync::atomic::AtomicBool,
    /// Wakes the publisher ([`Driver::publisher`]) when it did.
    publish: tokio::sync::Notify,
    /// When the sink was last handed the model. Held while it is copied and
    /// handed over, so the sink gets one model at a time, in order.
    published: Mutex<Option<tokio::time::Instant>>,
}

/// Ends the publisher with the [`Driver::run`] that started it.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<S: Sink> Driver<S> {
    pub fn new(link: Link, sink: S, backoff: Backoff, locale: Locale) -> Arc<Self> {
        Arc::new(Self {
            link,
            wake: tokio::sync::Notify::new(),
            aliases_read: Mutex::new(None),
            model: Mutex::new(Model::new(locale)),
            sink,
            backoff,
            sessions_due: std::sync::atomic::AtomicBool::new(false),
            dirty: std::sync::atomic::AtomicBool::new(false),
            publish: tokio::sync::Notify::new(),
            published: Mutex::new(None),
        })
    }

    pub fn link(&self) -> &Link {
        &self.link
    }

    pub fn model(&self) -> MutexGuard<'_, Model> {
        // A panic while holding the lock leaves a model that is still
        // readable; keep going with it.
        self.model.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write texts in another language from now on, and re-render.
    pub fn set_locale(&self, locale: Locale) {
        self.model().locale = locale;
        self.changed();
    }

    /// While offline, try to connect now instead of after the current wait.
    /// Does nothing while connected.
    pub fn reconnect_now(&self) {
        self.wake.notify_one();
    }

    /// The model changed: the sink is handed it soon, by the publisher of
    /// a running [`Driver::run`]. At once when nothing was handed over in the
    /// last [`PUBLISH_EVERY`], else when that has passed; changes made
    /// meanwhile go in the same snapshot, so the daemon's bursts (a line
    /// per hook of every session, the reads of a status sweep) cost one
    /// snapshot per [`PUBLISH_EVERY`] and not one each.
    pub fn changed(&self) {
        use std::sync::atomic::Ordering;
        crate::perf::CHANGED.fetch_add(1, Ordering::Relaxed);
        self.dirty.store(true, Ordering::Release);
        if crate::perf::every_change() {
            // The baseline of the measure: every change its own snapshot.
            self.publish_now();
            return;
        }
        self.publish.notify_one();
    }

    /// Hand the model to the sink now, if it changed since it last was:
    /// before a notice about what changed, and at the end of an action, so
    /// both find the state they are about already there.
    pub fn publish_now(&self) {
        let mut published = self.published.lock().unwrap_or_else(|e| e.into_inner());
        // Cleared before the copy: a change after it is in this snapshot or
        // marks the next.
        if !self.dirty.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let model = crate::perf::CLONE.time(|| self.model().clone());
        self.sink.changed(&model);
        *published = Some(tokio::time::Instant::now());
    }

    /// Hands the sink the model when it changed, no sooner than
    /// [`PUBLISH_EVERY`] after the last time: the first change after a quiet
    /// spell goes at once, and the last of a burst is never left out (it
    /// wakes this again while the snapshot before it is still on its way).
    async fn publisher(self: Arc<Self>) {
        loop {
            self.publish.notified().await;
            let last = *self.published.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(wait) = last
                .map(|at| PUBLISH_EVERY.saturating_sub(at.elapsed()))
                .filter(|wait| !wait.is_zero())
            {
                tokio::time::sleep(wait).await;
            }
            self.publish_now();
        }
    }

    /// Hold a `watch` stream for as long as the app runs, reconnecting with
    /// backoff while the daemon is down.
    pub async fn run(self: Arc<Self>) {
        // Its own task: the stream is read while a snapshot is built.
        let publisher = tokio::spawn(Arc::clone(&self).publisher());
        let _publisher = AbortOnDrop(publisher.abort_handle());
        let mut delay = self.backoff.min;
        loop {
            match self.link.watch().await {
                Ok(mut stream) => {
                    delay = self.backoff.min;
                    tracing::info!("watching the daemon on {}", self.link.socket.display());
                    self.model().connected();
                    // A restarted daemon may serve other names.
                    *self.aliases_read.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    self.changed();
                    let reason = loop {
                        match stream.next().await {
                            Ok(Some(event)) => {
                                let fx = self.model().apply(event);
                                // A line that only says what to read again
                                // changes nothing to show: the read's own
                                // `changed` follows it. The daemon sends
                                // `sessions` on every hook of every session,
                                // and each built a snapshot to find it was
                                // the one the app has.
                                if fx.touched {
                                    self.changed();
                                }
                                self.effects(fx);
                            }
                            Ok(None) => break Offline::Closed,
                            Err(e) => break Offline::from(&e),
                        }
                    };
                    tracing::info!("watch stream ended: {reason:?}");
                    self.model().disconnected(reason);
                    self.changed();
                }
                Err(e) => {
                    let reason = Offline::from(&e);
                    let first = {
                        let mut m = self.model();
                        let first = m.online || m.offline.as_ref() != Some(&reason);
                        m.disconnected(reason);
                        first
                    };
                    if first {
                        tracing::info!("daemon unreachable: {e}");
                        self.changed();
                    }
                }
            }
            tokio::select! {
                () = tokio::time::sleep(delay) => delay = self.backoff.next(delay),
                // Asked to try now: from the shortest wait again.
                () = self.wake.notified() => delay = self.backoff.min,
            }
        }
    }

    fn effects(self: &Arc<Self>, fx: Effects) {
        if !fx.notices.is_empty() {
            // A notice comes after the state that holds what it is about,
            // and as soon as it always did.
            self.publish_now();
        }
        for notice in fx.notices {
            match notice {
                Notice::ApprovalArrived(a) => self.sink.approval_arrived(&a),
                Notice::PromptWaiting(row) => self.sink.prompt_waiting(&row),
                Notice::ForPerson {
                    committee,
                    thread,
                    title,
                    body,
                } => self
                    .sink
                    .for_person(committee.as_str(), thread.0, &title, &body),
            }
        }
        let mut refresh = fx.refresh;
        // The daemon says `sessions` on every hook of every session: one
        // read at most every [`SESSIONS_EVERY`], after the latest.
        if refresh.contains(&Refresh::Sessions) {
            refresh.retain(|r| *r != Refresh::Sessions);
            self.sessions_soon();
        }
        if refresh.is_empty() {
            return;
        }
        let fx = Effects {
            refresh,
            notices: Vec::new(),
            touched: false,
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            for r in fx.refresh {
                this.refresh(r).await;
            }
        });
    }

    /// Read `sessions` once [`SESSIONS_EVERY`] has passed, unless a read
    /// is already due: what arrives meanwhile is in that read.
    fn sessions_soon(self: &Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.sessions_due.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(SESSIONS_EVERY).await;
            this.sessions_due.store(false, Ordering::Release);
            this.refresh(Refresh::Sessions).await;
        });
    }

    /// Make one read and fold its reply.
    pub async fn refresh(&self, what: Refresh) {
        match what {
            Refresh::Inbox { agent, committee } => self.refresh_inbox(agent, committee).await,
            Refresh::AllInboxes => {
                let targets = self.model().targets();
                for (agent, committee) in targets {
                    self.refresh_inbox(agent, committee).await;
                }
            }
            Refresh::Approvals => {
                match self.link.call::<ApprovalsReply>(&Request::Approvals).await {
                    Ok(reply) => {
                        let mut m = self.model();
                        m.set_approvals(reply);
                        m.clear_read(&ReadOf::Approvals);
                    }
                    Err(e) => self.read_failed(ReadOf::Approvals, &e),
                }
            }
            Refresh::Roster => {
                if let Ok(roster) = self.link.call(&Request::Roster).await {
                    self.model().roster = roster;
                }
            }
            Refresh::Docket => self.refresh_docket().await,
            Refresh::Conferences => self.refresh_conferences().await,
            Refresh::Aliases => self.refresh_aliases(false).await,
            Refresh::Sessions => self.refresh_sessions().await,
        }
        self.changed();
    }

    /// Read `aliases` once every few minutes unless forced.
    /// last read is older than [`ALIASES_EVERY`]. A failure keeps the old
    /// names (an id is still a correct way to show a seat).
    pub async fn refresh_aliases(&self, force: bool) {
        {
            let mut last = self.aliases_read.lock().unwrap_or_else(|e| e.into_inner());
            if !force && last.is_some_and(|at| at.elapsed() < ALIASES_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        match self.link.call::<KnownAliases>(&Request::Aliases).await {
            Ok(aliases) => crate::names::update(|n| n.aliases = Some(aliases)),
            Err(e) => tracing::debug!("reading aliases: {e}"),
        }
    }

    async fn refresh_sessions(&self) {
        match self.link.call::<SessionsReply>(&Request::Sessions).await {
            Ok(reply) => {
                let notices = {
                    let mut m = self.model();
                    let notices = m.set_sessions(reply);
                    m.clear_read(&ReadOf::Sessions);
                    notices
                };
                for notice in notices {
                    if let Notice::PromptWaiting(row) = notice {
                        self.sink.prompt_waiting(&row);
                    }
                }
            }
            Err(e) => self.read_failed(ReadOf::Sessions, &e),
        }
    }

    async fn refresh_inbox(&self, agent: SenatorName, committee: CommitteeId) {
        let what = ReadOf::Inbox {
            agent: agent.clone(),
            committee: committee.clone(),
        };
        let request = match (Action::Inbox { agent, committee }).request() {
            Ok(r) => r,
            Err(e) => {
                self.model().last_error = Some(e);
                return;
            }
        };
        match self.link.call::<InboxReply>(&request).await {
            Ok(reply) => {
                let mut m = self.model();
                m.set_inbox(reply);
                m.clear_read(&what);
            }
            Err(e) => self.read_failed(what, &e),
        }
    }

    async fn refresh_docket(&self) {
        if !self.model().presiding() {
            self.model().set_docket(None);
            return;
        }
        let request = Request::Docket {
            session: SessionSelector::default(),
            committee: None,
        };
        match self.link.call::<DocketReply>(&request).await {
            Ok(reply) => {
                let mut m = self.model();
                m.set_docket(Some(reply));
                m.clear_read(&ReadOf::Docket);
            }
            Err(e) => self.read_failed(ReadOf::Docket, &e),
        }
    }

    async fn refresh_conferences(&self) {
        let request = Request::ConferenceList {
            session: SessionSelector::default(),
        };
        if let Ok(reply) = self
            .link
            .call::<senate_core::ConferenceList>(&request)
            .await
        {
            self.model().set_conferences(reply);
        }
    }

    fn read_failed(&self, what: ReadOf, e: &ClientError) {
        tracing::debug!(?what, "read failed: {e}");
        self.model().last_error = Some(Failure::Read {
            what,
            message: message(e),
        });
    }

    /// Run one of the window's actions; returns the daemon's result. The
    /// sink has what the action changed before this returns.
    pub async fn act(&self, action: Action) -> Result<Value, Failure> {
        let request = action.request()?;
        let result = match &action {
            Action::Inbox { agent, committee } => {
                self.refresh_inbox(agent.clone(), committee.clone()).await;
                self.changed();
                self.publish_now();
                return Ok(Value::Null);
            }
            Action::Approvals => {
                self.refresh(Refresh::Approvals).await;
                self.publish_now();
                return Ok(Value::Null);
            }
            Action::Docket => {
                self.refresh(Refresh::Docket).await;
                self.publish_now();
                return Ok(Value::Null);
            }
            Action::Approve { id, .. } => {
                let r = self.link.call::<ApproveReply>(&request).await;
                if r.is_ok() {
                    self.model().approval_answered(id);
                }
                r.map(to_value)
            }
            Action::Answer { .. }
            | Action::Take { .. }
            | Action::Close { .. }
            | Action::Accept { .. }
            | Action::Say { .. } => self.link.call::<Posted>(&request).await.map(to_value),
            Action::MarkRead { .. } => self.link.call::<Value>(&request).await,
            Action::StopSession { .. }
            | Action::RestoreSession { .. }
            | Action::ApproveSession { .. }
            | Action::ResumeSession { .. }
            | Action::PauseSession { .. } => {
                let r = self.link.call::<Value>(&request).await;
                self.refresh_sessions().await;
                r
            }
        };
        if let Some(committee) = action.committee() {
            self.refresh_committee(committee).await;
        }
        let result = result.map_err(|e| Failure::Daemon(message(&e)));
        self.model().last_error = result.as_ref().err().cloned();
        self.changed();
        self.publish_now();
        result
    }
}

impl<S: Sink> Driver<S> {
    /// A read that is not state (a thread, the hosted sessions, the roles):
    /// its reply goes back to the caller, nothing is folded.
    pub async fn read<T: DeserializeOwned>(&self, request: &Request) -> Result<T, Failure> {
        self.link
            .call::<T>(request)
            .await
            .map_err(|e| Failure::Daemon(message(&e)))
    }

    pub(crate) async fn refresh_desk(&self) {
        self.refresh_docket().await;
        self.changed();
    }

    pub(crate) async fn refresh_status(&self) {
        if let Ok(reply) = self
            .link
            .call::<senate_bridge::ipc::StatusReply>(&Request::Status)
            .await
        {
            self.model().status = Some(reply);
            self.changed();
        }
    }

    /// Re-read every inbox of the primary Senate on `committee`, then
    /// re-render: after an action there, or reading a thread (which marks
    /// it seen).
    pub async fn refresh_committee(&self, committee: &CommitteeId) {
        let agents: Vec<SenatorName> = self
            .model()
            .targets()
            .into_iter()
            .filter(|(_, c)| c == committee)
            .map(|(a, _)| a)
            .collect();
        for agent in agents {
            self.refresh_inbox(agent, committee.clone()).await;
        }
        self.changed();
    }
}

fn to_value<T: serde::Serialize>(v: T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_max() {
        let b = Backoff {
            min: Duration::from_millis(100),
            max: Duration::from_millis(350),
        };
        assert_eq!(
            b.next(Duration::from_millis(100)),
            Duration::from_millis(200)
        );
        assert_eq!(
            b.next(Duration::from_millis(200)),
            Duration::from_millis(350)
        );
        assert_eq!(b.next(Duration::ZERO), Duration::from_millis(100));
    }
}
