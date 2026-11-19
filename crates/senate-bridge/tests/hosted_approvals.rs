//! A real terminal host with isolated runtime and committee fixtures.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::{Harness, RECORD_HOSTED_ARGV, WAIT, committee, eventually, fast_timing, hosted_fixture_argv, path};
use rustix::process::{Pid, Signal, kill_process_group};
use senate_bridge::approvals::Decision;
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::hook::HookKind;
use senate_bridge::ipc::{self, ApprovalsReply, ApproveReply, HostedRuntime, Request, SpawnReply};
use senate_bridge::{Paths, testenv};
use senate_core::{Address, Event, Locale, Seq, StatusPayload, StatusState};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

const BIN: &str = env!("CARGO_BIN_EXE_senate");

async fn fixture(chair_connected: bool, locale: Locale) -> (Harness, PathBuf) {
    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    h.config.locale = locale;
    h.config.committees.push(committee());
    h.server.state.inner.lock().unwrap().chair = Some((path("chair-test"), chair_connected));
    let program = h.dir.path().join("tui");
    std::fs::write(
        &program,
        format!("#!/bin/sh\n{RECORD_HOSTED_ARGV}\nwhile IFS= read -r line; do :; done\n"),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        h.paths.config(),
        format!(
            "[companion]\nclaude_bin = {:?}\n[codex]\nbin = {:?}\nlisten = {:?}\n",
            program,
            program,
            format!("unix://{}", h.dir.path().join("codex.sock").display()),
        ),
    )
    .unwrap();
    let project = h.workdir("approval", None);
    (h, project)
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

struct Terminal {
    socket: PathBuf,
    group: Pid,
}

impl Terminal {
    async fn spawn(h: &Harness, cwd: PathBuf, runtime: HostedRuntime) -> (SpawnReply, Self) {
        let launch: SpawnReply = ipc::request(
            &h.socket(),
            &Request::Spawn {
                session: Default::default(),
                runtime,
                committee: committee(),
                alias: None,
                cwd,
                chair: false,
            },
            WAIT,
            WAIT,
        )
        .await
        .unwrap();
        let terminal = Self::from_launch(h, &launch).await;
        (launch, terminal)
    }

    async fn from_launch(h: &Harness, launch: &SpawnReply) -> Self {
        let socket: PathBuf = ipc::request(
            &h.socket(),
            &Request::Attach {
                senator: launch.senator.clone(),
            },
            WAIT,
            WAIT,
        )
        .await
        .unwrap();
        let pid = std::fs::read_to_string(socket.parent().unwrap().join("owner")).unwrap();
        Self {
            socket,
            group: Pid::from_raw(pid.trim().parse().unwrap()).unwrap(),
        }
    }

    fn stop(&self) {
        if self.socket.exists() {
            let _ = kill_process_group(self.group, Signal::TERM);
        }
    }

    async fn viewer(&self) -> UnixStream {
        tokio::time::timeout(WAIT, async {
            loop {
                let mut viewer = UnixStream::connect(&self.socket).await.unwrap();
                if viewer.read_u8().await.unwrap() == 1 {
                    return viewer;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    fn host_id(&self) -> &str {
        self.socket
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
    }

    fn activity(&self) -> Option<Value> {
        let data = std::fs::read(self.socket.with_file_name("viewer.json")).ok()?;
        serde_json::from_slice(&data).ok()
    }

    async fn wait_detached(&self, previous: u64) -> Value {
        eventually("the viewer finished detaching", WAIT, || {
            let value = self.activity()?;
            (value["attached"] == false && value["detached_at"].as_u64()? > previous)
                .then_some(value)
        })
        .await
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop();
    }
}

fn notices(h: &Harness) -> Vec<Event> {
    let people = h.server.state.inner.lock().unwrap().roster.people();
    h.server
        .events("refund-v2")
        .into_iter()
        .filter(|event| event.is_hosted_approval(&people))
        .collect()
}

async fn next_notice(h: &Harness, count: usize) -> Event {
    eventually("only the expected hosted approval notices", WAIT, || {
        let observed = notices(h);
        assert!(observed.len() <= count, "{observed:?}");
        (observed.len() == count).then(|| observed[count - 1].clone())
    })
    .await
}

fn archived(h: &Harness, seq: Seq) -> Option<Event> {
    h.server.events("refund-v2").into_iter().find(|e| {
        e.thread == Some(seq)
            && StatusPayload::of(e).is_some_and(|status| status.state == StatusState::Archived)
    })
}

async fn wait_archived(h: &Harness, seq: Seq) {
    eventually("the approval thread archived", WAIT, || archived(h, seq)).await;
}

async fn view_without_input(terminal: &Terminal) {
    let mut viewer = terminal.viewer().await;
    let before = terminal.activity().unwrap();
    viewer.write_all(&[1, 0, 32, 0, 81]).await.unwrap();
    let reply = b"\x1b[1;32R\x1b[?31u\x1b[I";
    viewer.write_u8(2).await.unwrap();
    viewer
        .write_u16(reply.len().try_into().unwrap())
        .await
        .unwrap();
    viewer.write_all(reply).await.unwrap();
    viewer.shutdown().await.unwrap();
    let after = terminal
        .wait_detached(before["detached_at"].as_u64().unwrap())
        .await;
    assert_eq!(
        before["input_at"], after["input_at"],
        "the host processed the emulator replies without finding human input"
    );
}

async fn send_input(terminal: &Terminal) {
    let mut viewer = terminal.viewer().await;
    let previous = terminal.activity().unwrap();
    viewer.write_all(&[2, 0, 2, b'x', b'\n']).await.unwrap();
    viewer.shutdown().await.unwrap();
    let after = terminal
        .wait_detached(previous["detached_at"].as_u64().unwrap())
        .await;
    assert!(after["input_at"].as_u64() > previous["input_at"].as_u64());
}

async fn claude_hook(root: &Path, h: &Harness, host: &str, input: Value) {
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, root);
    let mut hook = cmd
        .args(["hook", "hosted-approval"])
        .env("SENATE_HOME", h.paths.home())
        .env(senate_bridge::host::HOSTED_CLAUDE_ENV, host)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .await
        .unwrap();
    let output = tokio::time::timeout(WAIT, hook.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    assert!(
        output.stdout.is_empty(),
        "an observer hook must print no permission response"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_prompt_requires_a_notification_and_the_desk_covers_an_away_chair() {
    let (mut h, project) = fixture(false, Locale::Zh).await;
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project.clone(), HostedRuntime::Claude).await;
    let args = hosted_fixture_argv(&terminal.socket, &project, WAIT).await;
    assert_eq!(
        args,
        [
            format!("--session-id={}", launch.session_id).as_str(),
            "--settings",
            terminal
                .socket
                .with_file_name("approval-hooks.json")
                .to_str()
                .unwrap()
        ]
    );
    let overlay: Value = serde_json::from_slice(&std::fs::read(&args[2]).unwrap()).unwrap();
    assert_eq!(overlay.as_object().unwrap().len(), 1);
    assert_eq!(overlay["hooks"].as_object().unwrap().len(), 4);
    for event in [
        "PermissionRequest",
        "Notification",
        "PostToolUse",
        "PostToolUseFailure",
    ] {
        assert!(
            overlay["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook hosted-approval >/dev/null 2>&1 || :")
        );
    }
    let old_binary = h.dir.path().join("old-senate");
    std::fs::write(
        &old_binary,
        "#!/bin/sh\nprintf 'unknown subcommand\\n' >&2\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&old_binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    let command = overlay["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let (_, suffix) = command.split_once(" hook hosted-approval").unwrap();
    let result = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            &format!("{old_binary:?} hook hosted-approval{suffix}"),
        ])
        .output()
        .unwrap();
    assert!(result.status.success());
    assert!(result.stdout.is_empty() && result.stderr.is_empty());
    let signal = |event: &str, command: &str| {
        json!({
            "hook_event_name": event, "session_id": launch.session_id,
            "tool_name": "Bash", "tool_input": {"command": command},
        })
    };
    let notify = json!({
        "hook_event_name": "Notification", "session_id": launch.session_id,
        "notification_type": "permission_prompt",
    });
    let send = |input| claude_hook(h.dir.path(), &h, terminal.host_id(), input);
    let wrong_host = senate_core::EventId::new().to_string();
    claude_hook(
        h.dir.path(),
        &h,
        &wrong_host,
        signal("PermissionRequest", "wrong host"),
    )
    .await;
    claude_hook(h.dir.path(), &h, &wrong_host, notify.clone()).await;
    assert!(
        notices(&h).is_empty(),
        "the unverified claim did not arm a prompt"
    );

    let mut attended = terminal.viewer().await;
    let before = terminal.activity().unwrap();
    attended.write_all(&[2, 0, 1, b'x']).await.unwrap();
    eventually("the connected viewer forwarded human input", WAIT, || {
        let view = terminal.activity()?;
        (view["input_at"].as_u64()? > before["input_at"].as_u64()?).then_some(())
    })
    .await;
    send(signal("PermissionRequest", "echo already-attended")).await;
    send(notify.clone()).await;
    send(signal("PostToolUse", "echo already-attended")).await;
    attended.shutdown().await.unwrap();
    terminal
        .wait_detached(before["detached_at"].as_u64().unwrap())
        .await;

    send(signal("PermissionRequest", "echo approval-fixture")).await;
    assert!(
        notices(&h).is_empty(),
        "the hook alone is not a pending prompt"
    );
    send(notify.clone()).await;
    let first = next_notice(&h, 1).await;
    assert!(first.body.contains("工具或命令 \"echo approval-fixture\""));
    assert!(
        first
            .body
            .contains(&format!("senate attach {}", launch.senator))
    );
    assert_eq!(first.to, [Address::Speaker]);
    send(notify.clone()).await;
    send(signal("PostToolUse", "other tool input")).await;
    view_without_input(&terminal).await;
    assert!(archived(&h, first.seq).is_none());
    send(signal("PostToolUseFailure", "echo approval-fixture")).await;
    wait_archived(&h, first.seq).await;
    assert_eq!(notices(&h).len(), 1);

    send(signal("PermissionRequest", "echo next-fixture")).await;
    send(notify.clone()).await;
    let next = next_notice(&h, 2).await;
    // An input can deny the prompt without a Claude tool result. Accept it
    // while the daemon is down and recover the same open notice on restart.
    h.stop().await;
    send_input(&terminal).await;
    start(&mut h).await;
    wait_archived(&h, next.seq).await;
    claude_hook(h.dir.path(), &h, terminal.host_id(), notify).await;
    let generic = next_notice(&h, 3).await;
    assert!(!generic.body.contains("echo next-fixture"));
    assert!(!generic.body.contains("工具或命令"));
    h.server.reset_log(Some("new-hosted-approval-epoch"));
    let carried = next_notice(&h, 1).await;
    assert_ne!(generic.id, carried.id);
    assert_eq!(generic.body, carried.body);
    terminal.stop();
    wait_archived(&h, carried.seq).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_hosted_notices_close_on_input_and_recur_on_native_replay() {
    let (mut h, project) = fixture(true, Locale::En).await;
    let fake = h.codex();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Codex).await;
    let thread = &launch.session_id;
    eventually("the relay subscribed", WAIT, || {
        (fake.subscribers(thread) == 1).then_some(())
    })
    .await;
    let method = "item/commandExecution/requestApproval";
    let params = json!({"threadId": thread, "turnId":"turn", "itemId":"cmd", "command":"echo approval-fixture"});
    h.server.state.inner.lock().unwrap().lose_approval_ack = true;
    let native = fake.request_approval(thread, method, params.clone());
    let first = next_notice(&h, 1).await;
    eventually(
        "the same approval was retried after its lost acknowledgement",
        WAIT,
        || (h.server.state.inner.lock().unwrap().hosted_approval_appends >= 2).then_some(()),
    )
    .await;
    assert_eq!(notices(&h).len(), 1);
    assert!(
        first
            .body
            .contains("Tool or command \"run: echo approval-fixture\"")
    );
    assert_eq!(first.to, [Address::Chair]);
    assert!(fake.responses().is_empty());

    let error = ipc::request::<ApproveReply>(
        &h.socket(),
        &Request::Approve {
            id: format!("{thread}:{native}"),
            decision: Decision::Allow,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("senate attach"), "{error}");
    assert!(
        fake.responses().is_empty(),
        "the daemon sent a permission decision"
    );

    fake.replay_approval(thread, native, method, params.clone());
    view_without_input(&terminal).await;
    assert!(archived(&h, first.seq).is_none());
    let mut active_viewer = terminal.viewer().await;
    active_viewer.write_all(&[2, 0, 1, b'x']).await.unwrap();
    wait_archived(&h, first.seq).await;
    assert_eq!(notices(&h).len(), 1);
    assert!(
        fake.responses().is_empty(),
        "viewer input must never answer through the relay"
    );

    fake.disconnect_subscribers(thread);
    eventually("the relay reconnected", WAIT, || {
        (fake.subscribers(thread) == 1).then_some(())
    })
    .await;
    fake.replay_approval(thread, native, method, params);
    tokio::time::timeout(WAIT, async {
        loop {
            let pending: ApprovalsReply =
                ipc::request(&h.socket(), &Request::Approvals, WAIT, WAIT)
                    .await
                    .unwrap();
            if pending
                .approvals
                .iter()
                .any(|request| request.id == format!("{thread}:{native}"))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        notices(&h).len(),
        1,
        "a connected viewer leaves this approval attended"
    );
    let previous = terminal.activity().unwrap();
    active_viewer.shutdown().await.unwrap();
    terminal
        .wait_detached(previous["detached_at"].as_u64().unwrap())
        .await;
    let second = next_notice(&h, 2).await;
    assert_ne!(first.id, second.id);
    assert!(
        fake.answer_in_tui(native),
        "the request was still pending when the TUI handled it"
    );
    wait_archived(&h, second.seq).await;
    // No resolution notification reaches the relay while it is away.
    let third_id = fake.request_approval(
        thread,
        method,
        json!({"threadId": thread, "command": "echo disconnect-fixture"}),
    );
    let third = next_notice(&h, 3).await;
    fake.resolve_while_disconnected(thread, third_id);
    wait_archived(&h, third.seq).await;
    assert!(fake.responses().is_empty());
    terminal.stop();
    eventually("the fixture host exited", WAIT, || {
        (!terminal.socket.exists()).then_some(())
    })
    .await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_a_restored_codex_host_closes_its_notice_and_leaves_the_native_session_live() {
    let (mut h, project) = fixture(true, Locale::En).await;
    std::fs::write(project.join(".senate/committee"), committee().as_str()).unwrap();
    let fake = h.codex();
    let thread = "00000000-0000-4000-8000-000000000123";
    fake.idle(thread);
    start(&mut h).await;
    let start_json = json!({
        "session_id": thread, "cwd": project, "hook_event_name": "SessionStart", "source": "startup",
    }).to_string();
    senate_bridge::hook::run_as(
        HookKind::SessionStart,
        &h.socket(),
        &start_json,
        &Default::default(),
        Some(senate_bridge::session::Runtime::Codex),
        Default::default(),
    )
    .await
    .unwrap();
    eventually("the manual relay subscribed", WAIT, || {
        (fake.subscribers(thread) == 1).then_some(())
    })
    .await;
    fake.request_approval(
        thread, "item/commandExecution/requestApproval",
        json!({"threadId":thread, "turnId":"turn", "itemId":"command", "command":"echo held-before-restore"}),
    );
    tokio::time::timeout(WAIT, async {
        loop {
            let list: ApprovalsReply = ipc::request(&h.socket(), &Request::Approvals, WAIT, WAIT)
                .await
                .unwrap();
            if list.approvals.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(notices(&h).is_empty());
    let senator = h.store().session(thread).unwrap().unwrap().agent;
    h.server.state.inner.lock().unwrap().chair = Some((senator.clone(), true));
    let launch: SpawnReply = ipc::request(
        &h.socket(),
        &Request::Restore {
            session: Default::default(),
            senator,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let terminal = Terminal::from_launch(&h, &launch).await;
    let waiting = next_notice(&h, 1).await;
    assert!(waiting.body.contains("echo held-before-restore"));
    assert_eq!(
        waiting.to,
        [Address::Speaker],
        "the stalled chair cannot attend to itself"
    );
    let stopped: senate_bridge::ipc::StopReply = ipc::request(
        &h.socket(),
        &Request::Stop {
            session: Default::default(),
            senator: launch.senator,
            force: false,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    assert_eq!(stopped.session_id, thread);
    assert!(!terminal.socket.parent().unwrap().exists());
    assert!(h.store().session(thread).unwrap().unwrap().is_live());
    wait_archived(&h, waiting.seq).await;
    assert!(fake.responses().is_empty());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_archive_refusal_stops_retrying_without_blocking_another_notice() {
    let (mut h, project) = fixture(false, Locale::En).await;
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Claude).await;
    let pending = json!({
        "hook_event_name": "Notification", "session_id": launch.session_id,
        "notification_type": "permission_prompt",
    });
    claude_hook(h.dir.path(), &h, terminal.host_id(), pending.clone()).await;
    let stranded = next_notice(&h, 1).await;
    h.server.state.inner.lock().unwrap().reject_approval_archive = true;
    send_input(&terminal).await;
    eventually("the archive retry budget was exhausted", WAIT, || {
        (h.server
            .state
            .inner
            .lock()
            .unwrap()
            .approval_archive_attempts
            == 6)
            .then_some(())
    })
    .await;
    h.server.state.inner.lock().unwrap().reject_approval_archive = false;
    claude_hook(h.dir.path(), &h, terminal.host_id(), pending).await;
    let fresh = next_notice(&h, 2).await;
    assert!(archived(&h, stranded.seq).is_none());
    assert_eq!(
        h.server
            .state
            .inner
            .lock()
            .unwrap()
            .approval_archive_attempts,
        6
    );
    terminal.stop();
    wait_archived(&h, fresh.seq).await;
    assert!(archived(&h, stranded.seq).is_none());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_outages_do_not_exhaust_notice_or_archive_retries() {
    let (mut h, project) = fixture(false, Locale::En).await;
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Claude).await;
    let pending = json!({
        "hook_event_name": "Notification", "session_id": launch.session_id,
        "notification_type": "permission_prompt",
    });
    h.server.state.inner.lock().unwrap().transient_append_delay = Some(std::time::Duration::ZERO);
    claude_hook(h.dir.path(), &h, terminal.host_id(), pending).await;
    eventually("more than six transient notice failures", WAIT, || {
        (h.server
            .state
            .inner
            .lock()
            .unwrap()
            .transient_append_attempts
            >= 7)
            .then_some(())
    })
    .await;
    assert!(notices(&h).is_empty());
    h.server.state.inner.lock().unwrap().transient_append_delay = None;
    let notice = next_notice(&h, 1).await;

    let next_attempt = {
        let mut server = h.server.state.inner.lock().unwrap();
        server.transient_append_delay = Some(std::time::Duration::ZERO);
        server.transient_append_attempts + 7
    };
    send_input(&terminal).await;
    eventually("more than six transient archive failures", WAIT, || {
        (h.server
            .state
            .inner
            .lock()
            .unwrap()
            .transient_append_attempts
            >= next_attempt)
            .then_some(())
    })
    .await;
    assert!(archived(&h, notice.seq).is_none());
    h.server.state.inner.lock().unwrap().transient_append_delay = None;
    wait_archived(&h, notice.seq).await;
    assert_eq!(notices(&h).len(), 1);
    terminal.stop();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_attached_viewer_does_not_hide_a_prompt_and_escape_then_x_counts() {
    let (mut h, project) = fixture(false, Locale::En).await;
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Claude).await;
    let mut viewer = terminal.viewer().await;
    let idle = terminal.activity().unwrap();
    assert_eq!(idle["attached"], true);
    assert_eq!(idle["input_at"], 0);
    claude_hook(
        h.dir.path(),
        &h,
        terminal.host_id(),
        json!({
            "hook_event_name": "Notification", "session_id": launch.session_id,
            "notification_type": "permission_prompt",
        }),
    )
    .await;
    let notice = next_notice(&h, 1).await;
    assert_eq!(terminal.activity().unwrap()["attached"], true);

    // Escape denies the prompt. The following capital X is ordinary
    // typing, even though these bytes also start an SOS control string.
    viewer
        .write_all(b"\x02\0\x01\x1b\x02\0\x06Xreply")
        .await
        .unwrap();
    wait_archived(&h, notice.seq).await;
    assert_eq!(terminal.activity().unwrap()["attached"], true);
    terminal.stop();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_names_the_host_with_missing_viewer_activity() {
    let (mut h, project) = fixture(false, Locale::En).await;
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Claude).await;
    std::fs::remove_file(terminal.socket.with_file_name("viewer.json")).unwrap();
    let status = senate_bridge::status::collect(&h.paths).await.unwrap();
    assert!(
        status
            .hosted_viewer_unavailable
            .contains(&launch.session_id)
    );
    let rendered = senate_bridge::status::render(&status, chrono::Utc::now());
    assert!(rendered.contains(&format!(
        "Approval notices for hosted seat {} are unavailable",
        launch.senator
    )));
    terminal.stop();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verified_claude_clear_keeps_approval_notices_on_the_same_seat() {
    use senate_bridge::ipc::{HookReply, HookReport};

    let (mut h, project) = fixture(false, Locale::En).await;
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project.clone(), HostedRuntime::Claude).await;
    let request = |session: &str, command: &str| {
        json!({
            "session_id": session, "hook_event_name": "PermissionRequest",
            "tool_name": "Bash", "tool_input": {"command": command},
        })
    };
    let pending = |session: &str| {
        json!({
            "session_id": session, "hook_event_name": "Notification",
            "notification_type": "permission_prompt",
        })
    };
    claude_hook(
        h.dir.path(),
        &h,
        terminal.host_id(),
        request(&launch.session_id, "echo old-context"),
    )
    .await;
    claude_hook(
        h.dir.path(),
        &h,
        terminal.host_id(),
        pending(&launch.session_id),
    )
    .await;
    let original = next_notice(&h, 1).await;
    assert!(original.body.contains("echo old-context"));

    // The fixture TUI owns an actual host. Its verified SessionStart hook
    // reports Claude's new native session after /clear.
    let native = "20000000-0000-4000-8000-000000000123";
    let pid = terminal.group.as_raw_pid();
    let clear = serde_json::from_value::<HookReport>(json!({
        "event": "session_start", "runtime": "claude", "session_id": native,
        "cwd": project, "source": "clear", "claude_pid": pid, "ancestor_pids": [pid],
        "transcript_path": project.join("cleared.jsonl"),
    }))
    .unwrap();
    let transferred: HookReply =
        ipc::request(&h.socket(), &Request::SessionHook(clear), WAIT, WAIT)
            .await
            .unwrap();
    assert_eq!(transferred.session.session_id, native);
    assert_eq!(transferred.session.agent, launch.senator);
    let same_terminal: PathBuf = ipc::request(
        &h.socket(),
        &Request::Attach {
            senator: launch.senator.clone(),
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    assert_eq!(same_terminal, terminal.socket);
    wait_archived(&h, original.seq).await;

    claude_hook(
        h.dir.path(),
        &h,
        terminal.host_id(),
        pending(&launch.session_id),
    )
    .await;
    claude_hook(
        h.dir.path(),
        &h,
        terminal.host_id(),
        request(native, "echo new-context"),
    )
    .await;
    claude_hook(h.dir.path(), &h, terminal.host_id(), pending(native)).await;
    let fresh = next_notice(&h, 2).await;
    assert_eq!(fresh.from, original.from);
    assert!(fresh.body.contains("echo new-context"));
    terminal.stop();
    wait_archived(&h, fresh.seq).await;
    h.stop().await;
}

#[tokio::test]
async fn an_unrelated_claude_result_skips_ipc_without_a_candidate() {
    let (h, _) = fixture(false, Locale::En).await;
    let listener = std::os::unix::net::UnixListener::bind(h.socket()).unwrap();
    listener.set_nonblocking(true).unwrap();
    let id = senate_core::EventId::new();
    claude_hook(
        h.dir.path(),
        &h,
        &id.to_string(),
        json!({
            "session_id": "fixture", "hook_event_name": "PostToolUse",
            "tool_name": "Bash", "tool_input": {"command": "echo no-approval"},
        }),
    )
    .await;
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
