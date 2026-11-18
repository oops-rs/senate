//! Exercise a real Senate terminal host and attach client with a tiny TUI.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use common::{Harness, committee, eventually, fast_timing, path};
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
        .unwrap_or_else(|error| panic!("{request:?}: {error}"))
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
    h.config.hosted_session_limit = 1;
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

    let spawn = |alias| Request::Spawn {
        session: Default::default(),
        runtime: HostedRuntime::Claude,
        committee: committee(),
        alias,
        cwd: project.clone(),
        chair: false,
    };
    let request = spawn(Some(path("fixture")));
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
    h.config.committees.push(committee());
    start(&mut h).await;
    h.server.state.inner.lock().unwrap().reject_appends = true;
    let error = ipc::request::<SpawnReply>(&h.socket(), &request, TIMEOUT, TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("injected log refusal"),
        "{error}"
    );
    h.server.state.inner.lock().unwrap().reject_appends = false;
    assert!(h.store().all_sessions().unwrap().is_empty());
    assert!(h.server.events(committee().as_str()).is_empty());
    {
        let server = h.server.state.inner.lock().unwrap();
        assert!(server.aliases.is_empty());
        assert!(server.roster.seated().is_empty());
        assert!(server.stream_opens.is_empty());
    }

    let db = rusqlite::Connection::open(h.paths.state_db()).unwrap();
    db.busy_timeout(TIMEOUT).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_host_publication BEFORE INSERT ON sessions
         WHEN NEW.host_id IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'injected publication failure'); END;",
    )
    .unwrap();
    let error = ipc::request::<SpawnReply>(&h.socket(), &spawn(None), TIMEOUT, TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("injected publication failure"),
        "{error}"
    );
    let notes = h.server.events(committee().as_str());
    let [ready, rollback] = notes.as_slice() else {
        panic!("expected a ready audit and a rollback note: {notes:?}");
    };
    assert!(
        ready
            .body
            .contains("[senator::alice] spawned [senator::claude-"),
        "{ready:?}"
    );
    assert!(
        rollback
            .body
            .contains("was rolled back after publication failed"),
        "{rollback:?}"
    );
    assert!(
        rollback.body.contains("injected publication failure"),
        "{rollback:?}"
    );
    for event in [ready, rollback] {
        assert_eq!(event.from, common::person("alice"));
        assert!(event.body.contains("(claude)"), "{event:?}");
    }
    assert!(h.store().all_sessions().unwrap().is_empty());
    {
        let server = h.server.state.inner.lock().unwrap();
        assert!(server.aliases.is_empty());
        assert!(server.roster.seated().is_empty());
        assert!(server.stream_opens.is_empty());
    }
    db.execute_batch("DROP TRIGGER fail_host_publication;")
        .unwrap();
    drop(db);

    // Success at a limit of one verifies both failures released capacity.
    let launched: SpawnReply = call(&h, &request).await;
    let expected_arg = format!("--session-id={}", launched.session_id);
    assert_eq!(
        std::fs::read_to_string(project.join("argv.log"))
            .unwrap()
            .lines()
            .last(),
        Some(expected_arg.as_str())
    );
    let attach_target = Request::Attach {
        senator: launched.senator.clone(),
    };
    let socket: PathBuf = call(&h, &attach_target).await;
    attach(&h, launched.senator.as_str(), b"\x1b[93;5u").await;
    assert!(socket.exists());

    h.stop().await;
    assert!(socket.exists());
    // Give the conflict attempt a slot beside the surviving host.
    h.config.hosted_session_limit = 2;
    start(&mut h).await;
    // This second child starts successfully and reaches Ready. Failure of
    // the final alias operation must unwind its staged session and cursor.
    h.server.state.inner.lock().unwrap().alias_delay = Duration::from_millis(350);
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
    use rustix::io::Errno;
    use rustix::process::{Pid, getpgid, test_kill_process_group};
    use std::process::Stdio;

    let root = tempfile::tempdir_in("/tmp").unwrap();
    let paths = senate_bridge::Paths::new(root.path());
    let id = senate_core::EventId::new();
    let directory = root.path().join("hosts").join(id.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    let program = root.path().join("fixture");
    std::fs::write(
        &program,
        "#!/bin/sh\nsleep 300 &\nprintf '%s %s\\n' \"$$\" \"$!\" > group.tmp\nmv group.tmp group\nwait\n",
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
    let (leader, helper) = eventually("the fixture's process group", TIMEOUT, || {
        let pids = std::fs::read_to_string(root.path().join("group")).ok()?;
        let (leader, helper) = pids.trim().split_once(' ')?;
        Some((
            Pid::from_raw(leader.parse().ok()?)?,
            Pid::from_raw(helper.parse().ok()?)?,
        ))
    })
    .await;
    assert_eq!(getpgid(Some(helper)).unwrap(), leader);

    drop(child.stdin.take()); // The starting daemon stopped before publishing.
    let exit = tokio::time::timeout(TIMEOUT, child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!exit.success());
    assert!(!directory.exists());
    eventually("the unpublished TUI group to exit", TIMEOUT, || {
        (test_kill_process_group(leader) == Err(Errno::SRCH)).then_some(())
    })
    .await;
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
                socket: Some(socket.clone()),
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
    std::fs::remove_file(socket).unwrap();

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

#[tokio::test]
async fn a_tui_that_exits_immediately_never_reports_ready() {
    use std::process::Stdio;
    let root = tempfile::tempdir_in("/tmp").unwrap();
    let id = senate_core::EventId::new();
    std::fs::create_dir_all(root.path().join("hosts").join(id.to_string())).unwrap();
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, root.path());
    cmd.args(["session-host", &id.to_string(), "claude"])
        .arg(root.path())
        .args(["native-id", "--program", "/bin/sh"])
        .env("SENATE_HOME", root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(TIMEOUT, cmd.output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("exited during startup") && error.contains("exit status:"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_native_end_during_or_after_restore_does_not_leave_a_live_row() {
    use senate_bridge::SessionState;
    use senate_bridge::agent::{self, AgentSource};
    use senate_bridge::session::{CommitteeSource, Runtime};
    use senate_bridge::state::{SessionUpsert, Store};
    use senate_core::{Address, Kind, NewEvent};

    for native_end in [Some(true), Some(false), None] {
        let mut h = Harness::new().await;
        h.dir = tempfile::tempdir_in("/tmp").unwrap();
        h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
        h.paths.ensure_home().unwrap();
        let project = h.workdir("codex", None);
        let program = h.dir.path().join("fixture");
        std::fs::write(
            &program,
            "#!/bin/sh\nwhile :; do printf 'fixture TUI\\n'; sleep 0.1; done\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let remote_socket = h.dir.path().join("codex.sock");
        let fake = h.codex();
        let id = "native-codex";
        fake.idle(id);
        std::fs::write(
            h.paths.config(),
            format!(
                "[codex]\nbin = {:?}\nlisten = {:?}\n",
                program.to_str().unwrap(),
                format!("unix://{}", remote_socket.display()),
            ),
        )
        .unwrap();
        let senator = agent::session_name(Runtime::Codex, id);
        let committee = committee();
        let prehistory = h.server.append(NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::senior(&h.config.person),
            "pre-existing history",
        ));
        {
            let store = Store::open(&h.paths.state_db()).unwrap();
            store
                .upsert_session(&SessionUpsert {
                    session_id: id.into(),
                    cwd: project,
                    git_root: None,
                    branch: None,
                    inferred_committee: Some((committee.clone(), CommitteeSource::File)),
                    socket: None,
                    token: None,
                    claude_pid: None,
                    transcript_path: None,
                    ancestor_pids: Vec::new(),
                    runtime: Runtime::Codex,
                    agent: senator.clone(),
                    agent_source: AgentSource::Session,
                })
                .unwrap();
            if native_end != Some(false) {
                // Keep the initial worker from seeding a cursor before this
                // test opens the hosted terminal.
                store.set_session_state(id, SessionState::Ended).unwrap();
            }
        }
        start(&mut h).await;
        let session_end = Request::SessionEnd {
            session_id: id.into(),
            reason: None,
        };
        let request = Request::Restore {
            session: Default::default(),
            senator: senator.clone(),
        };
        let restored: SpawnReply = if native_end == Some(true) {
            h.store().set_session_state(id, SessionState::Live).unwrap();
            let (result, ()) = tokio::join!(call(&h, &request), async {
                tokio::time::timeout(TIMEOUT, async {
                    loop {
                        let base = h.paths.home().join("hosts");
                        if std::fs::read_dir(base)
                            .ok()
                            .into_iter()
                            .flatten()
                            .flatten()
                            .any(|dir| dir.path().join("owner").is_file())
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
                let _: serde_json::Value = call(&h, &session_end).await;
                assert_eq!(
                    h.store().session(id).unwrap().unwrap().state,
                    SessionState::Ended
                );
            });
            result
        } else {
            let result = call(&h, &request).await;
            if native_end == Some(false) {
                let _: serde_json::Value = call(&h, &session_end).await;
                assert_eq!(
                    h.store().session(id).unwrap().unwrap().state,
                    SessionState::Live
                );
            }
            result
        };
        assert_eq!(restored.session_id, id);
        h.wait_stream_of(committee.as_str(), senator.as_str()).await;
        // The Ready audit itself adds a log event. Check the worker's
        // initial offset rather than a cursor that intake can advance.
        let opening = h
            .server
            .stream_opens()
            .into_iter()
            .find(|stream| {
                stream.accepted
                    && stream.committee == committee.as_str()
                    && stream.senator.as_deref() == Some(senator.as_str())
            })
            .expect("the restored senator opened its stream");
        assert!(
            opening.after >= prehistory.seq.0,
            "the stream should skip pre-existing history: {opening:?}"
        );
        if native_end.is_none() {
            assert_eq!(opening.after, prehistory.seq.0);
        }
        let socket: PathBuf = call(&h, &Request::Attach { senator }).await;
        let pid = std::fs::read_to_string(socket.parent().unwrap().join("owner")).unwrap();
        let group = rustix::process::Pid::from_raw(pid.trim().parse().unwrap()).unwrap();
        rustix::process::kill_process_group(group, rustix::process::Signal::TERM).unwrap();
        tokio::time::timeout(TIMEOUT, async {
            while socket.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            h.store().session(id).unwrap().unwrap().state,
            SessionState::Ended
        );
        h.stop().await;
    }
}

struct HostedClaude {
    h: Harness,
    seat: SpawnReply,
    project: PathBuf,
    terminal: PathBuf,
}

impl Drop for HostedClaude {
    fn drop(&mut self) {
        let _ = self.signal_terminal();
    }
}

impl HostedClaude {
    async fn launch(companion: bool) -> Self {
        let mut h = Harness::new().await;
        h.dir = tempfile::tempdir_in("/tmp").unwrap();
        h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
        h.paths.ensure_home().unwrap();
        let project = h.workdir("switch", None);
        let program = h.dir.path().join("fixture");
        let senate = BIN.replace('\'', "'\"'\"'");
        std::fs::write(
            &program,
            format!(
                r#"#!/bin/sh
output="${{0%/*}}"
if [ "$1" = "-p" ]; then
    cat >/dev/null
    printf '%s\n' "$@" > "$output/companion-args.tmp"
    mv "$output/companion-args.tmp" "$output/companion-args"
    printf '%s\n' '{{"type":"result","is_error":false,"result":"ok"}}'
    exit
fi
case "$1" in
    --session-id=*|--resume=*) printf '%s\n' "$1" > "$output/host-args" ;;
    *) exit 1 ;;
esac
trap 'printf "fixture TUI\n"' WINCH
while :; do
    IFS= read -r line || continue
    if [ "$line" = "nested" ]; then
        /bin/sh -c '
            printf "%s" "$2" | CLAUDE_PID=$$ "$1" hook session-start > /dev/null
        ' nested '{senate}' '{{"session_id":"fixture-nested","source":"startup"}}'
    fi
done
"#
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            h.paths.config(),
            format!(
                "[companion]\nenabled = {companion}\nclaude_bin = {:?}\n",
                program.to_str().unwrap()
            ),
        )
        .unwrap();
        h.server.state.inner.lock().unwrap().triaged = false;
        start(&mut h).await;
        let seat = call(
            &h,
            &Request::Spawn {
                session: Default::default(),
                runtime: HostedRuntime::Claude,
                committee: committee(),
                alias: Some(path("seat-fixture")),
                cwd: project.clone(),
                chair: false,
            },
        )
        .await;
        let terminal = call(&h, &Self::attach_request()).await;
        Self {
            h,
            seat,
            project,
            terminal,
        }
    }

    fn attach_request() -> Request {
        Request::Attach {
            senator: path("seat-fixture"),
        }
    }

    fn pid(&self) -> u32 {
        std::fs::read_to_string(self.terminal.parent().unwrap().join("owner"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn hook(
        &self,
        id: &str,
        event: senate_bridge::ipc::HookEvent,
        source: Option<&str>,
        pid: Option<u32>,
        ancestors: &[u32],
    ) -> Request {
        use senate_bridge::ipc::HookReport;
        use serde_json::json;

        Request::SessionHook(
            serde_json::from_value::<HookReport>(json!({
                "event":event, "runtime":"claude", "session_id":id,
                "cwd": self.project, "source":source, "claude_pid":pid,
                "ancestor_pids":ancestors,
                "transcript_path":self.project.join(format!("{id}.jsonl")),
            }))
            .unwrap(),
        )
    }

    async fn reject(&self, request: &Request, reason: &str) {
        let error = ipc::request::<serde_json::Value>(&self.h.socket(), request, TIMEOUT, TIMEOUT)
            .await
            .unwrap_err();
        assert!(error.to_string().contains(reason), "{error}");
    }

    fn signal_terminal(&self) -> std::io::Result<()> {
        let owner = std::fs::read_to_string(self.terminal.with_file_name("owner"))?;
        let group = owner
            .trim()
            .parse()
            .ok()
            .and_then(rustix::process::Pid::from_raw)
            .ok_or_else(|| std::io::Error::other("invalid fixture terminal PID"))?;
        rustix::process::kill_process_group(group, rustix::process::Signal::TERM)?;
        Ok(())
    }

    async fn end_terminal(&self) {
        self.signal_terminal().unwrap();
        eventually("host to exit", TIMEOUT, || {
            (!self.terminal.exists()).then_some(())
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_hook_changes_native_context_without_moving_the_seat_or_its_mail() {
    use senate_bridge::ipc::{HookEvent, HookReply, PresidesReply, SessionSelector, StatusReply};
    use senate_bridge::state::ItemStatus;
    use senate_bridge::{SessionState, session::Runtime};
    use senate_core::{Address, Kind, NewEvent};

    let mut fixture = HostedClaude::launch(true).await;
    let pid = fixture.pid();
    let h = &fixture.h;
    let spawned = &fixture.seat;
    let primary = &spawned.senator;

    // The nested process runs the actual hook CLI under the fixture's TUI.
    attach(h, "seat-fixture", b"nested\n\x1d").await;
    let nested = eventually("nested Claude hook", TIMEOUT, || {
        h.store().session("fixture-nested").unwrap()
    })
    .await;
    assert_eq!(nested.agent, path("claude-fixture-nested"));
    assert!(
        nested
            .claude_pid
            .is_some_and(|nested_pid| nested_pid != pid)
    );
    assert_eq!(
        call::<PathBuf>(h, &HostedClaude::attach_request()).await,
        fixture.terminal
    );

    let current_hook = fixture.hook(
        &spawned.session_id,
        HookEvent::UserPromptSubmit,
        None,
        Some(pid),
        &[pid],
    );
    let ambiguous = fixture.hook(
        "unattributed",
        HookEvent::UserPromptSubmit,
        None,
        None,
        &[pid],
    );
    fixture.reject(&ambiguous, "could not attribute").await;
    assert!(h.store().session("unattributed").unwrap().is_none());
    let status: StatusReply = call(h, &Request::Status).await;
    assert_eq!(
        status.hosted[&spawned.session_id].as_deref(),
        Some("unattributed")
    );
    let _: HookReply = call(h, &current_hook).await;
    let status: StatusReply = call(h, &Request::Status).await;
    assert_eq!(status.hosted[&spawned.session_id], None);

    let committee = committee();
    h.server.append(
        NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::Committee,
            "chair set",
        )
        .with_meta(
            senate_core::meta::CHAIR,
            senate_core::chair::ChairPayload {
                senator: primary.clone(),
            }
            .value(),
        ),
    );
    let question = h.server.append(
        NewEvent::new(
            committee.clone(),
            Kind::Question,
            Address::senior(&h.config.person),
            "queued before the native context changed",
        )
        .to(Address::senator(primary)),
    );
    eventually("question held for the original hook", TIMEOUT, || {
        h.store()
            .item(primary, &committee, question.seq)
            .unwrap()
            .filter(|item| item.status == ItemStatus::Queued)
    })
    .await;

    // The daemon missed SessionStart. The first user prompt must still
    // recover the seat and see the question already held for its old ID.
    let cleared = fixture.hook(
        "cleared",
        HookEvent::UserPromptSubmit,
        None,
        Some(pid),
        &[pid],
    );
    let clear: HookReply = call(h, &cleared).await;
    assert_eq!(clear.session.agent, *primary);
    assert_eq!(
        h.store()
            .session(&spawned.session_id)
            .unwrap()
            .unwrap()
            .state,
        SessionState::Ended
    );
    assert_eq!(
        call::<PathBuf>(h, &HostedClaude::attach_request()).await,
        fixture.terminal
    );
    assert!(
        clear
            .context
            .as_deref()
            .is_some_and(|text| text.contains(&question.body))
    );

    let notice = eventually("context change notice", TIMEOUT, || {
        h.server
            .events(committee.as_str())
            .into_iter()
            .find(|event| {
                event.body.contains("changed session context")
                    && event.body.contains(&spawned.session_id)
                    && event.body.contains("cleared")
            })
    })
    .await;
    assert_eq!(notice.to, vec![Address::Speaker]);
    assert!(notice.body.contains(primary.as_str()));
    assert!(
        notice
            .body
            .contains("committee chair's session context changed")
    );

    let appoint = |id: &str| Request::SetSecretarySession {
        target_session: Some(id.into()),
        senator: None,
        principal: None,
        appointed: true,
    };
    let team: senate_core::api::Secretaries = call(h, &appoint("cleared")).await;
    assert_eq!(team[&path(h.config.person.as_str())], vec![primary.clone()]);
    fixture
        .reject(&appoint(&spawned.session_id), "name the full ID")
        .await;
    let from_old_terminal = fixture.hook(
        &spawned.session_id,
        HookEvent::SessionStart,
        Some("startup"),
        None,
        &[],
    );
    fixture.reject(&from_old_terminal, "was retired").await;
    fixture
        .reject(&current_hook, "requires a verified resume")
        .await;
    let status: StatusReply = call(h, &Request::Status).await;
    assert_eq!(
        status.hosted["cleared"].as_deref(),
        Some(spawned.session_id.as_str())
    );

    let back = fixture.hook(
        &spawned.session_id,
        HookEvent::SessionStart,
        Some("resume"),
        Some(pid),
        &[pid],
    );
    let back: HookReply = call(h, &back).await;
    assert_eq!(back.session.agent, *primary);
    let status: StatusReply = call(h, &Request::Status).await;
    assert_eq!(status.hosted[&spawned.session_id], None);
    assert_eq!(
        h.store().session("cleared").unwrap().unwrap().state,
        SessionState::Ended
    );
    // Stale inbox state does not change who owns the host.
    h.store()
        .set_session_state(&spawned.session_id, SessionState::Stale)
        .unwrap();
    let resumed = fixture.hook(
        "resumed",
        HookEvent::SessionStart,
        Some("resume"),
        Some(pid),
        &[pid],
    );
    let resumed: HookReply = call(h, &resumed).await;
    assert_eq!(resumed.session.agent, *primary);
    assert_eq!(
        h.store()
            .session(&spawned.session_id)
            .unwrap()
            .unwrap()
            .state,
        SessionState::Ended
    );

    fixture.end_terminal().await;
    assert_eq!(
        h.store().session("resumed").unwrap().unwrap().state,
        SessionState::Ended
    );

    let presides = |id: &str| Request::Presides {
        session: SessionSelector {
            session_id: Some(id.into()),
            claude_pid: None,
        },
        runtime: Runtime::Claude,
        agent_env: None,
        agent_file: None,
    };
    assert_eq!(
        call::<PresidesReply>(h, &presides(&spawned.session_id))
            .await
            .agent,
        None
    );
    assert_eq!(
        call::<PresidesReply>(h, &presides("resumed")).await.agent,
        Some(primary.clone())
    );

    // Make a retired row newer than the current one to exercise the
    // companion's real choice of --resume argument.
    h.store().touch_session("cleared").unwrap();
    h.server.append(
        NewEvent::new(
            committee,
            Kind::Question,
            Address::senior(&h.config.person),
            "which conversation is the latest one for this seat?",
        )
        .to(Address::senator(primary)),
    );
    let args = eventually("companion command", TIMEOUT, || {
        std::fs::read_to_string(h.dir.path().join("companion-args")).ok()
    })
    .await;
    assert_eq!(
        args.lines().take(4).collect::<Vec<_>>(),
        ["-p", "--resume", "resumed", "--fork-session"]
    );

    let restored: SpawnReply = call(
        h,
        &Request::Restore {
            session: Default::default(),
            senator: path("seat-fixture"),
        },
    )
    .await;
    assert_eq!(restored.senator, *primary);
    assert_eq!(restored.session_id, "resumed");
    assert_eq!(
        std::fs::read_to_string(h.dir.path().join("host-args"))
            .unwrap()
            .trim(),
        "--resume=resumed"
    );
    fixture.terminal = call(&fixture.h, &HostedClaude::attach_request()).await;
    fixture.end_terminal().await;
    fixture.h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_refuses_known_native_ids_without_refreshing_the_other_session() {
    use senate_bridge::SessionState;
    use senate_bridge::ipc::{HookEvent, HookReply, StatusReply};

    let mut fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let pid = fixture.pid();
    for (target, state) in [
        ("ended-ordinary", SessionState::Ended),
        ("stale-ordinary", SessionState::Stale),
    ] {
        let independent = std::process::id();
        let existing = fixture.hook(
            target,
            HookEvent::SessionStart,
            Some("startup"),
            Some(independent),
            &[independent, pid],
        );
        let existing: HookReply = call(h, &existing).await;
        assert_ne!(existing.session.agent, fixture.seat.senator);
        h.store().set_session_state(target, state).unwrap();
        let before = h.store().session(target).unwrap().unwrap();

        for (event, source) in [
            (HookEvent::SessionStart, Some("resume")),
            (HookEvent::UserPromptSubmit, None),
        ] {
            let foreign = fixture.hook(target, event, source, Some(pid), &[pid]);
            fixture
                .reject(&foreign, "target native ID belongs to another senator")
                .await;
            assert_eq!(h.store().session(target).unwrap().unwrap(), before);
            let status: StatusReply = call(h, &Request::Status).await;
            assert_eq!(
                status.hosted[&fixture.seat.session_id].as_deref(),
                Some(target)
            );
        }
        assert_eq!(
            call::<PathBuf>(h, &HostedClaude::attach_request()).await,
            fixture.terminal
        );
    }
    fixture.end_terminal().await;
    fixture.h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_notice_expires_and_a_non_chair_switch_stays_in_the_committee() {
    use senate_bridge::ipc::{HookEvent, HookReply};
    use senate_core::{Address, Kind, NewEvent};

    let mut fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let pid = fixture.pid();
    let committee = committee();
    let chair = h.server.append(
        NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::Committee,
            "chair set",
        )
        .with_meta(
            senate_core::meta::CHAIR,
            senate_core::chair::ChairPayload {
                senator: path("another-chair"),
            }
            .value(),
        ),
    );
    eventually("the hosted seat to observe the new chair", TIMEOUT, || {
        (h.store()
            .delivered_cursor(&fixture.seat.senator, &committee)
            .unwrap()
            >= chair.seq)
            .then_some(())
    })
    .await;
    // Model a persisted, permanently refused head of the durable outbox.
    let mut blocked = NewEvent::new(
        committee.clone(),
        Kind::Note,
        Address::senior(&h.config.person),
        "refused fixture note",
    );
    blocked.id = "00000000000000000000000000".parse().unwrap();
    h.server
        .state
        .inner
        .lock()
        .unwrap()
        .reject_append_containing = Some(blocked.body.clone());
    let conn = rusqlite::Connection::open(h.paths.state_db()).unwrap();
    conn.execute(
        "INSERT INTO hosted_switch_notices (id, event) VALUES (?1, ?2)",
        [
            blocked.id.to_string(),
            serde_json::to_string(&blocked).unwrap(),
        ],
    )
    .unwrap();

    let next = fixture.hook(
        "announced",
        HookEvent::SessionStart,
        Some("clear"),
        Some(pid),
        &[pid],
    );
    let _: HookReply = call(h, &next).await;
    let published = eventually("later context notice to reach the server", TIMEOUT, || {
        h.server
            .events(committee.as_str())
            .into_iter()
            .find(|event| {
                event.body.contains("changed session context") && event.body.contains("announced")
            })
    })
    .await;
    assert!(published.to.is_empty());
    assert!(
        !published
            .body
            .contains("committee chair's session context changed")
    );
    eventually("refused notice to exhaust its attempts", TIMEOUT, || {
        let count: u32 = conn
            .query_row(
                "SELECT count(*) FROM hosted_switch_notices WHERE id = ?1",
                [blocked.id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        (count == 0).then_some(())
    })
    .await;

    fixture.end_terminal().await;
    fixture.h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_context_notice_survives_server_errors_and_timeouts() {
    use senate_bridge::ipc::{HookEvent, HookReply};
    use senate_core::{Address, Kind, NewEvent};

    let mut fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let committee = committee();
    let primary = &fixture.seat.senator;
    let chair = h.server.append(
        NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::Committee,
            "chair set",
        )
        .with_meta(
            senate_core::meta::CHAIR,
            senate_core::chair::ChairPayload {
                senator: primary.clone(),
            }
            .value(),
        ),
    );
    eventually(
        "the hosted seat to observe its chair appointment",
        TIMEOUT,
        || (h.store().delivered_cursor(primary, &committee).unwrap() >= chair.seq).then_some(()),
    )
    .await;
    let pid = fixture.pid();
    let conn = rusqlite::Connection::open(h.paths.state_db()).unwrap();
    let stored = || {
        conn.query_row(
            "SELECT id, failures FROM hosted_switch_notices",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)),
        )
        .unwrap()
    };
    for delay in [Duration::ZERO, Duration::from_secs(2)] {
        let baseline = {
            let mut server = h.server.state.inner.lock().unwrap();
            server.transient_append_delay = Some(delay);
            server.transient_append_attempts
        };
        if delay.is_zero() {
            let switch = fixture.hook(
                "outage-context",
                HookEvent::SessionStart,
                Some("clear"),
                Some(pid),
                &[pid],
            );
            let _: HookReply = call(h, &switch).await;
        }
        // Once a fourth attempt starts, three have failed or timed out.
        eventually(
            "four append attempts during the outage",
            common::WAIT,
            || {
                (h.server
                    .state
                    .inner
                    .lock()
                    .unwrap()
                    .transient_append_attempts
                    >= baseline + 4)
                    .then_some(())
            },
        )
        .await;
        assert_eq!(stored().1, 0);
    }
    let (id, _) = stored();
    h.server.state.inner.lock().unwrap().transient_append_delay = None;
    let notice = eventually(
        "the original notice after server recovery",
        common::WAIT,
        || {
            h.server
                .events(committee.as_str())
                .into_iter()
                .find(|event| event.id.to_string() == id)
        },
    )
    .await;
    assert_eq!(notice.to, vec![Address::Speaker]);
    assert!(notice.body.contains("outage-context"));
    fixture.end_terminal().await;
    fixture.h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pruning_keeps_an_expired_stale_seat_until_its_host_exits() {
    use senate_bridge::SessionState;
    use senate_bridge::ipc::{HookEvent, HookReply};

    let mut fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let pid = fixture.pid();
    let native = "aged-context";
    let hook = fixture.hook(
        native,
        HookEvent::SessionStart,
        Some("clear"),
        Some(pid),
        &[pid],
    );
    let changed: HookReply = call(h, &hook).await;
    assert_eq!(changed.session.agent, fixture.seat.senator);
    h.store()
        .set_session_state(native, SessionState::Stale)
        .unwrap();
    let conn = rusqlite::Connection::open(h.paths.state_db()).unwrap();
    conn.execute(
        "UPDATE sessions SET last_seen = 0 WHERE session_id = ?1",
        [native],
    )
    .unwrap();

    let retention = Duration::from_secs(7 * 24 * 3600);
    h.store().prune(retention, &h.paths).unwrap();
    assert_eq!(
        h.store()
            .delete_sessions(&[native.into()], &h.paths)
            .unwrap(),
        0
    );
    assert_eq!(
        h.store().session(native).unwrap().unwrap().state,
        SessionState::Stale
    );
    assert_eq!(
        call::<PathBuf>(h, &HostedClaude::attach_request()).await,
        fixture.terminal
    );

    fixture.end_terminal().await;
    h.store().prune(retention, &h.paths).unwrap();
    assert!(h.store().session(native).unwrap().is_none());
    let identity_rows: u32 = conn
        .query_row(
            "SELECT count(*) FROM hosted_native_seats WHERE session_id = ?1",
            [native],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(identity_rows, 0);
    fixture.h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unbound_claude_tui_holds_committee_mail_and_secretary_reminders() {
    use senate_bridge::ipc::{HookEvent, HookReply, StatusReply};
    use senate_bridge::state::ItemStatus;
    use senate_core::secretary_work::SecretaryReminder;
    use senate_core::{Address, EventId, Kind, NewEvent};

    let mut fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let pid = fixture.pid();
    let inbox = common::FakeInbox::bind(h.dir.path().join("fixture-inbox.sock"));
    let native = &fixture.seat.session_id;
    let primary = &fixture.seat.senator;
    let mut hook = fixture.hook(native, HookEvent::UserPromptSubmit, None, Some(pid), &[pid]);
    let Request::SessionHook(report) = &mut hook else {
        unreachable!()
    };
    report.socket = Some(inbox.path.clone());
    let _: HookReply = call(h, &hook).await;
    let independent = std::process::id();
    let occupied = fixture.hook(
        "unbound-context",
        HookEvent::SessionStart,
        Some("startup"),
        Some(independent),
        &[independent, pid],
    );
    let _: HookReply = call(h, &occupied).await;
    let refused = fixture.hook(
        "unbound-context",
        HookEvent::SessionStart,
        Some("resume"),
        Some(pid),
        &[pid],
    );
    fixture
        .reject(&refused, "target native ID belongs to another senator")
        .await;
    let uncertain = fixture.hook(
        "uncertain-context",
        HookEvent::UserPromptSubmit,
        None,
        None,
        &[pid],
    );
    fixture.reject(&uncertain, "could not attribute").await;
    let status: StatusReply = call(h, &Request::Status).await;
    assert_eq!(status.hosted[native].as_deref(), Some("uncertain-context"));
    assert_eq!(status.hosted_unbound[native], "unbound-context");

    let committee = committee();
    let note = h.server.append(
        NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::senior(&h.config.person),
            "held committee note",
        )
        .to(Address::senator(primary)),
    );
    let reminder = SecretaryReminder {
        principal: path("alice"),
        item: EventId::new(),
        id: EventId::new(),
        to: primary.clone(),
    };
    let baseline = {
        let mut server = h.server.state.inner.lock().unwrap();
        server.secretary_reminders.push(reminder.clone());
        server.secretary_reminder_polls
    };
    eventually("the committee message to wait for a hook", TIMEOUT, || {
        h.store()
            .item(primary, &committee, note.seq)
            .unwrap()
            .filter(|row| row.status == ItemStatus::Queued)
    })
    .await;
    // Once the next poll starts, the delivery attempt for the previous poll has completed.
    eventually(
        "the secretary reminder worker to check the held session",
        common::WAIT,
        || {
            (h.server
                .state
                .inner
                .lock()
                .unwrap()
                .secretary_reminder_polls
                >= baseline + 2)
                .then_some(())
        },
    )
    .await;
    assert!(inbox.connections().is_empty());
    assert_eq!(
        h.server.state.inner.lock().unwrap().secretary_reminders,
        vec![reminder.clone()]
    );

    let recovered: HookReply = call(h, &hook).await;
    let status: StatusReply = call(h, &Request::Status).await;
    assert!(status.hosted_unbound.is_empty());
    let context = recovered.context.unwrap();
    assert!(context.contains(&note.body));
    eventually(
        "the verified session to receive its pending reminder",
        TIMEOUT,
        || {
            inbox
                .messages()
                .into_iter()
                .find(|text| text.contains(&reminder.item.to_string()))
        },
    )
    .await;
    fixture.end_terminal().await;
    fixture.h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostic_only_reports_do_not_stop_pushes_to_an_idle_host() {
    use senate_bridge::ipc::{HookEvent, HookReply, StatusReply};
    use senate_core::{Address, Kind, NewEvent};

    let mut fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let pid = fixture.pid();
    let inbox = common::FakeInbox::bind(h.dir.path().join("current-inbox.sock"));
    let prior = &fixture.seat.session_id;
    let current = "current-context";
    let stale = fixture.hook(prior, HookEvent::UserPromptSubmit, None, Some(pid), &[pid]);
    let mut cleared = fixture.hook(
        current,
        HookEvent::SessionStart,
        Some("clear"),
        Some(pid),
        &[pid],
    );
    let Request::SessionHook(report) = &mut cleared else {
        unreachable!()
    };
    report.socket = Some(inbox.path.clone());
    let _: HookReply = call(h, &cleared).await;

    let ambiguous = fixture.hook(
        "uncertain-resume",
        HookEvent::SessionStart,
        Some("resume"),
        None,
        &[pid],
    );
    for (hook, native, reason) in [
        (stale, prior.as_str(), "requires a verified resume"),
        (ambiguous, "uncertain-resume", "could not attribute"),
    ] {
        fixture.reject(&hook, reason).await;
        let status: StatusReply = call(h, &Request::Status).await;
        assert_eq!(status.hosted[current].as_deref(), Some(native));
        assert!(status.hosted_unbound.is_empty());
        let body = format!("idle inbox delivery after refused hook for {native}");
        h.server.append(
            NewEvent::new(
                committee(),
                Kind::Note,
                Address::senior(&h.config.person),
                &body,
            )
            .to(Address::senator(&fixture.seat.senator)),
        );
        eventually(
            "the idle inbox to receive a note without another hook",
            TIMEOUT,
            || {
                inbox
                    .messages()
                    .into_iter()
                    .find(|text| text.contains(&body))
            },
        )
        .await;
    }
    let view = senate_bridge::status::collect(&h.paths).await.unwrap();
    let status = senate_bridge::status::render(&view, chrono::Utc::now());
    assert!(status.contains("refused a hook reporting native session uncertain-resume"));
    assert!(!status.contains("Messages wait for the original session"));
    fixture.end_terminal().await;
    fixture.h.stop().await;
}
