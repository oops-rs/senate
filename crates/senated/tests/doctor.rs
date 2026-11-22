//! The model opt-out must skip even the first connection to a configured provider.

use std::net::TcpListener;
use std::process::Command;

#[test]
fn no_model_skips_provider_calls_and_preserves_other_json_checks() {
    let provider = TcpListener::bind("127.0.0.1:0").unwrap();
    provider.set_nonblocking(true).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("senated.toml");
    std::fs::write(&config, format!(
        "listen = \"127.0.0.1:0\"\ndatabase_url = \"sqlite::memory:\"\ndata_dir = {:?}\n\
         [model]\nmodel = \"scripted\"\nbase_url = \"http://{}/v1\"\ndeadline_secs = 1\ntriage_deadline_secs = 1\n",
        dir.path(), provider.local_addr().unwrap(),
    )).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_senated"));
    let output = senate_bridge::testenv::isolate(&mut cmd, dir.path())
        .args(["doctor", "--config"])
        .arg(config)
        .args(["--no-model", "--json"])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let checks = report["checks"].as_array().unwrap();
    assert!(checks.iter().any(|c| c["id"] == "server"));
    assert!(checks.iter().all(|c| c["id"] != "model"));
    assert_eq!(
        provider.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}
