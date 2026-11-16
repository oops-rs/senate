//! The embedded on-disk store, for developer machines.
//!
//! A [`SqliteStore`] is one SQLite file (`sqlite:<path>` in
//! [`connect`](crate::connect)), bundled with the crate so a developer needs
//! no database server. It keeps the same tables as the Postgres store, with
//! the whole stored [`Event`] as a JSON `envelope` next to the scalar columns
//! that are keyed or indexed.
//!
//! # Blocking calls
//!
//! SQLite's API blocks, so every call runs on tokio's blocking pool
//! ([`tokio::task::spawn_blocking`]) over one connection behind a mutex.
//! Seq assignment happens in an `IMMEDIATE` transaction, so it stays gapless
//! and correct even when a second process opens the same file.
//!
//! # Search
//!
//! Search is a plain substring match, not an index: the committee's events
//! are scanned newest first and kept when every term of the query occurs in
//! the body, compared exactly as [`matches_terms`] does (case-insensitive,
//! any language). There are no near matches. That is fine for a developer's
//! log; use Postgres for a log people search heavily.
//!
//! # Schema versions
//!
//! `PRAGMA user_version` counts the migrations applied; opening a file runs
//! the ones after it, in one transaction each.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, SubsecRound, Utc};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use senate_core::api::Heartbeat;
use senate_core::hub::{matches_terms, search_terms};
use senate_core::{Appended, CommitteeId, Event, MirrorRef, NewEvent, SenatorPath, Seq};

use crate::error::LogError;
use crate::store::{CursorOwner, HeartbeatRecord, LogStore, check_storable, mint_epoch, now};

/// Migrations, in order. Index `n` takes the file from version `n` to `n + 1`.
const MIGRATIONS: &[&str] = &[include_str!("../migrations-sqlite/0001_init.sql")];

/// A [`LogStore`] over one SQLite file.
#[derive(Clone)]
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open (creating it when missing) the database at `path` and apply the
    /// embedded migrations. `":memory:"` gives a private in-memory database.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        let path = path.as_ref().to_path_buf();
        let conn = tokio::task::spawn_blocking(move || -> Result<Connection, LogError> {
            let mut conn = Connection::open(path)?;
            conn.busy_timeout(Duration::from_secs(5))?;
            // Journal mode answers with a row, so it is read, not executed.
            let _mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
            conn.pragma_update(None, "synchronous", "NORMAL")?;
            migrate(&mut conn)?;
            Ok(conn)
        })
        .await
        .map_err(task_failed)??;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run `f` on the blocking pool with the connection.
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, LogError> + Send + 'static,
    ) -> Result<T, LogError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn: MutexGuard<'_, Connection> =
                conn.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut conn)
        })
        .await
        .map_err(task_failed)?
    }
}

fn task_failed(error: tokio::task::JoinError) -> LogError {
    LogError::Task(error.to_string())
}

fn migrate(conn: &mut Connection) -> Result<(), LogError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (n, sql) in MIGRATIONS
        .iter()
        .enumerate()
        .skip(usize::try_from(version).unwrap_or(0))
    {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(
            None,
            "user_version",
            i64::try_from(n + 1).unwrap_or(i64::MAX),
        )?;
        tx.commit()?;
    }
    // The epoch is minted once, when the log is created.
    conn.execute(
        "INSERT OR IGNORE INTO log_meta (singleton, epoch) VALUES (1, ?1)",
        [mint_epoch()],
    )?;
    Ok(())
}

fn seq_to_db(seq: Seq) -> Result<i64, LogError> {
    i64::try_from(seq.0).map_err(|_| LogError::Invalid(format!("seq {seq} exceeds i64::MAX")))
}

fn seq_from_db(value: i64) -> Result<Seq, LogError> {
    u64::try_from(value)
        .map(Seq)
        .map_err(|_| LogError::Corrupt(format!("negative seq {value}")))
}

fn limit_to_db(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

fn decode_event(envelope: &str) -> Result<Event, LogError> {
    serde_json::from_str(envelope).map_err(|e| LogError::Corrupt(format!("event envelope: {e}")))
}

fn decode_committee(value: String) -> Result<CommitteeId, LogError> {
    CommitteeId::new(value).map_err(|e| LogError::Corrupt(e.to_string()))
}

fn time_to_db(at: DateTime<Utc>) -> String {
    at.trunc_subsecs(6)
        .to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn time_from_db(value: &str) -> Result<DateTime<Utc>, LogError> {
    DateTime::parse_from_rfc3339(value)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| LogError::Corrupt(format!("timestamp {value:?}: {e}")))
}

fn events(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Vec<Event>, LogError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(args, |r| r.get::<_, String>(0))?;
    rows.map(|envelope| decode_event(&envelope?)).collect()
}

fn stored_by_id(conn: &Connection, committee: &str, id: &str) -> Result<Option<Event>, LogError> {
    let envelope: Option<String> = conn
        .query_row(
            "SELECT envelope FROM events WHERE committee = ?1 AND id = ?2",
            [committee, id],
            |r| r.get(0),
        )
        .optional()?;
    envelope.as_deref().map(decode_event).transpose()
}

fn decode_mirror(row: &Row<'_>) -> Result<MirrorRef, LogError> {
    Ok(MirrorRef {
        committee: decode_committee(row.get("committee")?)?,
        seq: seq_from_db(row.get("seq")?)?,
        platform: row.get("platform")?,
        target: row.get("target")?,
        message_ref: row.get("message_ref")?,
        thread_ref: row.get("thread_ref")?,
    })
}

#[async_trait]
impl LogStore for SqliteStore {
    async fn append(&self, event: NewEvent) -> Result<Appended, LogError> {
        check_storable(&event)?;
        self.run(move |conn| {
            let committee = event.committee.as_str().to_string();
            let id = event.id.to_string();
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if let Some(stored) = stored_by_id(&tx, &committee, &id)? {
                return Ok(Appended {
                    event: stored,
                    duplicate: true,
                });
            }
            let last: Option<i64> = tx
                .query_row(
                    "SELECT last_seq FROM committee_heads WHERE committee = ?1",
                    [&committee],
                    |r| r.get(0),
                )
                .optional()?;
            let seq = last.unwrap_or(0) + 1;
            tx.execute(
                "INSERT INTO committee_heads (committee, last_seq) VALUES (?1, ?2) \
                 ON CONFLICT (committee) DO UPDATE SET last_seq = excluded.last_seq",
                params![committee, seq],
            )?;
            let stored = event.into_event(seq_from_db(seq)?, now());
            let envelope = serde_json::to_string(&stored)
                .map_err(|e| LogError::Invalid(format!("event does not serialize: {e}")))?;
            let thread = stored.thread.map(seq_to_db).transpose()?;
            tx.execute(
                "INSERT INTO events (committee, seq, id, ts, kind, from_addr, thread, body, envelope) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    committee,
                    seq,
                    id,
                    time_to_db(stored.ts),
                    stored.kind.as_str(),
                    stored.from.to_string(),
                    thread,
                    stored.body,
                    envelope,
                ],
            )?;
            tx.commit()?;
            Ok(Appended {
                event: stored,
                duplicate: false,
            })
        })
        .await
    }

    async fn read(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, LogError> {
        let committee = committee.as_str().to_string();
        self.run(move |conn| {
            // Past i64::MAX nothing is stored.
            let Ok(after) = seq_to_db(after) else {
                return Ok(Vec::new());
            };
            events(
                conn,
                "SELECT envelope FROM events WHERE committee = ?1 AND seq > ?2 \
                 ORDER BY seq LIMIT ?3",
                params![committee, after, limit_to_db(limit)],
            )
        })
        .await
    }

    async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, LogError> {
        let committee = committee.as_str().to_string();
        self.run(move |conn| {
            let Ok(seq) = seq_to_db(seq) else {
                return Ok(None);
            };
            Ok(events(
                conn,
                "SELECT envelope FROM events WHERE committee = ?1 AND seq = ?2",
                params![committee, seq],
            )?
            .pop())
        })
        .await
    }

    async fn head(&self, committee: &CommitteeId) -> Result<Seq, LogError> {
        let committee = committee.as_str().to_string();
        self.run(move |conn| {
            let last: Option<i64> = conn
                .query_row(
                    "SELECT last_seq FROM committee_heads WHERE committee = ?1",
                    [committee],
                    |r| r.get(0),
                )
                .optional()?;
            last.map_or(Ok(Seq::ZERO), seq_from_db)
        })
        .await
    }

    async fn committees(&self) -> Result<Vec<CommitteeId>, LogError> {
        self.run(|conn| {
            // BINARY collation is SQLite's default: byte order, as promised.
            let mut stmt =
                conn.prepare("SELECT committee FROM committee_heads ORDER BY committee")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.map(|name| decode_committee(name?)).collect()
        })
        .await
    }

    async fn cursor(
        &self,
        committee: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
    ) -> Result<Option<Seq>, LogError> {
        let (committee, owner, name) = (
            committee.as_str().to_string(),
            owner.key(),
            name.to_string(),
        );
        self.run(move |conn| {
            let seq: Option<i64> = conn
                .query_row(
                    "SELECT seq FROM cursors WHERE committee = ?1 AND owner = ?2 AND name = ?3",
                    [committee, owner, name],
                    |r| r.get(0),
                )
                .optional()?;
            seq.map(seq_from_db).transpose()
        })
        .await
    }

    async fn set_cursor(
        &self,
        committee: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
        seq: Seq,
    ) -> Result<(), LogError> {
        let (committee, owner, name) = (
            committee.as_str().to_string(),
            owner.key(),
            name.to_string(),
        );
        let seq = seq_to_db(seq)?;
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO cursors (committee, owner, name, seq) VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (committee, owner, name) DO UPDATE SET seq = excluded.seq",
                params![committee, owner, name, seq],
            )?;
            Ok(())
        })
        .await
    }

    async fn epoch(&self) -> Result<String, LogError> {
        self.run(|conn| Ok(conn.query_row("SELECT epoch FROM log_meta", [], |r| r.get(0))?))
            .await
    }

    async fn thread(&self, committee: &CommitteeId, root: Seq) -> Result<Vec<Event>, LogError> {
        let committee = committee.as_str().to_string();
        self.run(move |conn| {
            let Ok(root) = seq_to_db(root) else {
                return Ok(Vec::new());
            };
            events(
                conn,
                "SELECT envelope FROM events \
                 WHERE committee = ?1 AND (seq = ?2 OR (thread = ?2 AND seq > ?2)) ORDER BY seq",
                params![committee, root],
            )
        })
        .await
    }

    async fn bindings(&self) -> Result<Vec<Event>, LogError> {
        self.run(|conn| {
            events(
                conn,
                "SELECT envelope FROM events WHERE kind = 'binding' ORDER BY committee, seq",
                [],
            )
        })
        .await
    }

    async fn search(
        &self,
        committee: &CommitteeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Event>, LogError> {
        let terms = search_terms(query);
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let committee = committee.as_str().to_string();
        self.run(move |conn| {
            // Newest first; only a hit's envelope is decoded.
            let mut stmt = conn.prepare_cached(
                "SELECT body, envelope FROM events WHERE committee = ?1 ORDER BY seq DESC",
            )?;
            let mut rows = stmt.query([committee])?;
            let mut hits = Vec::new();
            while let Some(row) = rows.next()? {
                if matches_terms(&row.get::<_, String>(0)?, &terms) {
                    hits.push(decode_event(&row.get::<_, String>(1)?)?);
                    if hits.len() == limit {
                        break;
                    }
                }
            }
            Ok(hits)
        })
        .await
    }

    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), LogError> {
        let seq = seq_to_db(mirror.seq)?;
        self.run(move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO mirrors (committee, seq, platform, target, message_ref, thread_ref) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    mirror.committee.as_str(),
                    seq,
                    mirror.platform,
                    mirror.target,
                    mirror.message_ref,
                    mirror.thread_ref,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn mirrors_of_seq(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, LogError> {
        let (committee, platform) = (committee.as_str().to_string(), platform.to_string());
        self.run(move |conn| {
            let Ok(seq) = seq_to_db(seq) else {
                return Ok(Vec::new());
            };
            let mut stmt = conn.prepare_cached(
                "SELECT committee, seq, platform, target, message_ref, thread_ref FROM mirrors \
                 WHERE committee = ?1 AND seq = ?2 AND platform = ?3 ORDER BY target",
            )?;
            let mut rows = stmt.query(params![committee, seq, platform])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(decode_mirror(row)?);
            }
            Ok(out)
        })
        .await
    }

    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, LogError> {
        let (platform, message_ref) = (platform.to_string(), message_ref.to_string());
        self.run(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT committee, seq, platform, target, message_ref, thread_ref FROM mirrors \
                 WHERE platform = ?1 AND message_ref = ?2 \
                 ORDER BY committee, seq, target LIMIT 1",
            )?;
            let mut rows = stmt.query(params![platform, message_ref])?;
            rows.next()?.map(decode_mirror).transpose()
        })
        .await
    }

    async fn record_heartbeat(
        &self,
        committee: &CommitteeId,
        heartbeat: Heartbeat,
        at: DateTime<Utc>,
    ) -> Result<(), LogError> {
        let committee = committee.as_str().to_string();
        let cursor = seq_to_db(heartbeat.cursor)?;
        self.run(move |conn| {
            conn.execute(
                "INSERT INTO heartbeats (committee, agent, sessions, delivered_seq, seen_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (committee, agent) DO UPDATE SET \
                 sessions = excluded.sessions, delivered_seq = excluded.delivered_seq, \
                 seen_at = excluded.seen_at",
                params![
                    committee,
                    heartbeat.agent.to_string(),
                    i64::from(heartbeat.sessions),
                    cursor,
                    time_to_db(at),
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn heartbeats(&self, committee: &CommitteeId) -> Result<Vec<HeartbeatRecord>, LogError> {
        let committee = committee.clone();
        self.run(move |conn| {
            let mut stmt = conn.prepare_cached(
                "SELECT agent, sessions, delivered_seq, seen_at FROM heartbeats \
                 WHERE committee = ?1 ORDER BY agent",
            )?;
            let mut rows = stmt.query([committee.as_str()])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let agent: SenatorPath = row
                    .get::<_, String>(0)?
                    .parse()
                    .map_err(|e| LogError::Corrupt(format!("heartbeat agent: {e}")))?;
                let sessions: i64 = row.get(1)?;
                out.push(HeartbeatRecord {
                    committee: committee.clone(),
                    heartbeat: Heartbeat {
                        agent: agent.clone(),
                        sessions: u32::try_from(sessions)
                            .map_err(|_| LogError::Corrupt(format!("sessions {sessions}")))?,
                        cursor: seq_from_db(row.get(2)?)?,
                    },
                    agent,
                    at: time_from_db(&row.get::<_, String>(3)?)?,
                });
            }
            Ok(out)
        })
        .await
    }

    async fn last_heartbeat(
        &self,
        committee: &CommitteeId,
        agent: &SenatorPath,
    ) -> Result<Option<HeartbeatRecord>, LogError> {
        let committee = committee.clone();
        let agent = agent.clone();
        self.run(move |conn| {
            let row: Option<(i64, i64, String)> = conn
                .query_row(
                    "SELECT sessions, delivered_seq, seen_at FROM heartbeats \
                     WHERE committee = ?1 AND agent = ?2",
                    params![committee.as_str(), agent.to_string()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let Some((sessions, delivered, seen_at)) = row else {
                return Ok(None);
            };
            Ok(Some(HeartbeatRecord {
                committee,
                heartbeat: Heartbeat {
                    agent: agent.clone(),
                    sessions: u32::try_from(sessions)
                        .map_err(|_| LogError::Corrupt(format!("sessions {sessions}")))?,
                    cursor: seq_from_db(delivered)?,
                },
                agent,
                at: time_from_db(&seen_at)?,
            }))
        })
        .await
    }
}
