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
    std::fs::write(
        &failing_fixture,
        "#!/bin/sh\nprintf 'fixture bootstrap failure\\n'\nexit 47\n",
    )
    .unwrap();
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
        error.to_string().contains("exited during startup"),
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
