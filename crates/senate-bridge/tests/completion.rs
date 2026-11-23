#![allow(clippy::unwrap_used)]
use senate_bridge::testenv;
use serde_json::json;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const BIN: &str = env!("CARGO_BIN_EXE_senate");

async fn complete(home: &Path, shell: &str, words: &[&str]) -> String {
    let mut cmd = tokio::process::Command::new(BIN);
    testenv::isolate_async(&mut cmd, home);
    let out = tokio::time::timeout(
        Duration::from_secs(2),
        cmd.env("SENATE_HOME", home)
            .env("SENATE_COMPLETE", shell)
            .env("_CLAP_COMPLETE_INDEX", (words.len() - 1).to_string())
            .env("_CLAP_IFS", "\n")
            .arg("--")
            .args(words)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn bash_zsh_and_fish_complete_the_same_cli_and_live_choices() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let listener = tokio::net::UnixListener::bind(home.path().join("bridge.sock")).unwrap();
    let daemon = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let mut socket = BufReader::new(socket);
            let mut line = String::new();
            socket.read_line(&mut line).await.unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            let payload = match req["op"].as_str().unwrap() {
                "sessions" => json!({"rows": [{
                    "host_id":"01ARZ3NDEKTSV4RRFFQ69G5FAV","session_id":"test-session",
                    "primary":"claude-aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                    "alias":"reviewer","committee":"refund-v2",
                    "runtime":"claude","dir":"/work","state":"stopped","tty":"/tmp/host"
                }]}),
                "roster" => json!([{"committee":"refund-v2","chair":null,"chair_display_name":null,
                                     "chair_live":false,"members":[],"pending":0}]),
                "profiles" => json!({"review":{"runtime":"codex","note":"review"}}),
                op => panic!("unexpected completion request: {op}"),
            };
            let mut reply = serde_json::to_vec(&json!({"ok":true,"result":payload})).unwrap();
            reply.push(b'\n');
            socket.write_all(&reply).await.unwrap();
        }
    });
    for shell in ["bash", "zsh", "fish"] {
        assert!(
            complete(home.path(), shell, &["senate", "spa"])
                .await
                .contains("spawn")
        );
        assert!(
            complete(home.path(), shell, &["senate", "spawn", "co"])
                .await
                .contains("codex")
        );
        assert!(
            complete(home.path(), shell, &["senate", "spawn", "codex", "--att"])
                .await
                .contains("--attach")
        );
        let codex = complete(
            home.path(),
            shell,
            &["senate", "spawn", "codex", "--effort", ""],
        )
        .await;
        assert!(
            codex.contains("xhigh") && !codex.contains("max") && !codex.contains("none"),
            "{codex}"
        );
        let claude = complete(
            home.path(),
            shell,
            &["senate", "spawn", "claude", "--effort", "m"],
        )
        .await;
        assert!(claude.contains("max"), "{claude}");
        let approve = complete(home.path(), shell, &["senate", "session", "approve", "--"]).await;
        for flag in ["--once", "--always", "--deny"] {
            assert!(approve.contains(flag), "{approve}");
        }
        for args in [
            vec!["senate", "attach", "refund"],
            vec!["senate", "restore", "refund"],
            vec!["senate", "session", "stop", "refund"],
            vec!["senate", "session", "pause", "refund"],
            vec!["senate", "session", "resume", "refund"],
            vec!["senate", "session", "approve", "refund"],
            vec!["senate", "session", "replace", "refund"],
        ] {
            let names = complete(home.path(), shell, &args)
                .await
                .replace("\\:", ":");
            assert!(
                names.contains("refund-v2::reviewer"),
                "{shell} {args:?}: {names}"
            );
        }
        assert!(
            complete(home.path(), shell, &["senate", "join", "ref"])
                .await
                .contains("refund-v2")
        );
        assert!(
            complete(
                home.path(),
                shell,
                &["senate", "spawn", "--committee", "ref"]
            )
            .await
            .contains("refund-v2")
        );
        assert!(
            complete(home.path(), shell, &["senate", "spawn", "--profile", "rev"])
                .await
                .contains("review")
        );
    }
    daemon.abort();
}

#[tokio::test]
async fn completion_timeout_is_silent() {
    let home = tempfile::tempdir_in("/tmp").unwrap();
    let listener = tokio::net::UnixListener::bind(home.path().join("bridge.sock")).unwrap();
    for shell in ["zsh", "bash", "fish"] {
        let started = Instant::now();
        let (out, socket) = tokio::join!(
            complete(home.path(), shell, &["senate", "attach", "not-a-session"]),
            tokio::time::timeout(Duration::from_secs(1), listener.accept()),
        );
        // Keep the accepted connection open without replying.
        drop(socket.unwrap().unwrap());
        assert_eq!(out, "");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{shell}: {:?}",
            started.elapsed()
        );
    }
}
