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
    self, ApprovalsReply, ApproveReply, ClientError, DocketReply, InboxReply, MAX_LINE, Posted,
    Reply, Request, SessionRow, SessionSelector, SessionsReply, WatchEvent,
};
use senate_core::alias::KnownAliases;
use senate_core::{CommitteeId, Locale, SenatorName};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::model::{Action, Effects, Failure, Model, Notice, Offline, ReadOf, Refresh, message};

/// How often the aliases are read again on a status sweep.
const ALIASES_EVERY: Duration = Duration::from_secs(300);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Local reads.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Ops that append to the server's log through the daemon.
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);

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
        let timeout = match request {
            Request::Answer { .. }
            | Request::Take { .. }
            | Request::Task { .. }
            | Request::Stop { .. }
            | Request::Restore { .. } => WRITE_TIMEOUT,
            _ => READ_TIMEOUT,
        };
        ipc::request(&self.socket, request, CONNECT_TIMEOUT, timeout).await
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
        let mut line = serde_json::to_string(&Request::Watch { app: true })
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        line.push('\n');
        stream.reader.get_mut().write_all(line.as_bytes()).await?;
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
    /// The model changed; re-render.
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
    /// When the aliases were last read.
    aliases_read: Mutex<Option<Instant>>,
    model: Mutex<Model>,
    sink: S,
    backoff: Backoff,
}

impl<S: Sink> Driver<S> {
    pub fn new(link: Link, sink: S, backoff: Backoff, locale: Locale) -> Arc<Self> {
        Arc::new(Self {
            link,
            aliases_read: Mutex::new(None),
            model: Mutex::new(Model::new(locale)),
            sink,
            backoff,
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

    /// Hand the current model to the sink.
    pub fn changed(&self) {
        let model = self.model().clone();
        self.sink.changed(&model);
    }

    /// Hold a `watch` stream for as long as the app runs, reconnecting with
    /// backoff while the daemon is down.
    pub async fn run(self: Arc<Self>) {
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
                                self.changed();
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
            tokio::time::sleep(delay).await;
            delay = self.backoff.next(delay);
        }
    }

    fn effects(self: &Arc<Self>, fx: Effects) {
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
        if fx.refresh.is_empty() {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            for r in fx.refresh {
                this.refresh(r).await;
            }
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
            Refresh::Docket => self.refresh_docket().await,
            Refresh::Aliases => self.refresh_aliases(false).await,
            Refresh::Sessions => self.refresh_sessions().await,
        }
        self.changed();
    }

    /// Read the aliases into [`crate::names`]; unless `force`, only when the
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

    fn read_failed(&self, what: ReadOf, e: &ClientError) {
        tracing::debug!(?what, "read failed: {e}");
        self.model().last_error = Some(Failure::Read {
            what,
            message: message(e),
        });
    }

    /// Run one of the window's actions; returns the daemon's result.
    pub async fn act(&self, action: Action) -> Result<Value, Failure> {
        let request = action.request()?;
        let result = match &action {
            Action::Inbox { agent, committee } => {
                self.refresh_inbox(agent.clone(), committee.clone()).await;
                self.changed();
                return Ok(Value::Null);
            }
            Action::Approvals => {
                self.refresh(Refresh::Approvals).await;
                return Ok(Value::Null);
            }
            Action::Docket => {
                self.refresh(Refresh::Docket).await;
                return Ok(Value::Null);
            }
            Action::Approve { id, .. } => {
                let r = self.link.call::<ApproveReply>(&request).await;
                if r.is_ok() {
                    self.model().approval_answered(id);
                }
                r.map(to_value)
            }
            Action::Answer { .. } | Action::Take { .. } | Action::Close { .. } => {
                self.link.call::<Posted>(&request).await.map(to_value)
            }
            Action::MarkRead { .. } => self.link.call::<Value>(&request).await,
            Action::StopSession { .. } | Action::RestoreSession { .. } => {
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
