//! Checkpoints: the handover note a session's agent leaves in the log.
//!
//! The agent keeps three Markdown files in its worktree,
//! `<git root>/.senate/{PROGRESS,DECISIONS,OPEN}.md`. A checkpoint reads
//! them (after the `checkpoint` tool has written whatever it was given),
//! adds what git says about the worktree — branch, commit, `origin`'s URL
//! without credentials, and whether the commit is on a remote-tracking
//! branch — and the daemon appends it as a `checkpoint` event
//! ([`senate_core::CheckpointPayload`]). Files too big for
//! [`MAX_CHECKPOINT_BYTES`] are cut, largest first.
//!
//! The bridge never pushes on its own: only `checkpoint(push=true)` (what
//! `/senate handoff` calls) runs `git push origin HEAD:<branch>`, and it
//! refuses on a detached `HEAD` or without an `origin` remote; the
//! checkpoint is written either way, with the refusal reported.
//!
//! Every git command runs with a timeout and never prompts
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
    let run = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output();
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
    git(dir, &["rev-parse", "HEAD"], GIT_TIMEOUT)
        .await
        .ok()
        .filter(|s| !s.is_empty())
}

/// `HEAD`'s branch, `None` when detached.
pub async fn branch(dir: &Path) -> Option<String> {
    git(dir, &["symbolic-ref", "--short", "-q", "HEAD"], GIT_TIMEOUT)
        .await
        .ok()
        .filter(|s| !s.is_empty())
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

/// Whether `commit` is on some remote-tracking branch, as this clone last
/// saw its remotes (no network).
pub async fn is_pushed(dir: &Path, commit: &str) -> bool {
    git(dir, &["branch", "-r", "--contains", commit], GIT_TIMEOUT)
        .await
        .is_ok_and(|out| !out.trim().is_empty())
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
    git(dir, &["push", "origin", &refspec], PUSH_TIMEOUT)
        .await
        .map(|out| {
            if out.is_empty() {
                format!("pushed {branch} to origin")
            } else {
                out
            }
        })
}

fn file_paths(root: &Path) -> [(PathBuf, &'static str); 3] {
    let dir = root.join(CHECKPOINT_DIR);
    [
        (dir.join(PROGRESS_FILE), PROGRESS_FILE),
        (dir.join(DECISIONS_FILE), DECISIONS_FILE),
        (dir.join(OPEN_FILE), OPEN_FILE),
    ]
}

/// Write the given files under `<root>/.senate/`; the names written.
pub fn write_files(root: &Path, files: &CheckpointFiles) -> std::io::Result<Vec<&'static str>> {
    let [progress, decisions, open] = file_paths(root);
    let mut written = Vec::new();
    for ((path, name), text) in [
        (progress, &files.progress),
        (decisions, &files.decisions),
        (open, &files.open),
    ] {
        if let Some(text) = text {
            // The directory comes with its own `.gitignore`: nothing in
            // it is ever committed.
            crate::session::ensure_senate_dir(root)?;
            let mut text = text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            std::fs::write(&path, text)?;
            written.push(name);
        }
    }
    Ok(written)
}

/// The three files as they are now; a missing or unreadable file is `None`.
pub fn read_files(root: &Path) -> CheckpointFiles {
    let [progress, decisions, open] = file_paths(root);
    let read = |(path, _): (PathBuf, &str)| {
        std::fs::read(path)
            .ok()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    };
    CheckpointFiles {
        progress: read(progress),
        decisions: read(decisions),
        open: read(open),
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

/// What a checkpoint of the worktree at `root` says now. `transcript`: the
/// session's transcript path, recorded only as existing or not.
pub async fn snapshot(
    root: &Path,
    trigger: CheckpointTrigger,
    transcript: Option<&str>,
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
        files: read_files(root),
        session_transcript: transcript.is_some_and(|t| Path::new(t).exists()),
        trigger,
    };
    let cut = fit(&mut payload);
    Ok((payload, cut))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            snapshot(dir.path(), CheckpointTrigger::Tool, None)
                .await
                .is_err(),
            "not a git worktree"
        );
    }
}
