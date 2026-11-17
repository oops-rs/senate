//! A developer machine's home as an older Senate left it: owner-prefixed
//! names in its log and its `state.db`. The new daemon starts on it with no
//! edit by its owner: both are converted once, each after a copy is saved
//! beside it, and nothing the person had is lost (the thread, the read
//! marks, the item still queued for an agent).

#![allow(clippy::unwrap_used)]

use std::path::Path;

use rusqlite::{Connection, params};
use senate_bridge::config::Hosted;
use senate_bridge::daemon::Timing;
use senate_bridge::{BridgeConfig, Paths};
use senate_core::{CommitteeId, Hub, Seq};
use senated::Machine;

const LOG_V1: &str = include_str!("../../senate-log/migrations-sqlite/0001_init.sql");

const SENATE_TOML: &str =
    "person = \"wendell\"\nlocale = \"en\"\n\n[local]\nspeaker = \"claude\"\n";

fn envelope(
    seq: i64,
    kind: &str,
    from: &str,
    to: &[&str],
    thread: Option<i64>,
    body: &str,
) -> String {
    let mut value = serde_json::json!({
        "id": format!("01KQ8Z3M7N0000000000000E{seq:02}"),
        "committee": "refund",
        "seq": seq,
        "ts": "2026-11-01T09:00:00.000000Z",
        "kind": kind,
        "from": from,
        "to": to,
        "body": body,
        "meta": {},
    });
    if let Some(thread) = thread {
        value["thread"] = thread.into();
    }
    value.to_string()
}

/// `senates/top/log.db` at schema version 1, as the old store wrote it.
fn old_log(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(LOG_V1).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    conn.execute(
        "INSERT INTO log_meta (singleton, epoch) VALUES (1, '01KQ8Z3M7N00000000000000AA')",
        [],
    )
    .unwrap();
    let rows = [
        (
            1,
            "question",
            "senator::wendell",
            vec!["senator::wendell/claude"],
            None,
            "Where is the retry limit set?",
        ),
        (
            2,
            "answer",
            "senator::wendell/claude",
            vec!["senator::wendell"],
            Some(1),
            "In config/retry.rs.",
        ),
        (
            3,
            "question",
            "senator::wendell",
            vec!["senator::wendell/codex"],
            None,
            "Still there?",
        ),
    ];
    for (seq, kind, from, to, thread, body) in rows {
        conn.execute(
            "INSERT INTO events (committee, seq, id, ts, kind, from_addr, thread, body, envelope)
             VALUES ('refund', ?1, ?2, '2026-11-01T09:00:00.000000Z', ?3, ?4, ?5, ?6, ?7)",
            params![
                seq,
                format!("01KQ8Z3M7N0000000000000E{seq:02}"),
                kind,
                from,
                thread,
                body,
                envelope(seq, kind, from, &to, thread, body),
            ],
        )
        .unwrap();
    }
    conn.execute(
        "INSERT INTO committee_heads (committee, last_seq) VALUES ('refund', 3)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO cursors (committee, owner, name, seq) VALUES ('refund', 'senator::wendell/claude', 'read', 2)",
        [],
    )
    .unwrap();
}

/// `state.db` as the old bridge left it: codex's item still queued, the
/// person's read mark on thread 1.
fn old_state(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE cursors (agent TEXT NOT NULL, committee TEXT NOT NULL,
            delivered INTEGER NOT NULL, reported INTEGER NOT NULL DEFAULT 0,
            touched INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (agent, committee));
         CREATE TABLE seating (server TEXT PRIMARY KEY, presider TEXT);
         CREATE TABLE queue (agent TEXT NOT NULL, committee TEXT NOT NULL, seq INTEGER NOT NULL,
            event TEXT NOT NULL, status TEXT NOT NULL, session_id TEXT, via TEXT,
            enqueued_at INTEGER NOT NULL, delivered_at INTEGER,
            PRIMARY KEY (agent, committee, seq));
         CREATE TABLE person_seen (committee TEXT NOT NULL, root INTEGER NOT NULL,
            seen INTEGER NOT NULL, PRIMARY KEY (committee, root));
         INSERT INTO cursors VALUES ('wendell/codex', 'refund', 3, 3, 1);
         INSERT INTO seating VALUES ('http://127.0.0.1:1', NULL);
         INSERT INTO person_seen VALUES ('refund', 1, 2);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO queue (agent, committee, seq, event, status, enqueued_at)
         VALUES ('wendell/codex', 'refund', 3, ?1, 'pending', 1)",
        [envelope(
            3,
            "question",
            "senator::wendell",
            &["senator::wendell/codex"],
            None,
            "Still there?",
        )],
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_home_starts_with_its_log_read_marks_and_queue_intact() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("h");
    let paths = Paths::new(&home);
    std::fs::create_dir_all(paths.hosted("top")).unwrap();
    std::fs::write(paths.config(), SENATE_TOML).unwrap();
    old_log(&paths.hosted("top").join("log.db"));
    old_state(&paths.state_db());

    let config = BridgeConfig::load_unchecked(&paths.config()).unwrap();
    let hosted = Hosted::load(&paths.config()).unwrap();
    let machine = Machine::start_with(paths.clone(), config, hosted, Timing::default())
        .await
        .unwrap();
    let senate = machine.senate().unwrap();
    assert_eq!(senate.name.as_str(), "wendell-desk", "the default name");

    // `[local] speaker = "claude"`, as the old home says: the person is the
    // Speaker and claude works her desk as her secretary; never refused,
    // and said once.
    let office = senate.daemon.hub.office().expect("an office");
    assert_eq!(office.speaker.to_string(), "senator::wendell");
    assert_eq!(office.kind, senate_core::SenatorKind::Person);
    let claude: senate_core::SenatorName = "claude".parse().unwrap();
    assert_eq!(office.desk, std::slice::from_ref(&claude));
    let roster = senate.daemon.hub.roster().await.unwrap();
    let wendell: senate_core::SenatorName = "wendell".parse().unwrap();
    assert_eq!(roster.secretaries_of(&wendell), [claude]);
    let note = std::fs::read_to_string(paths.hosted("top").join("OFFICE-NOTE")).unwrap();
    assert!(note.contains("[senator::wendell]"), "{note}");
    assert!(note.contains("works your desk as your secretary"), "{note}");

    // The log: converted, the thread reads back with flat names.
    let refund = CommitteeId::new("refund").unwrap();
    let thread = senate.daemon.hub.thread(&refund, Seq(1)).await.unwrap();
    assert_eq!(thread.len(), 2);
    assert_eq!(thread[0].to[0].to_string(), "senator::claude");
    assert_eq!(thread[1].from.to_string(), "senator::claude");
    // Copies of both, beside them, never removed. The developer-machine
    // side opens state.db as it starts, under supervision: wait for it.
    assert!(paths.hosted("top").join("log.db.pre-flat-names").exists());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !paths.socket().exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(home.join("state.db.pre-flat-names").exists());
    machine.shutdown().await;

    // state.db: the read mark and codex's queued item are still there.
    let store = senate_bridge::state::Store::open(&paths.state_db()).unwrap();
    assert_eq!(store.seen(&refund).unwrap(), vec![(Seq(1), Seq(2))]);
    let codex = "codex".parse().unwrap();
    let item = store.item(&codex, &refund, Seq(3)).unwrap().unwrap();
    assert_eq!(item.event.to[0].to_string(), "senator::codex");
}
