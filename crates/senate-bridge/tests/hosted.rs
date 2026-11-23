//! Exercise a real Senate terminal host and attach client with a tiny TUI.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use common::{
    Harness, RECORD_HOSTED_ARGV, agy_fixture, committee, eventually, fast_timing,
    hosted_fixture_argv, path,
};
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::ipc::{self, HostedModel, HostedRuntime, Request, SpawnReply};
use senate_bridge::testenv;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const BIN: &str = env!("CARGO_BIN_EXE_senate");
const TIMEOUT: Duration = Duration::from_secs(5);

/// Kills every host under a home, and its TUI, when dropped: for a test
/// that starts `session-host` itself rather than through a [`Harness`].
struct KillHosts(senate_bridge::Paths);

impl Drop for KillHosts {
    fn drop(&mut self) {
        senate_bridge::host::kill_all(&self.0);
    }
}

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

async fn wait_for_session_end(h: &Harness, session_id: &str) {
    eventually(
        &format!("registry session {session_id} ended"),
        TIMEOUT,
        || {
            let session = h.store().session(session_id).unwrap()?;
            (session.state == senate_bridge::SessionState::Ended).then_some(())
        },
    )
    .await;
}

async fn begin_viewer(viewer: &mut tokio::net::UnixStream) {
    assert_eq!(viewer.read_u8().await.unwrap(), 3);
    viewer.write_all(b"\x01\0\x18\0\x50").await.unwrap();
}

async fn attach(h: &Harness, alias: &str, keys: &[u8]) {
    attach_typing(h, alias, true, keys).await;
}

/// [`attach`], asking the fixture to paint first only if `paint`: a line
/// left in its prompt would swallow the request.
async fn attach_typing(h: &Harness, alias: &str, paint: bool, keys: &[u8]) {
    pty_attach_command(h, &["attach", alias], paint, keys).await;
}

async fn pty_attach_command(h: &Harness, args: &[&str], paint: bool, keys: &[u8]) {
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(args)
        .env("SENATE_HOME", h.paths.home())
        .kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    tokio::time::timeout(TIMEOUT, async {
        if args == ["attach"] {
            wait_terminal_output(&mut output, b"paused").await;
            input.write_all(b"\r").await.unwrap();
        }
        // The attach CLI sets raw mode and
        // asks this terminal for its colours, the device attributes last.
        // Answer as a light Ghostty does, then type straight on and
        // request fresh fixture output over the actual PTY.
        wait_terminal_output(&mut output, b"\x1b[c").await;
        input
            .write_all(
                b"\x1b]10;rgb:4c4c/4f4f/6969\x1b\\\x1b]11;rgb:efef/f1f1/f5f5\x1b\\\x1b[?997;2n\x1b[?62;22c",
            )
            .await
            .unwrap();
        if paint {
            input.write_all(b"paint\r").await.unwrap();
            wait_terminal_output(&mut output, b"fixture TUI").await;
        } else {
            // Nothing to wait for on the screen: give the attachment time
            // to reach the host.
            tokio::time::sleep(Duration::from_millis(500)).await;
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

async fn wait_terminal_output(output: &mut (impl tokio::io::AsyncRead + Unpin), marker: &[u8]) {
    let mut observed = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let n = output.read(&mut buf).await.unwrap();
        assert_ne!(n, 0, "terminal closed before {marker:?}");
        observed.extend_from_slice(&buf[..n]);
        if observed.windows(marker.len()).any(|bytes| bytes == marker) {
            return;
        }
        let discard = observed.len().saturating_sub(marker.len() - 1);
        observed.drain(..discard);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixture_terminal_launch_detach_restart_and_restore() {
    let mut h = Harness::new().await;
    h.config.hosted_session_limit = Some(1);
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    let project = h.workdir("terminal", None);
    let fixture = h.dir.path().join("fixture-claude");
    std::fs::write(
        &fixture,
        format!(
            r#"#!/bin/sh
case "$1" in --session-id=?*|--resume=?*) ;; *) exit 64 ;; esac
{RECORD_HOSTED_ARGV}
while IFS= read -r line; do
    case "$line" in
        paint) printf 'fixture TUI\n' ;;
        q) exit 0 ;;
    esac
done
"#
        ),
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
        attempt_id: None,
        session: Default::default(),
        runtime: Some(HostedRuntime::Claude),
        profile: None,
        committee: committee(),
        alias,
        cwd: project.clone(),
        chair: false,
        secretary: false,
        prompt: None,
        model: Default::default(),
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
    let attach_target = Request::Attach {
        senator: launched.senator.clone().into(),
    };
    let socket: PathBuf = call(&h, &attach_target).await;
    let args = hosted_fixture_argv(&socket, &project, TIMEOUT).await;
    assert_eq!(args[0], format!("--session-id={}", launched.session_id));
    // The TUI is told the senator's alias as its own session name.
    assert!(args.contains(&"--name=fixture".to_owned()), "{args:?}");
    attach(&h, launched.senator.as_str(), b"\x1b[93;5u").await;
    assert!(socket.exists());

    h.stop().await;
    assert!(socket.exists());
    // Give the conflict attempt a slot beside the surviving host.
    h.config.hosted_session_limit = Some(2);
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
            server.aliases.get(&committee(), &path("fixture")),
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
    let transcript = project.join("saved-fixture.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    let _: senate_bridge::ipc::HookReply = call(&h, &Request::SessionHook(
        serde_json::from_value(serde_json::json!({
            "event": "user_prompt_submit", "runtime": "claude", "session_id": launched.session_id,
            "cwd": project, "transcript_path": transcript,
        })).unwrap(),
    )).await;

    // The runtime can exit while the daemon is unavailable. No SessionEnd
    // will reach the registry, so the restarted daemon must use the lock.
    h.stop().await;
    let mut terminal = tokio::net::UnixStream::connect(&socket).await.unwrap();
    begin_viewer(&mut terminal).await;
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
            senator: launched.senator.clone().into(),
            model: Default::default(),
        },
    )
    .await;
    assert_eq!(restored, launched);
    let socket: PathBuf = call(&h, &attach_target).await;
    let args = hosted_fixture_argv(&socket, &project, TIMEOUT).await;
    assert_eq!(args[0], format!("--resume={}", restored.session_id));
    // Restored by its primary name, it is still named by its alias.
    assert!(args.contains(&"--name=fixture".to_owned()), "{args:?}");
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
        "#!/bin/sh\nprintf '%s %s\\n' \"$TERM\" \"$COLORTERM\" > term\nsleep 300 &\nprintf '%s %s\\n' \"$$\" \"$!\" > group.tmp\nmv group.tmp group\nwait\n",
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
        // A daemon started by launchd has no terminal in its environment.
        // The host is the TUI's terminal and must say what it can show.
        .env("TERM", "dumb")
        .env_remove("COLORTERM")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd.spawn().unwrap();
    // Dropped before `child`: the host's TUI goes with it, not only the host.
    let _hosts = KillHosts(paths.clone());
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
    assert_eq!(
        std::fs::read_to_string(root.path().join("term")).unwrap(),
        "xterm-256color truecolor\n"
    );

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

/// A TUI asks its terminal for colours as it starts, when nobody can have
/// attached yet, and turns on reports a later viewer's terminal never saw.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosted_tui_learns_the_colours_of_the_persons_terminal() {
    use rustix::process::{Pid, Signal, kill_process};
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt as _;

    let root = tempfile::tempdir_in("/tmp").unwrap();
    let paths = senate_bridge::Paths::new(root.path());
    let id = senate_core::EventId::new();
    let directory = root.path().join("hosts").join(id.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    let program = root.path().join("fixture");
    // Ask for the background and then for device attributes, as Claude
    // Code does at startup: it keeps the colour only once the second
    // answer is in. Then turn on paste and theme reports and wait for a
    // theme report.
    std::fs::write(
        &program,
        r#"#!/bin/sh
stty raw -echo
printf '\033]11;?\033\\'
dd bs=1 count=25 of=background.tmp 2>/dev/null && mv background.tmp background
printf '\033[c'
dd bs=1 count=9 of=attributes.tmp 2>/dev/null && mv attributes.tmp attributes
printf '\033[?2004h\033[?2031h'
dd bs=1 count=9 of=scheme.tmp 2>/dev/null && mv scheme.tmp scheme
sleep 300
"#,
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
    // Dropped before `child`: the host's TUI goes with it, not only the host.
    let _hosts = KillHosts(paths.clone());
    let ready = tokio::time::timeout(TIMEOUT, child.stdout.as_mut().unwrap().read_u8())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready, 1);
    // Publish, as the daemon does, so the host starts taking viewers.
    let mut publication = child.stdin.take().unwrap();
    publication.write_all(&[1]).await.unwrap();

    // Nobody was attached: the host answered with a system palette.
    let background = eventually("the startup colour answer", TIMEOUT, || {
        std::fs::read(root.path().join("background")).ok()
    })
    .await;
    assert!(
        [
            b"\x1b]11;rgb:efef/f1f1/f5f5\x1b\\",
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\",
        ]
        .iter()
        .any(|color| background == *color),
        "{background:?}"
    );
    let attributes = eventually("the device attributes answer", TIMEOUT, || {
        std::fs::read(root.path().join("attributes")).ok()
    })
    .await;
    assert_eq!(attributes, b"\x1b[?62;22c");

    // A viewer attaching later is told which reports the TUI turned on.
    let socket = directory.join("tty");
    let expected = b"\x1b[?2004;2031h";
    let start = std::time::Instant::now();
    let mut viewer = loop {
        let mut viewer = tokio::net::UnixStream::connect(&socket).await.unwrap();
        begin_viewer(&mut viewer).await;
        let mut preamble = [0; 13];
        let read =
            tokio::time::timeout(Duration::from_millis(200), viewer.read_exact(&mut preamble))
                .await;
        if matches!(read, Ok(Ok(_))) && &preamble == expected {
            break viewer;
        }
        // The TUI had not yet turned them on when this viewer attached.
        assert!(start.elapsed() < TIMEOUT, "no mode replay: {preamble:?}");
        drop(viewer);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(!root.path().join("scheme").exists());

    // This viewer's terminal is dark: the TUI, which asked for theme
    // reports, is told so in the terminal's own words.
    let colors = br#"{"background":"rgb:1e1e/1e1e/2e2e","scheme":"dark"}"#;
    viewer.write_all(&[3, 0, colors.len() as u8]).await.unwrap();
    viewer.write_all(colors).await.unwrap();
    let scheme = eventually("the theme report", TIMEOUT, || {
        std::fs::read(root.path().join("scheme")).ok()
    })
    .await;
    assert_eq!(scheme, b"\x1b[?997;1n");

    let host = Pid::from_raw(child.id().unwrap().cast_signed()).unwrap();
    kill_process(host, Signal::TERM).unwrap();
    let _ = tokio::time::timeout(TIMEOUT, child.wait()).await.unwrap();
    assert!(!directory.exists());
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
        senator: senator.clone().into(),
        model: Default::default(),
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
    let binding: PathBuf = call(
        &h,
        &Request::Attach {
            senator: senator.into(),
        },
    )
    .await;
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

    #[derive(Debug, PartialEq, Eq)]
    enum NativeEnd {
        BeforePublication,
        AfterPublication,
        AlreadyEnded,
    }

    for native_end in [
        NativeEnd::BeforePublication,
        NativeEnd::AfterPublication,
        NativeEnd::AlreadyEnded,
    ] {
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
            if native_end != NativeEnd::AfterPublication {
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
            senator: senator.clone().into(),
            model: Default::default(),
        };
        let restored: SpawnReply = if native_end == NativeEnd::BeforePublication {
            h.store().set_session_state(id, SessionState::Live).unwrap();
            // Restore posts its ready audit before binding the host. Hold the
            // audit reply until the native end has been applied to the old row.
            let (ready, release) = h.server.hold_next_append(&format!(
                "restored {} (codex)",
                Address::senator(&senator).in_text(),
            ));
            let (result, ()) = tokio::join!(call(&h, &request), async {
                tokio::time::timeout(TIMEOUT, ready).await.unwrap().unwrap();
                let _: serde_json::Value = call(&h, &session_end).await;
                assert_eq!(
                    h.store().session(id).unwrap().unwrap().state,
                    SessionState::Ended,
                    "the native session ended while publication was held",
                );
                release.send(()).unwrap();
            });
            result
        } else {
            call(&h, &request).await
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
        if native_end == NativeEnd::AlreadyEnded {
            assert_eq!(opening.after, prehistory.seq.0);
        }
        let socket: PathBuf = call(
            &h,
            &Request::Attach {
                senator: senator.into(),
            },
        )
        .await;
        let shared = socket.with_file_name("shared-codex");
        if native_end == NativeEnd::AfterPublication {
            assert!(shared.is_file(), "publication should see the native client");
            let _: serde_json::Value = call(&h, &session_end).await;
            assert_eq!(
                h.store().session(id).unwrap().unwrap().state,
                SessionState::Live,
                "the hosted session is still live after the native client exits",
            );
        }
        assert!(!shared.exists(), "shared marker survived {native_end:?}");
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
        wait_for_session_end(&h, id).await;
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
        Self::launch_with_limit(companion, 4).await
    }

    async fn launch_with_limit(companion: bool, limit: usize) -> Self {
        Self::launch_with(companion, limit, HostedModel::default()).await
    }

    async fn launch_with(companion: bool, limit: usize, model: HostedModel) -> Self {
        let mut h = Harness::new().await;
        h.config.hosted_session_limit = Some(limit);
        h.dir = tempfile::tempdir_in("/tmp").unwrap();
        h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
        h.paths.ensure_home().unwrap();
        let project = h.workdir("switch", None);
        let program = h.dir.path().join("fixture");
        std::fs::write(
            h.dir.path().join("nested-input.json"),
            r#"{"session_id":"fixture-nested","source":"startup"}"#,
        )
        .unwrap();
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
    --resume=*)
        if [ -f "$output/empty-restore-fixture" ]; then
            printf 'this fixture has no saved history\n' >&2
            exit 1
        fi
        ;;
    --session-id=*) ;;
    *) exit 1 ;;
esac
{RECORD_HOSTED_ARGV}
while :; do
    IFS= read -r line || continue
    [ "$line" != "paint" ] || printf 'fixture TUI\n'
    if [ "$line" = "nested" ]; then
        /bin/sh -c '
            CLAUDE_PID=$$ "$1" hook session-start < "$2/nested-input.json" > "$2/nested-hook.log" 2>&1
            printf "%s\n" "$?" > "$2/nested-hook-status.tmp"
            mv "$2/nested-hook-status.tmp" "$2/nested-hook-status"
        ' nested '{senate}' "$output"
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
                attempt_id: None,
                session: Default::default(),
                runtime: Some(HostedRuntime::Claude),
                profile: None,
                committee: committee(),
                alias: Some(path("seat-fixture")),
                cwd: project.clone(),
                chair: false,
                secretary: false,
                prompt: None,
                model,
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
            senator: path("seat-fixture").into(),
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
async fn a_chosen_model_and_effort_reach_the_tui_and_are_kept_for_restore() {
    use senate_bridge::ipc::StopReply;

    let choice = |model: Option<&str>, effort: Option<&str>| {
        HostedModel::new(model.map(str::to_owned), effort.map(str::to_owned))
    };
    let mut fixture = HostedClaude::launch_with(false, 1, choice(Some("opus"), Some("high"))).await;
    let args = hosted_fixture_argv(&fixture.terminal, &fixture.project, TIMEOUT).await;
    assert!(args.contains(&"--model=opus".to_owned()), "{args:?}");
    assert!(args.contains(&"--effort=high".to_owned()), "{args:?}");
    let listed = |h: &Harness| {
        let h = h.socket();
        async move {
            let reply: senate_bridge::ipc::SessionsReply =
                ipc::request(&h, &Request::Sessions, TIMEOUT, TIMEOUT)
                    .await
                    .unwrap();
            let [row] = reply.rows.as_slice() else {
                panic!("one hosted session: {reply:?}");
            };
            row.model.label()
        }
    };
    assert_eq!(listed(&fixture.h).await, "opus/high");

    // An effort Claude Code does not take is refused before anything
    // starts, even with no slot left, naming the ones it does take.
    let refused = ipc::request::<SpawnReply>(
        &fixture.h.socket(),
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: committee(),
            alias: None,
            cwd: fixture.project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: choice(None, Some("minimal")),
        },
        TIMEOUT,
        TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(
        refused.to_string().contains(
            "claude does not take the effort \"minimal\"; use one of: low, medium, high, xhigh, max"
        ),
        "{refused}"
    );
    assert_eq!(fixture.h.store().all_sessions().unwrap().len(), 1);

    let mut restore = async |model: HostedModel| {
        let _: StopReply = call(
            &fixture.h,
            &Request::Stop {
                session: Default::default(),
                senator: fixture.seat.senator.clone().into(),
                force: true,
            },
        )
        .await;
        let restored: SpawnReply = call(
            &fixture.h,
            &Request::Restore {
                session: Default::default(),
                senator: path("seat-fixture").into(),
                model,
            },
        )
        .await;
        assert_eq!(restored, fixture.seat);
        fixture.terminal = call(&fixture.h, &HostedClaude::attach_request()).await;
        hosted_fixture_argv(&fixture.terminal, &fixture.project, TIMEOUT).await
    };
    // Restored as it was: the record brings back the same model and effort.
    let args = restore(HostedModel::default()).await;
    assert!(args.contains(&"--model=opus".to_owned()), "{args:?}");
    assert!(args.contains(&"--effort=high".to_owned()), "{args:?}");
    // What a restore changes is used, and becomes the record.
    let args = restore(choice(None, Some("low"))).await;
    assert!(args.contains(&"--model=opus".to_owned()), "{args:?}");
    assert!(args.contains(&"--effort=low".to_owned()), "{args:?}");
    let args = restore(HostedModel::default()).await;
    assert!(args.contains(&"--effort=low".to_owned()), "{args:?}");
    assert!(!args.contains(&"--effort=high".to_owned()), "{args:?}");
    assert_eq!(listed(&fixture.h).await, "opus/low");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_senator_group_names_a_session_and_sets_its_duty() {
    let fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let id = fixture.seat.session_id.clone();

    let (ok, out) = senate(h, &["senator", "alias", "renamed", "--session", &id]).await;
    assert!(ok, "{out}");
    assert!(
        out.contains("renamed ->") && out.contains("(refund-v2)"),
        "{out}"
    );
    let (ok, out) = senate(h, &["senator", "list"]).await;
    assert!(ok && out.contains("refund-v2  renamed"), "{out}");

    let (ok, out) = senate(
        h,
        &[
            "senator",
            "duty",
            "reviews the server crate",
            "--session",
            &id,
        ],
    )
    .await;
    assert!(ok && out.contains("Set the duty on refund-v2"), "{out}");
    let (ok, out) = senate(h, &["senator", "duty", "", "--session", &id]).await;
    assert!(ok && out.contains("Cleared the duty on refund-v2"), "{out}");

    // The old form still names the session, and says what it is now.
    let (ok, out) = senate(h, &["join", "--alias", "again", "--session", &id]).await;
    assert!(ok, "{out}");
    assert!(out.contains("is now `senate senator alias`"), "{out}");
    assert!(out.contains("again ->"), "{out}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_cli_protects_an_attending_viewer_and_releases_the_slot() {
    use senate_bridge::SessionState;
    use senate_bridge::ipc::StopReply;
    use senate_core::Kind;

    let mut fixture = HostedClaude::launch_with_limit(false, 1).await;
    let h = &fixture.h;
    let spare = Request::Spawn {
        attempt_id: None,
        session: Default::default(),
        runtime: Some(HostedRuntime::Claude),
        profile: None,
        committee: committee(),
        alias: None,
        cwd: fixture.project.clone(),
        chair: false,
        secretary: false,
        prompt: None,
        model: Default::default(),
    };
    fixture.reject(&spare, "hosted session limit (1)").await;
    let mut viewer = tokio::net::UnixStream::connect(&fixture.terminal)
        .await
        .unwrap();
    begin_viewer(&mut viewer).await;
    viewer.write_all(b"\x02\x00\x01x").await.unwrap();
    eventually("the host to observe viewer input", TIMEOUT, || {
        let fact: serde_json::Value = serde_json::from_slice(
            &std::fs::read(fixture.terminal.with_file_name("viewer.json")).ok()?,
        )
        .ok()?;
        (fact["input_at"].as_u64()? > 0).then_some(())
    })
    .await;
    let cli = |force| {
        let mut cmd = tokio::process::Command::new(BIN);
        testenv::isolate_async(&mut cmd, h.dir.path());
        cmd.env("SENATE_HOME", h.paths.home())
            .args(["session", "stop", "seat-fixture"]);
        if force {
            cmd.arg("--force");
        }
        cmd
    };
    let refused = cli(false).output().await.unwrap();
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("someone typed into it"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(fixture.terminal.exists());
    let mut host_errors = std::fs::File::open(fixture.terminal.with_file_name("errors")).unwrap();
    let stopped = cli(true).output().await.unwrap();
    let mut host_details = String::new();
    std::io::Read::read_to_string(&mut host_errors, &mut host_details).unwrap();
    assert!(
        stopped.status.success(),
        "{}\n{host_details}",
        String::from_utf8_lossy(&stopped.stderr),
    );
    assert_eq!(
        String::from_utf8_lossy(&stopped.stdout).trim(),
        "stopped refund-v2::seat-fixture"
    );
    assert!(!fixture.terminal.parent().unwrap().exists());
    assert_eq!(
        h.store()
            .session(&fixture.seat.session_id)
            .unwrap()
            .unwrap()
            .state,
        SessionState::Ended
    );
    // Its seat is away from now on: the bridge tells the server it left,
    // instead of letting the presence threshold run out.
    eventually(
        "the bridge to tell the server the seat left",
        TIMEOUT,
        || {
            h.server
                .heartbeats_of(committee().as_str(), fixture.seat.senator.as_str())
                .into_iter()
                .find(|beat| beat.left && beat.sessions == 0)
        },
    )
    .await;
    let notes = h.server.events(committee().as_str());
    let note = notes
        .iter()
        .find(|event| event.body.contains("stopped the hosted terminal"))
        .unwrap();
    assert_eq!(note.kind, Kind::Note);
    assert_eq!(note.from, common::person("alice"));
    assert!(note.body.contains(fixture.seat.senator.as_str()));
    assert!(note.body.contains("This was a forced stop."));

    std::fs::write(h.dir.path().join("empty-restore-fixture"), "").unwrap();
    let restored: SpawnReply = call(
        h,
        &Request::Restore {
            session: Default::default(),
            senator: path("seat-fixture").into(),
            model: Default::default(),
        },
    )
    .await;
    assert_eq!(restored, fixture.seat);
    fixture.terminal = call(&fixture.h, &HostedClaude::attach_request()).await;
    let args = hosted_fixture_argv(&fixture.terminal, &fixture.project, TIMEOUT).await;
    assert_eq!(args[0], format!("--session-id={}", restored.session_id));
    // An attachment that has sent no human input does not prevent stop.
    let mut passive = tokio::net::UnixStream::connect(&fixture.terminal)
        .await
        .unwrap();
    begin_viewer(&mut passive).await;
    fixture
        .h
        .server
        .state
        .inner
        .lock()
        .unwrap()
        .transient_append_delay = Some(Duration::ZERO);
    let stopped: StopReply = call(
        &fixture.h,
        &Request::Stop {
            session: Default::default(),
            senator: restored.senator.into(),
            force: false,
        },
    )
    .await;
    assert_eq!(stopped.session_id, restored.session_id);
    assert!(stopped.audit_pending);
    assert!(!fixture.terminal.exists());
    fixture
        .h
        .server
        .state
        .inner
        .lock()
        .unwrap()
        .transient_append_delay = None;
    eventually(
        "the queued stop note to reach the committee log",
        TIMEOUT,
        || {
            (fixture
                .h
                .server
                .events(committee().as_str())
                .iter()
                .filter(|event| event.body.contains("stopped the hosted terminal"))
                .count()
                == 2)
                .then_some(())
        },
    )
    .await;
    fixture.h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unconfirmed_forced_stop_audits_the_ended_host() {
    use senate_bridge::ipc::StopReply;

    let mut fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let db = rusqlite::Connection::open(h.paths.state_db()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse_end BEFORE UPDATE OF state ON sessions
         WHEN NEW.state = 'ended'
         BEGIN SELECT RAISE(ABORT, 'injected session-end refusal'); END;",
    )
    .unwrap();
    let refused = ipc::request::<StopReply>(
        &h.socket(),
        &Request::Stop {
            session: Default::default(),
            senator: path("seat-fixture").into(),
            force: true,
        },
        TIMEOUT,
        common::WAIT,
    )
    .await
    .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("did not acknowledge its session end"),
        "{refused}"
    );
    assert!(!fixture.terminal.parent().unwrap().exists());
    let note = eventually("the unconfirmed stop audit", TIMEOUT, || {
        h.server
            .events(committee().as_str())
            .into_iter()
            .find(|event| {
                event
                    .body
                    .contains("The host ended with the stop result unconfirmed")
            })
    })
    .await;
    assert_eq!(note.from, common::person("alice"));
    assert!(note.body.contains("requested a forced stop"));
    assert!(note.body.contains(fixture.seat.senator.as_str()));
    fixture.h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_retains_an_accepted_stop_audit_without_its_daemon() {
    use senate_core::{Kind, NewEvent};

    let mut fixture = HostedClaude::launch(false).await;
    let note = NewEvent::new(
        committee(),
        Kind::Note,
        common::person("alice"),
        "host ended after an unconfirmed stop",
    );
    // Deliver a prepared control request with the daemon offline. The host
    // must keep the audit before ending on its own.
    fixture.h.stop().await;
    let mut control = tokio::net::UnixStream::connect(fixture.terminal.with_file_name("ctl"))
        .await
        .unwrap();
    let mut request =
        serde_json::to_vec(&serde_json::json!({"force":false, "unconfirmed_audit":note})).unwrap();
    request.push(b'\n');
    control.write_all(&request).await.unwrap();
    assert_eq!(control.read_u8().await.unwrap(), 1);
    drop(control);
    eventually("the detached host to exit", TIMEOUT, || {
        (!fixture.terminal.parent().unwrap().exists()).then_some(())
    })
    .await;
    start(&mut fixture.h).await;
    let posted = eventually(
        "the restarted bridge to recover the stop audit",
        TIMEOUT,
        || {
            fixture
                .h
                .server
                .events(committee().as_str())
                .into_iter()
                .find(|event| event.id == note.id)
        },
    )
    .await;
    assert_eq!(posted.body, note.body);
    fixture.h.stop().await;
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
    let mut viewer = tokio::net::UnixStream::connect(&fixture.terminal)
        .await
        .unwrap();
    begin_viewer(&mut viewer).await;
    viewer.write_all(b"\x02\x00\x07nested\n").await.unwrap();
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let status = eventually("the nested Claude CLI to finish", TIMEOUT, || {
        std::fs::read_to_string(h.dir.path().join("nested-hook-status")).ok()
    })
    .await;
    let output = std::fs::read_to_string(h.dir.path().join("nested-hook.log")).unwrap();
    assert_eq!(status.trim(), "0", "nested Claude CLI output: {output:?}");
    // Exit zero is also the CLI's fail-open result. Independently observe
    // the registration within the same deadline.
    let nested = eventually(
        &format!("nested Claude registration (CLI output: {output:?})"),
        deadline.saturating_duration_since(tokio::time::Instant::now()),
        || h.store().session("fixture-nested").unwrap(),
    )
    .await;
    drop(viewer);
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
    wait_for_session_end(h, "resumed").await;

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
    std::fs::write(fixture.project.join("resumed.jsonl"), "{}\n").unwrap();

    let restored: SpawnReply = call(
        h,
        &Request::Restore {
            session: Default::default(),
            senator: path("seat-fixture").into(),
            model: Default::default(),
        },
    )
    .await;
    assert_eq!(restored.senator, *primary);
    assert_eq!(restored.session_id, "resumed");
    fixture.terminal = call(&fixture.h, &HostedClaude::attach_request()).await;
    let args = hosted_fixture_argv(&fixture.terminal, &fixture.project, TIMEOUT).await;
    assert_eq!(args[0], "--resume=resumed");
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

/// Every process whose command line names `root`: a host started there (its
/// cwd and program are under it) or the TUI it runs.
fn processes_under(root: &std::path::Path) -> Vec<String> {
    // `/tmp` is a link to `/private/tmp` on macOS: match either spelling.
    let names: Vec<String> = [root.to_path_buf(), root.canonicalize().unwrap_or_default()]
        .iter()
        .map(|p| p.display().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let out = std::process::Command::new("ps")
        .args(["-axww", "-o", "pid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| names.iter().any(|n| line.contains(n.as_str())))
        .map(str::to_owned)
        .collect()
}

/// A harness in a temporary home with one hosted session running (its
/// host and TUI), and that home.
async fn one_hosted_session() -> (Harness, PathBuf) {
    let mut h = Harness::new().await;
    h.config.hosted_session_limit = Some(1);
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("leak", None);
    let fixture = h.dir.path().join("fixture-claude");
    std::fs::write(
        &fixture,
        format!("#!/bin/sh\n{RECORD_HOSTED_ARGV}\nwhile IFS= read -r line; do :; done\n"),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[companion]\nclaude_bin = {:?}\n",
            fixture.to_str().unwrap()
        ),
    )
    .unwrap();
    h.config.committees.push(committee());
    start(&mut h).await;
    let _: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: committee(),
            alias: None,
            cwd: project,
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    let root = h.dir.path().to_path_buf();
    assert!(
        processes_under(&root).len() >= 2,
        "a host and its TUI run: {:?}",
        processes_under(&root)
    );
    (h, root)
}

/// Waits until no process names `root`, failing after `within`.
async fn none_left_under(root: &std::path::Path, within: Duration) {
    let deadline = std::time::Instant::now() + within;
    while !processes_under(root).is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "left behind: {:#?}",
            processes_under(root)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_test_leaves_no_host_or_tui_behind() {
    let (h, root) = one_hosted_session().await;
    // The test ends: the harness, and its temporary home, are dropped.
    drop(h);
    none_left_under(&root, Duration::from_secs(10)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_harness_kills_its_hosts_while_their_home_remains() {
    let (mut h, root) = one_hosted_session().await;
    // Keep the home: only the harness's own guard can end the host here.
    let home = std::mem::replace(&mut h.dir, tempfile::tempdir_in("/tmp").unwrap());
    drop(h);
    none_left_under(&root, Duration::from_secs(3)).await;
    assert!(home.path().exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_whose_directory_is_gone_stops_with_its_tui() {
    let (h, root) = one_hosted_session().await;
    // The harness lives on; the host's home goes, as a deleted test home does.
    std::fs::remove_dir_all(h.paths.home().join("hosts")).unwrap();
    none_left_under(&root, Duration::from_secs(5)).await;
    drop(h);
}

// ---- looking without typing ---------------------------------------------------

/// A hosted session whose TUI paints a line as it starts, answers `paint`
/// with `fixture TUI`, and records every other line it is typed.
async fn watched_session() -> (Harness, String, PathBuf) {
    let (h, typed) = watched_sessions(&["watched"]).await;
    (h, "watched".to_owned(), typed)
}

/// A running daemon whose hosted Claude is the fixture TUI, with room for
/// `limit` hosted sessions: the harness, the project, and the file the
/// fixture records typed lines in.
async fn fixture_daemon(limit: usize) -> (Harness, PathBuf, PathBuf) {
    let mut h = Harness::new().await;
    h.config.hosted_session_limit = Some(limit);
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("watched", None);
    let typed = project.join(".typed");
    let fixture = h.dir.path().join("fixture-claude");
    // A trapped WINCH can interrupt the shell's read mid-line. A separate
    // reader inherits the terminal but handles no resize signals.
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\n{RECORD_HOSTED_ARGV}\n\
             trap 'printf \"resized to %s\\r\\n\" \"$(stty size)\"' WINCH\n\
             printf '\\033[2J\\033[H\\033[32mpainted before anyone looked\\033[m\\r\\n'\n\
             exec 3<&0\n\
             while IFS= read -r line; do\n\
             \x20 if [ \"$line\" = paint ]; then printf 'fixture TUI\\r\\n'\n\
             \x20 else printf '%s\\n' \"$line\" >> {typed:?}; fi\n\
             done <&3 & reader=$!\n\
             while kill -0 \"$reader\" 2>/dev/null; do wait \"$reader\"; done\n",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[companion]\nclaude_bin = {:?}\n",
            fixture.to_str().unwrap()
        ),
    )
    .unwrap();
    h.config.committees.push(committee());
    start(&mut h).await;
    (h, project, typed)
}

/// One such session for each of `aliases`.
async fn watched_sessions(aliases: &[&str]) -> (Harness, PathBuf) {
    let (h, project, typed) = fixture_daemon(aliases.len()).await;
    for alias in aliases {
        let _: SpawnReply = call(
            &h,
            &Request::Spawn {
                attempt_id: None,
                session: Default::default(),
                runtime: Some(HostedRuntime::Claude),
                profile: None,
                committee: committee(),
                alias: Some(path(alias)),
                cwd: project.clone(),
                chair: false,
                secretary: false,
                prompt: None,
                model: Default::default(),
            },
        )
        .await;
    }
    // Every fixture has painted, so its WINCH trap is set: a fit from here
    // on is answered with a repaint.
    eventually("every session's watch socket", TIMEOUT, || {
        (watched_hosts(&h).len() == aliases.len()).then_some(())
    })
    .await;
    for dir in watched_hosts(&h) {
        RawWatcher::at(&dir)
            .await
            .until_said("painted before anyone looked")
            .await;
    }
    (h, typed)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quoted_screenshot_drop_outlives_the_original() {
    let (h, alias, typed) = watched_session().await;
    let session = watched_hosts(&h).pop().unwrap().canonicalize().unwrap();
    let ephemeral = h
        .dir
        .path()
        .join("TemporaryItems/NSIRD_screencaptureui_fixture");
    std::fs::create_dir_all(&ephemeral).unwrap();
    let original = ephemeral.join("Screenshot with spaces.png");
    std::fs::write(&original, b"fixture image bytes").unwrap();

    let keys = format!("\x1b[200~\"{}\"\x1b[201~\r\x1d", original.display());
    attach(&h, &alias, keys.as_bytes()).await;
    let pasted = eventually("the hosted TUI to receive the paste", TIMEOUT, || {
        std::fs::read_to_string(&typed).ok()
    })
    .await;
    std::fs::remove_file(&original).unwrap();
    let path = pasted
        .trim_end_matches('\n')
        .strip_prefix("\x1b[200~\"")
        .and_then(|paste| paste.strip_suffix("\"\x1b[201~"))
        .expect("the paste retains its quotes and framing");
    let copied = PathBuf::from(path);
    assert_eq!(
        std::fs::read(&copied).expect("the pasted path is readable after temporary-file cleanup"),
        b"fixture image bytes"
    );
    assert!(copied.starts_with(session.join("drops")));
    assert_eq!(copied.file_name(), original.file_name());

    let _: senate_bridge::ipc::StopReply = call(
        &h,
        &Request::Stop {
            session: Default::default(),
            senator: common::path(&alias).into(),
            force: false,
        },
    )
    .await;
    assert!(!copied.exists(), "the session owns the staged file");
}

/// Read the PTY as one screen, including markers arriving in the same read.
async fn wait_for_screen(
    output: &mut (impl tokio::io::AsyncRead + Unpin),
    rows: u16,
    cols: u16,
    ready: impl Fn(&str) -> bool,
) {
    let mut parser = vt100::Parser::new(rows.try_into().unwrap(), cols.try_into().unwrap(), 0);
    let mut buf = [0; 4096];
    let mut screen = String::new();
    tokio::time::timeout(TIMEOUT, async {
        while !ready(&screen) {
            let n = output.read(&mut buf).await.unwrap();
            assert_ne!(n, 0, "terminal closed: {screen}");
            parser.process(&buf[..n]);
            screen = parser.screen().contents();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for screen: {screen}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_attach_and_automatic_restore_use_the_hosted_session() {
    let (h, _root, _typed) = fixture_daemon(1).await;
    let alias = "new-viewer";
    let project = h.workdir("spawn-attach", Some("refund-v2"));
    let dir = project.to_str().unwrap();
    let prompt = project.join("prompt.txt");
    std::fs::write(&prompt, "Investigate the isolated fixture").unwrap();

    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    let rejected = cmd
        .env("SENATE_HOME", h.paths.home())
        .args([
            "spawn",
            "claude",
            "--committee",
            "refund-v2",
            "--alias",
            "without-tty",
            "--attach",
        ])
        .output()
        .await
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("needs a terminal"));
    assert!(
        !h.store()
            .all_sessions()
            .unwrap()
            .iter()
            .any(|s| s.cwd == project)
    );

    pty_attach_command(
        &h,
        &[
            "spawn",
            "claude",
            "--committee",
            "refund-v2",
            "--alias",
            "new-viewer",
            "--dir",
            dir,
            "--prompt-file",
            prompt.to_str().unwrap(),
            "-a",
        ],
        true,
        b"\x1d",
    )
    .await;
    let listing: senate_bridge::ipc::SessionsReply = call(&h, &Request::Sessions).await;
    let spawned = listing
        .rows
        .iter()
        .find(|row| row.alias.as_ref() == Some(&path("new-viewer")))
        .unwrap();
    assert!(spawned.is_running() && spawned.dir == project.canonicalize().unwrap());
    assert!(
        h.server
            .events("refund-v2")
            .iter()
            .any(|event| event.body == "Investigate the isolated fixture")
    );

    let senator: senate_core::alias::AliasName = alias.parse().unwrap();
    let _: senate_bridge::ipc::StopReply = call(
        &h,
        &Request::Stop {
            session: Default::default(),
            senator: senator.clone(),
            force: true,
        },
    )
    .await;
    let stopped: senate_bridge::ipc::AttachInfo = call(
        &h,
        &Request::AttachInfo {
            target: senate_bridge::ipc::AttachTarget::Senator {
                senator: senator.clone(),
            },
        },
    )
    .await;
    assert!(stopped.tty.is_none() && !stopped.paused);
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    let watching = cmd
        .env("SENATE_HOME", h.paths.home())
        .args(["attach", "--read-only", alias])
        .output()
        .await
        .unwrap();
    assert!(!watching.status.success());
    assert!(String::from_utf8_lossy(&watching.stderr).contains("not running"));
    let still_stopped: senate_bridge::ipc::AttachInfo = call(
        &h,
        &Request::AttachInfo {
            target: senate_bridge::ipc::AttachTarget::Senator {
                senator: senator.clone(),
            },
        },
    )
    .await;
    assert_eq!(still_stopped, stopped);

    attach(&h, alias, b"\x1d").await;
    let restored: senate_bridge::ipc::AttachInfo = call(
        &h,
        &Request::AttachInfo {
            target: senate_bridge::ipc::AttachTarget::Senator {
                senator: senator.clone(),
            },
        },
    )
    .await;
    assert_eq!(restored.session_id, stopped.session_id);
    assert!(restored.tty.is_some());

    let _: ipc::PauseReply = call(
        &h,
        &Request::Pause {
            senator: senator.clone(),
            force: true,
            timeout_secs: Some(1),
        },
    )
    .await;
    let paused: ipc::AttachInfo = call(
        &h,
        &Request::AttachInfo {
            target: ipc::AttachTarget::Senator {
                senator: senator.clone(),
            },
        },
    )
    .await;
    assert!(paused.paused && paused.tty.is_none());
    // With no name, the picker lists and opens the paused terminal.
    pty_attach_command(&h, &["attach"], true, b"\x1d").await;
    let resumed: ipc::AttachInfo = call(
        &h,
        &Request::AttachInfo {
            target: ipc::AttachTarget::Senator { senator },
        },
    )
    .await;
    assert_eq!(resumed.session_id, paused.session_id);
    assert!(!resumed.paused && resumed.tty.is_some());
    assert!(
        h.server
            .events("refund-v2")
            .iter()
            .any(|event| event.body.contains("resumed after a pause"))
    );
}

/// A real attach CLI and a hosted PTY, with no graphical terminal required.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_attach_uses_the_native_screen_and_cleans_up_on_detach() {
    let (h, alias, _typed) = watched_session().await;
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(["attach", &alias])
        .env("SENATE_HOME", h.paths.home())
        .kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let initial = b"painted before anyone looked";
    let mut answered = false;
    tokio::time::timeout(TIMEOUT, async {
        while !bytes.windows(initial.len()).any(|part| part == initial) {
            let n = output.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0);
            bytes.extend_from_slice(&chunk[..n]);
            if !answered && bytes.windows(3).any(|part| part == b"\x1b[c") {
                input.write_all(b"\x1b[?62;22c").await.unwrap();
                answered = true;
            }
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains(&format!("\x1b[2m── {alias} · ")));
    assert!(!bytes.windows(6).any(|part| part == b"?1049h"));
    assert!(!bytes.windows(5).any(|part| part == b"?1000"));
    assert!(!bytes.windows(4).any(|part| part == b"\x1b[3J"));

    input.write_all(b"\x1d").await.unwrap();
    tokio::time::timeout(
        TIMEOUT,
        wait_terminal_output(&mut output, b"\x1b[?25h\x1b[9999;1H\r\n"),
    )
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

/// `senate attach --read-only` shows the screen as it is, though it was
/// painted before the viewer came, and what the session writes after. What
/// is typed into it goes nowhere and is no activity of the session's
/// viewer; it runs beside a normal attach, and Ctrl-] ends it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_only_attach_shows_the_screen_and_types_nothing() {
    let (h, alias, typed) = watched_session().await;
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(["attach", "--read-only", &alias])
        .env("SENATE_HOME", h.paths.home())
        .kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(30, 100)).unwrap();
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    wait_for_screen(&mut output, 30, 100, |screen| {
        screen.contains("painted before anyone looked") && screen.contains("read-only")
    })
    .await;

    // Typing into it: a line, an arrow, and `q`.
    input.write_all(b"hello there\r\x1b[Aq").await.unwrap();
    // A normal attach beside it types `paint`, which the session answers;
    // the read-only viewer shows the answer.
    attach(&h, &alias, b"\x1d").await;
    tokio::time::timeout(TIMEOUT, wait_terminal_output(&mut output, b"fixture TUI"))
        .await
        .expect("what the session wrote next");
    assert!(
        child.try_wait().unwrap().is_none(),
        "`q` is typing, not leaving"
    );
    // Nothing the read-only viewer typed reached the session.
    let reached = std::fs::read_to_string(&typed).unwrap_or_default();
    assert!(!reached.contains("hello"), "{reached:?}");
    assert!(!reached.contains('q'), "{reached:?}");

    // A terminal goes on taking what is painted on it.
    let draining = tokio::spawn(async move {
        let mut buf = [0; 4096];
        while output.read(&mut buf).await.is_ok_and(|n| n > 0) {}
    });
    // Ctrl-] leaves, and the session runs on.
    input.write_all(b"\x1d").await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    draining.abort();
    attach(&h, &alias, b"\x1d").await;
}

/// A raw watcher of a host's `see` socket: the sizes it was told and the
/// text the TUI wrote, as they come.
struct RawWatcher {
    stream: tokio::net::UnixStream,
    sizes: Vec<(u16, u16)>,
    text: Vec<u8>,
}

/// Every host's directory with a watch socket.
fn watched_hosts(h: &Harness) -> Vec<PathBuf> {
    std::fs::read_dir(h.paths.home().join("hosts"))
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().path())
                .filter(|dir| dir.join("see").exists())
                .collect()
        })
        .unwrap_or_default()
}

impl RawWatcher {
    async fn open(h: &Harness) -> Self {
        let dir = watched_hosts(h)
            .into_iter()
            .next()
            .expect("a host with a watch socket");
        Self::at(&dir).await
    }

    async fn at(dir: &std::path::Path) -> Self {
        let mut stream = tokio::net::UnixStream::connect(dir.join("see"))
            .await
            .unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 1, "the watch greeting");
        Self {
            stream,
            sizes: Vec::new(),
            text: Vec::new(),
        }
    }

    async fn fit(&mut self, rows: u16, cols: u16) {
        let mut frame = vec![1u8];
        frame.extend_from_slice(&rows.to_be_bytes());
        frame.extend_from_slice(&cols.to_be_bytes());
        self.stream.write_all(&frame).await.unwrap();
    }

    async fn frame(&mut self) {
        match self.stream.read_u8().await.unwrap() {
            1 => {
                let rows = self.stream.read_u16().await.unwrap();
                let cols = self.stream.read_u16().await.unwrap();
                self.sizes.push((rows, cols));
            }
            2 => {
                let n = usize::from(self.stream.read_u16().await.unwrap());
                let mut bytes = vec![0; n];
                self.stream.read_exact(&mut bytes).await.unwrap();
                self.text.extend(bytes);
            }
            3 => {
                let n = usize::try_from(self.stream.read_u32().await.unwrap()).unwrap();
                let mut bytes = vec![0; n];
                self.stream.read_exact(&mut bytes).await.unwrap();
                self.text.extend(bytes);
            }
            tag => panic!("unknown watch frame {tag}"),
        }
    }

    /// Read frames until the TUI has said `text`.
    async fn until_said(&mut self, text: &str) {
        let wait = async {
            while !self.text.windows(text.len()).any(|w| w == text.as_bytes()) {
                self.frame().await;
            }
        };
        tokio::time::timeout(TIMEOUT, wait)
            .await
            .unwrap_or_else(|_| panic!("never said {text:?}"));
        self.text.clear();
    }

    /// Read frames until the session is `size` and the TUI has said `text`.
    async fn until(&mut self, size: (u16, u16), text: &str) {
        let wait = async {
            while self.sizes.last() != Some(&size)
                || !self.text.windows(text.len()).any(|w| w == text.as_bytes())
            {
                self.frame().await;
            }
        };
        tokio::time::timeout(TIMEOUT, wait)
            .await
            .unwrap_or_else(|_| panic!("never {size:?} with {text:?}: {:?}", self.sizes));
        self.text.clear();
    }

    /// Read whatever comes for `how_long`.
    async fn drain_for(&mut self, how_long: Duration) {
        let _ = tokio::time::timeout(how_long, async {
            loop {
                self.frame().await;
            }
        })
        .await;
    }
}

/// With nobody attached, a watcher's fit resizes the session and the TUI
/// repaints at that size; the latest fit wins. An attached viewer's size
/// wins instead, and the fit standing applies again when it detaches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watcher_fits_an_unattached_session_and_never_an_attached_one() {
    let (h, alias, _typed) = watched_session().await;
    let mut watcher = RawWatcher::open(&h).await;
    watcher.fit(12, 50).await;
    watcher.until((12, 50), "resized to 12 50").await;
    // The latest fit wins: a re-fit, as a tile does on a terminal resize.
    watcher.fit(15, 70).await;
    watcher.until((15, 70), "resized to 15 70").await;

    // Attached: the viewer's size, and a fit asked meanwhile is not applied.
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(["attach", &alias])
        .env("SENATE_HOME", h.paths.home())
        .kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    tokio::time::timeout(TIMEOUT, wait_terminal_output(&mut output, b"\x1b[c"))
        .await
        .unwrap();
    input.write_all(b"\x1b[?62;22c").await.unwrap();
    watcher.until((24, 80), "resized to 24 80").await;
    watcher.fit(9, 40).await;
    watcher.drain_for(Duration::from_millis(600)).await;
    assert_eq!(
        watcher.sizes.last(),
        Some(&(24, 80)),
        "the attached viewer's size stands: {:?}",
        watcher.sizes
    );
    let draining = tokio::spawn(async move {
        let mut buf = [0; 4096];
        while output.read(&mut buf).await.is_ok_and(|n| n > 0) {}
    });
    input.write_all(b"\x1d").await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    draining.abort();
    // Detached: the fit standing applies again.
    watcher.until((9, 40), "resized to 9 40").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_all_hints_when_every_viewer_is_attached_and_all_shows_them() {
    let (h, _) = watched_sessions(&["north"]).await;
    let tty: PathBuf = call(
        &h,
        &Request::Attach {
            senator: path("north").into(),
        },
    )
    .await;
    let mut interactive = tokio::net::UnixStream::connect(&tty).await.unwrap();
    begin_viewer(&mut interactive).await;

    let command = || {
        let mut cmd = tokio::process::Command::new(BIN);
        testenv::isolate_async(&mut cmd, h.dir.path());
        cmd.args(["session", "watch"])
            .env("SENATE_HOME", h.paths.home())
            .kill_on_drop(true);
        cmd
    };
    let out = tokio::time::timeout(TIMEOUT, command().output())
        .await
        .unwrap()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let hint = std::str::from_utf8(&out.stdout).unwrap();
    assert_eq!(hint.lines().count(), 1);
    assert!(hint.contains("--all"), "{hint}");

    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 100)).unwrap();
    let mut cmd = command();
    cmd.arg("--all");
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    wait_for_screen(&mut output, 24, 100, |s| {
        s.contains("refund-v2::north · claude")
    })
    .await;
    input.write_all(b"q").await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
}

/// `senate session watch` shows unattached sessions side by side, each under
/// its name. Enter attaches to the focused one as `senate attach` does,
/// detaching comes back to the grid, and `q` leaves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watch_all_shows_every_session_and_enter_attaches_to_one() {
    let (h, typed) = watched_sessions(&["north", "south"]).await;
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(["session", "watch"])
        .env("SENATE_HOME", h.paths.home())
        .kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(30, 120)).unwrap();
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    wait_for_screen(&mut output, 30, 120, |screen| {
        screen.contains("refund-v2::north · claude")
            && screen.contains("refund-v2::south · claude")
            && screen.matches("painted before anyone looked").count() == 2
            && screen.matches("resized to 28 59").count() == 2
            && screen.contains("2 sessions")
    })
    .await;

    // Typing in the grid reaches no session; the arrow moves to `south`,
    // and Enter attaches there.
    input.write_all(b"xyz\x1b[C\r").await.unwrap();
    tokio::time::timeout(TIMEOUT, async {
        // A normal attach: it asks this terminal for its colours first.
        wait_terminal_output(&mut output, b"\x1b[c").await;
        input.write_all(b"\x1b[?62;22c").await.unwrap();
        wait_terminal_output(&mut output, b"resized to 30 120").await;
        input.write_all(b"from the grid\rpaint\r").await.unwrap();
        wait_terminal_output(&mut output, b"fixture TUI").await;
        // Detaching comes back to the grid.
        input.write_all(b"\x1d").await.unwrap();
        wait_terminal_output(&mut output, b"2 sessions").await;
    })
    .await
    .expect("attached from the grid, and back");
    let reached = std::fs::read_to_string(&typed).unwrap_or_default();
    assert_eq!(reached, "from the grid\n", "only what the attach typed");

    let draining = tokio::spawn(async move {
        let mut buf = [0; 4096];
        while output.read(&mut buf).await.is_ok_and(|n| n > 0) {}
    });
    input.write_all(b"q").await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    draining.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spawn_prompt_is_an_ordinary_request_to_the_new_seat() {
    let mut h = Harness::new().await;
    h.config.hosted_session_limit = Some(1);
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("prompted", None);
    // The TUI records every line typed into it.
    let fixture = h.dir.path().join("fixture-claude");
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\n{RECORD_HOSTED_ARGV}\nwhile IFS= read -r line; do printf '%s\\n' \"$line\" >> typed; done\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[companion]\nclaude_bin = {:?}\n",
            fixture.to_str().unwrap()
        ),
    )
    .unwrap();
    h.config.committees.push(committee());
    start(&mut h).await;
    let reply: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: committee(),
            alias: Some(path("reviewer")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: Some("Review the open PR on the gateway".into()),
            model: Default::default(),
        },
    )
    .await;
    let seq = reply.request.expect("the prompt's request");
    let sent = h
        .server
        .events(committee().as_str())
        .into_iter()
        .find(|e| e.seq.0 == seq.0)
        .expect("the request in the log");
    assert_eq!(sent.kind, senate_core::Kind::Request);
    // From the person who spawned it (the CLI), to the new seat.
    assert_eq!(sent.from, common::person("alice"));
    assert_eq!(sent.to, vec![senate_core::Address::senator(&reply.senator)]);
    assert_eq!(sent.body, "Review the open PR on the gateway");
    assert!(sent.thread.is_none());
    // Nothing was typed into its terminal: not the prompt, and not the
    // idle nudge, which is agy's alone, even asked as agy's Stop asks.
    let waiting: senate_bridge::ipc::WaitingReply = call(
        &h,
        &Request::Waiting {
            session_id: reply.session_id.clone(),
        },
    )
    .await;
    let _ = waiting;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let typed = std::fs::read_to_string(project.join("typed")).unwrap_or_default();
    assert!(!typed.contains("Review the open PR"), "{typed}");
    assert!(!typed.contains("[senate] new messages"), "{typed}");

    h.stop().await;
}

fn tracked_spawn(project: PathBuf, prompt: Option<&str>) -> Request {
    Request::Spawn {
        attempt_id: Some(uuid::Uuid::now_v7().to_string()),
        session: Default::default(),
        runtime: Some(HostedRuntime::Claude),
        profile: None,
        committee: committee(),
        alias: None,
        cwd: project,
        chair: false,
        secretary: false,
        prompt: prompt.map(str::to_owned),
        model: Default::default(),
    }
}

async fn disconnect_spawn_after_creation(h: &Harness, request: &Request, prompt: &str) -> String {
    let (arrived, release) = h.server.hold_next_append(prompt);
    let mut connection = tokio::net::UnixStream::connect(h.socket()).await.unwrap();
    let mut line = serde_json::to_vec(request).unwrap();
    line.push(b'\n');
    connection.write_all(&line).await.unwrap();
    // The session already exists and the held prompt keeps the handler from
    // trying to write its IPC reply until this connection is gone.
    tokio::time::timeout(TIMEOUT, arrived)
        .await
        .unwrap()
        .unwrap();
    let sessions = h.store().all_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    drop(connection);
    release.send(()).unwrap();
    sessions[0].session_id.clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_attempt_replays_the_same_session_after_ipc_disconnect() {
    let (mut h, project, _) = fixture_daemon(2).await;
    let prompt = "Starting prompt for a spawn with a lost reply";
    let request = tracked_spawn(project, Some(prompt));
    let session_id = disconnect_spawn_after_creation(&h, &request, prompt).await;

    let retry: SpawnReply = call(&h, &request).await;
    assert_eq!(retry.session_id, session_id);
    assert!(retry.request.is_some());
    h.stop().await;
    start(&mut h).await;
    assert_eq!(call::<SpawnReply>(&h, &request).await, retry);
    assert_eq!(h.store().all_sessions().unwrap().len(), 1);
    assert_eq!(
        h.server
            .events(committee().as_str())
            .iter()
            .filter(|e| e.body == prompt)
            .count(),
        1
    );
    h.stop().await;
}

async fn expect_spawn_outcome_unknown(h: &Harness, request: &Request) -> ipc::IpcError {
    let error = ipc::request::<SpawnReply>(&h.socket(), request, TIMEOUT, TIMEOUT)
        .await
        .unwrap_err();
    let ipc::ClientError::Remote(error) = error else {
        panic!("expected a daemon error, got {error}");
    };
    assert_eq!(error.code, ipc::codes::SPAWN_OUTCOME_UNKNOWN);
    assert!(error.message.contains("senate session list"));
    error
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_attempt_stays_unknown_if_the_session_receipt_was_not_saved() {
    let (mut h, project, _) = fixture_daemon(2).await;
    let request = tracked_spawn(project, None);
    let db = rusqlite::Connection::open(h.paths.state_db()).unwrap();
    db.busy_timeout(TIMEOUT).unwrap();
    db.execute_batch(
        "CREATE TRIGGER fail_spawn_receipt BEFORE UPDATE ON spawn_attempts
         WHEN json_type(NEW.record, '$.phase.Created') IS NOT NULL
         BEGIN SELECT RAISE(ABORT, 'injected receipt failure'); END;",
    )
    .unwrap();

    expect_spawn_outcome_unknown(&h, &request).await;
    let sessions = h.store().all_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    db.execute_batch("DROP TRIGGER fail_spawn_receipt").unwrap();
    h.stop().await;
    start(&mut h).await;
    expect_spawn_outcome_unknown(&h, &request).await;
    let Request::Spawn {
        attempt_id: Some(id),
        cwd,
        ..
    } = &request
    else {
        unreachable!();
    };
    let (ok, output) = senate(
        &h,
        &[
            "spawn",
            "claude",
            "--attempt-id",
            id,
            "--committee",
            committee().as_str(),
            "--dir",
            cwd.to_str().unwrap(),
        ],
    )
    .await;
    assert!(
        !ok && output.contains("Spawn outcome unknown") && output.contains(id),
        "{output}"
    );
    let remaining = h.store().all_sessions().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].session_id, sessions[0].session_id);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_attempt_resumes_with_original_order_after_profile_is_removed() {
    let (mut h, project, _) = fixture_daemon(2).await;
    let missing = project.join("not-yet-created");
    h.stop().await;
    h.config.profiles.insert(
        "review".into(),
        senate_bridge::config::LaunchProfile {
            runtime: HostedRuntime::Claude,
            model: Default::default(),
            note: "Review".into(),
        },
    );
    start(&mut h).await;
    let mut request = tracked_spawn(missing.clone(), None);
    let Request::Spawn {
        runtime, profile, ..
    } = &mut request
    else {
        unreachable!();
    };
    *runtime = None;
    *profile = Some(ipc::SpawnProfile {
        name: "review".into(),
        runtime: None,
    });
    let error = ipc::request::<SpawnReply>(&h.socket(), &request, TIMEOUT, TIMEOUT)
        .await
        .unwrap_err();
    let ipc::ClientError::Remote(error) = error else {
        panic!("expected a daemon error, got {error}");
    };
    assert_eq!(error.code, ipc::codes::BAD_REQUEST);
    assert!(h.store().all_sessions().unwrap().is_empty());
    h.stop().await;
    h.config.profiles.clear();
    std::fs::create_dir(missing).unwrap();
    start(&mut h).await;

    let resumed: SpawnReply = call(&h, &request).await;
    assert_eq!(resumed.display_name.as_deref(), Some("review"));
    assert_eq!(call::<SpawnReply>(&h, &request).await, resumed);

    let Request::Spawn { attempt_id, .. } = &mut request else {
        unreachable!();
    };
    *attempt_id = Some(uuid::Uuid::now_v7().to_string());
    let fresh_error = ipc::request::<SpawnReply>(&h.socket(), &request, TIMEOUT, TIMEOUT)
        .await
        .unwrap_err();
    assert!(
        fresh_error.to_string().contains("unknown launch profile"),
        "{fresh_error}"
    );
    let sessions = h.store().all_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, resumed.session_id);
    h.stop().await;
}

// Control for the lost-reply test: retrying the same untracked wire
// request still launches a distinct session, including for older clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_attempt_legacy_requests_are_independent_launches() {
    let (mut h, project, _) = fixture_daemon(2).await;
    let prompt = "Starting prompt for a spawn with a lost reply";
    let mut request = tracked_spawn(project, Some(prompt));
    let Request::Spawn { attempt_id, .. } = &mut request else {
        unreachable!();
    };
    *attempt_id = None;

    let session_id = disconnect_spawn_after_creation(&h, &request, prompt).await;
    let retry: SpawnReply = call(&h, &request).await;
    assert_ne!(session_id, retry.session_id);
    assert_eq!(h.store().all_sessions().unwrap().len(), 2);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_attempt_expiry_prunes_the_receipt_and_refuses_reuse() {
    let (mut h, project, _) = fixture_daemon(2).await;
    let mut request = tracked_spawn(project, None);
    let Request::Spawn { attempt_id, .. } = &mut request else {
        unreachable!();
    };
    let days_ago = chrono::Utc::now().timestamp() - 8 * 24 * 60 * 60;
    let timestamp = uuid::Timestamp::from_unix(uuid::NoContext, days_ago.try_into().unwrap(), 0);
    let id = uuid::Uuid::new_v7(timestamp).to_string();
    *attempt_id = Some(id.clone());

    let db = rusqlite::Connection::open(h.paths.state_db()).unwrap();
    db.busy_timeout(TIMEOUT).unwrap();
    let expires = (days_ago + 7 * 24 * 60 * 60) * 1000;
    db.execute(
        "INSERT INTO spawn_attempts (id, record, expires) VALUES (?1, '{}', ?2)",
        rusqlite::params![id, expires],
    )
    .unwrap();
    h.store()
        .prune(Duration::from_secs(3600), &h.paths)
        .unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM spawn_attempts", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );

    let error = expect_spawn_outcome_unknown(&h, &request).await;
    assert!(error.message.contains("retry window has expired"));
    assert!(h.store().all_sessions().unwrap().is_empty());
    h.stop().await;
}

/// `senate <args>` against the harness's daemon; its output and success.
async fn senate(h: &Harness, args: &[&str]) -> (bool, String) {
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    let out = cmd
        .args(args)
        .env("SENATE_HOME", h.paths.home())
        .output()
        .await
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_spawn_profile_uses_its_alias_and_accepts_model_and_effort_overrides() {
    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("profile-cli", None);
    let fixture = agy_fixture(h.dir.path(), "3f2b8c1e-6d4a-4e7b-9c0d-1a2b3c4d5e6f");
    let config = toml::to_string(&h.config).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            r#"{config}
[agy]
bin = {fixture:?}
[profiles.review]
runtime = "agy"
model = "not-in-catalog"
effort = "low"
note = "Use for review."
"#
        ),
    )
    .unwrap();
    h.config = senate_bridge::BridgeConfig::load(&h.paths.config()).unwrap();
    h.config.committees.push(committee());
    start(&mut h).await;

    let committee = committee();
    let spawn = [
        "--profile",
        "review",
        "--committee",
        committee.as_str(),
        "--dir",
        project.to_str().unwrap(),
    ];
    let (ok, text) = senate(&h, &[&["spawn", "claude"], &spawn[..]].concat()).await;
    assert!(
        !ok && text.contains("conflicts with launch profile"),
        "{text}"
    );
    let (ok, text) = senate(
        &h,
        &[
            &["spawn", "agy"][..],
            &spawn,
            &["--model", "flash", "--effort", "minimal"],
        ]
        .concat(),
    )
    .await;
    assert!(
        !ok && text.contains("agy does not take the effort"),
        "{text}"
    );
    let (ok, text) = senate(
        &h,
        &[
            &["spawn", "agy"][..],
            &spawn,
            &["--model", "flash", "--effort", "high"],
        ]
        .concat(),
    )
    .await;
    assert!(ok, "{text}");
    assert!(text.contains("flash → gemini-3.8-flash"), "{text}");
    let terminal: PathBuf = call(
        &h,
        &Request::Attach {
            senator: path("review").into(),
        },
    )
    .await;
    let argv = hosted_fixture_argv(&terminal, &project, TIMEOUT).await;
    assert!(
        argv.contains(&"--model=gemini-3.8-flash".to_owned()),
        "{argv:?}"
    );
    assert!(argv.contains(&"--effort=high".to_owned()), "{argv:?}");
    assert!(!argv.contains(&"--effort=low".to_owned()), "{argv:?}");
    h.stop().await;
}

/// `senate join` on a name the server has no event of opens the committee
/// there with one note from the person, so a spawn right after it works;
/// spawn itself still refuses a committee nobody opened, and joining again
/// posts nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_a_new_committee_opens_it_so_a_spawn_works() {
    let (h, project, _typed) = fixture_daemon(1).await;
    let fresh = senate_core::CommitteeId::new("fresh-work").unwrap();
    let spawn = Request::Spawn {
        attempt_id: None,
        session: Default::default(),
        runtime: Some(HostedRuntime::Claude),
        profile: None,
        committee: fresh.clone(),
        alias: Some(path("main")),
        cwd: project.clone(),
        chair: false,
        secretary: false,
        prompt: None,
        model: Default::default(),
    };
    let refused = ipc::request::<SpawnReply>(
        &h.paths.socket(),
        &spawn,
        Duration::from_secs(1),
        Duration::from_secs(20),
    )
    .await
    .expect_err("spawn never creates a committee");
    assert!(
        refused.to_string().contains("unknown committee fresh-work"),
        "{refused}"
    );

    let join = || {
        let mut cmd = tokio::process::Command::new(BIN);
        testenv::isolate_async(&mut cmd, h.dir.path());
        cmd.args(["join", "fresh-work"])
            .current_dir(&project)
            .env("SENATE_HOME", h.paths.home());
        cmd
    };
    let out = join().output().await.unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        said.contains("Opened committee fresh-work on the server"),
        "{said}"
    );
    let opened = h.server.events("fresh-work");
    assert_eq!(opened.len(), 1, "{opened:#?}");
    assert_eq!(opened[0].kind, senate_core::Kind::Note);
    assert_eq!(opened[0].body, "alice opened committee fresh-work");
    assert_eq!(opened[0].from, common::person("alice"), "from the person");

    // Joining again posts nothing more.
    let out = join().output().await.unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(said.contains("is already open on the server"), "{said}");
    assert_eq!(h.server.events("fresh-work").len(), 1);

    let spawned: SpawnReply = call(&h, &spawn).await;
    assert_eq!(spawned.committee, fresh);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sessions_lists_stops_by_name_and_prunes_an_orphan() {
    let (mut h, root) = one_hosted_session().await;
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(reply.rows.len(), 1, "{reply:?}");
    let row = &reply.rows[0];
    assert_eq!(row.state, ipc::HostState::Running);
    assert_eq!(row.committee.as_ref(), Some(&committee()));
    assert!(row.tty.exists(), "the attach socket: {}", row.tty.display());
    assert!(row.started_at.is_some());
    assert!(reply.orphans.is_empty());
    let (ok, listed) = senate(&h, &["session", "list"]).await;
    assert!(ok, "{listed}");
    assert!(
        listed.contains(&row.display_name(false)) && listed.contains("running"),
        "{listed}"
    );

    // A host that no session owns: started by hand under the same home.
    let orphan = senate_core::EventId::new();
    std::fs::create_dir_all(h.paths.home().join("hosts").join(orphan.to_string())).unwrap();
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(["session-host", &orphan.to_string(), "claude"])
        .arg(h.dir.path())
        .args(["orphan-native", "--program"])
        .arg(h.dir.path().join("fixture-claude"))
        .env("SENATE_HOME", h.paths.home())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut host = cmd.spawn().unwrap();
    let ready = tokio::time::timeout(TIMEOUT, host.stdout.as_mut().unwrap().read_u8())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ready, 1);
    host.stdin.take().unwrap().write_all(&[1]).await.unwrap();
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(reply.orphans, vec![orphan.clone()]);

    let (ok, pruned) = senate(&h, &["session", "prune"]).await;
    assert!(ok, "{pruned}");
    assert!(
        pruned.contains(&format!("stopped orphaned host {orphan}")),
        "{pruned}"
    );
    let _ = tokio::time::timeout(TIMEOUT, host.wait()).await.unwrap();
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert!(reply.orphans.is_empty());
    assert!(
        !h.paths
            .home()
            .join("hosts")
            .join(orphan.to_string())
            .exists()
    );
    assert_eq!(
        reply.rows[0].state,
        ipc::HostState::Running,
        "prune leaves owned hosts"
    );

    // Stop by name, as listed; nobody typed into it, so no --force.
    let (ok, stopped) = senate(&h, &["session", "stop", &row.label()]).await;
    assert!(ok, "{stopped}");
    assert!(
        stopped.contains(&format!("stopped {}", row.label())),
        "{stopped}"
    );
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(reply.rows[0].state, ipc::HostState::Stopped, "{reply:?}");
    let (ok, again) = senate(&h, &["session", "stop", &row.label()]).await;
    assert!(
        !ok && again.contains("no running hosted session"),
        "{again}"
    );
    none_left_under(&root, Duration::from_secs(5)).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attach_without_a_name_picks_from_the_running_sessions() {
    let (mut h, root) = one_hosted_session().await;
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    let label = reply.rows[0].label();
    let open = || {
        let mut cmd = tokio::process::Command::new(BIN);
        testenv::isolate_async(&mut cmd, h.dir.path());
        cmd.arg("attach")
            .env("SENATE_HOME", h.paths.home())
            .kill_on_drop(true);
        let (pty, pts) = pty_process::open().unwrap();
        pty.resize(pty_process::Size::new(24, 80)).unwrap();
        let child = pty_process::Command::from(cmd).spawn(pts).unwrap();
        let (output, input) = pty.into_split();
        (child, output, input)
    };

    // Cancelled: nothing attaches.
    let (mut child, mut output, mut input) = open();
    tokio::time::timeout(TIMEOUT, async {
        wait_terminal_output(&mut output, b"Attach to which session?").await;
    })
    .await
    .unwrap();
    input.write_all(b"q").await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );

    // Enter on the one listed attaches to it: the attach asks the terminal
    // for its colours, then Ctrl-] detaches.
    let (mut child, mut output, mut input) = open();
    tokio::time::timeout(TIMEOUT, async {
        wait_terminal_output(&mut output, label.as_bytes()).await;
        input.write_all(b"\r").await.unwrap();
        wait_terminal_output(&mut output, b"\x1b[c").await;
        input
            .write_all(b"\x1b]11;rgb:efef/f1f1/f5f5\x1b\\\x1b[?62;22c")
            .await
            .unwrap();
    })
    .await
    .unwrap();
    // Attached once it has the answer; then Ctrl-] detaches.
    tokio::time::sleep(Duration::from_millis(500)).await;
    input.write_all(b"\x1d").await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    h.stop().await;
    drop(h);
    none_left_under(&root, Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bare_alias_on_several_committees_opens_the_picker_on_a_terminal() {
    let (mut h, root) = one_hosted_session().await;
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    let primary = reply.rows[0].primary.clone();
    {
        let mut inner = h.server.state.inner.lock().unwrap();
        inner
            .aliases
            .set(&committee(), path("reviewer"), primary.clone())
            .unwrap();
        inner
            .aliases
            .set(
                &senate_core::CommitteeId::new("other").unwrap(),
                path("reviewer"),
                path("codex"),
            )
            .unwrap();
    }

    // Not on a terminal: an error that lists them to name one.
    let (ok, text) = senate(&h, &["attach", "reviewer"]).await;
    assert!(!ok, "{text}");
    assert!(
        text.contains("reviewer is an alias on several committees"),
        "{text}"
    );
    assert!(
        text.contains("refund-v2::reviewer") && text.contains("other::reviewer"),
        "{text}"
    );

    // On a terminal: the picker, the hosted one described, the other not.
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(["attach", "reviewer"])
        .env("SENATE_HOME", h.paths.home())
        .kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 100)).unwrap();
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    tokio::time::timeout(TIMEOUT, async {
        // The prompt, then each choice: the other committee's ends its line
        // saying it is not hosted here.
        wait_terminal_output(&mut output, b"not hosted here").await;
    })
    .await
    .unwrap();
    input.write_all(b"\x1b").await.unwrap();
    let status = tokio::time::timeout(TIMEOUT, child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(!status.success(), "cancelled");
    h.stop().await;
    drop(h);
    none_left_under(&root, Duration::from_secs(5)).await;
}

/// Offline, `senate join` still binds the worktree, and says when the
/// committee will be opened on the server instead of failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn joining_offline_binds_and_says_when_the_committee_opens() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let home = dir.path().join("senate");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, dir.path());
    let out = cmd
        .args(["join", "fresh-work"])
        .current_dir(&work)
        .env("SENATE_HOME", &home)
        .output()
        .await
        .unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{said}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(said.contains("Bound"), "{said}");
    assert!(
        said.contains(
            "committee fresh-work is not open there yet: it opens the next time a session starts \
             there, or when you run `senate join fresh-work` again"
        ),
        "{said}"
    );
    assert_eq!(
        std::fs::read_to_string(work.join(".senate/committee")).unwrap(),
        "fresh-work\n"
    );
}

/// A message for a stopped hosted senator waits; once `senate session restore`
/// brings it back and its terminal reports in, it is pushed into the
/// session's inbox with nothing new arriving for it, not left as start
/// context in a conversation nobody is typing in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mail_held_while_stopped_is_pushed_once_restored() {
    use senate_bridge::ipc::{HookEvent, HookReply, StopReply};
    use senate_core::{Address, Kind, NewEvent};

    let fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    let native = fixture.seat.session_id.clone();
    let report = |pid: u32, event, source, inbox: &common::FakeInbox| {
        let mut hook = fixture.hook(&native, event, source, Some(pid), &[pid]);
        let Request::SessionHook(report) = &mut hook else {
            unreachable!()
        };
        report.socket = Some(inbox.path.clone());
        hook
    };
    let first = common::FakeInbox::bind(h.dir.path().join("first-inbox.sock"));
    let _: HookReply = call(
        h,
        &report(fixture.pid(), HookEvent::UserPromptSubmit, None, &first),
    )
    .await;

    let _: StopReply = call(
        h,
        &Request::Stop {
            session: Default::default(),
            senator: fixture.seat.senator.clone().into(),
            force: false,
        },
    )
    .await;
    // Its Claude Code is gone with the terminal.
    std::fs::remove_file(&first.path).unwrap();
    drop(first);
    let body = "a note for the stopped senator";
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Note,
            Address::senior(&h.config.person),
            body,
        )
        .to(Address::senator(&fixture.seat.senator)),
    );
    let restored: SpawnReply = call(
        h,
        &Request::Restore {
            session: Default::default(),
            senator: path("seat-fixture").into(),
            model: Default::default(),
        },
    )
    .await;
    assert_eq!(restored, fixture.seat);
    let terminal: PathBuf = call(h, &HostedClaude::attach_request()).await;
    let pid: u32 = std::fs::read_to_string(terminal.with_file_name("owner"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The resumed terminal reports in, as Claude Code does on resume.
    let inbox = common::FakeInbox::bind(h.dir.path().join("restored-inbox.sock"));
    let started: HookReply = call(
        h,
        &report(pid, HookEvent::SessionStart, Some("resume"), &inbox),
    )
    .await;
    // Not as hook context: a resumed hosted terminal is idle, and nothing
    // would act on it until something new arrived.
    assert!(
        !started.context.unwrap_or_default().contains(body),
        "not left in the idle session's start context"
    );
    // Pushed into its inbox, which starts a turn.
    eventually(
        "the held note pushed into the restored session",
        TIMEOUT,
        || {
            inbox
                .messages()
                .into_iter()
                .find(|text| text.contains(body))
        },
    )
    .await;
    rustix::process::kill_process_group(
        rustix::process::Pid::from_raw(pid as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    drop(fixture);
}

/// The hosted agy was given session settings that trust its directory,
/// by its real path, and nothing else.
fn agy_trusts_only(args: &[String], project: &std::path::Path) {
    let at = args
        .iter()
        .position(|a| a == "--settings")
        .expect("--settings");
    let settings: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&args[at + 1]).unwrap()).unwrap();
    let real = std::fs::canonicalize(project).unwrap();
    assert_eq!(
        settings,
        serde_json::json!({ "trustedWorkspaces": [real] }),
        "{args:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosted_agy_names_its_conversation_through_its_first_hook_and_resumes_it() {
    use senate_bridge::ipc::StopReply;
    use senate_bridge::session::Runtime;

    let mut h = Harness::new().await;
    h.config.hosted_session_limit = Some(1);
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("agy-hosted", None);
    let conversation = "3f2b8c1e-6d4a-4e7b-9c0d-1a2b3c4d5e6f";
    let fixture = agy_fixture(h.dir.path(), conversation);
    std::fs::write(
        h.paths.config(),
        format!("[agy]\nbin = {:?}\n", fixture.to_str().unwrap()),
    )
    .unwrap();
    h.config.committees.push(committee());
    start(&mut h).await;

    // agy takes Claude Code's efforts, and nothing else.
    let refused = ipc::request::<SpawnReply>(
        &h.socket(),
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Agy),
            profile: None,
            committee: committee(),
            alias: None,
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: HostedModel::new(None, Some("minimal".into())),
        },
        TIMEOUT,
        TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(
        refused.to_string().contains(
            "agy does not take the effort \"minimal\"; use one of: low, medium, high, xhigh, max"
        ),
        "{refused}"
    );

    let seat: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Agy),
            profile: None,
            committee: committee(),
            alias: Some(path("triage")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: HostedModel::new(Some("flash".into()), Some("high".into())),
        },
    )
    .await;
    let key = Runtime::Agy.session_key(conversation);
    assert_eq!(seat.session_id, key);
    assert_eq!(seat.senator.as_str(), format!("agy-{conversation}"));
    let session = h.store().session(&key).unwrap().unwrap();
    assert_eq!(session.runtime, Runtime::Agy);

    let attach_target = Request::Attach {
        senator: path("triage").into(),
    };
    let terminal: PathBuf = call(&h, &attach_target).await;
    let args = hosted_fixture_argv(&terminal, &project, TIMEOUT).await;
    assert!(
        args[0].starts_with("--prompt-interactive=You are a senator in Senate"),
        "{args:?}"
    );
    assert!(
        args.contains(&"--model=gemini-3.8-flash".to_owned()),
        "{args:?}"
    );
    assert!(args.contains(&"--effort=high".to_owned()), "{args:?}");
    agy_trusts_only(&args, &project);
    // The hook waited for the launch to publish, then answered as agy
    // takes it: Senate's identity line, then the orientation.
    let hook = eventually("the fixture's hook output", TIMEOUT, || {
        std::fs::read_to_string(project.join(".fixture-hook")).ok()
    })
    .await;
    let hook: serde_json::Value = serde_json::from_str(&hook).unwrap();
    let identity = hook["injectSteps"][0]["systemMessage"]["systemMessage"]
        .as_str()
        .unwrap();
    assert_eq!(
        identity,
        format!("[senate] {} · [senator::agy-{conversation}]", committee())
    );
    assert!(
        hook["injectSteps"][1]["userMessage"]
            .as_str()
            .unwrap()
            .contains("This session is joined"),
        "{hook}"
    );

    let _: StopReply = call(
        &h,
        &Request::Stop {
            session: Default::default(),
            senator: path("triage").into(),
            force: true,
        },
    )
    .await;
    let restored: SpawnReply = call(
        &h,
        &Request::Restore {
            session: Default::default(),
            senator: path("triage").into(),
            model: HostedModel::default(),
        },
    )
    .await;
    assert_eq!(restored.session_id, key);
    assert_eq!(restored.senator, seat.senator);
    let terminal: PathBuf = call(&h, &attach_target).await;
    let args = hosted_fixture_argv(&terminal, &project, TIMEOUT).await;
    assert_eq!(args[0], format!("--conversation={conversation}"));
    assert!(
        args.contains(&"--model=gemini-3.8-flash".to_owned()),
        "{args:?}"
    );
    agy_trusts_only(&args, &project);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosted_agy_that_never_names_its_conversation_is_rolled_back() {
    let mut h = Harness::new().await;
    h.config.hosted_session_limit = Some(1);
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("agy-silent", None);
    // agy without Senate's hooks: it runs, but no hook names it.
    let fixture = h.dir.path().join("silent-agy");
    std::fs::write(&fixture, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n").unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!("[agy]\nbin = {:?}\n", fixture.to_str().unwrap()),
    )
    .unwrap();
    h.config.committees.push(committee());
    start(&mut h).await;
    let error = ipc::request::<SpawnReply>(
        &h.socket(),
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Agy),
            profile: None,
            committee: committee(),
            alias: None,
            cwd: project,
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
        TIMEOUT,
        Duration::from_secs(30),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("senate setup install"),
        "{error}"
    );
    assert!(h.store().all_sessions().unwrap().is_empty());
    let hosts = h.paths.home().join("hosts");
    eventually("the unnamed host removed", TIMEOUT, || {
        std::fs::read_dir(&hosts)
            .map_or(true, |mut entries| entries.next().is_none())
            .then_some(())
    })
    .await;
    h.stop().await;
}

/// A stand-in for agy that takes turns: every line typed into it (but
/// `paint` and `q`) is recorded in `.fixture-typed` and, unless
/// `.fixture-hold` exists, starts a turn as agy's does: its PreInvocation
/// hook (output appended to `.fixture-prompts`), then its Stop hook, again
/// while Stop says to go on. It ends its seed turn the same way.
fn agy_turn_fixture(dir: &std::path::Path, conversation: &str) -> PathBuf {
    let fixture = dir.join("fixture-agy-turns");
    std::fs::write(
        &fixture,
        format!(
            r#"#!/bin/sh
case "$1" in
    --prompt-interactive=?*) conversation={conversation} ;;
    --conversation=?*) conversation="${{1#--conversation=}}" ;;
    *) exit 64 ;;
esac
input() {{ printf '{{"conversationId":"%s","cwd":"%s"}}' "$conversation" "$PWD"; }}
stop() {{ input | {BIN:?} hook stop --runtime agy; }}
turn() {{
    while :; do
        input | {BIN:?} hook user-prompt-submit --runtime agy >> .fixture-prompts
        printf '\n' >> .fixture-prompts
        [ -n "$(stop)" ] || break
    done
    printf 'stop\n' >> .fixture-stops
}}
case "$1" in
    --conversation=*)
        stty -icanon -echo
        printf 'New features\r\nEsc to dismiss\r\n'
        dd bs=1 count=1 of=.fixture-dismiss 2>/dev/null
        stty sane
        printf '\033[2J\033[H'
        ;;
esac
input | {BIN:?} hook session-start --runtime agy > /dev/null
case "$1" in
    --prompt-interactive=*) stop > /dev/null; printf 'stop\n' >> .fixture-stops ;;
esac
while IFS= read -r line; do
    case "$line" in
        paint) printf 'fixture TUI\n' ;;
        q) exit 0 ;;
        *)
            printf '%s\n' "$line" >> .fixture-typed
            [ -e .fixture-hold ] || turn
            ;;
    esac
done
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o700)).unwrap();
    fixture
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_hosted_agy_is_nudged_once_per_turn_and_never_over_a_person_typing_or_a_draft() {
    use senate_core::{Address, Kind, NewEvent};

    let mut h = Harness::new().await;
    h.config.hosted_session_limit = Some(1);
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    let project = h.workdir("agy-idle", None);
    let conversation = "7c1d2e3f-4a5b-4c6d-8e9f-0a1b2c3d4e5f";
    let fixture = agy_turn_fixture(h.dir.path(), conversation);
    std::fs::write(
        h.paths.config(),
        format!("[agy]\nbin = {:?}\n", fixture.to_str().unwrap()),
    )
    .unwrap();
    h.config.committees.push(committee());
    start(&mut h).await;
    let seat: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Agy),
            profile: None,
            committee: committee(),
            alias: Some(path("idler")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    let read = |name: &str| std::fs::read_to_string(project.join(name)).unwrap_or_default();
    let typed = || read(".fixture-typed");
    let nudges = || {
        typed()
            .lines()
            .filter(|l| *l == "[senate] new messages")
            .count()
    };
    let note = |body: &str| {
        h.server.append(
            NewEvent::new(
                committee(),
                Kind::Note,
                Address::senior(&h.config.person),
                body,
            )
            .to(Address::senator(&seat.senator)),
        );
    };
    eventually("the seed turn ended", TIMEOUT, || {
        (!read(".fixture-stops").is_empty()).then_some(())
    })
    .await;

    // Idle with something waiting: one nudge, and its prompt hook takes it.
    note("first note for the idle agy");
    eventually("the first nudge delivered", TIMEOUT, || {
        read(".fixture-prompts")
            .contains("first note for the idle agy")
            .then_some(())
    })
    .await;
    assert_eq!(typed(), "[senate] new messages\n");
    // Its turn ends idle: what arrives from here on is a new batch.
    let turns = || read(".fixture-stops").lines().count();
    eventually("the nudged turn ended", TIMEOUT, || {
        (turns() == 2).then_some(())
    })
    .await;

    // A new batch after that PreInvocation is nudged again. This time the
    // agy does not take its turn: what comes next waits, un-nudged.
    std::fs::write(project.join(".fixture-hold"), "").unwrap();
    note("second note");
    eventually("the second nudge", TIMEOUT, || {
        (nudges() == 2).then_some(())
    })
    .await;
    note("third note");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(nudges(), 2, "{}", typed());
    assert!(!read(".fixture-prompts").contains("third note"));

    // A person types a prompt: that turn takes what waited. A note right
    // after it waits out the quiet time before its nudge.
    std::fs::remove_file(project.join(".fixture-hold")).unwrap();
    let typed_at = std::time::Instant::now();
    attach(&h, "idler", b"prompt\r\x1d").await;
    eventually("the person's turn took the waiting notes", TIMEOUT, || {
        read(".fixture-prompts")
            .contains("third note")
            .then_some(())
    })
    .await;
    eventually("the person's turn ended", TIMEOUT, || {
        (turns() == 3).then_some(())
    })
    .await;
    note("fourth note");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(nudges(), 2, "nudged over a typing person: {}", typed());
    eventually("the nudge after the quiet time", TIMEOUT, || {
        read(".fixture-prompts")
            .contains("fourth note")
            .then_some(())
    })
    .await;
    assert_eq!(nudges(), 3, "{}", typed());
    // fast_timing's quiet time is 2 s.
    assert!(typed_at.elapsed() >= Duration::from_secs(2));
    eventually("the third nudged turn ended", TIMEOUT, || {
        (turns() == 4).then_some(())
    })
    .await;

    // A person leaves half a line in the prompt: no nudge is typed onto it,
    // however long it sits there. Once they clear it, the nudge comes.
    attach(&h, "idler", b"dra\x1d").await;
    note("fifth note");
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert_eq!(nudges(), 3, "nudged onto a draft: {}", typed());
    attach_typing(&h, "idler", false, b"\x15\x1d").await;
    eventually("the nudge once the draft is cleared", TIMEOUT, || {
        read(".fixture-prompts")
            .contains("fifth note")
            .then_some(())
    })
    .await;
    assert_eq!(nudges(), 4, "{}", typed());
    assert!(
        !typed().contains("dra"),
        "the draft was submitted: {}",
        typed()
    );
    let _: ipc::PauseReply = call(
        &h,
        &Request::Pause {
            senator: seat.senator.clone().into(),
            force: true,
            timeout_secs: Some(0),
        },
    )
    .await;
    note("backlog while paused");
    let _: ipc::ResumeReply = call(
        &h,
        &Request::Resume {
            senator: seat.senator.clone().into(),
        },
    )
    .await;
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let list: ipc::SessionsReply = call(&h, &Request::Sessions).await;
            if list.rows.iter().any(|row| {
                row.session_id == seat.session_id
                    && row.prompt.as_ref().is_some_and(|prompt| prompt.screen)
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the resumed agy's dismissal overlay appeared in session list");
    attach_typing(&h, "idler", false, b"\x1b\x1d").await;
    eventually("the viewer dismissed the overlay", TIMEOUT, || {
        (read(".fixture-dismiss") == "\x1b").then_some(())
    })
    .await;
    eventually(
        "resumed without a seed turn and received the backlog",
        TIMEOUT,
        || {
            read(".fixture-prompts")
                .contains("backlog while paused")
                .then_some(())
        },
    )
    .await;
    h.stop().await;
}

/// With no Codex app-server running, spawning a hosted Codex says that,
/// and how to start one, instead of a raw socket error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_spawn_without_an_app_server_says_the_daemon_is_not_running() {
    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[codex]\nlisten = \"unix://{}\"\n",
            h.dir.path().join("codex.sock").display()
        ),
    )
    .unwrap();
    h.config.committees.push(committee());
    let project = h.workdir("codex-down", None);
    start(&mut h).await;
    let request = Request::Spawn {
        attempt_id: None,
        session: Default::default(),
        runtime: Some(HostedRuntime::Codex),
        profile: None,
        committee: committee(),
        alias: None,
        cwd: project,
        chair: false,
        secretary: false,
        prompt: None,
        model: Default::default(),
    };
    let error = ipc::request::<SpawnReply>(&h.socket(), &request, TIMEOUT, TIMEOUT)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("no Codex app-server is listening")
            && error.contains("codex app-server daemon bootstrap"),
        "{error}"
    );
    assert!(!error.contains("WebSocket"), "{error}");
    assert!(h.store().all_sessions().unwrap().is_empty());
    h.stop().await;
}

/// `senate session replace`: a fresh hosted session takes the seat. Once it
/// is connected it gets the alias, the duty, the old seat's place on the
/// desk, the chair and its open question (handed over in the question's own
/// thread), then the starting prompt; the old session is stopped and
/// unseated. A failure partway (the desk) stops at that step and the same
/// command resumes there without starting a second session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_hands_the_seat_to_a_fresh_session_and_resumes_after_a_failure() {
    use senate_core::{Address, Kind, meta};
    let (h, project, _typed) = fixture_daemon(3).await;
    let old: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: committee(),
            alias: Some(path("reviewer")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    let (ok, out) = senate(
        &h,
        &[
            "senator",
            "duty",
            "reviews the gateway",
            "--session",
            &old.session_id,
        ],
    )
    .await;
    assert!(ok, "{out}");
    // The daemon learns the duty from its stream.
    let duty = h
        .server
        .events(committee().as_str())
        .into_iter()
        .rev()
        .find(|e| senate_core::DutyPayload::of(e).is_some())
        .unwrap();
    {
        let mut inner = h.server.state.inner.lock().unwrap();
        inner.chair = Some((old.senator.clone(), true));
        inner
            .secretaries
            .insert(path("alice"), vec![path("codex"), old.senator.clone()]);
        // The desk refuses once: the first run stops there.
        inner.refuse_secretaries = 1;
    }
    let open = h.server.append(common::question(
        common::person("bob"),
        Address::senator(&old.senator),
        "Which retry limit did you settle on?",
    ));
    // Wait until the daemon has taken in what came before the replace.
    eventually(
        "the daemon to know the duty and the question",
        TIMEOUT,
        || {
            (h.store()
                .delivered_cursor(&old.senator, &committee())
                .ok()?
                >= open.seq.max(duty.seq))
            .then_some(())
        },
    )
    .await;

    let (ok, out) = senate(
        &h,
        &[
            "session",
            "replace",
            "reviewer",
            "--prompt",
            "Pick up the gateway review",
        ],
    )
    .await;
    assert!(!ok, "{out}");
    assert!(out.contains("stopped at the desk step"), "{out}");
    assert!(out.contains("Done: spawn, alias, duty"), "{out}");
    assert!(out.contains("Run the same command again"), "{out}");

    let (ok, out) = senate(&h, &["session", "replace", "reviewer"]).await;
    assert!(ok, "{out}");
    assert!(out.contains("(done earlier)"), "{out}");
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    // One new session, not two: the second run resumed.
    assert_eq!(rows.rows.len(), 2, "{rows:?}");
    let new = rows
        .rows
        .iter()
        .find(|r| r.primary != old.senator)
        .unwrap()
        .primary
        .clone();
    assert!(
        out.contains(" replaced ") && out.contains("on `refund-v2`"),
        "{out}"
    );
    for step in [
        "spawn", "alias", "duty", "desk", "chair", "handover", "prompt", "stop", "unseat",
    ] {
        assert!(out.contains(&format!("\n  {step} ")), "{step} in {out}");
    }
    let old_row = rows.rows.iter().find(|r| r.primary == old.senator).unwrap();
    assert_ne!(old_row.state, ipc::HostState::Running, "{rows:?}");

    let events = h.server.events(committee().as_str());
    {
        let inner = h.server.state.inner.lock().unwrap();
        assert_eq!(
            inner.aliases.alias_of(&committee(), &new),
            Some(&path("reviewer"))
        );
        assert_eq!(inner.aliases.alias_of(&committee(), &old.senator), None);
        assert_eq!(inner.chair.as_ref().map(|c| &c.0), Some(&new));
        assert_eq!(
            inner.secretaries.get(&path("alice")).unwrap(),
            &[path("codex"), new.clone()],
            "the same place on the desk"
        );
        assert_eq!(inner.unseated.as_ref(), Some(&old.senator));
    }
    let duties: Vec<_> = events
        .iter()
        .filter_map(senate_core::DutyPayload::of)
        .filter(|d| d.senator == new)
        .collect();
    assert_eq!(duties.len(), 1, "one duty note, though the step ran once");
    assert_eq!(duties[0].duty, "reviews the gateway");
    let handed: Vec<_> = events
        .iter()
        .filter(|e| e.thread == Some(open.seq) && e.meta.contains_key(meta::HANDED_TO))
        .collect();
    assert_eq!(handed.len(), 1, "{handed:?}");
    assert_eq!(handed[0].to, [Address::senator(&new)]);
    assert_eq!(handed[0].kind, Kind::Question);
    assert!(
        handed[0].body.contains("Which retry limit"),
        "{}",
        handed[0].body
    );
    let prompts: Vec<_> = events
        .iter()
        .filter(|e| e.kind == Kind::Request && e.body == "Pick up the gateway review")
        .collect();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].to, [Address::senator(&new)]);
    // The handover and the prompt come after the alias moved, so nothing
    // for the seat reached a session that had not started.
    assert!(handed[0].seq < prompts[0].seq);
    assert!(h.store().replacements().unwrap().is_empty());
}

/// Starting the TUI succeeds, but the new senator never gets its SSE stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_stops_the_unconnected_session_and_preserves_the_old_seat() {
    let (h, project, _) = fixture_daemon(2).await;
    let old: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: committee(),
            alias: Some(path("reviewer")),
            cwd: project,
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    let (ok, out) = senate(
        &h,
        &[
            "senator",
            "duty",
            "reviews the gateway",
            "--session",
            &old.session_id,
        ],
    )
    .await;
    assert!(ok, "{out}");
    let duty = h
        .server
        .events(committee().as_str())
        .into_iter()
        .find(|e| senate_core::DutyPayload::of(e).is_some())
        .unwrap();
    let open = h.server.append(common::question(
        common::person("bob"),
        senate_core::Address::senator(&old.senator),
        "What remains to review?",
    ));
    eventually("old seat received its duty and question", TIMEOUT, || {
        (h.store()
            .delivered_cursor(&old.senator, &committee())
            .ok()?
            >= open.seq.max(duty.seq))
        .then_some(())
    })
    .await;
    let desk = vec![path("codex"), old.senator.clone()];
    {
        let mut inner = h.server.state.inner.lock().unwrap();
        inner.secretaries.insert(path("alice"), desk.clone());
        inner.chair = Some((old.senator.clone(), true));
        inner.only_stream = Some(old.senator.to_string());
    }
    let (ok, out) = senate(
        &h,
        &[
            "session",
            "replace",
            "reviewer",
            "--prompt",
            "Continue the review",
        ],
    )
    .await;
    assert!(!ok, "{out}");
    assert!(
        out.contains("started but was not seated and connected"),
        "{out}"
    );
    assert!(out.contains("stopped at the spawn step"), "{out}");

    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(rows.rows.len(), 2, "{rows:?}");
    let new = rows.rows.iter().find(|r| r.primary != old.senator).unwrap();
    assert_eq!(new.state, ipc::HostState::Stopped);
    assert!(!new.tty.exists());
    let kept = rows.rows.iter().find(|r| r.primary == old.senator).unwrap();
    assert_eq!(kept.state, ipc::HostState::Running);
    assert!(kept.tty.exists());
    assert!(h.store().replacements().unwrap().is_empty());
    let events = h.server.events(committee().as_str());
    assert_eq!(
        events
            .iter()
            .filter_map(senate_core::DutyPayload::of)
            .collect::<Vec<_>>(),
        [senate_core::DutyPayload::of(&duty).unwrap()]
    );
    assert!(
        !events
            .iter()
            .any(|e| e.meta.contains_key(senate_core::meta::HANDED_TO)
                || e.body == "Continue the review")
    );
    let inner = h.server.state.inner.lock().unwrap();
    assert!(
        inner
            .stream_opens
            .iter()
            .any(|s| s.senator.as_deref() == Some(new.primary.as_str()) && !s.accepted)
    );
    assert_eq!(
        inner.aliases.alias_of(&committee(), &old.senator),
        Some(&path("reviewer"))
    );
    assert_eq!(inner.chair.as_ref().map(|c| &c.0), Some(&old.senator));
    assert_eq!(inner.secretaries[&path("alice")], desk);
    assert!(inner.unseated.is_none());
}

/// A name that is no hosted session here is refused, and so is a
/// replacement whose new session cannot start (here: the machine is at its
/// limit of hosted sessions): the spawn step fails, nothing is handed over,
/// the old seat keeps its alias and terminal, and nothing is left under way.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replace_refuses_an_unknown_seat_and_hands_nothing_over_when_the_spawn_fails() {
    let (h, project, _typed) = fixture_daemon(1).await;
    let (ok, out) = senate(&h, &["session", "replace", "nobody-here"]).await;
    assert!(!ok, "{out}");
    assert!(out.contains("unknown alias nobody-here"), "{out}");

    let old: SpawnReply = call(
        &h,
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: committee(),
            alias: Some(path("reviewer")),
            cwd: project,
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
    )
    .await;
    let (ok, out) = senate(&h, &["session", "replace", "reviewer"]).await;
    assert!(!ok, "{out}");
    assert!(out.contains("stopped at the spawn step"), "{out}");
    assert!(out.contains("Nothing was handed over"), "{out}");
    assert_eq!(
        h.server
            .state
            .inner
            .lock()
            .unwrap()
            .aliases
            .alias_of(&committee(), &old.senator),
        Some(&path("reviewer"))
    );
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(rows.rows.len(), 1, "{rows:?}");
    assert_eq!(rows.rows[0].state, ipc::HostState::Running);
    assert!(h.store().replacements().unwrap().is_empty());
}

// ---- pause and resume -------------------------------------------------------

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `session pause` asks the session for a checkpoint with a Senate note,
/// waits for it and for the session to go quiet, stops the terminal and
/// records it paused: its presence goes absent and paused, and mail for it
/// waits. `session resume` restores it into the same conversation, points
/// it at that checkpoint, and the held mail reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_waits_for_the_checkpoint_and_resume_points_at_it() {
    use senate_bridge::ipc::{HookEvent, HookReply, PauseReply, ResumeReply, SessionSelector};
    use senate_core::{Address, Kind, NewEvent};

    let fixture = HostedClaude::launch(false).await;
    let h = &fixture.h;
    git(&fixture.project, &["init", "-q", "-b", "main"]);
    std::fs::write(fixture.project.join("README.md"), "hi\n").unwrap();
    git(&fixture.project, &["add", "README.md"]);
    git(&fixture.project, &["commit", "-q", "-m", "first"]);
    let native = fixture.seat.session_id.clone();
    let senator = fixture.seat.senator.clone();
    let report = |pid: u32, event, source, inbox: &common::FakeInbox| {
        let mut hook = fixture.hook(&native, event, source, Some(pid), &[pid]);
        let Request::SessionHook(report) = &mut hook else {
            unreachable!()
        };
        report.socket = Some(inbox.path.clone());
        hook
    };
    let first = common::FakeInbox::bind(h.dir.path().join("first-inbox.sock"));
    let _: HookReply = call(
        h,
        &report(fixture.pid(), HookEvent::UserPromptSubmit, None, &first),
    )
    .await;

    let started = std::time::Instant::now();
    let pausing = tokio::spawn({
        let socket = h.socket();
        let senator = senator.clone();
        async move {
            ipc::request::<PauseReply>(
                &socket,
                &Request::Pause {
                    senator: senator.into(),
                    force: false,
                    timeout_secs: Some(20),
                },
                TIMEOUT,
                Duration::from_secs(40),
            )
            .await
        }
    });
    // The note asking for a checkpoint reaches the session.
    eventually("the pause note pushed", TIMEOUT, || {
        first
            .messages()
            .into_iter()
            .find(|text| text.contains("about to be paused") && text.contains("checkpoint"))
    })
    .await;
    assert!(!pausing.is_finished(), "it waits for the checkpoint");
    let checkpoint: senate_bridge::ipc::CheckpointReply = call(
        h,
        &Request::Checkpoint {
            session: SessionSelector {
                session_id: Some(native.clone()),
                claude_pid: None,
            },
            summary: Some("paused mid-review".into()),
            progress: Some("- reviewing the gateway\n".into()),
            decisions: None,
            open: None,
            push: false,
            id: None,
        },
    )
    .await;
    let paused = pausing.await.unwrap().unwrap();
    assert_eq!(paused.checkpoint, Some(checkpoint.posted.seq));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "not the whole timeout: {:?}",
        started.elapsed()
    );

    // Stopped, and recorded paused.
    let reply: ipc::SessionsReply = call(h, &Request::Sessions).await;
    let row = reply.rows.iter().find(|r| r.primary == senator).unwrap();
    assert_eq!(row.state, ipc::HostState::Stopped);
    assert!(row.paused, "{row:?}");
    for args in [&["session", "list"][..], &["session", "list", "--all"]] {
        let (ok, listed) = senate(h, args).await;
        assert!(ok && listed.contains("paused"), "{listed}");
    }
    let events = h.server.events("refund-v2");
    assert!(
        events
            .iter()
            .any(|e| e.kind == Kind::Note && e.body.contains("paused the hosted terminal")),
        "the audit note says paused: {events:#?}"
    );
    eventually("the leave says paused", TIMEOUT, || {
        h.server
            .heartbeats_of("refund-v2", senator.as_str())
            .into_iter()
            .find(|beat| beat.left && beat.paused)
    })
    .await;
    std::fs::remove_file(&first.path).unwrap();
    drop(first);

    // Mail for it waits.
    let body = "a note for the paused senator";
    h.server.append(
        NewEvent::new(
            committee(),
            Kind::Note,
            Address::senior(&h.config.person),
            body,
        )
        .to(Address::senator(&senator)),
    );

    // Resume restores it in the same conversation and points it at the
    // checkpoint.
    let resumed: ResumeReply = ipc::request(
        &h.socket(),
        &Request::Resume {
            senator: path("seat-fixture").into(),
        },
        TIMEOUT,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    assert_eq!(resumed.senator, senator);
    assert_eq!(resumed.checkpoint, Some(checkpoint.posted.seq));
    let note = resumed.note.expect("the resumed note was posted");
    let note = h
        .server
        .events("refund-v2")
        .into_iter()
        .find(|e| e.seq == note)
        .unwrap();
    assert!(
        note.body.contains("resumed")
            && note
                .body
                .contains(&format!("seq {}", checkpoint.posted.seq)),
        "{}",
        note.body
    );
    assert_eq!(note.to, vec![Address::senator(&senator)]);
    let reply: ipc::SessionsReply = call(h, &Request::Sessions).await;
    let row = reply.rows.iter().find(|r| r.primary == senator).unwrap();
    assert_eq!(row.state, ipc::HostState::Running);
    assert!(!row.paused);
    // Resuming again: it is not paused.
    let again = ipc::request::<ResumeReply>(
        &h.socket(),
        &Request::Resume {
            senator: path("seat-fixture").into(),
        },
        TIMEOUT,
        TIMEOUT,
    )
    .await
    .unwrap_err();
    assert!(again.to_string().contains("not paused"), "{again}");

    // The held mail and the resumed note reach the restored terminal.
    let terminal: PathBuf = call(h, &HostedClaude::attach_request()).await;
    let pid: u32 = std::fs::read_to_string(terminal.with_file_name("owner"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let inbox = common::FakeInbox::bind(h.dir.path().join("resumed-inbox.sock"));
    let _: HookReply = call(
        h,
        &report(pid, HookEvent::SessionStart, Some("resume"), &inbox),
    )
    .await;
    eventually("the held note and the resumed note pushed", TIMEOUT, || {
        let all = inbox.messages().join("\n");
        (all.contains(body) && all.contains("resumed after a pause")).then_some(())
    })
    .await;
    rustix::process::kill_process_group(
        rustix::process::Pid::from_raw(pid as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    drop(fixture);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_claude_and_agy_remain_on_the_hosted_roster() {
    let (mut h, project, _) = fixture_daemon(3).await;
    let agy = agy_fixture(h.dir.path(), "3f2b8c1e-6d4a-4e7b-9c0d-1a2b3c4d5e6f");
    let config = h.paths.config();
    let claude_config = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{claude_config}\n[agy]\nbin = {agy:?}\n")).unwrap();
    for (runtime, alias) in [
        (HostedRuntime::Claude, "dev"),
        (HostedRuntime::Agy, "docs"),
        (HostedRuntime::Claude, "ended"),
    ] {
        let _: SpawnReply = call(
            &h,
            &Request::Spawn {
                attempt_id: None,
                session: Default::default(),
                runtime: Some(runtime),
                profile: None,
                committee: committee(),
                alias: Some(path(alias)),
                cwd: project.clone(),
                chair: false,
                secretary: false,
                prompt: None,
                model: Default::default(),
            },
        )
        .await;
    }
    let (ok, out) = senate(&h, &["session", "stop", "ended", "--force"]).await;
    assert!(ok, "{out}");
    let (ok, out) = senate(
        &h,
        &[
            "session",
            "pause",
            "refund-v2::dev",
            "refund-v2::docs",
            "--timeout",
            "1",
        ],
    )
    .await;
    assert!(ok, "{out}");

    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    let mut paused_ids = Vec::new();
    for (alias, runtime) in [
        ("dev", senate_bridge::session::Runtime::Claude),
        ("docs", senate_bridge::session::Runtime::Agy),
    ] {
        let row = reply
            .rows
            .iter()
            .find(|r| r.alias.as_ref() == Some(&path(alias)))
            .unwrap();
        assert_eq!(row.runtime, runtime);
        assert_eq!(row.state, ipc::HostState::Stopped);
        assert!(row.paused, "{row:?}");
        paused_ids.push(row.session_id.clone());
    }
    for all in [true, false] {
        let mut args = vec!["session", "list"];
        if all {
            args.push("--all");
        }
        let (ok, listed) = senate(&h, &args).await;
        assert!(ok, "{listed}");
        for alias in ["dev", "docs"] {
            assert!(
                listed
                    .lines()
                    .any(|line| line.split_whitespace().next() == Some(alias)
                        && line.split_whitespace().any(|word| word == "paused")),
                "{alias} missing or not paused in {args:?}: {listed}"
            );
        }
        assert_eq!(
            listed
                .lines()
                .any(|line| line.split_whitespace().next() == Some("ended")),
            all,
            "{listed}"
        );
    }
    let roster: Vec<ipc::RosterCommittee> = call(&h, &Request::Roster).await;
    let row = roster
        .iter()
        .find(|row| row.committee == committee())
        .unwrap();
    let active: Vec<_> = row
        .members
        .iter()
        .filter(|m| m.active)
        .map(|m| m.display_name.as_str())
        .collect();
    assert_eq!(active.len(), 2, "{roster:?}");
    assert!(
        active.contains(&"dev") && active.contains(&"docs"),
        "{roster:?}"
    );

    // Cleanup can remove the stopped row but must keep both resumable sessions.
    let store = h.store();
    assert_eq!(store.delete_sessions(&paused_ids, &h.paths).unwrap(), 0);
    store.prune(Duration::ZERO, &h.paths).unwrap();
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(reply.rows.len(), 2, "{reply:?}");
    assert!(reply.rows.iter().all(|row| row.paused), "{reply:?}");

    let original = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("locale = \"zh\"\n{original}")).unwrap();
    let (ok, listed) = senate(&h, &["session", "list"]).await;
    assert!(ok, "{listed}");
    assert_eq!(listed.matches("已暂停").count(), 2, "{listed}");

    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    // Paused sessions have no terminal to watch: the grid shows them only
    // with `--all`.
    cmd.args(["session", "watch", "--all"])
        .env("SENATE_HOME", h.paths.home())
        .kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 120)).unwrap();
    let mut child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (mut output, mut input) = pty.into_split();
    wait_for_screen(&mut output, 24, 120, |screen| {
        screen.contains("refund-v2::dev · claude")
            && screen.contains("refund-v2::docs · agy")
            && screen.matches("已暂停").count() == 2
    })
    .await;
    input.write_all(b"q").await.unwrap();
    assert!(
        tokio::time::timeout(TIMEOUT, child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    h.stop().await;
}

/// A session typed into lately is skipped unless `--force`; `--dry-run`
/// pauses nothing; a pause with no checkpoint in time still records it
/// paused; and `session resume` brings back only what is paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pause_cli_spares_recent_typing_dry_runs_and_resumes() {
    let (h, _typed) = watched_sessions(&["main"]).await;
    let label = "refund-v2::main";
    attach(&h, label, b"hello\r\x1d").await;
    eventually("the typing is seen", TIMEOUT, || {
        let reply = futures::executor::block_on(ipc::request::<ipc::SessionsReply>(
            &h.socket(),
            &Request::Sessions,
            TIMEOUT,
            TIMEOUT,
        ))
        .ok()?;
        reply.rows[0].typed_secs_ago.map(|_| ())
    })
    .await;

    let (ok, said) = senate(&h, &["session", "pause", "--all", "--dry-run"]).await;
    assert!(ok, "{said}");
    assert!(said.contains(&format!("skipped {label}")), "{said}");
    assert!(!said.contains("would pause"), "{said}");
    let (ok, said) = senate(&h, &["session", "pause", label]).await;
    assert!(!ok && said.contains("someone typed into it"), "{said}");
    let (ok, said) = senate(&h, &["session", "pause", "--all", "--force", "--dry-run"]).await;
    assert!(
        ok && said.contains(&format!("would pause {label}")),
        "{said}"
    );
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(reply.rows[0].state, ipc::HostState::Running, "dry run");

    let (ok, said) = senate(
        &h,
        &[
            "session",
            "pause",
            "--committee",
            "refund-v2",
            "--force",
            "--timeout",
            "1",
        ],
    )
    .await;
    assert!(ok, "{said}");
    assert!(
        said.contains(&format!("paused {label} (no new checkpoint")),
        "{said}"
    );
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert!(reply.rows[0].paused, "{reply:?}");
    assert_eq!(reply.rows[0].state, ipc::HostState::Stopped);

    let (ok, said) = senate(&h, &["session", "resume", "--all", "--dry-run"]).await;
    assert!(
        ok && said.contains(&format!("would resume {label}")),
        "{said}"
    );
    let (ok, said) = senate(&h, &["session", "resume", "main"]).await;
    assert!(ok, "{said}");
    assert!(said.contains(&format!("resumed {label}")), "{said}");
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(reply.rows[0].state, ipc::HostState::Running);
    assert!(!reply.rows[0].paused);

    // Stopped on purpose, it is not paused, and resume leaves it.
    let (ok, said) = senate(&h, &["session", "stop", label, "--force"]).await;
    assert!(ok, "{said}");
    let (ok, said) = senate(&h, &["session", "resume", "main"]).await;
    assert!(
        !ok && said.contains("no paused hosted session named main"),
        "{said}"
    );
}

/// With `pause_on_shutdown`, the daemon's SIGTERM step pauses every
/// running session within its cap: one that never checkpoints is recorded
/// paused all the same. With `resume_on_start`, the next daemon resumes
/// what is recorded paused, and never a session stopped on purpose.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_pauses_within_its_cap_and_start_resumes_only_the_paused() {
    let (mut h, project, _typed) = fixture_daemon(2).await;
    for alias in ["kept", "ended"] {
        let _: SpawnReply = call(
            &h,
            &Request::Spawn {
                attempt_id: None,
                session: Default::default(),
                runtime: Some(HostedRuntime::Claude),
                profile: None,
                committee: committee(),
                alias: Some(path(alias)),
                cwd: project.clone(),
                chair: false,
                secretary: false,
                prompt: None,
                model: Default::default(),
            },
        )
        .await;
    }
    let (ok, said) = senate(&h, &["session", "stop", "refund-v2::ended"]).await;
    assert!(ok, "{said}");

    // Off: SIGTERM's step does nothing.
    h.daemon.as_ref().unwrap().pause_for_shutdown().await;
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert!(reply.rows.iter().all(|r| !r.paused), "{reply:?}");

    // On, with a cap of 2 s: the session never checkpoints.
    h.stop().await;
    h.config.pause_on_shutdown = true;
    let mut timing = fast_timing();
    timing.shutdown_pause = Duration::from_secs(2);
    h.daemon = Some(
        start_with_host_binary(
            DaemonOptions {
                paths: h.paths.clone(),
                config: h.config.clone(),
                timing,
                desktop: None,
            },
            BIN.into(),
        )
        .await
        .unwrap(),
    );
    let began = std::time::Instant::now();
    h.daemon.as_ref().unwrap().pause_for_shutdown().await;
    assert!(
        began.elapsed() < Duration::from_secs(6),
        "within the cap: {:?}",
        began.elapsed()
    );
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    let row = |reply: &ipc::SessionsReply, label: &str| {
        reply
            .rows
            .iter()
            .find(|r| r.label() == label)
            .cloned()
            .unwrap()
    };
    let kept = row(&reply, "refund-v2::kept");
    assert!(kept.paused, "{kept:?}");
    assert_eq!(kept.state, ipc::HostState::Stopped);
    assert!(
        !row(&reply, "refund-v2::ended").paused,
        "stopped on purpose"
    );
    h.stop().await;

    // The next start resumes the paused one only.
    h.config.resume_on_start = true;
    start(&mut h).await;
    eventually(
        "the paused session resumed",
        Duration::from_secs(15),
        || {
            let reply = futures::executor::block_on(ipc::request::<ipc::SessionsReply>(
                &h.socket(),
                &Request::Sessions,
                TIMEOUT,
                TIMEOUT,
            ))
            .ok()?;
            (row(&reply, "refund-v2::kept").state == ipc::HostState::Running).then_some(reply)
        },
    )
    .await;
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert!(!row(&reply, "refund-v2::kept").paused);
    assert_eq!(
        row(&reply, "refund-v2::ended").state,
        ipc::HostState::Stopped,
        "never resumed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_taken_alias_is_refused_before_anything_starts() {
    let (mut h, project) = watched_sessions(&["rev"]).await;
    let spawn = |attempt_id: Option<String>| Request::Spawn {
        attempt_id,
        session: Default::default(),
        runtime: Some(HostedRuntime::Claude),
        profile: None,
        committee: committee(),
        alias: Some(path("rev")),
        cwd: project.clone(),
        chair: false,
        secretary: false,
        prompt: None,
        model: Default::default(),
    };
    // Untracked and tracked (as the CLI sends it) alike: a plain refusal,
    // never an unknown outcome to retry.
    for attempt in [None, Some(uuid::Uuid::now_v7().to_string())] {
        let error = ipc::request::<SpawnReply>(&h.socket(), &spawn(attempt), TIMEOUT, TIMEOUT)
            .await
            .unwrap_err();
        let ipc::ClientError::Remote(error) = error else {
            panic!("expected a daemon error, got {error}");
        };
        assert_eq!(error.code, ipc::codes::BAD_REQUEST, "{}", error.message);
        assert!(
            error.message.contains("rev is already the alias of")
                && error.message.contains("`senate attach rev`")
                && !error.message.contains("--attempt-id"),
            "{}",
            error.message
        );
    }
    let reply: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    assert_eq!(reply.rows.len(), 1, "nothing else started");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restoring_a_running_session_says_how_to_open_it() {
    let (mut h, _) = watched_sessions(&["rev"]).await;
    let error = ipc::request::<SpawnReply>(
        &h.socket(),
        &Request::Restore {
            session: Default::default(),
            senator: "rev".parse().unwrap(),
            model: Default::default(),
        },
        TIMEOUT,
        TIMEOUT,
    )
    .await
    .unwrap_err();
    let ipc::ClientError::Remote(error) = error else {
        panic!("expected a daemon error, got {error}");
    };
    assert!(
        error
            .message
            .contains("rev already has a running terminal; open it with `senate attach rev`."),
        "{}",
        error.message
    );
    h.stop().await;
}

/// `senate spawn --dir`: an explicit `--committee` wins; else the committee
/// bound at `--dir`; else this directory's, said on stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_seats_on_the_committee_bound_at_its_dir() {
    let (mut h, here, _) = fixture_daemon(3).await;
    std::fs::write(here.join(".senate/committee"), "refund-v2\n").unwrap();
    let bound = h.workdir("bound", Some("other"));
    let unbound = h.workdir("unbound", None);
    let spawn = |dir: &std::path::Path, extra: &[&str]| {
        let mut cmd = std::process::Command::new(BIN);
        testenv::isolate(&mut cmd, h.dir.path());
        let out = cmd
            .env("SENATE_HOME", h.paths.home())
            .current_dir(&here)
            .args(["spawn", "claude", "--dir"])
            .arg(dir)
            .args(extra)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        (
            String::from_utf8(out.stdout).unwrap().trim().to_owned(),
            String::from_utf8(out.stderr).unwrap(),
        )
    };
    let (at_dir, quiet) = spawn(&bound, &[]);
    assert!(quiet.is_empty(), "{quiet}");
    let (explicit, _) = spawn(&bound, &["--committee", "refund-v2"]);
    let (fallback, said) = spawn(&unbound, &[]);
    assert!(
        said.contains("is bound to no committee; seated on refund-v2, this directory's"),
        "{said}"
    );
    // Each committee numbers its own: the first on each is claude1, the
    // second named with its committee once two share the alias.
    assert_eq!(
        (at_dir.as_str(), explicit.as_str(), fallback.as_str()),
        ("claude1", "refund-v2::claude1", "claude2")
    );
    let rows: ipc::SessionsReply = call(&h, &Request::Sessions).await;
    let mut labels: Vec<String> = rows.rows.iter().map(ipc::SessionRow::label).collect();
    labels.sort();
    assert_eq!(
        labels,
        ["other::claude1", "refund-v2::claude1", "refund-v2::claude2"]
    );
    h.stop().await;
}
