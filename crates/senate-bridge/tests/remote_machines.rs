//! Two isolated bridge daemons, the normal pairing CLI, and a real PTY host.
#![allow(clippy::unwrap_used)]
#[path = "support/common.rs"]
mod common;

use common::{Harness, committee, fast_timing, name, path};
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::ipc::{self, HostedRuntime, Request, SpawnReply};
use senate_bridge::remote::{Client, Control, Device, Scopes};
use senate_bridge::{Paths, testenv};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};

const BIN: &str = env!("CARGO_BIN_EXE_senate");
const WAIT: Duration = Duration::from_secs(10);

fn senate(home: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(BIN);
    let root = std::env::var_os(testenv::CONFINE_ENV).unwrap();
    testenv::isolate_async(&mut cmd, Path::new(&root));
    cmd.env("SENATE_HOME", home).kill_on_drop(true);
    cmd
}

#[tokio::test]
async fn remote_machines_loopback() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap());
    child.args(["--exact", "remote_test_child", "--ignored", "--nocapture"]);
    testenv::isolate_async(&mut child, temp.path());
    child.env("TMPDIR", temp.path()).kill_on_drop(true);
    assert!(
        tokio::time::timeout(Duration::from_secs(80), child.status())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

async fn request<T: serde::de::DeserializeOwned>(target: &Harness, request: &Request) -> T {
    ipc::request(&target.socket(), request, WAIT, WAIT)
        .await
        .unwrap()
}

async fn control<T: serde::de::DeserializeOwned>(target: &Harness, command: Control) -> T {
    request(target, &Request::RemoteControl { command }).await
}

async fn pair(
    target: &Harness,
    client: &Harness,
    alias: &str,
    attach: bool,
    valid_secs: u64,
) -> (Device, String) {
    let code: String = control(
        target,
        Control::Pair {
            scopes: Scopes {
                attach,
                watch: true,
            },
            valid_secs,
        },
    )
    .await;
    let mut cmd = senate(client.paths.home());
    cmd.args([
        "setup",
        "remote",
        "connect",
        alias,
        "--device",
        "test-client",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped());
    let mut joining = cmd.spawn().unwrap();
    joining
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{code}\n").as_bytes())
        .await
        .unwrap();
    let mut output = BufReader::new(joining.stdout.take().unwrap());
    let mut line = String::new();
    tokio::time::timeout(WAIT, output.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let comparison = line
        .split_once("Comparison code ")
        .unwrap()
        .1
        .split('.')
        .next()
        .unwrap();
    let device: Device = control(
        target,
        Control::Confirm {
            comparison: comparison.to_owned(),
        },
    )
    .await;
    assert!(
        tokio::time::timeout(WAIT, joining.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    (device, code)
}

async fn until(stream: &mut (impl AsyncRead + Unpin), needle: &[u8]) {
    tokio::time::timeout(WAIT, async {
        let mut bytes = Vec::new();
        while !bytes.windows(needle.len()).any(|part| part == needle) {
            let mut buf = [0u8; 1024];
            let len = stream.read(&mut buf).await.unwrap();
            assert!(len != 0, "stream ended before marker");
            bytes.extend_from_slice(&buf[..len]);
        }
    })
    .await
    .unwrap();
}

async fn closed(stream: &mut (impl AsyncRead + Unpin)) {
    tokio::time::timeout(WAIT, async {
        let mut buf = [0u8; 4096];
        while matches!(stream.read(&mut buf).await, Ok(1..)) {}
    })
    .await
    .unwrap();
}

async fn terminal(client: &Harness, args: &[&str], attach: bool) {
    let mut command = senate(client.paths.home());
    command.args(args);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut child = pty_process::Command::from(command).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    if attach {
        until(&mut output, b"\x1b[c").await;
        input.write_all(b"\x1b[?62;22c").await.unwrap();
    }
    until(&mut output, b"terminal fixture").await;
    if attach {
        input.write_all(b"remote keys\r").await.unwrap();
        until(&mut output, b"got:remote keys").await;
        input.write_all(b"\x1d").await.unwrap();
    } else {
        input.write_all(b"q").await.unwrap();
    }
    assert!(
        tokio::time::timeout(WAIT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

#[tokio::test]
#[ignore = "called by the isolated loopback test"]
async fn remote_test_child() {
    assert!(std::env::var_os(testenv::CONFINE_ENV).is_some());
    let mut target = Harness::with_committees(&["refund-v2"]).await;
    let mut client = Harness::new().await;
    let project = target.workdir("project", None);
    let program = target.dir.path().join("fixture");
    let test = std::env::current_exe().unwrap();
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf 'terminal fixture\\r\\n'\n\
        while IFS= read -r line; do\n\
        if [ \"$line\" = guard ]; then\n\
          {test:?} --exact remote_agent_child --ignored > guard-result 2>&1 && touch guard-passed\n\
        else printf 'got:%s\\r\\n' \"$line\"; fi\n\
        done\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        target.paths.config(),
        format!(
            "[remote]\nlisten = \"127.0.0.1:0\"\n[companion]\nclaude_bin = {:?}\n",
            program.to_str().unwrap()
        ),
    )
    .unwrap();
    target.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: target.paths.clone(),
                config: target.config.clone(),
                timing: fast_timing(),
                desktop: None,
            },
            BIN.into(),
        )
        .await
        .unwrap(),
    );
    client.start().await;
    let disabled = ipc::request::<String>(
        &client.socket(),
        &Request::RemoteControl {
            command: Control::Pair {
                scopes: Scopes {
                    watch: true,
                    attach: false,
                },
                valid_secs: 60,
            },
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap_err();
    assert!(disabled.to_string().contains("disabled"));

    let launched: SpawnReply = request(
        &target,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: committee(),
            alias: Some(path("main")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    let (device, used) = pair(&target, &client, "target", false, 60).await;
    let remote = Client::open(&client.paths, &name("target")).unwrap();
    let rows = remote.list().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "refund-v2::main");
    let session = rows[0].session.clone();
    assert_eq!(session, launched.session_id);
    let mut viewer = remote.stream(session.clone(), false).await.unwrap();
    until(&mut viewer, b"terminal fixture").await;
    assert!(
        remote
            .stream(session.clone(), true)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("no attach grant")
    );

    terminal(&client, &["session", "watch", "--machine", "target"], false).await;
    let mut list = senate(client.paths.home());
    let result = list
        .args(["session", "list", "--all-machines"])
        .output()
        .await
        .unwrap();
    assert!(result.status.success());
    let listed = String::from_utf8(result.stdout).unwrap();
    assert!(
        listed.contains("local:") && listed.contains("target · refund-v2::main"),
        "{listed}"
    );

    let mut replay = senate(client.paths.home());
    replay
        .args([
            "setup",
            "remote",
            "connect",
            "replay",
            "--device",
            "test-client",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut replay = replay.spawn().unwrap();
    replay
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{used}\n").as_bytes())
        .await
        .unwrap();
    let result = tokio::time::timeout(WAIT, replay.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("already used"));

    let machine_path = client.paths.home().join("remote/machines/target.json");
    let good = std::fs::read(&machine_path).unwrap();
    let mut wrong: serde_json::Value = serde_json::from_slice(&good).unwrap();
    wrong["host"] = wrong["identity"]["cert"].clone();
    std::fs::write(&machine_path, serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(
        Client::open(&client.paths, &name("target"))
            .unwrap()
            .list()
            .await
            .is_err()
    );
    std::fs::write(&machine_path, &good).unwrap();

    let _: serde_json::Value = control(&target, Control::Revoke { device: device.id }).await;
    closed(&mut viewer).await;
    assert!(
        remote.list().await.is_err(),
        "revoked client key must fail a new handshake"
    );

    let (device, _) = pair(&target, &client, "interactive", true, 60).await;
    let remote = Client::open(&client.paths, &name("interactive")).unwrap();
    terminal(
        &client,
        &["attach", "--machine", "interactive", "main"],
        true,
    )
    .await;
    let mut attached = remote.stream(session.clone(), true).await.unwrap();
    assert_eq!(attached.read_u8().await.unwrap(), 3);
    attached.write_all(b"\x01\0\x18\0\x50").await.unwrap();
    let mut busy = remote.stream(session.clone(), true).await.unwrap();
    assert_eq!(busy.read_u8().await.unwrap(), 0);
    until(&mut attached, b"terminal fixture").await;
    attached.write_all(b"\x02\0\x06guard\r").await.unwrap();
    tokio::time::timeout(WAIT, async {
        while !project.join("guard-passed").exists() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|e| {
        panic!(
            "{e}: {:?}",
            std::fs::read_to_string(project.join("guard-result"))
        )
    });
    let _: serde_json::Value = control(&target, Control::Revoke { device: device.id }).await;
    closed(&mut attached).await;

    let (_, _) = pair(&target, &client, "short-lived", false, 3).await;
    let short = Client::open(&client.paths, &name("short-lived")).unwrap();
    let mut watch = short.stream(session, false).await.unwrap();
    until(&mut watch, b"terminal fixture").await;
    closed(&mut watch).await;

    let mut list = senate(client.paths.home());
    let result = list
        .args(["session", "list", "--all-machines"])
        .output()
        .await
        .unwrap();
    assert!(result.status.success());
    let listed = String::from_utf8(result.stdout).unwrap();
    assert!(listed.contains("target:\n  unreachable"), "{listed}");

    let audit = std::fs::read_to_string(target.paths.home().join("remote/audit.jsonl")).unwrap();
    for action in [
        "connect",
        "list",
        "watch",
        "attach_denied",
        "attach",
        "revoke",
    ] {
        assert!(
            audit.contains(&format!("\"action\":\"{action}\"")),
            "{audit}"
        );
    }
    client.stop().await;
    target.stop().await;
}

#[tokio::test]
#[ignore = "called from the hosted fixture's process tree"]
async fn remote_agent_child() {
    assert!(std::env::var_os(testenv::CONFINE_ENV).is_some());
    let paths = Paths::from_env().unwrap();
    for command in [
        Control::Pair {
            scopes: Scopes {
                watch: true,
                attach: true,
            },
            valid_secs: 60,
        },
        Control::Confirm {
            comparison: "123456".into(),
        },
    ] {
        let result = ipc::request::<serde_json::Value>(
            &paths.socket(),
            &Request::RemoteControl { command },
            WAIT,
            WAIT,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(result, ipc::ClientError::Remote(error) if error.code == ipc::codes::NOT_A_PERSON && error.message.contains("process tree"))
        );
    }
}
