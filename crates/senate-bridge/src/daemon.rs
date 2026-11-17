//! The per-machine daemon.
//!
//! # One person, several agents
//!
//! The daemon holds one person's token and seats one junior senator per kind
//! of coding agent it serves: every registered session speaks as one agent
//! ([`crate::agent`]: `<person>/claude` for Claude Code, `<person>/codex`
//! for Codex, unless renamed or pinned). Everything the server keeps per
//! senator is kept per agent here too: each agent has its own stream on
//! each committee, its own `delivered` and `read` cursors, heartbeat,
//! checkpoints, queue and inbox. Two agents of one person on one committee
//! never see each other's directed messages and never share a cursor.
//!
//! Tasks, all sharing one `Shared` state:
//!
//! - **IPC server** on `bridge.sock` ([`crate::ipc`]): hooks, MCP, status.
//! - **Supervisor**: keeps one stream worker per (agent, committee) — each
//!   live bound session's agent on its committee, plus the committees in
//!   `senate.toml` for every agent this machine has a session of on record.
//!   A configured committee opens nothing for an agent that has never had a
//!   session here, so configuration alone never seats an agent.
//! - **Stream workers** (`stream` module): seed the agent's cursor on first
//!   join, then hold the SSE stream (`?senator=<agent>`) with reconnect and
//!   `Last-Event-ID` = the agent's durable `delivered` cursor, plus cursor
//!   reports and the agent's heartbeat every [`Timing::heartbeat`].
//! - **Batcher**: once directed events stop arriving for
//!   [`Timing::batch_window`], routes every pending item to one session of
//!   its agent and pushes each session's items as one message through the
//!   [`Ladder`].
//! - **Sweeper**: marks sessions whose Claude process is gone as stale and
//!   prunes old state.
//!
//! # Which Senate
//!
//! `GET /v1/me` names the person and the Senate the server serves, by its
//! presider ([`senate_core::api::Me::presider`]). The daemon checks the
//! person against `senate.toml`, names its agents under the presider when
//! there is one ([`Seating`]), records it in `state.db` so a restart while
//! the server is down names them the same way, and shows it wherever it says
//! what it is connected to (`status`, the session-start orientation).
//!
//! # First join
//!
//! An agent with no local cursor on a committee (never joined on this
//! machine, or reset) starts at that agent's `delivered` cursor on the
//! server when that is set and still inside the log, else at the
//! committee's head. A fresh bridge does not replay history; `catch_up`
//! reads it. A machine that already has a cursor resumes from it.
//!
//! # Log epochs
//!
//! Seqs mean something only within one epoch of the server's log. The
//! daemon records the epoch it last saw in `state.db` and compares it with
//! `/v1/me` at start and with the epoch header of every stream it opens and
//! every page it reads. A different epoch, or a 409 `cursor_ahead` (the same
//! fact seen from the server's side), means the log was reset: the cursors
//! and queue rows of the old log are dropped with a warning and the affected
//! committees re-join from their new heads.
//!
//! # Pushes
//!
//! Pushes to one session's inbox are at least [`Timing::push_spacing`]
//! apart, including the parts of a backlog too big for one message. A batch
//! the inbox does not take is held for the session's hooks and retried
//! through the inbox with backoff ([`Timing::retry_base`] doubling to
//! [`Timing::retry_max`], [`INBOX_RETRIES`] times); while it is held, nothing
//! newer is pushed to that session ahead of it. A Codex session is pushed
//! the same way through its app-server instead of an inbox
//! ([`crate::delivery::codex`], `[codex]` in `senate.toml`), with the same
//! spacing, hold and retries. Its hooks never carry items: what waits for a
//! Codex session when it registers, or is held after a failed push, is
//! pushed through the app-server once its hook has run.
//!
//! # Task threads
//!
//! A root that [`needs_triage`] waits for the Speaker's verdict, at most
//! [`Timing::triage_wait`], and is then pushed with the verdict in its header
//! (`unsure` when none came). It waits only on a Senate whose server says a
//! Speaker judges roots there ([`Me::triaged`](senate_core::api::Me::triaged));
//! elsewhere no verdict will ever come, and it is pushed at once. While one waits, the rest of its session's
//! push waits with it, so a backlog stays one message in seq order. A
//! `needs_human` verdict cancels it; a status
//! with a `takeover` cancels everything of that thread not yet pushed, and
//! so does the Speaker's answer for one of this bridge's agents given while
//! it had taken over for it (`meta.takeover`, not provisional): the agent's
//! hand-back note lists it, so the question is not pushed again on return.
//! The session a root is first pushed to owns its thread, together with
//! its agent ([`TaskOwner`]): later events in the thread for that agent go
//! to that session while it is live.
//!
//! # Checkpoints
//!
//! Besides the `checkpoint` tool ([`crate::checkpoint`]), the daemon writes
//! one on its own — never pushing — when a bound session's worktree gets a
//! new commit (`git rev-parse HEAD`, polled every
//! [`Timing::checkpoint_poll`]), every [`Timing::checkpoint_interval`] while
//! a session stays open, and when a session ends. An automatic checkpoint is
//! skipped when the log's latest checkpoint of this agent already has the
//! same commit and files. Worktrees are tracked by (agent, root), so two
//! sessions of one agent in one worktree write one checkpoint, and two
//! agents in one worktree each write their own (from the same
//! `.senate/` files, which belong to the worktree).
//!
//! # The companion session
//!
//! With `[companion] enabled = true`, a directed question that no live
//! session of its agent can take — none bound to its committee, or none
//! seen for `idle_secs` — is given to a headless fork of that agent's
//! latest Claude Code session on the committee ([`crate::companion`]); the
//! item is in flight for
//! the companion meanwhile, delivered (`via = companion`) if it answers and
//! back to pending if not. Outside the per-session ladder, since there is
//! no session to deliver to; its outcome is
//! [`DeliveryOutcome::Spawned`].
//!
//! # Bounds
//!
//! With no session of an agent bound to a committee, at most
//! [`PENDING_CAP`] of that agent's items wait; older ones are dropped and
//! the next push says how many. Ended and stale sessions, and delivered
//! items, are forgotten after 7 days.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, bail};
use senate_core::api::{self, Heartbeat};
use senate_core::continuity::{DECISIONS_FILE, OPEN_FILE, PROGRESS_FILE};
use senate_core::{
    Address, BranchNaming, Brief, CheckpointFiles, CheckpointPayload, CheckpointTrigger,
    CommitteeId, Event, EventId, Kind, Locale, Name, NewEvent, PROPOSAL_DECISION, Refs,
    SenatorPath, Seq, SpeakerSeat, StatusPayload, TaskOwner, TriagePayload, TriageVerdict,
    is_provisional, meta, needs_triage,
};
use serde::Serialize;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, RwLock, RwLockReadGuard, broadcast, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::agent::{self, AgentSource, Seating};
use crate::approvals::Relay;
use crate::catch_up::{self, CatchUpItem};
use crate::checkpoint;
use crate::client::{self, ApiClient, ApiClientError, Decoded, RawPage};
use crate::companion::{self, CompanionConfig};

use crate::config::BridgeConfig;
use crate::delivery::codex::{CodexAppServerAdapter, CodexConfig};
use crate::delivery::{Batch, DeliveryOutcome, Ladder};
use crate::format;
use crate::ipc::{
    self, ApprovalsReply, ApproveReply, BoundReply, CatchUpReply, CheckpointReply, CommitteeStatus,
    HookEvent, HookReply, HookReport, InboxReply, IpcError, JoinReply, Pong, Posted, Reply,
    Request, SayKind, SearchReply, SessionSelector, StatusReply, ThreadReply, ThreadSeen,
    WatchEvent, codes,
};
use crate::notify::{News, Notifications, NotifyConfig};
use crate::paths::Paths;
use crate::routing;
use crate::session::{self, Runtime, Session, SessionState};
use crate::state::{ItemKey, ItemStatus, QueueItem, SessionUpsert, Store, StoreError};
use crate::stream::{self, CommitteeWorker};

mod front;
mod preside;

use front::Front;

/// Largest single inbox message, in chars.
pub const INBOX_MAX_CHARS: usize = 20_000;
/// Most events in one inbox message.
pub const MAX_BATCH_EVENTS: usize = 50;
/// Claude Code's cap on one hook's `additionalContext`, in chars.
pub const HOOK_CONTEXT_MAX: usize = 10_000;
/// Most items kept for a committee no session is bound to.
pub const PENDING_CAP: usize = 200;
/// Inbox retries of a held batch before it waits for the session's hooks
/// alone (until a hook or a successful push resets the count).
pub const INBOX_RETRIES: u32 = 6;
/// Ended and stale sessions, and delivered items, are forgotten after this.
const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);
/// Bound on the gallop-and-bisect search for a committee's head.
const HEAD_PROBES: usize = 128;
/// Bound on fetching the brief while a SessionStart hook waits (the hook
/// gives the whole request about two seconds).
const HOOK_BRIEF_TIMEOUT: Duration = Duration::from_secs(1);
/// Hook context kept for the backlog when the brief would crowd it out.
const HOOK_BACKLOG_RESERVE: usize = 2_000;
/// Bound on one push into a Codex thread: connect, read, start or steer.
const CODEX_PUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// Events a `watch` connection may fall behind by before it is told it
/// lagged.
const WATCH_CAPACITY: usize = 256;
/// Events remembered as announced to the person.
const ANNOUNCED_CAP: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Heartbeat (and cursor report) period per committee.
    pub heartbeat: Duration,
    /// Quiet time that closes a batch.
    pub batch_window: Duration,
    /// Longest a batch waits for quiet.
    pub batch_max_wait: Duration,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    /// No SSE data (including pings) for this long means the stream is dead.
    pub stall_timeout: Duration,
    /// Delay before reporting a moved cursor.
    pub cursor_debounce: Duration,
    /// Session liveness sweep period.
    pub sweep: Duration,
    /// Bound on connecting to / writing an inbox socket.
    pub inbox_timeout: Duration,
    /// Least time between two pushes into one session's inbox.
    pub push_spacing: Duration,
    /// First wait before retrying a batch the inbox did not take; doubles
    /// per failure up to `retry_max`.
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// Longest a root that needs triage waits for the Speaker's verdict
    /// before it is pushed as `unsure` ([`senate_core::task`]).
    pub triage_wait: Duration,
    /// How often each live bound session's `HEAD` is compared with the last
    /// one seen (a new commit is a checkpoint).
    pub checkpoint_poll: Duration,
    /// Time between automatic checkpoints of an open session.
    pub checkpoint_interval: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            heartbeat: Duration::from_secs(60),
            batch_window: Duration::from_secs(1),
            batch_max_wait: Duration::from_secs(5),
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            stall_timeout: Duration::from_secs(3 * api::SSE_PING_SECS),
            cursor_debounce: Duration::from_secs(2),
            sweep: Duration::from_secs(30),
            inbox_timeout: Duration::from_secs(3),
            push_spacing: Duration::from_secs(2),
            retry_base: Duration::from_secs(5),
            retry_max: Duration::from_secs(60),
            triage_wait: Duration::from_secs(senate_core::task::TRIAGE_WAIT_SECS),
            checkpoint_poll: Duration::from_secs(60),
            checkpoint_interval: Duration::from_secs(2 * 3600),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub paths: Paths,
    pub config: BridgeConfig,
    pub timing: Timing,
    /// How to post desktop notifications for what is addressed to the
    /// person ([`crate::notify`]); `None` posts none. `[notify] desktop =
    /// false` in `senate.toml` turns it off too.
    pub desktop: Option<crate::notify::Desktop>,
}

/// A connected client that shows notifications itself: while one is, the
/// daemon posts no desktop notification.
pub(crate) struct AppClient(Arc<std::sync::atomic::AtomicUsize>);

impl AppClient {
    pub(crate) fn enter(count: &Arc<std::sync::atomic::AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count.clone())
    }
}

impl Drop for AppClient {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One event off an agent's stream on a committee.
pub(crate) enum Incoming {
    Event(Box<Event>),
    /// An event this build cannot decode, known only by its seq.
    Undecodable {
        seq: Seq,
        error: String,
    },
}

impl Incoming {
    fn seq(&self) -> Seq {
        match self {
            Self::Event(e) => e.seq,
            Self::Undecodable { seq, .. } => *seq,
        }
    }
}

/// Why a stream must drop its cursor(s) and reconnect.
pub(crate) enum Resync {
    /// The server's log has a new epoch: every committee is affected.
    Epoch(String),
    /// The server says this committee's cursor is beyond its log.
    CursorAhead,
    /// Another worker resynced since this connection began.
    Moved,
}

impl fmt::Display for Resync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Epoch(e) => write!(f, "the server's log has a new epoch ({e})"),
            Self::CursorAhead => f.write_str("the server says the cursor is beyond its log"),
            Self::Moved => f.write_str("the log was resynced"),
        }
    }
}

/// Why intake stopped.
pub(crate) enum Intake {
    Resync(Resync),
    Failed(anyhow::Error),
}

impl From<StoreError> for Intake {
    fn from(e: StoreError) -> Self {
        Self::Failed(e.into())
    }
}

enum EpochCheck {
    Same,
    Changed(String),
}

/// Inbox pacing and retries for one session.
#[derive(Debug, Default)]
struct PushState {
    last_attempt: Option<Instant>,
    /// Consecutive batches the inbox did not take.
    failures: u32,
    retry_at: Option<Instant>,
}

/// Where a pending item goes now.
enum Route<'a> {
    To(&'a Session),
    /// Held for a triage verdict until `until`; it will go to `to`. The
    /// batcher holds the rest of that session's push too, so a backlog stays
    /// one message in seq order.
    Wait {
        until: Instant,
        to: Option<&'a Session>,
    },
    Nowhere,
}

enum Hold {
    Ready,
    Until(Instant),
    /// Retries are used up; items wait for the session's hooks.
    Indefinitely,
}

/// One agent's stream on one committee: the unit everything the bridge
/// keeps per stream is keyed by.
type Seat = (SenatorPath, CommitteeId);
/// The branch a session is on now and what it is bound to there.
type Rebind = (Option<String>, Option<CommitteeId>);

pub(crate) struct Shared {
    pub(crate) config: BridgeConfig,
    /// Everything written for people and agents is in this language.
    pub(crate) locale: Locale,
    /// Set when `committee_from_branch` is on: the naming a branch name is
    /// read by to find a committee.
    convention: Option<BranchNaming>,
    /// Where this bridge's agents sit, as `/v1/me` last said.
    seating: Mutex<Seating>,
    /// Whether a Speaker judges roots on this Senate, as `/v1/me` last said;
    /// until it has said, as if it does.
    triaged: AtomicBool,
    /// Who holds this Senate's Speaker's seat, as `/v1/me` last said.
    speaker: Mutex<Option<SpeakerSeat>>,
    pub(crate) store: Store,
    pub(crate) api: ApiClient,
    pub(crate) ladder: Ladder,
    pub(crate) timing: Timing,
    streams: Mutex<BTreeMap<Seat, CommitteeWorker>>,
    /// Wakes the batcher.
    pub(crate) work: Notify,
    /// Wakes the supervisor.
    committees_changed: Notify,
    /// The resync generation. Intake holds it shared while it writes, so a
    /// resync (which holds it exclusively and bumps it) never interleaves
    /// with an event from the old log.
    generation: RwLock<u64>,
    /// Announces each new generation to the stream workers.
    resyncs: watch::Sender<u64>,
    pushes: Mutex<HashMap<String, PushState>>,
    /// Automatic checkpoints, per agent and worktree root.
    auto: Mutex<HashMap<(SenatorPath, PathBuf), AutoCheckpoint>>,
    /// The daemon's home, handed to companion sessions.
    home: PathBuf,
    /// Sessions whose committee changed because their worktree's branch
    /// did, not yet told: the branch they are on, and what they are bound
    /// to now.
    rebinds: Mutex<HashMap<String, Rebind>>,
    companion_config: CompanionConfig,
    /// The companion running now, if any (one at a time).
    companion: Mutex<Option<companion::Running>>,
    /// Questions a companion already had this run.
    companion_tried: Mutex<BTreeSet<ItemKey>>,
    /// Codex approval requests ([`crate::approvals`]).
    relay: Arc<Relay>,
    /// Everything `watch` connections are told.
    watch: broadcast::Sender<WatchEvent>,
    /// Desktop notifications for what is addressed to the person.
    notifications: Option<Notifications>,
    /// Events already announced to the person (every agent's stream carries
    /// each one): the newest [`ANNOUNCED_CAP`].
    announced_to_person: Mutex<BTreeSet<(CommitteeId, Seq)>>,
    /// Agents whose work on a committee is reassigned to one of mine
    /// ([`routing::Covering`]).
    covering: Mutex<routing::Covering>,
    /// This Senate's place among those the machine serves.
    index: usize,
    /// Live Codex sessions of every Senate, for the shared approvals relay.
    codex_sessions: Arc<Mutex<BTreeMap<usize, Vec<String>>>>,
    /// Docket items each presiding session has been told of.
    announced: Mutex<HashMap<String, BTreeSet<(CommitteeId, Seq)>>>,
}

/// What the automatic checkpoints know of one worktree.
#[derive(Debug)]
struct AutoCheckpoint {
    /// `HEAD` when last polled.
    head: String,
    /// The last automatic checkpoint considered (or when the worktree was
    /// first seen).
    last: Instant,
}

/// A running daemon. Dropping it does not stop it; call [`DaemonHandle::shutdown`].
pub struct DaemonHandle {
    front: Arc<Front>,
    shutdown: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    socket: PathBuf,
}

impl DaemonHandle {
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The state database of the first Senate served (tests and
    /// diagnostics).
    pub fn store(&self) -> &Store {
        &self.front.senates()[0].store
    }

    /// The state database of the Senate presided over by `presider`
    /// (`None`: the one where the person's agents sit directly).
    pub fn store_of(&self, presider: Option<&SenatorPath>) -> Option<&Store> {
        self.front
            .senates()
            .iter()
            .find(|s| s.seating().presider() == presider)
            .map(|s| &s.store)
    }

    /// Stop every task, report final cursors, remove the socket.
    pub async fn shutdown(self) {
        // An error means every receiver is already gone, which is the goal.
        let _ = self.shutdown.send(true);
        self.front.relay().stop_all();
        for task in self.tasks {
            if let Err(e) = task.await {
                tracing::warn!("daemon task ended abnormally: {e}");
            }
        }
        if let Err(e) = std::fs::remove_file(&self.socket) {
            tracing::debug!("removing {}: {e}", self.socket.display());
        }
    }
}

/// Run until SIGINT or SIGTERM.
pub async fn run(options: DaemonOptions) -> anyhow::Result<()> {
    let handle = start(options).await?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        r = tokio::signal::ctrl_c() => r?,
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
    handle.shutdown().await;
    Ok(())
}

pub async fn start(options: DaemonOptions) -> anyhow::Result<DaemonHandle> {
    let DaemonOptions {
        paths,
        config,
        timing,
        desktop,
    } = options;
    config.validate().map_err(anyhow::Error::msg)?;
    paths.check_socket(config.locale)?;
    paths
        .ensure_home()
        .with_context(|| format!("creating {}", paths.home().display()))?;

    let socket = paths.socket();
    if std::fs::symlink_metadata(&socket).is_ok() {
        if UnixStream::connect(&socket).await.is_ok() {
            bail!("a bridge daemon is already running on {}", socket.display());
        }
        std::fs::remove_file(&socket)
            .with_context(|| format!("removing stale {}", socket.display()))?;
    }

    let companion_config = CompanionConfig::load(&paths.config()).map_err(anyhow::Error::msg)?;
    if companion_config.enabled {
        tracing::info!(
            bin = %companion_config.claude_bin.display(),
            "companion sessions are on: unanswerable questions go to a fork of the last session"
        );
    }
    let codex = CodexConfig::load(&paths.config()).map_err(anyhow::Error::msg)?;
    let codex = CodexAppServerAdapter {
        transport: codex.transport().map_err(anyhow::Error::msg)?,
        timeout: CODEX_PUSH_TIMEOUT,
    };
    let (watch, _) = broadcast::channel(WATCH_CAPACITY);
    let app_clients = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let notifications = match desktop {
        Some(d)
            if NotifyConfig::load(&paths.config())
                .map_err(anyhow::Error::msg)?
                .desktop =>
        {
            Some(Notifications::spawn(
                d.notifier,
                d.window,
                app_clients.clone(),
                config.locale,
            ))
        }
        _ => None,
    };
    let relay = Arc::new(Relay::new(
        codex.transport.clone(),
        (timing.backoff_base, timing.backoff_max),
        watch.clone(),
    ));
    tracing::debug!(transport = %codex.transport, "Codex sessions are pushed through the app-server");
    let common = Common {
        timing,
        codex,
        relay: relay.clone(),
        watch: watch.clone(),
        notifications,
        companion_config,
        home: paths.home().to_path_buf(),
        codex_sessions: Arc::new(Mutex::new(BTreeMap::new())),
    };
    let links = config.links();
    let several = links.len() > 1;
    let mut senates = Vec::with_capacity(links.len());
    for (index, link) in links.iter().enumerate() {
        let db = match (&link.presider, several) {
            (Some(presider), true) => paths.state_db_for(&presider.to_string()),
            _ => paths.state_db(),
        };
        let fixed = link.presider.as_ref().map(|p| p.under(&config.person));
        let shared = Shared::open(
            config.for_link(link),
            &db,
            several.then_some(fixed),
            index,
            &common,
        )?;
        verify_person(&shared).await?;
        senates.push(shared);
    }
    let front = Arc::new(Front::new(senates, relay, watch, app_clients, several));

    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;

    let (shutdown, rx) = watch::channel(false);
    let mut tasks = vec![tokio::spawn(serve_ipc(front.clone(), listener, rx.clone()))];
    for shared in front.senates() {
        shared.sweep_once();
        tasks.push(tokio::spawn(supervise(shared.clone(), rx.clone())));
        tasks.push(tokio::spawn(batcher(shared.clone(), rx.clone())));
        tasks.push(tokio::spawn(sweeper(shared.clone(), rx.clone())));
        tasks.push(tokio::spawn(checkpointer(shared.clone(), rx.clone())));
        shared.work.notify_one();
        tracing::info!(
            person = %shared.config.person,
            senate = %shared.senate_label(),
            server = shared.api.base_url(),
            socket = %socket.display(),
            "bridge daemon serving a Senate"
        );
    }
    Ok(DaemonHandle {
        front,
        shutdown,
        tasks,
        socket,
    })
}

/// What every Senate's half of the daemon shares: one Codex transport and
/// approvals relay, one watch channel, one companion configuration.
struct Common {
    timing: Timing,
    codex: CodexAppServerAdapter,
    relay: Arc<Relay>,
    watch: broadcast::Sender<WatchEvent>,
    notifications: Option<Notifications>,
    companion_config: CompanionConfig,
    home: PathBuf,
    /// Live Codex sessions per Senate, so the one relay watches all of them.
    codex_sessions: Arc<Mutex<BTreeMap<usize, Vec<String>>>>,
}

impl Shared {
    /// One Senate's half of the daemon, its state in `db`. `fixed` is where
    /// its agents sit when the machine serves several Senates (the
    /// configuration says, before `/v1/me` does); `None` with one Senate,
    /// which learns it from `/v1/me` and the state it recorded.
    fn open(
        config: BridgeConfig,
        db: &Path,
        fixed: Option<Option<SenatorPath>>,
        index: usize,
        common: &Common,
    ) -> anyhow::Result<Arc<Self>> {
        let store = Store::open(db).with_context(|| format!("opening {}", db.display()))?;
        let reset = store.reset_inflight()?;
        if reset > 0 {
            tracing::info!(
                reset,
                "re-queued items that were in flight at the last exit"
            );
        }
        let api = ApiClient::new(&config.server_url, &config.token)?;
        // Until `/v1/me` answers, agents sit where the configuration or the
        // last answer said they do.
        let presider = match fixed {
            Some(presider) => presider,
            None => match store.seating(api.base_url()) {
                Ok(known) => known.flatten(),
                Err(e) => {
                    tracing::warn!("reading the recorded Senate: {e}");
                    None
                }
            },
        };
        let seating = Seating::new(config.person.clone(), presider);
        let (resyncs, _) = watch::channel(0);
        Ok(Arc::new(Shared {
            ladder: Ladder::new_default(common.timing.inbox_timeout, common.codex.clone()),
            locale: config.locale,
            convention: config.branch_convention(),
            seating: Mutex::new(seating),
            triaged: AtomicBool::new(true),
            speaker: Mutex::new(None),
            config,
            store,
            api,
            timing: common.timing,
            streams: Mutex::new(BTreeMap::new()),
            work: Notify::new(),
            committees_changed: Notify::new(),
            generation: RwLock::new(0),
            resyncs,
            pushes: Mutex::new(HashMap::new()),
            auto: Mutex::new(HashMap::new()),
            home: common.home.clone(),
            rebinds: Mutex::new(HashMap::new()),
            companion_config: common.companion_config.clone(),
            companion: Mutex::new(None),
            companion_tried: Mutex::new(BTreeSet::new()),
            relay: common.relay.clone(),
            watch: common.watch.clone(),
            notifications: common.notifications.clone(),
            announced_to_person: Mutex::new(BTreeSet::new()),
            covering: Mutex::new(routing::Covering::new()),
            index,
            codex_sessions: common.codex_sessions.clone(),
            announced: Mutex::new(HashMap::new()),
        }))
    }
}

/// Refuse to start on a token that names someone else; tolerate an
/// unreachable server (the streams retry). The log epoch `/v1/me` names is
/// checked, and the Senate it serves recorded, before any stream opens.
async fn verify_person(shared: &Shared) -> anyhow::Result<()> {
    let config = &shared.config;
    match tokio::time::timeout(Duration::from_secs(5), shared.api.me()).await {
        Ok(Ok(me)) if me.person != config.person => bail!(
            "senate.toml says person {:?}, but the server says the token belongs to {:?}",
            config.person.as_str(),
            me.person.as_str()
        ),
        Ok(Ok(me)) => {
            shared.note_epoch(Some(&me.epoch)).await;
            shared.note_triaged(me.triaged);
            shared.note_seating(me.presider);
            shared.note_speaker(me.speaker);
            Ok(())
        }
        Ok(Err(e)) if e.status() == Some(401) => bail!("the server rejected the token: {e}"),
        Ok(Err(e)) => {
            tracing::warn!("could not confirm the person with the server yet: {e}");
            Ok(())
        }
        Err(_) => {
            tracing::warn!("the server did not answer /v1/me in time; continuing");
            Ok(())
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How far `seen` says the person had seen `root`; nothing yet is `0`.
fn seen_in(seen: &[(Seq, Seq)], root: Seq) -> Seq {
    seen.iter()
        .find(|(r, _)| *r == root)
        .map_or(Seq::ZERO, |(_, s)| *s)
}

fn internal(e: StoreError) -> IpcError {
    IpcError::internal(e)
}

fn server_error(e: ApiClientError) -> IpcError {
    IpcError::new(codes::SERVER, e.to_string())
}

fn wrap<T: Serialize>(result: Result<T, IpcError>) -> Reply {
    match result {
        Ok(v) => Reply::ok(&v),
        Err(e) => Reply::err(e),
    }
}

/// Which Senate, in words: the top one, or the one a senator presides over.
pub fn senate_words(presider: Option<&SenatorPath>, locale: Locale) -> String {
    match (presider, locale) {
        (None, Locale::Zh) => "顶层 Senate".to_string(),
        (None, Locale::En) => "the top Senate".to_string(),
        (Some(p), Locale::Zh) => format!("由 {} 主持的 Senate", Address::senator(p).in_text()),
        (Some(p), Locale::En) => format!(
            "the Senate presided over by {}",
            Address::senator(p).in_text()
        ),
    }
}

/// The line a starting session reads first: where it is joined, as whom,
/// and the tools it has.
fn orientation(
    committee: &CommitteeId,
    agent: &SenatorPath,
    presider: Option<&SenatorPath>,
    chat: bool,
    locale: Locale,
) -> String {
    let me = Address::senator(agent).in_text();
    let senate = senate_words(presider, locale);
    let reach_zh = if chat {
        "队友的 agent、Speaker 和委员会聊天群里的人"
    } else {
        "队友的 agent 和 Speaker "
    };
    let reach_en = if chat {
        "Teammates' agents, the Speaker, and people in the committee's chat"
    } else {
        "Teammates' agents and the Speaker"
    };
    match locale {
        Locale::Zh => format!(
            "[senate] 这个会话已加入 {senate} 的委员会 `{committee}`，身份是 {me}。{reach_zh}会以 `[senate · …]` 消息在这里找到你。工具：\
             mcp__senate__ask（问队友的 agent），mcp__senate__answer（在 thread 里回复），\
             mcp__senate__catch_up（读你错过的），mcp__senate__search（搜索整个日志），\
             mcp__senate__post（给团队留言），mcp__senate__propose_decision（提议一个决定，由人确认），\
             mcp__senate__checkpoint（留交接记录：保持 .senate/PROGRESS.md、DECISIONS.md 和 \
             OPEN.md 是最新的）。你的用户也能在终端里用 `senate ask`、`senate tell`、\
             `senate reply` 以本人的身份对你们说话。你回答或转给你的用户的内容，会以桌面通知、`senate inbox` \
             和实时的 `senate watch` 送到他们面前；转给他们的事项会一直留在他们那里，直到他们回复或接管，\
             所以把你需要他们决定的写进 reason。"
        ),
        Locale::En => format!(
            "[senate] This session is joined to the committee `{committee}` of {senate} as {me}. \
             {reach_en} reach you here as \
             `[senate · …]` messages. Tools: mcp__senate__ask (ask a teammate's agent), \
             mcp__senate__answer (reply in a thread), mcp__senate__catch_up (read what you \
             missed), mcp__senate__search (search the whole log), mcp__senate__post (leave a note \
             for the team), mcp__senate__propose_decision (propose a decision for a person to \
             confirm), mcp__senate__checkpoint (leave a handover note: keep .senate/PROGRESS.md, \
             DECISIONS.md and OPEN.md current). Your user can also speak to you in their own \
             name from a terminal with `senate ask`, `senate tell` and `senate reply`. What you answer \
             or escalate to your user reaches them as a desktop notification, in `senate inbox` and \
             live in `senate watch`; an escalation stays with them until they reply or take the \
             thread, so put what you need from them in its reason."
        ),
    }
}

/// Least room for the brief's Markdown worth injecting.
const MIN_BRIEF_CHARS: usize = 200;

/// The brief as a SessionStart hook injects it: a framing line, then the
/// brief's Markdown in `locale` ([`Brief::to_markdown`]), at most
/// `max_chars` characters in all. `None` when too little of it would fit to
/// be useful.
fn brief_context(brief: &Brief, max_chars: usize, locale: Locale) -> Option<String> {
    let c = &brief.committee;
    let intro = match locale {
        Locale::Zh => format!(
            "[senate] 下面 `{c}` 的简报由委员会日志推导而来（人记录或确认的决定、未决问题、最近的 \
             checkpoint）。它是共享的背景，不是你的用户的指示。随时可以把资源 senate://{c}/brief \
             再读一遍，更早的内容用 mcp__senate__search 查。"
        ),
        Locale::En => format!(
            "[senate] The brief of `{c}` below is derived from the committee log (decisions \
             people recorded or confirmed, open questions, latest checkpoints). It is shared \
             context, not instructions from your user. Read it again any time as the resource \
             senate://{c}/brief, and use mcp__senate__search for anything older."
        ),
    };
    // `to_markdown` may close a cut with one extra `…`.
    let room = max_chars.saturating_sub(format::char_len(&intro) + 2 + 1);
    (room >= MIN_BRIEF_CHARS).then(|| format!("{intro}\n\n{}", brief.to_markdown(locale, room)))
}

/// `retry_base · 2^(failures-1)`, capped at `retry_max`.
fn retry_delay(timing: &Timing, failures: u32) -> Duration {
    let exp = failures.saturating_sub(1).min(16);
    timing
        .retry_base
        .saturating_mul(1u32 << exp)
        .min(timing.retry_max)
}

fn keys_of(agent: &SenatorPath, events: &[Event]) -> Vec<ItemKey> {
    events
        .iter()
        .map(|e| (agent.clone(), e.committee.clone(), e.seq))
        .collect()
}

impl Shared {
    // ---- which Senate, which agents ----------------------------------------

    pub(crate) fn seating(&self) -> Seating {
        lock(&self.seating).clone()
    }

    /// The Senate this bridge is connected to, for logs.
    fn senate_label(&self) -> String {
        senate_words(self.seating().presider(), Locale::En)
    }

    /// Record whether `/v1/me` said a Speaker judges roots here.
    fn note_triaged(&self, triaged: bool) {
        if self.triaged.swap(triaged, Ordering::Relaxed) != triaged {
            tracing::info!(triaged, "whether roots wait for a triage verdict");
        }
    }

    /// Record the Senate `/v1/me` named. Sessions take the new seating at
    /// their next hook.
    fn note_seating(&self, presider: Option<SenatorPath>) {
        let seating = Seating::new(self.config.person.clone(), presider);
        if let Err(e) = self
            .store
            .set_seating(self.api.base_url(), seating.presider())
        {
            tracing::warn!("recording the Senate: {e}");
        }
        let mut current = lock(&self.seating);
        if *current != seating {
            tracing::info!(senate = %senate_words(seating.presider(), Locale::En), "connected");
            *current = seating;
        }
    }

    /// Record who holds this Senate's Speaker's seat, as `/v1/me` named it.
    pub(crate) fn note_speaker(&self, speaker: Option<SpeakerSeat>) {
        let mut current = lock(&self.speaker);
        if *current != speaker {
            tracing::info!(speaker = ?speaker, senate = %self.senate_label(), "the Speaker's seat");
            *current = speaker;
        }
    }

    /// The Speaker's seat of this Senate, as last heard.
    pub(crate) fn speaker_seat(&self) -> Option<SpeakerSeat> {
        lock(&self.speaker).clone()
    }

    /// Whether `agent`, one of this machine's person's agents, holds this
    /// Senate's Speaker's seat.
    pub(crate) fn presided_by(&self, agent: &SenatorPath) -> bool {
        agent.owner() == &self.config.person
            && self
                .speaker_seat()
                .is_some_and(|seat| seat.senator() == Some(agent))
    }

    /// Whether `event` is put to the Speaker of this Senate and `agent`
    /// holds its seat: pushed to the presiding session like a message for
    /// the agent itself. Not the Speaker's own words.
    pub(crate) fn for_speaker(&self, agent: &SenatorPath, event: &Event) -> bool {
        event.from != Address::Speaker
            && event.is_directed_to(&Address::Speaker)
            && self.presided_by(agent)
    }

    /// Whether `agent` holds this Senate's seat without sitting in it: the
    /// presider of a nested Senate, whose session sits in the Senate above.
    /// It speaks here only as `speaker`.
    pub(crate) fn presides_from_above(&self, agent: &SenatorPath) -> bool {
        self.presided_by(agent) && !self.seating().owns(agent)
    }

    /// `senator::<person>`: the person this bridge holds the token of.
    fn person(&self) -> Address {
        Address::senior(&self.config.person)
    }

    /// Every agent this machine has a session of on record, any state.
    fn served_agents(&self) -> Result<BTreeSet<SenatorPath>, StoreError> {
        Ok(self
            .store
            .all_sessions()?
            .into_iter()
            .map(|s| s.agent)
            .collect())
    }

    /// The agents this bridge holds a stream for on `committee`.
    fn agents_on(&self, committee: &CommitteeId) -> Vec<SenatorPath> {
        lock(&self.streams)
            .keys()
            .filter(|(_, c)| c == committee)
            .map(|(a, _)| a.clone())
            .collect()
    }

    /// The agent a request with no session acts as on `committee`: the one
    /// it names, else the only agent this bridge holds a stream for there,
    /// else the only agent this machine has a session of on record. With
    /// several to choose from it is refused rather than guessed.
    fn agent_without_session(
        &self,
        committee: &CommitteeId,
        named: Option<&Name>,
    ) -> Result<SenatorPath, IpcError> {
        if let Some(name) = named {
            return Ok(self.seating().agent(name));
        }
        let mut agents = self.agents_on(committee);
        if agents.is_empty() {
            agents = self
                .served_agents()
                .map_err(internal)?
                .into_iter()
                .collect();
        }
        match agents.as_slice() {
            [one] => Ok(one.clone()),
            [] => Err(IpcError::bad_request(format!(
                "no agent of this bridge is on `{committee}`; name one with `agent`"
            ))),
            several => Err(IpcError::bad_request(format!(
                "several agents of this bridge are on `{committee}` ({}); name one with `agent`",
                several
                    .iter()
                    .map(|a| Address::senator(a).in_text())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    // ---- supervision -----------------------------------------------------

    /// Each live bound session's agent on its committee; with companions on,
    /// every session still on record (ended ones are kept for 7 days), so a
    /// question can reach a companion after the developer closed their last
    /// session. Plus each configured committee for every agent this machine
    /// has a session of on record.
    fn desired_seats(&self) -> Result<BTreeSet<Seat>, StoreError> {
        let mut set: BTreeSet<Seat> = BTreeSet::new();
        let sessions = if self.companion_config.enabled {
            self.store.all_sessions()?
        } else {
            self.store.live_sessions()?
        };
        for s in sessions {
            if let Some(f) = s.committee() {
                set.insert((s.agent.clone(), f.clone()));
            }
        }
        if !self.config.committees.is_empty() {
            for agent in self.served_agents()? {
                for committee in &self.config.committees {
                    set.insert((agent.clone(), committee.clone()));
                }
            }
        }
        Ok(set)
    }

    fn reconcile(self: &Arc<Self>) {
        let desired = match self.desired_seats() {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("cannot read sessions: {e}");
                return;
            }
        };
        let mut streams = lock(&self.streams);
        streams.retain(|(agent, committee), worker| {
            let keep = desired.contains(&(agent.clone(), committee.clone()));
            if !keep {
                tracing::info!(%agent, %committee, "leaving committee (no bound session)");
                worker.stop();
            }
            keep
        });
        for seat in desired {
            if let std::collections::btree_map::Entry::Vacant(slot) = streams.entry(seat) {
                let (agent, committee) = slot.key().clone();
                tracing::info!(%agent, %committee, "joining committee");
                slot.insert(stream::spawn(self.clone(), agent, committee));
            }
        }
    }

    fn stop_all(&self) -> Vec<JoinHandle<()>> {
        let mut streams = lock(&self.streams);
        let workers = std::mem::take(&mut *streams);
        workers
            .into_values()
            .flat_map(|w| {
                w.stop();
                w.into_tasks()
            })
            .collect()
    }

    /// Live sessions, after marking those whose Claude process is gone as stale.
    fn checked_live_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let mut live = Vec::new();
        let mut changed = false;
        for s in self.store.live_sessions()? {
            if s.claude_pid.is_some_and(|pid| !session::pid_alive(pid)) {
                tracing::info!(session = %s.session_id, "session process is gone; marking stale");
                changed |= self
                    .store
                    .set_session_state(&s.session_id, SessionState::Stale)?;
                continue;
            }
            live.push(s);
        }
        if changed {
            self.committees_changed.notify_one();
        }
        Ok(live)
    }

    fn sweep_once(&self) {
        if let Err(e) = self.checked_live_sessions() {
            tracing::warn!("session sweep failed: {e}");
        }
        if let Err(e) = self.store.prune(RETENTION) {
            tracing::warn!("pruning state failed: {e}");
        }
        self.committees_changed.notify_one();
    }

    // ---- log epochs and resyncs -------------------------------------------

    pub(crate) fn resyncs(&self) -> watch::Receiver<u64> {
        self.resyncs.subscribe()
    }

    /// Compare an epoch the server named with the recorded one. The first
    /// epoch seen is recorded, unless cursors from another server are on
    /// file (then it counts as a change). `None` (a server from before
    /// epochs) tells nothing.
    fn epoch_check(&self, observed: Option<&str>) -> Result<EpochCheck, StoreError> {
        let Some(observed) = observed.filter(|e| !e.is_empty()) else {
            return Ok(EpochCheck::Same);
        };
        let server = self.api.base_url();
        match self.store.epoch(server)? {
            Some(known) if known == observed => Ok(EpochCheck::Same),
            Some(_) => Ok(EpochCheck::Changed(observed.to_string())),
            None if self.store.has_other_epochs(server)? => {
                Ok(EpochCheck::Changed(observed.to_string()))
            }
            None => {
                self.store.set_epoch(server, observed)?;
                tracing::info!(epoch = observed, "recorded the server's log epoch");
                Ok(EpochCheck::Same)
            }
        }
    }

    /// For intake: a changed epoch stops it.
    pub(crate) fn check_epoch(&self, observed: Option<&str>) -> Result<(), Intake> {
        match self.epoch_check(observed)? {
            EpochCheck::Same => Ok(()),
            EpochCheck::Changed(new) => Err(Intake::Resync(Resync::Epoch(new))),
        }
    }

    /// For everything else: resync now when the epoch changed.
    async fn note_epoch(&self, observed: Option<&str>) {
        match self.epoch_check(observed) {
            Ok(EpochCheck::Same) => {}
            Ok(EpochCheck::Changed(new)) => self.resync_epoch(&new).await,
            Err(e) => tracing::warn!("reading the recorded log epoch: {e}"),
        }
    }

    /// What an API error means for intake: a new epoch or `cursor_ahead`
    /// is a resync, anything else a lost connection.
    pub(crate) fn api_failure(&self, e: ApiClientError) -> Intake {
        if let Err(stop) = self.check_epoch(e.epoch()) {
            return stop;
        }
        if e.is_cursor_ahead() {
            Intake::Resync(Resync::CursorAhead)
        } else {
            Intake::Failed(e.into())
        }
    }

    /// Hold off resyncs while intake writes; `Moved` when one happened
    /// since `generation`.
    async fn intake_guard(&self, generation: u64) -> Result<RwLockReadGuard<'_, u64>, Intake> {
        let guard = self.generation.read().await;
        if *guard == generation {
            Ok(guard)
        } else {
            Err(Intake::Resync(Resync::Moved))
        }
    }

    /// Carry out what a stream worker found, `seen` being the generation
    /// its connection began in.
    pub(crate) async fn resync(
        &self,
        agent: &SenatorPath,
        committee: &CommitteeId,
        resync: Resync,
        seen: u64,
    ) {
        match resync {
            Resync::Epoch(new) => self.resync_epoch(&new).await,
            Resync::CursorAhead => self.resync_committee(agent, committee, seen).await,
            Resync::Moved => {}
        }
    }

    /// The server's log was reset: drop every cursor and queue row, record
    /// the new epoch, and make every stream reconnect (and so re-seed).
    async fn resync_epoch(&self, new: &str) {
        let mut generation = self.generation.write().await;
        let server = self.api.base_url();
        let old = match self.store.epoch(server) {
            Ok(old) => old,
            Err(e) => {
                tracing::warn!("reading the recorded log epoch: {e}");
                return;
            }
        };
        if old.as_deref() == Some(new) {
            return; // Another worker got here first.
        }
        match self.store.reset_for_epoch(server, new) {
            Ok(summary) => {
                *generation += 1;
                self.resyncs.send_replace(*generation);
                for p in lock(&self.pushes).values_mut() {
                    p.failures = 0;
                    p.retry_at = None;
                }
                tracing::warn!(
                    old = old.as_deref().unwrap_or("(none recorded)"),
                    new,
                    committees = summary.committees,
                    dropped = summary.undelivered,
                    "the server's log was reset (its epoch changed): dropped every cursor and the \
                     undelivered items of the old log; each committee re-joins at its current head"
                );
            }
            Err(e) => tracing::warn!("resetting state for the new log epoch failed: {e}"),
        }
    }

    /// The server says `agent`'s cursor on `committee` is beyond its log:
    /// drop that agent's cursor and queue rows there so its stream re-seeds.
    /// Another agent's stream on the same committee finds the same and
    /// resets its own; until then its cursor is left alone, so a stream
    /// still connected never sees its cursor vanish under it.
    async fn resync_committee(&self, agent: &SenatorPath, committee: &CommitteeId, seen: u64) {
        let generation = self.generation.write().await;
        if *generation != seen {
            return; // A full resync already happened since.
        }
        match self.store.reset_committee(agent, committee) {
            Ok(dropped) => tracing::warn!(
                %agent,
                %committee,
                dropped,
                "the server says this bridge's cursor is beyond the committee's log \
                 (cursor_ahead), so the log was reset: dropped the committee's cursor and its \
                 undelivered items; re-joining at the current head"
            ),
            Err(e) => tracing::warn!(%committee, "resetting the committee failed: {e}"),
        }
    }

    // ---- first join ------------------------------------------------------

    /// Give `agent` a cursor on `committee` if it has none (first join, or
    /// after a reset).
    pub(crate) async fn ensure_seeded(
        &self,
        agent: &SenatorPath,
        committee: &CommitteeId,
        generation: u64,
    ) -> Result<(), Intake> {
        if self.store.has_cursor(agent, committee)? {
            return Ok(());
        }
        let (delivered, reported, from) = self.seed_point(agent, committee).await?;
        let _guard = self.intake_guard(generation).await?;
        if self
            .store
            .seed_cursor(agent, committee, delivered, reported)?
        {
            tracing::info!(
                %agent,
                %committee,
                cursor = %delivered,
                "first join: starting after seq {delivered} ({from}); earlier events wait for \
                 catch_up"
            );
        }
        Ok(())
    }

    /// `(delivered, reported, why)` for an agent joining a committee for the
    /// first time.
    async fn seed_point(
        &self,
        agent: &SenatorPath,
        committee: &CommitteeId,
    ) -> Result<(Seq, Seq, &'static str), Intake> {
        let server = self
            .api
            .cursor(committee, api::CURSOR_DELIVERED, Some(agent))
            .await
            .map_err(|e| self.api_failure(e))?;
        if server > Seq::ZERO {
            let before = Seq(server.0 - 1);
            if self.probe(committee, before).await? == Some(server) {
                return Ok((
                    server,
                    server,
                    "this agent's delivered cursor on the server",
                ));
            }
            tracing::warn!(
                %agent,
                %committee,
                cursor = %server,
                "this agent's delivered cursor on the server is beyond the log; ignoring it"
            );
        }
        Ok((
            self.head(committee).await?,
            Seq::ZERO,
            "the committee's head",
        ))
    }

    /// The seq of the first event after `after`, if any.
    async fn probe(&self, committee: &CommitteeId, after: Seq) -> Result<Option<Seq>, Intake> {
        let page = self.page(committee, after, 1).await?;
        Ok(page.and_then(|p| p.events.first().and_then(client::seq_of)))
    }

    /// One page, epoch-checked; `None` when the server answers
    /// `cursor_ahead` (nothing after `after`).
    async fn page(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Option<RawPage>, Intake> {
        match self.api.events(committee, after, Some(limit)).await {
            Ok(page) => {
                self.check_epoch(page.epoch.as_deref())?;
                Ok(Some(page))
            }
            Err(e) if e.is_cursor_ahead() => {
                self.check_epoch(e.epoch())?;
                Ok(None)
            }
            Err(e) => Err(self.api_failure(e)),
        }
    }

    /// The committee's last seq: one full page for a short log, else a
    /// gallop and bisection with one-event probes.
    async fn head(&self, committee: &CommitteeId) -> Result<Seq, Intake> {
        let Some(page) = self.page(committee, Seq::ZERO, api::MAX_PAGE).await? else {
            return Ok(Seq::ZERO);
        };
        let Some(mut lo) = page.events.iter().filter_map(client::seq_of).max() else {
            return Ok(Seq::ZERO);
        };
        if !page.has_more {
            return Ok(lo);
        }
        // `lo` holds an event; nothing comes after `hi`.
        let mut probes = 0;
        let mut step = lo.0.max(1);
        let mut hi = loop {
            let at = Seq(lo.0.saturating_add(step));
            probes += 1;
            match self.probe(committee, at).await? {
                Some(s) if probes < HEAD_PROBES => {
                    lo = s;
                    step = step.saturating_mul(2);
                }
                Some(s) => return Ok(s),
                None => break at,
            }
        };
        while hi > lo && probes < HEAD_PROBES {
            let mid = Seq(lo.0 + (hi.0 - lo.0) / 2);
            probes += 1;
            match self.probe(committee, mid).await? {
                Some(s) => lo = s,
                None => hi = mid,
            }
        }
        Ok(lo)
    }

    // ---- intake (called by stream workers) -------------------------------

    /// Take in one event from `agent`'s stream on `committee`: fill any gap
    /// before it from the events API, then record it (and enqueue it for
    /// the agent when directed to it). An undecodable event only moves the
    /// cursor past it.
    pub(crate) async fn take_in(
        &self,
        agent: &SenatorPath,
        committee: &CommitteeId,
        incoming: Incoming,
        generation: u64,
        dirty: &Notify,
    ) -> Result<(), Intake> {
        let _guard = self.intake_guard(generation).await?;
        if let Incoming::Event(e) = &incoming
            && &e.committee != committee
        {
            tracing::warn!(%agent, %committee, other = %e.committee, "stream carried another committee's event; ignored");
            return Ok(());
        }
        let seq = incoming.seq();
        let cursor = self.store.delivered_cursor(agent, committee)?;
        if seq <= cursor {
            return Ok(());
        }
        if seq > cursor.next() {
            tracing::info!(%agent, %committee, after = %cursor, next = %seq, "gap in stream; reading it from the log");
            self.fill_gap(agent, committee, cursor, seq, dirty).await?;
        }
        match incoming {
            Incoming::Event(e) => self.ingest_one(agent, &e, dirty)?,
            Incoming::Undecodable { seq, error } => {
                self.skip_undecodable(agent, committee, seq, &error, dirty)?;
            }
        }
        Ok(())
    }

    async fn fill_gap(
        &self,
        agent: &SenatorPath,
        committee: &CommitteeId,
        mut after: Seq,
        until: Seq,
        dirty: &Notify,
    ) -> Result<(), Intake> {
        loop {
            let Some(page) = self.page(committee, after, api::MAX_PAGE).await? else {
                // Our cursor is beyond the log, yet the stream sent `until`.
                return Err(Intake::Resync(Resync::CursorAhead));
            };
            let mut progressed = false;
            for value in page.events {
                let decoded = client::decode_event(value);
                let Some(seq) = decoded.seq() else {
                    tracing::warn!(%committee, after = %after, "skipped an event with no readable seq in a page");
                    continue;
                };
                if seq >= until {
                    return Ok(());
                }
                if seq <= after {
                    continue;
                }
                after = seq;
                progressed = true;
                match decoded {
                    Decoded::Event(e) if &e.committee == committee => {
                        self.ingest_one(agent, &e, dirty)?;
                    }
                    Decoded::Event(e) => {
                        let error = format!("it belongs to committee {}", e.committee);
                        self.skip_undecodable(agent, committee, seq, &error, dirty)?;
                    }
                    Decoded::Undecodable { error, .. } => {
                        self.skip_undecodable(agent, committee, seq, &error, dirty)?;
                    }
                }
            }
            if !page.has_more || !progressed {
                return Ok(());
            }
        }
    }

    /// Move the cursor past an event this build cannot read, so it is never
    /// fetched again.
    fn skip_undecodable(
        &self,
        agent: &SenatorPath,
        committee: &CommitteeId,
        seq: Seq,
        error: &str,
        dirty: &Notify,
    ) -> Result<(), StoreError> {
        if self.store.advance(agent, committee, seq)? {
            tracing::warn!(%agent, %committee, %seq, "skipped event seq {seq}: this bridge cannot decode it ({error})");
            dirty.notify_one();
        }
        Ok(())
    }

    /// Seed which agents' work on `committee` is `agent`'s from the
    /// server's presence (a stream opened). Best effort: a failure keeps
    /// what the stream folded so far.
    pub(crate) async fn seed_covering(&self, agent: &SenatorPath, committee: &CommitteeId) {
        match self.api.presence(committee).await {
            Ok(page) => lock(&self.covering).seed(agent, committee, &page.agents),
            Err(e) => tracing::debug!(%committee, "presence unavailable for reassigned work: {e}"),
        }
    }

    fn ingest_one(
        &self,
        agent: &SenatorPath,
        event: &Event,
        dirty: &Notify,
    ) -> Result<(), StoreError> {
        let directed = {
            let mut covering = lock(&self.covering);
            covering.observe(event, agent);
            routing::should_push_covering(event, agent, &covering)
        } || self.for_speaker(agent, event);
        if self.store.ingest(agent, event, directed)? {
            dirty.notify_one();
            self.announce_to_person(event);
            if matches!(
                event.kind,
                Kind::Question | Kind::Request | Kind::Answer | Kind::Status | Kind::Note
            ) {
                self.publish(WatchEvent::Inbox {
                    agent: agent.clone(),
                    committee: event.committee.clone(),
                    seq: event.seq,
                    kind: event.kind.clone(),
                });
            }
            if self.observe_task(agent, event)? {
                self.work.notify_one();
            }
            if directed {
                tracing::debug!(%agent, committee = %event.committee, seq = %event.seq, "directed event queued");
                self.cap_unbound(agent, &event.committee)?;
                self.work.notify_one();
            } else if event.is_directed_to(&Address::senator(agent))
                && routing::names_unknown_address(event)
            {
                tracing::warn!(
                    %agent,
                    committee = %event.committee,
                    seq = %event.seq,
                    "event names an address this bridge does not know; not pushed (catch_up shows it)"
                );
            }
        }
        Ok(())
    }

    /// Tell the person what is addressed to them: `watch` clients get a
    /// `for_person` line and, with no client showing notifications itself,
    /// the desktop gets one notification (coalesced). Each event once,
    /// whichever agent's stream carried it.
    fn announce_to_person(&self, event: &Event) {
        let Some(attention) = senate_core::attention(event, &self.config.person) else {
            return;
        };
        {
            let mut seen = lock(&self.announced_to_person);
            if !seen.insert((event.committee.clone(), event.seq)) {
                return;
            }
            while seen.len() > ANNOUNCED_CAP {
                seen.pop_first();
            }
        }
        if let Some(notifications) = &self.notifications {
            let by = crate::person::byline(&event.from, event.on_behalf_of.as_ref());
            let verb = crate::person::did(&event.kind, attention.escalated, self.locale);
            let line = format::one_line(attention.text.lines().next().unwrap_or_default().trim());
            let sep = self.locale.pick("：", ": ");
            notifications.push(News {
                committee: event.committee.clone(),
                line: if line.is_empty() {
                    format!("{by} {verb}")
                } else {
                    format!("{by} {verb}{sep}{line}")
                },
            });
        }
        self.publish(WatchEvent::ForPerson {
            news: Box::new(event.clone()),
            escalated: attention.escalated,
            text: attention.text,
        });
    }

    /// Triage verdicts and takeovers off `agent`'s stream (see the module
    /// docs); each agent's stream cancels that agent's own waiting items.
    /// Returns whether the delivery plan may have changed.
    fn observe_task(&self, agent: &SenatorPath, event: &Event) -> Result<bool, StoreError> {
        let mut changed = false;
        if event.from == Address::Speaker
            && let Some(triage) = TriagePayload::of(event)
        {
            let root = event.thread.unwrap_or(event.seq);
            self.store
                .record_verdict(&event.committee, root, triage.verdict)?;
            if triage.verdict == TriageVerdict::NeedsHuman {
                let parked =
                    self.store
                        .cancel(Some(agent), &event.committee, root, false, "parked")?;
                if !parked.is_empty() {
                    tracing::info!(
                        %agent,
                        committee = %event.committee,
                        %root,
                        "the Speaker parked this on a person; not pushed"
                    );
                }
            }
            changed = true;
        }
        if let Some(root) = event.thread
            && answered_for_me_in_takeover(event, agent)
        {
            let answered =
                self.store
                    .cancel(Some(agent), &event.committee, root, true, "answered")?;
            if !answered.is_empty() {
                tracing::info!(
                    committee = %event.committee,
                    %root,
                    count = answered.len(),
                    "the Speaker answered this for me while I was away; not pushed (the hand-back note lists it)"
                );
            }
            changed = true;
        }
        if let (Some(root), Some(status)) = (event.thread, StatusPayload::of(event))
            && let Some(who) = &status.takeover
        {
            let cancelled =
                self.store
                    .cancel(Some(agent), &event.committee, root, true, "takeover")?;
            if !cancelled.is_empty() {
                tracing::info!(
                    committee = %event.committee,
                    %root,
                    %who,
                    count = cancelled.len(),
                    "a person took the thread over; its waiting items are cancelled"
                );
            }
            changed = true;
        }
        Ok(changed)
    }

    /// Where `item` goes now, among `cands`, its agent's sessions (see the
    /// module docs).
    fn route<'a>(&self, item: &QueueItem, cands: &[&'a Session]) -> Result<Route<'a>, StoreError> {
        let event = &item.event;
        let target = self.recipient(&item.agent, event, cands)?;
        if needs_triage(event) && self.triaged.load(Ordering::Relaxed) {
            match self.store.verdict(&event.committee, event.seq)? {
                Some(TriageVerdict::NeedsHuman) => {
                    self.store.cancel(
                        Some(&item.agent),
                        &event.committee,
                        event.seq,
                        false,
                        "parked",
                    )?;
                    return Ok(Route::Nowhere);
                }
                Some(_) => {}
                None => {
                    let waited = (chrono::Utc::now() - item.enqueued_at)
                        .to_std()
                        .unwrap_or_default();
                    if waited < self.timing.triage_wait {
                        return Ok(Route::Wait {
                            until: Instant::now() + (self.timing.triage_wait - waited),
                            to: target,
                        });
                    }
                }
            }
        }
        Ok(target.map_or(Route::Nowhere, Route::To))
    }

    /// The session of `agent` that `event` goes to: its thread's owner
    /// while that is a live session of this agent, else the usual choice
    /// ([`routing::choose`]).
    fn recipient<'a>(
        &self,
        agent: &SenatorPath,
        event: &Event,
        cands: &[&'a Session],
    ) -> Result<Option<&'a Session>, StoreError> {
        if let Some(root) = event.thread
            && let Some(owner) = self.store.owner(&event.committee, root)?
            && &owner.agent == agent
            && let Some(session) = cands.iter().find(|s| s.session_id == owner.session)
        {
            return Ok(Some(session));
        }
        Ok(routing::choose(event, cands, routing::cwd_contains))
    }

    /// The event as pushed to `agent`: for reassigned work it says whose
    /// message it was; a root that went through triage carries the verdict
    /// in `meta.triage`, so its header can say so (`unsure` when none came
    /// in time).
    fn annotate(&self, agent: &SenatorPath, mut event: Event) -> Event {
        event.meta.remove(format::COVERING_META);
        event.meta.remove(format::SPEAKER_META);
        if !event.is_directed_to(&Address::senator(agent)) && self.for_speaker(agent, &event) {
            let senate = match self.seating().presider() {
                Some(presider) => presider.to_string(),
                None => String::new(),
            };
            event
                .meta
                .insert(format::SPEAKER_META.into(), senate.into());
        }
        if !event.is_directed_to(&Address::senator(agent))
            && let Some(old) = lock(&self.covering).covered_for(&event, agent)
        {
            event
                .meta
                .insert(format::COVERING_META.into(), old.to_string().into());
        }
        // Where nothing triages, no verdict is not "the Speaker was too slow".
        if needs_triage(&event) && self.triaged.load(Ordering::Relaxed) {
            let verdict = self
                .store
                .verdict(&event.committee, event.seq)
                .ok()
                .flatten();
            let triage = TriagePayload {
                verdict: verdict.unwrap_or(TriageVerdict::Unsure),
                p_needs_human: 0.0,
                p_agent: 0.0,
                p_unsure: 0.0,
                rule: None,
                reason: verdict
                    .is_none()
                    .then(|| "the Speaker gave no verdict in time".into()),
            };
            event.meta.insert(meta::TRIAGE.into(), triage.value());
        }
        event
    }

    /// The session, as `agent`, now owns the threads of the roots it was
    /// pushed.
    fn own_roots(
        &self,
        agent: &SenatorPath,
        events: &[Event],
        sid: &str,
    ) -> Result<(), StoreError> {
        let owner = TaskOwner {
            agent: agent.clone(),
            session: sid.to_string(),
        };
        for e in events.iter().filter(|e| e.thread.is_none()) {
            self.store.set_owner(&e.committee, e.seq, &owner)?;
        }
        Ok(())
    }

    /// With no live session of `agent` bound to `committee`, keep only the
    /// agent's newest [`PENDING_CAP`] items there.
    fn cap_unbound(&self, agent: &SenatorPath, committee: &CommitteeId) -> Result<(), StoreError> {
        let bound = self
            .store
            .live_sessions()?
            .iter()
            .any(|s| &s.agent == agent && s.committee() == Some(committee));
        if bound {
            return Ok(());
        }
        let dropped = self.store.cap_pending(agent, committee, PENDING_CAP)?;
        if dropped > 0 {
            tracing::warn!(
                %agent,
                %committee,
                dropped,
                "no session is bound; dropped the oldest waiting items (keeping {PENDING_CAP}); \
                 the next push says so"
            );
        }
        Ok(())
    }

    /// `PUT` `agent`'s `delivered` cursor on `committee` when it moved.
    pub(crate) async fn report_cursor(&self, agent: &SenatorPath, committee: &CommitteeId) {
        let row = match self.store.cursor(agent, committee) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%agent, %committee, "reading cursor: {e}");
                return;
            }
        };
        if row.delivered <= row.reported {
            return;
        }
        match self
            .api
            .put_cursor(committee, api::CURSOR_DELIVERED, Some(agent), row.delivered)
            .await
        {
            Ok(_) => {
                if let Err(e) = self.store.mark_reported(agent, committee, row.delivered) {
                    tracing::warn!(%agent, %committee, "recording cursor report: {e}");
                }
            }
            Err(e) => tracing::debug!(%agent, %committee, "cursor report failed: {e}"),
        }
    }

    /// `agent`'s heartbeat on `committee`: its own live sessions there and
    /// its own delivered cursor.
    pub(crate) async fn heartbeat(&self, agent: &SenatorPath, committee: &CommitteeId) {
        let (sessions, cursor) = match (
            self.store.live_sessions(),
            self.store.delivered_cursor(agent, committee),
        ) {
            (Ok(s), Ok(c)) => (
                s.iter()
                    .filter(|s| &s.agent == agent && s.committee() == Some(committee))
                    .count(),
                c,
            ),
            (Err(e), _) | (_, Err(e)) => {
                tracing::warn!(%agent, %committee, "reading state for heartbeat: {e}");
                return;
            }
        };
        let beat = Heartbeat {
            agent: agent.clone(),
            sessions: u32::try_from(sessions).unwrap_or(u32::MAX),
            cursor,
        };
        if let Err(e) = self.api.heartbeat(committee, &beat).await {
            tracing::debug!(%agent, %committee, "heartbeat failed: {e}");
        }
    }

    // ---- delivery --------------------------------------------------------

    /// Whether `sid`'s inbox may be pushed to now. `held`: items are held
    /// for the session after an inbox failure.
    fn hold(&self, sid: &str, held: bool, now: Instant) -> Hold {
        let pushes = lock(&self.pushes);
        let Some(p) = pushes.get(sid) else {
            return Hold::Ready;
        };
        if held && p.failures > INBOX_RETRIES {
            return Hold::Indefinitely;
        }
        let mut at = now;
        if held && let Some(retry) = p.retry_at {
            at = at.max(retry);
        }
        if let Some(last) = p.last_attempt {
            at = at.max(last + self.timing.push_spacing);
        }
        if at > now {
            Hold::Until(at)
        } else {
            Hold::Ready
        }
    }

    fn note_attempt(&self, sid: &str) {
        lock(&self.pushes)
            .entry(sid.to_string())
            .or_default()
            .last_attempt = Some(Instant::now());
    }

    fn note_delivered(&self, sid: &str) {
        if let Some(p) = lock(&self.pushes).get_mut(sid) {
            p.failures = 0;
            p.retry_at = None;
        }
    }

    /// The inbox did not take a batch; when to retry, if retries remain.
    fn note_refused(&self, sid: &str) -> Option<Duration> {
        let mut pushes = lock(&self.pushes);
        let p = pushes.entry(sid.to_string()).or_default();
        p.failures = p.failures.saturating_add(1);
        if p.failures > INBOX_RETRIES {
            p.retry_at = None;
            return None;
        }
        let delay = retry_delay(&self.timing, p.failures);
        p.retry_at = Some(Instant::now() + delay);
        Some(delay)
    }

    /// One pass: route pending items, add items held for sessions with an
    /// inbox, and push each session's share. Returns when a held session
    /// becomes ready, if one is waiting.
    async fn delivery_pass(self: &Arc<Self>) -> anyhow::Result<Option<Instant>> {
        let pending = self.store.pending()?;
        let sessions = self.checked_live_sessions()?;
        let mut plan: BTreeMap<String, Vec<QueueItem>> = BTreeMap::new();
        let mut due: Option<Instant> = None;
        let mut waiting: BTreeSet<String> = BTreeSet::new();
        let mut for_companion: Option<QueueItem> = None;
        for item in pending {
            let cands = routing::candidates(&sessions, &item.agent, &item.event.committee);
            let route = self.route(&item, &cands)?;
            if !matches!(route, Route::Wait { .. })
                && for_companion.is_none()
                && self.companion_wanted(&item, &cands)
            {
                for_companion = Some(item);
                continue;
            }
            match route {
                Route::To(s) => plan.entry(s.session_id.clone()).or_default().push(item),
                Route::Wait { until, to } => {
                    due = Some(due.map_or(until, |d| d.min(until)));
                    if let Some(s) = to {
                        waiting.insert(s.session_id.clone());
                    }
                }
                Route::Nowhere => {}
            }
        }
        if let Some(item) = for_companion {
            self.spawn_companion(item)?;
        }
        // A session with an item waiting on triage gets nothing yet: its push
        // waits so it stays one message in seq order.
        plan.retain(|sid, _| !waiting.contains(sid));
        // Items held after an inbox failure go back to that inbox, ahead of
        // anything newer.
        for s in sessions.iter().filter(|s| s.can_push()) {
            let held = self.store.queued_for(&s.session_id)?;
            if !held.is_empty() {
                plan.entry(s.session_id.clone()).or_default().extend(held);
            }
        }
        let now = Instant::now();
        let mut again = false;
        for (sid, mut items) in plan {
            let Some(session) = sessions.iter().find(|s| s.session_id == sid) else {
                continue;
            };
            if session.can_push() {
                let held = items.iter().any(|i| i.status == ItemStatus::Queued);
                match self.hold(&sid, held, now) {
                    Hold::Ready => {}
                    Hold::Until(at) => {
                        due = Some(due.map_or(at, |d| d.min(at)));
                        continue;
                    }
                    Hold::Indefinitely => continue,
                }
            }
            items.sort_by_key(QueueItem::key);
            again |= self.deliver_to(session, items).await?;
        }
        if again {
            self.work.notify_one();
        }
        Ok(due)
    }

    /// Push one session's items as one message. Every item routed to a
    /// session is its agent's. Returns whether another pass is needed
    /// (items held back, or the session turned out gone).
    async fn deliver_to(&self, session: &Session, items: Vec<QueueItem>) -> anyhow::Result<bool> {
        let sid = session.session_id.as_str();
        let agent = &session.agent;
        let keys: Vec<ItemKey> = items
            .iter()
            .filter(|i| &i.agent == agent)
            .map(QueueItem::key)
            .collect();
        if !session.can_push() {
            // Nothing can be pushed to it, so what is routed to it waits for
            // its next hook. Hold it straight from pending: were it in
            // flight first, a hook running just then would find nothing and
            // the backlog would wait for the hook after it.
            let held = self.store.hold_pending_for_hooks(&keys, sid)?;
            if !held.is_empty() {
                tracing::info!(
                    session = sid,
                    count = held.len(),
                    "held for the session's next hook"
                );
            }
            return Ok(false);
        }
        let claimed = self.store.claim_inflight_with_held(&keys, sid)?;
        let mut events: Vec<Event> = items
            .into_iter()
            .filter(|i| claimed.contains(&i.key()))
            .map(|i| self.annotate(agent, i.event))
            .collect();
        events.sort_by(|a, b| (&a.committee, a.seq).cmp(&(&b.committee, b.seq)));
        let Some(committee) = events.first().map(|e| e.committee.clone()) else {
            return Ok(false);
        };
        let dropped = self.store.dropped(agent, &committee)?;
        let take = events.len().min(MAX_BATCH_EVENTS);
        let rendered = format::render_push_noting(
            &events[..take],
            &Address::senator(agent),
            INBOX_MAX_CHARS,
            events.len() - take,
            dropped,
            self.locale,
        );
        let later = keys_of(agent, &events[rendered.included..]);
        events.truncate(rendered.included);
        let mut again = false;
        if !later.is_empty() {
            self.store.release(&later)?;
            again = true;
        }
        let now = keys_of(agent, &events);
        let batch = Batch {
            committee: committee.clone(),
            events,
            text: rendered.text,
        };
        self.note_attempt(sid);
        let report = self.ladder.deliver(session, &batch).await;
        tracing::debug!(session = sid, attempts = ?report.attempts, "delivery ladder");
        match report.outcome {
            DeliveryOutcome::Injected => {
                let via = match session.runtime {
                    Runtime::Claude => "inbox",
                    Runtime::Codex => "codex",
                };
                self.store.finish_inflight(&now, sid, via)?;
                self.own_roots(agent, &batch.events, sid)?;
                self.store.set_socket_ok(sid, true)?;
                self.store.consume_dropped(agent, &committee, dropped)?;
                self.note_delivered(sid);
                tracing::info!(
                    session = sid,
                    %agent,
                    count = now.len(),
                    via,
                    "pushed into the session"
                );
            }
            DeliveryOutcome::Spawned => {
                self.store.finish_inflight(&now, sid, "spawned")?;
                self.own_roots(agent, &batch.events, sid)?;
                self.store.consume_dropped(agent, &committee, dropped)?;
            }
            DeliveryOutcome::Queued => {
                self.store.hold_for_hooks(&now, sid)?;
                if let Some(delay) = self.note_refused(sid) {
                    let why = report
                        .attempts
                        .iter()
                        .find_map(|(_, o)| match o {
                            DeliveryOutcome::Unavailable { reason, .. }
                                if !reason.starts_with("not a ") =>
                            {
                                Some(reason.as_str())
                            }
                            _ => None,
                        })
                        .unwrap_or("");
                    tracing::info!(
                        session = sid,
                        count = now.len(),
                        why,
                        "the push was not taken; held for the session's next hook and retried in \
                         {delay:?}"
                    );
                } else {
                    tracing::warn!(
                        session = sid,
                        count = now.len(),
                        "the inbox did not take the batch after {INBOX_RETRIES} retries; it waits \
                         for the session's next hook"
                    );
                }
            }
            DeliveryOutcome::Unavailable {
                reason,
                unreachable,
            } => {
                self.store.release(&now)?;
                if unreachable {
                    tracing::warn!(session = sid, %reason, "session unreachable; marking stale");
                    self.store.set_socket_ok(sid, false)?;
                    self.store.set_session_state(sid, SessionState::Stale)?;
                    lock(&self.pushes).remove(sid);
                    self.committees_changed.notify_one();
                    again = true;
                } else {
                    tracing::warn!(session = sid, %reason, "no delivery adapter took the batch");
                }
            }
        }
        Ok(again)
    }

    // ---- the desktop app: approvals and watch -------------------------------

    /// One approvals subscriber per live Codex session.
    pub(crate) fn reconcile_approvals(&self) {
        match self.store.live_sessions() {
            Ok(sessions) => {
                let codex: Vec<String> = sessions
                    .into_iter()
                    .filter(|s| s.runtime == Runtime::Codex)
                    .map(|s| s.session_id)
                    .collect();
                // One relay watches the Codex sessions of every Senate.
                let all: Vec<String> = {
                    let mut by_senate = lock(&self.codex_sessions);
                    by_senate.insert(self.index, codex);
                    by_senate.values().flatten().cloned().collect()
                };
                self.relay.reconcile(&all);
            }
            Err(e) => tracing::warn!("reading sessions for the approvals relay: {e}"),
        }
    }

    fn publish(&self, event: WatchEvent) {
        // Nobody watching is not an error.
        let _ = self.watch.send(event);
    }

    fn publish_status(&self) {
        if self.watch.receiver_count() == 0 {
            return;
        }
        match self.on_status() {
            Ok(status) => self.publish(WatchEvent::Status(Box::new(status))),
            Err(e) => tracing::debug!("status for watchers: {e}"),
        }
    }

    // ---- the companion session ---------------------------------------------

    /// Whether `item` should go to a companion now (see the module docs):
    /// companions are on and none is running, it is a root question to its
    /// agent not tried yet, and no session of that agent has been active
    /// within `idle_secs`.
    fn companion_wanted(&self, item: &QueueItem, cands: &[&Session]) -> bool {
        let event = &item.event;
        let config = &self.companion_config;
        if !config.enabled
            || event.kind != Kind::Question
            || event.thread.is_some()
            || !event.is_directed_to(&Address::senator(&item.agent))
            || lock(&self.companion).is_some()
            || lock(&self.companion_tried).contains(&item.key())
        {
            return false;
        }
        let idle = chrono::Duration::from_std(config.idle()).unwrap_or(chrono::Duration::MAX);
        let now = chrono::Utc::now();
        cands.iter().all(|s| now - s.last_seen > idle)
    }

    /// Start a companion for `item`: a fork of its agent's latest Claude
    /// Code session on its committee. The item is in flight for it until it
    /// exits. A question for an agent with no Claude Code session on record
    /// (a Codex agent) waits: only Claude Code sessions can be forked.
    fn spawn_companion(self: &Arc<Self>, item: QueueItem) -> Result<(), StoreError> {
        let key = item.key();
        lock(&self.companion_tried).insert(key.clone());
        let committee = item.event.committee.clone();
        let agent = item.agent.clone();
        let base = self.store.all_sessions()?.into_iter().find(|s| {
            s.committee() == Some(&committee)
                && s.agent == agent
                && s.runtime == Runtime::Claude
                && s.transcript_path.is_some()
                && !s.session_id.starts_with(companion::SESSION_PREFIX)
        });
        let Some(base) = base else {
            tracing::info!(
                %agent,
                %committee,
                seq = %item.event.seq,
                "no earlier Claude Code session of this agent on the committee to fork; the \
                 question waits"
            );
            return Ok(());
        };
        let sid = format!("{}{}", companion::SESSION_PREFIX, EventId::new());
        let keys = vec![key];
        if self.store.claim_inflight(&keys, &sid)?.is_empty() {
            return Ok(());
        }
        let event = self.annotate(&agent, item.event);
        let pushed = format::render_push_noting(
            std::slice::from_ref(&event),
            &Address::senator(&agent),
            INBOX_MAX_CHARS,
            0,
            0,
            self.locale,
        );
        let prompt = companion::prompt(&agent, &event, &pushed.text, self.locale);
        *lock(&self.companion) = Some(companion::Running {
            session_id: sid.clone(),
            agent: agent.clone(),
            committee: committee.clone(),
            root: event.seq,
            cwd: base.cwd.clone(),
            branch: base.branch.clone(),
            git_root: base.git_root.clone(),
            answered: false,
        });
        tracing::info!(
            %committee,
            seq = %event.seq,
            resume = %base.session_id,
            companion = %sid,
            outcome = ?DeliveryOutcome::Spawned,
            "a companion session takes the question"
        );
        let shared = self.clone();
        tokio::spawn(async move {
            let outcome = companion::run(
                &shared.companion_config,
                &base.session_id,
                &base.cwd,
                &sid,
                &shared.home,
                &prompt,
            )
            .await;
            let answered = lock(&shared.companion)
                .take()
                .is_some_and(|running| running.answered);
            let settled = if answered {
                shared.store.finish_inflight(&keys, &sid, "companion")
            } else {
                shared.store.release(&keys)
            };
            if let Err(e) = settled {
                tracing::warn!(companion = %sid, "recording the companion's outcome: {e}");
            }
            if answered {
                tracing::info!(companion = %sid, ?outcome, "the companion answered");
            } else {
                tracing::warn!(
                    companion = %sid,
                    ?outcome,
                    "the companion did not answer; the question waits for a session"
                );
            }
            shared.work.notify_one();
        });
        Ok(())
    }

    /// The running companion as a session, if `selector` names it.
    fn companion_session(&self, selector: &SessionSelector) -> Option<Session> {
        let sid = selector.session_id.as_deref()?;
        let running = lock(&self.companion);
        let c = running.as_ref().filter(|c| c.session_id == sid)?;
        let now = chrono::Utc::now();
        Some(Session {
            session_id: c.session_id.clone(),
            runtime: Runtime::Claude,
            agent: c.agent.clone(),
            agent_source: agent::AgentSource::Harness,
            cwd: c.cwd.clone(),
            git_root: c.git_root.clone(),
            branch: c.branch.clone(),
            inferred_committee: None,
            committee_source: None,
            joined_committee: Some(c.committee.clone()),
            socket: None,
            token: None,
            claude_pid: None,
            transcript_path: None,
            state: SessionState::Live,
            started_at: now,
            last_seen: now,
            socket_ok: None,
        })
    }

    fn is_companion(session: &Session) -> bool {
        session.session_id.starts_with(companion::SESSION_PREFIX)
    }

    /// A companion may answer and read, nothing else.
    fn companion_refusal(&self, request: &Request) -> Option<IpcError> {
        let (op, session) = match request {
            Request::Ask { session, .. } => ("ask", session),
            Request::Join { session, .. } => ("join", session),
            Request::Post { session, .. } => ("post", session),
            Request::Task { session, .. } => ("status", session),
            Request::Escalate { session, .. } => ("escalate", session),
            Request::Take { session, .. } => ("take", session),
            Request::Inbox { session, .. } => ("inbox", session),
            Request::Checkpoint { session, .. } => ("checkpoint", session),
            _ => return None,
        };
        self.companion_session(session).map(|_| {
            IpcError::bad_request(format!(
                "a companion session only answers its question and reads; `{op}` is not for it"
            ))
        })
    }

    // ---- IPC handlers ----------------------------------------------------

    async fn dispatch(self: &Arc<Self>, request: Request) -> Reply {
        if let Some(refused) = self.companion_refusal(&request) {
            return Reply::err(refused);
        }
        match request {
            Request::Ping => Reply::ok(&Pong {
                protocol: ipc::PROTOCOL_VERSION,
                pid: std::process::id(),
                person: self.config.person.clone(),
                locale: self.locale,
                version: env!("CARGO_PKG_VERSION").to_string(),
            }),
            Request::Status => wrap(self.on_status()),
            Request::Hook(report) => {
                let codex = report.runtime == Runtime::Codex;
                let reply = wrap(self.on_hook(report, None).await);
                if codex {
                    self.reconcile_approvals();
                }
                reply
            }
            // The front serves these: they span Senates.
            Request::Presides { .. }
            | Request::Docket { .. }
            | Request::Route { .. }
            | Request::Cover { .. }
            | Request::Refer { .. } => Reply::err(IpcError::bad_request(
                "a presiding request is served by the daemon's front",
            )),
            Request::SessionEnd { session_id, .. } => wrap(self.on_session_end(&session_id)),
            Request::Checkpoint {
                session,
                summary,
                progress,
                decisions,
                open,
                push,
                id,
            } => {
                let files = CheckpointFiles {
                    progress,
                    decisions,
                    open,
                };
                wrap(self.on_checkpoint(&session, summary, files, push, id).await)
            }
            Request::Ask {
                session,
                to,
                body,
                refs,
                id,
                request,
            } => wrap(self.on_ask(&session, to, body, refs, id, request).await),
            Request::Answer {
                session,
                thread,
                body,
                id,
                committee,
                agent,
                as_person,
            } => {
                let acting = Acting {
                    committee,
                    agent,
                    as_person,
                };
                wrap(self.on_answer(&session, thread, body, id, acting).await)
            }
            Request::CatchUp { session, limit } => wrap(self.on_catch_up(&session, limit).await),
            Request::Join { session, committee } => wrap(self.on_join(&session, committee).await),
            Request::Search {
                session,
                query,
                limit,
                committee,
            } => wrap(self.on_search(&session, committee, query, limit).await),
            Request::Post {
                session,
                body,
                proposal,
                committee,
                agent,
                id,
            } => wrap(
                self.on_post(&session, committee, agent, body, proposal, id)
                    .await,
            ),
            Request::Brief { session, committee } => wrap(self.on_brief(&session, committee).await),
            Request::Bound { session } => wrap(self.on_bound(&session)),
            Request::Task {
                session,
                thread,
                status,
                id,
                committee,
                agent,
                as_person,
            } => {
                let acting = Acting {
                    committee,
                    agent,
                    as_person,
                };
                wrap(self.on_task(&session, thread, status, id, acting).await)
            }
            Request::Escalate {
                session,
                thread,
                to,
                reason,
                id,
            } => wrap(self.on_escalate(&session, thread, to, reason, id).await),
            Request::Take {
                session,
                thread,
                id,
                committee,
            } => wrap(self.on_take(&session, thread, id, committee).await),
            Request::Say {
                session,
                committee,
                kind,
                to,
                body,
                thread,
                id,
            } => wrap(
                self.on_say(&session, committee, kind, to, body, thread, id)
                    .await,
            ),
            Request::Thread {
                session,
                committee,
                thread,
                mark_seen,
            } => wrap(self.on_thread(&session, committee, thread, mark_seen).await),
            Request::Seen { committee, marks } => wrap(self.on_seen(&committee, &marks)),
            Request::Inbox {
                session,
                committee,
                agent,
                all,
            } => wrap(self.on_inbox(&session, committee, agent, all).await),
            Request::Approvals => Reply::ok(&ApprovalsReply {
                approvals: self.relay.list(),
            }),
            Request::Approve { id, decision } => wrap(
                self.relay
                    .answer(&id, decision)
                    .await
                    .map(|approval| ApproveReply { approval, decision })
                    .map_err(IpcError::bad_request),
            ),
            // Served by the connection itself (`serve_connection`).
            Request::Watch { .. } => Reply::err(IpcError::bad_request(
                "watch turns a connection into a stream; send it as the connection's request",
            )),
        }
    }

    fn on_status(&self) -> Result<StatusReply, IpcError> {
        let sessions = self.store.sessions().map_err(internal)?;
        let cursors: BTreeMap<Seat, _> = self
            .store
            .cursors()
            .map_err(internal)?
            .into_iter()
            .map(|(a, c, row)| ((a, c), row))
            .collect();
        let streams = lock(&self.streams);
        let mut seats: BTreeSet<Seat> = streams.keys().cloned().collect();
        seats.extend(cursors.keys().cloned());
        let committees = seats
            .into_iter()
            .map(|(agent, committee)| {
                let seat = (agent, committee);
                let cursor = cursors.get(&seat);
                let (agent, committee) = seat.clone();
                CommitteeStatus {
                    delivered: cursor.map_or(Seq::ZERO, |c| c.delivered),
                    reported: cursor.map_or(Seq::ZERO, |c| c.reported),
                    stream: streams.get(&seat).map(CommitteeWorker::state),
                    configured: self.config.committees.contains(&committee),
                    sessions: sessions
                        .iter()
                        .filter(|s| {
                            s.is_live() && s.agent == agent && s.committee() == Some(&committee)
                        })
                        .count(),
                    agent,
                    committee,
                }
            })
            .collect();
        let seating = self.seating();
        Ok(StatusReply {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            person: self.config.person.clone(),
            presider: seating.presider().cloned(),
            locale: self.locale,
            server_url: self.api.base_url().to_string(),
            committees,
            sessions,
            queue: self.store.counts().map_err(internal)?,
            speaker: self.speaker_seat(),
            others: Vec::new(),
        })
    }

    /// Register the session the hook reports. `placed` is its agent when
    /// the front already worked it out (several Senates); else it is worked
    /// out here, under this Senate's seating.
    async fn on_hook(
        &self,
        report: HookReport,
        placed: Option<(SenatorPath, AgentSource)>,
    ) -> Result<HookReply, IpcError> {
        self.on_hook_as(report, placed, false).await
    }

    /// [`Shared::on_hook`]; `as_speaker`: the session is attached here as
    /// the Speaker of this Senate (it sits in another), so it is told
    /// nothing at start but what is put to it.
    pub(crate) async fn on_hook_as(
        &self,
        report: HookReport,
        placed: Option<(SenatorPath, AgentSource)>,
        as_speaker: bool,
    ) -> Result<HookReply, IpcError> {
        let inferred = session::infer_committee(
            report.branch.as_deref(),
            report.committee_file.as_deref(),
            self.convention.as_ref(),
        );
        let (agent, agent_source) = match placed {
            Some(placed) => placed,
            None => {
                let (rel, source) = agent::choose(
                    &self.config.agents,
                    report.runtime,
                    agent::Pins {
                        env: report.agent_env.as_deref(),
                        file: report.agent_file.as_deref(),
                    },
                );
                (self.seating().place(&rel), source)
            }
        };
        let before = self.store.session(&report.session_id).map_err(internal)?;
        let session = self
            .store
            .upsert_session(&SessionUpsert {
                session_id: report.session_id.clone(),
                cwd: report.cwd.clone(),
                git_root: report.git_root.clone(),
                branch: report.branch.clone(),
                inferred_committee: inferred,
                socket: report.socket.clone(),
                token: report.token.clone(),
                claude_pid: report.claude_pid,
                transcript_path: report.transcript_path.clone(),
                ancestor_pids: report.ancestor_pids.clone(),
                runtime: report.runtime,
                agent,
                agent_source,
            })
            .map_err(internal)?;
        if before.as_ref().is_some_and(|b| b.agent != session.agent) {
            let released = self
                .store
                .release_held_for_others(&session.session_id, &session.agent)
                .map_err(internal)?;
            tracing::info!(
                session = %session.session_id,
                agent = %session.agent,
                released,
                "the session speaks as another agent now"
            );
        }
        // The branch changed under the session and took its committee
        // with it (an explicit join is not disturbed): tell it at this hook.
        if before.as_ref().is_some_and(|b| {
            b.branch != session.branch && b.inferred_committee != session.inferred_committee
        }) && session.joined_committee.is_none()
        {
            lock(&self.rebinds).insert(
                session.session_id.clone(),
                (session.branch.clone(), session.inferred_committee.clone()),
            );
        }
        let was = before.map(|b| (b.agent.clone(), b.committee().cloned(), b.state));
        let now = (
            session.agent.clone(),
            session.committee().cloned(),
            SessionState::Live,
        );
        if was != Some(now) {
            self.committees_changed.notify_one();
        }
        let brief = match (report.event, session.committee()) {
            (HookEvent::SessionStart, Some(committee)) if !as_speaker => {
                self.hook_brief(committee).await
            }
            _ => None,
        };
        let event = if as_speaker {
            HookEvent::UserPromptSubmit
        } else {
            report.event
        };
        let (context, delivered) = self.hook_context(&session, event, brief.as_ref())?;
        // The worktree's branch changed under this session since it last
        // spoke: say what it is bound to now.
        let rebound = lock(&self.rebinds).remove(&session.session_id);
        let context = match rebound {
            Some((branch, committee)) if session.joined_committee.is_none() => {
                let note = rebind_note(self.locale, branch.as_deref(), committee.as_ref());
                Some(match context {
                    Some(text) => format!("{note}\n\n{text}"),
                    None => note,
                })
            }
            _ => context,
        };
        // The user is active in the session: a held backlog may try its
        // inbox again right away.
        self.note_delivered(&session.session_id);
        self.work.notify_one();
        Ok(HookReply {
            session,
            context,
            delivered,
        })
    }

    /// The committee's brief for a starting session, or `None` when
    /// `brief_chars` is 0 or the server does not give it in time. A session
    /// start never fails for want of it.
    async fn hook_brief(&self, committee: &CommitteeId) -> Option<Brief> {
        if self.config.brief_chars == 0 {
            return None;
        }
        match tokio::time::timeout(HOOK_BRIEF_TIMEOUT, self.api.brief(committee)).await {
            Ok(Ok(brief)) => Some(brief),
            Ok(Err(e)) => {
                tracing::warn!(%committee, "session start without the brief: {e}");
                None
            }
            Err(_) => {
                tracing::warn!(%committee, "session start without the brief: the server was slow");
                None
            }
        }
    }

    /// Hand the session what is waiting for it: at SessionStart the
    /// orientation line and the `brief`, then items held for its hooks and
    /// pending items of its committee that routing gives to it.
    ///
    /// A Codex session gets no items here: Codex answers the user's prompt
    /// and leaves hook context unanswered, so a question handed over that
    /// way is never answered. Its backlog stays queued, and the delivery
    /// pass the hook wakes pushes it through the app-server as a turn of
    /// its own (a steer while the user's turn runs), like any live push.
    fn hook_context(
        &self,
        session: &Session,
        event: HookEvent,
        brief: Option<&Brief>,
    ) -> Result<(Option<String>, usize), IpcError> {
        let Some(committee) = session.committee().cloned() else {
            return Ok((None, 0));
        };
        let sid = session.session_id.as_str();
        let agent = &session.agent;
        let mut items = Vec::new();
        if session.runtime == Runtime::Claude {
            items = self.store.queued_for(sid).map_err(internal)?;
            let live = self.store.live_sessions().map_err(internal)?;
            let cands = routing::candidates(&live, agent, &committee);
            for item in self
                .store
                .pending_for(agent, &committee)
                .map_err(internal)?
            {
                if let Route::To(s) = self.route(&item, &cands).map_err(internal)?
                    && s.session_id == sid
                {
                    items.push(item);
                }
            }
        }
        items.sort_by_key(QueueItem::key);
        items.dedup_by_key(|i| i.key());

        let mut parts = Vec::new();
        let mut budget = HOOK_CONTEXT_MAX;
        if event == HookEvent::SessionStart {
            let seating = self.seating();
            let line = orientation(
                &committee,
                agent,
                seating.presider(),
                brief.is_some_and(|b| !b.groups.is_empty()),
                self.locale,
            );
            budget = budget.saturating_sub(format::char_len(&line) + 2);
            parts.push(line);
        }
        // A brief_chars near the hook's cap still leaves the backlog room.
        let reserve = if items.is_empty() {
            0
        } else {
            HOOK_BACKLOG_RESERVE
        };
        let room = self.config.brief_chars.min(budget.saturating_sub(reserve));
        if let Some(text) = brief.and_then(|b| brief_context(b, room, self.locale)) {
            budget = budget.saturating_sub(format::char_len(&text) + 2);
            parts.push(text);
        }
        let mut delivered = 0;
        if !items.is_empty() {
            let dropped = self.store.dropped(agent, &committee).map_err(internal)?;
            let events: Vec<Event> = items
                .into_iter()
                .filter(|i| &i.agent == agent)
                .map(|i| self.annotate(agent, i.event))
                .collect();
            let fit = format::render_push_noting(
                &events,
                &Address::senator(agent),
                budget,
                0,
                dropped,
                self.locale,
            )
            .included;
            let keys = keys_of(agent, &events[..fit]);
            let claimed = self.store.claim_for_hook(&keys, sid).map_err(internal)?;
            let mine: Vec<Event> = events
                .iter()
                .filter(|e| claimed.contains(&(agent.clone(), e.committee.clone(), e.seq)))
                .cloned()
                .collect();
            if !mine.is_empty() {
                self.own_roots(agent, &mine, sid).map_err(internal)?;
                let rendered = format::render_push_noting(
                    &mine,
                    &Address::senator(agent),
                    budget,
                    events.len() - fit,
                    dropped,
                    self.locale,
                );
                parts.push(rendered.text);
                delivered = mine.len();
                self.store
                    .consume_dropped(agent, &committee, dropped)
                    .map_err(internal)?;
            }
        }
        let text = parts.join("\n\n");
        Ok(((!text.is_empty()).then_some(text), delivered))
    }

    fn on_session_end(self: &Arc<Self>, session_id: &str) -> Result<serde_json::Value, IpcError> {
        let before = self.store.session(session_id).map_err(internal)?;
        if self
            .store
            .set_session_state(session_id, SessionState::Ended)
            .map_err(internal)?
        {
            self.committees_changed.notify_one();
            self.work.notify_one();
            // The hook waits for this reply; the checkpoint does not keep it.
            if let Some(session) = before.filter(|s| s.committee().is_some()) {
                let shared = self.clone();
                tokio::spawn(async move {
                    shared
                        .auto_checkpoint(&session, CheckpointTrigger::SessionEnd)
                        .await;
                });
            }
        }
        Ok(serde_json::json!({}))
    }

    // ---- checkpoints -------------------------------------------------------

    /// Make the session's worktree `.senate/` match the branch checked out
    /// there now ([`crate::follow`]), and its committee what that says: a
    /// checkpoint must carry the notes and committee of the branch it is
    /// about, never the previous branch's. Returns the session as it is
    /// now. An explicit `join` is not disturbed.
    async fn follow_branch(&self, session: &Session) -> Session {
        let paths = Paths::new(&self.home);
        let (facts, _) = session::gather_synced(&paths, &session.cwd, Duration::from_secs(2)).await;
        let inferred = session::infer_committee(
            facts.branch.as_deref(),
            facts.committee_file.as_deref(),
            self.convention.as_ref(),
        );
        let known = session
            .inferred_committee
            .clone()
            .zip(session.committee_source);
        if facts.branch == session.branch && inferred == known {
            return session.clone();
        }
        if let Err(e) = self.store.set_inferred(
            &session.session_id,
            facts.branch.as_deref(),
            inferred.as_ref(),
        ) {
            tracing::warn!("recording the session's branch: {e}");
            return session.clone();
        }
        if facts.branch != session.branch && session.joined_committee.is_none() {
            lock(&self.rebinds).insert(
                session.session_id.clone(),
                (
                    facts.branch.clone(),
                    inferred.as_ref().map(|(c, _)| c.clone()),
                ),
            );
        }
        self.committees_changed.notify_one();
        self.store
            .session(&session.session_id)
            .ok()
            .flatten()
            .unwrap_or_else(|| session.clone())
    }

    /// Append a checkpoint of the worktree at `root` for `session`, as the
    /// session's agent: a checkpoint is one agent's handover note.
    async fn append_checkpoint(
        &self,
        session: &Session,
        committee: CommitteeId,
        root: &Path,
        trigger: CheckpointTrigger,
        summary: Option<String>,
        id: Option<EventId>,
    ) -> Result<(CheckpointPayload, bool, Posted), IpcError> {
        let (payload, truncated) =
            checkpoint::snapshot(root, trigger, session.transcript_path.as_deref())
                .await
                .map_err(IpcError::bad_request)?;
        let body = summary
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| default_summary(&payload));
        let mut event = NewEvent::new(
            committee,
            Kind::Checkpoint,
            Address::senator(&session.agent),
            body,
        )
        .with_refs(Refs {
            commit: Some(payload.commit.clone()),
            path: None,
            branch: Some(payload.branch.clone()),
        })
        .with_meta(meta::CHECKPOINT, payload.value());
        if let Some(id) = id {
            event = event.with_id(id);
        }
        let posted = self.post(event).await?;
        tracing::info!(
            agent = %session.agent,
            committee = %posted.committee,
            seq = %posted.seq,
            trigger = trigger.as_str(),
            at = %payload.at(),
            "checkpoint"
        );
        Ok((payload, truncated, posted))
    }

    async fn on_checkpoint(
        &self,
        selector: &SessionSelector,
        summary: Option<String>,
        files: CheckpointFiles,
        push: bool,
        id: Option<EventId>,
    ) -> Result<CheckpointReply, IpcError> {
        let session = self.resolve(selector)?;
        let session = self.follow_branch(&session).await;
        let committee = self.bound(&session)?;
        let Some(root) = worktree(&session).await else {
            return Err(IpcError::bad_request(format!(
                "{} is not in a git worktree; a checkpoint records a branch and a commit",
                session.cwd.display()
            )));
        };
        let written = checkpoint::write_files(&root, &files).map_err(|e| {
            IpcError::internal(format!(
                "writing the checkpoint files under {}: {e}",
                root.display()
            ))
        })?;
        let (push_output, push_error) = if push {
            match checkpoint::push(&root).await {
                Ok(out) => (Some(out), None),
                Err(e) => (None, Some(e)),
            }
        } else {
            (None, None)
        };
        let trigger = if push {
            CheckpointTrigger::Handoff
        } else {
            CheckpointTrigger::Tool
        };
        let (payload, truncated, posted) = self
            .append_checkpoint(&session, committee, &root, trigger, summary, id)
            .await?;
        if let Some(entry) = lock(&self.auto).get_mut(&(session.agent.clone(), root.clone())) {
            entry.head.clone_from(&payload.commit);
            entry.last = Instant::now();
        }
        let present = [
            (&payload.files.progress, PROGRESS_FILE),
            (&payload.files.decisions, DECISIONS_FILE),
            (&payload.files.open, OPEN_FILE),
        ]
        .into_iter()
        .filter(|(f, _)| f.is_some())
        .map(|(_, name)| name.to_string())
        .collect();
        Ok(CheckpointReply {
            posted,
            branch: payload.branch,
            commit: payload.commit,
            pushed: payload.pushed,
            written: written.into_iter().map(str::to_string).collect(),
            files: present,
            truncated,
            push_output,
            push_error,
        })
    }

    /// An automatic checkpoint (see the module docs): never pushes, and is
    /// skipped when the log's latest one of the session's agent already says
    /// the same. Another agent's checkpoint does not count.
    async fn auto_checkpoint(&self, session: &Session, trigger: CheckpointTrigger) {
        let session = &self.follow_branch(session).await;
        let Some(committee) = session.committee().cloned() else {
            return;
        };
        let Some(root) = worktree(session).await else {
            return;
        };
        let Some(head) = checkpoint::head(&root).await else {
            return;
        };
        let files = checkpoint::read_files(&root);
        match self.api.latest_checkpoint(&committee, &session.agent).await {
            Ok(Some(latest))
                if latest.checkpoint.commit == head && latest.checkpoint.files == files =>
            {
                tracing::debug!(%committee, trigger = trigger.as_str(), "checkpoint unchanged; skipped");
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::debug!(%committee, "no automatic checkpoint: the latest is unknown ({e})");
                return;
            }
        }
        if let Err(e) = self
            .append_checkpoint(session, committee.clone(), &root, trigger, None, None)
            .await
        {
            tracing::warn!(%committee, trigger = trigger.as_str(), "automatic checkpoint failed: {e}");
        }
    }

    /// One poll of every live bound session's worktree: a new commit, or a
    /// session open for another interval, is a checkpoint.
    async fn poll_checkpoints(&self) {
        let sessions = match self.store.live_sessions() {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("reading sessions for checkpoints: {e}");
                return;
            }
        };
        let mut seen = BTreeSet::new();
        for session in sessions.iter().filter(|s| s.committee().is_some()) {
            let refreshed = self.follow_branch(session).await;
            let session = &refreshed;
            if session.committee().is_none() {
                continue;
            }
            let Some(root) = worktree(session).await else {
                continue;
            };
            let key = (session.agent.clone(), root.clone());
            if !seen.insert(key.clone()) {
                continue;
            }
            let Some(head) = checkpoint::head(&root).await else {
                continue;
            };
            let trigger = {
                let mut auto = lock(&self.auto);
                let entry = auto.entry(key.clone()).or_insert_with(|| AutoCheckpoint {
                    head: head.clone(),
                    last: Instant::now(),
                });
                if entry.head != head {
                    entry.head.clone_from(&head);
                    Some(CheckpointTrigger::Commit)
                } else if entry.last.elapsed() >= self.timing.checkpoint_interval {
                    Some(CheckpointTrigger::Interval)
                } else {
                    None
                }
            };
            if let Some(trigger) = trigger {
                self.auto_checkpoint(session, trigger).await;
                if let Some(entry) = lock(&self.auto).get_mut(&key) {
                    entry.last = Instant::now();
                }
            }
        }
        lock(&self.auto).retain(|key, _| seen.contains(key));
    }

    fn resolve(&self, selector: &SessionSelector) -> Result<Session, IpcError> {
        if let Some(companion) = self.companion_session(selector) {
            return Ok(companion);
        }
        let sessions = self.store.sessions().map_err(internal)?;
        let below = match selector.claude_pid {
            Some(pid) => self.store.sessions_below(pid).map_err(internal)?,
            None => Vec::new(),
        };
        let by_id = selector
            .session_id
            .as_ref()
            .and_then(|id| sessions.iter().find(|s| &s.session_id == id));
        // `sessions()` is most recent first, so `find` picks the latest: the
        // pid a hook reported as Claude's, else a pid above a hook.
        let by_pid = || {
            let pid = selector.claude_pid?;
            sessions
                .iter()
                .find(|s| s.claude_pid == Some(pid))
                .or_else(|| sessions.iter().find(|s| below.contains(&s.session_id)))
        };
        let Some(session) = by_id.or_else(by_pid) else {
            return Err(IpcError::new(
                codes::NO_SESSION,
                self.locale.pick(
                    "这个会话还没有在 Senate bridge 登记。bridge 通过会话的 SessionStart / \
                     UserPromptSubmit 钩子知道它：确认已运行过 `senate install` 且 daemon \
                     在运行，在这个会话里随便发一条消息，然后再试。",
                    "This session is not registered with the Senate bridge. The bridge learns \
                     about a session from its SessionStart / UserPromptSubmit hooks: make sure \
                     `senate install` was run and the daemon is up, send any prompt in this \
                     session, then try again.",
                ),
            ));
        };
        self.store
            .touch_session(&session.session_id)
            .map_err(internal)?;
        Ok(session.clone())
    }

    fn bound(&self, session: &Session) -> Result<CommitteeId, IpcError> {
        session.committee().cloned().ok_or_else(|| {
            // The branch convention is a way to bind only when it is on.
            let branch = self.convention.as_ref().map(|naming| {
                naming
                    .developer(
                        &CommitteeId::new("committee")
                            .unwrap_or_else(|_| unreachable!("a valid id")),
                        &self.config.person,
                    )
                    .replace("/committee/", "/<committee>/")
            });
            IpcError::new(
                codes::NOT_JOINED,
                match self.locale {
                    Locale::Zh => format!(
                        "这个会话没有加入任何 Senate 委员会。请你的用户在这个工作区里运行 \
                         `senate join <committee>`（写入 `.senate/committee`）{}，\
                         或者调用 join(committee) 只为这个会话加入。",
                        branch
                            .map(|b| format!("，或者改在 `{b}` 分支上工作"))
                            .unwrap_or_default()
                    ),
                    Locale::En => format!(
                        "This session is not joined to a Senate committee. Ask your user to run \
                         `senate join <committee>` in this worktree (it writes \
                         `.senate/committee`){}, or call join(committee) to join for this \
                         session only.",
                        branch
                            .map(|b| format!(", or to work on a `{b}` branch"))
                            .unwrap_or_default()
                    ),
                },
            )
        })
    }

    /// Append, turning the server's `ambiguous_recipient` into an error that
    /// carries the candidates and tells the asker how to choose.
    async fn post(&self, event: NewEvent) -> Result<Posted, IpcError> {
        let r = match self.api.append(&event).await {
            Ok(r) => r,
            Err(e) => {
                if let Some(candidates) = e.ambiguous_candidates() {
                    return Err(IpcError::ambiguous(
                        ambiguous_message(candidates, self.locale),
                        candidates.to_vec(),
                    ));
                }
                return Err(server_error(e));
            }
        };
        Ok(Posted {
            committee: r.event.committee,
            seq: r.event.seq,
            to: r.event.to,
            thread: r.event.thread,
            duplicate: r.duplicate,
            unseen_escalation: None,
        })
    }

    async fn on_ask(
        &self,
        selector: &SessionSelector,
        to: Vec<Address>,
        body: String,
        refs: Option<Refs>,
        id: Option<EventId>,
        request: bool,
    ) -> Result<Posted, IpcError> {
        let session = self.resolve(selector)?;
        let committee = self.bound(&session)?;
        routing::check_ask_recipients(&to, self.locale).map_err(IpcError::bad_request)?;
        // The presider of a nested Senate asks its subagents as their Speaker.
        let me = if self.presides_from_above(&session.agent) {
            Address::Speaker
        } else {
            Address::senator(&session.agent)
        };
        if to.contains(&me) {
            return Err(IpcError::bad_request(match self.locale {
                Locale::Zh => format!("{} 就是这个会话自己；请问队友的 agent", me.in_text()),
                Locale::En => format!(
                    "{} is this session's own agent; ask a teammate's agent instead",
                    me.in_text()
                ),
            }));
        }
        if body.trim().is_empty() {
            return Err(IpcError::bad_request("`body` is empty"));
        }
        let mut refs = refs.unwrap_or_default();
        if refs.branch.is_none() {
            refs.branch = session.branch.clone();
        }
        let kind = if request {
            Kind::Request
        } else {
            Kind::Question
        };
        let mut event = NewEvent::new(committee, kind, me, body)
            .with_to(to)
            .with_refs(refs);
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    /// An API error from a page read, after noting the epoch it names.
    async fn page_error(&self, e: ApiClientError) -> IpcError {
        self.note_epoch(e.epoch()).await;
        server_error(e)
    }

    /// One stored event, read as `GET .../events?after=<seq-1>&limit=1`.
    async fn fetch_event(&self, committee: &CommitteeId, seq: Seq) -> Result<Event, IpcError> {
        let not_found =
            || IpcError::bad_request(format!("there is no event with seq {seq} in {committee}"));
        let Some(before) = seq.0.checked_sub(1) else {
            return Err(not_found());
        };
        let page = match self.api.events(committee, Seq(before), Some(1)).await {
            Ok(page) => page,
            // `seq - 1` is beyond the head: there is no such event.
            Err(e) if e.is_cursor_ahead() => {
                self.note_epoch(e.epoch()).await;
                return Err(not_found());
            }
            Err(e) => return Err(self.page_error(e).await),
        };
        self.note_epoch(page.epoch.as_deref()).await;
        let Some(value) = page.events.into_iter().next() else {
            return Err(not_found());
        };
        match client::decode_event(value) {
            Decoded::Event(e) if e.seq == seq => Ok(*e),
            Decoded::Undecodable {
                seq: Some(found),
                error,
            } if found == seq => Err(IpcError::bad_request(format!(
                "event {seq} in {committee} cannot be read by this bridge ({error})"
            ))),
            _ => Err(not_found()),
        }
    }

    /// The session a request names, if any, and the committee it acts on:
    /// `committee` when given (then no session is needed: the desktop app has
    /// none), else the session's bound one.
    fn session_and_committee(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
    ) -> Result<(Option<Session>, CommitteeId), IpcError> {
        match committee {
            Some(committee) => {
                let named = selector.session_id.is_some() || selector.claude_pid.is_some();
                let session = if named {
                    self.resolve(selector).ok()
                } else {
                    None
                };
                Ok((session, committee))
            }
            None => {
                let session = self.resolve(selector)?;
                let committee = self.bound(&session)?;
                Ok((Some(session), committee))
            }
        }
    }

    /// Who speaks in a request that may come without a session: the
    /// person (`as_person`), else the session's agent, else the agent the
    /// request names or the only one on the committee
    /// ([`Shared::agent_without_session`]).
    fn speaker_for(
        &self,
        session: Option<&Session>,
        committee: &CommitteeId,
        acting: &Acting,
    ) -> Result<Address, IpcError> {
        if acting.as_person {
            return Ok(self.person());
        }
        let agent = match session {
            Some(s) => s.agent.clone(),
            None => self.agent_without_session(committee, acting.agent.as_ref())?,
        };
        Ok(Address::senator(&agent))
    }

    async fn on_answer(
        &self,
        selector: &SessionSelector,
        thread: Seq,
        body: String,
        id: Option<EventId>,
        acting: Acting,
    ) -> Result<Posted, IpcError> {
        let (session, committee) =
            self.session_and_committee(selector, acting.committee.clone())?;
        if body.trim().is_empty() {
            return Err(IpcError::bad_request("`body` is empty"));
        }
        let mut root = self.fetch_event(&committee, thread).await?;
        // Given a message inside a thread rather than its root, answer the root.
        if let Some(r) = root.thread.filter(|r| *r != root.seq) {
            root = self.fetch_event(&committee, r).await?;
        }
        // The thread rule: the root's principal if someone spoke for them,
        // else whoever wrote the root.
        let to = root
            .on_behalf_of
            .clone()
            .unwrap_or_else(|| root.from.clone());
        let companion = session.as_ref().is_some_and(Self::is_companion);
        if companion && acting.as_person {
            return Err(IpcError::bad_request(
                "a companion session answers as the agent",
            ));
        }
        if companion
            && lock(&self.companion)
                .as_ref()
                .is_some_and(|c| c.root != root.seq)
        {
            return Err(IpcError::bad_request(
                "a companion session answers only the question it was started for",
            ));
        }
        let from = self.speaker_for(session.as_ref(), &committee, &acting)?;
        let seen = self.seen_before(&committee, &[root.seq])?;
        let mut event = NewEvent::new(committee, Kind::Answer, from, body)
            .to(to)
            .in_thread(root.seq);
        if acting.as_person {
            event = event.with_meta(meta::SEEN, serde_json::json!(seen_in(&seen, root.seq).0));
        }
        if companion {
            event = event.with_meta(meta::COMPANION, serde_json::json!(true));
        }
        if let Some(id) = id {
            event = event.with_id(id);
        }
        let mut posted = self.post(event).await?;
        if companion && let Some(c) = lock(&self.companion).as_mut() {
            c.answered = true;
        }
        if acting.as_person {
            posted.unseen_escalation = self
                .mark_after_speaking(&posted, &[root.seq], &seen)
                .await?;
        }
        Ok(posted)
    }

    /// The root of the thread `thread` is in (a message inside a thread
    /// names its root).
    async fn task_root(&self, committee: &CommitteeId, thread: Seq) -> Result<Event, IpcError> {
        let event = self.fetch_event(committee, thread).await?;
        match event.thread {
            Some(root) if root != event.seq => self.fetch_event(committee, root).await,
            _ => Ok(event),
        }
    }

    fn status_event(
        committee: CommitteeId,
        from: Address,
        root: Seq,
        status: &StatusPayload,
        id: Option<EventId>,
    ) -> NewEvent {
        let event = NewEvent::new(committee, Kind::Status, from, "")
            .in_thread(root)
            .with_meta(meta::STATUS, status.value());
        match id {
            Some(id) => event.with_id(id),
            None => event,
        }
    }

    async fn on_task(
        &self,
        selector: &SessionSelector,
        thread: Seq,
        mut status: StatusPayload,
        id: Option<EventId>,
        acting: Acting,
    ) -> Result<Posted, IpcError> {
        let (session, committee) =
            self.session_and_committee(selector, acting.committee.clone())?;
        if status.takeover.is_some() || status.escalated_to.is_some() {
            return Err(IpcError::bad_request(
                "use escalate to hand a thread to a person; only people take one over",
            ));
        }
        status.check().map_err(IpcError::bad_request)?;
        let root = self.task_root(&committee, thread).await?;
        if let Some(session) = session.as_ref().filter(|_| !acting.as_person) {
            status.session = Some(session.session_id.clone());
            let owner = TaskOwner {
                agent: session.agent.clone(),
                session: session.session_id.clone(),
            };
            self.store
                .set_owner(&committee, root.seq, &owner)
                .map_err(internal)?;
        }
        let from = self.speaker_for(session.as_ref(), &committee, &acting)?;
        let event = Self::status_event(committee, from, root.seq, &status, id);
        self.post(event).await
    }

    /// The session's agent hands a thread to a person: `to`, a handle, or
    /// this bridge's own person.
    async fn on_escalate(
        &self,
        selector: &SessionSelector,
        thread: Seq,
        to: Option<Name>,
        reason: Option<String>,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        let session = self.resolve(selector)?;
        let committee = self.bound(&session)?;
        let to = to.unwrap_or_else(|| self.config.person.clone());
        let root = self.task_root(&committee, thread).await?;
        let mut status = StatusPayload::new(senate_core::StatusState::Suspended);
        status.escalated_to = Some(to);
        status.reason = reason
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty());
        status.check().map_err(IpcError::bad_request)?;
        let event = Self::status_event(
            committee,
            Address::senator(&session.agent),
            root.seq,
            &status,
            id,
        );
        self.post(event).await
    }

    async fn on_take(
        &self,
        selector: &SessionSelector,
        thread: Seq,
        id: Option<EventId>,
        committee: Option<CommitteeId>,
    ) -> Result<Posted, IpcError> {
        let (_, committee) = self.session_and_committee(selector, committee)?;
        let root = self.task_root(&committee, thread).await?;
        // A person takes a thread over; the takeover is the field, and the
        // log stores no words for it (they are rendered when shown).
        let mut status = StatusPayload::new(senate_core::StatusState::Suspended);
        status.takeover = Some(self.config.person.clone());
        let event = Self::status_event(committee.clone(), self.person(), root.seq, &status, id);
        let posted = self.post(event).await?;
        // The stream brings the same status back; cancelling now closes the
        // gap before it arrives, for every agent of this bridge.
        self.store
            .cancel(None, &committee, root.seq, true, "takeover")
            .map_err(internal)?;
        self.mark_seen_all(&committee, &[root.seq], posted.seq)?;
        Ok(posted)
    }

    /// The events of the thread `seq` is in, root first, in seq order, and
    /// the roots of the threads they are in. Pages the log from the root on
    /// and keeps the thread's own events, and those of a thread the Speaker
    /// relayed it into (`meta.relayed_from`: a question put to the Speaker
    /// to pick an agent is answered in the relay's thread).
    async fn thread_events(
        &self,
        committee: &CommitteeId,
        seq: Seq,
    ) -> Result<(Vec<Seq>, Vec<Event>), IpcError> {
        let root = self.task_root(committee, seq).await?;
        let mut roots = vec![root.seq];
        let mut events = vec![root.clone()];
        let mut after = root.seq;
        loop {
            let page = match self.api.events(committee, after, Some(api::MAX_PAGE)).await {
                Ok(page) => page,
                Err(e) => return Err(self.page_error(e).await),
            };
            self.note_epoch(page.epoch.as_deref()).await;
            let Some(last) = page.events.iter().filter_map(client::seq_of).max() else {
                break;
            };
            for value in page.events {
                let Decoded::Event(e) = client::decode_event(value) else {
                    continue;
                };
                let relayed = e.thread.is_none()
                    && e.meta
                        .get(meta::RELAYED_FROM)
                        .and_then(serde_json::Value::as_u64)
                        .is_some_and(|from| roots.contains(&Seq(from)));
                if relayed {
                    roots.push(e.seq);
                    events.push(*e);
                } else if e.thread.is_some_and(|t| roots.contains(&t)) {
                    events.push(*e);
                }
            }
            after = last;
            if !page.has_more {
                break;
            }
        }
        Ok((roots, events))
    }

    /// Everyone but the person who took part in a thread, as recipients of
    /// what the person says in it: its senators (the Speaker only when no
    /// senator is left).
    fn thread_participants(&self, events: &[Event]) -> Vec<Address> {
        let me = self.person();
        let mut out: Vec<Address> = Vec::new();
        for e in events {
            let named = std::iter::once(&e.from)
                .chain(e.on_behalf_of.iter())
                .chain(e.to.iter());
            for a in named {
                if matches!(a, Address::Senator(_)) && *a != me && !out.contains(a) {
                    out.push(a.clone());
                }
            }
        }
        if out.is_empty()
            && events
                .iter()
                .any(|e| e.from == Address::Speaker || e.to.contains(&Address::Speaker))
        {
            out.push(Address::Speaker);
        }
        out
    }

    /// The person speaks, as themself: a question, note or request, in a
    /// thread of their choosing or a new one.
    #[allow(clippy::too_many_arguments)]
    async fn on_say(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
        kind: SayKind,
        mut to: Vec<Address>,
        body: String,
        thread: Option<Seq>,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        let (_, committee) = self.session_and_committee(selector, committee)?;
        if body.trim().is_empty() {
            return Err(IpcError::bad_request("`body` is empty"));
        }
        if let Some(bad) = to.iter().find(|a| {
            !matches!(
                a,
                Address::Senator(_) | Address::AgentOf(_) | Address::Speaker
            )
        }) {
            return Err(IpcError::bad_request(format!(
                "a person writes to a senator, `senator::<person>/*` or `speaker`, not {}",
                bad.in_text()
            )));
        }
        let mut spoke_in = Vec::new();
        let thread = match thread {
            Some(seq) => {
                let (roots, events) = self.thread_events(&committee, seq).await?;
                if to.is_empty() {
                    to = self.thread_participants(&events);
                }
                // Where the agent that took it works: the latest relay's
                // thread when the Speaker relayed it, else its own.
                let last = roots.last().copied();
                spoke_in = roots;
                last
            }
            None => None,
        };
        if to.is_empty() && kind != SayKind::Note {
            return Err(IpcError::bad_request(
                "a question or request needs someone to ask",
            ));
        }
        let seen = self.seen_before(&committee, &spoke_in)?;
        let mut event =
            NewEvent::new(committee, kind.event_kind(), self.person(), body).with_to(to);
        if let Some(root) = thread {
            // What they had been shown of this thread when they spoke: a
            // fact in the log, so an escalation they had not seen is not
            // ended by words written before it.
            event = event
                .in_thread(root)
                .with_meta(meta::SEEN, serde_json::json!(seen_in(&seen, root).0));
        }
        if let Some(id) = id {
            event = event.with_id(id);
        }
        let mut posted = self.post(event).await?;
        posted.unseen_escalation = self.mark_after_speaking(&posted, &spoke_in, &seen).await?;
        Ok(posted)
    }

    /// How far the person had seen each of `roots` before they spoke.
    fn seen_before(
        &self,
        committee: &CommitteeId,
        roots: &[Seq],
    ) -> Result<Vec<(Seq, Seq)>, IpcError> {
        Ok(self
            .store
            .seen(committee)
            .map_err(internal)?
            .into_iter()
            .filter(|(root, _)| roots.contains(root))
            .collect())
    }

    /// The person just spoke in `roots`: replying is reading, so they have
    /// seen each thread up to their words, but when an escalation to them
    /// arrived after what they had seen (`before`) that one was never shown:
    /// the thread's mark stays where it was, so it stays new. Returns the earliest such escalation
    /// still in force after their words, for the command to say so.
    ///
    /// When the server cannot say what is in force, nothing is marked: the
    /// person sees a thread again rather than miss an escalation.
    async fn mark_after_speaking(
        &self,
        posted: &Posted,
        roots: &[Seq],
        before: &[(Seq, Seq)],
    ) -> Result<Option<Seq>, IpcError> {
        let board = match self.api.threads(&posted.committee, None).await {
            Ok(page) => page.threads,
            Err(e) => {
                tracing::debug!("could not tell what the reply left unseen: {e}");
                return Ok(None);
            }
        };
        let me = &self.config.person;
        let mut unseen: Option<Seq> = None;
        for root in roots {
            let crossed = board
                .iter()
                .find(|t| t.root == *root)
                .and_then(|t| t.escalation.as_ref())
                .filter(|e| &e.to == me && e.seq > seen_in(before, *root))
                .map(|e| e.seq);
            match crossed {
                // Only what was shown before they spoke counts as seen: the
                // mark is one number, and anything above it that was not
                // shown (the escalation, and what came with it) stays new.
                Some(esc) => unseen = Some(unseen.map_or(esc, |u| u.min(esc))),
                None => self.mark_seen_all(&posted.committee, &[*root], posted.seq)?,
            }
        }
        Ok(unseen)
    }

    /// The person has seen each of `roots` up to `seq`.
    fn mark_seen_all(
        &self,
        committee: &CommitteeId,
        roots: &[Seq],
        seq: Seq,
    ) -> Result<(), IpcError> {
        for root in roots {
            self.store
                .mark_seen(committee, *root, seq)
                .map_err(internal)?;
        }
        Ok(())
    }

    fn on_seen(
        &self,
        committee: &CommitteeId,
        marks: &[crate::ipc::ThreadSeen],
    ) -> Result<serde_json::Value, IpcError> {
        for m in marks {
            self.store
                .mark_seen(committee, m.thread, m.seen)
                .map_err(internal)?;
        }
        Ok(serde_json::json!({}))
    }

    async fn on_thread(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
        thread: Seq,
        mark_seen: bool,
    ) -> Result<ThreadReply, IpcError> {
        let (_, committee) = self.session_and_committee(selector, committee)?;
        let (roots, mut events) = self.thread_events(&committee, thread).await?;
        events.sort_by_key(|e| e.seq);
        if mark_seen && let Some(last) = events.last() {
            // Showing a thread is reading it: the person has seen all of it
            // (and of the thread it was relayed into).
            self.mark_seen_all(&committee, &roots, last.seq)?;
        }
        Ok(ThreadReply {
            committee,
            root: roots[0],
            events,
        })
    }

    /// Open threads that wait on the calling session's agent (or, without
    /// a session, the named agent or every agent on the committee) or are
    /// escalated to this bridge's person. Another agent's threads are not
    /// in a session's inbox.
    ///
    /// Without a session this is the person's own read: it also lists, and
    /// keeps among the settled ones, the threads that wait on the person
    /// ([`senate_core::ThreadSummary::waits_on_person`]: something said to
    /// them they have not seen), with how far they have seen each. The rule
    /// for an agent's inbox is unchanged.
    async fn on_inbox(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
        agent: Option<Name>,
        all: bool,
    ) -> Result<InboxReply, IpcError> {
        let (session, committee) = self.session_and_committee(selector, committee)?;
        let for_person = session.is_none();
        let agents = match (session, agent) {
            (Some(s), _) => vec![s.agent],
            (None, Some(name)) => vec![self.seating().agent(&name)],
            (None, None) => self.agents_on(&committee),
        };
        let page = self
            .api
            .threads(&committee, None)
            .await
            .map_err(server_error)?;
        let person = &self.config.person;
        let seen: Vec<ThreadSeen> = if for_person {
            self.store
                .seen(&committee)
                .map_err(internal)?
                .into_iter()
                .map(|(thread, seen)| ThreadSeen { thread, seen })
                .collect()
        } else {
            Vec::new()
        };
        let mut reply = InboxReply {
            committee,
            agents,
            person: person.clone(),
            threads: Vec::new(),
            settled: 0,
            seen,
        };
        let mut threads: Vec<_> = page
            .threads
            .into_iter()
            .filter(|t| t.state != senate_core::StatusState::Archived)
            .filter(|t| {
                reply.agents.iter().any(|a| t.is_for(a))
                    || t.escalated_to.as_ref() == Some(person)
                    || for_person && t.directed_to(person).is_some()
            })
            .collect();
        // What waits on no one is left out and counted, unless asked for.
        // What waits on the person only because they have not seen it is
        // not settled, whatever the agents' rule says.
        let mut settled = 0;
        if !all {
            threads.retain(|t| {
                let waits = t.waits().is_some() || for_person && reply.waits_on_person(t).is_some();
                settled += usize::from(!waits);
                waits
            });
        }
        reply.threads = threads;
        reply.settled = settled;
        Ok(reply)
    }

    async fn on_catch_up(
        &self,
        selector: &SessionSelector,
        limit: Option<usize>,
    ) -> Result<CatchUpReply, IpcError> {
        let session = self.resolve(selector)?;
        let committee = self.bound(&session)?;
        let agent = session.agent.clone();
        let me = Address::senator(&agent);
        let limit = limit
            .unwrap_or(catch_up::DEFAULT_LIMIT)
            .clamp(1, api::MAX_PAGE);
        let read_before = self
            .api
            .cursor(&committee, api::CURSOR_READ, Some(&agent))
            .await
            .map_err(server_error)?;
        let page = match self.api.events(&committee, read_before, Some(limit)).await {
            Ok(page) => page,
            Err(e) => return Err(self.page_error(e).await),
        };
        self.note_epoch(page.epoch.as_deref()).await;
        // Read past everything on the page, including events this build
        // cannot show.
        let last = page.events.iter().filter_map(client::seq_of).max();
        let mut events = Vec::with_capacity(page.events.len());
        for value in page.events {
            match client::decode_event(value) {
                Decoded::Event(e) if e.committee == committee => events.push(*e),
                Decoded::Event(e) => tracing::warn!(
                    %committee,
                    seq = %e.seq,
                    other = %e.committee,
                    "catch_up skipped another committee's event"
                ),
                Decoded::Undecodable { seq, error } => tracing::warn!(
                    %committee,
                    seq = ?seq.map(|s| s.0),
                    "catch_up skipped an event this bridge cannot decode: {error}"
                ),
            }
        }
        let items: Vec<CatchUpItem> = catch_up::arrange(events, &me);
        // A companion reads without moving the agent's read position: what
        // it saw is still unread for the developer.
        if Self::is_companion(&session) {
            return Ok(CatchUpReply {
                committee,
                me,
                items,
                has_more: page.has_more,
                read_before,
                read: read_before,
            });
        }
        let read = match last {
            Some(last) if last > read_before => self
                .api
                .put_cursor(&committee, api::CURSOR_READ, Some(&agent), last)
                .await
                .map_err(server_error)?,
            _ => read_before,
        };
        // Directed events the agent has now read need no push.
        let seen: Vec<ItemKey> = items
            .iter()
            .filter(|i| i.directed)
            .map(|i| (agent.clone(), i.event.committee.clone(), i.event.seq))
            .collect();
        self.store
            .mark_read(&seen, &session.session_id)
            .map_err(internal)?;
        Ok(CatchUpReply {
            committee,
            me,
            items,
            has_more: page.has_more,
            read_before,
            read,
        })
    }

    async fn on_join(
        &self,
        selector: &SessionSelector,
        committee: CommitteeId,
    ) -> Result<JoinReply, IpcError> {
        let session = self.resolve(selector)?;
        let warning = match tokio::time::timeout(Duration::from_secs(5), self.api.me()).await {
            Ok(Ok(me)) => {
                self.note_triaged(me.triaged);
                self.note_seating(me.presider.clone());
                if !me.committees.is_empty() && !me.committees.contains(&committee) {
                    let known: Vec<&str> = me.committees.iter().map(CommitteeId::as_str).collect();
                    return Err(IpcError::bad_request(format!(
                        "{} cannot join `{committee}`; the server lists: {}",
                        Address::senator(&session.agent).in_text(),
                        known.join(", ")
                    )));
                }
                None
            }
            Ok(Err(e)) => Some(format!(
                "could not confirm the committee with the server ({e}); joined anyway"
            )),
            Err(_) => Some("the server did not answer in time; joined anyway".to_string()),
        };
        let previous = session.committee().cloned();
        self.store
            .set_joined_committee(&session.session_id, Some(&committee))
            .map_err(internal)?;
        self.committees_changed.notify_one();
        self.work.notify_one();
        Ok(JoinReply {
            session_id: session.session_id,
            agent: session.agent,
            committee,
            previous,
            warning,
        })
    }

    /// The committee a request names, else the calling session's bound one.
    fn target(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
    ) -> Result<CommitteeId, IpcError> {
        match committee {
            Some(committee) => Ok(committee),
            None => {
                let session = self.resolve(selector)?;
                self.bound(&session)
            }
        }
    }

    async fn on_search(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
        query: String,
        limit: Option<usize>,
    ) -> Result<SearchReply, IpcError> {
        let committee = self.target(selector, committee)?;
        let query = query.trim().to_string();
        if query.is_empty() {
            return Err(IpcError::bad_request("`query` is empty"));
        }
        let limit = limit
            .unwrap_or(api::DEFAULT_SEARCH)
            .clamp(1, api::MAX_SEARCH);
        let page = match self.api.search(&committee, &query, Some(limit)).await {
            Ok(page) => page,
            Err(e) => return Err(self.page_error(e).await),
        };
        self.note_epoch(page.epoch.as_deref()).await;
        let mut events = Vec::with_capacity(page.events.len());
        for value in page.events {
            match client::decode_event(value) {
                Decoded::Event(e) if e.committee == committee => events.push(*e),
                Decoded::Event(e) => tracing::warn!(
                    %committee,
                    seq = %e.seq,
                    other = %e.committee,
                    "search skipped another committee's event"
                ),
                Decoded::Undecodable { seq, error } => tracing::warn!(
                    %committee,
                    seq = ?seq.map(|s| s.0),
                    "search skipped an event this bridge cannot decode: {error}"
                ),
            }
        }
        Ok(SearchReply {
            committee,
            query,
            events,
        })
    }

    /// An undirected `note` from the session's agent (or, without a
    /// session, the named or only agent on the committee); with `proposal`,
    /// a decision proposal ([`meta::PROPOSAL`]) for a person to confirm.
    async fn on_post(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
        agent: Option<Name>,
        body: String,
        proposal: bool,
        id: Option<EventId>,
    ) -> Result<Posted, IpcError> {
        let (session, committee) = self.session_and_committee(selector, committee)?;
        if body.trim().is_empty() {
            return Err(IpcError::bad_request("`body` is empty"));
        }
        let acting = Acting {
            committee: None,
            agent,
            as_person: false,
        };
        let from = self.speaker_for(session.as_ref(), &committee, &acting)?;
        let mut event = NewEvent::new(committee, Kind::Note, from, body);
        if proposal {
            event = event.with_meta(meta::PROPOSAL, serde_json::json!(PROPOSAL_DECISION));
        }
        if let Some(id) = id {
            event = event.with_id(id);
        }
        self.post(event).await
    }

    async fn on_brief(
        &self,
        selector: &SessionSelector,
        committee: Option<CommitteeId>,
    ) -> Result<Brief, IpcError> {
        let committee = self.target(selector, committee)?;
        self.api.brief(&committee).await.map_err(server_error)
    }

    fn on_bound(&self, selector: &SessionSelector) -> Result<BoundReply, IpcError> {
        let session = self.resolve(selector)?;
        Ok(BoundReply {
            committee: session.committee().cloned(),
            session_id: session.session_id,
        })
    }
}

// ---- tasks -----------------------------------------------------------------

async fn serve_ipc(
    shared: Arc<Front>,
    listener: UnixListener,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(serve_connection(shared.clone(), stream, shutdown.clone()));
                }
                Err(e) => {
                    tracing::warn!("accepting an IPC connection: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            _ = shutdown.changed() => return,
        }
    }
}

async fn serve_connection(shared: Arc<Front>, stream: UnixStream, shutdown: watch::Receiver<bool>) {
    use tokio::io::AsyncWriteExt;

    let (read, mut write) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read);
    loop {
        let (reply, last) = match ipc::read_line(&mut reader).await {
            Ok(None) => return,
            Ok(Some(line)) => match serde_json::from_str::<Request>(&line) {
                Ok(Request::Watch { app }) => {
                    // An app that shows notifications itself keeps the
                    // daemon quiet while it is connected.
                    let _guard = app.then(|| AppClient::enter(shared.app_clients()));
                    serve_watch(&shared, reader, write, shutdown).await;
                    return;
                }
                Ok(request) => (shared.dispatch(request).await, false),
                Err(e) => (
                    Reply::err(IpcError::bad_request(format!("unreadable request: {e}"))),
                    false,
                ),
            },
            Err(e) => (Reply::err(IpcError::bad_request(e.to_string())), true),
        };
        let mut out = match serde_json::to_string(&reply) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("encoding an IPC reply: {e}");
                return;
            }
        };
        out.push('\n');
        if write.write_all(out.as_bytes()).await.is_err() || last {
            return;
        }
    }
}

/// A `watch` connection: the reply, a snapshot (status, pending
/// approvals), then every [`WatchEvent`] until the client closes or the
/// daemon stops. Anything the client sends is ignored.
async fn serve_watch(
    shared: &Front,
    mut reader: tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    mut write: tokio::net::unix::OwnedWriteHalf,
    mut shutdown: watch::Receiver<bool>,
) {
    use tokio::io::AsyncWriteExt;

    async fn send<T: Serialize>(write: &mut tokio::net::unix::OwnedWriteHalf, value: &T) -> bool {
        let Ok(mut line) = serde_json::to_string(value) else {
            return false;
        };
        line.push('\n');
        write.write_all(line.as_bytes()).await.is_ok()
    }

    // Subscribe before the snapshot, so nothing falls between the two.
    let mut events = shared.watch().subscribe();
    if !send(
        &mut write,
        &Reply::ok(&serde_json::json!({"watching": true})),
    )
    .await
    {
        return;
    }
    let mut snapshot = Vec::new();
    match shared.status() {
        Ok(status) => snapshot.push(WatchEvent::Status(Box::new(status))),
        Err(e) => tracing::debug!("status for a new watcher: {e}"),
    }
    snapshot.extend(
        shared
            .relay()
            .list()
            .into_iter()
            .map(|a| WatchEvent::ApprovalRequested(Box::new(a))),
    );
    for event in &snapshot {
        if !send(&mut write, event).await {
            return;
        }
    }
    loop {
        let event = tokio::select! {
            event = events.recv() => match event {
                Ok(event) => event,
                Err(broadcast::error::RecvError::Lagged(missed)) => WatchEvent::Lagged { missed },
                Err(broadcast::error::RecvError::Closed) => return,
            },
            line = ipc::read_line(&mut reader) => match line {
                Ok(Some(_)) => continue,
                Ok(None) | Err(_) => return,
            },
            _ = shutdown.changed() => return,
        };
        if !send(&mut write, &event).await {
            return;
        }
    }
}

async fn supervise(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    loop {
        shared.reconcile();
        tokio::select! {
            _ = shared.committees_changed.notified() => {}
            _ = shutdown.changed() => break,
        }
    }
    let tasks = shared.stop_all();
    for task in tasks {
        if tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .is_err()
        {
            tracing::warn!("a stream worker did not stop in time");
        }
    }
}

async fn batcher(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    // When a session held back by spacing or a retry becomes ready.
    let mut due: Option<Instant> = None;
    loop {
        let ready = async {
            match due {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        let woken = tokio::select! {
            _ = shared.work.notified() => true,
            _ = ready => false,
            _ = shutdown.changed() => return,
        };
        if woken {
            // Close the batch once directed events stop arriving, or at the cap.
            let deadline = Instant::now() + shared.timing.batch_max_wait;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(shared.timing.batch_window) => break,
                    _ = shared.work.notified() => {
                        if Instant::now() >= deadline {
                            break;
                        }
                    }
                    _ = shutdown.changed() => return,
                }
            }
        }
        due = match shared.delivery_pass().await {
            Ok(due) => due,
            Err(e) => {
                tracing::warn!("delivery pass failed: {e:#}");
                None
            }
        };
    }
}

async fn checkpointer(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(shared.timing.checkpoint_poll);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => shared.poll_checkpoints().await,
            _ = shutdown.changed() => return,
        }
    }
}

/// The worktree root a session's checkpoints are about.
async fn worktree(session: &Session) -> Option<PathBuf> {
    match &session.git_root {
        Some(root) => Some(root.clone()),
        None => checkpoint::toplevel(&session.cwd).await,
    }
}

/// The one line a session is told when its worktree's branch changed under
/// it: where it is bound now.
fn rebind_note(locale: Locale, branch: Option<&str>, committee: Option<&CommitteeId>) -> String {
    let (zh_at, en_at) = match branch {
        Some(b) => (
            format!("分支已切换到 `{b}`"),
            format!("The branch changed to `{b}`"),
        ),
        None => (
            "HEAD 已分离（detached）".to_string(),
            "HEAD is now detached".to_string(),
        ),
    };
    match (locale, committee) {
        (Locale::Zh, Some(c)) => format!("{zh_at}：这个会话现在绑定到 {c}。"),
        (Locale::En, Some(c)) => format!("{en_at}: this session is now bound to {c}."),
        (Locale::Zh, None) => format!(
            "{zh_at}：这个会话不再绑定任何委员会（这个分支没有保存的绑定；\
             `senate join <committee>` 可以绑定）。"
        ),
        (Locale::En, None) => format!(
            "{en_at}: this session is no longer bound to a committee (nothing is saved for \
             this branch; `senate join <committee>` binds it)."
        ),
    }
}

/// A checkpoint's body when the agent gave none: the first line of
/// `PROGRESS.md` that says something, else where the worktree is.
fn default_summary(payload: &CheckpointPayload) -> String {
    let line = payload.files.progress.as_deref().and_then(|p| {
        p.lines()
            .map(|l| l.trim().trim_start_matches(['#', '-', '*', ' ']).trim())
            .find(|l| !l.is_empty())
            .map(|l| format::truncate_chars(l, 120).0)
    });
    line.unwrap_or_else(|| {
        format!(
            "checkpoint · {} · {}",
            payload.trigger.as_str(),
            payload.at()
        )
    })
}

async fn sweeper(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let mut tick = tokio::time::interval(shared.timing.sweep);
    tick.tick().await;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                shared.sweep_once();
                shared.reconcile_approvals();
                shared.publish_status();
            }
            _ = shutdown.changed() => return,
        }
    }
}

/// Who a request that may come without a session acts as.
#[derive(Debug, Clone, Default)]
struct Acting {
    /// Act on this committee, with or without a session.
    committee: Option<CommitteeId>,
    /// Without a session: which of this bridge's agents.
    agent: Option<Name>,
    /// As the person rather than an agent.
    as_person: bool,
}

/// The refusal an agent reads when `senator::<person>/*` names a person with
/// several agents: the candidates as mentions, and what to do.
pub fn ambiguous_message(candidates: &[SenatorPath], locale: Locale) -> String {
    let list = candidates
        .iter()
        .map(|c| Address::senator(c).in_text())
        .collect::<Vec<_>>()
        .join(", ");
    let owner = candidates
        .first()
        .map(|c| Address::senior(c.owner()).in_text())
        .unwrap_or_default();
    match locale {
        Locale::Zh => {
            format!("{owner} 在这里有好几个 agent：{list}。在 `to` 里写明其中一个，再问一次。")
        }
        Locale::En => format!(
            "{owner} has several agents here: {list}. Name one of them in `to` and ask again."
        ),
    }
}

/// Whether `event` is the Speaker's answer for `me` given while it had
/// taken over for it (`meta.takeover`): the thread is answered, and the
/// hand-back note lists it when that agent returns. A provisional answer is
/// no answer. An answer for another agent of the same person does not count.
fn answered_for_me_in_takeover(event: &Event, me: &SenatorPath) -> bool {
    event.kind == Kind::Answer
        && event.from == Address::Speaker
        && event.on_behalf_of.as_ref() == Some(&Address::senator(me))
        && event.meta.get(meta::TAKEOVER).and_then(|v| v.as_bool()) == Some(true)
        && !is_provisional(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_to_the_cap() {
        let t = Timing::default();
        let delays: Vec<u64> = (1..=7).map(|n| retry_delay(&t, n).as_secs()).collect();
        assert_eq!(delays, [5, 10, 20, 40, 60, 60, 60]);
    }

    #[test]
    fn the_brief_context_fits_its_bound_or_is_left_out() {
        let f = CommitteeId::new("refund-v2").unwrap();
        let pm: Address = "senator::pm".parse().unwrap();
        let events: Vec<Event> = (1..=40)
            .map(|i| {
                NewEvent::new(f.clone(), Kind::Decision, pm.clone(), "x".repeat(100))
                    .into_event(Seq(i), chrono::Utc::now())
            })
            .collect();
        let brief = Brief::from_events(f, &events);
        for locale in Locale::ALL {
            for max in [600, 1_000, 6_000] {
                let text = brief_context(&brief, max, locale).unwrap();
                assert!(format::char_len(&text) <= max, "{max}");
                // The brief itself is core's, in the same language.
                let md = brief.to_markdown(locale, 100_000);
                let title = md.lines().next().unwrap();
                assert!(text.contains(title), "{title} in {text}");
            }
            assert_eq!(brief_context(&brief, 300, locale), None);
        }
        let zh = brief_context(&brief, 6_000, Locale::Zh).unwrap();
        assert!(zh.starts_with("[senate] 下面 `refund-v2` 的简报"));
        let en = brief_context(&brief, 6_000, Locale::En).unwrap();
        assert!(en.starts_with("[senate] The brief of `refund-v2` below"));
    }

    #[test]
    fn the_orientation_names_the_agent_and_the_senate() {
        let c = CommitteeId::new("refund-v2").unwrap();
        let agent: SenatorPath = "alice/codex".parse().unwrap();
        let en = orientation(&c, &agent, None, false, Locale::En);
        assert!(en.contains("committee `refund-v2` of the top Senate as [senator::alice/codex]"));
        let presider: SenatorPath = "alice/claude".parse().unwrap();
        let nested: SenatorPath = "alice/claude/reviewer".parse().unwrap();
        let zh = orientation(&c, &nested, Some(&presider), false, Locale::Zh);
        assert!(
            zh.contains("已加入 由 [senator::alice/claude] 主持的 Senate 的委员会 `refund-v2`，身份是 [senator::alice/claude/reviewer]"),
            "{zh}"
        );
        for text in [en, zh] {
            assert!(!text.to_lowercase().contains(&["fei", "shu"].concat()));
        }
    }

    #[test]
    fn the_orientation_says_the_person_can_speak_from_a_terminal() {
        let c = CommitteeId::new("refund-v2").unwrap();
        let agent: SenatorPath = "alice/codex".parse().unwrap();
        let en = orientation(&c, &agent, None, false, Locale::En);
        assert!(
            en.contains("`senate ask`, `senate tell` and `senate reply`"),
            "{en}"
        );
        let zh = orientation(&c, &agent, None, false, Locale::Zh);
        assert!(
            zh.contains("`senate ask`、`senate tell`、`senate reply`"),
            "{zh}"
        );
    }

    #[test]
    fn the_orientation_mentions_the_chat_only_when_a_group_is_bound() {
        let c = CommitteeId::new("refund-v2").unwrap();
        let agent: SenatorPath = "alice/codex".parse().unwrap();
        let line = |chat, l| orientation(&c, &agent, None, chat, l);
        assert!(
            line(true, Locale::En)
                .contains("Speaker, and people in the committee's chat reach you")
        );
        assert!(
            line(false, Locale::En).contains("Teammates' agents and the Speaker reach you here")
        );
        assert!(!line(false, Locale::En).contains("chat"));
        assert!(line(true, Locale::Zh).contains("Speaker 和委员会聊天群里的人会以"));
        assert!(line(false, Locale::Zh).contains("队友的 agent 和 Speaker 会以"));
        assert!(!line(false, Locale::Zh).contains("聊天"));
    }

    #[test]
    fn an_ambiguous_recipient_reads_as_a_choice() {
        let candidates: Vec<SenatorPath> =
            vec!["bob/claude".parse().unwrap(), "bob/codex".parse().unwrap()];
        assert_eq!(
            ambiguous_message(&candidates, Locale::En),
            "[senator::bob] has several agents here: [senator::bob/claude], \
             [senator::bob/codex]. Name one of them in `to` and ask again."
        );
        assert!(ambiguous_message(&candidates, Locale::Zh).starts_with(
            "[senator::bob] 在这里有好几个 agent：[senator::bob/claude], [senator::bob/codex]"
        ));
    }
}
