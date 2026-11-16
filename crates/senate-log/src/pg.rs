//! The Postgres store.
//!
//! # Schema (`migrations/`)
//!
//! `events` keeps the whole stored [`Event`] as one JSONB `envelope` column,
//! next to scalar copies of the fields that are keyed or indexed (`committee`,
//! `seq`, `id`, `ts`, `kind`, `from_addr`, `thread`). Reads decode the
//! envelope, so the one `Event` serde definition in `senate-core` is the only
//! mapping between Rust and the row, and a new optional envelope field (the
//! wire only ever adds fields) needs no migration. The scalar columns carry
//! the primary key `(committee, seq)`, the idempotency key `UNIQUE (committee,
//! id)`, the thread index, and the partial index over `binding` events.
//!
//! # Search
//!
//! [`LogStore::search`] runs over a `pg_trgm` GIN index on `envelope->>'body'`
//! (trigrams need no tokenizer, so Chinese works as is). Exact matches come
//! first: every query term `ILIKE`-matched, newest first. If there is room
//! under `limit`, near matches follow: bodies that are not exact matches but
//! whose `word_similarity(query, body)` is at least
//! [`NEAR_MATCH_SIMILARITY`], most similar first.
//!
//! # Epoch
//!
//! `log_meta` holds the one row naming the log's epoch. [`PgStore::migrate`]
//! inserts it when the table is empty, so a new database gets a new epoch and
//! an existing one keeps its own.
//!
//! # Seq assignment
//!
//! `committee_heads` holds each committee's last seq. An append upserts that row
//! (`last_seq + 1`) inside the same transaction as the event insert, so
//! concurrent appends to one committee serialize on the row lock and seq has no
//! gaps. The id check runs again after the lock is taken, so of two
//! concurrent appends with one id, the second sees the first's committed row
//! and rolls back, releasing its increment. The server's per-committee single
//! writer makes contention rare; this keeps the store correct without it.
//!
//! Queries are runtime `sqlx::query`, never the compile-time macros, so a
//! build never needs a database.

use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};
use senate_core::api::Heartbeat;
use senate_core::hub::search_terms;
use senate_core::{Appended, CommitteeId, Event, MirrorRef, NewEvent, SenatorPath, Seq};
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgExecutor, PgPool, PgPoolOptions, PgRow};
use sqlx::{AssertSqlSafe, Row};

use crate::error::LogError;
use crate::store::{CursorOwner, HeartbeatRecord, LogStore, check_storable, mint_epoch, now};

static MIGRATOR: Migrator = sqlx::migrate!();

/// Lowest `pg_trgm` word similarity of a near match in [`LogStore::search`].
pub const NEAR_MATCH_SIMILARITY: f64 = 0.3;

/// A [`LogStore`] over a Postgres pool.
#[derive(Debug, Clone)]
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Connect with a default pool and apply the embedded migrations.
    pub async fn connect(database_url: &str) -> Result<Self, LogError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(database_url)
            .await?;
        let store = Self::from_pool(pool);
        store.migrate().await?;
        Ok(store)
    }

    /// Wrap an existing pool. Call [`PgStore::migrate`] before first use
    /// unless the schema is known to be current.
    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Apply the embedded migrations and mint the log's epoch if it has
    /// none (idempotent).
    pub async fn migrate(&self) -> Result<(), LogError> {
        MIGRATOR.run(&self.pool).await?;
        sqlx::query("INSERT INTO log_meta (epoch) VALUES ($1) ON CONFLICT (singleton) DO NOTHING")
            .bind(mint_epoch())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
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

fn decode_event(row: &PgRow) -> Result<Event, LogError> {
    let envelope: serde_json::Value = row.try_get("envelope")?;
    serde_json::from_value(envelope).map_err(|e| LogError::Corrupt(format!("event envelope: {e}")))
}

fn decode_committee(value: String) -> Result<CommitteeId, LogError> {
    CommitteeId::new(value).map_err(|e| LogError::Corrupt(e.to_string()))
}

fn decode_mirror(row: &PgRow) -> Result<MirrorRef, LogError> {
    Ok(MirrorRef {
        committee: decode_committee(row.try_get("committee")?)?,
        seq: seq_from_db(row.try_get("seq")?)?,
        platform: row.try_get("platform")?,
        target: row.try_get("target")?,
        message_ref: row.try_get("message_ref")?,
        thread_ref: row.try_get("thread_ref")?,
    })
}

/// An `ILIKE` pattern matching `text` anywhere, literally. It escapes with
/// `\`, which is `LIKE`'s default escape, so it needs no `ESCAPE` clause.
fn contains_pattern(text: &str) -> String {
    let mut pattern = String::with_capacity(text.len() + 2);
    pattern.push('%');
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    pattern.push('%');
    pattern
}

async fn stored_by_id<'e>(
    executor: impl PgExecutor<'e>,
    committee: &CommitteeId,
    id: &str,
) -> Result<Option<Event>, LogError> {
    let row = sqlx::query("SELECT envelope FROM events WHERE committee = $1 AND id = $2")
        .bind(committee.as_str())
        .bind(id)
        .fetch_optional(executor)
        .await?;
    row.as_ref().map(decode_event).transpose()
}

#[async_trait]
impl LogStore for PgStore {
    async fn append(&self, event: NewEvent) -> Result<Appended, LogError> {
        check_storable(&event)?;
        let committee = event.committee.clone();
        let id = event.id.to_string();

        // Fast path for retries: no lock, no transaction.
        if let Some(stored) = stored_by_id(&self.pool, &committee, &id).await? {
            return Ok(Appended {
                event: stored,
                duplicate: true,
            });
        }

        let mut tx = self.pool.begin().await?;
        let last_seq: i64 = sqlx::query(
            "INSERT INTO committee_heads (committee, last_seq) VALUES ($1, 1) \
             ON CONFLICT (committee) DO UPDATE SET last_seq = committee_heads.last_seq + 1 \
             RETURNING last_seq",
        )
        .bind(committee.as_str())
        .fetch_one(&mut *tx)
        .await?
        .try_get("last_seq")?;

        // Under the head lock now: a concurrent append of the same id has
        // either committed (visible here) or not started its insert.
        if let Some(stored) = stored_by_id(&mut *tx, &committee, &id).await? {
            tx.rollback().await?;
            return Ok(Appended {
                event: stored,
                duplicate: true,
            });
        }

        let stored = event.into_event(seq_from_db(last_seq)?, now());
        let envelope = serde_json::to_value(&stored)
            .map_err(|e| LogError::Invalid(format!("event does not serialize: {e}")))?;
        let thread = stored.thread.map(seq_to_db).transpose()?;
        sqlx::query(
            "INSERT INTO events (committee, seq, id, ts, kind, from_addr, thread, envelope) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(committee.as_str())
        .bind(last_seq)
        .bind(&id)
        .bind(stored.ts)
        .bind(stored.kind.as_str())
        .bind(stored.from.to_string())
        .bind(thread)
        .bind(&envelope)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

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
    ) -> Result<Vec<Event>, LogError> {
        let rows = sqlx::query(
            "SELECT envelope FROM events WHERE committee = $1 AND seq > $2 ORDER BY seq LIMIT $3",
        )
        .bind(committee.as_str())
        .bind(seq_to_db(after)?)
        .bind(limit_to_db(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_event).collect()
    }

    async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, LogError> {
        let Ok(seq) = seq_to_db(seq) else {
            return Ok(None);
        };
        let row = sqlx::query("SELECT envelope FROM events WHERE committee = $1 AND seq = $2")
            .bind(committee.as_str())
            .bind(seq)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(decode_event).transpose()
    }

    async fn head(&self, committee: &CommitteeId) -> Result<Seq, LogError> {
        let row = sqlx::query("SELECT last_seq FROM committee_heads WHERE committee = $1")
            .bind(committee.as_str())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => seq_from_db(row.try_get("last_seq")?),
            None => Ok(Seq::ZERO),
        }
    }

    async fn committees(&self) -> Result<Vec<CommitteeId>, LogError> {
        let rows =
            sqlx::query("SELECT committee FROM committee_heads ORDER BY committee COLLATE \"C\"")
                .fetch_all(&self.pool)
                .await?;
        rows.iter()
            .map(|row| decode_committee(row.try_get("committee")?))
            .collect()
    }

    async fn cursor(
        &self,
        committee: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
    ) -> Result<Option<Seq>, LogError> {
        let row = sqlx::query(
            "SELECT seq FROM cursors WHERE committee = $1 AND owner = $2 AND name = $3",
        )
        .bind(committee.as_str())
        .bind(owner.key())
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| seq_from_db(row.try_get("seq")?)).transpose()
    }

    async fn set_cursor(
        &self,
        committee: &CommitteeId,
        owner: &CursorOwner,
        name: &str,
        seq: Seq,
    ) -> Result<(), LogError> {
        sqlx::query(
            "INSERT INTO cursors (committee, owner, name, seq) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (committee, owner, name) DO UPDATE SET seq = EXCLUDED.seq, updated_at = now()",
        )
        .bind(committee.as_str())
        .bind(owner.key())
        .bind(name)
        .bind(seq_to_db(seq)?)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn epoch(&self) -> Result<String, LogError> {
        let row = sqlx::query("SELECT epoch FROM log_meta")
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(row.try_get("epoch")?),
            None => Err(LogError::Corrupt(
                "log_meta has no epoch; PgStore::migrate mints one".into(),
            )),
        }
    }

    async fn thread(&self, committee: &CommitteeId, root: Seq) -> Result<Vec<Event>, LogError> {
        // Nothing is stored at, or threaded under, a seq the column cannot hold.
        let Ok(root) = seq_to_db(root) else {
            return Ok(Vec::new());
        };
        let rows = sqlx::query(
            "SELECT envelope FROM events \
             WHERE committee = $1 AND (seq = $2 OR (thread = $2 AND seq > $2)) ORDER BY seq",
        )
        .bind(committee.as_str())
        .bind(root)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_event).collect()
    }

    async fn bindings(&self) -> Result<Vec<Event>, LogError> {
        let rows = sqlx::query(
            "SELECT envelope FROM events WHERE kind = 'binding' \
             ORDER BY committee COLLATE \"C\", seq",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_event).collect()
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
        let patterns: Vec<String> = terms.iter().map(|t| contains_pattern(t)).collect();
        // One transaction, so the threshold below stays local to it.
        let mut tx = self.pool.begin().await?;
        // One `ILIKE` per term rather than `ILIKE ALL ($n)`: the trigram index
        // serves the former and not the latter. The text is placeholders only.
        let each_term = (0..patterns.len())
            .map(|i| format!("envelope->>'body' ILIKE ${}", i + 3))
            .collect::<Vec<_>>()
            .join(" AND ");
        let exact = format!(
            "SELECT envelope FROM events WHERE committee = $1 AND {each_term} \
             ORDER BY seq DESC LIMIT $2"
        );
        let mut exact = sqlx::query(AssertSqlSafe(exact))
            .bind(committee.as_str())
            .bind(limit_to_db(limit));
        for pattern in &patterns {
            exact = exact.bind(pattern);
        }
        let rows = exact.fetch_all(&mut *tx).await?;
        let mut events = rows
            .iter()
            .map(decode_event)
            .collect::<Result<Vec<_>, _>>()?;

        let room = limit - events.len();
        if room > 0 {
            // `<%` is the indexable form of `word_similarity(q, body) >=
            // threshold`, with the threshold a setting; the explicit
            // comparison keeps the rule visible whatever the setting says.
            sqlx::query("SELECT set_config('pg_trgm.word_similarity_threshold', $1, true)")
                .bind(NEAR_MATCH_SIMILARITY.to_string())
                .execute(&mut *tx)
                .await?;
            let rows = sqlx::query(
                "SELECT envelope FROM events \
                 WHERE committee = $1 AND $2 <% (envelope->>'body') \
                 AND word_similarity($2, envelope->>'body') >= $3 \
                 AND NOT (envelope->>'body' ILIKE ALL ($4)) \
                 ORDER BY word_similarity($2, envelope->>'body') DESC, seq DESC LIMIT $5",
            )
            .bind(committee.as_str())
            .bind(terms.join(" "))
            .bind(NEAR_MATCH_SIMILARITY)
            .bind(&patterns)
            .bind(limit_to_db(room))
            .fetch_all(&mut *tx)
            .await?;
            for row in &rows {
                events.push(decode_event(row)?);
            }
        }
        tx.commit().await?;
        Ok(events)
    }

    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), LogError> {
        sqlx::query(
            "INSERT INTO mirrors (committee, seq, platform, target, message_ref, thread_ref) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (committee, seq, platform, target) DO NOTHING",
        )
        .bind(mirror.committee.as_str())
        .bind(seq_to_db(mirror.seq)?)
        .bind(&mirror.platform)
        .bind(&mirror.target)
        .bind(&mirror.message_ref)
        .bind(&mirror.thread_ref)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn mirrors_of_seq(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, LogError> {
        let rows = sqlx::query(
            "SELECT committee, seq, platform, target, message_ref, thread_ref FROM mirrors \
             WHERE committee = $1 AND seq = $2 AND platform = $3 ORDER BY target COLLATE \"C\"",
        )
        .bind(committee.as_str())
        .bind(seq_to_db(seq)?)
        .bind(platform)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_mirror).collect()
    }

    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, LogError> {
        let row = sqlx::query(
            "SELECT committee, seq, platform, target, message_ref, thread_ref FROM mirrors \
             WHERE platform = $1 AND message_ref = $2 \
             ORDER BY committee COLLATE \"C\", seq, target COLLATE \"C\" LIMIT 1",
        )
        .bind(platform)
        .bind(message_ref)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(decode_mirror).transpose()
    }

    async fn record_heartbeat(
        &self,
        committee: &CommitteeId,
        heartbeat: Heartbeat,
        at: DateTime<Utc>,
    ) -> Result<(), LogError> {
        sqlx::query(
            "INSERT INTO heartbeats (committee, agent, sessions, delivered_seq, seen_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (committee, agent) DO UPDATE SET \
             sessions = EXCLUDED.sessions, delivered_seq = EXCLUDED.delivered_seq, \
             seen_at = EXCLUDED.seen_at",
        )
        .bind(committee.as_str())
        .bind(heartbeat.agent.to_string())
        .bind(i64::from(heartbeat.sessions))
        .bind(seq_to_db(heartbeat.cursor)?)
        .bind(at.trunc_subsecs(6))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn last_heartbeat(
        &self,
        committee: &CommitteeId,
        agent: &SenatorPath,
    ) -> Result<Option<HeartbeatRecord>, LogError> {
        let row = sqlx::query(
            "SELECT sessions, delivered_seq, seen_at FROM heartbeats \
             WHERE committee = $1 AND agent = $2",
        )
        .bind(committee.as_str())
        .bind(agent.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let sessions: i64 = row.try_get("sessions")?;
        Ok(Some(HeartbeatRecord {
            committee: committee.clone(),
            agent: agent.clone(),
            heartbeat: Heartbeat {
                agent: agent.clone(),
                sessions: u32::try_from(sessions)
                    .map_err(|_| LogError::Corrupt(format!("sessions {sessions}")))?,
                cursor: seq_from_db(row.try_get("delivered_seq")?)?,
            },
            at: row.try_get("seen_at")?,
        }))
    }
}
