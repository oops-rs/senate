use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};
use senate_core::api::Heartbeat;
use senate_core::{Appended, CommitteeId, Event, EventId, MirrorRef, NewEvent, SenatorName, Seq};

use crate::error::LogError;
use crate::memory::MemoryStore;
use crate::pg::PgStore;
use crate::sqlite::SqliteStore;

/// The `database_url` that selects [`MemoryStore`].
pub const MEMORY_URL: &str = "memory:";

/// The `database_url` prefix that selects [`SqliteStore`]: `sqlite:<path>`.
pub const SQLITE_PREFIX: &str = "sqlite:";

/// The latest heartbeat one agent's bridge sent for a committee. It is kept
/// per agent ([`Heartbeat::agent`], the full path), so two agents of one
/// person each have their own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatRecord {
    pub committee: CommitteeId,
    pub agent: SenatorName,
    pub heartbeat: Heartbeat,
    pub at: DateTime<Utc>,
}

/// Whose a named cursor is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CursorOwner {
    /// An in-process, server-side consumer (the Speaker, a gateway).
    Hub,
    /// One senator, by full path: `claude` and `codex` have
    /// separate cursors, and neither is `wendell`'s own.
    Senator(SenatorName),
}

impl CursorOwner {
    /// The text a store keeps. A senator's is prefixed so no path can equal
    /// the hub's.
    pub fn key(&self) -> String {
        match self {
            Self::Hub => "hub".to_string(),
            Self::Senator(path) => format!("senator::{path}"),
        }
    }
}

impl From<SenatorName> for CursorOwner {
    fn from(path: SenatorName) -> Self {
        Self::Senator(path)
    }
}

impl From<&SenatorName> for CursorOwner {
    fn from(path: &SenatorName) -> Self {
        Self::Senator(path.clone())
    }
}

/// Refuse what the log cannot hold: an event whose `to`, `from` or
/// `on_behalf_of` still names `senator::*`. A stored event always
/// names concrete senators, so whoever accepts the event resolves the
/// wildcard first. Every store calls this before it writes.
pub(crate) fn check_storable(event: &NewEvent) -> Result<(), LogError> {
    if event.is_resolved() {
        Ok(())
    } else {
        Err(LogError::Invalid(format!(
            "event {} still holds a senator::<person>/* address; resolve it before appending",
            event.id
        )))
    }
}

/// The log's storage seam. Object safe: the server holds `Arc<dyn LogStore>`.
///
/// See the crate docs for the semantics every implementation keeps.
#[async_trait]
pub trait LogStore: Send + Sync + 'static {
    /// Store `event` as the committee's next seq, stamped with [`now`]. A known
    /// `(committee, id)` returns the stored event with `duplicate: true`.
    /// An event that is not [`NewEvent::is_resolved`] is [`LogError::Invalid`].
    async fn append(&self, event: NewEvent) -> Result<Appended, LogError>;

    /// Events with `seq > after`, ascending, at most `limit`.
    async fn read(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, LogError>;

    async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, LogError>;

    /// The committee's highest stored seq; [`Seq::ZERO`] when it has no events.
    async fn head(&self, committee: &CommitteeId) -> Result<Seq, LogError>;

    /// Committees with at least one event, sorted.
    async fn committees(&self) -> Result<Vec<CommitteeId>, LogError>;

    /// A named cursor. `owner` scopes it: the agent's full path for a
    /// bridge, [`CursorOwner::Hub`] for in-process consumers.
    async fn cursor(
        &self,
        committee: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
    ) -> Result<Option<Seq>, LogError>;

    /// Overwrite a named cursor (not monotonic: a consumer may rewind).
    async fn set_cursor(
        &self,
        committee: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
        seq: Seq,
    ) -> Result<(), LogError>;

    /// The log's identity: minted once when the log is created and constant
    /// for its life, so a reset log (a new `memory:` process, a recreated
    /// database) is told apart from the old one.
    async fn epoch(&self) -> Result<String, LogError>;

    /// The thread rooted at `root`: the root itself (if stored), then every
    /// event with `thread == root` and `seq > root`, ascending.
    async fn thread(&self, committee: &CommitteeId, root: Seq) -> Result<Vec<Event>, LogError>;

    /// Every `binding` event of every committee: committees in [`LogStore::committees`]
    /// order, each committee's in seq order.
    async fn bindings(&self) -> Result<Vec<Event>, LogError>;

    /// Up to `limit` of the committee's events matching `query`, best first:
    /// first the events whose body contains every term of
    /// [`senate_core::hub::search_terms`] (as
    /// [`senate_core::hub::matches_terms`] decides), newest first; then,
    /// from a store with a text index, near matches by similarity. Terms are
    /// literal text: `%` and `_` are not wildcards. A query with no terms
    /// matches nothing.
    async fn search(
        &self,
        committee: &CommitteeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Event>, LogError>;

    /// Record one outbound copy; first write wins on
    /// `(committee, seq, platform, target)`.
    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), LogError>;

    /// Every recorded copy of one event on `platform`, ordered by `target`.
    async fn mirrors_of_seq(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, LogError>;

    /// The copy that is this platform message (lowest `(committee, seq,
    /// target)` when several claim it).
    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, LogError>;

    /// Keep `heartbeat` as its agent's ([`Heartbeat::agent`]) latest for the
    /// committee.
    async fn record_heartbeat(
        &self,
        committee: &CommitteeId,
        heartbeat: Heartbeat,
        at: DateTime<Utc>,
    ) -> Result<(), LogError>;

    /// The latest heartbeat of every agent that has sent one for the
    /// committee, by agent path. A restarted server restores presence from
    /// these.
    async fn heartbeats(&self, committee: &CommitteeId) -> Result<Vec<HeartbeatRecord>, LogError>;

    /// The latest heartbeat of exactly this agent.
    async fn last_heartbeat(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<HeartbeatRecord>, LogError>;

    /// The registry: what the Senate keeps beside its log, facts that are
    /// no committee's events (a child Senate registered from below, the
    /// secretaries registered for a senator). Every entry whose key starts
    /// with `prefix`, by key. Keys are namespaced (`child:<name>`); values
    /// are JSON.
    ///
    /// The default keeps nothing (a store that wraps another for a test);
    /// every store Senate ships keeps it.
    async fn registry(&self, prefix: &str) -> Result<Vec<(String, serde_json::Value)>, LogError> {
        let _ = prefix;
        Ok(Vec::new())
    }

    /// Read one exact registry key.
    async fn registry_value(&self, key: &str) -> Result<Option<serde_json::Value>, LogError> {
        Ok(self
            .registry(key)
            .await?
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value))
    }

    /// Set (`Some`) or remove (`None`) one registry entry. The default
    /// refuses: it keeps nothing.
    async fn put_registry(
        &self,
        key: &str,
        value: Option<serde_json::Value>,
    ) -> Result<(), LogError> {
        let _ = value;
        Err(LogError::Invalid(format!(
            "this store keeps no registry; cannot set {key}"
        )))
    }
}

/// The log's clock: now, truncated to the microsecond precision Postgres
/// stores, so values round-trip exactly through every store.
pub fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

/// A new epoch: a fresh ULID string (an [`EventId`] is one).
pub(crate) fn mint_epoch() -> String {
    EventId::new().to_string()
}

/// What a log at `database_url` is, found by reading only: it is neither
/// created nor migrated (`senated doctor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inspection {
    /// `memory:`: a log that is gone when senated stops.
    Memory,
    /// No log there yet; senated creates it when it first starts.
    Missing,
    /// A Senate log. `epoch` is `None` before the first start minted one;
    /// `pending` when senated will migrate it when it next starts.
    Log {
        epoch: Option<String>,
        pending: bool,
    },
}

/// Look at the log `database_url` names, reading only. A database that is
/// not Senate's is refused with [`LogError::ForeignDatabase`], as opening
/// it would be.
pub async fn inspect(database_url: &str) -> Result<Inspection, LogError> {
    if database_url == MEMORY_URL {
        return Ok(Inspection::Memory);
    }
    if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
        return PgStore::inspect_url(database_url).await;
    }
    if let Some(path) = database_url.strip_prefix(SQLITE_PREFIX) {
        return SqliteStore::inspect_path(std::path::Path::new(path)).await;
    }
    Err(LogError::Invalid(format!(
        "database_url must be {MEMORY_URL:?}, a postgres:// URL or {SQLITE_PREFIX}<path>"
    )))
}

/// Open the store a `database_url` names:
///
/// - [`MEMORY_URL`] (`memory:`): a fresh [`MemoryStore`];
/// - `postgres://` / `postgresql://`: a [`PgStore`] with its migrations
///   applied;
/// - `sqlite:<path>`: a [`SqliteStore`] on that file, created when missing.
pub async fn connect(database_url: &str) -> Result<Arc<dyn LogStore>, LogError> {
    if database_url == MEMORY_URL {
        return Ok(Arc::new(MemoryStore::new()));
    }
    if database_url.starts_with("postgres://") || database_url.starts_with("postgresql://") {
        return Ok(Arc::new(PgStore::connect(database_url).await?));
    }
    if let Some(path) = database_url.strip_prefix(SQLITE_PREFIX) {
        return Ok(Arc::new(SqliteStore::open(path).await?));
    }
    Err(LogError::Invalid(format!(
        "database_url must be {MEMORY_URL:?}, a postgres:// URL or {SQLITE_PREFIX}<path>"
    )))
}
