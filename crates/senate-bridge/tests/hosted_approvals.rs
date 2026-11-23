//! A real terminal host with isolated runtime and committee fixtures.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use common::{
    Harness, RECORD_HOSTED_ARGV, WAIT, committee, eventually, fast_timing, hosted_fixture_argv,
    path,
};
use rustix::process::{Pid, Signal, kill_process_group};
use senate_bridge::approvals::Decision;
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::hook::HookKind;
use senate_bridge::ipc::{
    self, ApprovalsReply, ApproveReply, ApproveSessionReply, HostedRuntime, Request, SpawnReply,
};
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
    let mut timing = fast_timing();
    // These fixtures observe actual posts and retry counts. Keep the
    // ordering and backoff while avoiding seconds of idle test time.
    timing.sweep = std::time::Duration::from_millis(50);
    timing.retry_base = std::time::Duration::from_millis(50);
    timing.retry_max = std::time::Duration::from_millis(150);
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
                secretary: false,
                prompt: None,
                model: Default::default(),
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
                senator: launch.senator.clone().into(),
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
                if viewer.read_u8().await.unwrap() == 2 {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stalled_hosted_hook_reports_a_warning_without_updating_the_seat() {
    use senate_bridge::ipc::{HookDropReport, HookReply, HookReport, StatusReply};
    use senate_bridge::session::Runtime;

    let (mut h, project) = fixture(false, Locale::En).await;
    start(&mut h).await;
    let (launched, terminal) = Terminal::spawn(&h, project.clone(), HostedRuntime::Claude).await;
    let before = h.store().session(&launched.session_id).unwrap().unwrap();
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(["hook", "hosted-approval"])
        .env("SENATE_HOME", h.paths.home())
        .env(senate_bridge::host::HOSTED_CLAUDE_ENV, terminal.host_id())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let began = chrono::Utc::now();
    let mut child = cmd.spawn().unwrap();
    let _hold_stdin_open = child.stdin.take().unwrap();
    let out = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("the timed-out hook exited within the watchdog")
        .unwrap();
    assert!(
        out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
        "{out:?}"
    );
    let status: StatusReply = ipc::request(&h.socket(), &Request::Status, WAIT, WAIT)
        .await
        .unwrap();
    let drops = status.dropped_hooks.unwrap();
    assert_eq!(drops.count, 1);
    assert!(drops.last_at >= began);
    assert_eq!(drops.last.kind, HookKind::HostedApproval);
    assert_eq!(drops.last.runtime_hint, Some(Runtime::Claude));
    assert_eq!(drops.last.host_id.unwrap().to_string(), terminal.host_id());
    assert_eq!(drops.last.parent_pid, std::process::id());
    assert_eq!(
        drops.hosted_last_at.get(&launched.session_id),
        Some(&drops.last_at)
    );
    assert_eq!(
        h.store().session(&launched.session_id).unwrap().unwrap(),
        before
    );
    assert!(notices(&h).is_empty());
    assert!(!h.paths.hook_drops_log().exists());
    let view = senate_bridge::status::collect(&h.paths).await.unwrap();
    let status = senate_bridge::status::render(&view, chrono::Utc::now());
    assert!(status.contains("Dropped hook reports"), "{status}");
    assert!(status.contains(terminal.host_id()), "{status}");
    assert!(
        status.contains("may have missed an approval or context hook"),
        "{status}"
    );
    assert!(
        status.contains(&format!(
            "Its last hook report was dropped at {}",
            drops.last_at.format("%Y-%m-%d %H:%M:%S UTC")
        )),
        "{status}"
    );
    let _: () = ipc::request(
        &h.socket(),
        &Request::HookDropped(HookDropReport {
            kind: HookKind::SessionStart,
            runtime_hint: Some(Runtime::Codex),
            host_id: None,
            parent_pid: 0,
        }),
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let status: StatusReply = ipc::request(&h.socket(), &Request::Status, WAIT, WAIT)
        .await
        .unwrap();
    let drops = status.dropped_hooks.unwrap();
    assert_eq!(drops.count, 2);
    assert!(drops.last.host_id.is_none());
    assert!(drops.hosted_last_at.contains_key(&launched.session_id));

    let pid = terminal.group.as_raw_pid();
    let hook: HookReport = serde_json::from_value(json!({
        "event": "user_prompt_submit", "runtime": "claude", "session_id": launched.session_id,
        "cwd": project, "claude_pid": pid, "ancestor_pids": [pid],
    }))
    .unwrap();
    let reply: HookReply = ipc::request(&h.socket(), &Request::SessionHook(hook), WAIT, WAIT)
        .await
        .unwrap();
    assert_eq!(reply.session.agent, launched.senator);
    let view = senate_bridge::status::collect(&h.paths).await.unwrap();
    let after = view.dropped_hooks.as_ref().unwrap();
    assert!(after.hosted_last_at.is_empty());
    assert_eq!(
        (after.count, after.last_at, &after.last),
        (drops.count, drops.last_at, &drops.last)
    );
    let status = senate_bridge::status::render(&view, chrono::Utc::now());
    assert!(status.contains("Dropped hook reports"), "{status}");
    assert!(
        !status.contains("may have missed an approval or context hook"),
        "{status}"
    );
    terminal.stop();
    h.stop().await;
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
                .unwrap(),
            "--name=claude1",
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
async fn a_hosted_codex_thread_is_named_after_the_senators_alias() {
    let (mut h, project) = fixture(true, Locale::En).await;
    let fake = h.codex();
    start(&mut h).await;
    let named = |thread: &str| {
        fake.calls()
            .into_iter()
            .filter(|(method, params)| method == "thread/name/set" && params["threadId"] == thread)
            .map(|(_, params)| params["name"].clone())
            .collect::<Vec<_>>()
    };
    let spawn = |alias: Option<&str>| Request::Spawn {
        session: Default::default(),
        runtime: HostedRuntime::Codex,
        committee: committee(),
        alias: alias.map(path),
        cwd: project.clone(),
        chair: false,
        secretary: false,
        prompt: None,
        model: Default::default(),
    };
    let reviewer: SpawnReply = ipc::request(&h.socket(), &spawn(Some("reviewer")), WAIT, WAIT)
        .await
        .unwrap();
    assert_eq!(named(&reviewer.session_id), [json!("reviewer")]);
    // A spawn without an explicit alias gives Codex the generated name.
    let plain: SpawnReply = ipc::request(&h.socket(), &spawn(None), WAIT, WAIT)
        .await
        .unwrap();
    assert_eq!(named(&plain.session_id), [json!("codex1")]);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_codex_model_alias_is_resolved_before_launch_and_pinned_on_restore() {
    let (mut h, project) = fixture(true, Locale::En).await;
    std::fs::write(
        h.dir.path().join("tui"),
        r#"#!/bin/sh
printf '%s\n' "$@" > argv.tmp && mv argv.tmp argv || exit 1
while IFS= read -r line; do :; done
"#,
    )
    .unwrap();
    let args_contain = |model: &str| {
        std::fs::read_to_string(project.join("argv"))
            .ok()
            .filter(|args| args.lines().any(|arg| arg == format!("--model={model}")))
            .map(|_| ())
    };
    let fake = h.codex();
    start(&mut h).await;
    let spawn = |requested: &str| {
        let mut cmd = tokio::process::Command::new(BIN);
        testenv::isolate_async(&mut cmd, h.dir.path());
        cmd.current_dir(&project)
            .env("SENATE_HOME", h.paths.home())
            .args([
                "spawn",
                "codex",
                "--committee",
                committee().as_str(),
                "--model",
                requested,
            ]);
        cmd
    };
    let unknown = spawn("astr").output().await.unwrap();
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("close matches"));
    assert!(!fake.methods().iter().any(|method| method == "thread/start"));

    let out = spawn("astra").output().await.unwrap();
    assert!(out.status.success(), "{out:?}");
    let senator = path(std::str::from_utf8(&out.stdout).unwrap().trim());
    let resolved = "gpt-6.10-astra";
    assert!(String::from_utf8_lossy(&out.stderr).contains(&format!("astra → {resolved}")));
    assert!(
        fake.calls()
            .iter()
            .any(|(method, params)| method == "thread/start" && params["model"] == resolved)
    );
    eventually("spawned Codex model", WAIT, || args_contain(resolved)).await;
    let list_count = || fake.methods().iter().filter(|m| *m == "model/list").count();
    let mut queries = list_count();
    let mut guard = None;
    for (override_id, expected) in [(None, resolved), (Some("sol"), "gpt-6-sol")] {
        let _: senate_bridge::ipc::StopReply = ipc::request(
            &h.socket(),
            &Request::Stop {
                session: Default::default(),
                senator: senator.clone().into(),
                force: true,
            },
            WAIT,
            WAIT,
        )
        .await
        .unwrap();
        drop(guard.take());
        std::fs::remove_file(project.join("argv")).unwrap();
        let reply: SpawnReply = ipc::request(
            &h.socket(),
            &Request::Restore {
                session: Default::default(),
                senator: senator.clone().into(),
                model: senate_bridge::ipc::HostedModel::new(override_id.map(str::to_string), None),
            },
            WAIT,
            WAIT,
        )
        .await
        .unwrap();
        queries += if override_id.is_some() { 2 } else { 0 };
        assert_eq!(list_count(), queries);
        let terminal = Terminal::from_launch(&h, &reply).await;
        eventually("restored Codex model", WAIT, || args_contain(expected)).await;
        let listing: senate_bridge::ipc::SessionsReply =
            ipc::request(&h.socket(), &Request::Sessions, WAIT, WAIT)
                .await
                .unwrap();
        assert_eq!(listing.rows[0].model.model.as_deref(), Some(expected));
        // The next iteration stops this host through IPC.
        guard = Some(terminal);
    }
    drop(guard);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_codex_stop_interrupts_an_attended_turn_only_when_forced_and_waits() {
    let (mut h, project) = fixture(true, Locale::En).await;
    let fake = h.codex();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Codex).await;
    let thread = &launch.session_id;
    fake.busy(thread, "running");
    fake.state.lock().unwrap().defer_interrupt = true;
    let mut viewer = terminal.viewer().await;
    viewer.write_all(&[2, 0, 1, b'x']).await.unwrap();
    eventually("viewer attended", WAIT, || {
        (terminal.activity()?["input_at"].as_u64()? > 0).then_some(())
    })
    .await;
    let stop = |force| Request::Stop {
        session: Default::default(),
        senator: launch.senator.clone().into(),
        force,
    };
    let socket = h.socket();
    let refused = ipc::request::<senate_bridge::ipc::StopReply>(&socket, &stop(false), WAIT, WAIT)
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("viewer"));
    let interrupts = || {
        fake.calls()
            .into_iter()
            .filter(|(m, _)| m == "turn/interrupt")
            .map(|(_, p)| p)
            .collect::<Vec<_>>()
    };
    assert!(interrupts().is_empty());
    let force = stop(true);
    let stopping = ipc::request::<senate_bridge::ipc::StopReply>(&socket, &force, WAIT, WAIT);
    let confirm = async {
        eventually("interrupt and subsequent status check", WAIT, || {
            let calls = fake.methods();
            let at = calls.iter().position(|m| m == "turn/interrupt")?;
            calls[at + 1..]
                .iter()
                .any(|m| m == "thread/turns/list")
                .then_some(())
        })
        .await;
        assert_eq!(
            interrupts(),
            [json!({"threadId": thread, "turnId": "running"})]
        );
        assert!(terminal.socket.exists());
        fake.idle(thread);
    };
    let (stopped, ()) = tokio::join!(stopping, confirm);
    stopped.unwrap();
    assert!(!terminal.socket.exists());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_codex_stop_can_be_retried_if_the_turn_does_not_end() {
    let (mut h, project) = fixture(true, Locale::En).await;
    let fake = h.codex();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Codex).await;
    fake.busy(&launch.session_id, "slow");
    fake.state.lock().unwrap().defer_interrupt = true;
    let request = Request::Stop {
        session: Default::default(),
        senator: launch.senator.into(),
        force: true,
    };
    let socket = h.socket();
    let stop = || ipc::request::<senate_bridge::ipc::StopReply>(&socket, &request, WAIT, WAIT);
    let error = stop().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("timed out waiting for Codex thread"),
        "{error}"
    );
    assert!(terminal.socket.exists());
    fake.state.lock().unwrap().defer_interrupt = false;
    stop().await.unwrap();
    assert!(fake.active_turn(&launch.session_id).is_none());
    h.stop().await;
}

/// What a hosted Codex restore does: it rejoins the same thread on the
/// app-server, with no new `thread/start`. Codex keeps a loaded thread's MCP
/// connections, and so the tools it listed, until the thread has had no
/// subscriber for its unload delay (60 s by default), and Senate's relay
/// stays subscribed; so a stop and restore is no way to change a Codex
/// session's tools. That is why a Codex session lists every role's tools
/// from the start (`tests/mcp.rs`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hosted_codex_restore_rejoins_its_thread_and_starts_none() {
    let (mut h, project) = fixture(true, Locale::En).await;
    let fake = h.codex();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Codex).await;
    let thread = launch.session_id.clone();
    let count = |name: &str| {
        fake.calls()
            .iter()
            .filter(|(method, params)| {
                method == name && (name == "thread/start" || params["threadId"] == thread.as_str())
            })
            .count()
    };
    assert_eq!(count("thread/start"), 1);

    let _: senate_bridge::ipc::StopReply = ipc::request(
        &h.socket(),
        &Request::Stop {
            session: Default::default(),
            senator: launch.senator.clone().into(),
            force: true,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    assert!(!fake.methods().iter().any(|m| m == "turn/interrupt"));
    terminal.stop();
    let restored: SpawnReply = ipc::request(
        &h.socket(),
        &Request::Restore {
            session: Default::default(),
            senator: launch.senator.clone().into(),
            model: Default::default(),
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    // The same thread and seat, rejoined: no new `thread/start`.
    assert_eq!(restored.session_id, thread);
    assert_eq!(restored.senator, launch.senator);
    assert_eq!(count("thread/start"), 1);
    assert!(count("thread/resume") >= 1, "{:?}", fake.methods());
    let terminal = Terminal::from_launch(&h, &restored).await;
    terminal.stop();
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
    let method = "item/commandExecution/requestApproval";
    let params = json!({"threadId":thread, "turnId":"turn", "itemId":"command", "command":"echo held-before-restore"});
    let request_id = fake.request_approval(thread, method, params.clone());
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
    let resumed = || {
        fake.calls()
            .iter()
            .filter(|(method, params)| method == "thread/resume" && params["threadId"] == thread)
            .count()
    };
    let mut replayed_through = resumed();
    // Make the reconnect path explicit. A real app-server retains and
    // replays its unresolved request after thread/resume; the fake needs
    // the test to supply that replay on each new relay subscription.
    fake.disconnect_subscribers(thread);
    eventually(
        "the relay rejoined the still-pending manual thread",
        WAIT,
        || (resumed() > replayed_through && fake.subscribers(thread) == 1).then_some(()),
    )
    .await;
    let senator = h.store().session(thread).unwrap().unwrap().agent;
    h.server.state.inner.lock().unwrap().chair = Some((senator.clone(), true));
    let launch: SpawnReply = ipc::request(
        &h.socket(),
        &Request::Restore {
            session: Default::default(),
            senator: senator.into(),
            model: Default::default(),
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let terminal = Terminal::from_launch(&h, &launch).await;
    let waiting = eventually(
        "the restored host's notice after the native replay",
        WAIT,
        || {
            let generation = resumed();
            if generation > replayed_through && fake.subscribers(thread) == 1 {
                fake.replay_approval(thread, request_id, method, params.clone());
                replayed_through = generation;
            }
            let mut found = notices(&h);
            assert!(found.len() <= 1, "{found:?}");
            found.pop()
        },
    )
    .await;
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
            senator: launch.senator.into(),
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
            senator: launch.senator.clone().into(),
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

/// Read the watch stream until a `sessions` event (other lines pass by).
async fn next_sessions_event(stream: &mut ipc::WatchStream) {
    tokio::time::timeout(WAIT, async {
        loop {
            match stream.next().await.unwrap() {
                Some(ipc::WatchEvent::Sessions) => return,
                Some(_) => {}
                None => panic!("the watch stream closed"),
            }
        }
    })
    .await
    .expect("a sessions event");
}

async fn session_prompt(h: &Harness, session_id: &str) -> Option<ipc::HostedPrompt> {
    let reply: ipc::SessionsReply = ipc::request(&h.socket(), &Request::Sessions, WAIT, WAIT)
        .await
        .unwrap();
    reply
        .rows
        .into_iter()
        .find(|r| r.session_id == session_id)
        .and_then(|r| r.prompt)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_prompt_shows_on_its_session_row_and_watchers_hear_it_open_and_close() {
    let (mut h, project) = fixture(true, Locale::En).await;
    start(&mut h).await;
    let mut stream = ipc::IpcClient::connect(&h.socket(), WAIT)
        .await
        .unwrap()
        .watch(true, WAIT)
        .await
        .unwrap();
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Claude).await;
    // The host started.
    next_sessions_event(&mut stream).await;
    assert_eq!(session_prompt(&h, &launch.session_id).await, None);

    let signal = |event: &str| {
        json!({
            "hook_event_name": event, "session_id": launch.session_id,
            "tool_name": "Bash", "tool_input": {"command": "echo row-fixture"},
        })
    };
    let notify = json!({
        "hook_event_name": "Notification", "session_id": launch.session_id,
        "notification_type": "permission_prompt",
    });
    let send = |input| claude_hook(h.dir.path(), &h, terminal.host_id(), input);
    send(signal("PermissionRequest")).await;
    send(notify).await;
    // The prompt opened: watchers hear it, and the row says what waits.
    next_sessions_event(&mut stream).await;
    let prompt = session_prompt(&h, &launch.session_id)
        .await
        .expect("the row carries the waiting prompt");
    assert_eq!(prompt.tool.as_deref(), Some("echo row-fixture"));
    let age = chrono::Utc::now() - prompt.since;
    assert!(
        age >= chrono::Duration::zero() && age < chrono::Duration::seconds(60),
        "dated on the wall clock: {age:?}"
    );

    // Answered in the terminal: the row clears, and watchers hear it.
    send(signal("PostToolUse")).await;
    next_sessions_event(&mut stream).await;
    assert_eq!(session_prompt(&h, &launch.session_id).await, None);
    terminal.stop();
    h.stop().await;
}

/// A TUI that stops on a choice no hook or app-server tells of (Codex's
/// "hooks need review" at startup): once its screen is quiet, the chair
/// hears of it, the row and doctor show it, and answering it closes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_choice_on_the_screen_is_a_notice_until_someone_answers_it() {
    let (mut h, project) = fixture(true, Locale::En).await;
    let program = h.dir.path().join("tui");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n{RECORD_HOSTED_ARGV}\n\
             printf '  Hooks need review\\r\\n\\r\\n\\342\\200\\272 1. Review hooks\\r\\n  2. Trust all and continue\\r\\n'\n\
             IFS= read -r line\n\
             printf '\\033[2J\\033[Hchosen\\r\\n'\n\
             while IFS= read -r line; do :; done\n"
        ),
    )
    .unwrap();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Claude).await;

    let notice = next_notice(&h, 1).await;
    assert!(
        notice
            .body
            .contains("waiting on a prompt in its terminal in committee `refund-v2` that Senate does not answer"),
        "{}",
        notice.body
    );
    assert!(notice.body.contains("senate attach"), "{}", notice.body);
    assert_eq!(notice.to, vec![Address::Chair]);
    let prompt = session_prompt(&h, &launch.session_id)
        .await
        .expect("the row carries the waiting prompt");
    assert!(prompt.screen && prompt.tool.is_none(), "{prompt:?}");

    // Senate cannot answer it from outside the terminal.
    let refused = approve_session(&h, &launch, Decision::Allow)
        .await
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("not a permission prompt Senate knows of"),
        "{refused}"
    );
    assert!(refused.to_string().contains("senate attach"), "{refused}");

    // Someone attaches and chooses: the TUI repaints and the notice closes.
    send_input(&terminal).await;
    wait_archived(&h, notice.seq).await;
    assert_eq!(session_prompt(&h, &launch.session_id).await, None);
    // The screen no longer shows a choice: nothing more is said.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert_eq!(notices(&h).len(), 1);
    terminal.stop();
    h.stop().await;
}

// ---- answering from outside the terminal (`senate session approve`) --------

async fn approve_session(
    h: &Harness,
    launch: &SpawnReply,
    decision: Decision,
) -> Result<ApproveSessionReply, ipc::ClientError> {
    ipc::request(
        &h.socket(),
        &Request::ApproveSession {
            senator: launch.senator.clone().into(),
            decision,
        },
        WAIT,
        WAIT,
    )
    .await
}

/// The person's notes that a prompt was answered from outside a terminal.
fn answer_notes(h: &Harness) -> Vec<Event> {
    h.server
        .events("refund-v2")
        .into_iter()
        .filter(|event| event.meta.contains_key(senate_core::meta::PROMPT_ANSWER))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hosted_stop_reminds_only_about_delivered_unanswered_relays_once() {
    let (mut h, project) = fixture(false, Locale::En).await;
    let _fake = h.codex();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project.clone(), HostedRuntime::Codex).await;
    let mut roots = Vec::new();
    for _ in 0..3 {
        let root = h.server.append_silently(
            senate_core::NewEvent::new(
                committee(),
                senate_core::Kind::Question,
                Address::Committee,
                "check this",
            )
            .to(Address::senator(&launch.senator))
            .with_meta(
                senate_core::meta::ORIGIN,
                json!({"locator": "/parent::refund-v2::speaker", "thread": 7}),
            ),
        );
        h.store().enqueue(&launch.senator, &root).unwrap();
        h.store()
            .mark_read(
                &[(launch.senator.clone(), committee(), root.seq)],
                &launch.session_id,
            )
            .unwrap();
        roots.push(root.seq);
    }
    for (kind, root) in [
        (senate_core::Kind::Answer, roots[0]),
        (senate_core::Kind::Status, roots[1]),
    ] {
        h.server.append_silently(
            senate_core::NewEvent::new(
                committee(),
                kind,
                Address::senator(&launch.senator),
                "reported",
            )
            .in_thread(root),
        );
    }
    let reminder = h
        .hook_codex(HookKind::Stop, &launch.session_id, &project)
        .await
        .unwrap();
    assert_eq!(reminder["decision"], "block");
    assert!(
        reminder["reason"]
            .as_str()
            .unwrap()
            .contains(&format!("(thread {})", roots[2]))
    );
    assert!(
        h.hook_codex(HookKind::Stop, &launch.session_id, &project)
            .await
            .is_none()
    );
    terminal.stop();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codex_approval_on_a_relay_reports_the_task_even_at_an_attended_terminal() {
    let (mut h, project) = fixture(false, Locale::En).await;
    let fake = h.codex();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Codex).await;
    let mut viewer = terminal.viewer().await;
    viewer.write_all(&[2, 0, 1, b'x']).await.unwrap();
    let native = &launch.session_id;
    eventually("subscribed", WAIT, || {
        (fake.subscribers(native) == 1).then_some(())
    })
    .await;
    {
        let mut server = h.server.state.inner.lock().unwrap();
        server
            .roster
            .name_speaker(Some(path("alice").into()))
            .unwrap();
        server
            .roster
            .set_secretaries(path("alice"), vec![launch.senator.clone()])
            .unwrap();
    }
    let request = senate_core::NewEvent::new(
        committee(),
        senate_core::Kind::Request,
        Address::Committee,
        "check the build",
    )
    .to(Address::senator(&launch.senator))
    .on_behalf_of(Address::senior(&h.config.person))
    .with_source(senate_core::Source {
        platform: "chat".into(),
        group_id: "team".into(),
        group_name: None,
        simulated: false,
        simulated_by: None,
    })
    .with_meta(
        senate_core::meta::INTENDED,
        json!(format!(
            "/parent/child::refund-v2::senator::{}",
            launch.senator
        )),
    )
    .with_meta(
        senate_core::meta::ORIGIN,
        json!({"locator": "/parent::refund-v2::speaker", "thread": 174}),
    );
    let incoming = h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Request,
            Address::Committee,
            "earlier task",
        )
        .to(Address::Speaker)
        .with_meta(
            senate_core::meta::ORIGIN,
            json!({"locator":"/parent::refund-v2::speaker", "thread":170}),
        ),
    );
    let older = h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Request,
            Address::Speaker,
            "earlier task",
        )
        .to(Address::senator(&launch.senator))
        .with_meta(senate_core::meta::RELAYED_FROM, json!(incoming.seq)),
    );
    h.store().enqueue(&launch.senator, &older).unwrap();
    h.store()
        .mark_read(&[(launch.senator.clone(), committee(), older.seq)], native)
        .unwrap();
    let root = h.server.append_silently(request);
    let mut status = StatusPayload::new(StatusState::Processing);
    status.session = Some(native.clone());
    status.title = Some("Running build checks".into());
    status.steps = Some(vec![senate_core::task::Step {
        id: "test".into(),
        label: "Run the tests".into(),
        done: false,
    }]);
    h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Status,
            Address::senator(&launch.senator),
            "",
        )
        .in_thread(root.seq)
        .with_meta(senate_core::meta::STATUS, status.value()),
    );
    let id = fake.request_approval(
        native,
        "item/commandExecution/requestApproval",
        json!({"threadId":native,"command":"cargo test"}),
    );
    wait_relayed(&h, &format!("{native}:{id}")).await;
    approve_session(&h, &launch, Decision::Allow).await.unwrap();
    let notice = next_notice(&h, 1).await;
    assert_eq!(notice.thread, Some(root.seq));
    assert_eq!(notice.to, [Address::senior(&h.config.person)]);
    assert_eq!(
        notice.meta[senate_core::meta::APPROVAL_WAIT],
        json!({"thread":root.seq, "label":"run: cargo test"})
    );
    wait_archived(&h, notice.seq).await;
    let archive = archived(&h, notice.seq).unwrap();
    assert_eq!(
        archive.meta[senate_core::meta::APPROVAL_WAIT],
        json!({"decision":"allow","actor":Address::senior(&h.config.person).in_text()})
    );
    h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Answer,
            Address::senator(&launch.senator),
            "finished",
        )
        .in_thread(root.seq),
    );
    let next = fake.request_approval(
        native,
        "item/commandExecution/requestApproval",
        json!({"threadId":native,"command":"cargo check"}),
    );
    wait_relayed(&h, &format!("{native}:{next}")).await;
    let forwarded = next_notice(&h, 2).await;
    assert_eq!(forwarded.thread, Some(older.seq));
    approve_session(&h, &launch, Decision::Allow).await.unwrap();
    wait_archived(&h, forwarded.seq).await;

    // The desk worker passed the old task on to a local Speaker seat.
    h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Note,
            Address::Speaker,
            "forwarded",
        )
        .in_thread(older.seq)
        .with_meta(
            senate_core::meta::FORWARDED_TO,
            json!(format!("{}::speaker", committee())),
        )
        .with_meta(
            senate_core::meta::ACTED_BY,
            json!(launch.senator.to_string()),
        ),
    );
    let local = h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Request,
            Address::senior(&h.config.person),
            "local checks",
        )
        .to(Address::senator(&launch.senator)),
    );
    let append_status = |thread, session: &str| {
        let mut status = StatusPayload::new(StatusState::Processing);
        status.session = Some(session.to_owned());
        h.server.append_silently(
            senate_core::NewEvent::new(
                committee(),
                senate_core::Kind::Status,
                Address::senator(&launch.senator),
                "",
            )
            .in_thread(thread)
            .with_meta(senate_core::meta::STATUS, status.value()),
        );
    };
    append_status(local.seq, native);

    // An unrelated chat and its relay arrive after the active local status.
    let chat = h.server.append_silently(senate_core::NewEvent::new(
        committee(),
        senate_core::Kind::Chat,
        Address::senior(&h.config.person),
        "different request",
    ));
    let chat_relay = h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Request,
            Address::Speaker,
            "different request",
        )
        .to(Address::senator(&launch.senator))
        .with_meta(senate_core::meta::RELAYED_FROM, json!(chat.seq)),
    );
    append_status(chat_relay.seq, "different-session");

    let id = fake.request_approval(
        native,
        "item/commandExecution/requestApproval",
        json!({"threadId":native,"command":"cargo fmt"}),
    );
    wait_relayed(&h, &format!("{native}:{id}")).await;
    let notice = next_notice(&h, 3).await;
    assert_eq!(notice.thread, Some(local.seq));
    approve_session(&h, &launch, Decision::Allow).await.unwrap();
    wait_archived(&h, notice.seq).await;

    h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Answer,
            Address::senator(&launch.senator),
            "done",
        )
        .in_thread(local.seq),
    );
    // A standalone notice is only posted once the terminal is unattended.
    let previous = terminal.activity().unwrap();
    viewer.shutdown().await.unwrap();
    terminal
        .wait_detached(previous["detached_at"].as_u64().unwrap())
        .await;
    let id = fake.request_approval(
        native,
        "item/commandExecution/requestApproval",
        json!({"threadId":native,"command":"git status"}),
    );
    wait_relayed(&h, &format!("{native}:{id}")).await;
    let notice = next_notice(&h, 4).await;
    assert_eq!(
        notice.thread, None,
        "only forwarded and unrelated work remains"
    );
    approve_session(&h, &launch, Decision::Allow).await.unwrap();
    wait_archived(&h, notice.seq).await;
    terminal.stop();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unseated_host_drops_the_notice_before_attempting_an_append() {
    let (mut h, project) = fixture(false, Locale::En).await;
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Claude).await;
    h.server.state.inner.lock().unwrap().unseated = Some(launch.senator.clone());
    claude_hook(
        h.dir.path(),
        &h,
        terminal.host_id(),
        json!({
            "hook_event_name":"Notification", "session_id": launch.session_id,
            "notification_type":"permission_prompt"
        }),
    )
    .await;
    eventually("the unseated notice was suppressed", WAIT, || {
        let db = rusqlite::Connection::open(h.paths.state_db()).unwrap();
        let suppressed: bool = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM hosted_approvals,
            json_each(state, '$.notices') WHERE json_extract(value, '$.unseated') = 1)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        suppressed.then_some(())
    })
    .await;
    assert!(notices(&h).is_empty());
    assert_eq!(
        h.server.state.inner.lock().unwrap().hosted_approval_appends,
        0
    );
    terminal.stop();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_approve_answers_a_hosted_codex_prompt_through_its_app_server() {
    let (mut h, project) = fixture(true, Locale::En).await;
    let fake = h.codex();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project, HostedRuntime::Codex).await;
    let thread = launch.session_id.clone();
    eventually("the relay subscribed", WAIT, || {
        (fake.subscribers(&thread) == 1).then_some(())
    })
    .await;
    let nothing = approve_session(&h, &launch, Decision::Allow)
        .await
        .unwrap_err();
    assert!(
        nothing
            .to_string()
            .contains("has no permission prompt waiting"),
        "{nothing}"
    );
    let method = "item/commandExecution/requestApproval";
    let cases = [
        (Decision::Allow, "accept", "allowed once"),
        (
            Decision::AllowForSession,
            "acceptForSession",
            "always allowed",
        ),
        (Decision::Deny, "decline", "denied"),
    ];
    for (n, (decision, wire, verb)) in cases.into_iter().enumerate() {
        let command = format!("echo remote-{n}");
        let native = fake.request_approval(
            &thread,
            method,
            json!({"threadId": thread, "turnId": "turn", "itemId": "cmd", "command": command}),
        );
        wait_relayed(&h, &format!("{thread}:{native}")).await;
        let reply = approve_session(&h, &launch, decision).await.unwrap();
        assert_eq!(reply.decision, decision);
        assert_eq!(reply.senator, launch.senator);
        assert_eq!(
            reply.tool.as_deref(),
            Some(format!("run: {command}").as_str())
        );
        let responses = fake.responses();
        assert_eq!(responses.len(), n + 1, "{responses:?}");
        assert_eq!(responses[n].0, json!(native));
        assert_eq!(responses[n].1, json!({"decision": wire}));
        let note = eventually("the answer is noted in the log", WAIT, || {
            answer_notes(&h).get(n).cloned()
        })
        .await;
        assert_eq!(note.kind, senate_core::Kind::Note);
        assert!(
            note.body.contains(&format!(
                "{verb} a permission prompt of {}",
                Address::senator(&launch.senator).in_text()
            )),
            "{}",
            note.body
        );
        assert!(
            note.body.contains(&format!("\"run: {command}\"")),
            "{}",
            note.body
        );
        assert_eq!(
            note.meta[senate_core::meta::PROMPT_ANSWER]["runtime"],
            "codex"
        );
    }
    let nothing = approve_session(&h, &launch, Decision::Deny)
        .await
        .unwrap_err();
    assert!(
        nothing
            .to_string()
            .contains("has no permission prompt waiting"),
        "{nothing}"
    );
    assert_eq!(fake.responses().len(), 3);
    terminal.stop();
    h.stop().await;
}

/// Wait until the relay lists approval `id`.
async fn wait_relayed(h: &Harness, id: &str) {
    tokio::time::timeout(WAIT, async {
        loop {
            let pending: ApprovalsReply =
                ipc::request(&h.socket(), &Request::Approvals, WAIT, WAIT)
                    .await
                    .unwrap();
            if pending.approvals.iter().any(|a| a.id == id) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the relay holds the approval");
}

/// A Claude Code permission dialog that records each key it is answered
/// with in `.keys`, then draws itself again.
fn claude_dialog_program(h: &Harness) {
    std::fs::write(
        h.dir.path().join("tui"),
        format!(
            "#!/bin/sh\n{RECORD_HOSTED_ARGV}\n\
             stty raw -echo\n\
             dialog() {{ printf '\\033[2J\\033[H Bash command\\r\\n\\r\\n   echo approve-fixture\\r\\n\\r\\n Do you want to proceed?\\r\\n \\342\\235\\257 1. Yes\\r\\n   2. Yes, and do not ask again for echo commands\\r\\n   3. No, and tell Claude what to do differently (esc)\\r\\n'; }}\n\
             dialog\n\
             while :; do\n\
               key=$(dd bs=1 count=1 2>/dev/null)\n\
               printf '\\033[2J\\033[Hchose %s\\r\\n' \"$key\"\n\
               sleep 0.3\n\
               dialog\n\
               printf '%s' \"$key\" >> .keys\n\
             done\n"
        ),
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_approve_chooses_the_option_of_a_hosted_claude_dialog() {
    let (mut h, project) = fixture(true, Locale::En).await;
    claude_dialog_program(&h);
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project.clone(), HostedRuntime::Claude).await;
    hosted_fixture_argv(&terminal.socket, &project, WAIT).await;
    let request = json!({
        "hook_event_name": "PermissionRequest", "session_id": launch.session_id,
        "tool_name": "Bash", "tool_input": {"command": "echo approve-fixture"},
    });
    // No permission request heard: nothing is typed.
    let nothing = approve_session(&h, &launch, Decision::Allow)
        .await
        .unwrap_err();
    assert!(
        nothing
            .to_string()
            .contains("has no permission prompt waiting"),
        "{nothing}"
    );
    let root = h.server.append_silently(
        senate_core::NewEvent::new(
            committee(),
            senate_core::Kind::Request,
            Address::Committee,
            "check the build",
        )
        .to(Address::senator(&launch.senator))
        .with_meta(
            senate_core::meta::ORIGIN,
            json!({"locator":"/parent::refund-v2::speaker","thread":174}),
        ),
    );
    h.store().enqueue(&launch.senator, &root).unwrap();
    h.store()
        .mark_read(
            &[(launch.senator.clone(), committee(), root.seq)],
            &launch.session_id,
        )
        .unwrap();
    let keys = || std::fs::read_to_string(project.join(".keys")).unwrap_or_default();
    let cases = [
        (Decision::Allow, "1", "allowed once"),
        (Decision::AllowForSession, "2", "always allowed"),
        (Decision::Deny, "3", "denied"),
    ];
    let mut typed = String::new();
    for (n, (decision, key, verb)) in cases.into_iter().enumerate() {
        claude_hook(h.dir.path(), &h, terminal.host_id(), request.clone()).await;
        claude_hook(h.dir.path(), &h, terminal.host_id(), json!({
            "hook_event_name":"Notification", "session_id":launch.session_id, "notification_type":"permission_prompt"
        })).await;
        let notice = next_notice(&h, n + 1).await;
        assert_eq!(notice.thread, Some(root.seq));
        let reply = approve_session(&h, &launch, decision).await.unwrap();
        assert_eq!(reply.tool.as_deref(), Some("echo approve-fixture"));
        wait_archived(&h, notice.seq).await;
        assert_eq!(
            archived(&h, notice.seq).unwrap().meta[senate_core::meta::APPROVAL_WAIT],
            json!({"decision":decision, "actor":Address::senior(&h.config.person).in_text()})
        );
        typed.push_str(key);
        eventually(
            "the dialog took the key and drew itself again",
            WAIT,
            || (keys() == typed).then_some(()),
        )
        .await;
        let note = eventually("the answer is noted in the log", WAIT, || {
            answer_notes(&h).get(n).cloned()
        })
        .await;
        assert!(note.body.contains(verb), "{}", note.body);
        assert!(
            note.body.contains("\"echo approve-fixture\""),
            "{}",
            note.body
        );
        assert_eq!(
            note.meta[senate_core::meta::PROMPT_ANSWER]["runtime"],
            "claude"
        );
        // Answered: a denial brings no result hook, and none is needed.
        let again = approve_session(&h, &launch, decision).await.unwrap_err();
        assert!(
            again
                .to_string()
                .contains("has no permission prompt waiting"),
            "{again}"
        );
    }
    assert_eq!(keys(), "123");
    terminal.stop();
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_approve_is_refused_inside_a_hosted_sessions_process_tree() {
    let (mut h, project) = fixture(true, Locale::En).await;
    // The hosted TUI itself asks to approve, as an agent would from a tool.
    std::fs::write(
        h.dir.path().join("tui"),
        format!(
            "#!/bin/sh\n{RECORD_HOSTED_ARGV}\n\
             while [ ! -e .go ]; do sleep 0.05; done\n\
             {BIN:?} session approve somebody > .guard.tmp 2>&1; echo \"exit $?\" >> .guard.tmp; mv .guard.tmp .guard\n\
             while IFS= read -r line; do :; done\n"
        ),
    )
    .unwrap();
    start(&mut h).await;
    let (launch, terminal) = Terminal::spawn(&h, project.clone(), HostedRuntime::Claude).await;
    // Once the host is published as the senator's.
    std::fs::write(project.join(".go"), "").unwrap();
    let out = eventually("the TUI's approve came back", WAIT, || {
        std::fs::read_to_string(project.join(".guard")).ok()
    })
    .await;
    assert!(out.contains("exit 1"), "{out}");
    assert!(
        out.contains(&format!(
            "inside the process tree of {}",
            Address::senator(&launch.senator).in_text()
        )),
        "{out}"
    );
    assert!(out.contains("a person's decision"), "{out}");
    assert!(answer_notes(&h).is_empty());
    terminal.stop();
    h.stop().await;
}
