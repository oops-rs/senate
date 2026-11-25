//! Terminal use end to end: a real daemon, real scratch hosts started by
//! the real `senate terminal open`, driven through the MCP tool and the
//! CLI; and who is refused what.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Harness, WAIT, committee, eventually, fast_timing, path};
use rmcp::ServiceExt as _;
use senate_bridge::daemon::{DaemonOptions, start_with_host_binary};
use senate_bridge::ipc::{
    self, HookReply, HostedRuntime, Request, SessionSelector, SpawnReply, TerminalAction,
    TerminalReply, TerminalScreen, TerminalTarget,
};
use senate_bridge::mcp::SenateMcp;
use senate_bridge::testenv;
use senate_core::EventId;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};

const BIN: &str = env!("CARGO_BIN_EXE_senate");

/// A menu to drive: arrows move the mark, `r` repaints (with the size it
/// sees), Enter picks; then it asks a name on an ordinary line and exits
/// with code 3.
const MENU: &str = r#"#!/bin/sh
stty raw -echo
esc=$(printf '\033')
cr=$(printf '\r')
sel=1
draw() {
  printf '\033[2J\033[HPick one (size %s)\r\n' "$(stty size)"
  n=1
  for item in alpha beta gamma; do
    if [ "$n" -eq "$sel" ]; then printf '> %s\r\n' "$item"; else printf '  %s\r\n' "$item"; fi
    n=$((n + 1))
  done
}
key() { dd bs=1 count=1 2>/dev/null; }
draw
while :; do
  k=$(key)
  case "$k" in
    "$esc")
      key >/dev/null
      case "$(key)" in
        A) if [ "$sel" -gt 1 ]; then sel=$((sel - 1)); fi ;;
        B) if [ "$sel" -lt 3 ]; then sel=$((sel + 1)); fi ;;
      esac ;;
    r) ;;
    "$cr") break ;;
    *) continue ;;
  esac
  draw
done
stty sane
printf '\r\nname? '
read -r name
echo "hello $name, you picked $sel"
exit 3
"#;

/// A hosted "senator" that shows a question and records every line typed
/// into it: nothing may ever be.
const STUCK_SENATOR: &str = r#"#!/bin/sh
printf '\033[2J\033[HAllow rm -rf build? (y/n)\r\n'
while IFS= read -r line; do printf '%s\n' "$line" >> "$PWD/.typed"; done
"#;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let file = dir.join(name);
    std::fs::write(&file, body).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    file
}

/// A daemon in a short temporary home (host sockets need a short path),
/// with `claude_bin` standing in for a hosted Claude.
async fn daemon(claude_bin: Option<&str>) -> Harness {
    let mut h = Harness::new().await;
    h.dir = tempfile::tempdir_in("/tmp").unwrap();
    h.paths = senate_bridge::Paths::new(h.dir.path().join("senate"));
    h.paths.ensure_home().unwrap();
    if let Some(body) = claude_bin {
        let fixture = script(h.dir.path(), "fixture-claude", body);
        std::fs::write(
            h.paths.config(),
            format!(
                "[companion]\nclaude_bin = {:?}\n",
                fixture.to_str().unwrap()
            ),
        )
        .unwrap();
    }
    h.config.committees.push(committee());
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
    h
}

/// Register a coding session on `on`, in a directory of its own. Its
/// senator is `claude-<id>`.
async fn session(h: &Harness, id: &str, on: &str) -> PathBuf {
    session_with(h, id, on, None).await
}

async fn session_with(h: &Harness, id: &str, on: &str, claude_pid: Option<u32>) -> PathBuf {
    let cwd = h.workdir(id, Some(on));
    let mut hook = json!({
        "event": "session_start", "runtime": "claude", "session_id": id,
        "cwd": cwd, "committee_file": on, "source": "startup",
    });
    if let Some(pid) = claude_pid {
        hook["claude_pid"] = json!(pid);
    }
    let _: HookReply = ipc::request(
        &h.socket(),
        &Request::SessionHook(serde_json::from_value(hook).unwrap()),
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    cwd
}

fn selector(id: &str) -> SessionSelector {
    SessionSelector {
        session_id: Some(id.into()),
        claude_pid: None,
    }
}

fn command(h: &Harness, cwd: &Path, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, h.dir.path());
    cmd.args(args)
        .current_dir(cwd)
        .env("SENATE_HOME", h.paths.home());
    cmd
}

/// `senate <args>` in `cwd`: whether it succeeded, its stdout, its stderr.
async fn senate(h: &Harness, cwd: &Path, args: &[&str]) -> (bool, String, String) {
    let out = command(h, cwd, args).output().await.unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// `senate terminal --session <as_session> open … -- <program>`, as a
/// coding session's shell runs it: the terminal's id, which is all it
/// prints.
async fn open(
    h: &Harness,
    as_session: &str,
    cwd: &Path,
    options: &[&str],
    program: &[&str],
) -> EventId {
    let mut args = vec!["terminal", "--session", as_session, "open"];
    args.extend_from_slice(options);
    args.push("--");
    args.extend_from_slice(program);
    let (ok, stdout, stderr) = senate(h, cwd, &args).await;
    assert!(ok, "open failed: {stderr}");
    assert_eq!(stderr, "", "open says nothing beside the id");
    let id = stdout.strip_suffix('\n').expect("one line");
    id.parse()
        .unwrap_or_else(|_| panic!("open prints the id alone: {stdout:?}"))
}

/// A terminal request as `session` (the person, without one), over IPC.
async fn ask(
    h: &Harness,
    session: Option<&str>,
    action: TerminalAction,
) -> Result<TerminalReply, ipc::IpcError> {
    let request = Request::Terminal {
        session: session.map(selector).unwrap_or_default(),
        action,
    };
    match ipc::request(&h.socket(), &request, WAIT, Duration::from_secs(30)).await {
        Ok(reply) => Ok(reply),
        Err(ipc::ClientError::Remote(error)) => Err(error),
        Err(error) => panic!("{request:?}: {error}"),
    }
}

async fn screen(h: &Harness, session: Option<&str>, id: &EventId) -> TerminalScreen {
    let action = TerminalAction::Screen {
        target: TerminalTarget::Id(id.clone()),
        scrollback: 0,
    };
    match ask(h, session, action).await.unwrap() {
        TerminalReply::Screen(screen) => screen,
        other => panic!("{other:?}"),
    }
}

async fn wait_for(h: &Harness, session: Option<&str>, id: &EventId, text: &str) -> TerminalScreen {
    let action = TerminalAction::Wait {
        id: id.clone(),
        text: Some(text.to_owned()),
        still_ms: None,
        timeout_ms: 10_000,
    };
    match ask(h, session, action).await.unwrap() {
        TerminalReply::Screen(screen) => {
            assert!(
                !screen.timed_out,
                "never showed {text:?}: {:?}",
                screen.lines
            );
            screen
        }
        other => panic!("{other:?}"),
    }
}

/// Every action that drives or ends a terminal, for `id`.
fn driving(id: &EventId) -> Vec<TerminalAction> {
    vec![
        TerminalAction::Type {
            id: id.clone(),
            text: "y\n".into(),
        },
        TerminalAction::Key {
            id: id.clone(),
            keys: "y enter".into(),
        },
        TerminalAction::Resize {
            id: id.clone(),
            rows: 10,
            cols: 40,
        },
        TerminalAction::Wait {
            id: id.clone(),
            text: None,
            still_ms: Some(10),
            timeout_ms: 100,
        },
        TerminalAction::Close { id: id.clone() },
    ]
}

fn host_dir(h: &Harness, id: &EventId) -> PathBuf {
    h.paths.home().join("hosts").join(id.to_string())
}

fn recorded_pid(h: &Harness, id: &EventId, file: &str) -> u32 {
    std::fs::read_to_string(host_dir(h, id).join(file))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn pid(pid: u32) -> rustix::process::Pid {
    rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap()
}

fn alive(process: u32) -> bool {
    rustix::process::test_kill_process(pid(process)).is_ok()
}

/// The host of `id` and its program are gone, and so is its directory.
async fn gone(h: &Harness, id: &EventId, pids: [u32; 2]) {
    eventually(&format!("terminal {id} closed"), WAIT, || {
        (!host_dir(h, id).exists() && !pids.into_iter().any(alive)).then_some(())
    })
    .await;
}

/// The MCP server as a coding session's client talks to it.
struct Tool {
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    next_id: u64,
}

impl Tool {
    async fn start(h: &Harness, session: &str) -> Self {
        let (client, server) = tokio::io::duplex(1 << 20);
        let mcp = SenateMcp::new(h.socket(), selector(session));
        tokio::spawn(async move {
            let running = mcp.serve(server).await.unwrap();
            let _ = running.waiting().await;
        });
        let (read, writer) = tokio::io::split(client);
        let mut tool = Self {
            lines: BufReader::new(read).lines(),
            writer,
            next_id: 1,
        };
        let init = tool
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "test", "version": "0"}
                }),
            )
            .await;
        assert!(
            init["instructions"]
                .as_str()
                .unwrap()
                .contains("senate terminal open -- <command>"),
            "the server says how a terminal is opened"
        );
        tool.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        tool
    }

    async fn send(&mut self, message: Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let line = tokio::time::timeout(Duration::from_secs(40), self.lines.next_line())
                .await
                .expect("an MCP reply in time")
                .unwrap()
                .expect("the MCP stream open");
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply.get("id") == Some(&json!(id)) {
                assert!(reply.get("error").is_none(), "rpc error: {reply}");
                return reply["result"].clone();
            }
        }
    }

    /// Call `terminal`: whether it failed, and what it said.
    async fn call(&mut self, args: Value) -> (bool, String) {
        let result = self
            .request("tools/call", json!({"name": "terminal", "arguments": args}))
            .await;
        (
            result["isError"].as_bool().unwrap_or(false),
            result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        )
    }

    /// A call that must succeed.
    async fn ok(&mut self, args: Value) -> String {
        let (failed, text) = self.call(args.clone()).await;
        assert!(!failed, "{args}: {text}");
        text
    }

    /// A call that must be refused.
    async fn refused(&mut self, args: Value) -> String {
        let (failed, text) = self.call(args.clone()).await;
        assert!(failed, "{args} was not refused: {text}");
        text
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_scripted_tui_is_driven_end_to_end_through_the_tool() {
    let mut h = daemon(None).await;
    let cwd = session(&h, "driver", "refund-v2").await;
    let menu = script(h.dir.path(), "menu", MENU);
    let id = open(
        &h,
        "driver",
        &cwd,
        &["--rows", "10", "--cols", "40"],
        &[menu.to_str().unwrap()],
    )
    .await;
    let pids = [
        recorded_pid(&h, &id, "host-pid"),
        recorded_pid(&h, &id, "owner"),
    ];

    let mut tool = Tool::start(&h, "driver").await;
    let tools = tool.request("tools/list", json!({})).await;
    let described = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "terminal")
        .expect("every session has the terminal tool");
    assert!(
        described["description"]
            .as_str()
            .unwrap()
            .contains("senate terminal open -- <command>")
    );
    // Opening is the shell's, and the tool says so instead of doing it.
    let no_open = tool
        .refused(json!({"action": "open", "id": id, "text": "sh"}))
        .await;
    assert!(
        no_open.contains("senate terminal open -- <command>"),
        "{no_open}"
    );

    // Wait for the program to paint, rather than poll: for the whole menu
    // (a text to wait for may span rows), then for it to have gone quiet.
    let menu = "Pick one (size 10 40)\n> alpha\n  beta\n  gamma";
    tool.ok(json!({"action": "wait", "id": id, "text": menu}))
        .await;
    let first = tool
        .ok(json!({"action": "wait", "id": id, "still_ms": 200}))
        .await;
    assert!(
        first.starts_with(&format!(
            "terminal {id}: 10 rows × 40 columns, cursor at row 5 column 1, running"
        )),
        "{first}"
    );
    assert!(
        first.contains("Pick one (size 10 40)\n> alpha\n  beta\n  gamma\n</screen"),
        "{first}"
    );

    // Keys answer with a screen; the wait that follows is for a machine
    // too busy to have repainted by then.
    let pressed = tool
        .ok(json!({"action": "key", "id": id, "keys": "down down"}))
        .await;
    assert!(
        pressed.starts_with(&format!("terminal {id}: 10 rows")),
        "{pressed}"
    );
    let moved = tool
        .ok(json!({"action": "wait", "id": id, "text": "> gamma"}))
        .await;
    assert!(moved.contains("  alpha\n  beta\n> gamma"), "{moved}");
    tool.ok(json!({"action": "key", "id": id, "keys": "up"}))
        .await;
    let back = tool
        .ok(json!({"action": "wait", "id": id, "text": "> beta\n  gamma"}))
        .await;
    assert!(back.contains("  alpha\n> beta\n  gamma"), "{back}");
    let unknown = tool
        .refused(json!({"action": "key", "id": id, "keys": "upp"}))
        .await;
    assert!(unknown.contains("unknown key \"upp\""), "{unknown}");
    assert!(
        tool.ok(json!({"action": "screen", "id": id}))
            .await
            .contains("> beta")
    );

    // The program sees the new size.
    let resized = tool
        .ok(json!({"action": "resize", "id": id, "rows": 12, "cols": 50}))
        .await;
    assert!(resized.contains("12 rows × 50 columns"), "{resized}");
    tool.ok(json!({"action": "key", "id": id, "keys": "r"}))
        .await;
    let repainted = tool
        .ok(json!({
            "action": "wait", "id": id,
            "text": "Pick one (size 12 50)\n  alpha\n> beta\n  gamma",
        }))
        .await;
    assert!(repainted.contains("12 rows × 50 columns"), "{repainted}");

    // Enter picks; the program then reads a line, which `type` gives it.
    tool.ok(json!({"action": "key", "id": id, "keys": "enter"}))
        .await;
    tool.ok(json!({"action": "wait", "id": id, "text": "name?"}))
        .await;
    tool.ok(json!({"action": "type", "id": id, "text": "Ada\n"}))
        .await;
    let ended = tool
        .ok(json!({"action": "wait", "id": id, "text": "you picked", "timeout_s": 10}))
        .await;
    assert!(ended.contains("hello Ada, you picked 2"), "{ended}");
    // A wait on a program that has exited returns at once and says so.
    let after = tool
        .ok(json!({"action": "wait", "id": id, "text": "never shown", "timeout_s": 30}))
        .await;
    assert!(after.contains("the program exited (code 3)"), "{after}");
    assert!(!after.contains("Timed out"), "{after}");
    assert!(
        after.contains("hello Ada, you picked 2"),
        "the last screen stays: {after}"
    );
    let late = tool
        .refused(json!({"action": "type", "id": id, "text": "x"}))
        .await;
    assert!(late.contains("has exited (code 3)"), "{late}");
    let listed = tool.ok(json!({"action": "list"})).await;
    assert!(
        listed.contains(&format!("{id}  exited (code 3)")),
        "{listed}"
    );

    let closed = tool.ok(json!({"action": "close", "id": id})).await;
    assert_eq!(closed, format!("Closed {id}."));
    gone(&h, &id, pids).await;
    assert!(
        tool.ok(json!({"action": "list"}))
            .await
            .contains("No scratch terminals")
    );
    let reread = tool.refused(json!({"action": "screen", "id": id})).await;
    assert!(reread.contains("no scratch terminal"), "{reread}");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_runs_with_its_openers_rights_and_outlives_the_command_that_opened_it() {
    let mut h = daemon(None).await;
    let cwd = session(&h, "opener", "refund-v2").await;
    let elsewhere = h.workdir("elsewhere", None);
    // The opener's shell command, in a process group of its own with an
    // environment of its own, as a runtime starts one.
    let mut cmd = command(
        &h,
        &cwd,
        &[
            "terminal",
            "--session",
            "opener",
            "open",
            "--cwd",
            elsewhere.to_str().unwrap(),
            "--",
            "/bin/sh",
            "-c",
            "echo \"$OPENER_ONLY|${SENATE_HOSTED_CLAUDE-unset}|$TERM|$(pwd -P)\"; exec sleep 600",
        ],
    );
    cmd.env("OPENER_ONLY", "from-the-shell")
        .env("SENATE_HOSTED_CLAUDE", "01ARZ3NDEKTSV4RRFFQ69G5FAV")
        .process_group(0)
        .stdout(std::process::Stdio::piped());
    let child = cmd.spawn().unwrap();
    let group = child.id().unwrap();
    let out = child.wait_with_output().await.unwrap();
    assert!(out.status.success());
    let id: EventId = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // The environment and the directory are the opener's, not the daemon's;
    // only the name that would call it a hosted session's TUI is taken out.
    let shown = wait_for(&h, Some("opener"), &id, "from-the-shell").await;
    let expected = format!(
        "from-the-shell|unset|xterm-256color|{}",
        elsewhere.canonicalize().unwrap().display()
    );
    assert_eq!(shown.lines[0], expected);

    // The runtime ends the command's process group: the terminal is not in
    // it, so it stays whole, neither killed nor half alive.
    let host = recorded_pid(&h, &id, "host-pid");
    let program = recorded_pid(&h, &id, "owner");
    assert_eq!(
        rustix::process::kill_process_group(pid(group), rustix::process::Signal::KILL),
        Err(rustix::io::Errno::SRCH),
        "nothing is left in the opener's process group"
    );
    assert!(alive(host) && alive(program));
    let parent = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &host.to_string()])
        .output()
        .unwrap();
    assert_ne!(
        String::from_utf8_lossy(&parent.stdout).trim(),
        std::process::id().to_string(),
        "the daemon is not the terminal's parent: it started nothing"
    );
    assert_eq!(screen(&h, Some("opener"), &id).await.lines[0], expected);

    // It is no hosted session: not an orphan to prune, and no slot taken.
    let sessions: ipc::SessionsReply = ipc::request(&h.socket(), &Request::Sessions, WAIT, WAIT)
        .await
        .unwrap();
    assert!(sessions.orphans.is_empty(), "{:?}", sessions.orphans);
    let pruned: ipc::PruneReply = ipc::request(&h.socket(), &Request::PruneHosts, WAIT, WAIT)
        .await
        .unwrap();
    assert!(pruned.stopped.is_empty() && pruned.removed.is_empty());
    assert!(alive(host) && alive(program));

    // A program that cannot start leaves nothing reserved.
    let (ok, stdout, stderr) = senate(
        &h,
        &cwd,
        &[
            "terminal",
            "--session",
            "opener",
            "open",
            "--",
            "/no/such/program",
        ],
    )
    .await;
    assert!(!ok && stdout.is_empty(), "{stdout}");
    assert!(stderr.contains("the terminal did not start"), "{stderr}");
    assert!(stderr.contains("/no/such/program"), "{stderr}");
    match ask(&h, Some("opener"), TerminalAction::List).await.unwrap() {
        TerminalReply::List { terminals } => assert_eq!(terminals.len(), 1, "{terminals:?}"),
        other => panic!("{other:?}"),
    }
    h.stop().await;
    drop(h);
    eventually("a dropped harness to leave no host behind", WAIT, || {
        (!alive(host) && !alive(program)).then_some(())
    })
    .await;
}

/// A sandbox the opener's shell command runs in holds for the terminal
/// too, although its host has left that command's session and outlived it.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_opened_inside_a_sandbox_stays_inside_it() {
    let sandboxed = |profile: &str| {
        let mut cmd = tokio::process::Command::new("/usr/bin/sandbox-exec");
        cmd.args(["-p", profile]);
        cmd
    };
    // Nested sandboxes are refused where this test itself runs in one.
    let usable = sandboxed("(version 1)(allow default)")
        .arg("/usr/bin/true")
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|status| status.success());
    if !usable {
        eprintln!("skipped: sandbox-exec cannot apply a profile here");
        return;
    }
    let mut h = daemon(None).await;
    let cwd = session(&h, "boxed", "refund-v2").await;
    let forbidden = h.dir.path().canonicalize().unwrap().join("forbidden");
    std::fs::create_dir(&forbidden).unwrap();
    let profile = format!(
        "(version 1)(allow default)(deny file-write* (subpath {:?}))",
        forbidden.to_str().unwrap()
    );
    let mut cmd = sandboxed(&profile);
    testenv::isolate_async(&mut cmd, h.dir.path());
    let out = cmd
        .args([
            BIN,
            "terminal",
            "--session",
            "boxed",
            "open",
            "--",
            "/bin/sh",
        ])
        .current_dir(&cwd)
        .env("SENATE_HOME", h.paths.home())
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id: EventId = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // The opening command is long gone; what is typed now still runs in
    // its sandbox.
    let attempt = format!(
        "touch {0}/x; echo \"denied=$?\"; touch {1}/y; echo \"allowed=$?\"\n",
        forbidden.display(),
        cwd.display()
    );
    ask(
        &h,
        Some("boxed"),
        TerminalAction::Type {
            id: id.clone(),
            text: attempt,
        },
    )
    .await
    .unwrap();
    // The answer, not the echo of what was typed.
    let shown = wait_for(&h, Some("boxed"), &id, "allowed=0").await;
    let text = shown.lines.join("\n");
    assert!(text.contains("denied=1"), "{text}");
    assert!(text.contains("allowed=0"), "{text}");
    assert!(!forbidden.join("x").exists());
    assert!(cwd.join("y").exists());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminal_is_its_openers_alone_and_a_person_sees_them_all() {
    let mut h = daemon(None).await;
    let cwd = session(&h, "owner", "refund-v2").await;
    session(&h, "member", "refund-v2").await;
    // The chair of the committee has no more right to it than a member.
    session(&h, "chair", "refund-v2").await;
    h.server.state.inner.lock().unwrap().chair = Some((path("claude-chair"), true));
    let id = open(&h, "owner", &cwd, &[], &["/bin/cat"]).await;

    for other in ["member", "chair"] {
        let read = TerminalAction::Screen {
            target: TerminalTarget::Id(id.clone()),
            scrollback: 0,
        };
        for action in driving(&id).into_iter().chain([read]) {
            let refused = ask(&h, Some(other), action.clone()).await.unwrap_err();
            assert_eq!(refused.code, ipc::codes::NO_SESSION, "{action:?}");
            assert!(
                refused.message.contains("no scratch terminal")
                    && refused.message.contains("of yours"),
                "{action:?}: {}",
                refused.message
            );
        }
        match ask(&h, Some(other), TerminalAction::List).await.unwrap() {
            TerminalReply::List { terminals } => assert!(terminals.is_empty(), "{terminals:?}"),
            reply => panic!("{reply:?}"),
        }
    }
    // A session nobody registered is nobody.
    let stranger = ask(&h, Some("stranger"), TerminalAction::List)
        .await
        .unwrap_err();
    assert_eq!(stranger.code, ipc::codes::NO_SESSION);

    // Nothing the others asked reached the program: the first thing on
    // its screen is the owner's line, and cat's echo of it.
    ask(
        &h,
        Some("owner"),
        TerminalAction::Type {
            id: id.clone(),
            text: "mine\n".into(),
        },
    )
    .await
    .unwrap();
    let shown = wait_for(&h, Some("owner"), &id, "mine\nmine").await;
    assert_eq!(shown.lines[..2], ["mine", "mine"]);

    // The person, from their own terminal: every terminal, whose it is,
    // and its screen. Nothing runs hidden.
    let (ok, listing, error) = senate(&h, h.dir.path(), &["terminal", "list"]).await;
    assert!(ok, "{error}");
    assert!(listing.contains(&format!("{id}  running")), "{listing}");
    assert!(
        listing.contains("claude-owner") && listing.contains("/bin/cat"),
        "{listing}"
    );
    let (ok, shown, error) =
        senate(&h, h.dir.path(), &["terminal", "screen", &id.to_string()]).await;
    assert!(ok && shown.contains("\nmine\nmine\n"), "{shown}{error}");
    // …and may end one.
    let pids = [
        recorded_pid(&h, &id, "host-pid"),
        recorded_pid(&h, &id, "owner"),
    ];
    let (ok, _, error) = senate(&h, h.dir.path(), &["terminal", "close", &id.to_string()]).await;
    assert!(ok, "{error}");
    gone(&h, &id, pids).await;
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_chair_reads_a_senators_screen_and_no_agent_types_into_one() {
    let mut h = daemon(Some(STUCK_SENATOR)).await;
    let project = h.workdir("senator", Some("refund-v2"));
    let _: SpawnReply = ipc::request(
        &h.socket(),
        &Request::Spawn {
            attempt_id: None,
            session: Default::default(),
            runtime: Some(HostedRuntime::Claude),
            profile: None,
            committee: committee(),
            alias: Some(path("stuck")),
            cwd: project.clone(),
            chair: false,
            secretary: false,
            prompt: None,
            model: Default::default(),
        },
        WAIT,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    let rows: ipc::SessionsReply = ipc::request(&h.socket(), &Request::Sessions, WAIT, WAIT)
        .await
        .unwrap();
    let hosted = rows.rows[0].host_id.clone();
    let typed = project.join(".typed");

    session(&h, "chair", "refund-v2").await;
    session(&h, "member", "refund-v2").await;
    session(&h, "outsider", "other").await;
    h.server.state.inner.lock().unwrap().chair = Some((path("claude-chair"), true));
    let read = |alias: &str| TerminalAction::Screen {
        target: TerminalTarget::Session(senate_bridge::mcp::parse_alias_name(alias).unwrap()),
        scrollback: 0,
    };

    // The chair reads what the senator shows, marked as untrusted text.
    let mut chair = Tool::start(&h, "chair").await;
    let mut shown = String::new();
    for _ in 0..50 {
        shown = chair
            .ok(json!({"action": "screen", "session": "stuck"}))
            .await;
        if shown.contains("Allow rm -rf build?") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        shown.starts_with("screen of refund-v2::stuck: 24 rows × 80 columns"),
        "{shown}"
    );
    let warning = shown
        .find("Untrusted: between the markers")
        .expect("marked untrusted");
    assert!(
        warning < shown.find("Allow rm -rf build?").unwrap(),
        "{shown}"
    );

    // A member does not.
    let refused = ask(&h, Some("member"), read("stuck")).await.unwrap_err();
    assert_eq!(refused.code, ipc::codes::NOT_CHAIRING);
    assert!(
        refused.message.contains("only the chair of refund-v2"),
        "{}",
        refused.message
    );
    // Nor does the chair of another committee, by any name.
    h.server.state.inner.lock().unwrap().chair = Some((path("claude-outsider"), true));
    let refused = ask(&h, Some("outsider"), read("refund-v2::stuck"))
        .await
        .unwrap_err();
    assert_eq!(refused.code, ipc::codes::NOT_CHAIRING);
    assert!(
        refused.message.contains("is not on other"),
        "{}",
        refused.message
    );
    assert!(ask(&h, Some("outsider"), read("stuck")).await.is_err());
    h.server.state.inner.lock().unwrap().chair = Some((path("claude-chair"), true));

    // Nobody drives it: not the chair, not a member, not through the tool,
    // not by the session's host id, which is no scratch terminal.
    for who in ["chair", "member", "outsider"] {
        for action in driving(&hosted) {
            let refused = ask(&h, Some(who), action.clone()).await.unwrap_err();
            assert_eq!(refused.code, ipc::codes::NO_SESSION, "{who}: {action:?}");
            assert!(
                refused.message.contains("cannot be driven"),
                "{}",
                refused.message
            );
        }
    }
    for args in [
        json!({"action": "type", "id": hosted, "text": "y\n"}),
        json!({"action": "key", "id": hosted, "keys": "y enter"}),
        json!({"action": "type", "session": "stuck", "text": "y\n"}),
        json!({"action": "key", "session": "stuck", "keys": "y enter"}),
        json!({"action": "close", "session": "stuck"}),
    ] {
        chair.refused(args).await;
    }
    // Not even with a record forged into the session's host directory
    // that names the chair as its opener…
    std::fs::write(
        host_dir(&h, &hosted).join("scratch.json"),
        json!({"owner": "chair", "command": ["sh"], "cwd": "/", "opened_at": "2026-01-01T00:00:00Z"})
            .to_string(),
    )
    .unwrap();
    for action in driving(&hosted) {
        let refused = ask(&h, Some("chair"), action.clone()).await.unwrap_err();
        assert_eq!(refused.code, ipc::codes::NO_SESSION, "{action:?}");
    }
    // …or names a session that is gone, which would have a real scratch
    // terminal closed by the sweep: a session's host is never one.
    std::fs::write(
        host_dir(&h, &hosted).join("scratch.json"),
        json!({"owner": "long-gone", "command": ["sh"], "cwd": "/", "opened_at": "2026-01-01T00:00:00Z"})
            .to_string(),
    )
    .unwrap();
    tokio::time::sleep(fast_timing().sweep * 3).await;
    // …and a session's host has no input request at all: asked as the
    // daemon itself, on its control socket, it answers nothing.
    let mut control = tokio::net::UnixStream::connect(host_dir(&h, &hosted).join("ctl"))
        .await
        .unwrap();
    control
        .write_all(b"{\"scratch\":{\"text\":\"y\\n\"}}\n")
        .await
        .unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), control.read_to_end(&mut answer)).await;
    assert!(answer.is_empty(), "{answer:?}");

    // The senator is still there, still asking, and was typed nothing.
    let after = chair
        .ok(json!({"action": "screen", "session": "stuck"}))
        .await;
    assert!(after.contains("Allow rm -rf build?"), "{after}");
    assert!(!typed.exists(), "{:?}", std::fs::read_to_string(&typed));
    let rows: ipc::SessionsReply = ipc::request(&h.socket(), &Request::Sessions, WAIT, WAIT)
        .await
        .unwrap();
    assert!(rows.rows[0].is_running());
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminals_are_limited_per_session_and_close_when_their_session_ends() {
    let mut h = daemon(None).await;
    let one = session(&h, "one", "refund-v2").await;
    let two = session(&h, "two", "refund-v2").await;
    let mut mine = Vec::new();
    for _ in 0..4 {
        mine.push(open(&h, "one", &one, &[], &["/bin/cat"]).await);
    }
    // The fifth is refused, naming what to close; another session's count
    // is its own.
    let (ok, stdout, stderr) = senate(
        &h,
        &one,
        &["terminal", "--session", "one", "open", "--", "/bin/cat"],
    )
    .await;
    assert!(!ok && stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("4 scratch terminals are open already"),
        "{stderr}"
    );
    assert!(stderr.contains(&mine[0].to_string()), "{stderr}");
    let theirs = open(&h, "two", &two, &[], &["/bin/cat"]).await;
    // Closing one makes room.
    let pids = [
        recorded_pid(&h, &mine[3], "host-pid"),
        recorded_pid(&h, &mine[3], "owner"),
    ];
    ask(
        &h,
        Some("one"),
        TerminalAction::Close {
            id: mine[3].clone(),
        },
    )
    .await
    .unwrap();
    gone(&h, &mine[3], pids).await;
    mine[3] = open(&h, "one", &one, &[], &["/bin/cat"]).await;

    // The session ends: everything it opened goes with it, at once.
    let pids: Vec<[u32; 2]> = mine
        .iter()
        .map(|id| {
            [
                recorded_pid(&h, id, "host-pid"),
                recorded_pid(&h, id, "owner"),
            ]
        })
        .collect();
    let _: Value = ipc::request(
        &h.socket(),
        &Request::SessionEnd {
            session_id: "one".into(),
            reason: None,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    for (id, pids) in mine.iter().zip(pids) {
        gone(&h, id, pids).await;
    }
    assert_eq!(
        screen(&h, Some("two"), &theirs).await.id,
        Some(theirs.clone())
    );

    // A session that dies without saying so: its process is gone, the
    // sweep finds it, and its terminal is closed.
    let mut runtime = tokio::process::Command::new("/bin/sleep")
        .arg("600")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let three = session_with(&h, "three", "refund-v2", runtime.id()).await;
    let orphaned = open(&h, "three", &three, &[], &["/bin/cat"]).await;
    let pids = [
        recorded_pid(&h, &orphaned, "host-pid"),
        recorded_pid(&h, &orphaned, "owner"),
    ];
    runtime.kill().await.unwrap();
    gone(&h, &orphaned, pids).await;
    assert_eq!(screen(&h, Some("two"), &theirs).await.id, Some(theirs));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_who_takes_a_terminal_over_holds_its_keyboard() {
    let mut h = daemon(None).await;
    let cwd = session(&h, "owner", "refund-v2").await;
    let id = open(&h, "owner", &cwd, &[], &["/bin/cat"]).await;
    let type_line = |text: &str| TerminalAction::Type {
        id: id.clone(),
        text: text.to_owned(),
    };
    ask(&h, Some("owner"), type_line("before\n")).await.unwrap();

    // A person attaches, as `senate terminal attach` does.
    let mut person = senate_bridge::host::open_fixture_viewer(&host_dir(&h, &id).join("tty"), true)
        .await
        .unwrap();
    assert_eq!(person.read_u8().await.unwrap(), 3);
    person.write_all(b"\x01\0\x18\0\x50").await.unwrap();
    eventually("the terminal to show as attached", WAIT, || {
        std::fs::read_to_string(host_dir(&h, &id).join("viewer.json"))
            .is_ok_and(|viewer| viewer.contains("\"attached\":true"))
            .then_some(())
    })
    .await;

    // The owner's input is refused, in words that stop a retry; reading
    // still works and says who has the keyboard.
    for action in [
        type_line("agent\n"),
        TerminalAction::Key {
            id: id.clone(),
            keys: "enter".into(),
        },
        TerminalAction::Resize {
            id: id.clone(),
            rows: 10,
            cols: 40,
        },
    ] {
        let refused = ask(&h, Some("owner"), action).await.unwrap_err();
        assert!(
            refused.message.contains("a person is attached")
                && refused.message.contains("Do not retry"),
            "{}",
            refused.message
        );
    }
    let shown = screen(&h, Some("owner"), &id).await;
    assert!(shown.attached);
    assert_eq!(
        (shown.rows, shown.cols),
        (24, 80),
        "the size is the person's too"
    );
    let (_, listing, _) = senate(&h, &cwd, &["terminal", "--session", "owner", "list"]).await;
    assert!(listing.contains("a person is attached"), "{listing}");

    // What the person types reaches the program.
    person.write_all(b"\x02\0\x07person\r").await.unwrap();
    let shown = wait_for(&h, Some("owner"), &id, "person\nperson").await;
    assert!(
        !shown.lines.join("\n").contains("agent"),
        "{:?}",
        shown.lines
    );

    // They leave: the keyboard is the owner's again.
    drop(person);
    eventually("the person to have detached", WAIT, || {
        std::fs::read_to_string(host_dir(&h, &id).join("viewer.json"))
            .is_ok_and(|viewer| viewer.contains("\"attached\":false"))
            .then_some(())
    })
    .await;
    ask(&h, Some("owner"), type_line("after\n")).await.unwrap();
    let shown = wait_for(&h, Some("owner"), &id, "after\nafter").await;
    assert!(!shown.attached);
    h.stop().await;
}

/// `senate terminal <args>` on a terminal of its own, as a person runs it:
/// what it paints, where to type into it, and the command itself.
fn in_a_terminal(
    h: &Harness,
    args: &[&str],
) -> (
    pty_process::OwnedReadPty,
    pty_process::OwnedWritePty,
    tokio::process::Child,
) {
    let mut cmd = command(h, h.dir.path(), args);
    cmd.kill_on_drop(true);
    let (pty, pts) = pty_process::open().unwrap();
    pty.resize(pty_process::Size::new(24, 80)).unwrap();
    let child = pty_process::Command::from(cmd).spawn(pts).unwrap();
    let (output, input) = pty.into_split();
    (output, input, child)
}

/// Read what a terminal is painted until it has been painted `text`,
/// answering the attach command's question about the terminal on the way.
async fn painted(
    output: &mut pty_process::OwnedReadPty,
    input: &mut pty_process::OwnedWritePty,
    text: &str,
) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let mut answered = false;
    let reading = async {
        while !bytes
            .windows(text.len())
            .any(|part| part == text.as_bytes())
        {
            let n = output.read(&mut chunk).await.unwrap();
            assert_ne!(n, 0, "the command left before painting {text:?}");
            bytes.extend_from_slice(&chunk[..n]);
            if !answered && bytes.windows(3).any(|part| part == b"\x1b[c") {
                input.write_all(b"\x1b[?62;22c").await.unwrap();
                answered = true;
            }
        }
    };
    tokio::time::timeout(WAIT, reading)
        .await
        .unwrap_or_else(|_| panic!("never painted {text:?}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_person_looks_at_a_terminal_or_takes_it_over_from_their_own() {
    let mut h = daemon(None).await;
    let cwd = session(&h, "owner", "refund-v2").await;
    let id = open(&h, "owner", &cwd, &[], &["/bin/cat"]).await;
    let name = id.to_string();
    let type_line = |text: &str| TerminalAction::Type {
        id: id.clone(),
        text: text.to_owned(),
    };
    ask(&h, Some("owner"), type_line("from the agent\n"))
        .await
        .unwrap();
    wait_for(&h, Some("owner"), &id, "from the agent\nfrom the agent").await;

    // Taking it over: the person sees what is there, and types.
    let (mut output, mut input, mut attached) = in_a_terminal(&h, &["terminal", "attach", &name]);
    painted(&mut output, &mut input, "from the agent").await;
    let refused = ask(&h, Some("owner"), type_line("too late\n"))
        .await
        .unwrap_err();
    assert!(
        refused.message.contains("a person is attached"),
        "{}",
        refused.message
    );
    input.write_all(b"by hand\r").await.unwrap();
    wait_for(&h, Some("owner"), &id, "by hand\nby hand").await;
    input.write_all(b"\x1d").await.unwrap();
    let draining = tokio::spawn(async move {
        let mut buf = [0; 4096];
        while output.read(&mut buf).await.is_ok_and(|n| n > 0) {}
    });
    let left = tokio::time::timeout(WAIT, attached.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(left.success(), "Ctrl-] detaches");
    draining.abort();

    // Only looking: what is typed there goes nowhere, and the owner keeps
    // the keyboard.
    let (mut output, mut input, mut looking) =
        in_a_terminal(&h, &["terminal", "attach", "--read-only", &name]);
    painted(&mut output, &mut input, "by hand").await;
    input.write_all(b"not typed\r").await.unwrap();
    eventually("the owner to have the keyboard back", WAIT, || {
        std::fs::read_to_string(host_dir(&h, &id).join("viewer.json"))
            .is_ok_and(|viewer| viewer.contains("\"attached\":false"))
            .then_some(())
    })
    .await;
    ask(&h, Some("owner"), type_line("still mine\n"))
        .await
        .unwrap();
    let shown = wait_for(&h, Some("owner"), &id, "still mine\nstill mine").await;
    assert!(
        !shown.lines.join("\n").contains("not typed"),
        "{:?}",
        shown.lines
    );
    assert!(
        !shown.lines.join("\n").contains("too late"),
        "{:?}",
        shown.lines
    );
    painted(&mut output, &mut input, "still mine").await;
    input.write_all(b"\x1d").await.unwrap();
    let draining = tokio::spawn(async move {
        let mut buf = [0; 4096];
        while output.read(&mut buf).await.is_ok_and(|n| n > 0) {}
    });
    let left = tokio::time::timeout(WAIT, looking.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(left.success(), "Ctrl-] leaves");
    draining.abort();

    // An id that is no scratch terminal is not attached to.
    let (ok, _, error) = senate(
        &h,
        h.dir.path(),
        &[
            "terminal",
            "attach",
            "--read-only",
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
        ],
    )
    .await;
    assert!(
        !ok && error.contains("no running scratch terminal"),
        "{error}"
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wait_ends_on_stillness_on_any_change_or_on_time_and_scrollback_is_kept() {
    let mut h = daemon(None).await;
    let cwd = session(&h, "waiter", "refund-v2").await;
    // Counts to 30, a line every 20 ms, then waits for a line and echoes it.
    let id = open(
        &h,
        "waiter",
        &cwd,
        &["--rows", "5", "--cols", "30"],
        &[
            "/bin/sh",
            "-c",
            "i=1; while [ $i -le 30 ]; do echo line-$i; i=$((i+1)); sleep 0.02; done; read x; echo got-$x; exec sleep 600",
        ],
    )
    .await;
    let wait = |text: Option<&str>, still_ms: Option<u64>, timeout_ms: u64| TerminalAction::Wait {
        id: id.clone(),
        text: text.map(str::to_owned),
        still_ms,
        timeout_ms,
    };
    let shown = |reply: TerminalReply| match reply {
        TerminalReply::Screen(screen) => screen,
        other => panic!("{other:?}"),
    };

    // Still for 300 ms: only once the count is over.
    let still = shown(
        ask(&h, Some("waiter"), wait(None, Some(300), 10_000))
            .await
            .unwrap(),
    );
    assert!(!still.timed_out);
    assert_eq!(still.lines[3], "line-30", "{:?}", still.lines);
    // The rows that scrolled off are there to ask for, oldest first.
    let history = ask(
        &h,
        Some("waiter"),
        TerminalAction::Screen {
            target: TerminalTarget::Id(id.clone()),
            scrollback: 100,
        },
    )
    .await
    .unwrap();
    let history = shown(history);
    assert_eq!(
        history.scrollback.first().map(String::as_str),
        Some("line-1")
    );
    assert_eq!(history.scrollback.len(), 26, "{:?}", history.scrollback);

    // Nothing changes: a wait for change, and one for a text, time out and
    // say so, with the screen as it is.
    let unchanged = shown(
        ask(&h, Some("waiter"), wait(None, None, 300))
            .await
            .unwrap(),
    );
    assert!(unchanged.timed_out);
    assert_eq!(unchanged.lines[3], "line-30");
    let absent = shown(
        ask(&h, Some("waiter"), wait(Some("got-"), None, 300))
            .await
            .unwrap(),
    );
    assert!(absent.timed_out);
    let (ok, stdout, stderr) = senate(
        &h,
        &cwd,
        &[
            "terminal",
            "--session",
            "waiter",
            "wait",
            &id.to_string(),
            "--text",
            "got-",
            "--timeout",
            "1",
        ],
    )
    .await;
    assert!(!ok, "a wait that runs out fails the command");
    assert!(
        stdout.starts_with("Timed out:") && stdout.contains("line-30"),
        "{stdout}"
    );
    assert!(stderr.contains("did not happen in 1 s"), "{stderr}");
    for bad in [wait(Some("a"), Some(1), 100), wait(Some(""), None, 100)] {
        assert_eq!(
            ask(&h, Some("waiter"), bad).await.unwrap_err().code,
            ipc::codes::BAD_REQUEST
        );
    }

    // A change made while its owner was not waiting is not missed. The
    // person types a line from their own terminal and reads the result;
    // the owner's wait for "anything else than I was last shown" then
    // returns at once, because what the person read is not what the owner
    // was shown.
    let (ok, _, error) = senate(
        &h,
        h.dir.path(),
        &["terminal", "type", &id.to_string(), "it\n"],
    )
    .await;
    assert!(ok, "{error}");
    let (ok, seen, error) = senate(
        &h,
        h.dir.path(),
        &["terminal", "wait", &id.to_string(), "--text", "got-it"],
    )
    .await;
    assert!(ok && seen.contains("got-it"), "{seen}{error}");
    let began = std::time::Instant::now();
    let changed = shown(
        ask(&h, Some("waiter"), wait(None, None, 10_000))
            .await
            .unwrap(),
    );
    assert!(!changed.timed_out);
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    assert!(
        changed.lines.iter().any(|line| line == "got-it"),
        "{:?}",
        changed.lines
    );
    h.stop().await;
}
