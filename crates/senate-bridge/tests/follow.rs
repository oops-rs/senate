//! `.senate/` follows the branch: one saved copy per (repository, branch)
//! under the Senate home, swapped when the checked-out branch changes, from
//! every entry point, safe against crashes and concurrent processes. And the
//! commands built on it: `senate join | leave | pin | unpin | where | list |
//! free`, through the built binary in temp homes and temp repositories.
//! Nothing here touches the real home, and nothing under `.git` ever changes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use common::*;
use senate_bridge::Paths;
use senate_bridge::follow::{self, MANAGED, Outcome, SlotKey, Stop};
use senate_bridge::hook::HookKind;
use senate_bridge::ipc::{self, CheckpointReply, Request, SessionSelector};
use senate_bridge::session::CommitteeSource;
use senate_core::{CheckpointPayload, CheckpointTrigger, CommitteeId, Event, Kind};

const BIN: &str = env!("CARGO_BIN_EXE_senate");

// ---- git and files -----------------------------------------------------------

fn git_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    cmd
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_cmd(dir).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repository on `branch` with one commit.
fn repo(root: &Path, name: &str, branch: &str) -> PathBuf {
    let wd = root.join(name);
    std::fs::create_dir_all(&wd).unwrap();
    git(&wd, &["init", "-q", "-b", branch]);
    std::fs::write(wd.join("README.md"), "hi\n").unwrap();
    git(&wd, &["add", "README.md"]);
    git(&wd, &["commit", "-q", "-m", "first"]);
    wd
}

fn commit(wd: &Path, file: &str) -> String {
    std::fs::write(wd.join(file), file).unwrap();
    git(wd, &["add", file]);
    git(wd, &["commit", "-q", "-m", file]);
    git(wd, &["rev-parse", "HEAD"])
}

/// What Senate could have touched under `.git`: the config, the exclude
/// file and the hooks, as they were.
#[derive(Debug, PartialEq, Eq)]
struct DotGit {
    config: Vec<u8>,
    exclude: Vec<u8>,
    hooks: Vec<String>,
}

fn dot_git(wd: &Path) -> DotGit {
    let dir = wd.join(git(wd, &["rev-parse", "--git-common-dir"]));
    let mut hooks: Vec<String> = std::fs::read_dir(dir.join("hooks"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    hooks.sort();
    DotGit {
        config: std::fs::read(dir.join("config")).unwrap(),
        exclude: std::fs::read(dir.join("info/exclude")).unwrap_or_default(),
        hooks,
    }
}

/// Nothing under `.git` mentions Senate.
fn assert_no_senate_in_git(wd: &Path) {
    fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.to_string_lossy().contains("senate") {
                found.push(path.clone());
            }
            if path.is_dir() {
                walk(&path, found);
            } else if path
                .file_name()
                .is_some_and(|n| n == "config" || n == "exclude")
                && std::fs::read_to_string(&path).is_ok_and(|t| t.contains("senate"))
            {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(
        &wd.join(git(wd, &["rev-parse", "--git-common-dir"])),
        &mut found,
    );
    assert!(
        found.is_empty(),
        "Senate left something under .git: {found:?}"
    );
}

fn status(wd: &Path) -> String {
    git(wd, &["status", "--porcelain", "--untracked-files=all"])
}

/// The managed files of a worktree's `.senate/`, with their bytes.
fn managed(wd: &Path) -> BTreeMap<String, Vec<u8>> {
    MANAGED
        .iter()
        .filter_map(|n| {
            std::fs::read(wd.join(".senate").join(n))
                .ok()
                .map(|b| ((*n).to_string(), b))
        })
        .collect()
}

/// A branch's `.senate/` content: a committee and the three notes, each
/// carrying `tag` so one branch's can be told from another's.
fn notes(tag: &str) -> senate_core::CheckpointFiles {
    senate_core::CheckpointFiles {
        progress: Some(format!("## progress {tag}\n- done {tag}\n")),
        decisions: Some(format!("- decided {tag}\n")),
        open: Some(format!("- waiting {tag}\n")),
    }
}

/// Write a branch's committee and notes into the worktree.
fn put(wd: &Path, committee: &str, tag: &str) {
    senate_bridge::checkpoint::write_files(wd, &notes(tag)).unwrap();
    std::fs::write(wd.join(".senate/committee"), format!("{committee}\n")).unwrap();
}

fn tree(wd: &Path) -> follow::Tree {
    follow::tree_of(wd).unwrap()
}

fn sync(paths: &Paths, wd: &Path) -> Outcome {
    follow::sync(paths, &tree(wd)).unwrap()
}

fn home(dir: &Path) -> Paths {
    Paths::new(dir.join("home"))
}

// ---- the binary ---------------------------------------------------------------

fn senate(dir: &Path, cwd: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(BIN);
    senate_bridge::testenv::isolate(&mut cmd, dir);
    cmd.env("HOME", dir)
        .env("SENATE_HOME", dir.join("home"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn failed(out: &Output) -> String {
    assert!(!out.status.success(), "should have failed: {out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// ---- swapping ----------------------------------------------------------------

#[test]
fn a_b_a_brings_each_branchs_files_back_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    // A file somebody put in `.senate/` is nobody's but theirs.
    std::fs::create_dir_all(wd.join(".senate")).unwrap();
    std::fs::write(wd.join(".senate/mine.txt"), "keep me").unwrap();
    put(&wd, "refund-v2", "A");
    let a = managed(&wd);
    assert_eq!(a.len(), 4, "committee and three notes: {:?}", a.keys());
    assert_eq!(sync(&paths, &wd), Outcome::Adopted);
    assert_eq!(sync(&paths, &wd), Outcome::InStep);

    // A new branch has no saved copy: unbound, and A's files are not here.
    git(&wd, &["switch", "-q", "-c", "b"]);
    assert_eq!(
        sync(&paths, &wd),
        Outcome::Swapped {
            from: SlotKey::Branch { branch: "a".into() },
            to: SlotKey::Branch { branch: "b".into() }
        }
    );
    assert!(managed(&wd).is_empty(), "{:?}", managed(&wd));
    assert_eq!(
        std::fs::read(wd.join(".senate/mine.txt")).unwrap(),
        b"keep me"
    );

    // B's own work.
    put(&wd, "other", "B");
    let b = managed(&wd);
    git(&wd, &["switch", "-q", "a"]);
    sync(&paths, &wd);
    assert_eq!(managed(&wd), a, "A's committee and notes, byte for byte");

    // Edits made on A do not leak into B's copy, nor B's into A's.
    std::fs::write(wd.join(".senate/PROGRESS.md"), "edited on A\n").unwrap();
    git(&wd, &["switch", "-q", "b"]);
    sync(&paths, &wd);
    assert_eq!(managed(&wd), b);
    std::fs::write(wd.join(".senate/OPEN.md"), "edited on B\n").unwrap();
    git(&wd, &["switch", "-q", "a"]);
    sync(&paths, &wd);
    let now = managed(&wd);
    assert_eq!(now["PROGRESS.md"], b"edited on A\n");
    assert_eq!(now["OPEN.md"], a["OPEN.md"], "B's edit is not in A's files");
    assert_eq!(now["committee"], a["committee"]);
    assert_eq!(
        std::fs::read(wd.join(".senate/mine.txt")).unwrap(),
        b"keep me"
    );
    // The copies are in the Senate home, not in the repository.
    let slots = follow::slots(&paths);
    assert_eq!(slots.len(), 2, "{slots:?}");
    assert!(
        slots
            .iter()
            .all(|s| s.dir.starts_with(dir.path().join("home/branches")))
    );
    assert_eq!(status(&wd), "", "nothing in .senate/ shows in git status");
}

#[test]
fn a_branch_that_never_had_files_is_unbound_and_a_worktree_with_a_copy_gets_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    // No `.senate/` at all: nothing is made just to keep a marker.
    assert_eq!(sync(&paths, &wd), Outcome::Nothing);
    assert!(!wd.join(".senate").exists());
    put(&wd, "refund-v2", "A");
    sync(&paths, &wd);
    let a = managed(&wd);
    git(&wd, &["switch", "-q", "-c", "b"]);
    sync(&paths, &wd);
    // A fresh worktree on A (A is free again after a detached HEAD here).
    git(&wd, &["switch", "-q", "--detach"]);
    sync(&paths, &wd);
    let other = dir.path().join("w2");
    git(
        &wd,
        &["worktree", "add", "-q", other.to_str().unwrap(), "a"],
    );
    assert!(!other.join(".senate").exists());
    assert_eq!(sync(&paths, &other), Outcome::Restored);
    assert_eq!(
        managed(&other),
        a,
        "a line of work comes back with its branch"
    );
    assert_eq!(
        std::fs::read_to_string(other.join(".senate/.gitignore")).unwrap(),
        "*\n"
    );
    assert_eq!(status(&other), "");
}

#[test]
fn two_worktrees_of_one_repository_each_keep_their_own_branchs_files() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let w1 = repo(dir.path(), "r", "a");
    let w2 = dir.path().join("w2");
    git(
        &w1,
        &["worktree", "add", "-q", "-b", "b", w2.to_str().unwrap()],
    );
    put(&w1, "c1", "one");
    put(&w2, "c2", "two");
    sync(&paths, &w1);
    sync(&paths, &w2);
    let (m1, m2) = (managed(&w1), managed(&w2));
    // w1 moves on; w2 is untouched.
    git(&w1, &["switch", "-q", "-c", "a2"]);
    sync(&paths, &w1);
    assert!(managed(&w1).is_empty());
    assert_eq!(managed(&w2), m2);
    // And back.
    git(&w1, &["switch", "-q", "a"]);
    sync(&paths, &w1);
    assert_eq!(managed(&w1), m1);
    assert_eq!(sync(&paths, &w2), Outcome::InStep);
    assert_eq!(managed(&w2), m2);
}

#[test]
fn a_detached_head_is_a_slot_per_worktree_and_the_branch_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    sync(&paths, &wd);
    let a = managed(&wd);
    git(&wd, &["switch", "-q", "--detach"]);
    assert!(matches!(
        sync(&paths, &wd),
        Outcome::Swapped {
            to: SlotKey::Detached,
            ..
        }
    ));
    assert!(managed(&wd).is_empty(), "a detached HEAD starts unbound");
    put(&wd, "other", "rebase");
    let rebase = managed(&wd);
    // A commit in the middle of a rebase or a bisect is still the same slot.
    commit(&wd, "x.txt");
    assert_eq!(sync(&paths, &wd), Outcome::InStep);
    git(&wd, &["switch", "-q", "a"]);
    sync(&paths, &wd);
    assert_eq!(managed(&wd), a);
    git(&wd, &["switch", "-q", "--detach"]);
    sync(&paths, &wd);
    assert_eq!(managed(&wd), rebase);
}

#[test]
fn a_renamed_branch_keeps_its_files_and_its_copy_moves_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    sync(&paths, &wd);
    // Make a copy for `a` to carry along: leave and come back.
    git(&wd, &["switch", "-q", "-c", "b"]);
    sync(&paths, &wd);
    git(&wd, &["switch", "-q", "a"]);
    sync(&paths, &wd);
    let a = managed(&wd);
    assert_eq!(follow::slots(&paths).len(), 1);
    // Commits since the last look: only the reflog tells it is a rename.
    commit(&wd, "more.txt");
    git(&wd, &["branch", "-m", "a", "renamed"]);
    assert_eq!(
        sync(&paths, &wd),
        Outcome::Renamed {
            from: "a".into(),
            to: "renamed".into()
        }
    );
    assert_eq!(managed(&wd), a, "the files stayed");
    let slots = follow::slots(&paths);
    assert_eq!(slots.len(), 1);
    assert_eq!(
        slots[0].meta.key,
        SlotKey::Branch {
            branch: "renamed".into()
        }
    );
    // Nothing is left under a name that is gone.
    assert!(follow::stale(&slots[0].meta).is_none());
}

#[test]
fn a_deleted_branch_with_new_work_is_not_taken_for_a_rename() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    let a = managed(&wd);
    sync(&paths, &wd);
    git(&wd, &["switch", "-q", "-c", "b"]);
    commit(&wd, "b.txt");
    git(&wd, &["branch", "-D", "a"]);
    assert!(matches!(sync(&paths, &wd), Outcome::Swapped { .. }));
    assert!(managed(&wd).is_empty(), "b is a new line of work");
    // A's files were shelved, not lost: its copy is in the home, now stale.
    let slots = follow::slots(&paths);
    assert_eq!(slots.len(), 1);
    assert_eq!(slots[0].committee.as_deref(), Some("refund-v2"));
    assert_eq!(
        std::fs::read(slots[0].dir.join("committee")).unwrap(),
        a["committee"]
    );
    assert_eq!(
        follow::stale(&slots[0].meta),
        Some(follow::Stale::BranchGone)
    );
}

#[test]
fn a_first_run_adopts_what_is_there_and_saves_nothing_over_a_copy() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    sync(&paths, &wd);
    git(&wd, &["switch", "-q", "-c", "b"]);
    sync(&paths, &wd);
    git(&wd, &["switch", "-q", "a"]);
    sync(&paths, &wd);
    let copy = |p: &Paths| {
        let slot = &follow::slots(p)[0];
        let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for entry in std::fs::read_dir(&slot.dir).unwrap() {
            let path = entry.unwrap().path();
            files.insert(
                path.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(path).unwrap(),
            );
        }
        files
    };
    let before = copy(&paths);
    // A worktree from before branch copies: files, no marker.
    std::fs::remove_file(wd.join(".senate/.branch")).unwrap();
    std::fs::write(wd.join(".senate/OPEN.md"), "newer than the copy\n").unwrap();
    assert_eq!(sync(&paths, &wd), Outcome::Adopted);
    assert_eq!(copy(&paths), before, "the copy was not written over");
    assert_eq!(
        follow::belongs_to(&tree(&wd)),
        Some(SlotKey::Branch { branch: "a".into() })
    );
}

// ---- crashes and concurrency -----------------------------------------------------

#[test]
fn a_crash_at_any_step_of_a_swap_loses_nothing_on_the_next_run() {
    for stop in [Stop::AfterSave, Stop::AfterPending, Stop::MidRestore] {
        let dir = tempfile::tempdir().unwrap();
        let paths = home(dir.path());
        let wd = repo(dir.path(), "r", "a");
        put(&wd, "refund-v2", "A");
        sync(&paths, &wd);
        git(&wd, &["switch", "-q", "-c", "b"]);
        sync(&paths, &wd);
        put(&wd, "other", "B");
        let (a, b) = {
            git(&wd, &["switch", "-q", "a"]);
            sync(&paths, &wd);
            let a = managed(&wd);
            git(&wd, &["switch", "-q", "b"]);
            sync(&paths, &wd);
            (a, managed(&wd))
        };
        // Now on b with B's files; the swap to a is cut short.
        git(&wd, &["switch", "-q", "a"]);
        let t = tree(&wd);
        let lock = follow::acquire(&paths, &t).unwrap().unwrap();
        let crashed = follow::sync_locked(&paths, &t, &lock, Some(stop));
        assert!(crashed.is_err(), "{stop:?}");
        drop(lock);
        // The next run completes it: A's files are back, B's copy is intact.
        sync(&paths, &wd);
        assert_eq!(managed(&wd), a, "{stop:?}");
        assert!(!wd.join(".senate/.stage").exists(), "{stop:?}");
        git(&wd, &["switch", "-q", "b"]);
        sync(&paths, &wd);
        assert_eq!(managed(&wd), b, "{stop:?}");
        git(&wd, &["switch", "-q", "a"]);
        sync(&paths, &wd);
        assert_eq!(managed(&wd), a, "{stop:?}");
    }
}

#[test]
fn a_crashed_copy_is_cleaned_up_and_a_replaced_copy_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    sync(&paths, &wd);
    git(&wd, &["switch", "-q", "-c", "b"]);
    sync(&paths, &wd);
    let slot = follow::slots(&paths).pop().unwrap().dir;
    let parent = slot.parent().unwrap().to_path_buf();
    // A half-written copy, and a copy renamed aside whose replacement never came.
    std::fs::create_dir_all(parent.join("x.tmp-1")).unwrap();
    let aside = parent.join(format!(
        "{}.old-1",
        slot.file_name().unwrap().to_string_lossy()
    ));
    std::fs::rename(&slot, &aside).unwrap();
    sync(&paths, &wd);
    assert!(!parent.join("x.tmp-1").exists());
    assert!(slot.join("committee").exists(), "the copy was put back");
    assert!(!aside.exists());
}

#[test]
fn two_processes_swapping_at_once_leave_one_consistent_state() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    sync(&paths, &wd);
    git(&wd, &["switch", "-q", "-c", "b"]);
    sync(&paths, &wd);
    put(&wd, "other", "B");
    let (a, b) = (
        {
            git(&wd, &["switch", "-q", "a"]);
            sync(&paths, &wd);
            managed(&wd)
        },
        {
            git(&wd, &["switch", "-q", "b"]);
            sync(&paths, &wd);
            managed(&wd)
        },
    );
    for round in 0..6 {
        let (target, want) = if round % 2 == 0 { ("a", &a) } else { ("b", &b) };
        git(&wd, &["switch", "-q", target]);
        let children: Vec<_> = (0..3)
            .map(|_| {
                let mut cmd = Command::new(BIN);
                senate_bridge::testenv::isolate(&mut cmd, dir.path());
                cmd.env("HOME", dir.path())
                    .env("SENATE_HOME", dir.path().join("home"))
                    .current_dir(&wd)
                    .arg("where")
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for mut child in children {
            assert!(child.wait().unwrap().success());
        }
        assert_eq!(&managed(&wd), want, "round {round}");
    }
}

#[test]
fn the_repository_lock_waits_and_then_gives_up_without_changing_anything() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    let wd = repo(dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    sync(&paths, &wd);
    git(&wd, &["switch", "-q", "-c", "b"]);
    let t = tree(&wd);
    let held = follow::acquire(&paths, &t).unwrap().unwrap();
    assert_eq!(sync(&paths, &wd), Outcome::Busy);
    assert_eq!(managed(&wd).len(), 4, "nothing was changed");
    drop(held);
    assert!(matches!(sync(&paths, &wd), Outcome::Swapped { .. }));
}

// ---- every entry point ---------------------------------------------------------

/// Alice's worktree on `a` bound to refund-v2 with A's notes, the Senate
/// home next to it; then the branch is switched to a new `b`, without
/// Senate being told.
fn on_a_then_switched(h: &Harness) -> PathBuf {
    let wd = repo(h.dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    follow::sync(&h.paths, &tree(&wd)).unwrap();
    wd
}

fn switched_away(wd: &Path) {
    git(wd, &["switch", "-q", "-c", "b"]);
}

fn a_copy_exists(h: &Harness) -> bool {
    follow::slots(&h.paths).iter().any(|s| {
        s.meta.key == SlotKey::Branch { branch: "a".into() }
            && s.committee.as_deref() == Some("refund-v2")
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_session_hook_swaps_before_it_reads() {
    for kind in [
        HookKind::SessionStart,
        HookKind::UserPromptSubmit,
        HookKind::SessionEnd,
    ] {
        let mut h = Harness::new().await;
        h.start().await;
        let wd = on_a_then_switched(&h);
        // A session registered on `a`, bound to refund-v2.
        h.hook(HookKind::SessionStart, "s1", &wd, None).await;
        assert_eq!(
            h.store()
                .session("s1")
                .unwrap()
                .unwrap()
                .committee()
                .map(ToString::to_string),
            Some("refund-v2".into())
        );
        switched_away(&wd);
        h.hook(kind, "s1", &wd, None).await;
        assert!(managed(&wd).is_empty(), "{kind:?}: a's files are not on b");
        assert!(a_copy_exists(&h), "{kind:?}");
        if kind != HookKind::SessionEnd {
            // The session is told, in one line, that it is unbound now.
            let s = h.store().session("s1").unwrap().unwrap();
            assert_eq!(s.committee(), None, "{kind:?}");
            assert_eq!(s.branch.as_deref(), Some("b"));
        }
        h.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_session_is_rebound_and_told_in_one_line_but_an_explicit_join_is_not_disturbed() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = on_a_then_switched(&h);
    git(&wd, &["switch", "-q", "-c", "b"]);
    follow::sync(&h.paths, &tree(&wd)).unwrap();
    put(&wd, "other", "B");
    git(&wd, &["switch", "-q", "a"]);
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    h.hook(HookKind::SessionStart, "s2", &wd, None).await;
    // s2 joins explicitly; s1 relies on the worktree.
    ipc::request::<serde_json::Value>(
        &h.socket(),
        &Request::Join {
            session: selector("s2"),
            committee: committee(),
        },
        Duration::from_secs(1),
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    git(&wd, &["switch", "-q", "b"]);
    let told = h
        .hook(HookKind::UserPromptSubmit, "s1", &wd, None)
        .await
        .expect("a line");
    let text = told.to_string();
    assert!(
        text.contains("The branch changed to `b`") && text.contains("now bound to other"),
        "{text}"
    );
    let s1 = h.store().session("s1").unwrap().unwrap();
    assert_eq!(
        s1.committee().map(ToString::to_string),
        Some("other".into())
    );
    assert_eq!(s1.effective_source(), Some(CommitteeSource::File));
    // Not told twice.
    let again = h.hook(HookKind::UserPromptSubmit, "s1", &wd, None).await;
    assert!(
        again.is_none_or(|v| !v.to_string().contains("The branch changed")),
        "told once"
    );
    // s2 spoke for itself: still joined, and not told.
    let said = h.hook(HookKind::UserPromptSubmit, "s2", &wd, None).await;
    assert!(said.is_none_or(|v| !v.to_string().contains("The branch changed")));
    assert_eq!(
        h.store()
            .session("s2")
            .unwrap()
            .unwrap()
            .committee()
            .map(ToString::to_string),
        Some("refund-v2".into())
    );
    h.stop().await;
}

fn selector(id: &str) -> SessionSelector {
    SessionSelector {
        session_id: Some(id.into()),
        claude_pid: None,
    }
}

#[test]
fn each_command_swaps_before_it_reads_or_writes() {
    for command in [
        &["where"][..],
        &["join", "later"],
        &["leave"],
        &["pin", "reviewer"],
        &["unpin"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let paths = home(dir.path());
        let wd = repo(dir.path(), "r", "a");
        put(&wd, "refund-v2", "A");
        let a = managed(&wd);
        sync(&paths, &wd);
        switched_away(&wd);
        ok(&senate(dir.path(), &wd, command));
        let now = managed(&wd);
        assert!(
            !now.contains_key("PROGRESS.md") && now.get("committee") != a.get("committee")
                || command == &["join", "later"][..],
            "{command:?}: a's notes are not on b: {:?}",
            now.keys()
        );
        assert!(!now.contains_key("PROGRESS.md"), "{command:?}");
        // A's files are safe in the home.
        let slot = follow::slots(&paths).pop().unwrap();
        assert_eq!(
            std::fs::read(slot.dir.join("committee")).unwrap(),
            a["committee"],
            "{command:?}"
        );
        assert_eq!(status(&wd), "", "{command:?}");
    }
}

// ---- the daemon's checkpoint path -------------------------------------------------

fn checkpoints(h: &Harness, committee: &str) -> Vec<(CheckpointPayload, Event)> {
    h.server
        .events(committee)
        .into_iter()
        .filter(|e| e.kind == Kind::Checkpoint)
        .map(|e| (CheckpointPayload::of(&e).expect("a payload"), e))
        .collect()
}

async fn checkpoint_tool(h: &Harness, sid: &str) -> CheckpointReply {
    ipc::request(
        &h.socket(),
        &Request::Checkpoint {
            session: selector(sid),
            summary: None,
            progress: None,
            decisions: None,
            open: None,
            push: false,
            id: None,
        },
        Duration::from_secs(1),
        Duration::from_secs(30),
    )
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_checkpoint_right_after_a_switch_carries_the_checked_out_branchs_notes_and_committee() {
    let mut h = Harness::new().await;
    h.start().await;
    let wd = repo(h.dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    follow::sync(&h.paths, &tree(&wd)).unwrap();
    git(&wd, &["switch", "-q", "-c", "b"]);
    follow::sync(&h.paths, &tree(&wd)).unwrap();
    put(&wd, "other", "B");
    git(&wd, &["switch", "-q", "a"]);
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    let before = dot_git(&wd);

    let reply = checkpoint_tool(&h, "s1").await;
    assert_eq!(reply.branch, "a");
    assert_eq!(reply.posted.committee.as_str(), "refund-v2");
    let (payload, event) = checkpoints(&h, "refund-v2").pop().unwrap();
    assert_eq!(
        payload.files.progress.as_deref(),
        Some("## progress A\n- done A\n")
    );
    assert_eq!(event.refs.as_ref().unwrap().branch.as_deref(), Some("a"));

    // The worktree is switched; no hook has run since. The checkpoint tool
    // swaps first, re-binds the session, and speaks for `b` on b's committee.
    git(&wd, &["switch", "-q", "b"]);
    let reply = checkpoint_tool(&h, "s1").await;
    assert_eq!(reply.branch, "b");
    assert_eq!(reply.posted.committee.as_str(), "other");
    let (payload, event) = checkpoints(&h, "other").pop().unwrap();
    assert_eq!(
        payload.files.progress.as_deref(),
        Some("## progress B\n- done B\n")
    );
    assert_eq!(payload.files.open.as_deref(), Some("- waiting B\n"));
    assert_eq!(event.refs.as_ref().unwrap().branch.as_deref(), Some("b"));
    // Never B's notes on A's committee.
    for (p, _) in checkpoints(&h, "refund-v2") {
        assert!(!p.files.progress.unwrap_or_default().contains("B"));
    }
    assert_eq!(dot_git(&wd), before, "nothing under .git changed");
    assert_no_senate_in_git(&wd);
    assert_eq!(status(&wd), "");
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_commit_triggered_checkpoint_swaps_first_too() {
    let mut h = Harness::new().await;
    h.start_with(senate_bridge::Timing {
        checkpoint_poll: Duration::from_millis(150),
        checkpoint_interval: Duration::from_secs(3600),
        ..fast_timing()
    })
    .await;
    let wd = repo(h.dir.path(), "r", "a");
    put(&wd, "refund-v2", "A");
    follow::sync(&h.paths, &tree(&wd)).unwrap();
    git(&wd, &["switch", "-q", "-c", "b"]);
    follow::sync(&h.paths, &tree(&wd)).unwrap();
    put(&wd, "other", "B");
    git(&wd, &["switch", "-q", "a"]);
    h.hook(HookKind::SessionStart, "s1", &wd, None).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        checkpoints(&h, "refund-v2").is_empty(),
        "the first look is a baseline"
    );

    // Switch to b and commit there: the poll sees a new HEAD.
    git(&wd, &["switch", "-q", "b"]);
    let head = commit(&wd, "b.txt");
    let (payload, _) = eventually("a commit checkpoint on b's committee", WAIT, || {
        checkpoints(&h, "other").pop()
    })
    .await;
    assert_eq!(payload.trigger, CheckpointTrigger::Commit);
    assert_eq!(payload.commit, head);
    assert_eq!(payload.branch, "b");
    assert_eq!(
        payload.files.progress.as_deref(),
        Some("## progress B\n- done B\n")
    );
    assert!(
        checkpoints(&h, "refund-v2")
            .iter()
            .all(|(p, _)| p.branch != "b"),
        "b's work never lands on a's committee"
    );
    h.stop().await;
}

// ---- the commands ----------------------------------------------------------------

#[test]
fn join_leave_pin_unpin_where_list_round_trip_and_touch_nothing_under_dot_git() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "r", "a");
    let before = dot_git(&wd);

    let out = ok(&senate(dir.path(), &wd, &["where"]));
    assert!(out.contains("committee unbound"), "{out}");
    assert!(out.contains("senate join <committee>"), "{out}");

    let out = ok(&senate(dir.path(), &wd, &["join", "refund-v2"]));
    assert!(
        out.contains("Bound this worktree on branch `a` to refund-v2."),
        "{out}"
    );
    assert!(out.contains("kept per branch"), "{out}");
    assert!(out.contains("Here now: refund-v2 (source: file)."), "{out}");
    assert_eq!(
        std::fs::read_to_string(wd.join(".senate/committee")).unwrap(),
        "refund-v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(wd.join(".senate/.gitignore")).unwrap(),
        "*\n"
    );

    // Not a committee id: refused, nothing written.
    failed(&senate(dir.path(), &wd, &["join", "Not A Slug"]));
    assert_eq!(
        std::fs::read_to_string(wd.join(".senate/committee")).unwrap(),
        "refund-v2\n"
    );

    let out = ok(&senate(dir.path(), &wd, &["pin", "reviewer"]));
    assert!(
        out.contains("can claim the initial alias reviewer"),
        "{out}"
    );
    assert!(out.contains("(source: file)"), "{out}");
    assert_eq!(
        std::fs::read_to_string(wd.join(".senate/agent")).unwrap(),
        "reviewer\n"
    );
    let err = failed(&senate(dir.path(), &wd, &["pin", "Not An Agent"]));
    assert!(err.contains("not an agent"), "{err}");
    // A path is not a name: refused, the pin unchanged.
    let err = failed(&senate(dir.path(), &wd, &["pin", "claude/reviewer"]));
    assert!(err.contains("one name, with no path inside it"), "{err}");
    assert_eq!(
        std::fs::read_to_string(wd.join(".senate/agent")).unwrap(),
        "reviewer\n"
    );

    // On another branch: its own, unbound; `a`'s copy is listed.
    git(&wd, &["switch", "-q", "-c", "b"]);
    let out = ok(&senate(dir.path(), &wd, &["where"]));
    assert!(out.contains("branch b"), "{out}");
    assert!(out.contains("committee unbound"), "{out}");
    assert!(out.contains(".senate/ belongs to `b`"), "{out}");
    assert!(
        out.contains("Saved copies for other branches of this repository (1): a → refund-v2"),
        "{out}"
    );
    assert!(!wd.join(".senate/agent").exists(), "a's pin is not on b");
    let json: serde_json::Value =
        serde_json::from_str(&ok(&senate(dir.path(), &wd, &["list", "--json"]))).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
    assert_eq!(json[0]["branch"], "a");
    assert_eq!(json[0]["committee"], "refund-v2");
    assert_eq!(json[0]["stale"], serde_json::Value::Null);
    let out = ok(&senate(dir.path(), &wd, &["list"]));
    assert!(out.contains("refund-v2") && !out.contains("stale"), "{out}");

    ok(&senate(dir.path(), &wd, &["join", "other"]));
    git(&wd, &["switch", "-q", "a"]);
    let out = ok(&senate(dir.path(), &wd, &["where"]));
    assert!(out.contains("committee refund-v2 (file)"), "{out}");
    assert!(out.contains("reviewer"), "{out}");
    assert!(
        out.contains("Saved copies for other branches of this repository (1): b → other"),
        "{out}"
    );

    let out = ok(&senate(dir.path(), &wd, &["unpin"]));
    assert!(out.contains("Removed the agent pin"), "{out}");
    assert!(!wd.join(".senate/agent").exists());
    let out = ok(&senate(dir.path(), &wd, &["unpin"]));
    assert!(out.contains("There was no agent pin"), "{out}");
    let out = ok(&senate(dir.path(), &wd, &["leave"]));
    assert!(
        out.contains("Removed the binding of this worktree on branch `a`."),
        "{out}"
    );
    assert!(out.contains("Here now: no committee."), "{out}");
    let out = ok(&senate(dir.path(), &wd, &["leave"]));
    assert!(out.contains("There was no binding"), "{out}");

    assert_eq!(
        dot_git(&wd),
        before,
        "nothing under .git changed by any command"
    );
    assert_eq!(
        status(&wd),
        "",
        "git status stays clean with a populated .senate/"
    );
    assert!(!wd.join(".gitignore").exists());
}

#[test]
fn the_commands_work_outside_git_and_speak_chinese_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    std::fs::write(
        dir.path().join("home/senate.toml"),
        "person = \"alice\"\nlocale = \"zh\"\n",
    )
    .unwrap();
    let plain = dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let out = ok(&senate(dir.path(), &plain, &["join", "refund-v2"]));
    assert!(out.contains("已把这个目录绑定到 refund-v2。"), "{out}");
    assert!(out.contains("不是 git 仓库"), "{out}");
    assert!(plain.join(".senate/.gitignore").exists());
    let out = ok(&senate(dir.path(), &plain, &["where"]));
    assert!(out.contains("委员会 refund-v2 (文件)"), "{out}");
    assert!(out.contains("[senator::claude-<session-id>]"), "{out}");
    assert!(
        !out.contains(".senate/ 属于"),
        "no branch outside git: {out}"
    );
    let wd = repo(dir.path(), "r", "a");
    let out = ok(&senate(dir.path(), &wd, &["join", "refund-v2"]));
    assert!(
        out.contains("已把这个工作区在分支 `a` 上绑定到 refund-v2。"),
        "{out}"
    );
    assert!(out.contains("按分支保存"), "{out}");
    let out = ok(&senate(dir.path(), &wd, &["leave"]));
    assert!(out.contains("已解除"), "{out}");
    assert!(out.contains("这里现在没有绑定任何委员会"), "{out}");
    let out = ok(&senate(dir.path(), &wd, &["list"]));
    assert!(out.contains("没有保存的"), "{out}");
}

// ---- free ------------------------------------------------------------------------

fn tree_listing(dir: &Path) -> Vec<(PathBuf, u64)> {
    fn walk(d: &Path, out: &mut Vec<(PathBuf, u64)>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push((p, e.metadata().unwrap().len()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}

/// Rewrite a slot's `saved_at` far into the past.
fn age(slot: &Path) {
    let file = slot.join("slot.json");
    let text = std::fs::read_to_string(&file).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["saved_at"] = "2001-01-01T00:00:00Z".into();
    std::fs::write(&file, v.to_string()).unwrap();
}

#[test]
fn free_removes_each_kind_of_stale_copy_and_never_a_live_branchs() {
    let dir = tempfile::tempdir().unwrap();
    let paths = home(dir.path());
    // A live branch's copy, however old.
    let live = repo(dir.path(), "live", "a");
    put(&live, "c-live", "L");
    sync(&paths, &live);
    git(&live, &["switch", "-q", "-c", "other"]);
    sync(&paths, &live);
    // A branch that is gone.
    let gone_branch = repo(dir.path(), "gb", "main");
    git(&gone_branch, &["switch", "-q", "-c", "x"]);
    put(&gone_branch, "c-x", "X");
    sync(&paths, &gone_branch);
    git(&gone_branch, &["switch", "-q", "main"]);
    sync(&paths, &gone_branch);
    git(&gone_branch, &["branch", "-D", "x"]);
    // A repository that is gone.
    let gone_repo = repo(dir.path(), "gr", "a");
    put(&gone_repo, "c-gr", "G");
    sync(&paths, &gone_repo);
    git(&gone_repo, &["switch", "-q", "-c", "b"]);
    sync(&paths, &gone_repo);
    // A detached worktree that is gone.
    let host = repo(dir.path(), "host", "main");
    let linked = dir.path().join("linked");
    git(
        &host,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().unwrap(),
        ],
    );
    put(&linked, "c-d", "D");
    sync(&paths, &linked);
    git(&linked, &["switch", "-q", "-c", "keep"]);
    sync(&paths, &linked);
    git(
        &host,
        &["worktree", "remove", "--force", linked.to_str().unwrap()],
    );
    std::fs::remove_dir_all(&gone_repo).unwrap();
    for s in follow::slots(&paths) {
        age(&s.dir);
    }
    let rows = |dir: &Path| -> serde_json::Value {
        serde_json::from_str(&ok(&senate(dir, dir, &["list", "--json"]))).unwrap()
    };
    let listed = rows(dir.path());
    let stale: Vec<_> = listed
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| {
            r["stale"]
                .as_str()
                .map(|s| (r["committee"].as_str().unwrap().to_string(), s.to_string()))
        })
        .collect();
    assert_eq!(stale.len(), 3, "{listed}");
    assert!(stale.contains(&("c-x".into(), "branch gone".into())));
    assert!(stale.contains(&("c-gr".into(), "repository gone".into())));
    assert!(stale.contains(&("c-d".into(), "worktree gone".into())));

    // A dry run changes nothing.
    let snapshot = tree_listing(&dir.path().join("home"));
    let out = ok(&senate(dir.path(), dir.path(), &["free"]));
    assert!(out.contains("would free 3, kept 1"), "{out}");
    assert!(out.contains("dry run"), "{out}");
    assert_eq!(tree_listing(&dir.path().join("home")), snapshot);

    let out = ok(&senate(dir.path(), dir.path(), &["free", "--yes"]));
    assert!(out.contains("freed 3, kept 1"), "{out}");
    let left = rows(dir.path());
    assert_eq!(left.as_array().unwrap().len(), 1, "{left}");
    assert_eq!(
        left[0]["committee"], "c-live",
        "the live branch's copy stays"
    );
    // Again: nothing to do.
    let out = ok(&senate(dir.path(), dir.path(), &["free", "--yes"]));
    assert!(out.contains("freed 0, kept 1"), "{out}");
}

#[test]
fn free_lists_a_senate_log_no_longer_hosted_and_removes_it_only_with_the_extra_flag() {
    // A machine hosts one Senate (`top/`). A Senate once nested under an
    // agent (`under-<agent>/`, still named by a `[[preside]]` entry) is no
    // longer hosted: its log is listed like any other.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join("senates/top")).unwrap();
    std::fs::create_dir_all(home.join("senates/under-claude")).unwrap();
    std::fs::create_dir_all(home.join("senates/under-codex")).unwrap();
    std::fs::create_dir_all(home.join("senates/old")).unwrap();
    std::fs::write(home.join("senates/top/log.db"), "keep").unwrap();
    std::fs::write(home.join("senates/under-claude/log.db"), "keep").unwrap();
    std::fs::write(home.join("senates/under-codex/log.db"), "the log").unwrap();
    std::fs::write(home.join("senates/old/log.db"), vec![0u8; 3000]).unwrap();
    std::fs::write(
        home.join("senate.toml"),
        "person = \"alice\"\n[local]\nspeaker = \"claude\"\n[[preside]]\nagent = \"claude\"\n",
    )
    .unwrap();
    let out = ok(&senate(dir.path(), dir.path(), &["free"]));
    assert!(out.contains("senates/old"), "{out}");
    assert!(out.contains("senates/under-codex"), "{out}");
    assert!(out.contains("senates/under-claude"), "{out}");
    assert!(!out.contains("senates/top"), "{out}");
    // --yes alone keeps logs, and says so.
    let out = ok(&senate(dir.path(), dir.path(), &["free", "--yes"]));
    assert!(
        out.contains("kept") && out.contains("--delete-logs"),
        "{out}"
    );
    assert!(home.join("senates/old/log.db").exists());
    assert!(home.join("senates/under-codex/log.db").exists());
    assert!(home.join("senates/under-claude/log.db").exists());
    // The extra flag needs --yes.
    failed(&senate(dir.path(), dir.path(), &["free", "--delete-logs"]));
    assert!(home.join("senates/old/log.db").exists());
    let out = ok(&senate(
        dir.path(),
        dir.path(),
        &["free", "--yes", "--delete-logs"],
    ));
    assert!(out.contains("logs deleted"), "{out}");
    assert!(!home.join("senates/old").exists());
    assert!(!home.join("senates/under-codex").exists());
    assert!(!home.join("senates/under-claude").exists());
    assert!(home.join("senates/top/log.db").exists(), "still hosted");
}

#[test]
fn free_forgets_old_sessions_and_idle_agents_and_keeps_the_rest() {
    use rusqlite::params;
    use senate_bridge::agent::AgentSource;
    use senate_bridge::session::Runtime;
    use senate_bridge::state::{SessionUpsert, Store};
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("senate.toml"),
        "person = \"alice\"\ncommittees = [\"watched\"]\n",
    )
    .unwrap();
    let db = home.join("state.db");
    let store = Store::open(&db).unwrap();
    let upsert = |id: &str, agent: &str, pid: Option<u32>| SessionUpsert {
        session_id: id.into(),
        cwd: "/w".into(),
        git_root: None,
        branch: None,
        inferred_committee: None,
        socket: None,
        token: None,
        claude_pid: pid,
        transcript_path: None,
        ancestor_pids: Vec::new(),
        runtime: Runtime::Claude,
        agent: path(agent),
        agent_source: AgentSource::Harness,
    };
    for (id, agent, pid) in [
        ("ended-long-ago", "old", None),
        ("process-gone", "gone", Some(2_000_000_000)),
        ("ended-just-now", "fresh", None),
        ("live", "live", Some(std::process::id())),
    ] {
        store.upsert_session(&upsert(id, agent, pid)).unwrap();
    }
    store
        .set_session_state("ended-long-ago", senate_bridge::SessionState::Ended)
        .unwrap();
    store
        .set_session_state("ended-just-now", senate_bridge::SessionState::Ended)
        .unwrap();
    // Cursors: an old idle agent, one with an old session, a recent one,
    // and one on a committee the configuration keeps open.
    for (agent, committee) in [
        ("idle", "refund-v2"),
        ("old", "refund-v2"),
        ("fresh", "refund-v2"),
        ("watching", "watched"),
        ("live", "refund-v2"),
    ] {
        store
            .seed_cursor(
                &path(agent),
                &CommitteeId::new(committee).unwrap(),
                senate_core::Seq(3),
                senate_core::Seq(3),
            )
            .unwrap();
    }
    drop(store);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let ago = |days: i64| (chrono::Utc::now() - chrono::Duration::days(days)).timestamp_millis();
    conn.execute(
        "UPDATE sessions SET last_seen = ?1 WHERE session_id = 'ended-long-ago'",
        params![ago(40)],
    )
    .unwrap();
    conn.execute(
        "UPDATE sessions SET last_seen = ?1 WHERE session_id = 'process-gone'",
        params![ago(3)],
    )
    .unwrap();
    conn.execute(
        "UPDATE sessions SET last_seen = ?1 WHERE session_id = 'ended-just-now'",
        params![ago(0)],
    )
    .unwrap();
    for agent in ["idle", "old", "watching"] {
        conn.execute(
            "UPDATE cursors SET touched = ?1 WHERE agent = ?2",
            params![ago(90), agent],
        )
        .unwrap();
    }
    drop(conn);

    let snapshot = std::fs::read(&db).unwrap().len();
    let out = ok(&senate(dir.path(), dir.path(), &["free"]));
    assert!(out.contains("would free 2 (ended over 7 days ago"), "{out}");
    assert!(
        out.contains("idle") || out.contains("[senator::idle]"),
        "{out}"
    );
    assert!(
        out.contains("old") || out.contains("[senator::old]"),
        "{out}"
    );
    assert!(
        !out.contains("watching"),
        "a committee the configuration keeps: {out}"
    );
    let _ = snapshot;
    let count = |sql: &str| -> i64 {
        rusqlite::Connection::open(&db)
            .unwrap()
            .query_row(sql, [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        count("SELECT COUNT(*) FROM sessions"),
        4,
        "a dry run changed nothing"
    );
    assert_eq!(count("SELECT COUNT(*) FROM cursors"), 5);

    let out = ok(&senate(dir.path(), dir.path(), &["free", "--yes"]));
    assert!(out.contains("freed 2 (ended over 7 days ago"), "{out}");
    assert_eq!(count("SELECT COUNT(*) FROM sessions"), 2);
    let remaining: Vec<String> = {
        let c = rusqlite::Connection::open(&db).unwrap();
        let mut stmt = c
            .prepare("SELECT session_id FROM sessions ORDER BY 1")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(remaining, ["ended-just-now", "live"]);
    // idle (nothing at all for 90 days) goes; old's only session
    // went with it, and its cursor was old too; the watched committee and
    // the recent and live ones stay.
    let agents: Vec<String> = {
        let c = rusqlite::Connection::open(&db).unwrap();
        let mut stmt = c.prepare("SELECT agent FROM cursors ORDER BY 1").unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(agents, ["fresh", "live", "watching"]);
}
