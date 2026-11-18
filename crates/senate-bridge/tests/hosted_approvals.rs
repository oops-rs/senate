//! A real terminal host with isolated runtime and committee fixtures.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::{Harness, WAIT, committee, eventually, fast_timing, path};
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
    std::fs::write(&program, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n").unwrap();
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
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.stop();
    }
}

fn notices(h: &Harness) -> Vec<Event> {
    h.server
        .events("refund-v2")
        .into_iter()
        .filter(Event::is_hosted_approval)
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
    viewer.write_all(&[1, 0, 32, 0, 81, 2, 0, 0]).await.unwrap();
}

async fn send_input(terminal: &Terminal) {
    let mut viewer = terminal.viewer().await;
    viewer.write_all(&[2, 0, 2, b'x', b'\n']).await.unwrap();
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
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Claude).await;
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
    send(notify.clone()).await;
    assert!(
        notices(&h).is_empty(),
        "the unverified claim did not arm a prompt"
    );

    send(signal("PermissionRequest", "echo approval-fixture")).await;
    assert!(
        notices(&h).is_empty(),
        "the hook alone is not a pending prompt"
    );
    send(notify.clone()).await;
    let first = next_notice(&h, 1).await;
    assert!(first.body.contains("工具或命令：echo approval-fixture"));
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
    send(notify).await;
    let next = next_notice(&h, 2).await;
    terminal.stop();
    wait_archived(&h, next.seq).await;
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
            .contains("Tool or command: run: echo approval-fixture")
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
    send_input(&terminal).await;
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
    let second = next_notice(&h, 2).await;
    assert_ne!(first.id, second.id);
    assert!(
        fake.answer_in_tui(native),
        "the request was still pending when the TUI handled it"
    );
    wait_archived(&h, second.seq).await;
    assert!(fake.responses().is_empty());
    terminal.stop();
    eventually("the fixture host exited", WAIT, || {
        (!terminal.socket.exists()).then_some(())
    })
    .await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_existing_codex_approval_is_observed_only_after_hosted_restore() {
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
    terminal.stop();
    wait_archived(&h, waiting.seq).await;
    assert!(fake.responses().is_empty());
    h.stop().await;
}
