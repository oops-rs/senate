//! A log written when senators were paths, opened by the embedded store:
//! converted once, in place, after a copy is saved beside it.
//!
//! The fixture is written the way the store wrote it then: schema version
//! 1 (the first migration), every event's envelope the JSON of a stored
//! event with owner-prefixed names, cursor owners `senator::<path>`,
//! heartbeat agents by path.

#![allow(clippy::unwrap_used)]

use std::path::Path;

use chrono::Utc;
use rusqlite::{Connection, params};
use senate_core::{Address, CommitteeId, Kind, NewEvent, SenatorName, Seq};
use senate_log::{CursorOwner, LogError, LogStore, SqliteStore};
use serde_json::{Value, json};

const V1: &str = include_str!("../migrations-sqlite/0001_init.sql");

fn committee() -> CommitteeId {
    CommitteeId::new("refund").unwrap()
}

/// An event as the old store kept it: its envelope with the names given.
fn envelope(seq: i64, kind: Kind, from: &str, to: &[&str], meta: Value) -> Value {
    // Built with today's types and then given the old names: the envelope's
    // shape is the same, only the address strings differ.
    let event = NewEvent::new(committee(), kind, Address::Speaker, "text")
        .into_event(Seq(u64::try_from(seq).unwrap()), Utc::now());
    let mut value = serde_json::to_value(event).unwrap();
    value["from"] = json!(from);
    value["to"] = json!(to);
    value["meta"] = meta;
    value
}

/// Write a version-1 file at `path` holding `events`, cursors and
/// heartbeats with old names.
fn old_log(path: &Path, events: &[Value], cursors: &[(&str, i64)], beats: &[&str]) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(V1).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute(
        "INSERT INTO log_meta (singleton, epoch) VALUES (1, '01JOLDEPOCH000000000000000')",
        [],
    )
    .unwrap();
    for e in events {
        let seq = e["seq"].as_i64().unwrap();
        conn.execute(
            "INSERT INTO events (committee, seq, id, ts, kind, from_addr, thread, body, envelope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?8)",
            params![
                "refund",
                seq,
                e["id"].as_str().unwrap(),
                e["ts"].as_str().unwrap(),
                e["kind"].as_str().unwrap(),
                e["from"].as_str().unwrap(),
                e["body"].as_str().unwrap(),
                e.to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO committee_heads (committee, last_seq) VALUES ('refund', ?1)
             ON CONFLICT(committee) DO UPDATE SET last_seq = excluded.last_seq",
            [seq],
        )
        .unwrap();
    }
    for (owner, seq) in cursors {
        conn.execute(
            "INSERT INTO cursors (committee, owner, name, seq) VALUES ('refund', ?1, 'delivered', ?2)",
            params![owner, seq],
        )
        .unwrap();
    }
    for agent in beats {
        conn.execute(
            "INSERT INTO heartbeats (committee, agent, sessions, delivered_seq, seen_at)
             VALUES ('refund', ?1, 1, 2, '2026-11-01T10:00:00.000000Z')",
            [agent],
        )
        .unwrap();
    }
}

fn version(path: &Path) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

fn agent(s: &str) -> SenatorName {
    s.parse().unwrap()
}

#[tokio::test]
async fn an_old_log_is_converted_once_after_a_copy_is_saved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.db");
    old_log(
        &path,
        &[
            envelope(
                1,
                Kind::Question,
                "senator::wendell",
                &["senator::wendell/claude"],
                json!({}),
            ),
            envelope(
                2,
                Kind::Answer,
                "senator::wendell/claude",
                &["senator::wendell"],
                json!({ "proposed_by": "senator::wendell/codex" }),
            ),
            envelope(
                3,
                Kind::Presence,
                "committee",
                &[],
                json!({ "presence": {
                    "agent": "wendell/codex", "state": "online",
                    "since": "2026-11-01T10:00:00Z", "since_seq": 2
                } }),
            ),
            envelope(
                4,
                Kind::Question,
                "senator::wendell",
                &["speaker"],
                json!({ "pick": { "owner": "wendell", "candidates": ["wendell/claude", "wendell/codex"] } }),
            ),
        ],
        &[
            ("senator::wendell/claude", 3),
            ("senator::wendell", 4),
            ("hub", 4),
        ],
        &["wendell/claude"],
    );
    let epoch_before = Connection::open(&path)
        .unwrap()
        .query_row("SELECT epoch FROM log_meta", [], |r| r.get::<_, String>(0))
        .unwrap();

    let store = SqliteStore::open(&path).await.unwrap();
    let c = committee();
    let events = store.read(&c, Seq::ZERO, 10).await.unwrap();
    assert_eq!(events.len(), 4);
    assert_eq!(events[0].from.to_string(), "senator::wendell");
    assert_eq!(events[0].to, ["senator::claude".parse().unwrap()]);
    assert_eq!(events[1].from.to_string(), "senator::claude");
    assert_eq!(events[1].meta["proposed_by"], "senator::codex");
    assert_eq!(events[2].meta["presence"]["agent"], "codex");
    assert_eq!(
        events[3].meta["pick"]["candidates"],
        json!(["claude", "codex"])
    );
    // Cursors and heartbeats follow; the person's and the hub's stay.
    assert_eq!(
        store
            .cursor(&c, &CursorOwner::from(agent("claude")), "delivered")
            .await
            .unwrap(),
        Some(Seq(3))
    );
    assert_eq!(
        store
            .cursor(&c, &CursorOwner::from(agent("wendell")), "delivered")
            .await
            .unwrap(),
        Some(Seq(4))
    );
    assert_eq!(
        store
            .cursor(&c, &CursorOwner::Hub, "delivered")
            .await
            .unwrap(),
        Some(Seq(4))
    );
    let beats = store.heartbeats(&c).await.unwrap();
    assert_eq!(beats[0].agent, agent("claude"));
    // The log is the same log: its epoch is kept, and so is the next seq.
    assert_eq!(store.epoch().await.unwrap(), epoch_before);
    let appended = store
        .append(NewEvent::new(
            c.clone(),
            Kind::Note,
            "senator::claude".parse().unwrap(),
            "x",
        ))
        .await
        .unwrap();
    assert_eq!(appended.event.seq, Seq(5));
    drop(store);

    // The copy beside it is the log as it was, version 1, never removed.
    let copy = dir.path().join("log.db.pre-flat-names");
    assert!(copy.exists());
    assert_eq!(version(&copy), 1);
    let old_from: String = Connection::open(&copy)
        .unwrap()
        .query_row("SELECT from_addr FROM events WHERE seq = 2", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(old_from, "senator::wendell/claude");
    assert!(version(&path) >= 2);

    // Once: opening again converts nothing and saves no second copy.
    let again = SqliteStore::open(&path).await.unwrap();
    assert_eq!(again.read(&c, Seq::ZERO, 10).await.unwrap().len(), 5);
    drop(again);
    assert!(!dir.path().join("log.db.pre-flat-names.1").exists());
}

#[tokio::test]
async fn an_old_log_with_a_nested_agent_is_refused_and_left_as_it_was() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.db");
    old_log(
        &path,
        &[
            envelope(1, Kind::Note, "senator::wendell/claude", &[], json!({})),
            envelope(
                2,
                Kind::Question,
                "senator::wendell/claude/reviewer",
                &["senator::wendell"],
                json!({}),
            ),
        ],
        &[],
        &[],
    );
    let rows = |path: &Path| -> Vec<String> {
        let conn = Connection::open(path).unwrap();
        let mut stmt = conn
            .prepare("SELECT from_addr || envelope FROM events ORDER BY seq")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = rows(&path);
    let Err(err) = SqliteStore::open(&path).await else {
        panic!("a nested agent cannot be converted");
    };
    assert!(matches!(err, LogError::Flatten(_)), "{err}");
    let text = err.to_string();
    assert!(text.contains("refund seq 2"), "{text}");
    assert!(text.contains("wendell/claude/reviewer"), "{text}");
    assert_eq!(version(&path), 1, "the file is left at its version");
    // Only the journal mode is set on open; the log itself is unchanged.
    assert_eq!(rows(&path), before, "and its events unchanged");
    assert!(!dir.path().join("log.db.pre-flat-names").exists());
}

#[tokio::test]
async fn an_old_log_with_one_name_under_two_people_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.db");
    old_log(
        &path,
        &[
            envelope(1, Kind::Note, "senator::alice/claude", &[], json!({})),
            envelope(2, Kind::Note, "senator::bob/claude", &[], json!({})),
        ],
        &[],
        &[],
    );
    let err = SqliteStore::open(&path).await.err().unwrap();
    assert!(err.to_string().contains("alice/claude"), "{err}");
    assert!(err.to_string().contains("bob/claude"), "{err}");
    assert_eq!(version(&path), 1);
}

#[tokio::test]
async fn a_new_file_is_created_at_the_flat_names_version_with_no_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log.db");
    let store = SqliteStore::open(&path).await.unwrap();
    drop(store);
    // Flat names are version 2; the schema goes on from there.
    assert!(version(&path) >= 2);
    assert!(!dir.path().join("log.db.pre-flat-names").exists());
}
