//! No test reaches the session that runs the tests: every spawn of a
//! `senate` or `senated` binary goes through
//! [`senate_bridge::testenv::isolate`], and a binary confined to a test's
//! directory uses no inbox socket outside it, even one set on purpose.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::common;

use std::io::Write;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use common::*;
use senate_bridge::testenv;

const BIN: &str = env!("CARGO_BIN_EXE_senate");

/// A listening socket outside the test's directory, standing in for the
/// inbox of the session running the tests. It records whether anything
/// connected.
struct Sentinel {
    _dir: tempfile::TempDir,
    path: PathBuf,
    listener: UnixListener,
}

impl Sentinel {
    fn new() -> Self {
        // A short path: a Unix socket path has a length limit.
        let dir = tempfile::Builder::new()
            .prefix("sntl")
            .tempdir_in("/tmp")
            .unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            _dir: dir,
            path,
            listener,
        }
    }

    fn reached(&self) -> bool {
        self.listener.accept().is_ok()
    }
}

fn run(mut cmd: Command, stdin: &str) -> Output {
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

#[test]
fn isolate_removes_every_inherited_variable() {
    let root = tempfile::tempdir().unwrap();
    let mut cmd = Command::new("env");
    // As if the process running the tests had them all.
    for var in testenv::INHERITED {
        cmd.env(var, "/sentinel/inherited");
    }
    testenv::isolate(&mut cmd, root.path());
    let out = cmd.output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains("/sentinel/inherited"), "{text}");
    assert!(
        text.contains(&format!(
            "{}={}",
            testenv::CONFINE_ENV,
            root.path().display()
        )),
        "{text}"
    );
}

#[test]
fn a_confined_push_never_reaches_a_socket_outside() {
    let root = tempfile::tempdir().unwrap();
    let sentinel = Sentinel::new();
    let mut cmd = Command::new(BIN);
    testenv::isolate(&mut cmd, root.path());
    cmd.env("HOME", root.path())
        .env("SENATE_HOME", root.path().join("h"))
        .args([
            "debug", "push", "--token", "t", "--text", "hello", "--socket",
        ])
        .arg(&sentinel.path);
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("outside the directory"), "{err}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!sentinel.reached(), "the push reached the sentinel socket");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_confined_hook_registers_no_socket_from_outside() {
    let sentinel = Sentinel::new();
    let mut h = Harness::new().await;
    h.start().await;
    let wd = h.workdir("b", Some("refund-v2"));
    let root = h.dir.path().to_path_buf();
    let home = h.paths.home().to_path_buf();

    let out = {
        let mut cmd = Command::new(BIN);
        testenv::isolate(&mut cmd, &root);
        // Set again after isolating, as a careless test would.
        cmd.env("HOME", &root)
            .env("SENATE_HOME", &home)
            .env("CLAUDE_CODE_MESSAGING_SOCKET", &sentinel.path)
            .env("CLAUDE_CODE_MESSAGING_TOKEN", "sentinel-token")
            .args(["hook", "session-start"]);
        let input = serde_json::json!({
            "session_id": "b1", "cwd": wd, "hook_event_name": "SessionStart", "source": "startup"
        })
        .to_string();
        tokio::task::spawn_blocking(move || run(cmd, &input))
            .await
            .unwrap()
    };
    assert!(out.status.success());
    let session = h.store().session("b1").unwrap().unwrap();
    assert_eq!(session.socket, None, "the sentinel socket was registered");
    assert_eq!(session.token, None);
    // A message for the session is held for its hooks, never pushed there.
    h.wait_stream("refund-v2").await;
    h.server.append(relayed_question("never to the sentinel"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!sentinel.reached(), "a push reached the sentinel socket");
}

/// Every test that spawns `senate` or `senated` does it through
/// `testenv::isolate`: as many isolations as spawns, in every file.
#[test]
fn every_spawn_of_a_senate_binary_is_isolated() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "target") {
                    walk(&path, out);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    walk(&crates, &mut files);
    let mut checked = 0;
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        // The binaries are reached by these names in tests.
        let spawns = text.matches("Command::new(BIN)").count()
            + text.matches("Command::new(env!(\"CARGO_BIN_EXE_").count();
        if spawns == 0 {
            continue;
        }
        checked += 1;
        let isolated = text.matches("isolate(&mut cmd").count()
            + text.matches("isolate_async(&mut cmd").count();
        assert!(
            isolated >= spawns,
            "{}: {spawns} spawn(s) of a senate binary, {isolated} isolated",
            file.display()
        );
    }
    assert!(
        checked >= 5,
        "the scan found the spawning tests ({checked})"
    );
}
