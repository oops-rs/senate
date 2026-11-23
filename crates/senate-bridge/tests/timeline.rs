//! A person's session timeline through a real local bridge socket.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

#[path = "support/agy_timeline.rs"]
mod agy;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use common::Harness;
use senate_bridge::ipc::{
    self, HookEvent, HookReply, HookReport, Request, SessionsReply, TimelineDirection,
    TimelineKind, TimelineReply, TimelineToolDetail, TimelineToolStatus, TimelineUnavailable,
    TimelineUpdate,
};
use senate_bridge::session::Runtime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const TIMEOUT: Duration = Duration::from_secs(5);
const SESSION: &str = "local-timeline";

async fn request<T: serde::de::DeserializeOwned>(bridge: &Harness, request: &Request) -> T {
    ipc::request(&bridge.socket(), request, TIMEOUT, TIMEOUT)
        .await
        .unwrap()
}

async fn register(bridge: &Harness, id: &str, runtime: Runtime, path: &Path, event: HookEvent) {
    let _: HookReply = request(
        bridge,
        &Request::SessionHook(HookReport {
            session_id: id.into(),
            runtime,
            event,
            cwd: bridge.dir.path().into(),
            transcript_path: Some(path.to_str().unwrap().into()),
            source: Some("startup".into()),
            branch: Some("feature/refund-v2/alice".into()),
            git_root: None,
            committee_file: None,
            socket: None,
            token: None,
            claude_pid: Some(std::process::id()),
            ancestor_pids: Vec::new(),
            agent_env: None,
            agent_file: None,
            host: None,
        }),
    )
    .await;
}

async fn page(
    bridge: &Harness,
    id: &str,
    direction: TimelineDirection,
    cursor: Option<u64>,
) -> TimelineReply {
    request(
        bridge,
        &Request::Timeline {
            session_id: id.into(),
            direction,
            cursor,
            limit: Some(2),
        },
    )
    .await
}

async fn watch(bridge: &Harness, id: &str, after: u64) -> BufReader<UnixStream> {
    let mut connection = UnixStream::connect(bridge.socket()).await.unwrap();
    let mut request = serde_json::to_vec(&Request::TimelineWatch {
        session_id: id.into(),
        after,
    })
    .unwrap();
    request.push(b'\n');
    connection.write_all(&request).await.unwrap();
    BufReader::new(connection)
}

async fn update(reader: &mut BufReader<UnixStream>) -> TimelineUpdate {
    let mut line = String::new();
    let len = tokio::time::timeout(TIMEOUT, reader.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert!(len > 0);
    serde_json::from_str(&line).unwrap()
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paging_and_watch_keep_stable_tool_positions_and_reset_on_transcript_change() {
    let mut h = Harness::new().await;
    h.start().await;
    let path = h.dir.path().join("one.jsonl");
    std::fs::write(&path, concat!(
        r#"{"type":"user","message":{"content":"Conversation title\nDetails here"}}"#, "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tool","name":"Bash","input":{"command":"pwd"}}]}}"#, "\n",
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Looking"}]}}"#, "\n",
    )).unwrap();
    register(
        &h,
        SESSION,
        Runtime::Claude,
        &path,
        HookEvent::UserPromptSubmit,
    )
    .await;

    let tail = page(&h, SESSION, TimelineDirection::Tail, None).await;
    assert!(tail.available && tail.working && tail.has_older && !tail.has_newer);
    assert_eq!((tail.first_seq, tail.last_seq), (Some(2), Some(3)));
    assert!(matches!(
        tail.items[0].kind,
        TimelineKind::ToolCall {
            status: TimelineToolStatus::Running,
            ..
        }
    ));
    let older = page(&h, SESSION, TimelineDirection::Before, tail.first_seq).await;
    assert_eq!((older.first_seq, older.last_seq), (Some(1), Some(1)));
    assert!(!older.has_older && older.has_newer);
    let newer = page(&h, SESSION, TimelineDirection::After, older.last_seq).await;
    assert_eq!(newer.items, tail.items);
    let sessions: SessionsReply = request(&h, &Request::Sessions).await;
    assert!(sessions.rows.is_empty());
    let row = sessions
        .local_rows
        .iter()
        .find(|row| row.session_id == SESSION)
        .unwrap();
    assert_eq!(row.title.as_deref(), Some("Conversation title"));
    assert_eq!(row.branch.as_deref(), Some("feature/refund-v2/alice"));
    assert!(row.working);

    let mut reader = watch(&h, SESSION, tail.last_seq.unwrap()).await;
    let initial = update(&mut reader).await;
    assert!(initial.items.is_empty() && initial.working && !initial.reset);

    append(
        &path,
        r#"{"type":"assistant","message":{"content":[{"type":"text","text":"New"#,
    );
    // A partial line is not published or counted until its newline arrives.
    let mut no_line = String::new();
    assert!(
        tokio::time::timeout(Duration::from_millis(250), reader.read_line(&mut no_line))
            .await
            .is_err()
    );
    append(&path, "\"}]}}\n");
    let appended = update(&mut reader).await;
    assert_eq!(appended.items[0].seq, 4);
    assert!(
        matches!(&appended.items[0].kind, TimelineKind::AssistantMessage { text } if text == "New")
    );

    append(
        &path,
        concat!(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tool","content":"/tmp"}]}}"#,
            "\n"
        ),
    );
    let replacement = update(&mut reader).await;
    assert_eq!(replacement.items.len(), 1);
    assert_eq!(replacement.items[0].seq, tail.items[0].seq);
    assert!(
        matches!(&replacement.items[0].kind, TimelineKind::ToolCall {
        status: TimelineToolStatus::Completed, detail: TimelineToolDetail::Shell { output: Some(output), .. }, ..
    } if output == "/tmp")
    );

    // A result appended between tail and watch also arrives under its old ID.
    let mut late_watch = watch(&h, SESSION, 4).await;
    assert_eq!(update(&mut late_watch).await.items, replacement.items);
    drop(late_watch);

    let _: ipc::WaitingReply = request(
        &h,
        &Request::Waiting {
            session_id: SESSION.into(),
        },
    )
    .await;
    let stopped = update(&mut reader).await;
    assert!(!stopped.working && stopped.items.is_empty());

    let other = h.dir.path().join("two.jsonl");
    std::fs::write(
        &other,
        concat!(
            r#"{"type":"user","message":{"content":"After clear"}}"#,
            "\n"
        ),
    )
    .unwrap();
    register(
        &h,
        SESSION,
        Runtime::Claude,
        &other,
        HookEvent::SessionStart,
    )
    .await;
    assert!(update(&mut reader).await.reset);
    assert_eq!(
        tokio::time::timeout(TIMEOUT, reader.read_line(&mut String::new()))
            .await
            .unwrap()
            .unwrap(),
        0
    );

    let _: serde_json::Value = request(
        &h,
        &Request::SessionEnd {
            session_id: SESSION.into(),
            reason: None,
        },
    )
    .await;
    let ended = page(&h, SESSION, TimelineDirection::Tail, None).await;
    assert!(ended.available && !ended.working);
    assert!(
        matches!(&ended.items[0].kind, TimelineKind::UserMessage { text } if text == "After clear")
    );
    let sessions: SessionsReply = request(&h, &Request::Sessions).await;
    assert!(
        sessions
            .local_rows
            .iter()
            .all(|row| row.session_id != SESSION)
    );

    // A runtime with no transcript stays unavailable even with a path.
    register(
        &h,
        "agy:timeline",
        Runtime::Agy,
        &other,
        HookEvent::SessionStart,
    )
    .await;
    let unavailable = page(&h, "agy:timeline", TimelineDirection::Tail, None).await;
    assert_eq!(unavailable.runtime, Runtime::Agy);
    assert!(!unavailable.available && unavailable.items.is_empty());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agy_pages_and_watches_root_steps_and_replaces_a_completed_tool() {
    use agy::{field, number};

    let mut h = Harness::new().await;
    h.start().await;
    let path = h.dir.path().join(format!("{}.db", agy::ID));
    let db = agy::create(&path);
    agy::user(&db, 0, 6, "Injected context");
    agy::user(
        &db,
        1,
        6,
        "[senate · fixtures · seq 42 · from chair · question]\n│ Read this",
    );
    agy::user(&db, 2, 4, "A human prompt\nWith a second line");
    agy::planner(&db, 3, "A reply", "A thought");
    agy::tool(&db, 4, 2, "run_command", r#"{"CommandLine":"pwd"}"#, None);
    agy::tool(
        &db,
        5,
        3,
        "invoke_subagent",
        r#"{"Prompt":"Investigate the fixture"}"#,
        Some(field(1, "Finished")),
    );
    agy::tool(
        &db,
        6,
        3,
        "read_file",
        r#"{"Path":"file.txt"}"#,
        Some(field(1, "file contents")),
    );
    agy::tool(
        &db,
        7,
        3,
        "TodoWrite",
        r#"{"todos":[{"content":"Try it","status":"pending"}]}"#,
        None,
    );
    agy::step(&db, 8, 23, 3, 5, Vec::new(), field(30, Vec::new()));
    agy::step(
        &db,
        9,
        17,
        3,
        5,
        Vec::new(),
        field(
            24,
            [field(3, field(1, "Fixture error")), number(5, 1)].concat(),
        ),
    );
    agy::step(
        &db,
        10,
        101,
        3,
        5,
        Vec::new(),
        field(114, field(3, "ignored")),
    );
    // An unknown protobuf step is represented by a notice rather than guessed.
    agy::step(&db, 11, 999, 3, 5, Vec::new(), Vec::new());
    register(
        &h,
        agy::SESSION,
        Runtime::Agy,
        &path,
        HookEvent::UserPromptSubmit,
    )
    .await;

    let first = page(&h, agy::SESSION, TimelineDirection::After, Some(0)).await;
    assert!(first.available && first.working && first.has_newer && !first.has_older);
    assert!(matches!(&first.items[0].kind, TimelineKind::SenateMessage {
        committee: Some(committee), seq: Some(42), text, ..
    } if committee == "fixtures" && text == "Read this"));
    assert!(
        matches!(&first.items[1].kind, TimelineKind::UserMessage { text } if text.starts_with("A human prompt"))
    );
    assert_eq!(first.items[1].ts.unwrap().timestamp(), 1_700_000_002);
    let answer = page(&h, agy::SESSION, TimelineDirection::After, first.last_seq).await;
    assert!(
        matches!(&answer.items[0].kind, TimelineKind::Reasoning { text } if text == "A thought")
    );
    assert!(
        matches!(&answer.items[1].kind, TimelineKind::AssistantMessage { text } if text == "A reply")
    );
    let inside_same_row = page(&h, agy::SESSION, TimelineDirection::Before, answer.last_seq).await;
    assert_eq!(inside_same_row.items.last(), answer.items.first());
    let tool = page(&h, agy::SESSION, TimelineDirection::After, answer.last_seq).await;
    assert!(matches!(&tool.items[0].kind, TimelineKind::ToolCall {
        status: TimelineToolStatus::Running, detail: TimelineToolDetail::Shell { command, .. }, ..
    } if command == "pwd"));
    assert!(matches!(
        &tool.items[1].kind,
        TimelineKind::ToolCall {
            detail: TimelineToolDetail::SubAgent { .. },
            ..
        }
    ));
    let read = page(&h, agy::SESSION, TimelineDirection::After, tool.last_seq).await;
    assert!(
        matches!(&read.items[0].kind, TimelineKind::ToolCall { output: Some(text), detail: TimelineToolDetail::Read { .. }, .. } if text == "file contents")
    );
    assert!(matches!(&read.items[1].kind, TimelineKind::Todo { .. }));
    let notices = page(&h, agy::SESSION, TimelineDirection::Tail, None).await;
    assert!(notices.has_older && !notices.has_newer);
    assert!(
        matches!(&notices.items[0].kind, TimelineKind::Notice { text, .. } if text == "Fixture error")
    );
    assert!(
        matches!(&notices.items[1].kind, TimelineKind::Notice { text, .. } if text.contains("Unsupported"))
    );
    let before = page(
        &h,
        agy::SESSION,
        TimelineDirection::Before,
        notices.first_seq,
    )
    .await;
    assert!(
        matches!(&before.items[1].kind, TimelineKind::Notice { text, .. } if text == "Conversation compacted")
    );

    let sessions: SessionsReply = request(&h, &Request::Sessions).await;
    let row = sessions
        .local_rows
        .iter()
        .find(|row| row.session_id == agy::SESSION)
        .unwrap();
    assert_eq!(row.title.as_deref(), Some("A human prompt"));
    assert_eq!(row.branch.as_deref(), Some("feature/refund-v2/alice"));
    assert!(row.working);

    let mut watch = watch(&h, agy::SESSION, notices.last_seq.unwrap()).await;
    assert!(update(&mut watch).await.working);
    agy::tool(
        &db,
        4,
        3,
        "run_command",
        r#"{"CommandLine":"pwd"}"#,
        Some(agy::shell_result(17, &"x".repeat(100_000))),
    );
    agy::planner(&db, 12, "New reply", "");
    let delta = update(&mut watch).await;
    assert!(!delta.reset);
    assert_eq!(delta.items.len(), 2);
    let replaced = &delta.items[0];
    assert_eq!(replaced.seq, tool.items[0].seq);
    assert!(replaced.truncated);
    assert!(matches!(&replaced.kind, TimelineKind::ToolCall {
        status: TimelineToolStatus::Failed, detail: TimelineToolDetail::Shell {
            output: Some(output), exit_code: Some(17), cwd: Some(cwd), ..
        }, ..
    } if output.len() == 65536 && cwd == "/tmp/fixtures"));
    assert!(
        matches!(&delta.items[1].kind, TimelineKind::AssistantMessage { text } if text == "New reply")
    );

    // A new inode must reset an existing stream.
    let other = h.dir.path().join("replacement");
    std::fs::create_dir(&other).unwrap();
    let other = other.join(format!("{}.db", agy::ID));
    drop(agy::create(&other));
    drop(db);
    std::fs::rename(other, &path).unwrap();
    assert!(update(&mut watch).await.reset);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agy_missing_locked_or_unknown_databases_are_unavailable() {
    let mut h = Harness::new().await;
    h.start().await;
    let path = h.dir.path().join(format!("{}.db", agy::ID));
    register(
        &h,
        agy::SESSION,
        Runtime::Agy,
        &path,
        HookEvent::SessionStart,
    )
    .await;
    assert!(
        !page(&h, agy::SESSION, TimelineDirection::Tail, None)
            .await
            .available
    );
    let db = agy::create(&path);
    db.execute_batch("PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;")
        .unwrap();
    let locked = page(&h, agy::SESSION, TimelineDirection::Tail, None).await;
    assert!(!locked.available);
    assert_eq!(
        locked.unavailable_reason,
        Some(TimelineUnavailable::TemporarilyUnreadable)
    );
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);

    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("PRAGMA user_version=2").unwrap();
    for query in [
        "PRAGMA user_version=2",
        "PRAGMA user_version=1; INSERT INTO steps(idx,step_format) VALUES (0,1)",
        "DELETE FROM steps; INSERT INTO trajectory_metadata_blob VALUES('main',x'2a0178')",
        "DELETE FROM trajectory_metadata_blob; ALTER TABLE steps ADD COLUMN different TEXT",
    ] {
        db.execute_batch(query).unwrap();
        let reply = page(&h, agy::SESSION, TimelineDirection::Tail, None).await;
        assert!(!reply.available, "{query}");
        assert_eq!(
            reply.unavailable_reason,
            Some(TimelineUnavailable::UnsupportedSchema),
            "{query}"
        );
    }
}

/// Separate process fixture so SQLite and the operating system coordinate
/// exactly as they do for Antigravity and the bridge.
#[test]
#[ignore = "spawned by agy_reader_during_an_external_write"]
fn agy_writer_child() {
    use rusqlite::config::DbConfig;

    let path = std::path::PathBuf::from(std::env::var_os("SENATE_AGY_TEST_PATH").unwrap());
    let db = rusqlite::Connection::open(&path).unwrap();
    db.busy_timeout(Duration::from_millis(25)).unwrap();
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
        .unwrap();
    db.execute_batch("PRAGMA wal_autocheckpoint=0; BEGIN IMMEDIATE")
        .unwrap();
    agy::user(&db, 1, 4, "Commit from another process");
    std::fs::write(path.with_extension("ready"), "").unwrap();
    let deadline = std::time::Instant::now() + TIMEOUT;
    while !path.with_extension("commit").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "parent never signalled"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    db.execute_batch("COMMIT; BEGIN IMMEDIATE; COMMIT").unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agy_reader_during_an_external_write() {
    let mut h = Harness::new().await;
    h.start().await;
    let path = h.dir.path().join(format!("{}.db", agy::ID));
    let db = agy::create(&path);
    agy::user(&db, 0, 4, "Committed fixture");
    drop(db);
    register(
        &h,
        agy::SESSION,
        Runtime::Agy,
        &path,
        HookEvent::SessionStart,
    )
    .await;

    let mut writer = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "agy_writer_child", "--ignored"])
        .env("SENATE_AGY_TEST_PATH", &path)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::timeout(TIMEOUT, async {
        while !path.with_extension("ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();

    let before = page(&h, agy::SESSION, TimelineDirection::Tail, None).await;
    assert!(before.available);
    assert_eq!(before.items.len(), 1, "the writer has not committed yet");
    let mut watch = watch(&h, agy::SESSION, before.last_seq.unwrap()).await;
    assert!(!update(&mut watch).await.reset);
    std::fs::write(path.with_extension("commit"), "").unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, writer.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let committed = update(&mut watch).await;
    assert!(
        matches!(&committed.items[0].kind, TimelineKind::UserMessage { text } if text == "Commit from another process")
    );

    let wal = path.with_extension("db-wal");
    let snapshot = std::fs::read(&wal).unwrap();
    assert!(snapshot.len() > 32);
    let main = std::fs::read(&path).unwrap();
    let after = page(&h, agy::SESSION, TimelineDirection::Tail, None).await;
    assert!(after.available);
    assert_eq!(after.items.len(), 2);
    assert_eq!(
        std::fs::read(wal).unwrap(),
        snapshot,
        "reader must not change the WAL"
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        main,
        "reader must not checkpoint"
    );
}
