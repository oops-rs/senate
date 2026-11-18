//! `state.db`: the daemon's durable memory (sqlite, WAL, `synchronous=FULL`).
//!
//! Everything kept per stream is keyed by the **agent** as well as the
//! committee: a machine seats several agents of one person, each with its
//! own stream, cursor, queue and dropped count, and two agents of one
//! person on one committee never share any of them. The agent is stored as
//! its [`SenatorName`] (`claude`).
//!
//! - `cursors` — per (agent, committee), the highest `seq` the bridge has
//!   durably taken in (`delivered`) and the highest it has reported to the
//!   server. A pair with no row has never been joined (or was reset) and is
//!   seeded before its stream opens.
//! - `epochs` — the log epoch last seen per server. Seqs are only comparable
//!   within one epoch, so a new epoch drops every cursor and queue row
//!   ([`Store::reset_for_epoch`]).
//! - `seating` — per server, the Senate `/v1/me` last said it serves (its
//!   presider), so a restart while the server is down still names agents
//!   the same way.
//! - `sessions` — the session registry ([`Session`]), each with its agent;
//!   `session_ancestors` holds the pids above each session's hooks, for
//!   finding a session by its Claude pid when Claude does not export
//!   `CLAUDE_PID`.
//! - `queue` — directed events awaiting one of that agent's sessions, keyed
//!   by `(agent, committee, seq)`. An event addressed to two agents of the
//!   person is two rows.
//! - `dropped` — per (agent, committee), how many pending items were dropped
//!   by the cap ([`Store::cap_pending`]) and not yet announced in a push.
//! - `person_seen` — per (committee, thread root), the highest `seq` of the
//!   thread the person has seen on this machine (`senate thread`, a reply,
//!   a take, `senate inbox --mark-read`): this machine's view, not a fact of
//!   the log, so nothing is appended for it. It only moves up
//!   ([`Store::mark_seen`]), and is dropped with the cursors when the log's
//!   epoch changes, since seqs of an old log mean nothing in the new one. A
//!   database from before it gets the empty table when it is opened: nothing
//!   is seen yet, so what was answered to the person and never opened shows
//!   once as new.
//! - `verdicts` and `owners` — facts of the log, the same for every agent:
//!   the Speaker's triage verdict per thread root, and the
//!   [`TaskOwner`] (agent and session) that took each thread.
//!
//! Queue items move `pending → inflight → delivered`, or `inflight → queued`
//! (held for the session's hooks) or back to `pending` (no one could take
//! it). Every transition is a conditional update on the current status, so a
//! hook and the batcher racing for one item cannot both deliver it.
//!
//! The cursor and the queue row for an event are written in one transaction
//! ([`Store::ingest`]), so the cursor never passes an event that was not kept.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, params};
use senate_core::{CommitteeId, Event, SenatorName, Seq, TaskOwner, TriageVerdict};
use serde::{Deserialize, Serialize};

use senate_core::EventId;

use crate::agent::AgentSource;
use crate::session::{CommitteeSource, Runtime, Session, SessionState};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS cursors (
    agent     TEXT NOT NULL,
    committee TEXT NOT NULL,
    delivered INTEGER NOT NULL,
    reported  INTEGER NOT NULL DEFAULT 0,
    touched   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (agent, committee)
);
CREATE TABLE IF NOT EXISTS epochs (
    server  TEXT PRIMARY KEY,
    epoch   TEXT NOT NULL,
    seen_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    session_id       TEXT PRIMARY KEY,
    cwd              TEXT NOT NULL,
    git_root         TEXT,
    branch           TEXT,
    inferred_committee TEXT,
    committee_source   TEXT,
    joined_committee   TEXT,
    socket           TEXT,
    token            TEXT,
    claude_pid       INTEGER,
    transcript_path  TEXT,
    state            TEXT NOT NULL,
    started_at       INTEGER NOT NULL,
    last_seen        INTEGER NOT NULL,
    socket_ok        INTEGER,
    runtime          TEXT NOT NULL,
    agent            TEXT NOT NULL,
    agent_source     TEXT NOT NULL,
    host_id          TEXT
);
CREATE TABLE IF NOT EXISTS session_ancestors (
    session_id TEXT NOT NULL,
    pid        INTEGER NOT NULL,
    PRIMARY KEY (session_id, pid)
);
CREATE TABLE IF NOT EXISTS queue (
    agent        TEXT NOT NULL,
    committee    TEXT NOT NULL,
    seq          INTEGER NOT NULL,
    event        TEXT NOT NULL,
    status       TEXT NOT NULL,
    session_id   TEXT,
    via          TEXT,
    enqueued_at  INTEGER NOT NULL,
    delivered_at INTEGER,
    PRIMARY KEY (agent, committee, seq)
);
CREATE INDEX IF NOT EXISTS queue_by_status ON queue (status, agent, committee, seq);
CREATE TABLE IF NOT EXISTS dropped (
    agent     TEXT NOT NULL,
    committee TEXT NOT NULL,
    count     INTEGER NOT NULL,
    PRIMARY KEY (agent, committee)
);
CREATE TABLE IF NOT EXISTS verdicts (
    committee TEXT NOT NULL,
    root    INTEGER NOT NULL,
    verdict TEXT NOT NULL,
    PRIMARY KEY (committee, root)
);
CREATE TABLE IF NOT EXISTS person_seen (
    committee TEXT NOT NULL,
    root      INTEGER NOT NULL,
    seen      INTEGER NOT NULL,
    PRIMARY KEY (committee, root)
);
CREATE TABLE IF NOT EXISTS owners (
    committee  TEXT NOT NULL,
    root       INTEGER NOT NULL,
    agent      TEXT NOT NULL,
    session_id TEXT NOT NULL,
    PRIMARY KEY (committee, root)
);
";

const SESSION_COLS: &str = "session_id, cwd, git_root, branch, inferred_committee, committee_source, \
     joined_committee, socket, token, claude_pid, transcript_path, state, started_at, last_seen, \
     socket_ok, runtime, agent, agent_source";

/// Columns of a queue row, in the order [`item_from_row`] reads them.
const ITEM_COLS: &str = "agent, event, status, session_id, enqueued_at";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("state db: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("state db: {0}")]
    Io(#[from] std::io::Error),
    #[error("state db holds an unreadable value: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemStatus {
    Pending,
    Inflight,
    Queued,
    Delivered,
    /// Never to be pushed: triage parked it on a person, a person took the
    /// thread over, or the Speaker answered it for this agent while it had
    /// taken over. `via` says which (`parked`, `takeover`, `answered`).
    Cancelled,
}

impl ItemStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Inflight => "inflight",
            Self::Queued => "queued",
            Self::Delivered => "delivered",
            Self::Cancelled => "cancelled",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "inflight" => Some(Self::Inflight),
            "queued" => Some(Self::Queued),
            "delivered" => Some(Self::Delivered),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

/// `(agent, committee, seq)`: a queue item's identity.
pub type ItemKey = (SenatorName, CommitteeId, Seq);

#[derive(Debug, Clone, PartialEq)]
pub struct QueueItem {
    /// The agent of this bridge the event is for.
    pub agent: SenatorName,
    pub event: Event,
    pub status: ItemStatus,
    pub session_id: Option<String>,
    /// When this bridge received it (local clock): the triage wait runs from here.
    pub enqueued_at: DateTime<Utc>,
}

impl QueueItem {
    pub fn key(&self) -> ItemKey {
        (
            self.agent.clone(),
            self.event.committee.clone(),
            self.event.seq,
        )
    }
}

/// What a hook tells the registry about a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUpsert {
    pub session_id: String,
    pub cwd: PathBuf,
    pub git_root: Option<PathBuf>,
    pub branch: Option<String>,
    pub inferred_committee: Option<(CommitteeId, CommitteeSource)>,
    pub socket: Option<PathBuf>,
    pub token: Option<String>,
    pub claude_pid: Option<u32>,
    pub transcript_path: Option<String>,
    /// The hook's parent, grandparent, ...; empty keeps what was recorded.
    pub ancestor_pids: Vec<u32>,
    pub runtime: Runtime,
    /// The agent the session speaks as, chosen at every hook.
    pub agent: SenatorName,
    pub agent_source: AgentSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRow {
    pub delivered: Seq,
    pub reported: Seq,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueCounts {
    pub pending: u64,
    pub inflight: u64,
    pub queued: u64,
}

/// What [`Store::reset_for_epoch`] threw away.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResetSummary {
    /// (agent, committee) cursors dropped.
    pub committees: usize,
    /// Queue items not yet delivered.
    pub undelivered: usize,
}

/// An agent that no session has spoken for a long time, with what the
/// state still holds for it ([`Store::idle_agents`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleAgent {
    pub agent: SenatorName,
    pub committees: Vec<CommitteeId>,
    /// Queued items, of any status.
    pub items: usize,
}

pub struct Store {
    conn: Mutex<Connection>,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn ms_to_time(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}

fn verdict_str(v: TriageVerdict) -> &'static str {
    match v {
        TriageVerdict::NeedsHuman => "needs_human",
        TriageVerdict::Agent => "agent",
        TriageVerdict::Unsure => "unsure",
    }
}

fn parse_verdict(s: &str) -> Option<TriageVerdict> {
    match s {
        "needs_human" => Some(TriageVerdict::NeedsHuman),
        "agent" => Some(TriageVerdict::Agent),
        "unsure" => Some(TriageVerdict::Unsure),
        _ => None,
    }
}

fn seq_sql(seq: Seq) -> i64 {
    i64::try_from(seq.0).unwrap_or(i64::MAX)
}

fn seq_from(v: i64) -> Seq {
    Seq(u64::try_from(v).unwrap_or(0))
}

fn count(n: i64) -> usize {
    usize::try_from(n).unwrap_or(0)
}

fn path_text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn conversion(idx: usize, msg: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, msg.into())
}

fn opt_committee(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<CommitteeId>> {
    row.get::<_, Option<String>>(idx)?
        .map(|s| CommitteeId::new(s).map_err(|e| conversion(idx, e.to_string())))
        .transpose()
}

fn committee_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<CommitteeId> {
    opt_committee(row, idx)?.ok_or_else(|| conversion(idx, "missing committee".into()))
}

fn agent_col(row: &Row<'_>, idx: usize) -> rusqlite::Result<SenatorName> {
    let raw: String = row.get(idx)?;
    raw.parse()
        .map_err(|e: senate_core::SenatorNameError| conversion(idx, e.to_string()))
}

fn agent_text(agent: &SenatorName) -> String {
    agent.to_string()
}

fn parse_host_id(id: &str) -> Result<EventId, StoreError> {
    id.parse()
        .map_err(|_| StoreError::Corrupt("invalid host ID".into()))
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    let state: String = row.get(11)?;
    Ok(Session {
        session_id: row.get(0)?,
        cwd: PathBuf::from(row.get::<_, String>(1)?),
        git_root: row.get::<_, Option<String>>(2)?.map(PathBuf::from),
        branch: row.get(3)?,
        inferred_committee: opt_committee(row, 4)?,
        committee_source: row
            .get::<_, Option<String>>(5)?
            .and_then(|s| CommitteeSource::parse(&s)),
        joined_committee: opt_committee(row, 6)?,
        socket: row.get::<_, Option<String>>(7)?.map(PathBuf::from),
        token: row.get(8)?,
        claude_pid: row
            .get::<_, Option<i64>>(9)?
            .and_then(|p| u32::try_from(p).ok()),
        transcript_path: row.get(10)?,
        state: SessionState::parse(&state)
            .ok_or_else(|| conversion(11, format!("unknown session state {state:?}")))?,
        started_at: ms_to_time(row.get(12)?),
        last_seen: ms_to_time(row.get(13)?),
        socket_ok: row.get::<_, Option<i64>>(14)?.map(|v| v != 0),
        runtime: {
            let runtime: String = row.get(15)?;
            Runtime::parse(&runtime)
                .ok_or_else(|| conversion(15, format!("unknown session runtime {runtime:?}")))?
        },
        agent: agent_col(row, 16)?,
        agent_source: {
            let source: String = row.get(17)?;
            AgentSource::parse(&source)
                .ok_or_else(|| conversion(17, format!("unknown agent source {source:?}")))?
        },
    })
}

fn item_from_row(row: &Row<'_>) -> rusqlite::Result<QueueItem> {
    let agent = agent_col(row, 0)?;
    let json: String = row.get(1)?;
    let event: Event = serde_json::from_str(&json).map_err(|e| conversion(1, e.to_string()))?;
    let status: String = row.get(2)?;
    Ok(QueueItem {
        agent,
        event,
        status: ItemStatus::parse(&status)
            .ok_or_else(|| conversion(2, format!("unknown item status {status:?}")))?,
        session_id: row.get(3)?,
        enqueued_at: ms_to_time(row.get(4)?),
    })
}

/// Move `agent`'s cursor on `committee` to `seq`, enqueueing `event` for it
/// first when given. `false` (and nothing written) when `seq` is at or
/// behind the cursor.
fn take(
    tx: &Transaction<'_>,
    agent: &SenatorName,
    committee: &CommitteeId,
    seq: Seq,
    enqueue: Option<&Event>,
) -> Result<bool, StoreError> {
    let agent = agent_text(agent);
    let current: Option<i64> = tx
        .query_row(
            "SELECT delivered FROM cursors WHERE agent = ?1 AND committee = ?2",
            params![agent, committee.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    if current.map(seq_from).unwrap_or(Seq::ZERO) >= seq {
        return Ok(false);
    }
    if let Some(event) = enqueue {
        let json = serde_json::to_string(event).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        tx.execute(
            "INSERT OR IGNORE INTO queue (agent, committee, seq, event, status, enqueued_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![agent, committee.as_str(), seq_sql(seq), json, now_ms()],
        )?;
    }
    tx.execute(
        "INSERT INTO cursors (agent, committee, delivered, touched) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(agent, committee) DO UPDATE SET
             delivered = excluded.delivered, touched = excluded.touched",
        params![agent, committee.as_str(), seq_sql(seq), now_ms()],
    )?;
    Ok(true)
}

/// The `user_version` of a state database whose agents are flat names.
pub const FLAT_NAMES_VERSION: i64 = 1;

/// The tables whose `agent` column names an agent.
const AGENT_TABLES: [&str; 5] = ["cursors", "sessions", "queue", "dropped", "owners"];

/// Convert a state database written when senators were paths
/// (`wendell/claude`) to flat names, once, recorded as `user_version` 1.
///
/// - Every `agent` column (cursors, sessions, queued items, drop counts,
///   task owners) loses its person: `wendell/claude` → `claude`. Queued
///   items keep their place and status; the event each holds is converted
///   like the log's ([`senate_core::flatten`]). Read marks (`person_seen`)
///   are keyed by committee and thread and are kept as they are.
/// - Rows of an agent nested deeper (`wendell/claude/reviewer`) belonged to
///   a Senate nested under an agent, which is no longer hosted: they are
///   dropped, and how many is logged.
/// - `seating` (where the bridge last sat, a cache) is dropped.
///
/// Before anything changes, the file is copied beside itself
/// (`state.db.pre-flat-names`, never removed). Everything happens in one
/// transaction: a crash leaves the old database. A new database is created
/// at version 1.
fn flatten_names(conn: &mut Connection, path: Option<&Path>) -> Result<(), StoreError> {
    use senate_core::flatten::Flattener;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version >= FLAT_NAMES_VERSION {
        return Ok(());
    }
    let exists = |conn: &Connection, table: &str| -> rusqlite::Result<bool> {
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
            .exists([table])
    };
    let mut old = false;
    for table in AGENT_TABLES {
        if exists(conn, table)? {
            old |= conn
                .prepare(&format!(
                    "SELECT 1 FROM {table} WHERE agent LIKE '%/%' LIMIT 1"
                ))?
                .exists([])?;
        }
    }
    if old && let Some(path) = path {
        let base = format!("{}.pre-flat-names", path.display());
        let mut target = std::path::PathBuf::from(&base);
        let mut n = 1;
        while target.exists() {
            target = std::path::PathBuf::from(format!("{base}.{n}"));
            n += 1;
        }
        conn.execute("VACUUM INTO ?1", [target.to_string_lossy().as_ref()])?;
        tracing::info!(copy = %target.display(), "saved state.db before converting its names");
    }
    let tx = conn.transaction()?;
    if old {
        let mut dropped = 0usize;
        for table in AGENT_TABLES {
            if !exists(&tx, table)? {
                continue;
            }
            let agents: Vec<String> = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT DISTINCT agent FROM {table} WHERE agent LIKE '%/%'"
                ))?;
                let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for agent in agents {
                if agent.matches('/').count() > 1 {
                    dropped +=
                        tx.execute(&format!("DELETE FROM {table} WHERE agent = ?1"), [&agent])?;
                    continue;
                }
                let flat = Flattener::agent(&agent);
                tx.execute(
                    &format!("UPDATE OR REPLACE {table} SET agent = ?1 WHERE agent = ?2"),
                    params![flat, agent],
                )?;
            }
        }
        if exists(&tx, "queue")? {
            let items: Vec<(String, String, i64, String)> = {
                let mut stmt = tx.prepare("SELECT agent, committee, seq, event FROM queue")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            for (agent, committee, seq, text) in items {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
                    continue;
                };
                let flat = Flattener::envelope(&value);
                if flat != value {
                    tx.execute(
                        "UPDATE queue SET event = ?1 WHERE agent = ?2 AND committee = ?3 AND seq = ?4",
                        params![flat.to_string(), agent, committee, seq],
                    )?;
                }
            }
        }
        if exists(&tx, "seating")? {
            tx.execute("DROP TABLE seating", [])?;
        }
        tracing::info!(
            dropped,
            "converted state.db to flat senator names (rows of agents nested under an agent \
             were dropped; the cache of where the bridge sat was reset)"
        );
    }
    tx.pragma_update(None, "user_version", FLAT_NAMES_VERSION)?;
    tx.commit()?;
    Ok(())
}

impl Store {
    /// Open (creating if needed) the daemon's database, mode 0600.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Self::init_at(conn, Some(path))
    }

    /// For `status` while the daemon is down.
    pub fn open_read_only(path: &Path) -> Result<Self, StoreError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(2))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StoreError> {
        Self::init_at(conn, None)
    }

    /// Open the database at `path`, converting one written when senators
    /// were paths first ([`flatten_names`]).
    fn init_at(mut conn: Connection, path: Option<&Path>) -> Result<Self, StoreError> {
        conn.busy_timeout(Duration::from_secs(5))?;
        flatten_names(&mut conn, path)?;
        // journal_mode answers with a row, so it is a query, not an update.
        let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.execute_batch(SCHEMA)?;
        // A database from before cursors were dated: they were seen now.
        let dated = conn
            .prepare("SELECT 1 FROM pragma_table_info('cursors') WHERE name = 'touched'")?
            .exists([])?;
        if !dated {
            conn.execute_batch(
                "ALTER TABLE cursors ADD COLUMN touched INTEGER NOT NULL DEFAULT 0",
            )?;
            conn.execute("UPDATE cursors SET touched = ?1", [now_ms()])?;
        }
        let hosted = conn
            .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'host_id'")?
            .exists([])?;
        if !hosted {
            conn.execute("ALTER TABLE sessions ADD COLUMN host_id TEXT", [])?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // A panic while holding the lock leaves sqlite itself consistent
        // (statements are atomic), so a poisoned guard is still usable.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- cursors ---------------------------------------------------------

    pub fn cursor(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
    ) -> Result<CursorRow, StoreError> {
        let row = self
            .conn()
            .query_row(
                "SELECT delivered, reported FROM cursors WHERE agent = ?1 AND committee = ?2",
                params![agent_text(agent), committee.as_str()],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?;
        Ok(row.map_or(
            CursorRow {
                delivered: Seq::ZERO,
                reported: Seq::ZERO,
            },
            |(d, r)| CursorRow {
                delivered: seq_from(d),
                reported: seq_from(r),
            },
        ))
    }

    pub fn delivered_cursor(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
    ) -> Result<Seq, StoreError> {
        Ok(self.cursor(agent, committee)?.delivered)
    }

    /// Whether `agent` has a local cursor on this committee (it was joined
    /// before and not reset since).
    pub fn has_cursor(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
    ) -> Result<bool, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM cursors WHERE agent = ?1 AND committee = ?2",
                params![agent_text(agent), committee.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// First join: start `agent`'s cursor on the committee at `delivered`.
    /// Does nothing (and returns `false`) when it already has one.
    pub fn seed_cursor(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
        delivered: Seq,
        reported: Seq,
    ) -> Result<bool, StoreError> {
        Ok(self.conn().execute(
            "INSERT INTO cursors (agent, committee, delivered, reported, touched)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(agent, committee) DO NOTHING",
            params![
                agent_text(agent),
                committee.as_str(),
                seq_sql(delivered),
                seq_sql(reported),
                now_ms()
            ],
        )? > 0)
    }

    /// Every cursor, by agent then committee.
    pub fn cursors(&self) -> Result<Vec<(SenatorName, CommitteeId, CursorRow)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT agent, committee, delivered, reported FROM cursors ORDER BY agent, committee",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                agent_col(r, 0)?,
                committee_col(r, 1)?,
                CursorRow {
                    delivered: seq_from(r.get(2)?),
                    reported: seq_from(r.get(3)?),
                },
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mark_reported(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
        seq: Seq,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE cursors SET reported = MAX(reported, ?3), touched = ?4
             WHERE agent = ?1 AND committee = ?2",
            params![
                agent_text(agent),
                committee.as_str(),
                seq_sql(seq),
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// Take in one event from `agent`'s stream: enqueue it for that agent if
    /// `directed`, then move the agent's cursor to it, atomically. Returns
    /// `false` (and changes nothing) when the event is at or behind the
    /// cursor.
    pub fn ingest(
        &self,
        agent: &SenatorName,
        event: &Event,
        directed: bool,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let moved = take(
            &tx,
            agent,
            &event.committee,
            event.seq,
            directed.then_some(event),
        )?;
        tx.commit()?;
        Ok(moved)
    }

    /// Queue `event` for `agent` without touching its cursor: an item the
    /// agent is to see though it was not addressed to it (a question it
    /// covers as its principal's secretary). `false` when the agent has the
    /// item already, in whatever state: an item pushed, or withdrawn, is
    /// never queued again.
    pub fn enqueue(&self, agent: &SenatorName, event: &Event) -> Result<bool, StoreError> {
        let json = serde_json::to_string(event).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        let added = self.conn().execute(
            "INSERT OR IGNORE INTO queue (agent, committee, seq, event, status, enqueued_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
            params![
                agent_text(agent),
                event.committee.as_str(),
                seq_sql(event.seq),
                json,
                now_ms()
            ],
        )?;
        Ok(added > 0)
    }

    /// Move `agent`'s cursor past `seq` without keeping anything (an event
    /// this build cannot decode). `false` when `seq` is at or behind it.
    pub fn advance(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
        seq: Seq,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let moved = take(&tx, agent, committee, seq, None)?;
        tx.commit()?;
        Ok(moved)
    }

    // ---- log epochs ------------------------------------------------------

    /// The epoch last seen from `server`.
    pub fn epoch(&self, server: &str) -> Result<Option<String>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT epoch FROM epochs WHERE server = ?1",
                [server],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Whether an epoch is recorded for any server other than `server` (the
    /// cursors then belong to that other server).
    pub fn has_other_epochs(&self, server: &str) -> Result<bool, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT 1 FROM epochs WHERE server != ?1 LIMIT 1",
                [server],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    /// Record `server`'s epoch without touching anything else.
    pub fn set_epoch(&self, server: &str, epoch: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO epochs (server, epoch, seen_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(server) DO UPDATE SET epoch = excluded.epoch, seen_at = excluded.seen_at",
            params![server, epoch, now_ms()],
        )?;
        Ok(())
    }

    /// The log was reset: forget every cursor, every queue row (their seqs
    /// name events of the old log), every dropped count and every other
    /// server's epoch, and record `epoch` for `server`. One transaction.
    pub fn reset_for_epoch(&self, server: &str, epoch: &str) -> Result<ResetSummary, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let committees: i64 = tx.query_row("SELECT COUNT(*) FROM cursors", [], |r| r.get(0))?;
        let undelivered: i64 = tx.query_row(
            "SELECT COUNT(*) FROM queue WHERE status NOT IN ('delivered', 'cancelled')",
            [],
            |r| r.get(0),
        )?;
        tx.execute_batch(
            "DELETE FROM cursors; DELETE FROM queue; DELETE FROM dropped; DELETE FROM epochs;
             DELETE FROM verdicts; DELETE FROM owners; DELETE FROM person_seen;",
        )?;
        tx.execute(
            "INSERT INTO epochs (server, epoch, seen_at) VALUES (?1, ?2, ?3)",
            params![server, epoch, now_ms()],
        )?;
        tx.commit()?;
        Ok(ResetSummary {
            committees: count(committees),
            undelivered: count(undelivered),
        })
    }

    /// `agent`'s stream found the committee's log reset: forget that
    /// agent's cursor, queue rows and dropped count there, and the facts read
    /// from the old log (verdicts, owners), which are the same for every
    /// agent and are learned again. Another agent's cursor is its own stream's
    /// to reset. Returns how many undelivered items were dropped.
    pub fn reset_committee(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
    ) -> Result<usize, StoreError> {
        let agent = agent_text(agent);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let undelivered: i64 = tx.query_row(
            "SELECT COUNT(*) FROM queue WHERE agent = ?1 AND committee = ?2
               AND status NOT IN ('delivered', 'cancelled')",
            params![agent, committee.as_str()],
            |r| r.get(0),
        )?;
        for table in ["queue", "cursors", "dropped"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE agent = ?1 AND committee = ?2"),
                params![agent, committee.as_str()],
            )?;
        }
        for table in ["verdicts", "owners", "person_seen"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE committee = ?1"),
                [committee.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(count(undelivered))
    }

    // ---- sessions --------------------------------------------------------

    /// Register a session or refresh it. Marks it live and active now. Keeps
    /// an earlier explicit join, and keeps socket/token/pid/ancestors when
    /// the hook did not see them this time. Each agy hook supplies the
    /// current launch proof, or clears it if the launch has no proof.
    pub fn upsert_session(&self, s: &SessionUpsert) -> Result<Session, StoreError> {
        self.upsert(s, None)
    }

    pub(crate) fn register_hosted_session(
        &self,
        s: &SessionUpsert,
        id: &EventId,
        committee: &CommitteeId,
        cursor: Option<Seq>,
    ) -> Result<Session, StoreError> {
        self.upsert(s, Some((id, committee, cursor)))
    }

    pub(crate) fn session_host(&self, session_id: &str) -> Result<Option<EventId>, StoreError> {
        let id: Option<Option<String>> = self
            .conn()
            .query_row(
                "SELECT host_id FROM sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?;
        id.flatten().as_deref().map(parse_host_id).transpose()
    }

    pub(crate) fn clear_session_host(
        &self,
        session_id: &str,
        id: &EventId,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sessions SET host_id = NULL WHERE session_id = ?1 AND host_id = ?2",
            params![session_id, id.to_string()],
        )?;
        Ok(())
    }

    fn upsert(
        &self,
        s: &SessionUpsert,
        hosted: Option<(&EventId, &CommitteeId, Option<Seq>)>,
    ) -> Result<Session, StoreError> {
        let now = now_ms();
        let (inferred, source) = match &s.inferred_committee {
            Some((f, src)) => (Some(f.as_str().to_string()), Some(src.as_str())),
            None => (None, None),
        };
        {
            let mut conn = self.conn();
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO sessions (session_id, cwd, git_root, branch, inferred_committee,
                     committee_source, socket, token, claude_pid, transcript_path, state,
                     started_at, last_seen, runtime, agent, agent_source, host_id, joined_committee)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'live', ?11, ?11, ?12, ?13, ?14,
                     ?15, ?16)
                 ON CONFLICT(session_id) DO UPDATE SET
                     runtime = excluded.runtime,
                     agent = excluded.agent,
                     agent_source = excluded.agent_source,
                     cwd = excluded.cwd,
                     git_root = excluded.git_root,
                     branch = excluded.branch,
                     inferred_committee = excluded.inferred_committee,
                     committee_source = excluded.committee_source,
                     socket = CASE WHEN ?15 IS NOT NULL AND excluded.runtime = 'claude' THEN excluded.socket
                                   ELSE COALESCE(excluded.socket, sessions.socket) END,
                     token = CASE WHEN (?15 IS NOT NULL AND excluded.runtime = 'claude') OR excluded.runtime = 'agy' THEN excluded.token
                                  ELSE COALESCE(excluded.token, sessions.token) END,
                     claude_pid = CASE WHEN ?15 IS NOT NULL AND excluded.runtime = 'claude' THEN excluded.claude_pid
                                       ELSE COALESCE(excluded.claude_pid, sessions.claude_pid) END,
                     transcript_path = COALESCE(excluded.transcript_path, sessions.transcript_path),
                     socket_ok = CASE WHEN ?15 IS NULL AND sessions.state = 'live'
                                      THEN sessions.socket_ok END,
                     host_id = COALESCE(excluded.host_id, sessions.host_id),
                     joined_committee = COALESCE(excluded.joined_committee, sessions.joined_committee),
                     state = 'live',
                     last_seen = excluded.last_seen",
                params![
                    s.session_id,
                    path_text(&s.cwd),
                    s.git_root.as_deref().map(path_text),
                    s.branch,
                    inferred,
                    source,
                    s.socket.as_deref().map(path_text),
                    s.token,
                    s.claude_pid.map(i64::from),
                    s.transcript_path,
                    now,
                    s.runtime.as_str(),
                    agent_text(&s.agent),
                    s.agent_source.as_str(),
                    hosted.map(|(id, _, _)| id.to_string()),
                    hosted.map(|(_, committee, _)| committee.as_str()),
                ],
            )?;
            if (hosted.is_some() && s.runtime == Runtime::Claude) || !s.ancestor_pids.is_empty() {
                tx.execute(
                    "DELETE FROM session_ancestors WHERE session_id = ?1",
                    [&s.session_id],
                )?;
                for pid in &s.ancestor_pids {
                    tx.execute(
                        "INSERT OR IGNORE INTO session_ancestors (session_id, pid) VALUES (?1, ?2)",
                        params![s.session_id, i64::from(*pid)],
                    )?;
                }
            }
            if let Some((_, committee, cursor)) = hosted {
                if let Some(cursor) = cursor {
                    tx.execute(
                        "INSERT OR IGNORE INTO cursors (agent, committee, delivered, reported, touched)
                         VALUES (?1, ?2, ?3, 0, ?4)",
                        params![agent_text(&s.agent), committee.as_str(), seq_sql(cursor), now],
                    )?;
                }
                tx.execute(
                    "UPDATE queue SET status = 'pending', session_id = NULL
                     WHERE session_id = ?1 AND status = 'queued' AND (agent != ?2 OR committee != ?3)",
                    params![s.session_id, s.agent.as_str(), committee.as_str()],
                )?;
            }
            let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE session_id = ?1");
            let session = tx.query_row(&sql, [&s.session_id], session_from_row)?;
            tx.commit()?;
            Ok(session)
        }
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>, StoreError> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions WHERE session_id = ?1");
        Ok(self
            .conn()
            .query_row(&sql, [id], session_from_row)
            .optional()?)
    }

    /// Every session that has not ended, most recently active first.
    pub fn sessions(&self) -> Result<Vec<Session>, StoreError> {
        let sql = format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE state != 'ended' ORDER BY last_seen DESC"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Every session on record, ended ones too (kept until pruned), most
    /// recently active first: what a companion session forks from.
    pub fn all_sessions(&self) -> Result<Vec<Session>, StoreError> {
        let sql = format!("SELECT {SESSION_COLS} FROM sessions ORDER BY last_seen DESC");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], session_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn live_sessions(&self) -> Result<Vec<Session>, StoreError> {
        Ok(self
            .sessions()?
            .into_iter()
            .filter(Session::is_live)
            .collect())
    }

    /// Ids of sessions whose hooks ran below process `pid`.
    pub fn sessions_below(&self, pid: u32) -> Result<Vec<String>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT session_id FROM session_ancestors WHERE pid = ?1")?;
        let rows = stmt.query_map([i64::from(pid)], |r| r.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn touch_session(&self, id: &str) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sessions SET last_seen = ?2 WHERE session_id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_joined_committee(
        &self,
        id: &str,
        committee: Option<&CommitteeId>,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sessions SET joined_committee = ?2, last_seen = ?3 WHERE session_id = ?1",
            params![id, committee.map(CommitteeId::as_str), now_ms()],
        )?;
        Ok(())
    }

    /// Commit the session's explicit choices and return anything held for its old seat
    /// to routing before another hook or delivery can see the new binding.
    pub fn join_session(&self, id: &str, committee: &CommitteeId) -> Result<Session, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let sql = format!(
            "UPDATE sessions SET joined_committee = ?2, last_seen = ?3
             WHERE session_id = ?1 AND state != 'ended' RETURNING {SESSION_COLS}"
        );
        let session = tx.query_row(
            &sql,
            params![id, committee.as_str(), now_ms()],
            session_from_row,
        )?;
        tx.execute(
            "UPDATE queue SET status = 'pending', session_id = NULL
             WHERE session_id = ?1 AND status = 'queued' AND (agent != ?2 OR committee != ?3)",
            params![id, session.agent.as_str(), committee.as_str()],
        )?;
        tx.commit()?;
        Ok(session)
    }

    /// The branch a session is on and the committee its worktree says,
    /// when the worktree changed under it (the branch was switched).
    pub fn set_inferred(
        &self,
        id: &str,
        branch: Option<&str>,
        inferred: Option<&(CommitteeId, CommitteeSource)>,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sessions SET branch = ?2, inferred_committee = ?3, committee_source = ?4
             WHERE session_id = ?1",
            params![
                id,
                branch,
                inferred.map(|(c, _)| c.as_str()),
                inferred.map(|(_, s)| s.as_str())
            ],
        )?;
        Ok(())
    }

    pub fn set_socket_ok(&self, id: &str, ok: bool) -> Result<(), StoreError> {
        self.conn().execute(
            "UPDATE sessions SET socket_ok = ?2 WHERE session_id = ?1",
            params![id, i64::from(ok)],
        )?;
        Ok(())
    }

    /// Mark a session stale or ended and hand its hook-held items back to
    /// routing. Returns whether the state changed.
    pub fn set_session_state(&self, id: &str, state: SessionState) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE sessions SET state = ?2 WHERE session_id = ?1 AND state != ?2",
            params![id, state.as_str()],
        )? > 0;
        if state != SessionState::Live {
            tx.execute(
                "UPDATE queue SET status = 'pending', session_id = NULL
                 WHERE session_id = ?1 AND status = 'queued'",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub(crate) fn mark_host_gone(
        &self,
        session_id: &str,
        id: &EventId,
    ) -> Result<bool, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE sessions SET state = 'stale'
             WHERE session_id = ?1 AND host_id = ?2 AND state = 'live'",
            params![session_id, id.to_string()],
        )? > 0;
        if changed {
            tx.execute(
                "UPDATE queue SET status = 'pending', session_id = NULL
                 WHERE session_id = ?1 AND status = 'queued'",
                [session_id],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Hand items held for `id`'s hooks that are not `agent`'s back to
    /// routing: the session speaks as `agent` now (a pin changed it), and
    /// what was held for its old agent goes to that agent's other sessions.
    pub fn release_held_for_others(
        &self,
        id: &str,
        agent: &SenatorName,
    ) -> Result<usize, StoreError> {
        Ok(self.conn().execute(
            "UPDATE queue SET status = 'pending', session_id = NULL
             WHERE session_id = ?1 AND status = 'queued' AND agent != ?2",
            params![id, agent_text(agent)],
        )?)
    }

    /// Forget ended and stale sessions and delivered items not touched for
    /// `older_than`, and hand items held for a session that is no longer
    /// live back to routing.
    pub fn prune(&self, older_than: Duration) -> Result<(), StoreError> {
        let cutoff = now_ms() - i64::try_from(older_than.as_millis()).unwrap_or(i64::MAX);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM sessions WHERE state IN ('ended', 'stale') AND last_seen < ?1",
            [cutoff],
        )?;
        tx.execute(
            "DELETE FROM session_ancestors
             WHERE session_id NOT IN (SELECT session_id FROM sessions)",
            [],
        )?;
        tx.execute(
            "UPDATE queue SET status = 'pending', session_id = NULL
             WHERE status = 'queued'
               AND session_id NOT IN (SELECT session_id FROM sessions WHERE state = 'live')",
            [],
        )?;
        tx.execute(
            "DELETE FROM queue WHERE status IN ('delivered', 'cancelled')
               AND COALESCE(delivered_at, enqueued_at) < ?1",
            [cutoff],
        )?;
        tx.commit()?;
        Ok(())
    }

    // ---- freeing what has expired -----------------------------------------

    /// Forget these sessions (and their ancestor pids). How many were there.
    pub fn delete_sessions(&self, ids: &[String]) -> Result<usize, StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut gone = 0;
        for id in ids {
            gone += tx.execute("DELETE FROM sessions WHERE session_id = ?1", [id])?;
            tx.execute("DELETE FROM session_ancestors WHERE session_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(gone)
    }

    /// Agents with queued items or cursors and no contact since `cutoff_ms`:
    /// none of their sessions is registered (other than `ignoring`, which
    /// is being forgotten), and no cursor, queue row or session of theirs
    /// is newer. Committees in `keep` are left out.
    pub fn idle_agents(
        &self,
        cutoff_ms: i64,
        ignoring: &std::collections::BTreeSet<String>,
        keep: &std::collections::BTreeSet<CommitteeId>,
    ) -> Result<Vec<IdleAgent>, StoreError> {
        let conn = self.conn();
        let mut agents: std::collections::BTreeMap<String, IdleAgent> =
            std::collections::BTreeMap::new();
        let mut newest: std::collections::BTreeMap<String, i64> = std::collections::BTreeMap::new();
        let mut bump = |agent: &str, at: i64| {
            let e = newest.entry(agent.to_string()).or_insert(0);
            *e = (*e).max(at);
        };
        {
            let mut stmt = conn.prepare("SELECT agent, committee, touched FROM cursors")?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })? {
                let (agent, committee, touched) = row?;
                bump(&agent, touched);
                let Ok(committee) = CommitteeId::new(committee) else {
                    continue;
                };
                if let Ok(path) = agent.parse::<SenatorName>() {
                    agents
                        .entry(agent.clone())
                        .or_insert_with(|| IdleAgent {
                            agent: path,
                            committees: Vec::new(),
                            items: 0,
                        })
                        .committees
                        .push(committee);
                }
            }
        }
        {
            let mut stmt = conn.prepare(
                "SELECT agent, committee, enqueued_at, COALESCE(delivered_at, 0) FROM queue",
            )?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })? {
                let (agent, committee, enqueued, delivered) = row?;
                bump(&agent, enqueued.max(delivered));
                if let (Ok(path), Ok(committee)) =
                    (agent.parse::<SenatorName>(), CommitteeId::new(committee))
                {
                    let entry = agents.entry(agent.clone()).or_insert_with(|| IdleAgent {
                        agent: path,
                        committees: Vec::new(),
                        items: 0,
                    });
                    entry.items += 1;
                    if !entry.committees.contains(&committee) {
                        entry.committees.push(committee);
                    }
                }
            }
        }
        let mut registered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        {
            let mut stmt = conn.prepare("SELECT session_id, agent, last_seen FROM sessions")?;
            for row in stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })? {
                let (id, agent, seen) = row?;
                bump(&agent, seen);
                if !ignoring.contains(&id) {
                    registered.insert(agent);
                }
            }
        }
        Ok(agents
            .into_iter()
            .filter(|(agent, _)| {
                !registered.contains(agent) && newest.get(agent).copied().unwrap_or(0) < cutoff_ms
            })
            .map(|(_, mut idle)| {
                idle.committees.retain(|c| !keep.contains(c));
                idle
            })
            .filter(|idle| !idle.committees.is_empty())
            .collect())
    }

    /// Forget an agent's cursors, queued items and dropped counts on
    /// `committees`. Returns (cursors, items) removed.
    pub fn forget_agent(
        &self,
        agent: &SenatorName,
        committees: &[CommitteeId],
    ) -> Result<(usize, usize), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let (mut cursors, mut items) = (0, 0);
        for committee in committees {
            let args = params![agent_text(agent), committee.as_str()];
            cursors += tx.execute(
                "DELETE FROM cursors WHERE agent = ?1 AND committee = ?2",
                args,
            )?;
            items += tx.execute(
                "DELETE FROM queue WHERE agent = ?1 AND committee = ?2",
                args,
            )?;
            tx.execute(
                "DELETE FROM dropped WHERE agent = ?1 AND committee = ?2",
                args,
            )?;
        }
        tx.commit()?;
        Ok((cursors, items))
    }

    // ---- queue -----------------------------------------------------------

    fn items(
        &self,
        filter: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<QueueItem>, StoreError> {
        let sql =
            format!("SELECT {ITEM_COLS} FROM queue WHERE {filter} ORDER BY agent, committee, seq");
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(args, item_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// What `agent` has waiting to be pushed (pending, or held for hooks).
    pub fn waiting_for(&self, agent: &SenatorName) -> Result<Vec<QueueItem>, StoreError> {
        self.items(
            "agent = ?1 AND status IN ('pending', 'queued')",
            &[&agent_text(agent)],
        )
    }

    /// Items waiting for routing, by agent, committee, then seq.
    pub fn pending(&self) -> Result<Vec<QueueItem>, StoreError> {
        self.items("status = 'pending'", &[])
    }

    /// `agent`'s items on `committee` waiting for routing.
    pub fn pending_for(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
    ) -> Result<Vec<QueueItem>, StoreError> {
        self.items(
            "status = 'pending' AND agent = ?1 AND committee = ?2",
            &[&agent_text(agent), &committee.as_str()],
        )
    }

    /// Items held for this session's hooks.
    pub fn queued_for(&self, session_id: &str) -> Result<Vec<QueueItem>, StoreError> {
        self.items("status = 'queued' AND session_id = ?1", &[&session_id])
    }

    /// Every item in a state other than delivered (for tests and status).
    pub fn undelivered(&self) -> Result<Vec<QueueItem>, StoreError> {
        self.items("status NOT IN ('delivered', 'cancelled')", &[])
    }

    pub fn item(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
        seq: Seq,
    ) -> Result<Option<QueueItem>, StoreError> {
        Ok(self
            .items(
                "agent = ?1 AND committee = ?2 AND seq = ?3",
                &[&agent_text(agent), &committee.as_str(), &seq_sql(seq)],
            )?
            .into_iter()
            .next())
    }

    pub fn local_deliveries(
        &self,
        event: &Event,
    ) -> Result<Vec<crate::ipc::LocalDelivery>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT agent, status, via FROM queue WHERE committee = ?1 AND seq = ?2
             AND json_extract(event, '$.id') = ?3 ORDER BY agent",
        )?;
        let rows = stmt.query_map(
            params![
                event.committee.as_str(),
                seq_sql(event.seq),
                event.id.to_string()
            ],
            |row| {
                let raw: String = row.get(1)?;
                Ok(crate::ipc::LocalDelivery {
                    agent: agent_col(row, 0)?,
                    state: ItemStatus::parse(&raw)
                        .ok_or_else(|| conversion(1, format!("unknown item status {raw}")))?,
                    via: row.get(2)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn counts(&self) -> Result<QueueCounts, StoreError> {
        self.counts_for(None)
    }

    pub fn counts_for(&self, committee: Option<&CommitteeId>) -> Result<QueueCounts, StoreError> {
        let conn = self.conn();
        let sql = match committee {
            Some(_) => {
                "SELECT status, COUNT(*) FROM queue WHERE status IN ('pending','inflight','queued') AND committee=?1 GROUP BY status"
            }
            None => {
                "SELECT status, COUNT(*) FROM queue WHERE status IN ('pending','inflight','queued') GROUP BY status"
            }
        };
        let mut stmt = conn.prepare(sql)?;
        let params = rusqlite::params_from_iter(committee.map(CommitteeId::as_str));
        let rows = stmt.query_map(params, |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        let mut counts = QueueCounts::default();
        for row in rows {
            let (status, n) = row?;
            let n = u64::try_from(n).unwrap_or(0);
            match ItemStatus::parse(&status) {
                Some(ItemStatus::Pending) => counts.pending = n,
                Some(ItemStatus::Inflight) => counts.inflight = n,
                Some(ItemStatus::Queued) => counts.queued = n,
                _ => {}
            }
        }
        Ok(counts)
    }

    /// Conditional move of `keys` to `to`; returns the keys that moved.
    /// `where_extra` narrows which current rows may move.
    fn transition(
        &self,
        keys: &[ItemKey],
        to: ItemStatus,
        session_id: Option<&str>,
        via: Option<&str>,
        where_extra: &str,
    ) -> Result<Vec<ItemKey>, StoreError> {
        let delivered_at = (to == ItemStatus::Delivered).then(now_ms);
        let sql = format!(
            "UPDATE queue SET status = ?1, session_id = ?2, via = COALESCE(?3, via),
                 delivered_at = COALESCE(?4, delivered_at)
             WHERE agent = ?5 AND committee = ?6 AND seq = ?7 AND ({where_extra})"
        );
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut moved = Vec::new();
        {
            let mut stmt = tx.prepare(&sql)?;
            for key in keys {
                let n = stmt.execute(params![
                    to.as_str(),
                    session_id,
                    via,
                    delivered_at,
                    agent_text(&key.0),
                    key.1.as_str(),
                    seq_sql(key.2),
                ])?;
                if n > 0 {
                    moved.push(key.clone());
                }
            }
        }
        tx.commit()?;
        Ok(moved)
    }

    /// Batcher: `pending → inflight` for `session`.
    pub fn claim_inflight(
        &self,
        keys: &[ItemKey],
        session: &str,
    ) -> Result<Vec<ItemKey>, StoreError> {
        self.transition(
            keys,
            ItemStatus::Inflight,
            Some(session),
            None,
            "status = 'pending'",
        )
    }

    /// Batcher retrying an inbox: pending items, and items held for
    /// `session`'s hooks, go `inflight` for it.
    pub fn claim_inflight_with_held(
        &self,
        keys: &[ItemKey],
        session: &str,
    ) -> Result<Vec<ItemKey>, StoreError> {
        // In the WHERE clause `session_id` is the row's current value and
        // `?2` the session being claimed for.
        self.transition(
            keys,
            ItemStatus::Inflight,
            Some(session),
            None,
            "status = 'pending' OR (status = 'queued' AND session_id = ?2)",
        )
    }

    /// `inflight → delivered` after a push.
    pub fn finish_inflight(
        &self,
        keys: &[ItemKey],
        session: &str,
        via: &str,
    ) -> Result<Vec<ItemKey>, StoreError> {
        self.transition(
            keys,
            ItemStatus::Delivered,
            Some(session),
            Some(via),
            "status = 'inflight'",
        )
    }

    /// `pending → queued`, in one step: held for `session`'s hooks, for a
    /// session nothing can be pushed to. There is no moment when the items
    /// are in flight, so a hook running meanwhile finds them.
    pub fn hold_pending_for_hooks(
        &self,
        keys: &[ItemKey],
        session: &str,
    ) -> Result<Vec<ItemKey>, StoreError> {
        self.transition(
            keys,
            ItemStatus::Queued,
            Some(session),
            None,
            "status = 'pending'",
        )
    }

    /// `inflight → queued`: held for `session`'s hooks.
    pub fn hold_for_hooks(
        &self,
        keys: &[ItemKey],
        session: &str,
    ) -> Result<Vec<ItemKey>, StoreError> {
        self.transition(
            keys,
            ItemStatus::Queued,
            Some(session),
            None,
            "status = 'inflight'",
        )
    }

    /// `inflight → pending`: back to routing.
    pub fn release(&self, keys: &[ItemKey]) -> Result<Vec<ItemKey>, StoreError> {
        self.transition(keys, ItemStatus::Pending, None, None, "status = 'inflight'")
    }

    /// A hook takes items: pending ones, or ones held for this session.
    pub fn claim_for_hook(
        &self,
        keys: &[ItemKey],
        session: &str,
    ) -> Result<Vec<ItemKey>, StoreError> {
        // In the WHERE clause `session_id` is the row's current value and
        // `?2` the session being claimed for.
        self.transition(
            keys,
            ItemStatus::Delivered,
            Some(session),
            Some("hook"),
            "status = 'pending' OR (status = 'queued' AND session_id = ?2)",
        )
    }

    /// `catch_up` showed these to the agent: nothing left to push.
    pub fn mark_read(&self, keys: &[ItemKey], session: &str) -> Result<Vec<ItemKey>, StoreError> {
        self.transition(
            keys,
            ItemStatus::Delivered,
            Some(session),
            Some("catch_up"),
            "status IN ('pending', 'queued')",
        )
    }

    /// After a crash nothing is really in flight.
    pub fn reset_inflight(&self) -> Result<usize, StoreError> {
        Ok(self.conn().execute(
            "UPDATE queue SET status = 'pending', session_id = NULL WHERE status = 'inflight'",
            [],
        )?)
    }

    // ---- the pending cap -------------------------------------------------

    /// Keep only the newest `keep` pending items of `agent` on `committee`;
    /// the dropped ones are counted for [`Store::dropped`]. Returns how many
    /// were dropped.
    pub fn cap_pending(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
        keep: usize,
    ) -> Result<usize, StoreError> {
        let keep = i64::try_from(keep).unwrap_or(i64::MAX);
        let agent = agent_text(agent);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "DELETE FROM queue WHERE agent = ?1 AND committee = ?2 AND status = 'pending'
               AND seq NOT IN (
                 SELECT seq FROM queue WHERE agent = ?1 AND committee = ?2 AND status = 'pending'
                 ORDER BY seq DESC LIMIT ?3)",
            params![agent, committee.as_str(), keep],
        )?;
        if n > 0 {
            tx.execute(
                "INSERT INTO dropped (agent, committee, count) VALUES (?1, ?2, ?3)
                 ON CONFLICT(agent, committee) DO UPDATE SET count = count + excluded.count",
                params![
                    agent,
                    committee.as_str(),
                    i64::try_from(n).unwrap_or(i64::MAX)
                ],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Items of `agent` on `committee` dropped by the cap and not yet
    /// announced.
    pub fn dropped(&self, agent: &SenatorName, committee: &CommitteeId) -> Result<u64, StoreError> {
        let n: Option<i64> = self
            .conn()
            .query_row(
                "SELECT count FROM dropped WHERE agent = ?1 AND committee = ?2",
                params![agent_text(agent), committee.as_str()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(n.and_then(|n| u64::try_from(n).ok()).unwrap_or(0))
    }

    /// `n` dropped items were announced in a push that went out.
    pub fn consume_dropped(
        &self,
        agent: &SenatorName,
        committee: &CommitteeId,
        n: u64,
    ) -> Result<(), StoreError> {
        if n == 0 {
            return Ok(());
        }
        let conn = self.conn();
        conn.execute(
            "UPDATE dropped SET count = MAX(count - ?3, 0) WHERE agent = ?1 AND committee = ?2",
            params![
                agent_text(agent),
                committee.as_str(),
                i64::try_from(n).unwrap_or(i64::MAX)
            ],
        )?;
        conn.execute("DELETE FROM dropped WHERE count <= 0", [])?;
        Ok(())
    }

    // ---- task threads ------------------------------------------------------

    /// Remember the Speaker's triage verdict for the thread rooted at `root`.
    pub fn record_verdict(
        &self,
        committee: &CommitteeId,
        root: Seq,
        verdict: TriageVerdict,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO verdicts (committee, root, verdict) VALUES (?1, ?2, ?3)
             ON CONFLICT (committee, root) DO UPDATE SET verdict = excluded.verdict",
            params![committee.as_str(), seq_sql(root), verdict_str(verdict)],
        )?;
        Ok(())
    }

    pub fn verdict(
        &self,
        committee: &CommitteeId,
        root: Seq,
    ) -> Result<Option<TriageVerdict>, StoreError> {
        let raw: Option<String> = self
            .conn()
            .query_row(
                "SELECT verdict FROM verdicts WHERE committee = ?1 AND root = ?2",
                params![committee.as_str(), seq_sql(root)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|v| parse_verdict(&v)))
    }

    /// The session of one agent that owns the thread rooted at `root`: the
    /// first one it was delivered to, or that reported on it. First write
    /// wins. A session id means nothing without its agent, so both are kept
    /// ([`TaskOwner`]).
    pub fn set_owner(
        &self,
        committee: &CommitteeId,
        root: Seq,
        owner: &TaskOwner,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT OR IGNORE INTO owners (committee, root, agent, session_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                committee.as_str(),
                seq_sql(root),
                agent_text(&owner.agent),
                owner.session
            ],
        )?;
        Ok(())
    }

    /// The person has seen `root`'s thread up to `seq`. Never lowers a mark.
    pub fn mark_seen(
        &self,
        committee: &CommitteeId,
        root: Seq,
        seq: Seq,
    ) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO person_seen (committee, root, seen) VALUES (?1, ?2, ?3)
             ON CONFLICT(committee, root) DO UPDATE SET seen = MAX(seen, excluded.seen)",
            params![committee.as_str(), seq_sql(root), seq_sql(seq)],
        )?;
        Ok(())
    }

    /// Every thread of the committee the person has seen some of, with how
    /// far.
    pub fn seen(&self, committee: &CommitteeId) -> Result<Vec<(Seq, Seq)>, StoreError> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT root, seen FROM person_seen WHERE committee = ?1 ORDER BY root")?;
        let rows = stmt.query_map([committee.as_str()], |r| {
            Ok((seq_from(r.get(0)?), seq_from(r.get(1)?)))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn owner(
        &self,
        committee: &CommitteeId,
        root: Seq,
    ) -> Result<Option<TaskOwner>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT agent, session_id FROM owners WHERE committee = ?1 AND root = ?2",
                params![committee.as_str(), seq_sql(root)],
                |r| {
                    Ok(TaskOwner {
                        agent: agent_col(r, 0)?,
                        session: r.get(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// Cancel items not yet pushed (pending, or held for hooks): the root
    /// itself, and with `whole_thread` every item in its thread too. With
    /// `agent`, only that agent's items; else every agent's. Returns the keys
    /// cancelled.
    pub fn cancel(
        &self,
        agent: Option<&SenatorName>,
        committee: &CommitteeId,
        root: Seq,
        whole_thread: bool,
        reason: &str,
    ) -> Result<Vec<ItemKey>, StoreError> {
        let waiting = self.items(
            "committee = ?1 AND status IN ('pending', 'queued')",
            &[&committee.as_str()],
        )?;
        let keys: Vec<ItemKey> = waiting
            .iter()
            .filter(|i| agent.is_none_or(|a| &i.agent == a))
            .filter(|i| i.event.seq == root || (whole_thread && i.event.thread == Some(root)))
            .map(QueueItem::key)
            .collect();
        self.transition(
            &keys,
            ItemStatus::Cancelled,
            None,
            Some(reason),
            "status IN ('pending', 'queued')",
        )
    }
}

#[cfg(test)]
mod tests {
    use senate_core::{Address, Kind, NewEvent};

    use super::*;

    fn committee() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn agent(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn claude() -> SenatorName {
        agent("claude")
    }

    fn codex() -> SenatorName {
        agent("codex")
    }

    fn event(seq: u64) -> Event {
        NewEvent::new(
            committee(),
            Kind::Question,
            Address::Speaker,
            format!("q{seq}"),
        )
        .to(Address::senator(&claude()))
        .into_event(Seq(seq), Utc::now())
    }

    fn key(agent: &SenatorName, seq: u64) -> ItemKey {
        (agent.clone(), committee(), Seq(seq))
    }

    fn upsert(id: &str) -> SessionUpsert {
        SessionUpsert {
            session_id: id.into(),
            cwd: "/w".into(),
            git_root: None,
            branch: Some("feature/refund-v2/alice".into()),
            inferred_committee: Some((committee(), CommitteeSource::Branch)),
            socket: Some("/tmp/x.sock".into()),
            token: Some("tok".into()),
            claude_pid: Some(42),
            transcript_path: None,
            ancestor_pids: Vec::new(),
            runtime: Runtime::Claude,
            agent: claude(),
            agent_source: AgentSource::Harness,
        }
    }

    /// Replaces the predecessor's migration test (the column it added exists
    /// from the start here): a session's runtime and agent are stored, read
    /// back read-only, and follow the latest hook.
    #[test]
    fn sessions_keep_their_runtime_and_agent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let s = Store::open(&path).unwrap();
        s.upsert_session(&upsert("cc")).unwrap();
        let mut codex_session = upsert("thread-1");
        codex_session.runtime = Runtime::Codex;
        codex_session.agent = codex();
        codex_session.socket = None;
        codex_session.token = None;
        codex_session.claude_pid = None;
        let got = s.upsert_session(&codex_session).unwrap();
        assert_eq!((got.runtime, &got.agent), (Runtime::Codex, &codex()));
        // A pin changes the agent at the next hook.
        let mut pinned = upsert("cc");
        pinned.agent = agent("reviewer");
        pinned.agent_source = AgentSource::Env;
        let got = s.upsert_session(&pinned).unwrap();
        assert_eq!(got.agent, agent("reviewer"));
        assert_eq!(got.agent_source, AgentSource::Env);
        drop(s);
        let ro = Store::open_read_only(&path).unwrap();
        assert_eq!(ro.session("thread-1").unwrap().unwrap().agent, codex());
        assert_eq!(ro.sessions().unwrap().len(), 2);
    }

    #[test]
    fn ingest_moves_cursor_and_dedupes() {
        let s = Store::in_memory().unwrap();
        assert!(s.ingest(&claude(), &event(1), true).unwrap());
        assert!(s.ingest(&claude(), &event(2), false).unwrap());
        assert!(!s.ingest(&claude(), &event(2), true).unwrap());
        assert!(!s.ingest(&claude(), &event(1), true).unwrap());
        assert_eq!(s.delivered_cursor(&claude(), &committee()).unwrap(), Seq(2));
        let pending = s.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            (pending[0].event.seq, &pending[0].agent),
            (Seq(1), &claude())
        );
        assert!(s.advance(&claude(), &committee(), Seq(3)).unwrap());
        assert!(!s.advance(&claude(), &committee(), Seq(3)).unwrap());
        assert_eq!(s.delivered_cursor(&claude(), &committee()).unwrap(), Seq(3));
        assert_eq!(s.pending().unwrap().len(), 1);
    }

    #[test]
    fn two_agents_of_one_person_have_their_own_cursors_and_queues() {
        let s = Store::in_memory().unwrap();
        // One event addressed to both: one row each.
        let both = NewEvent::new(committee(), Kind::Question, Address::Speaker, "q")
            .with_to(vec![
                Address::senator(&claude()),
                Address::senator(&codex()),
            ])
            .into_event(Seq(1), Utc::now());
        assert!(s.ingest(&claude(), &both, true).unwrap());
        assert!(s.ingest(&codex(), &both, true).unwrap());
        assert!(s.ingest(&claude(), &event(2), true).unwrap());
        assert_eq!(s.delivered_cursor(&claude(), &committee()).unwrap(), Seq(2));
        assert_eq!(s.delivered_cursor(&codex(), &committee()).unwrap(), Seq(1));
        let seqs = |a: &SenatorName| -> Vec<u64> {
            s.pending_for(a, &committee())
                .unwrap()
                .iter()
                .map(|i| i.event.seq.0)
                .collect()
        };
        assert_eq!(seqs(&claude()), [1, 2]);
        assert_eq!(seqs(&codex()), [1]);
        // Delivering claude's copy leaves codex's waiting.
        s.claim_for_hook(&[key(&claude(), 1)], "cc").unwrap();
        assert_eq!(
            s.item(&claude(), &committee(), Seq(1))
                .unwrap()
                .unwrap()
                .status,
            ItemStatus::Delivered
        );
        assert_eq!(
            s.item(&codex(), &committee(), Seq(1))
                .unwrap()
                .unwrap()
                .status,
            ItemStatus::Pending
        );
        // Reporting and the cap are per agent too.
        s.mark_reported(&claude(), &committee(), Seq(2)).unwrap();
        assert_eq!(
            s.cursor(&codex(), &committee()).unwrap().reported,
            Seq::ZERO
        );
        s.cap_pending(&claude(), &committee(), 0).unwrap();
        assert_eq!(s.dropped(&claude(), &committee()).unwrap(), 1);
        assert_eq!(s.dropped(&codex(), &committee()).unwrap(), 0);
        assert_eq!(seqs(&codex()), [1]);
        let all: Vec<(String, u64)> = s
            .cursors()
            .unwrap()
            .into_iter()
            .map(|(a, _, c)| (a.to_string(), c.delivered.0))
            .collect();
        assert_eq!(all, [("claude".to_string(), 2), ("codex".to_string(), 1)]);
    }

    #[test]
    fn seeding_only_fills_a_missing_cursor() {
        let s = Store::in_memory().unwrap();
        assert!(!s.has_cursor(&claude(), &committee()).unwrap());
        assert!(
            s.seed_cursor(&claude(), &committee(), Seq(7), Seq::ZERO)
                .unwrap()
        );
        assert!(s.has_cursor(&claude(), &committee()).unwrap());
        assert!(!s.has_cursor(&codex(), &committee()).unwrap());
        assert!(
            !s.seed_cursor(&claude(), &committee(), Seq(9), Seq(9))
                .unwrap()
        );
        let row = s.cursor(&claude(), &committee()).unwrap();
        assert_eq!((row.delivered, row.reported), (Seq(7), Seq::ZERO));
    }

    #[test]
    fn a_new_epoch_forgets_cursors_and_every_queue_row() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.epoch("http://a").unwrap(), None);
        s.set_epoch("http://a", "e1").unwrap();
        s.ingest(&claude(), &event(1), true).unwrap();
        s.ingest(&claude(), &event(2), true).unwrap();
        s.ingest(&codex(), &event(1), false).unwrap();
        s.claim_for_hook(&[key(&claude(), 2)], "x").unwrap();
        let summary = s.reset_for_epoch("http://a", "e2").unwrap();
        assert_eq!(
            summary,
            ResetSummary {
                committees: 2,
                undelivered: 1
            }
        );
        assert_eq!(s.epoch("http://a").unwrap().as_deref(), Some("e2"));
        assert!(!s.has_cursor(&claude(), &committee()).unwrap());
        assert!(!s.has_cursor(&codex(), &committee()).unwrap());
        // Delivered rows go too, or seq 2 of the new log would be ignored.
        assert!(s.item(&claude(), &committee(), Seq(2)).unwrap().is_none());
        s.seed_cursor(&claude(), &committee(), Seq::ZERO, Seq::ZERO)
            .unwrap();
        assert!(s.ingest(&claude(), &event(2), true).unwrap());
        assert!(!s.has_other_epochs("http://a").unwrap());
        assert!(s.has_other_epochs("http://b").unwrap());
    }

    #[test]
    fn a_committee_reset_leaves_other_committees_and_agents_alone() {
        let s = Store::in_memory().unwrap();
        let other = CommitteeId::new("other").unwrap();
        s.ingest(&claude(), &event(1), true).unwrap();
        s.ingest(&codex(), &event(1), true).unwrap();
        s.advance(&claude(), &other, Seq(5)).unwrap();
        s.record_verdict(&committee(), Seq(1), TriageVerdict::Agent)
            .unwrap();
        assert_eq!(s.reset_committee(&claude(), &committee()).unwrap(), 1);
        assert!(!s.has_cursor(&claude(), &committee()).unwrap());
        // The other agent's stream resets its own cursor when it finds out.
        assert!(s.has_cursor(&codex(), &committee()).unwrap());
        assert!(s.item(&codex(), &committee(), Seq(1)).unwrap().is_some());
        assert_eq!(s.delivered_cursor(&claude(), &other).unwrap(), Seq(5));
        // Facts of the old log go for everyone.
        assert_eq!(s.verdict(&committee(), Seq(1)).unwrap(), None);
    }

    #[test]
    fn a_state_database_written_with_paths_is_converted_once_keeping_queue_and_read_marks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute_batch(
                "CREATE TABLE seating (server TEXT PRIMARY KEY, presider TEXT);
                 INSERT INTO seating VALUES ('http://127.0.0.1:7979', NULL);",
            )
            .unwrap();
            let event = serde_json::json!({
                "id": "01KQ8Z3M7N0000000000000E03", "committee": "refund", "seq": 3,
                "ts": "2026-11-01T09:02:00Z", "kind": "question",
                "from": "senator::wendell", "to": ["senator::wendell/claude"],
                "body": "still there?", "meta": {}
            });
            conn.execute(
                "INSERT INTO queue (agent, committee, seq, event, status, enqueued_at)
                 VALUES ('wendell/claude', 'refund', 3, ?1, 'pending', 1)",
                [event.to_string()],
            )
            .unwrap();
            conn.execute_batch(
                "INSERT INTO cursors (agent, committee, delivered, reported, touched)
                   VALUES ('wendell/claude', 'refund', 3, 2, 1),
                          ('wendell/claude/reviewer', 'refund', 1, 1, 1);
                 INSERT INTO person_seen (committee, root, seen) VALUES ('refund', 1, 2);
                 INSERT INTO owners (committee, root, agent, session_id)
                   VALUES ('refund', 1, 'wendell/claude', 's1');",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let claude: SenatorName = "claude".parse().unwrap();
        let refund = CommitteeId::new("refund").unwrap();
        assert_eq!(s.delivered_cursor(&claude, &refund).unwrap(), Seq(3));
        let item = s.item(&claude, &refund, Seq(3)).unwrap().unwrap();
        assert_eq!(item.event.to, ["senator::claude".parse().unwrap()]);
        assert_eq!(s.seen(&refund).unwrap(), vec![(Seq(1), Seq(2))]);
        drop(s);
        assert!(dir.path().join("state.db.pre-flat-names").exists());
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, FLAT_NAMES_VERSION);
        let nested: i64 = conn
            .query_row(
                "SELECT count(*) FROM cursors WHERE agent LIKE '%/%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(nested, 0, "a nested agent's rows are dropped");
        drop(conn);
        // Once: no second copy.
        drop(Store::open(&path).unwrap());
        assert!(!dir.path().join("state.db.pre-flat-names.1").exists());
    }

    #[test]
    fn claims_are_exclusive() {
        let s = Store::in_memory().unwrap();
        s.ingest(&claude(), &event(1), true).unwrap();
        let key = key(&claude(), 1);
        assert_eq!(
            s.claim_inflight(std::slice::from_ref(&key), "a")
                .unwrap()
                .len(),
            1
        );
        // A hook cannot take an item that is in flight.
        assert!(
            s.claim_for_hook(std::slice::from_ref(&key), "b")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            s.hold_for_hooks(std::slice::from_ref(&key), "a")
                .unwrap()
                .len(),
            1
        );
        // Held for a: neither a plain claim nor b's retry may take it.
        assert!(
            s.claim_inflight(std::slice::from_ref(&key), "a")
                .unwrap()
                .is_empty()
        );
        assert!(
            s.claim_inflight_with_held(std::slice::from_ref(&key), "b")
                .unwrap()
                .is_empty()
        );
        // Only the session it is held for may take it.
        assert!(
            s.claim_for_hook(std::slice::from_ref(&key), "b")
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            s.claim_inflight_with_held(std::slice::from_ref(&key), "a")
                .unwrap()
                .len(),
            1
        );
        s.hold_for_hooks(std::slice::from_ref(&key), "a").unwrap();
        assert_eq!(
            s.claim_for_hook(std::slice::from_ref(&key), "a")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            s.item(&claude(), &committee(), Seq(1))
                .unwrap()
                .unwrap()
                .status,
            ItemStatus::Delivered
        );
    }

    #[test]
    fn items_for_a_session_nothing_can_be_pushed_to_are_never_in_flight() {
        let s = Store::in_memory().unwrap();
        s.upsert_session(&upsert("a")).unwrap();
        s.ingest(&claude(), &event(1), true).unwrap();
        s.ingest(&claude(), &event(2), true).unwrap();
        let (one, two) = (key(&claude(), 1), key(&claude(), 2));
        // One is already in flight (somebody's push): not this call's.
        s.claim_inflight(std::slice::from_ref(&two), "a").unwrap();
        let held = s
            .hold_pending_for_hooks(&[one.clone(), two.clone()], "a")
            .unwrap();
        assert_eq!(held, vec![one.clone()]);
        // Pending to queued in one step: a hook reading just after finds it.
        let item = s.item(&claude(), &committee(), Seq(1)).unwrap().unwrap();
        assert_eq!(
            (item.status, item.session_id.as_deref()),
            (ItemStatus::Queued, Some("a"))
        );
        assert_eq!(s.queued_for("a").unwrap().len(), 1);
        assert_eq!(
            s.claim_for_hook(&[one], "a").unwrap().len(),
            1,
            "and the hook takes it"
        );
        // Something in flight is not held by this call.
        let again = s.hold_pending_for_hooks(&[two], "b").unwrap();
        assert!(again.is_empty());
    }

    #[test]
    fn stale_session_releases_held_items() {
        let s = Store::in_memory().unwrap();
        s.upsert_session(&upsert("a")).unwrap();
        s.ingest(&claude(), &event(1), true).unwrap();
        let key = key(&claude(), 1);
        s.claim_inflight(std::slice::from_ref(&key), "a").unwrap();
        s.hold_for_hooks(std::slice::from_ref(&key), "a").unwrap();
        assert!(s.set_session_state("a", SessionState::Stale).unwrap());
        let item = s.item(&claude(), &committee(), Seq(1)).unwrap().unwrap();
        assert_eq!((item.status, item.session_id), (ItemStatus::Pending, None));
    }

    #[test]
    fn a_session_that_changes_agent_lets_go_of_the_old_agents_items() {
        let s = Store::in_memory().unwrap();
        s.upsert_session(&upsert("a")).unwrap();
        s.ingest(&claude(), &event(1), true).unwrap();
        let k = key(&claude(), 1);
        s.claim_inflight(std::slice::from_ref(&k), "a").unwrap();
        s.hold_for_hooks(std::slice::from_ref(&k), "a").unwrap();
        assert_eq!(s.release_held_for_others("a", &claude()).unwrap(), 0);
        assert_eq!(s.release_held_for_others("a", &codex()).unwrap(), 1);
        let item = s.item(&claude(), &committee(), Seq(1)).unwrap().unwrap();
        assert_eq!((item.status, item.session_id), (ItemStatus::Pending, None));
    }

    #[test]
    fn upsert_keeps_join_and_credentials() {
        let s = Store::in_memory().unwrap();
        let mut first = upsert("a");
        first.ancestor_pids = vec![100, 99];
        s.upsert_session(&first).unwrap();
        let other = CommitteeId::new("other").unwrap();
        s.set_joined_committee("a", Some(&other)).unwrap();
        let mut again = upsert("a");
        again.socket = None;
        again.token = None;
        again.inferred_committee = None;
        let got = s.upsert_session(&again).unwrap();
        assert_eq!(got.committee(), Some(&other));
        assert_eq!(got.socket.as_deref(), Some(Path::new("/tmp/x.sock")));
        assert_eq!(got.token.as_deref(), Some("tok"));
        assert_eq!(got.claude_pid, Some(42));
        assert_eq!(s.sessions_below(99).unwrap(), ["a"]);
        assert!(s.sessions_below(7).unwrap().is_empty());
    }

    #[test]
    fn a_hosted_binding_survives_reopen_and_replaces_old_inbox_credentials() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("state.db");
        let store = Store::open(&path).unwrap();
        let mut manual = upsert("a");
        manual.ancestor_pids = vec![100, 99];
        store.upsert_session(&manual).unwrap();
        store.set_session_state("a", SessionState::Ended).unwrap();

        let host = EventId::new();
        let committee = CommitteeId::new("review").unwrap();
        let mut resumed = upsert("a");
        resumed.socket = None;
        resumed.token = None;
        resumed.claude_pid = None;
        let session = store
            .register_hosted_session(&resumed, &host, &committee, None)
            .unwrap();
        assert!(session.is_live());
        assert!(
            session.socket.is_none() && session.token.is_none() && session.claude_pid.is_none()
        );
        assert!(store.sessions_below(99).unwrap().is_empty());
        drop(store);

        let store = Store::open(&path).unwrap();
        let updated = store.upsert_session(&resumed).unwrap();
        assert_eq!(updated.committee(), Some(&committee));
        assert_eq!(store.session_host("a").unwrap(), Some(host));
    }

    #[test]
    fn prune_forgets_old_stale_sessions_and_frees_their_items() {
        let s = Store::in_memory().unwrap();
        let mut a = upsert("a");
        a.ancestor_pids = vec![5];
        s.upsert_session(&a).unwrap();
        s.upsert_session(&upsert("live")).unwrap();
        s.ingest(&claude(), &event(1), true).unwrap();
        let key = key(&claude(), 1);
        s.claim_inflight(std::slice::from_ref(&key), "a").unwrap();
        s.set_session_state("a", SessionState::Stale).unwrap();
        // A hold that raced the session going stale.
        s.transition(
            std::slice::from_ref(&key),
            ItemStatus::Queued,
            Some("a"),
            None,
            "1 = 1",
        )
        .unwrap();
        // `last_seen` must be strictly older than the cutoff.
        std::thread::sleep(Duration::from_millis(5));
        s.prune(Duration::ZERO).unwrap();
        assert!(s.session("a").unwrap().is_none());
        assert!(s.session("live").unwrap().is_some());
        assert!(s.sessions_below(5).unwrap().is_empty());
        let item = s.item(&claude(), &committee(), Seq(1)).unwrap().unwrap();
        assert_eq!((item.status, item.session_id), (ItemStatus::Pending, None));
    }

    #[test]
    fn the_cap_keeps_the_newest_and_counts_the_rest() {
        let s = Store::in_memory().unwrap();
        for i in 1..=5 {
            s.ingest(&claude(), &event(i), true).unwrap();
        }
        assert_eq!(s.cap_pending(&claude(), &committee(), 2).unwrap(), 3);
        let left: Vec<u64> = s.pending().unwrap().iter().map(|i| i.event.seq.0).collect();
        assert_eq!(left, [4, 5]);
        assert_eq!(s.dropped(&claude(), &committee()).unwrap(), 3);
        assert_eq!(s.cap_pending(&claude(), &committee(), 2).unwrap(), 0);
        s.consume_dropped(&claude(), &committee(), 2).unwrap();
        assert_eq!(s.dropped(&claude(), &committee()).unwrap(), 1);
        s.consume_dropped(&claude(), &committee(), 5).unwrap();
        assert_eq!(s.dropped(&claude(), &committee()).unwrap(), 0);
    }

    #[test]
    fn a_thread_is_owned_by_one_session_of_one_agent() {
        let s = Store::in_memory().unwrap();
        let mine = TaskOwner {
            agent: codex(),
            session: "thread-1".into(),
        };
        s.set_owner(&committee(), Seq(7), &mine).unwrap();
        // First write wins.
        s.set_owner(
            &committee(),
            Seq(7),
            &TaskOwner {
                agent: claude(),
                session: "cc".into(),
            },
        )
        .unwrap();
        assert_eq!(s.owner(&committee(), Seq(7)).unwrap(), Some(mine));
        assert_eq!(s.owner(&committee(), Seq(8)).unwrap(), None);
    }

    #[test]
    fn a_cancel_can_be_narrowed_to_one_agent() {
        let s = Store::in_memory().unwrap();
        s.ingest(&claude(), &event(1), true).unwrap();
        s.ingest(&codex(), &event(1), true).unwrap();
        let mine = s
            .cancel(Some(&codex()), &committee(), Seq(1), true, "answered")
            .unwrap();
        assert_eq!(mine, [key(&codex(), 1)]);
        let rest = s
            .cancel(None, &committee(), Seq(1), true, "takeover")
            .unwrap();
        assert_eq!(rest, [key(&claude(), 1)]);
    }

    #[test]
    fn what_the_person_has_seen_only_moves_up_and_goes_with_the_log() {
        let store = Store::in_memory().unwrap();
        let c = committee();
        assert!(store.seen(&c).unwrap().is_empty());
        store.mark_seen(&c, Seq(3), Seq(9)).unwrap();
        store.mark_seen(&c, Seq(3), Seq(5)).unwrap();
        store.mark_seen(&c, Seq(7), Seq(8)).unwrap();
        assert_eq!(
            store.seen(&c).unwrap(),
            [(Seq(3), Seq(9)), (Seq(7), Seq(8))]
        );
        let other = CommitteeId::new("other").unwrap();
        assert!(store.seen(&other).unwrap().is_empty());
        // A reset of one committee's log forgets its marks only.
        store.mark_seen(&other, Seq(1), Seq(2)).unwrap();
        store.reset_committee(&claude(), &c).unwrap();
        assert!(store.seen(&c).unwrap().is_empty());
        assert_eq!(store.seen(&other).unwrap().len(), 1);
        // A new epoch forgets all of them: seqs of the old log mean nothing.
        store.reset_for_epoch("http://s", "e2").unwrap();
        assert!(store.seen(&other).unwrap().is_empty());
    }

    #[test]
    fn a_database_from_before_read_state_gets_the_table_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            // The schema as it was: no person_seen table.
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE cursors (agent TEXT NOT NULL, committee TEXT NOT NULL,
                     delivered INTEGER NOT NULL, reported INTEGER NOT NULL DEFAULT 0,
                     touched INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (agent, committee));
                 INSERT INTO cursors (agent, committee, delivered) VALUES ('claude', 'refund-v2', 7);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.delivered_cursor(&claude(), &committee()).unwrap(),
            Seq(7)
        );
        assert!(store.seen(&committee()).unwrap().is_empty());
        store.mark_seen(&committee(), Seq(1), Seq(4)).unwrap();
        assert_eq!(store.seen(&committee()).unwrap(), [(Seq(1), Seq(4))]);
    }

    #[test]
    fn an_item_queued_for_a_secretary_leaves_the_cursor_alone_and_is_queued_once() {
        let store = Store::in_memory().unwrap();
        let (secretary, question) = (agent("claude"), event(7));
        assert!(store.enqueue(&secretary, &question).unwrap());
        assert!(
            !store.enqueue(&secretary, &question).unwrap(),
            "the same item is not queued twice"
        );
        assert_eq!(
            store.cursor(&secretary, &committee()).unwrap().delivered,
            Seq::ZERO,
            "its cursor is its own stream's"
        );
        let waiting = store.waiting_for(&secretary).unwrap();
        assert_eq!(waiting.len(), 1);
        assert_eq!(waiting[0].event.seq, Seq(7));
        assert!(store.waiting_for(&codex()).unwrap().is_empty());
        // Withdrawn, it stays withdrawn: it is never queued again.
        let gone = store
            .cancel(Some(&secretary), &committee(), Seq(7), false, "withdrawn")
            .unwrap();
        assert_eq!(gone, vec![key(&secretary, 7)]);
        assert!(store.waiting_for(&secretary).unwrap().is_empty());
        assert!(!store.enqueue(&secretary, &question).unwrap());
        assert!(store.waiting_for(&secretary).unwrap().is_empty());
    }
}
