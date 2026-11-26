//! Upgrade rehearsal: a state database and session rows as 0.12.3 leaves
//! them, this daemon started on top, and those sessions' hooks and MCP
//! tools still served — by the caller checks a release daemon applies, not
//! the disposable-fixture shortcut ([`testenv::STRICT_IDENTITY_ENV`]). The
//! clients are separate processes that attach no caller proof, as a 0.12.3
//! MCP server still running in such a session does.
//!
//! Then the reverse: the statements 0.12.3 runs when it opens its state
//! database still succeed on the database this daemon has touched.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use common::*;
use senate_bridge::testenv;

/// The schema of `crates/senate-bridge/src/state.rs` at v0.12.3.
const SCHEMA_0_12_3: &str = include_str!("fixtures/state-v0.12.3.sql");

fn senate(root: &Path, home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_senate"));
    testenv::isolate(&mut cmd, root);
    cmd.env("SENATE_HOME", home);
    cmd
}

fn insert_session(db: &Path, id: &str, claude_pid: Option<u32>, host: Option<&str>, cwd: &Path) {
    let conn = rusqlite::Connection::open(db).unwrap();
    let now = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT INTO sessions (session_id, cwd, joined_committee, committee_source, claude_pid,
             state, started_at, last_seen, runtime, agent, agent_source, host_id)
         VALUES (?1, ?2, 'refund-v2', 'join', ?3, 'live', ?4, ?4, 'claude', ?5, 'harness', ?6)",
        rusqlite::params![
            id,
            cwd.display().to_string(),
            claude_pid,
            now,
            format!("claude-{id}"),
            host
        ],
    )
    .unwrap();
}

/// One `tools/call` through a `senate mcp` child for `session`; the text
/// of its result and whether it was an error.
fn mcp_call(root: &Path, home: &Path, session: &str, tool: &str) -> (String, bool) {
    mcp_call_with(root, home, session, tool, serde_json::json!({}))
}

fn mcp_call_with(
    root: &Path,
    home: &Path,
    session: &str,
    tool: &str,
    arguments: serde_json::Value,
) -> (String, bool) {
    let mut child = senate(root, home)
        .arg("mcp")
        .env("CLAUDE_CODE_SESSION_ID", session)
        .env("CLAUDE_PID", std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for line in [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"rehearsal","version":"0"}}}"#.to_owned(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_owned(),
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": tool, "arguments": arguments},
        })
        .to_string(),
    ] {
        writeln!(stdin, "{line}").unwrap();
    }
    let mut reply = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let value: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        if value["id"] == 2 {
            reply = Some(value);
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    let reply = reply.expect("a tools/call reply");
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    (text, reply["result"]["isError"] == true)
}

fn session_start_hook(root: &Path, home: &Path, session: &str, cwd: &Path) -> String {
    let mut child = senate(root, home)
        .args(["hook", "session-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let input = serde_json::json!({
        "session_id": session,
        "cwd": cwd,
        "transcript_path": format!("/tmp/{session}.jsonl"),
        "hook_event_name": "SessionStart",
        "source": "resume",
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

// The clients below block this thread; the daemon runs on the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(unsafe_code)]
async fn sessions_from_0_12_3_keep_working_after_the_upgrade_and_rollback_can_open_the_home() {
    // Set before any thread reads the environment: this test binary holds
    // this one test, and the daemon starts below. That is why this file is
    // a binary of its own and not a module of `suite`: there the variable
    // would reach every other test's daemon.
    unsafe { std::env::set_var(testenv::STRICT_IDENTITY_ENV, "1") };
    let mut h = Harness::new().await;
    let root = h.dir.path().to_path_buf();
    let home = h.paths.home().to_path_buf();
    let cwd = h.workdir("project", Some("refund-v2"));

    // The home as 0.12.3 leaves it: its schema, its flattened names, a
    // native session (its runtime is this test process, the parent of the
    // MCP and hook children) and a hosted one whose host predates bindings.
    {
        let conn = rusqlite::Connection::open(h.paths.state_db()).unwrap();
        conn.execute_batch(SCHEMA_0_12_3).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
    }
    let db = h.paths.state_db();
    insert_session(&db, "old-native", Some(std::process::id()), None, &cwd);
    let host = senate_core::EventId::new().to_string();
    insert_session(&db, "old-hosted", None, Some(&host), &cwd);
    // A running 0.12.3 host: its directory and the lock on its `owner`
    // file, without the process records later hosts write.
    let host_dir = home.join("hosts").join(&host);
    std::fs::create_dir_all(&host_dir).unwrap();
    let owner = std::fs::File::create(host_dir.join("owner")).unwrap();
    rustix::fs::flock(&owner, rustix::fs::FlockOperation::LockExclusive).unwrap();

    h.start().await;

    // The old sessions' MCP tools, from a client that attaches no proof.
    for session in ["old-native", "old-hosted"] {
        let (text, error) = mcp_call(&root, &home, session, "inbox");
        assert!(
            !error && !text.contains("no verified authority"),
            "{session}: {text}"
        );
    }
    // The old native session's hook, which does not bind it.
    let out = session_start_hook(&root, &home, "old-native", &cwd);
    assert!(out.contains("senate"), "the hook was served: {out:?}");
    let (text, error) = mcp_call(&root, &home, "old-native", "inbox");
    assert!(!error, "still served after its hook: {text}");

    // A chair's act within its role through its MCP `spawn` tool: from a
    // member, the role check refuses it; from the chair, it passes the
    // caller and role checks and reaches the handler (which finds no such
    // recorded senator, so nothing is launched).
    let restore = serde_json::json!({"action": "restore", "senator": "nobody-recorded"});
    let (text, error) = mcp_call_with(&root, &home, "old-native", "spawn", restore.clone());
    assert!(error && text.contains("committee's chair may"), "{text}");
    h.server.state.inner.lock().unwrap().chair = Some(("claude-old-native".parse().unwrap(), true));
    let (text, error) = mcp_call_with(&root, &home, "old-native", "spawn", restore);
    eprintln!("chair restore: {text}");
    assert!(
        error
            && !text.contains("committee's chair may")
            && !text.contains("could not tell")
            && !text.contains("verified authority")
            && !text.contains("confirmation in the signed Senate app"),
        "the chair's restore reached its handler: {text}"
    );

    // A session that appears after this daemon started has no such
    // allowance: without a proof of its process it is refused.
    insert_session(&db, "new-unbound", Some(std::process::id()), None, &cwd);
    let (text, error) = mcp_call(&root, &home, "new-unbound", "inbox");
    assert!(error && text.contains("no verified authority"), "{text}");

    h.stop().await;

    // Rollback: 0.12.3 opens the same database with its own schema
    // statements and reads its sessions with its own columns.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(SCHEMA_0_12_3).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1, "0.12.3 converts names only below version 1");
    let mut read = conn
        .prepare(
            "SELECT session_id, cwd, git_root, branch, inferred_committee, committee_source, \
             joined_committee, socket, token, claude_pid, transcript_path, state, started_at, \
             last_seen, socket_ok, runtime, agent, agent_source, host_id FROM sessions",
        )
        .unwrap();
    let rows = read
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .count();
    assert_eq!(rows, 3);
}
