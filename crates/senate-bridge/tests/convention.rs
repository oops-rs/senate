//! The branch-name convention is off unless `senate.toml` turns it on, and
//! `.senate/` is never tracked: it holds its own `.gitignore`, whichever way
//! it came to exist, and Senate touches nothing under `.git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/common.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use senate_bridge::hook::HookKind;
use senate_bridge::session::CommitteeSource;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn repo(root: &Path, name: &str, branch: &str) -> PathBuf {
    let wd = root.join(name);
    std::fs::create_dir_all(&wd).unwrap();
    git(&wd, &["init", "-q", "-b", branch]);
    std::fs::write(wd.join("README.md"), "hi\n").unwrap();
    git(&wd, &["add", "README.md"]);
    git(&wd, &["commit", "-q", "-m", "first"]);
    wd
}

/// Every file under `.git` but the index (which `git status` refreshes),
/// with its bytes.
fn dot_git(wd: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(dir: &Path, base: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else if path.file_name().is_some_and(|n| n != "index") {
                out.insert(
                    path.strip_prefix(base).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap_or_default(),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&wd.join(".git"), &wd.join(".git"), &mut out);
    out
}

fn status(wd: &Path) -> String {
    git(wd, &["status", "--porcelain", "--untracked-files=all"])
}

fn bound(h: &Harness, session: &str) -> Option<(String, CommitteeSource)> {
    let s = h.store().session(session).unwrap().unwrap();
    s.committee()
        .map(|c| (c.to_string(), s.effective_source().unwrap()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_branch_name_convention_is_off_by_default_and_on_when_enabled() {
    for (on, expected) in [
        (false, None),
        (
            true,
            Some(("refund-v2".to_string(), CommitteeSource::Branch)),
        ),
    ] {
        let mut h = Harness::new().await;
        h.config.committee_from_branch = on;
        h.config.branch_prefix = "team".into();
        h.start().await;
        let wd = repo(h.dir.path(), "r", "team/refund-v2/alice");
        h.hook(HookKind::SessionStart, "s", &wd, None).await;
        assert_eq!(bound(&h, "s"), expected, "committee_from_branch = {on}");
        h.stop().await;
    }
}

#[test]
fn a_branch_prefix_alone_does_not_enable_the_convention() {
    let head = "server_url = \"http://x\"\ntoken = \"t\"\nperson = \"alice\"\n";
    let c: senate_bridge::BridgeConfig =
        toml::from_str(&format!("{head}branch_prefix = \"team\"\n")).unwrap();
    assert!(!c.committee_from_branch && c.branch_convention().is_none());
    assert_eq!(c.branches().prefix(), "team");
    let c: senate_bridge::BridgeConfig = toml::from_str(&format!(
        "{head}committee_from_branch = true\nbranch_prefix = \"crew\"\n"
    ))
    .unwrap();
    assert_eq!(c.branch_convention().unwrap().prefix(), "crew");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_file_binds_without_the_convention_and_the_name_wins_with_it() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = repo(h.dir.path(), "r", "team/other/alice");
    std::fs::create_dir_all(wd.join(".senate")).unwrap();
    std::fs::write(wd.join(".senate/committee"), "refund-v2\n").unwrap();
    h.hook(HookKind::SessionStart, "s", &wd, None).await;
    assert_eq!(
        bound(&h, "s"),
        Some(("refund-v2".into(), CommitteeSource::File))
    );
    h.stop().await;

    let mut h = Harness::new().await;
    h.config.committee_from_branch = true;
    h.config.branch_prefix = "team".into();
    h.start().await;
    let wd = repo(h.dir.path(), "r", "team/other/alice");
    std::fs::create_dir_all(wd.join(".senate")).unwrap();
    std::fs::write(wd.join(".senate/committee"), "refund-v2\n").unwrap();
    h.hook(HookKind::SessionStart, "s", &wd, None).await;
    assert_eq!(
        bound(&h, "s"),
        Some(("other".into(), CommitteeSource::Branch))
    );
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hook_gives_an_untracked_senate_directory_its_gitignore_once() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = repo(h.dir.path(), "r", "main");
    // A `.senate/` somebody made by hand: no ignore file yet.
    std::fs::create_dir_all(wd.join(".senate")).unwrap();
    std::fs::write(wd.join(".senate/committee"), "refund-v2\n").unwrap();
    std::fs::write(wd.join(".senate/PROGRESS.md"), "- done\n").unwrap();
    assert!(status(&wd).contains(".senate/"), "tracked-looking before");
    let before = dot_git(&wd);
    h.hook(HookKind::SessionStart, "s", &wd, None).await;
    let ignore = wd.join(".senate/.gitignore");
    assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "*\n");
    assert_eq!(dot_git(&wd), before, "nothing under .git changed");
    assert_eq!(status(&wd), "", "nothing in .senate/ shows in git status");
    // Not overwritten when it exists.
    std::fs::write(&ignore, "*\n!keep\n").unwrap();
    h.hook(HookKind::UserPromptSubmit, "s", &wd, None).await;
    assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "*\n!keep\n");
    // A directory that is not there is not made by a hook.
    let plain = repo(h.dir.path(), "plain", "main");
    h.hook(HookKind::SessionStart, "p", &plain, None).await;
    assert!(!plain.join(".senate").exists());
    // And the repository's own ignore rules are never touched.
    assert!(!wd.join(".gitignore").exists());
    assert!(
        !std::fs::read_to_string(wd.join(".git/info/exclude"))
            .unwrap()
            .contains("senate")
    );
    h.stop().await;
}

#[test]
fn checkpoint_notes_are_written_behind_the_same_ignore() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "r", "main");
    let before = dot_git(&wd);
    let files = senate_core::CheckpointFiles {
        progress: Some("done".into()),
        decisions: Some("none".into()),
        open: Some("nothing".into()),
    };
    let written = senate_bridge::checkpoint::write_files(&wd, &files).unwrap();
    assert_eq!(written.len(), 3);
    assert_eq!(
        std::fs::read_to_string(wd.join(".senate/.gitignore")).unwrap(),
        "*\n"
    );
    assert_eq!(dot_git(&wd), before);
    assert_eq!(status(&wd), "");
}
