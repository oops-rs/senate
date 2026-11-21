//! `senate service` through the built binary: the dry runs only. Install
//! and uninstall with `--yes`, and `status`, would ask the real service
//! manager, so they are tested through the seam in the module's own tests.
//! Every spawn has a temp HOME and SENATE_HOME.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_senate");

fn senate(home: &Path, senate_home: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    senate_bridge::testenv::isolate(&mut cmd, home);
    cmd.env("HOME", home)
        .env("SENATE_HOME", senate_home)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn install_is_a_dry_run_that_shows_the_file_and_the_commands() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("a home");
    let senate_home = dir.path().join("sen ate");
    std::fs::create_dir_all(&home).unwrap();
    let out = stdout(&senate(
        &home,
        &senate_home,
        &[
            "setup",
            "service",
            "install",
            "--senated",
            "/opt/my tools/senated",
        ],
    ));
    assert!(out.contains("/opt/my tools/senated"), "{out}");
    assert!(out.contains("--machine"), "{out}");
    assert!(
        out.contains("SENATE_HOME"),
        "a non-default home is passed: {out}"
    );
    assert!(out.contains("senated.log"), "{out}");
    assert!(
        out.contains("would write:") && out.contains("dry run"),
        "{out}"
    );
    let (file, steps) = if cfg!(target_os = "macos") {
        (
            home.join("Library/LaunchAgents/dev.senate.senated.plist"),
            "launchctl bootstrap",
        )
    } else {
        (
            home.join(".config/systemd/user/senated.service"),
            "systemctl --user enable --now",
        )
    };
    assert!(out.contains(&file.display().to_string()), "{out}");
    assert!(out.contains(steps), "{out}");
    // Nothing was written: not the file, not the Senate home.
    assert!(!file.exists());
    assert!(!senate_home.exists());
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
}

#[test]
fn the_default_home_passes_no_environment_and_the_report_follows_the_locale() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let senate_home = home.join(".senate");
    std::fs::create_dir_all(&senate_home).unwrap();
    std::fs::write(senate_home.join("senate.toml"), "locale = \"zh\"\n").unwrap();
    let out = stdout(&senate(
        &home,
        &senate_home,
        &["setup", "service", "install", "--senated", "/x/senated"],
    ));
    assert!(!out.contains("SENATE_HOME"), "{out}");
    assert!(
        out.contains("将写入：") && out.contains("加 --yes"),
        "{out}"
    );
}

#[test]
fn uninstall_is_a_dry_run_too() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = stdout(&senate(
        &home,
        &home.join(".senate"),
        &["setup", "service", "uninstall"],
    ));
    assert!(
        out.contains("would remove") && out.contains("dry run"),
        "{out}"
    );
    assert!(out.contains("it was not there"), "{out}");
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
}

#[test]
fn install_without_a_daemon_to_run_says_how_to_name_one() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // PATH holds no senated, and none is built next to this binary's own
    // directory unless the workspace built it: pass an empty PATH and accept
    // either outcome, but never a panic.
    let mut cmd = Command::new(BIN);
    senate_bridge::testenv::isolate(&mut cmd, &home);
    let out = cmd
        .env("HOME", &home)
        .env("SENATE_HOME", home.join(".senate"))
        .env("PATH", "")
        .args(["setup", "service", "install"])
        .output()
        .unwrap();
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("--senated"), "{err}");
    }
    assert_eq!(std::fs::read_dir(&home).unwrap().count(), 0);
}
