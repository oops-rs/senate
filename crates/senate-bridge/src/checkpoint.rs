//! Checkpoints: the handover note a session's agent leaves in the log.
//!
//! The daemon combines submitted checkpoint notes with read-only Git metadata.
//! Explicit checkpoints record the submitted notes; automatic checkpoints
//! read the current files beneath the verified worktree.
//!
////! Every git command runs with a timeout and never prompts
//! (`GIT_TERMINAL_PROMPT=0`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use senate_core::continuity::{
    CHECKPOINT_DIR, DECISIONS_FILE, MAX_CHECKPOINT_BYTES, OPEN_FILE, PROGRESS_FILE,
};
use senate_core::{CheckpointFiles, CheckpointPayload, CheckpointTrigger};

/// Bound on a local git query.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(5);
/// Bound on `git push`.
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(120);
/// Appended to a file the cap cut.
pub const TRUNCATED: &str = "\n\n…(truncated to fit the checkpoint)";

/// Run git in `dir`; its trimmed stdout, or why it failed.
pub async fn git(dir: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let directory = crate::safe_path::Directory::open(dir).map_err(|error| error.to_string())?;
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // SAFETY: the child only makes the async-signal-safe fchdir syscall
    // before exec. The captured descriptor lives through the syscall.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(move || rustix::process::fchdir(directory.fd()).map_err(Into::into));
    }
    let run = command.output();
    match tokio::time::timeout(timeout, run).await {
        Err(_) => Err(format!("git {} timed out", args.join(" "))),
        Ok(Err(e)) => Err(format!("running git: {e}")),
        Ok(Ok(out)) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(Ok(out)) => {
            let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(if err.is_empty() {
                format!("git {} failed ({})", args.join(" "), out.status)
            } else {
                err
            })
        }
    }
}

/// The worktree root of `dir`, if it is in one.
pub async fn toplevel(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"], GIT_TIMEOUT)
        .await
        .ok()
        .map(PathBuf::from)
}

/// The commit `HEAD` names in `dir`, if any.
pub async fn head(dir: &Path) -> Option<String> {
    if let Some(read) = read_pinned(dir).await {
        return read.and_then(|repository| repository.head);
    }
    git(dir, &["rev-parse", "HEAD"], GIT_TIMEOUT)
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

/// `HEAD`'s branch, `None` when detached.
pub async fn branch(dir: &Path) -> Option<String> {
    if let Some(read) = read_pinned(dir).await {
        return read.and_then(|repository| repository.branch);
    }
    git(dir, &["symbolic-ref", "--short", "-q", "HEAD"], GIT_TIMEOUT)
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

/// What the repository's own files say of `dir` ([`crate::git_read`]):
/// `Some(None)` for no repository, and `None` when `git` must be asked.
///
/// The checkpointer asks these two things of every live session every
/// minute, and a `git` process each time was the largest thing left in an
/// idle daemon. [`git`] runs in a directory it has opened component by
/// component, following no link; the files are read only while `dir` names
/// that same directory, before the read and after it.
async fn read_pinned(dir: &Path) -> Option<Option<crate::git_read::Repository>> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let pinned = crate::safe_path::Directory::open(&dir).ok()?;
        read_in(&dir, &pinned)
    })
    .await
    .ok()
    .flatten()
}

fn read_in(
    dir: &Path,
    pinned: &crate::safe_path::Directory,
) -> Option<Option<crate::git_read::Repository>> {
    use std::os::unix::fs::MetadataExt;

    let same = || {
        let held = rustix::fs::fstat(pinned.fd()).ok()?;
        let named = std::fs::metadata(dir).ok()?;
        // The casts only widen: the fields' types differ by platform.
        #[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
        let is = named.dev() == held.st_dev as u64 && named.ino() == held.st_ino as u64;
        is.then_some(())
    };
    same()?;
    let answer = crate::git_read::read(dir);
    same()?;
    match answer {
        crate::git_read::Answer::Repository(repository) => Some(Some(repository)),
        crate::git_read::Answer::NotARepository => Some(None),
        crate::git_read::Answer::Uncertain => None,
    }
}

/// `origin`'s URL without credentials, if there is an `origin`.
pub async fn origin_url(dir: &Path) -> Option<String> {
    git(dir, &["remote", "get-url", "origin"], GIT_TIMEOUT)
        .await
        .ok()
        .filter(|s| !s.is_empty())
        .map(|url| strip_credentials(&url))
}

/// `https://user:token@host/x` → `https://host/x`; anything else unchanged.
pub fn strip_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if path.is_empty() && !rest.contains('/') {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}/{path}")
    }
}

/// Whether `commit` is on the current branch's upstream, as this clone last
/// saw its remotes (no network).
pub async fn is_pushed(dir: &Path, commit: &str) -> bool {
    let Ok(upstream) = git(
        dir,
        &["rev-parse", "--symbolic-full-name", "@{upstream}"],
        GIT_TIMEOUT,
    )
    .await
    else {
        return false;
    };
    upstream.starts_with("refs/remotes/")
        && git(
            dir,
            &["merge-base", "--is-ancestor", commit, &upstream],
            GIT_TIMEOUT,
        )
        .await
        .is_ok()
}

/// `git push origin HEAD:<branch>`, refused on a detached `HEAD` or
/// without an `origin`. Git's own summary on success.
pub async fn push(dir: &Path) -> Result<String, String> {
    let Some(branch) = branch(dir).await else {
        return Err("HEAD is detached: there is no branch to push".into());
    };
    if origin_url(dir).await.is_none() {
        return Err("there is no `origin` remote to push to".into());
    }
    let refspec = format!("HEAD:{branch}");
    let mut args = vec!["-c", "core.hooksPath=/dev/null", "push"];
    if git(
        dir,
        &["config", "--get", &format!("branch.{branch}.merge")],
        GIT_TIMEOUT,
    )
    .await
    .is_err()
    {
        args.push("--set-upstream");
    }
    args.extend(["origin", &refspec]);
    git(dir, &args, PUSH_TIMEOUT).await.map(|out| {
        if out.is_empty() {
            format!("pushed {branch} to origin")
        } else {
            out
        }
    })
}

/// Write checkpoint files relative to a pinned worktree directory.
pub fn write_files(root: &Path, files: &CheckpointFiles) -> std::io::Result<Vec<&'static str>> {
    if files.progress.is_none() && files.decisions.is_none() && files.open.is_none() {
        return Ok(Vec::new());
    }
    let dir = crate::safe_path::Directory::open(root)?.child(CHECKPOINT_DIR, true)?;
    let mut written = Vec::new();
    if matches!(dir.read(".gitignore", 1), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        dir.replace(".gitignore", b"*\n", 2)?;
    }
    for (name, text) in [
        (PROGRESS_FILE, &files.progress),
        (DECISIONS_FILE, &files.decisions),
        (OPEN_FILE, &files.open),
    ] {
        if let Some(text) = text {
            let mut text = text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            dir.replace(name, text.as_bytes(), MAX_CHECKPOINT_BYTES)?;
            written.push(name);
        }
    }
    Ok(written)
}

fn read_files(root: &Path) -> CheckpointFiles {
    let dir =
        crate::safe_path::Directory::open(root).and_then(|root| root.child(CHECKPOINT_DIR, false));
    let read = |name| {
        dir.as_ref()
            .ok()?
            .read(name, MAX_CHECKPOINT_BYTES)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    CheckpointFiles {
        progress: read(PROGRESS_FILE),
        decisions: read(DECISIONS_FILE),
        open: read(OPEN_FILE),
    }
}

/// Cut the largest file in half (on a char boundary) until the payload
/// fits [`MAX_CHECKPOINT_BYTES`]. Whether anything was cut.
pub fn fit(payload: &mut CheckpointPayload) -> bool {
    let mut cut = false;
    while payload.size() > MAX_CHECKPOINT_BYTES {
        let files = &mut payload.files;
        let largest = [&mut files.progress, &mut files.decisions, &mut files.open]
            .into_iter()
            .filter_map(|f| f.as_mut())
            .max_by_key(|f| f.len());
        let Some(text) = largest else { break };
        let keep = text.len().saturating_sub(TRUNCATED.len()) / 2;
        let mut at = keep.min(text.len());
        while !text.is_char_boundary(at) {
            at -= 1;
        }
        text.truncate(at);
        text.push_str(TRUNCATED);
        cut = true;
    }
    cut
}

/// Read the Git facts for a checkpoint and bound the caller-supplied notes.
pub async fn snapshot(
    root: &Path,
    trigger: CheckpointTrigger,
    transcript: Option<&str>,
    files: Option<CheckpointFiles>,
) -> Result<(CheckpointPayload, bool), String> {
    let Some(commit) = head(root).await else {
        return Err(format!(
            "{} is not a git worktree with a commit; a checkpoint names a branch and a commit",
            root.display()
        ));
    };
    let (branch, remote_url, pushed) =
        tokio::join!(branch(root), origin_url(root), is_pushed(root, &commit));
    let mut payload = CheckpointPayload {
        branch: branch.unwrap_or_else(|| "HEAD".into()),
        commit,
        pushed,
        remote_url,
        files: files.unwrap_or_else(|| read_files(root)),
        session_transcript: transcript.is_some_and(|t| Path::new(t).exists()),
        trigger,
    };
    let cut = fit(&mut payload);
    Ok((payload, cut))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        (output.status.success() && !text.trim().is_empty()).then(|| text.trim().to_owned())
    }

    /// The branch and the commit are what `git` says, whether the files
    /// settled them or `git` was asked.
    #[tokio::test]
    async fn the_branch_and_head_are_gits_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("repo");
        std::fs::create_dir(&root).unwrap();
        let agree = |root: PathBuf| async move {
            assert_eq!(
                branch(&root).await,
                run_git(&root, &["symbolic-ref", "--short", "-q", "HEAD"])
            );
            assert_eq!(head(&root).await, run_git(&root, &["rev-parse", "HEAD"]));
        };
        // No repository, then one with no commit, a commit, a tag sharing
        // the branch's name (the files do not settle that), detached.
        agree(root.clone()).await;
        run_git(&root, &["init", "-q", "-b", "main"]);
        agree(root.clone()).await;
        assert_eq!(branch(&root).await.as_deref(), Some("main"));
        assert_eq!(head(&root).await, None);
        std::fs::write(root.join("a"), "a").unwrap();
        run_git(&root, &["add", "a"]);
        run_git(&root, &["commit", "-q", "-m", "a"]);
        agree(root.clone()).await;
        assert!(head(&root).await.is_some());
        run_git(&root, &["tag", "main"]);
        agree(root.clone()).await;
        assert_eq!(branch(&root).await.as_deref(), Some("heads/main"));
        run_git(&root, &["tag", "-d", "main"]);
        run_git(&root, &["checkout", "-q", "--detach"]);
        agree(root.clone()).await;
        assert_eq!(branch(&root).await, None);
    }

    /// The files are read only while the path still names the directory
    /// that was opened: another directory moved into its place is not read.
    #[test]
    fn a_directory_swapped_under_its_path_is_left_to_git() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (root, other) = (base.join("repo"), base.join("other"));
        for repo in [&root, &other] {
            std::fs::create_dir(repo).unwrap();
            run_git(repo, &["init", "-q", "-b", "main"]);
        }
        let pinned = crate::safe_path::Directory::open(&root).unwrap();
        if crate::git_read::read(&root) == crate::git_read::Answer::Uncertain {
            // An environment the files are never read in.
            return;
        }
        assert!(matches!(read_in(&root, &pinned), Some(Some(_))));
        std::fs::rename(&root, base.join("moved")).unwrap();
        std::fs::rename(&other, &root).unwrap();
        assert!(read_in(&root, &pinned).is_none());
    }

    #[test]
    fn credentials_are_stripped() {
        assert_eq!(
            strip_credentials("https://bob:ghp_x@github.com/acme/pay.git"),
            "https://github.com/acme/pay.git"
        );
        assert_eq!(
            strip_credentials("git@github.com:acme/pay.git"),
            "git@github.com:acme/pay.git"
        );
        assert_eq!(strip_credentials("ssh://git@host:22/x"), "ssh://host:22/x");
    }

    #[test]
    fn oversized_files_are_cut_largest_first() {
        let mut p = CheckpointPayload {
            branch: "b".into(),
            commit: "c".into(),
            pushed: false,
            remote_url: None,
            files: CheckpointFiles {
                progress: Some("进".repeat(40_000)),
                decisions: Some("d".repeat(10)),
                open: None,
            },
            session_transcript: false,
            trigger: CheckpointTrigger::Interval,
        };
        assert!(fit(&mut p));
        assert!(p.check().is_ok());
        assert!(p.files.progress.as_ref().unwrap().ends_with(TRUNCATED));
        assert_eq!(p.files.decisions.as_deref(), Some("dddddddddd"));
    }

    #[test]
    fn automatic_notes_refuse_links_and_linked_note_directories() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let note = outside.path().join(OPEN_FILE);
        std::fs::write(&note, "outside fixture").unwrap();
        let dir = root.path().join(CHECKPOINT_DIR);
        symlink(outside.path(), &dir).unwrap();
        assert_eq!(read_files(root.path()), CheckpointFiles::default());
        assert!(
            write_files(
                root.path(),
                &CheckpointFiles {
                    open: Some("inside".into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
        std::fs::remove_file(&dir).unwrap();
        std::fs::create_dir(&dir).unwrap();
        symlink(&note, dir.join(OPEN_FILE)).unwrap();
        assert_eq!(read_files(root.path()), CheckpointFiles::default());
        assert_eq!(std::fs::read_to_string(note).unwrap(), "outside fixture");
    }

    #[tokio::test]
    async fn files_round_trip_through_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let written = write_files(
            dir.path(),
            &CheckpointFiles {
                progress: Some("- done".into()),
                decisions: None,
                open: Some("- seq 3".into()),
            },
        )
        .unwrap();
        assert_eq!(written, [PROGRESS_FILE, OPEN_FILE]);
        let files = read_files(dir.path());
        assert_eq!(files.progress.as_deref(), Some("- done\n"));
        assert_eq!(files.decisions, None);
        assert!(
            snapshot(dir.path(), CheckpointTrigger::Tool, None, None)
                .await
                .is_err(),
            "not a git worktree"
        );
    }
}
