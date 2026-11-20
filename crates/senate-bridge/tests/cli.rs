//! The `senate` binary itself. Every spawn gets a temp HOME and
//! SENATE_HOME and has the CLAUDE_* session variables removed, so nothing
//! can reach the real Claude Code configuration or a real inbox socket.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::*;
use senate_bridge::state::ItemStatus;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_senate");

fn bridge(home: &Path, senate_home: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    senate_bridge::testenv::isolate(&mut cmd, home);
    // The full form, which most of these tests read; `brief` is the default.
    cmd.env("HOME", home)
        .env("SENATE_HOME", senate_home)
        .env("SENATE_VERBOSE", "1");
    cmd
}

/// As a person runs it day to day: the brief output.
fn brief(home: &Path, senate_home: &Path) -> Command {
    let mut cmd = bridge(home, senate_home);
    cmd.env_remove("SENATE_VERBOSE");
    cmd
}

fn run_with_stdin(mut cmd: Command, stdin: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn hook_input(session: &str, cwd: &Path, event: &str) -> String {
    json!({"session_id": session, "cwd": cwd, "hook_event_name": event, "source": "startup"})
        .to_string()
}

#[test]
fn hooks_exit_zero_and_skip_git_without_a_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let git = dir.path().join("git");
    let git_started = dir.path().join("git-started");
    std::fs::write(&git, "#!/bin/sh\n: > \"$TEST_HOOK_GIT_WITNESS\"\nexit 1\n").unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
    let hook = |sub: &str| {
        let mut cmd = bridge(dir.path(), &dir.path().join("senate"));
        cmd.args(["hook", sub])
            .env("PATH", dir.path())
            .env("TEST_HOOK_GIT_WITNESS", &git_started);
        cmd
    };
    for (sub, event) in [
        ("session-start", "SessionStart"),
        ("user-prompt-submit", "UserPromptSubmit"),
        ("session-end", "SessionEnd"),
    ] {
        let out = run_with_stdin(hook(sub), &hook_input("s", dir.path(), event));
        assert!(out.status.success(), "{sub}: {out:?}");
        assert!(
            out.stdout.is_empty(),
            "{sub} printed {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(out.stderr.is_empty(), "{sub}: {out:?}");
        assert!(
            !git_started.exists(),
            "{sub} ran git before contacting the daemon"
        );
    }
    let out = run_with_stdin(hook("user-prompt-submit"), "this is not json");
    assert!(out.status.success() && out.stdout.is_empty());
    assert!(
        !senate_bridge::Paths::new(dir.path().join("senate"))
            .hook_drops_log()
            .exists()
    );
}

#[tokio::test]
async fn a_hook_timeout_leaves_a_bounded_local_trace_without_a_daemon_reply() {
    use senate_bridge::hook::STDIN_BUDGET;
    use tokio::io::AsyncBufReadExt as _;

    let dir = tempfile::tempdir().unwrap();
    let paths = senate_bridge::Paths::new(dir.path().join("senate"));
    paths.ensure_home().unwrap();

    for stalled in [false, true] {
        let listener = if stalled {
            // The next append must stay under the cap even if the file is full.
            std::fs::write(paths.hook_drops_log(), vec![b'x'; 64 * 1024]).unwrap();
            Some(tokio::net::UnixListener::bind(paths.socket()).unwrap())
        } else {
            None
        };
        let mut cmd = tokio::process::Command::from(bridge(dir.path(), paths.home()));
        cmd.args(["hook", "session-end", "--codex"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let began = std::time::Instant::now();
        let mut child = cmd.spawn().unwrap();
        let _hold_stdin_open = child.stdin.take().unwrap();
        // The unit test pins the internal deadlines. This watchdog keeps an
        // unresponsive child from holding the subprocess fixture open.
        let out = tokio::time::timeout(WAIT, async {
            let _unanswered = if let Some(listener) = listener {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["op"], "hook_dropped");
                Some(reader)
            } else {
                None
            };
            child.wait_with_output().await.unwrap()
        })
        .await
        .expect("the hook exited with stdin held open");
        assert!(began.elapsed() >= STDIN_BUDGET);
        assert!(
            out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
            "{out:?}"
        );
        let log = std::fs::read_to_string(paths.hook_drops_log()).unwrap();
        assert!(log.len() <= 64 * 1024);
        let lines: Vec<_> = log.lines().collect();
        assert_eq!(lines.len(), 1);
        let drop: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(drop["reason"], "stdin_timeout");
        assert_eq!(drop["report"]["kind"], "session_end");
        assert_eq!(drop["report"]["runtime_hint"], "codex");
        assert_eq!(drop["report"]["parent_pid"], std::process::id());
        assert_eq!(
            std::fs::metadata(paths.hook_drops_log())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn a_hook_timeout_does_not_create_a_missing_senate_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("senate");
    let mut cmd = tokio::process::Command::from(bridge(dir.path(), &home));
    let mut child = cmd
        .args(["hook", "session-start"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let _hold_stdin_open = child.stdin.take().unwrap();
    let out = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
        "{out:?}"
    );
    assert!(!home.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_hook_binary_registers_and_returns_queued_items() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("b", Some("refund-v2"));
    let home = h.dir.path().to_path_buf();
    let senate = h.paths.home().to_path_buf();

    let start = {
        let mut cmd = bridge(&home, &senate);
        cmd.args(["hook", "session-start"])
            .env("CLAUDE_PID", std::process::id().to_string());
        let input = hook_input("b1", &wd, "SessionStart");
        tokio::task::spawn_blocking(move || run_with_stdin(cmd, &input))
            .await
            .unwrap()
    };
    assert!(start.status.success());
    let out: Value = serde_json::from_slice(&start.stdout).unwrap();
    assert_eq!(out["hookSpecificOutput"]["hookEventName"], "SessionStart");
    let s = h.store().session("b1").unwrap().unwrap();
    assert_eq!(s.claude_pid, Some(std::process::id()));
    assert_eq!(s.socket, None, "no inbox socket in this environment");
    let status: senate_bridge::ipc::StatusReply = senate_bridge::ipc::request(
        &h.socket(),
        &senate_bridge::ipc::Request::Status,
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    assert!(status.dropped_hooks.is_none());
    assert!(!h.paths.hook_drops_log().exists());

    h.wait_stream_of("refund-v2", "claude-b1").await;
    h.server
        .append(relayed_to(senator("claude-b1"), "via the real binary"));
    eventually("held for hooks", WAIT, || {
        let i = h.item_of("claude-b1", 1)?;
        (i.status == ItemStatus::Queued).then_some(())
    })
    .await;
    let prompt = {
        let mut cmd = bridge(&home, &senate);
        cmd.args(["hook", "user-prompt-submit"])
            .env("CLAUDE_PID", std::process::id().to_string());
        let input = hook_input("b1", &wd, "UserPromptSubmit");
        tokio::task::spawn_blocking(move || run_with_stdin(cmd, &input))
            .await
            .unwrap()
    };
    assert!(prompt.status.success());
    let out: Value = serde_json::from_slice(&prompt.stdout).unwrap();
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert!(context(&out).contains("via the real binary"));
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_push_writes_the_inbox_framing() {
    let dir = tempfile::tempdir().unwrap();
    let inbox = FakeInbox::bind(dir.path().join("inbox.sock"));
    let mut cmd = bridge(dir.path(), &dir.path().join("senate"));
    cmd.args(["debug", "push", "--socket"])
        .arg(&inbox.path)
        .args(["--token", "t0k", "--text", "hello\nworld"]);
    let out = tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stdout).contains("pushed 11 chars"));
    let conns = eventually("the push", WAIT, || {
        let c = inbox.connections();
        (!c.is_empty()).then_some(c)
    })
    .await;
    assert_eq!(conns[0][0], r#"{"type":"auth","token":"t0k"}"#);
    assert_eq!(
        conns[0][1],
        r#"{"type":"user","message":{"role":"user","content":"hello\nworld"}}"#
    );

    let mut no_token = bridge(dir.path(), &dir.path().join("senate"));
    no_token
        .args(["debug", "push", "--socket"])
        .arg(&inbox.path)
        .args(["--text", "x"]);
    let out = tokio::task::spawn_blocking(move || no_token.output().unwrap())
        .await
        .unwrap();
    assert!(!out.status.success(), "a token is required");
}

fn fake_claude(dir: &Path) -> (PathBuf, PathBuf) {
    let log = dir.join("claude.log");
    let bin = dir.join("fake-claude");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s|%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$*\" >> '{}'\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    (bin, log)
}

#[test]
fn install_is_a_dry_run_without_yes_and_idempotent_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let claude_dir = dir.path().join("claude-config");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let settings = claude_dir.join("settings.json");
    let original = json!({
        "model": "opus",
        "permissions": {"allow": ["Bash(ls:*)"]},
        "hooks": {"Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]}
    });
    std::fs::write(&settings, serde_json::to_string_pretty(&original).unwrap()).unwrap();
    let before = std::fs::read(&settings).unwrap();
    let (claude_bin, log) = fake_claude(dir.path());
    let exe = std::fs::canonicalize(BIN).unwrap();
    let install = |yes: bool| {
        let mut cmd = bridge(dir.path(), &dir.path().join("senate"));
        cmd.args(["install", "--no-codex"])
            .arg("--claude-dir")
            .arg(&claude_dir)
            .arg("--claude-bin")
            .arg(&claude_bin);
        if yes {
            cmd.arg("--yes");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };

    // Dry run: shows everything, writes nothing.
    let text = install(false);
    assert!(text.contains("dry run"), "{text}");
    assert!(
        text.contains(&format!(
            "will run: CLAUDE_CONFIG_DIR={} {} mcp add --scope user senate -- {} mcp",
            claude_dir.display(),
            claude_bin.display(),
            exe.display()
        )),
        "{text}"
    );
    assert!(text.contains(&format!(
        "+ hooks.SessionStart: {} hook session-start",
        exe.display()
    )));
    assert!(text.contains("+ hooks.UserPromptSubmit"));
    assert!(text.contains("+ hooks.SessionEnd"));
    assert!(text.contains("+ crossSessionInbound: \"accept\""));
    assert!(text.contains("dropped after 5 minutes"));
    assert!(text.contains("Nothing was written"));
    assert_eq!(std::fs::read(&settings).unwrap(), before);
    assert!(!log.exists(), "dry run must not call claude");
    let command = claude_dir.join("commands").join("senate.md");
    assert!(
        text.contains("will write: the /senate slash command"),
        "{text}"
    );
    assert!(!command.exists(), "dry run writes no command");

    // --yes: merged, other keys and hooks kept, MCP registered via the CLI.
    let text = install(true);
    assert!(text.contains("Done."), "{text}");
    let merged: Value = serde_json::from_slice(&std::fs::read(&settings).unwrap()).unwrap();
    assert_eq!(merged["model"], "opus");
    assert_eq!(merged["permissions"], original["permissions"]);
    assert_eq!(merged["hooks"]["Stop"], original["hooks"]["Stop"]);
    assert_eq!(merged["crossSessionInbound"], "accept");
    for (event, sub) in [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "user-prompt-submit"),
        ("SessionEnd", "session-end"),
    ] {
        let groups = merged["hooks"][event].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["hooks"][0]["type"], "command");
        assert_eq!(
            groups[0]["hooks"][0]["command"],
            format!("{} hook {sub}", exe.display())
        );
    }
    assert_eq!(
        std::fs::read(claude_dir.join("settings.json.bak")).unwrap(),
        before
    );
    let md = std::fs::read_to_string(&command).unwrap();
    assert!(
        md.contains("mcp__senate__inbox") && md.contains("take <seq>"),
        "{md}"
    );
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        calls.trim(),
        format!(
            "{}|mcp add --scope user senate -- {} mcp",
            claude_dir.display(),
            exe.display()
        )
    );

    // Claude records the server where install looks for it; a second run is a no-op.
    std::fs::write(
        claude_dir.join(".claude.json"),
        json!({"mcpServers": {"senate": {"type": "stdio", "command": exe, "args": ["mcp"]}}})
            .to_string(),
    )
    .unwrap();
    let after_first = std::fs::read(&settings).unwrap();
    let text = install(true);
    assert!(text.contains("Nothing to do: already installed."), "{text}");
    assert!(text.contains("already registered"));
    assert!(text.contains("already up to date"));
    assert_eq!(
        std::fs::read(&settings).unwrap(),
        after_first,
        "byte-identical"
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        calls,
        "claude not called again"
    );

    // A /senate command of the user's own is not overwritten.
    std::fs::write(&command, "my own /senate").unwrap();
    let text = install(true);
    assert!(text.contains("left alone"), "{text}");
    assert_eq!(std::fs::read_to_string(&command).unwrap(), "my own /senate");
}

/// With `locale = "zh"` in bridge.toml, install reports in Chinese and
/// writes the Chinese slash command; the hooks and the MCP server are the
/// same.
#[test]
fn install_reports_in_the_bridges_locale() {
    let dir = tempfile::tempdir().unwrap();
    let senate = dir.path().join("senate");
    std::fs::create_dir_all(&senate).unwrap();
    std::fs::write(senate.join("bridge.toml"), "locale = \"zh\"\n").unwrap();
    let claude_dir = dir.path().join("claude-config");
    std::fs::create_dir_all(&claude_dir).unwrap();
    let (claude_bin, _log) = fake_claude(dir.path());
    let exe = std::fs::canonicalize(BIN).unwrap();
    let install = |yes: bool| {
        let mut cmd = bridge(dir.path(), &senate);
        cmd.args(["install", "--no-codex"])
            .arg("--claude-dir")
            .arg(&claude_dir)
            .arg("--claude-bin")
            .arg(&claude_bin);
        if yes {
            cmd.arg("--yes");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let text = install(false);
    assert!(
        text.starts_with("senate install（演练：不加 --yes 什么都不写）"),
        "{text}"
    );
    assert!(text.contains(&format!(
        "+ hooks.SessionStart: {} hook session-start",
        exe.display()
    )));
    assert!(text.contains("5 分钟后被丢弃"), "{text}");
    assert!(text.contains("什么都没写"), "{text}");
    let text = install(true);
    assert!(text.contains("完成。"), "{text}");
    let md = std::fs::read_to_string(claude_dir.join("commands").join("senate.md")).unwrap();
    assert!(md.contains("Senate 命令：`$ARGUMENTS`"), "{md}");
    assert!(md.contains("mcp__senate__inbox"), "{md}");
}

#[test]
fn install_sets_up_codex_in_its_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    let claude_dir = dir.path().join("claude-config");
    let codex_home = dir.path().join("codex-home");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::create_dir_all(&codex_home).unwrap();
    let config = codex_home.join("config.toml");
    let original = "# mine\nmodel = \"gpt-6\"\n\n[mcp_servers.dial]\nurl = \"https://x/mcp\"\n";
    std::fs::write(&config, original).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (claude_bin, _log) = fake_claude(dir.path());
    let exe = std::fs::canonicalize(BIN).unwrap();
    let install = |yes: bool| {
        let mut cmd = bridge(dir.path(), &dir.path().join("senate"));
        cmd.args(["install", "--codex", "--codex-home"])
            .arg(&codex_home)
            .arg("--claude-dir")
            .arg(&claude_dir)
            .arg("--claude-bin")
            .arg(&claude_bin);
        if yes {
            cmd.arg("--yes");
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };

    let text = install(false);
    assert!(
        text.contains(&format!("4. {} (Codex)", config.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("+ mcp_servers.senate: {} mcp", exe.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "+ hooks.SessionStart: {} hook session-start --codex",
            exe.display()
        )),
        "{text}"
    );
    assert!(text.contains("[[hooks.UserPromptSubmit.hooks]]"), "{text}");
    assert!(text.contains("open /hooks and trust"), "{text}");
    assert!(text.contains("codex app-server daemon start"), "{text}");
    assert!(text.contains("codex --remote unix://"), "{text}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), original);

    let text = install(true);
    assert!(text.contains("Done."), "{text}");
    let written = std::fs::read_to_string(&config).unwrap();
    assert!(written.starts_with(original.trim_end()), "{written}");
    let parsed: toml::Value = toml::from_str(&written).unwrap();
    assert_eq!(
        parsed["mcp_servers"]["senate"]["command"].as_str(),
        exe.to_str()
    );
    for (event, sub) in [
        ("SessionStart", "session-start"),
        ("UserPromptSubmit", "user-prompt-submit"),
        ("SessionEnd", "session-end"),
    ] {
        assert_eq!(
            parsed["hooks"][event][0]["hooks"][0]["command"].as_str(),
            Some(format!("{} hook {sub} --codex", exe.display()).as_str())
        );
    }
    assert_eq!(
        std::fs::read_to_string(codex_home.join("config.toml.bak")).unwrap(),
        original
    );
    let mode = std::fs::metadata(&config).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    // Claude's part is now installed too; the second run touches nothing.
    std::fs::write(
        claude_dir.join(".claude.json"),
        json!({"mcpServers": {"senate": {"type": "stdio", "command": exe, "args": ["mcp"]}}})
            .to_string(),
    )
    .unwrap();
    let text = install(true);
    assert!(text.contains("Nothing to do: already installed."), "{text}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), written);
}

#[test]
fn install_refuses_a_broken_settings_file() {
    let dir = tempfile::tempdir().unwrap();
    let claude_dir = dir.path().join("c");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join("settings.json"), "{ not json").unwrap();
    let (claude_bin, log) = fake_claude(dir.path());
    let out = bridge(dir.path(), &dir.path().join("senate"))
        .args(["install", "--no-codex", "--yes", "--claude-dir"])
        .arg(&claude_dir)
        .arg("--claude-bin")
        .arg(&claude_bin)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not valid JSON"));
    assert_eq!(
        std::fs::read_to_string(claude_dir.join("settings.json")).unwrap(),
        "{ not json"
    );
    assert!(!log.exists());
}

#[test]
fn mcp_binary_speaks_stdio_and_reports_a_missing_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = bridge(dir.path(), &dir.path().join("senate"))
        .arg("mcp")
        .env("CLAUDE_CODE_SESSION_ID", "test-session")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |v: Value| -> Value {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
        let id = v["id"].clone();
        if id.is_null() {
            return Value::Null; // a notification
        }
        loop {
            let mut line = String::new();
            assert!(stdout.read_line(&mut line).unwrap() > 0, "mcp exited");
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == id {
                return reply["result"].clone();
            }
        }
    };
    let init = rpc(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}}),
    );
    assert_eq!(init["serverInfo"]["name"], "senate");
    rpc(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let tools = rpc(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}));
    // The everyday tools and secretary tools. The presiding
    // and chair tools are listed only for a session that holds those roles.
    assert_eq!(tools["tools"].as_array().unwrap().len(), 17);
    let call = rpc(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": {"name": "catch_up", "arguments": {}}}));
    assert_eq!(call["isError"], true);
    assert!(
        call["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("daemon is not running")
    );
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "{status:?}");
}

#[test]
fn status_without_a_daemon_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let out = bridge(dir.path(), &dir.path().join("senate"))
        .arg("status")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("daemon: not running"), "{text}");
    assert!(text.contains("none registered"));
}

#[test]
fn help_groups_the_commands_and_gives_each_one_line_when_piped() {
    let dir = tempfile::tempdir().unwrap();
    let out = bridge(dir.path(), &dir.path().join("senate"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    for needle in [
        "See what is happening:\n  status ",
        "Speak as yourself:\n  ask ",
        "This worktree:\n  where ",
        "Set up this machine:\n  init ",
        "Start here:\n  senate status",
    ] {
        assert!(text.contains(needle), "{needle:?} in:\n{text}");
    }
    assert!(
        text.lines().all(|line| line.chars().count() <= 80),
        "{text}"
    );

    // The details and the examples are a command's own help.
    let out = bridge(dir.path(), &dir.path().join("senate"))
        .args(["help", "ask"])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("whichever agent (`*`)"), "{text}");
    assert!(text.contains("Examples:\n  senate ask claude"), "{text}");
}

#[test]
fn the_default_output_is_brief_and_verbose_says_everything() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("senate");
    let run = |args: &[&str]| {
        let out = brief(dir.path(), &home)
            .current_dir(dir.path())
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    };

    let status = run(&["status"]);
    assert!(
        status.contains("Daemon     offline · start with senated --machine"),
        "{status}"
    );
    assert!(status.contains("No live sessions"), "{status}");
    assert!(status.contains("--verbose adds"), "{status}");
    assert!(
        !status.contains("Socket") && !status.contains("queue:"),
        "{status}"
    );
    // The flag goes before or after the command, and has a short form.
    for args in [["status", "--verbose"], ["-v", "status"]] {
        let full = run(&args);
        assert!(full.contains("daemon: not running"), "{full}");
        assert!(full.contains("queue: 0 pending"), "{full}");
    }

    let here = run(&["where"]);
    assert!(here.starts_with("Committee         unbound\n"), "{here}");
    assert!(
        here.contains("Sessions join as  claude-<session-id> · codex-<session-id>"),
        "{here}"
    );
    assert!(here.contains("senate join <committee>"), "{here}");
    assert!(!here.contains("directory "), "{here}");
    assert!(run(&["where", "-v"]).starts_with("directory "));

    // The variable takes any spelling of yes or no, and never fails to parse.
    for (value, verbose) in [("true", true), ("yes", true), ("0", false), ("", false)] {
        let out = brief(dir.path(), &home)
            .current_dir(dir.path())
            .env("SENATE_VERBOSE", value)
            .arg("where")
            .output()
            .unwrap();
        assert!(out.status.success(), "{value:?}: {out:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(text.starts_with("directory "), verbose, "{value:?}: {text}");
    }

    assert_eq!(run(&["free"]), "Nothing has expired.\n");
    let free = run(&["free", "--verbose"]);
    assert!(free.contains("Session records would free 0"), "{free}");
}

#[test]
fn a_command_that_needs_the_daemon_says_how_to_start_it() {
    let dir = tempfile::tempdir().unwrap();
    let out = bridge(dir.path(), &dir.path().join("senate"))
        .args(["thread", "1", "--committee", "refund-v2"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = String::from_utf8(out.stderr).unwrap();
    for needle in [
        "senate: the Senate daemon is not running",
        "senated --machine",
        "senate service install --yes",
        "senate status",
    ] {
        assert!(text.contains(needle), "{needle:?} in:\n{text}");
    }
    // The cause is said once.
    assert_eq!(text.matches("No such file").count(), 0, "{text}");
}

#[test]
fn spawn_defaults_to_the_directory_committee_and_says_so_when_there_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let out = bridge(dir.path(), &dir.path().join("senate"))
        .current_dir(dir.path())
        .args(["spawn", "claude"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(
        text.contains("--committee") && text.contains("senate join"),
        "{text}"
    );
}

#[test]
fn init_local_writes_one_profile_and_never_overwrites_it() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("senate");
    let out = bridge(dir.path(), &home)
        .args(["init", "--local", "--person", "alice", "--locale", "zh"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let written = home.join("senate.toml");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        written.display().to_string()
    );
    let text = std::fs::read_to_string(&written).unwrap();
    assert!(text.contains("person = \"alice\""));
    // The person presides; an agent is a secretary, commented out.
    assert!(text.contains("speaker = \"senator::alice\""));
    assert!(text.contains("# secretaries = [\"claude\"]"));
    assert!(text.contains("listen = \"127.0.0.1:7979\""));
    assert!(text.contains("locale = \"zh\""));
    // Only that file and the commented secretary.toml, under the Senate
    // home; HOME is untouched.
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 2);
    assert!(home.join("secretary.toml").exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);

    let again = bridge(dir.path(), &home)
        .args(["init", "--local", "--person", "bob"])
        .output()
        .unwrap();
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));
    assert_eq!(std::fs::read_to_string(&written).unwrap(), text);

    let no_flag = bridge(dir.path(), &dir.path().join("other"))
        .args(["init", "--person", "alice"])
        .output()
        .unwrap();
    assert!(!no_flag.status.success());
    assert!(!dir.path().join("other").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_defaults_to_the_directory_committee_and_global_includes_both_queues() {
    use senate_bridge::hook::HookKind;
    use senate_core::{CommitteeId, Kind, NewEvent, Seq};

    let mut h = Harness::new().await;
    h.start().await;
    let first = h.workdir("first", Some("refund-v2"));
    let second = h.workdir("second", Some("other"));
    let nested = first.join("src");
    std::fs::create_dir(&nested).unwrap();
    h.hook(HookKind::SessionStart, "first-session", &first, None)
        .await;
    h.hook(HookKind::SessionStart, "second-session", &second, None)
        .await;
    for (committee, seq) in [("refund-v2", 50), ("other", 51), ("other", 52)] {
        let event = NewEvent::new(
            CommitteeId::new(committee).unwrap(),
            Kind::Note,
            person("pm"),
            "waiting",
        )
        .to(senator("away"))
        .into_event(Seq(seq), chrono::Utc::now());
        h.store().enqueue(&path("away"), &event).unwrap();
    }
    let check = |dir: &Path, args: &[&str]| {
        let out = bridge(h.dir.path(), h.paths.home())
            .current_dir(dir)
            .arg("status")
            .arg("--full-ids")
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    for dir in [&first, &nested] {
        let local = check(dir, &[]);
        assert!(local.contains("Committee refund-v2") && local.contains("first-session"));
        assert!(!local.contains("second-session"));
        assert!(local.contains("queue: 1 pending"), "{local}");
    }
    let global = check(&nested, &["--global"]);
    assert!(global.contains("first-session") && global.contains("second-session"));
    assert!(global.contains("queue: 3 pending"), "{global}");
    let named = check(&first, &["--committee", "other"]);
    assert!(named.contains("second-session") && !named.contains("first-session"));
    assert!(named.contains("queue: 2 pending"));
    assert!(check(h.dir.path(), &[]).contains("second-session"));
    h.stop().await;
}
