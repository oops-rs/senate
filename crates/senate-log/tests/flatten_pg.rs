//! A Postgres log written when senators were paths, converted only when
//! asked ([`PgStore::flatten_names`]). Needs `SENATE_TEST_DATABASE_URL`
//! (see `tests/conformance.rs`); skips cleanly without it. Each test runs in
//! a fresh schema, dropped afterwards.

#![allow(clippy::unwrap_used)]

use std::str::FromStr;

use chrono::Utc;
use senate_core::{Address, CommitteeId, EventId, Kind, NewEvent, Seq};
use senate_log::{FLAT_NAMES_FORMAT, FlattenOutcome, LogError, LogStore, PgStore};
use serde_json::{Value, json};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, PgPool};

const PG_URL_ENV: &str = "SENATE_TEST_DATABASE_URL";

struct Scratch {
    admin: PgPool,
    store: PgStore,
    schema: String,
}

impl Scratch {
    async fn new() -> Option<Self> {
        let Ok(url) = std::env::var(PG_URL_ENV) else {
            eprintln!("skipping Postgres flatten tests: {PG_URL_ENV} is unset");
            return None;
        };
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect admin pool");
        let schema = format!("flat_{}", EventId::new().to_string().to_lowercase());
        sqlx::query(AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .expect("create schema");
        let _ = sqlx::query("CREATE EXTENSION IF NOT EXISTS pg_trgm SCHEMA public")
            .execute(&admin)
            .await;
        let options = PgConnectOptions::from_str(&url)
            .expect("parse url")
            .options([("search_path", format!("{schema},public").as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .expect("connect store pool");
        let store = PgStore::from_pool(pool);
        store.migrate().await.expect("migrate");
        Some(Self {
            admin,
            store,
            schema,
        })
    }

    async fn run(&self, sql: &str, binds: &[&str]) {
        let mut q = sqlx::query(AssertSqlSafe(sql.to_owned()));
        for b in binds {
            q = q.bind(*b);
        }
        q.execute(self.store.pool())
            .await
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// Write an event the way the store wrote it when senators were paths.
    async fn old_event(&self, seq: i64, from: &str, to: &[&str], meta: Value) {
        let c = CommitteeId::new("refund").unwrap();
        let event = NewEvent::new(c, Kind::Note, Address::Speaker, "text")
            .into_event(Seq(u64::try_from(seq).unwrap()), Utc::now());
        let mut value = serde_json::to_value(&event).unwrap();
        value["from"] = json!(from);
        value["to"] = json!(to);
        value["meta"] = meta;
        sqlx::query(
            "INSERT INTO events (committee, seq, id, ts, kind, from_addr, thread, envelope) \
             VALUES ('refund', $1, $2, now(), 'note', $3, NULL, $4::jsonb)",
        )
        .bind(seq)
        .bind(event.id.to_string())
        .bind(from)
        .bind(value.to_string())
        .execute(self.store.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO committee_heads (committee, last_seq) VALUES ('refund', $1) \
             ON CONFLICT (committee) DO UPDATE SET last_seq = excluded.last_seq",
        )
        .bind(seq)
        .execute(self.store.pool())
        .await
        .unwrap();
    }

    async fn drop(self) {
        sqlx::query(AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.admin)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn an_old_postgres_log_is_converted_only_when_asked() {
    let Some(db) = Scratch::new().await else {
        return;
    };
    // A new, empty log is created at the flat-names format.
    assert_eq!(db.store.format().await.unwrap(), FLAT_NAMES_FORMAT);
    // An older log: format 1, owner-prefixed names.
    db.run("UPDATE log_meta SET format = 1", &[]).await;
    db.old_event(1, "senator::alice/claude", &["senator::alice"], json!({}))
        .await;
    db.old_event(
        2,
        "senator::bob",
        &["senator::bob/codex"],
        json!({ "presence": { "agent": "bob/codex" } }),
    )
    .await;
    db.run(
        "INSERT INTO cursors (committee, owner, name, seq) VALUES ('refund', 'senator::alice/claude', 'delivered', 1)",
        &[],
    )
    .await;
    // Opening it again changes nothing: never automatic.
    db.store.migrate().await.unwrap();
    assert_eq!(db.store.format().await.unwrap(), 1);
    let c = CommitteeId::new("refund").unwrap();
    assert!(matches!(
        db.store.read(&c, Seq::ZERO, 10).await.unwrap()[0].from,
        Address::Other(_)
    ));

    let outcome = db.store.flatten_names().await.unwrap();
    let FlattenOutcome::Converted { events, agents } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(events, 2);
    assert_eq!(agents["claude"], "alice");
    assert_eq!(agents["codex"], "bob");
    let read = db.store.read(&c, Seq::ZERO, 10).await.unwrap();
    assert_eq!(read[0].from.to_string(), "senator::claude");
    assert_eq!(read[1].to[0].to_string(), "senator::codex");
    assert_eq!(read[1].meta["presence"]["agent"], "codex");
    assert_eq!(
        db.store
            .cursor(
                &c,
                &"claude".parse::<senate_core::SenatorName>().unwrap().into(),
                "delivered"
            )
            .await
            .unwrap(),
        Some(Seq(1))
    );
    assert_eq!(db.store.format().await.unwrap(), FLAT_NAMES_FORMAT);
    // The copies are kept.
    let kept: i64 = sqlx::query_scalar("SELECT count(*) FROM events_pre_flat_names")
        .fetch_one(db.store.pool())
        .await
        .unwrap();
    assert_eq!(kept, 2);
    // Once.
    assert_eq!(
        db.store.flatten_names().await.unwrap(),
        FlattenOutcome::AlreadyFlat
    );
    db.drop().await;
}

#[tokio::test]
async fn an_old_postgres_log_with_a_nested_agent_is_refused_unchanged() {
    let Some(db) = Scratch::new().await else {
        return;
    };
    db.run("UPDATE log_meta SET format = 1", &[]).await;
    db.old_event(1, "senator::alice/claude/reviewer", &[], json!({}))
        .await;
    let err = db.store.flatten_names().await.unwrap_err();
    assert!(matches!(err, LogError::Flatten(_)), "{err}");
    assert!(err.to_string().contains("refund seq 1"), "{err}");
    assert_eq!(db.store.format().await.unwrap(), 1);
    let copied: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('events_pre_flat_names')::text")
            .fetch_one(db.store.pool())
            .await
            .unwrap();
    assert_eq!(copied, None, "nothing was written");
    db.drop().await;
}
