//! Two isolated bridge daemons, the normal pairing CLI, and a real PTY host.
#![allow(clippy::unwrap_used)]
#[path = "support/common.rs"]
mod common;

use common::{Harness, committee, fast_timing, name, path};
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::ipc::{self, HostedRuntime, Request, SpawnReply};
use senate_bridge::remote::{Client, Control, Device, Scopes, authorize_isolated_test};
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
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
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

fn admin(target: &Harness, args: &[&str], answer: &str) -> String {
    authorize_isolated_test(
        &target.paths,
        &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
        answer,
    )
    .unwrap()
}

async fn pair(
    target: &Harness,
    client: &Harness,
    person: &Paths,
    alias: &str,
    attach: bool,
    watch: bool,
    valid_secs: u64,
) -> (Device, String) {
    let mut command = senate(person.home());
    command.args([
        "setup",
        "remote",
        "pair",
        "--valid-secs",
        &valid_secs.to_string(),
    ]);
    if attach {
        command.arg("--attach");
    } else if !watch {
        command.arg("--list-only");
    }
    command.stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut pairing = command.spawn().unwrap();
    drop(pairing.stdin.take());
    let mut target_out = BufReader::new(pairing.stdout.take().unwrap());
    let mut header = String::new();
    let mut code = String::new();
    tokio::time::timeout(WAIT, async {
        target_out.read_line(&mut header).await.unwrap();
        target_out.read_line(&mut code).await.unwrap();
    })
    .await
    .unwrap();
    assert!(header.contains("Pairing code"), "{header}");
    let code = code.trim().to_owned();
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
    let mut client_details = String::new();
    tokio::time::timeout(WAIT, output.read_line(&mut client_details))
        .await
        .unwrap()
        .unwrap();
    assert!(client_details.contains("list, watch=") && client_details.contains("expires"));
    let mut candidate = String::new();
    tokio::time::timeout(WAIT, target_out.read_line(&mut candidate))
        .await
        .unwrap()
        .unwrap();
    assert!(
        candidate.contains("Candidate test-client") && candidate.contains("key "),
        "{candidate}"
    );
    let mut command = String::new();
    tokio::time::timeout(WAIT, target_out.read_line(&mut command))
        .await
        .unwrap()
        .unwrap();
    let command = command.trim();
    let request = command
        .strip_prefix("sudo -k /usr/local/libexec/senate-remote-admin enroll ")
        .unwrap();
    let devices: Vec<Device> = control(target, Control::Devices).await;
    assert!(
        !devices
            .iter()
            .any(|device| candidate.contains(&device.id[..16]))
    );
    assert!(
        authorize_isolated_test(&target.paths, &["enroll".into(), request.into()], "wrong")
            .is_err()
    );
    let approval = admin(target, &["enroll", request], comparison);
    assert!(approval.contains("test-client") && approval.contains("attach="));
    let compact = comparison.replace('-', "");
    for displayed in [&approval, &candidate] {
        assert!(!displayed.contains(comparison));
        assert!(!displayed.contains(&compact));
        assert!(!displayed.as_bytes().windows(20).any(|window| {
            window
                .iter()
                .all(|byte| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(byte))
        }));
    }

    assert!(
        tokio::time::timeout(WAIT, pairing.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(
        tokio::time::timeout(WAIT, joining.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let devices: Vec<Device> = control(target, Control::Devices).await;
    let device = devices
        .into_iter()
        .find(|device| candidate.contains(&device.id[..16]))
        .unwrap();
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
        input.write_all(b"\x1b[?0u\x1b[?62;22c").await.unwrap();
        until(&mut output, b"\x1b[=7u").await;
        input.write_all(b"remote keys \x1b[1;").await.unwrap();
        input.write_all(b"5:2A\r").await.unwrap();
        until(&mut output, b"got:remote keys \x1b[1;5:2A").await;
        input.write_all(b"\x1d").await.unwrap();
    } else {
        until(&mut output, b"terminal fixture").await;
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
    let recording = senate_bridge::notify::Recording::new();
    target.desktop = Some(senate_bridge::notify::Desktop::new(recording.clone()));
    let mut client = Harness::new().await;
    let project = target.workdir("project", None);
    let program = target.dir.path().join("fixture");
    let test = std::env::current_exe().unwrap();
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '\\033[>7uterminal fixture\\r\\n'\n\
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
            "[remote]\nexperimental = true\nlisten = \"127.0.0.1:0\"\n[companion]\nclaude_bin = {:?}\n",
            program.to_str().unwrap()
        ),
    )
    .unwrap();
    let valid_config = std::fs::read(target.paths.config()).unwrap();
    std::fs::write(target.paths.config(), "[remote]\nexperimental = \"bad\"\n").unwrap();
    target.start().await;
    let _: ipc::StatusReply = request(&target, &Request::Status).await;
    target.stop().await;
    std::fs::write(target.paths.config(), valid_config).unwrap();
    // The old local listener flag alone cannot enable machine access.
    target.start().await;
    let unauthorized = ipc::request::<String>(
        &target.socket(),
        &Request::RemoteControl {
            command: Control::PrepareEnroll,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap_err();
    assert!(unauthorized.to_string().contains("disabled"));
    let enable: String = control(
        &target,
        Control::PrepareEnable {
            listen: "127.0.0.1:0".parse().unwrap(),
        },
    )
    .await;
    admin(&target, &["enable", &enable], "enable");
    target.stop().await;

    // A user-owned legacy registry is outside the authority store.
    std::fs::write(
        target.paths.home().join("remote/devices-v2.json"),
        "tampered user registry",
    )
    .unwrap();
    target.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: target.paths.clone(),
                config: target.config.clone(),
                timing: fast_timing(),
                desktop: target.desktop.clone(),
            },
            BIN.into(),
        )
        .await
        .unwrap(),
    );
    // The CLI binary speaks its real wire protocol. In-process test clients
    // stand in for an external person because this test itself runs under
    // a coding session. Every hosted-agent refusal below uses the real socket.
    let person = Paths::new(target.dir.path().join("cli"));
    std::fs::create_dir(person.home()).unwrap();
    let forwarding = tokio::net::UnixListener::bind(person.socket()).unwrap();
    let target_socket = target.socket();
    let person_fixture = tokio::spawn(async move {
        while let Ok((mut client, _)) = forwarding.accept().await {
            let Ok(mut bridge) = tokio::net::UnixStream::connect(&target_socket).await else {
                break;
            };
            let _ = tokio::io::copy_bidirectional(&mut client, &mut bridge).await;
        }
    });
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
    let (device, used) = pair(&target, &client, &person, "target", false, true, 60).await;
    let remote = Client::open(&client.paths, &name("target")).unwrap();
    let rows = remote.list().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, "refund-v2::main");
    let session = rows[0].session.clone();
    assert_eq!(session, launched.session_id);
    client.server.state.inner.lock().unwrap().chair = Some((launched.senator.clone(), true));
    let role_elsewhere = senate(client.paths.home())
        .args(["attach", "refund-v2::chair"])
        .output()
        .await
        .unwrap();
    assert!(!role_elsewhere.status.success());
    let hint = String::from_utf8_lossy(&role_elsewhere.stderr);
    assert!(
        hint.contains("session on target")
            && hint.contains("senate attach --machine target refund-v2::chair"),
        "{hint}"
    );
    pair(&target, &client, &person, "metadata", false, false, 60).await;
    let metadata = Client::open(&client.paths, &name("metadata")).unwrap();
    assert_eq!(metadata.list().await.unwrap().len(), 1);
    assert!(
        metadata
            .stream(session.clone(), false)
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("no watch grant")
    );
    let mut input_watch = remote.stream(session.clone(), false).await.unwrap();
    until(&mut input_watch, b"terminal fixture").await;
    input_watch
        .write_all(b"unexpected watch input")
        .await
        .unwrap();
    closed(&mut input_watch).await;
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
    assert!(String::from_utf8_lossy(&result.stderr).contains("pairing unavailable"));

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

    let root = std::path::PathBuf::from(std::env::var_os(testenv::CONFINE_ENV).unwrap());
    let profile = root
        .join("authorization")
        .join(format!("{}.v3.json", senate_bridge::service::current_uid()));
    let older_profile = std::fs::read(&profile).unwrap();
    admin(&target, &["revoke", &device.id], "revoke");
    closed(&mut viewer).await;
    assert!(
        remote.list().await.is_err(),
        "revoked client key must fail a new handshake"
    );

    let (device, _) = pair(&target, &client, &person, "interactive", true, true, 60).await;
    let remote = Client::open(&client.paths, &name("interactive")).unwrap();
    terminal(
        &client,
        &["attach", "--machine", "interactive", "refund-v2::chair"],
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
    let audit_path = target.paths.home().join("remote/audit-important.jsonl");
    let audit_backup = audit_path.with_extension("saved");
    tokio::time::timeout(WAIT, async {
        loop {
            if std::fs::read_to_string(target.paths.home().join("remote/audit-attach.jsonl"))
                .is_ok_and(|log| log.contains("\"action\":\"attach\""))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    std::fs::rename(&audit_path, &audit_backup).unwrap();
    std::fs::create_dir(&audit_path).unwrap();
    admin(&target, &["revoke", &device.id], "revoke");
    closed(&mut attached).await;

    let (_, _) = pair(&target, &client, &person, "short-lived", false, true, 3).await;
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

    let audit = std::fs::read_to_string(audit_backup).unwrap()
        + &std::fs::read_to_string(target.paths.home().join("remote/audit.jsonl")).unwrap()
        + &std::fs::read_to_string(target.paths.home().join("remote/audit-attach.jsonl")).unwrap();
    for action in [
        "pair",
        "connect",
        "list",
        "watch",
        "attach_denied",
        "attach",
        "revoke",
        "disconnect",
    ] {
        assert!(
            audit.contains(&format!("\"action\":\"{action}\"")),
            "{audit}"
        );
    }
    assert!(!audit.contains("enrollment_connect"), "{audit}");

    let latest_profile = std::fs::read(&profile).unwrap();
    std::fs::write(&profile, &older_profile).unwrap();
    tokio::time::timeout(WAIT, async {
        while metadata.list().await.is_ok() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let _: ipc::StatusReply = request(&target, &Request::Status).await;
    std::fs::write(&profile, &latest_profile).unwrap();
    tokio::time::timeout(WAIT, async {
        while metadata.list().await.is_err() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        remote.list().await.is_err(),
        "restoring current policy must not restore the revoked grant"
    );
    admin(&target, &["disable"], "disable");
    tokio::time::timeout(WAIT, async {
        while metadata.list().await.is_ok() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let _: ipc::StatusReply = request(&target, &Request::Status).await;
    tokio::time::timeout(WAIT, async {
        loop {
            let entries = recording.posted();
            if entries
                .iter()
                .any(|(_, body)| body.contains("listener was enabled"))
                && entries
                    .iter()
                    .any(|(_, body)| body.contains("device was enrolled"))
                && entries
                    .iter()
                    .any(|(_, body)| body.contains("listener was disabled"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let status = senate(target.paths.home())
        .arg("status")
        .output()
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&status.stdout).contains("remote listener was disabled"));
    let doctor = senate(target.paths.home())
        .args(["setup", "doctor", "--json"])
        .output()
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&doctor.stdout).contains("remote.last_administration"));
    // A trusted disable also advances the rollback floor.
    std::fs::write(&profile, latest_profile).unwrap();
    tokio::time::sleep(Duration::from_millis(550)).await;
    assert!(metadata.list().await.is_err());
    person_fixture.abort();
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
        Control::PrepareEnroll,
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
