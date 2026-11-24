#![allow(clippy::unwrap_used)]

use std::process::Stdio;
use std::time::Duration;

use senate_bridge::agent::AgentSource;
use senate_bridge::ipc::{self, Pong, Request, StatusReply, StreamState};
use senate_bridge::session::Runtime;
use senate_bridge::state::{SessionUpsert, Store};
use senate_bridge::{Paths, testenv};

const AGENTS: usize = 10;
const COMMITTEES: usize = 25;

#[tokio::test]
async fn machine_daemon_serves_many_seats_with_a_hard_limit_of_128() {
    // Darwin Unix socket names have a short limit.
    let root = tempfile::Builder::new()
        .prefix("senate-fd-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = root.path().join("s");
    let paths = Paths::new(&home);
    paths.ensure_home().unwrap();
    let user = root.path().join("u");
    std::fs::create_dir(&user).unwrap();

    let committees: Vec<_> = (0..COMMITTEES)
        .map(|i| format!("\"fixture-{i}\""))
        .collect();
    std::fs::write(
        paths.config(),
        format!(
            "person = \"alice\"\ncommittees = [{}]\n[local]\nlisten = \"127.0.0.1:0\"\n",
            committees.join(", ")
        ),
    )
    .unwrap();
    let transcript = root.path().join("transcript.jsonl");
    std::fs::write(
        &transcript,
        r#"{"type":"user","timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"A synthetic prompt."}}"#.to_owned() + "\n",
    )
    .unwrap();
    {
        let state = Store::open(&paths.state_db()).unwrap();
        for i in 0..AGENTS {
            state
                .upsert_session(&SessionUpsert {
                    session_id: format!("descriptor-fixture-{i}"),
                    cwd: root.path().to_owned(),
                    git_root: None,
                    branch: None,
                    inferred_committee: None,
                    socket: None,
                    token: None,
                    claude_pid: None,
                    transcript_path: Some(transcript.display().to_string()),
                    ancestor_pids: Vec::new(),
                    runtime: Runtime::Claude,
                    agent: format!("fixture-agent-{i}").parse().unwrap(),
                    agent_source: AgentSource::Session,
                })
                .unwrap();
        }
    }

    let log_path = root.path().join("senated.log");
    let mut daemon = tokio::process::Command::new("/bin/sh")
        .args([
            "-ec",
            "ulimit -Sn 128; ulimit -Hn 128; exec \"$@\"",
            "senated",
        ])
        .arg(env!("CARGO_BIN_EXE_senated"))
        .arg("--machine")
        .arg("--home")
        .arg(&home)
        .env("HOME", &user)
        .env("SENATE_HOME", &home)
        .env(testenv::CONFINE_ENV, root.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log_path).unwrap())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let socket = paths.socket();
    let connect = Duration::from_millis(250);
    let reply = Duration::from_secs(2);
    let poll = async {
        loop {
            if ipc::request::<Pong>(&socket, &Request::Ping, connect, reply)
                .await
                .is_ok()
                && let Ok(status) =
                    ipc::request::<StatusReply>(&socket, &Request::Status, connect, reply).await
                && status
                    .committees
                    .iter()
                    .filter(|row| matches!(row.stream, Some(StreamState::Connected { .. })))
                    .count()
                    == AGENTS * COMMITTEES
            {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(30), poll).await;
    daemon.kill().await.unwrap();
    let status = result.unwrap_or_else(|_| {
        let log = std::fs::read_to_string(log_path).unwrap();
        panic!("daemon did not connect all seats under the descriptor limit\n{log}");
    });
    let descriptors = status.descriptors.unwrap();
    assert_eq!(descriptors.soft_limit, Some(128));
    assert!(
        descriptors.open.is_some_and(|open| open <= 64),
        "{descriptors:?}"
    );
}
