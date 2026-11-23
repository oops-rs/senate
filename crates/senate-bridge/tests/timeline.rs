//! A person's session timeline through a real local bridge socket.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use common::Harness;
use senate_bridge::ipc::{
    self, HookEvent, HookReply, HookReport, Request, SessionsReply, TimelineDirection,
    TimelineKind, TimelineReply, TimelineToolDetail, TimelineToolStatus, TimelineUpdate,
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

async fn watch(bridge: &Harness, after: u64) -> BufReader<UnixStream> {
    let mut connection = UnixStream::connect(bridge.socket()).await.unwrap();
    let mut request = serde_json::to_vec(&Request::TimelineWatch {
        session_id: SESSION.into(),
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

    let mut reader = watch(&h, tail.last_seq.unwrap()).await;
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
    let mut late_watch = watch(&h, 4).await;
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
