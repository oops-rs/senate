//! Exercise a real Senate terminal host and attach client with a tiny TUI.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use common::{Harness, committee, fast_timing, path};
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::ipc::{self, HostedRuntime, Request, SpawnReply};
use senate_bridge::testenv;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const BIN: &str = env!("CARGO_BIN_EXE_senate");
const TIMEOUT: Duration = Duration::from_secs(5);

async fn start(h: &mut Harness) {
    h.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: h.paths.clone(),
                config: h.config.clone(),
                timing: fast_timing(),
                desktop: None,
            },
            BIN.into(),
        )
        .await
        .unwrap(),
    );
}

async fn call<T: serde::de::DeserializeOwned>(h: &Harness, request: &Request) -> T {
    ipc::request(&h.socket(), request, TIMEOUT, TIMEOUT)
        .await
        .unwrap()
}

async fn attach(h: &Harness, alias: &str, keys: &[u8]) {
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(["attach", alias])
        .env("SENATE_HOME", h.paths.home())
        .kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    let mut observed = Vec::new();
    tokio::time::timeout(TIMEOUT, async {
        let mut buf = [0; 4096];
        loop {
            let n = output.read(&mut buf).await.unwrap();
            assert_ne!(n, 0);
            observed.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&observed).contains("fixture TUI") {
                return;
            }
        }
    })
    .await
    .unwrap();

    input.write_all(keys).await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixture_terminal_launch_detach_restart_and_restore() {
    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    let project = h.workdir("terminal", None);
    let fixture = h.dir.path().join("fixture-claude");
    std::fs::write(
        &fixture,
        r#"#!/bin/sh
case "$1" in --session-id=?*|--resume=?*) ;; *) exit 64 ;; esac
printf '%s\n' "$1" >> argv.log
(while :; do printf 'fixture TUI\n'; sleep 0.1; done) &
painter=$!
trap 'kill "$painter" 2>/dev/null; wait "$painter" 2>/dev/null' EXIT
while IFS= read -r line; do
    [ "$line" = q ] && exit 0
done
"#,
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    let failing_fixture = h.dir.path().join("failing-claude");
    std::fs::write(&failing_fixture, "#!/senate-test-no-such-interpreter\n").unwrap();
    std::fs::set_permissions(&failing_fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    h.paths.ensure_home().unwrap();
    let config = h.paths.config();
    let choose = |program: &std::path::Path| {
        std::fs::write(
            &config,
            format!(
                "[companion]\nclaude_bin = {:?}\n",
                program.to_str().unwrap()
            ),
        )
        .unwrap();
    };
    choose(&failing_fixture);
    start(&mut h).await;

    let request = Request::Spawn {
        session: Default::default(),
        runtime: HostedRuntime::Claude,
        committee: committee(),
        alias: Some(path("fixture")),
        cwd: project.clone(),
        chair: false,
    };
    let error = ipc::request::<SpawnReply>(&h.socket(), &request, TIMEOUT, TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("start coding TUI on its PTY"),
        "{error}"
    );
    assert!(h.store().all_sessions().unwrap().is_empty());
    {
        let server = h.server.state.inner.lock().unwrap();
        assert!(server.aliases.is_empty());
        assert!(server.roster.seated().is_empty());
        assert!(server.cursors.is_empty());
        assert!(server.stream_opens.is_empty());
    }

    h.stop().await;
    choose(&fixture);
    start(&mut h).await;
    let launched: SpawnReply = call(&h, &request).await;
    assert_eq!(
        std::fs::read_to_string(project.join("argv.log"))
            .unwrap()
            .trim(),
        format!("--session-id={}", launched.session_id)
    );
    // This second child starts successfully and reaches Ready. Failure of
    // the final alias operation must unwind its staged session and cursor.
    let conflict = ipc::request::<SpawnReply>(&h.socket(), &request, TIMEOUT, TIMEOUT)
        .await
        .unwrap_err();
    assert!(conflict.to_string().contains("already taken"), "{conflict}");
    assert_eq!(h.store().all_sessions().unwrap().len(), 1);
    {
        let server = h.server.state.inner.lock().unwrap();
        assert_eq!(
            server.aliases.get(&path("fixture")),
            Some(&launched.senator)
        );
        assert!(
            server
                .roster
                .seated()
                .iter()
                .all(|agent| agent.name == launched.senator)
        );
    }
    let attach_target = Request::Attach {
        senator: launched.senator.clone(),
    };
    let socket: PathBuf = call(&h, &attach_target).await;
    attach(&h, launched.senator.as_str(), b"\x1b[93;5u").await;
    assert!(socket.exists());

    h.stop().await;
    assert!(socket.exists());
    start(&mut h).await;
    assert_eq!(call::<PathBuf>(&h, &attach_target).await, socket);
    attach(&h, launched.senator.as_str(), b"\x1d").await;

    // The runtime can exit while the daemon is unavailable. No SessionEnd
    // will reach the registry, so the restarted daemon must use the lock.
    h.stop().await;
    let mut terminal = tokio::net::UnixStream::connect(&socket).await.unwrap();
    assert_eq!(terminal.read_u8().await.unwrap(), 1);
    terminal.write_all(b"\x02\x00\x02q\r").await.unwrap();
    tokio::time::timeout(TIMEOUT, async {
        while socket.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    start(&mut h).await;
    assert_eq!(
        h.store()
            .session(&launched.session_id)
            .unwrap()
            .unwrap()
            .state,
        senate_bridge::SessionState::Stale
    );

    let restored: SpawnReply = call(
        &h,
        &Request::Restore {
            session: Default::default(),
            senator: launched.senator.clone(),
        },
    )
    .await;
    assert_eq!(restored, launched);
    assert!(
        std::fs::read_to_string(project.join("argv.log"))
            .unwrap()
            .lines()
            .any(|arg| arg == format!("--resume={}", restored.session_id))
    );
    let socket: PathBuf = call(&h, &attach_target).await;
    attach(&h, restored.senator.as_str(), b"q\r").await;
    tokio::time::timeout(TIMEOUT, async {
        while socket.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    h.stop().await;
}

#[tokio::test]
async fn host_ready_without_publication_closes_its_tui_group() {
    use std::process::Stdio;

    let root = tempfile::tempdir_in("/tmp").unwrap();
    let paths = senate_bridge::Paths::new(root.path());
    let id = senate_core::EventId::new();
    let directory = root.path().join("hosts").join(id.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    let program = root.path().join("fixture");
    std::fs::write(
        &program,
        "#!/bin/sh\ntrap 'touch stopped; exit 0' TERM\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, root.path());
    cmd.args(["session-host", &id.to_string(), "claude"])
        .arg(root.path())
        .args(["native-id", "--program"])
        .arg(program)
        .env("SENATE_HOME", paths.home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    let ready = tokio::time::timeout(TIMEOUT, child.stdout.as_mut().unwrap().read_u8())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready, 1);
    drop(child.stdin.take()); // The starting daemon stopped before publishing.
    let exit = tokio::time::timeout(TIMEOUT, child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!exit.success());
    assert!(!directory.exists());
    assert!(
        root.path().join("stopped").exists(),
        "the TUI did not receive TERM"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_refuses_a_reachable_claude_and_keeps_the_servers_backlog() {
    use senate_bridge::agent::{self, AgentSource};
    use senate_bridge::session::{CommitteeSource, Runtime};
    use senate_bridge::state::{SessionUpsert, Store};
    use senate_core::{Address, Kind, NewEvent, Seq};

    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("restoring", None);
    let program = h.dir.path().join("fixture");
    std::fs::write(
        &program,
        "#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[companion]\nclaude_bin = {:?}\n",
            program.to_str().unwrap()
        ),
    )
    .unwrap();

    let senator = agent::session_name(Runtime::Claude, "restore-fixture");
    let committee = committee();
    let socket = h.dir.path().join("manual.sock");
    let manual = tokio::net::UnixListener::bind(&socket).unwrap();
    {
        let store = Store::open(&h.paths.state_db()).unwrap();
        store
            .upsert_session(&SessionUpsert {
                session_id: "restore-fixture".into(),
                cwd: project,
                git_root: None,
                branch: None,
                inferred_committee: Some((committee.clone(), CommitteeSource::File)),
                socket: Some(socket),
                token: None,
                claude_pid: None,
                transcript_path: None,
                ancestor_pids: Vec::new(),
                runtime: Runtime::Claude,
                agent: senator.clone(),
                agent_source: AgentSource::Session,
            })
            .unwrap();
        store
            .set_session_state("restore-fixture", senate_bridge::SessionState::Ended)
            .unwrap();
    }
    let sender = Address::senior(&h.config.person);
    let first = h.server.append(NewEvent::new(
        committee.clone(),
        Kind::Note,
        sender.clone(),
        "already seen",
    ));
    let waiting = h.server.append(
        NewEvent::new(
            committee.clone(),
            Kind::Question,
            sender,
            "send this after restore",
        )
        .to(Address::senator(&senator)),
    );
    h.server.state.inner.lock().unwrap().cursors.insert(
        (
            committee.to_string(),
            senator.to_string(),
            "delivered".into(),
        ),
        first.seq,
    );
    start(&mut h).await;
    let restore = Request::Restore {
        session: Default::default(),
        senator: senator.clone(),
    };
    let refused = ipc::request::<SpawnReply>(&h.socket(), &restore, TIMEOUT, TIMEOUT)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("still running"), "{refused}");
    drop(manual);

    let hosted: SpawnReply = call(&h, &restore).await;
    assert_eq!(hosted.senator, senator);
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if h.store()
                .item(&senator, &committee, waiting.seq)
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(waiting.seq > Seq::ZERO);
    let binding: PathBuf = call(&h, &Request::Attach { senator }).await;
    let child_pid = std::fs::read_to_string(binding.parent().unwrap().join("owner"))
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    let group = rustix::process::Pid::from_raw(child_pid).unwrap();
    rustix::process::kill_process_group(group, rustix::process::Signal::TERM).unwrap();
    tokio::time::timeout(TIMEOUT, async {
        while binding.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    h.stop().await;
}
