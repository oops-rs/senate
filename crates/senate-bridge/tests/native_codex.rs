//! A native Codex session end to end, with the caller checks a release
//! daemon applies (not the disposable-fixture shortcut,
//! [`testenv::STRICT_IDENTITY_ENV`]) and real processes for everything the
//! daemon judges:
//!
//! - this test binary, copied as `codex` (`codex.exe`), is the Codex
//!   session's process. As `codex tui …` it runs Senate's SessionStart hook
//!   and `senate mcp` as its own children, the way Codex does, and relays
//!   the tool calls the test asks for;
//! - as `codex app-server proxy` it is the app-server the daemon pushes to
//!   ([`senate_bridge::delivery::codex::Transport::Proxy`]): WebSocket over
//!   stdio, recording each request.
//!
//! So the session binds to the `codex` process its hook and MCP server run
//! under, a read-only tool works from inside that process tree and not from
//! outside it, a question reaches the thread exactly once, and the answer
//! comes back exactly once. On Windows the daemon's IPC is the named pipe
//! of [`senate_bridge::local`], and a person-only request is refused
//! because nothing there proves a person.
//!
//! macOS needs signed caller proofs for any strict session, which a test
//! build has not: the scenario runs on Windows and Linux.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(target_os = "macos", allow(dead_code, unused_imports))]

#[path = "support/common.rs"]
mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const THREAD: &str = "thread-native-1";
const QUESTION: &str = "native codex ping 7f3a";
const REPLY: &str = "native codex pong 7f3a";
const CALLS: &str = "calls.jsonl";
const DIR_ENV: &str = "FAKE_CODEX_DIR";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("app-server") if args.get(2).map(String::as_str) == Some("proxy") => proxy(),
        Some("tui") => tui(&args[2..]),
        _ => scenario(),
    }
}

// ---- the fake app-server (`codex app-server proxy`) -------------------------

fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len < 65536 {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// One masked client frame: its opcode and payload; `None` at end of input.
fn read_frame(input: &mut impl Read) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    input.read_exact(&mut head).ok()?;
    let len = match head[1] & 0x7f {
        126 => {
            let mut b = [0u8; 2];
            input.read_exact(&mut b).ok()?;
            usize::from(u16::from_be_bytes(b))
        }
        127 => {
            let mut b = [0u8; 8];
            input.read_exact(&mut b).ok()?;
            u64::from_be_bytes(b) as usize
        }
        n => usize::from(n),
    };
    let mut mask = [0u8; 4];
    if head[1] & 0x80 != 0 {
        input.read_exact(&mut mask).ok()?;
    }
    let mut payload = vec![0u8; len];
    input.read_exact(&mut payload).ok()?;
    for (i, byte) in payload.iter_mut().enumerate() {
        *byte ^= mask[i % 4];
    }
    Some((head[0] & 0x0f, payload))
}

fn record(method: &str, params: &Value) {
    let Some(dir) = std::env::var_os(DIR_ENV) else {
        return;
    };
    // Readable too: Windows locks only a handle opened for reading or
    // writing, and append is neither.
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(Path::new(&dir).join(CALLS))
        .unwrap();
    // Several proxies may run at once: one whole line each.
    file.lock().unwrap();
    let mut line = json!({"method": method, "params": params}).to_string();
    line.push('\n');
    (&file).write_all(line.as_bytes()).unwrap();
}

fn answer(method: &str, params: &Value, turns: &mut u32) -> Result<Value, String> {
    let thread = params["threadId"].as_str().unwrap_or_default();
    let known = thread == THREAD;
    match method {
        "initialize" => Ok(json!({
            "userAgent": "fake/0.172.0",
            "codexHome": "",
            "platformFamily": std::env::consts::FAMILY,
            "platformOs": std::env::consts::OS,
        })),
        "thread/read" if known => {
            Ok(json!({"thread": {"id": thread, "status": {"type": "idle"}, "turns": []}}))
        }
        "thread/resume" if known => {
            Ok(json!({"thread": {"id": thread, "status": {"type": "idle"}, "turns": []}}))
        }
        "thread/turns/list" if known => Ok(json!({
            "data": [{"id": "turn-old", "status": "completed", "items": []}],
            "nextCursor": null
        })),
        "turn/start" if known => {
            *turns += 1;
            Ok(
                json!({"turn": {"id": format!("turn-{turns}"), "status": "inProgress", "items": []}}),
            )
        }
        "model/list" => Ok(json!({"data": [], "nextCursor": null})),
        _ if !known && params.get("threadId").is_some() => {
            Err(format!("thread not found: {thread}"))
        }
        other => Err(format!("unknown method {other}")),
    }
}

fn proxy() {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut stdout = std::io::stdout().lock();
    // The upgrade request, up to its blank line.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if input.read_exact(&mut byte).is_err() {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_lowercase();
    assert!(head.contains("upgrade: websocket"), "{head}");
    stdout
        .write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nconnection: Upgrade\r\nupgrade: websocket\r\n\
              sec-websocket-accept: x\r\n\r\n",
        )
        .unwrap();
    stdout.flush().unwrap();
    let mut turns = 0;
    while let Some((opcode, payload)) = read_frame(&mut input) {
        match opcode {
            0x1 => {}
            0x8 => return,
            _ => continue,
        }
        let msg: Value = serde_json::from_slice(&payload).unwrap();
        let (Some(id), Some(method)) = (msg.get("id"), msg["method"].as_str()) else {
            continue; // `initialized`, or a response
        };
        record(method, &msg["params"]);
        let reply = match answer(method, &msg["params"], &mut turns) {
            Ok(result) => json!({"id": id, "result": result}),
            Err(message) => json!({"id": id, "error": {"code": -32600, "message": message}}),
        };
        if stdout
            .write_all(&frame(1, reply.to_string().as_bytes()))
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return;
        }
    }
}

// ---- the fake Codex session (`codex tui`) -----------------------------------

/// `codex tui <senate> <thread> <cwd>`: run the SessionStart hook, then a
/// `senate mcp` server, both as children of this process, and relay one
/// tool call per line of stdin (`{"tool": …, "arguments": …}`), printing
/// each reply as one line.
fn tui(args: &[String]) {
    let [senate, thread, cwd] = args else {
        panic!("codex tui <senate> <thread> <cwd>");
    };
    let hook = Command::new(senate)
        .args(["hook", "session-start", "--runtime", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let input = json!({
        "session_id": thread,
        "cwd": cwd,
        "transcript_path": Path::new(cwd).join("rollout.jsonl"),
        "hook_event_name": "SessionStart",
        "source": "startup",
    });
    hook.stdin
        .as_ref()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let hook = hook.wait_with_output().unwrap();
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{}",
        json!({
            "hook": String::from_utf8_lossy(&hook.stdout),
            "stderr": String::from_utf8_lossy(&hook.stderr),
            "status": hook.status.to_string(),
        })
    )
    .unwrap();
    out.flush().unwrap();

    let mut mcp = Command::new(senate)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut to_mcp = mcp.stdin.take().unwrap();
    let mut from_mcp = BufReader::new(mcp.stdout.take().unwrap());
    let mut send = |value: Value| writeln!(to_mcp, "{value}").unwrap();
    send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "codex-mcp-client", "version": "0.172.0"},
        }}),
    );
    let reply = |from: &mut BufReader<_>, id: u64| loop {
        let mut line = String::new();
        if from.read_line(&mut line).unwrap() == 0 {
            panic!("senate mcp closed before answering {id}");
        }
        let value: Value = serde_json::from_str(&line).unwrap();
        if value["id"] == id {
            break value;
        }
    };
    reply(&mut from_mcp, 1);
    send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let mut id = 1;
    for line in std::io::stdin().lock().lines() {
        let call: Value = serde_json::from_str(&line.unwrap()).unwrap();
        id += 1;
        send(
            json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
                "name": call["tool"],
                "arguments": call["arguments"],
                "_meta": {"threadId": thread},
            }}),
        );
        let value = reply(&mut from_mcp, id);
        writeln!(out, "{value}").unwrap();
        out.flush().unwrap();
    }
    drop(send);
    let _ = mcp.wait();
}

// ---- the scenario -------------------------------------------------------------

fn senate_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_senate"))
}

/// One tool call through a `senate mcp` this test starts itself: outside
/// any Codex process. Its result's text and whether it is an error.
fn mcp_from_outside(root: &Path, home: &Path, tool: &str) -> (String, bool) {
    let mut cmd = Command::new(senate_bin());
    senate_bridge::testenv::isolate(&mut cmd, root);
    let mut child = cmd
        .arg("mcp")
        .env("SENATE_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for value in [
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "codex-mcp-client", "version": "0.172.0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": tool, "arguments": {}, "_meta": {"threadId": THREAD}}}),
    ] {
        writeln!(stdin, "{value}").unwrap();
    }
    let mut reply = None;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        let value: Value = serde_json::from_str(&line.unwrap()).unwrap();
        if value["id"] == 2 {
            reply = Some(value);
            break;
        }
    }
    drop(stdin);
    let _ = child.wait();
    tool_result(&reply.expect("a tools/call reply"))
}

fn tool_result(reply: &Value) -> (String, bool) {
    let text = reply["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    (text, reply["result"]["isError"] == true)
}

/// The app-server requests the fake proxies recorded, in order.
fn calls(dir: &Path) -> Vec<(String, Value)> {
    std::fs::read_to_string(dir.join(CALLS))
        .unwrap_or_default()
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            (
                value["method"].as_str().unwrap().to_owned(),
                value["params"].clone(),
            )
        })
        .collect()
}

fn pushes_of(dir: &Path, text: &str) -> usize {
    calls(dir)
        .iter()
        .filter(|(method, params)| {
            matches!(method.as_str(), "turn/start" | "turn/steer")
                && params["threadId"] == THREAD
                && params.to_string().contains(text)
        })
        .count()
}

#[cfg(target_os = "macos")]
fn scenario() {
    println!("native_codex: skipped on macOS (strict sessions need signed caller proofs)");
}

#[cfg(not(target_os = "macos"))]
#[allow(unsafe_code)]
fn scenario() {
    use common::*;

    let fake = tempfile::tempdir().unwrap();
    // Set before any thread reads the environment, and before the daemon
    // (whose proxies inherit it) starts. This binary holds this one
    // scenario, as `upgrade.rs` does, so nothing else sees them.
    unsafe {
        std::env::set_var(senate_bridge::testenv::STRICT_IDENTITY_ENV, "1");
        std::env::set_var(DIR_ENV, fake.path());
        // The test's directories are not inside any repository of the
        // machine's (a home directory can be one).
        std::env::set_var("GIT_CEILING_DIRECTORIES", std::env::temp_dir());
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut h = Harness::new().await;
        // `codex` is this binary, under its runtime's name.
        let bin = h.dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let program = bin.join(format!("codex{}", std::env::consts::EXE_SUFFIX));
        std::fs::copy(std::env::current_exe().unwrap(), &program).unwrap();
        std::fs::write(
            h.paths.config(),
            format!("[codex]\nbin = '{}'\n", program.display()),
        )
        .unwrap();
        h.start().await;
        let wd = h.workdir("cx", Some("refund-v2"));

        // The session: a `codex` process whose hook and MCP server are its
        // children.
        let mut cmd = tokio::process::Command::new(&program);
        senate_bridge::testenv::isolate_async(&mut cmd, h.dir.path());
        let mut session = cmd
            .arg("tui")
            .arg(senate_bin())
            .arg(THREAD)
            .arg(&wd)
            .env("SENATE_HOME", h.paths.home())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut to_session = session.stdin.take().unwrap();
        let mut from_session = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
            session.stdout.take().unwrap(),
        ));
        let mut next = async || -> Value {
            let line = tokio::time::timeout(WAIT, from_session.next_line())
                .await
                .expect("the session answers in time")
                .unwrap()
                .expect("the session is still running");
            serde_json::from_str(&line).unwrap()
        };
        let mut call = async |tool: &str, arguments: Value| {
            use tokio::io::AsyncWriteExt;
            let line = format!("{}\n", json!({"tool": tool, "arguments": arguments}));
            to_session.write_all(line.as_bytes()).await.unwrap();
        };

        let ran = next().await;
        let hook = ran["hook"].as_str().unwrap();
        assert!(
            hook.contains("joined to the committee `refund-v2`"),
            "the SessionStart hook joins the Codex session: {ran}"
        );
        // The senator a native Codex thread gets: `codex-<thread>`.
        let name = hook
            .split("as [senator::")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("the hook names the senator")
            .to_owned();
        let s = h.store().session(THREAD).unwrap().unwrap();
        assert_eq!(s.runtime, senate_bridge::session::Runtime::Codex);
        assert_eq!(s.agent, path(&name));
        println!("native_codex: SessionStart hook joined {THREAD} to refund-v2 as senator::{name}");

        // A read-only tool: served from inside the session's process tree…
        call("inbox", json!({})).await;
        let (text, error) = tool_result(&next().await);
        assert!(!error, "inbox from the Codex session: {text}");
        println!("native_codex: read-only MCP (inbox) from the session's tree: served");
        // …and refused to a process outside it that names the same thread.
        let (text, error) = mcp_from_outside(h.dir.path(), h.paths.home(), "inbox");
        assert!(error && text.contains("no verified authority"), "{text}");
        println!("native_codex: the same tool from outside the tree: refused ({text})");

        // A question for the session reaches its thread, once.
        h.wait_stream("refund-v2").await;
        let asked = h.server.append(relayed_to(senator(&name), QUESTION));
        eventually("the question pushed to the thread", WAIT, || {
            (pushes_of(fake.path(), QUESTION) > 0).then_some(())
        })
        .await;
        eventually("the question delivered", WAIT, || {
            let item = h.item_of(&name, asked.seq.0)?;
            (item.status == senate_bridge::state::ItemStatus::Delivered).then_some(())
        })
        .await;
        println!(
            "native_codex: seq {} pushed through `codex app-server proxy` (turn/start)",
            asked.seq.0
        );

        // The agent answers through its MCP server, once.
        call("answer", json!({"thread": asked.seq.0, "body": REPLY})).await;
        let (text, error) = tool_result(&next().await);
        assert!(!error, "answer from the Codex session: {text}");
        let answers = || {
            h.server
                .events("refund-v2")
                .into_iter()
                .filter(|e| e.kind == senate_core::Kind::Answer && e.body.contains(REPLY))
                .collect::<Vec<_>>()
        };
        eventually("the answer posted", WAIT, || {
            (!answers().is_empty()).then_some(())
        })
        .await;
        // Long enough for a retry or a second push to show.
        tokio::time::sleep(Duration::from_secs(3)).await;
        let posted = answers();
        assert_eq!(posted.len(), 1, "exactly one answer: {posted:?}");
        assert_eq!(posted[0].from, senator(&name));
        assert_eq!(
            pushes_of(fake.path(), QUESTION),
            1,
            "{:?}",
            calls(fake.path())
        );
        println!(
            "native_codex: one push, one answer (seq {}, from {})",
            posted[0].seq.0, posted[0].from
        );

        // Windows proves no person: a person-only request is refused even
        // from the person's own terminal.
        #[cfg(windows)]
        {
            let mut cmd = Command::new(senate_bin());
            senate_bridge::testenv::isolate(&mut cmd, h.dir.path());
            let out = cmd
                .arg("stats")
                .env("SENATE_HOME", h.paths.home())
                .current_dir(&wd)
                .output()
                .unwrap();
            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(!out.status.success(), "stats must be refused: {said}");
            println!(
                "native_codex: person-only `senate stats` refused: {}",
                said.trim()
            );
        }

        drop(to_session);
        let _ = tokio::time::timeout(WAIT, session.wait()).await;
        h.stop().await;
    });
    println!("native_codex: ok");
}
