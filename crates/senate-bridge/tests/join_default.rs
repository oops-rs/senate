//! `senate join` with no name: the committee is derived from the repository
//! and branch, printed with how, and nothing under `.git` is ever written.
//! Through the built binary, in temp homes and temp repositories.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_senate");

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

/// Every file under `dir`, by relative path, with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path.strip_prefix(base).unwrap().display().to_string();
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

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

fn bound(wd: &Path) -> String {
    std::fs::read_to_string(wd.join(".senate/committee")).unwrap()
}

fn speak_chinese(dir: &Path) {
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::write(
        dir.join("home/senate.toml"),
        "person = \"alice\"\nlocale = \"zh\"\n",
    )
    .unwrap();
}

#[test]
fn on_the_default_branch_the_repository_name_alone() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "Senate_Work", "main");
    let before = snapshot(&wd.join(".git"));
    let out = ok(&senate(dir.path(), &wd, &["join"]));
    assert!(
        out.contains(
            "No name given: derived `senate-work` from the repository `Senate_Work` \
             (the folder of its main worktree: no origin remote); the default branch `main` \
             adds nothing. Give a name to choose another: `senate join <name>`."
        ),
        "{out}"
    );
    assert!(out.contains("to senate-work."), "{out}");
    assert_eq!(bound(&wd), "senate-work\n");
    assert_eq!(snapshot(&wd.join(".git")), before);
}

#[test]
fn on_a_feature_branch_the_branch_is_added_and_the_origin_names_the_repository() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "checkout", "main");
    git(
        &wd,
        &["remote", "add", "origin", "git@example.com:org/senate.git"],
    );
    git(&wd, &["switch", "-q", "-c", "fix/inbox"]);
    let before = snapshot(&wd.join(".git"));
    let out = ok(&senate(dir.path(), &wd, &["join"]));
    assert!(
        out.contains(
            "No name given: derived `senate-fix-inbox` from the repository `senate` \
             (its origin remote) and the branch `fix/inbox`. Give a name to choose another: \
             `senate join <name>`."
        ),
        "{out}"
    );
    assert_eq!(bound(&wd), "senate-fix-inbox\n");
    assert_eq!(snapshot(&wd.join(".git")), before);
}

#[test]
fn origin_head_decides_which_branch_is_the_default() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "r", "develop");
    git(
        &wd,
        &["remote", "add", "origin", "https://example.com/o/proj"],
    );
    git(
        &wd,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/develop",
        ],
    );
    ok(&senate(dir.path(), &wd, &["join"]));
    assert_eq!(bound(&wd), "proj\n");
    git(&wd, &["switch", "-q", "-c", "main"]);
    ok(&senate(dir.path(), &wd, &["join"]));
    assert_eq!(bound(&wd), "proj-main\n");
}

#[test]
fn a_detached_head_is_the_repository_alone() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "r", "topic");
    git(
        &wd,
        &["remote", "add", "origin", "ssh://git@h:22/o/proj.git"],
    );
    git(&wd, &["switch", "-q", "--detach"]);
    let out = ok(&senate(dir.path(), &wd, &["join"]));
    assert!(out.contains("a detached HEAD adds nothing"), "{out}");
    assert_eq!(bound(&wd), "proj\n");
}

#[test]
fn a_linked_worktree_derives_the_same_repository_name() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "Main_Folder", "main");
    let linked = dir.path().join("elsewhere");
    git(
        &wd,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "topic",
            linked.to_str().unwrap(),
        ],
    );
    ok(&senate(dir.path(), &linked, &["join"]));
    assert_eq!(bound(&linked), "main-folder-topic\n");
    ok(&senate(dir.path(), &wd, &["join"]));
    assert_eq!(bound(&wd), "main-folder\n");
}

#[test]
fn a_clone_named_differently_derives_the_origins_name() {
    let dir = tempfile::tempdir().unwrap();
    let src = repo(dir.path(), "src", "main");
    let bare = dir.path().join("Upstream.Name.git");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            "--bare",
            src.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    let clone = dir.path().join("my-local-copy");
    git(
        dir.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    let before = snapshot(&clone.join(".git"));
    let out = ok(&senate(dir.path(), &clone, &["join"]));
    assert!(
        out.contains("derived `upstream-name` from the repository `Upstream.Name`"),
        "{out}"
    );
    assert_eq!(bound(&clone), "upstream-name\n");
    assert_eq!(snapshot(&clone.join(".git")), before);
}

#[test]
fn outside_git_the_directory_names_it() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("Scratch Pad");
    std::fs::create_dir_all(&plain).unwrap();
    let out = ok(&senate(dir.path(), &plain, &["join"]));
    assert!(
        out.contains(
            "derived `scratch-pad` from the directory `Scratch Pad` (not a git repository)."
        ),
        "{out}"
    );
    assert_eq!(bound(&plain), "scratch-pad\n");
}

#[test]
fn nothing_usable_is_refused_in_both_languages_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("日本語");
    std::fs::create_dir_all(&plain).unwrap();
    let err = failed(&senate(dir.path(), &plain, &["join"]));
    assert!(err.contains("none can be derived here"), "{err}");
    assert!(err.contains("`日本語`"), "{err}");
    assert!(err.contains("`senate join <name>`"), "{err}");
    assert!(!plain.join(".senate").exists());

    speak_chinese(dir.path());
    let err = failed(&senate(dir.path(), &plain, &["join"]));
    assert!(err.contains("这里也推不出名字"), "{err}");
    assert!(err.contains("`senate join <name>`"), "{err}");
    assert!(!plain.join(".senate").exists());

    // An explicit name still works there.
    ok(&senate(dir.path(), &plain, &["join", "refund-v2"]));
    assert_eq!(bound(&plain), "refund-v2\n");
}

#[test]
fn an_explicit_name_is_taken_as_given_and_derives_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "r", "fix/x");
    let out = ok(&senate(dir.path(), &wd, &["join", "refund-v2"]));
    assert!(!out.contains("No name given"), "{out}");
    assert_eq!(bound(&wd), "refund-v2\n");
}

#[test]
fn chinese_says_the_same() {
    let dir = tempfile::tempdir().unwrap();
    speak_chinese(dir.path());
    let wd = repo(dir.path(), "r", "main");
    git(&wd, &["remote", "add", "origin", "git@h:o/senate.git"]);
    git(&wd, &["switch", "-q", "-c", "fix/inbox"]);
    let out = ok(&senate(dir.path(), &wd, &["join"]));
    assert!(
        out.contains(
            "没有给名字：由仓库 `senate`（来自它的 origin 远程）和分支 `fix/inbox` 推出 \
             `senate-fix-inbox`。想用别的名字：`senate join <name>`。"
        ),
        "{out}"
    );
}

#[test]
fn where_says_what_join_would_call_an_unbound_directory() {
    let dir = tempfile::tempdir().unwrap();
    let wd = repo(dir.path(), "r", "main");
    git(&wd, &["remote", "add", "origin", "git@h:o/senate.git"]);
    git(&wd, &["switch", "-q", "-c", "fix/inbox"]);
    let before = snapshot(&wd.join(".git"));
    let out = ok(&senate(dir.path(), &wd, &["where"]));
    assert!(
        out.contains("`senate join` with no name would call it `senate-fix-inbox` here."),
        "{out}"
    );
    assert!(out.contains("senate join <committee>"), "{out}");
    assert_eq!(snapshot(&wd.join(".git")), before);

    // Once bound, the line is gone.
    ok(&senate(dir.path(), &wd, &["join"]));
    let out = ok(&senate(dir.path(), &wd, &["where"]));
    assert!(!out.contains("would call it"), "{out}");

    // No name derivable: it says a name must be given.
    let odd = dir.path().join("日本語");
    std::fs::create_dir_all(&odd).unwrap();
    let out = ok(&senate(dir.path(), &odd, &["where"]));
    assert!(out.contains("with no name cannot derive one here"), "{out}");
    assert!(out.contains("give a name"), "{out}");

    speak_chinese(dir.path());
    let out = ok(&senate(dir.path(), &odd, &["where"]));
    assert!(out.contains("在这里推不出名字"), "{out}");
    let plain = dir.path().join("Plain");
    std::fs::create_dir_all(&plain).unwrap();
    let out = ok(&senate(dir.path(), &plain, &["where"]));
    assert!(out.contains("在这里会叫它 `plain`"), "{out}");
}
