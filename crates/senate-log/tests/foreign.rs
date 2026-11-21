//! Pointing Senate at a database it did not make: refused plainly, and left
//! exactly as it was.
//!
//! The Postgres cases need `SENATE_TEST_DATABASE_URL` (see
//! `tests/conformance.rs`) and skip cleanly without it. Each runs in a fresh
//! schema, dropped afterwards. The SQLite cases always run.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::str::FromStr;

use senate_core::{Address, CommitteeId, EventId, Kind, NewEvent, SenatorName};
use senate_log::{LogError, LogStore, PgStore, SqliteStore};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};

const PG_URL_ENV: &str = "SENATE_TEST_DATABASE_URL";

/// Senate's own first two migration versions.
const INIT_VERSION: i64 = 20_261_026_000_000;

fn is_foreign(error: &LogError) -> bool {
    matches!(error, LogError::ForeignDatabase(_))
}

fn plain_words(error: &LogError) {
    let text = error.to_string();
    for words in [
        "was not created by Senate",
        "incompatible version",
        "Senate did not change it",
        "new, empty database",
    ] {
        assert!(text.contains(words), "{text:?} lacks {words:?}");
    }
}

// ---- Postgres ---------------------------------------------------------

struct Scratch {
    admin: PgPool,
    pool: PgPool,
    schema: String,
}

impl Scratch {
    async fn new() -> Option<Self> {
        let Ok(url) = std::env::var(PG_URL_ENV) else {
            eprintln!("skipping Postgres foreign-database tests: {PG_URL_ENV} is unset");
            return None;
        };
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect admin pool");
        let schema = format!("foreign_{}", EventId::new().to_string().to_lowercase());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create schema");
        install_pg_trgm(&admin).await;
        let options = PgConnectOptions::from_str(&url)
            .expect("parse url")
            .options([("search_path", format!("{schema},public").as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect store pool");
        Some(Self {
            admin,
            pool,
            schema,
        })
    }

    async fn run(&self, sql: &str) {
        sqlx::query(AssertSqlSafe(sql.to_owned()))
            .execute(&self.pool)
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// Every table of the schema with its columns, indexes and rows: what
    /// "unchanged" means.
    async fn snapshot(&self) -> BTreeMap<String, String> {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename::text FROM pg_tables WHERE schemaname = $1 ORDER BY 1",
        )
        .bind(&self.schema)
        .fetch_all(&self.admin)
        .await
        .unwrap();
        let mut out = BTreeMap::new();
        for table in tables {
            let columns: Vec<String> = sqlx::query_scalar(
                "SELECT column_name || ' ' || data_type FROM information_schema.columns \
                 WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
            )
            .bind(&self.schema)
            .bind(&table)
            .fetch_all(&self.admin)
            .await
            .unwrap();
            let indexes: Vec<String> = sqlx::query_scalar(
                "SELECT indexdef FROM pg_indexes WHERE schemaname = $1 AND tablename = $2 \
                 ORDER BY 1",
            )
            .bind(&self.schema)
            .bind(&table)
            .fetch_all(&self.admin)
            .await
            .unwrap();
            let rows: Vec<String> = sqlx::query_scalar(AssertSqlSafe(format!(
                "SELECT row_to_json(t)::text FROM {}.{table} t ORDER BY 1",
                self.schema
            )))
            .fetch_all(&self.admin)
            .await
            .unwrap();
            out.insert(
                table,
                format!("{columns:?}\n{indexes:?}\n{}", rows.join("\n")),
            );
        }
        out
    }

    async fn sqlx_table(&self) {
        self.run(
            "CREATE TABLE _sqlx_migrations (\
               version BIGINT PRIMARY KEY, description TEXT NOT NULL, \
               installed_on TIMESTAMPTZ NOT NULL DEFAULT now(), success BOOLEAN NOT NULL, \
               checksum BYTEA NOT NULL, execution_time BIGINT NOT NULL)",
        )
        .await;
    }

    async fn finish(self) {
        self.pool.close().await;
        sqlx::query(AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .expect("drop schema");
    }
}

/// See `tests/conformance.rs`: left to the migration, `pg_trgm` would land in
/// one test's schema and be dropped with it.
async fn install_pg_trgm(admin: &PgPool) {
    if let Err(error) = sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public")
        .execute(admin)
        .await
    {
        let installed: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_trgm')",
        )
        .fetch_one(admin)
        .await
        .expect("check pg_trgm");
        assert!(installed, "install pg_trgm: {error}");
    }
}

fn committee() -> CommitteeId {
    CommitteeId::new("refund-v2").unwrap()
}

fn note(body: &str) -> NewEvent {
    NewEvent::new(
        committee(),
        Kind::Note,
        Address::Senator("alice".parse::<SenatorName>().unwrap()),
        body,
    )
}

#[tokio::test]
async fn a_migrations_table_with_a_clashing_version_and_another_checksum_is_refused() {
    let Some(db) = Scratch::new().await else {
        return;
    };
    // Another schema's record: the same version number, other SQL.
    db.sqlx_table().await;
    db.run(&format!(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES ({INIT_VERSION}, 'init', true, '\\x0102'::bytea, 1)"
    ))
    .await;
    db.run("CREATE TABLE events (topic text NOT NULL, seq bigint NOT NULL, envelope jsonb)")
        .await;
    db.run("INSERT INTO events VALUES ('refund', 1, '{}')")
        .await;
    let before = db.snapshot().await;

    let error = PgStore::from_pool(db.pool.clone())
        .migrate()
        .await
        .unwrap_err();
    assert!(is_foreign(&error), "{error}");
    plain_words(&error);
    assert!(error.to_string().contains("different SQL"), "{error}");
    assert_eq!(db.snapshot().await, before, "nothing was written");
    db.finish().await;
}

#[tokio::test]
async fn a_migrations_table_with_versions_senate_does_not_have_is_refused() {
    let Some(db) = Scratch::new().await else {
        return;
    };
    db.sqlx_table().await;
    db.run(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (20240101000000, 'theirs', true, '\\x01'::bytea, 1)",
    )
    .await;
    let before = db.snapshot().await;

    let error = PgStore::from_pool(db.pool.clone())
        .migrate()
        .await
        .unwrap_err();
    assert!(is_foreign(&error), "{error}");
    plain_words(&error);
    assert!(error.to_string().contains("does not have"), "{error}");
    assert_eq!(db.snapshot().await, before);
    db.finish().await;
}

#[tokio::test]
async fn an_events_table_of_another_shape_is_refused_and_no_migrations_table_appears() {
    let Some(db) = Scratch::new().await else {
        return;
    };
    db.run(
        "CREATE TABLE events (topic text NOT NULL, seq bigint NOT NULL, id text NOT NULL, \
         ts timestamptz NOT NULL, kind text NOT NULL, from_addr text NOT NULL, \
         envelope jsonb NOT NULL, PRIMARY KEY (topic, seq))",
    )
    .await;
    db.run("INSERT INTO events VALUES ('refund', 1, 'e1', now(), 'chat', 'someone', '{}')")
        .await;
    let before = db.snapshot().await;

    let error = PgStore::from_pool(db.pool.clone())
        .migrate()
        .await
        .unwrap_err();
    assert!(is_foreign(&error), "{error}");
    plain_words(&error);
    assert_eq!(db.snapshot().await, before, "nothing was written");
    assert!(!before.contains_key("_sqlx_migrations"));
    db.finish().await;
}

#[tokio::test]
async fn senates_own_migrations_over_an_events_table_that_lost_a_column_are_refused() {
    let Some(db) = Scratch::new().await else {
        return;
    };
    PgStore::from_pool(db.pool.clone()).migrate().await.unwrap();
    db.run("ALTER TABLE events RENAME COLUMN committee TO topic")
        .await;
    let before = db.snapshot().await;

    let error = PgStore::from_pool(db.pool.clone())
        .migrate()
        .await
        .unwrap_err();
    assert!(is_foreign(&error), "{error}");
    assert!(error.to_string().contains("`committee`"), "{error}");
    assert_eq!(db.snapshot().await, before);
    db.finish().await;
}

#[tokio::test]
async fn a_fresh_database_migrates() {
    let Some(db) = Scratch::new().await else {
        return;
    };
    PgStore::from_pool(db.pool.clone()).migrate().await.unwrap();
    let tables = db.snapshot().await;
    for table in ["events", "log_meta", "_sqlx_migrations", "mirrors"] {
        assert!(tables.contains_key(table), "{table}");
    }
    db.finish().await;
}

#[tokio::test]
async fn a_database_senate_made_reopens_with_its_log_and_epoch() {
    let Some(db) = Scratch::new().await else {
        return;
    };
    let first = PgStore::from_pool(db.pool.clone());
    first.migrate().await.unwrap();
    let epoch = first.epoch().await.unwrap();
    first.append(note("kept")).await.unwrap();

    for _ in 0..2 {
        let again = PgStore::from_pool(db.pool.clone());
        again.migrate().await.unwrap();
        assert_eq!(again.epoch().await.unwrap(), epoch);
        let log = again
            .read(&committee(), senate_core::Seq::ZERO, 10)
            .await
            .unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].body, "kept");
    }
    db.finish().await;
}

// ---- SQLite -----------------------------------------------------------

#[tokio::test]
async fn a_sqlite_file_of_someone_elses_is_refused_and_left_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("theirs.db");
    {
        let conn = rusqlite_open(&path);
        conn.execute_batch("CREATE TABLE notes (id INTEGER PRIMARY KEY, text TEXT); INSERT INTO notes (text) VALUES ('mine');")
            .unwrap();
    }
    let before = std::fs::read(&path).unwrap();
    let error = SqliteStore::open(&path).await.err().expect("refused");
    assert!(is_foreign(&error), "{error}");
    plain_words(&error);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!dir.path().join("theirs.db-wal").exists());
}

#[tokio::test]
async fn a_file_that_is_not_a_database_is_refused_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    std::fs::write(
        &path,
        "a plain text file, long enough to be read as a header ".repeat(40),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let error = SqliteStore::open(&path).await.err().expect("refused");
    assert!(is_foreign(&error), "{error}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn a_new_sqlite_file_works_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("senate.db");
    let epoch = {
        let store = SqliteStore::open(&path).await.unwrap();
        store.append(note("kept")).await.unwrap();
        store.epoch().await.unwrap()
    };
    let store = SqliteStore::open(&path).await.unwrap();
    assert_eq!(store.epoch().await.unwrap(), epoch);
    assert_eq!(
        store
            .read(&committee(), senate_core::Seq::ZERO, 10)
            .await
            .unwrap()
            .len(),
        1
    );
}

fn rusqlite_open(path: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}
