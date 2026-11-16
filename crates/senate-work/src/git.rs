//! Host-side git for a run: clone, branch, commit, push.
//!
//! The git directory lives *outside* the directory the agent's container
//! mounts, so nothing the agent writes (hooks, config, `core.fsmonitor`,
//! `url.*.insteadOf`) is ever read by the git that holds the credential.
//! Every git here is hermetic: no global or system config, no hooks, no
//! prompts. The credential is configured by path and passed to git only on
//! the commands that talk to the remote; its content is never read by this
//! process, put on a command line, or logged.

use std::path::{Path, PathBuf};
use std::time::Duration;

use senate_core::Address;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::order::WorkOrder;
use crate::proc::{Redactor, output, sh_quote, tail};

/// The author and committer of every Speaker commit.
pub const SPEAKER_NAME: &str = "Senate Speaker";
pub const SPEAKER_EMAIL: &str = "speaker@senate";

/// How the host authenticates to the repository's remote.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitCredential {
    /// No credential: clone and push the order's URL as is (local or
    /// already-authorized remotes; tests).
    #[default]
    None,
    /// A deploy key (read-write on the listed repositories). The clone URL is
    /// `git@<git_host>:<owner>/<name>.git`.
    SshKey {
        key_file: PathBuf,
        /// Pinned host keys; without it, the first key seen is accepted
        /// (`StrictHostKeyChecking=accept-new`).
        #[serde(default)]
        known_hosts_file: Option<PathBuf>,
    },
    /// A token file (e.g. a fine-grained GitHub token with contents
    /// read/write). The clone URL is `https://<git_host>/<owner>/<name>.git`.
    Token { token_file: PathBuf },
}

impl GitCredential {
    /// The URL to clone: derived from the slug for SSH/token credentials so a
    /// checkpoint's URL can never point the credential at another host.
    pub fn clone_url(&self, order: &WorkOrder, git_host: &str) -> String {
        match self {
            Self::None => order.repo.url.clone(),
            Self::SshKey { .. } => format!("git@{git_host}:{}.git", order.repo.slug),
            Self::Token { .. } => format!("https://{git_host}/{}.git", order.repo.slug),
        }
    }

    /// `-c` options and environment for commands that reach the remote.
    fn remote_setup(&self) -> (Vec<String>, Vec<(&'static str, String)>) {
        match self {
            Self::None => (Vec::new(), Vec::new()),
            Self::SshKey {
                key_file,
                known_hosts_file,
            } => {
                let mut ssh = format!(
                    "ssh -i {} -o IdentitiesOnly=yes -o BatchMode=yes -F /dev/null",
                    sh_quote(&key_file.to_string_lossy())
                );
                match known_hosts_file {
                    Some(k) => ssh.push_str(&format!(
                        " -o StrictHostKeyChecking=yes -o UserKnownHostsFile={}",
                        sh_quote(&k.to_string_lossy())
                    )),
                    None => ssh.push_str(" -o StrictHostKeyChecking=accept-new"),
                }
                (Vec::new(), vec![("GIT_SSH_COMMAND", ssh)])
            }
            Self::Token { token_file } => {
                let helper = format!(
                    "!f() {{ test \"$1\" = get || exit 0; echo username=x-access-token; \
                     printf 'password=%s\\n' \"$(cat {})\"; }}; f",
                    sh_quote(&token_file.to_string_lossy())
                );
                (
                    vec![
                        "-c".into(),
                        "credential.helper=".into(),
                        "-c".into(),
                        format!("credential.helper={helper}"),
                    ],
                    Vec::new(),
                )
            }
        }
    }
}

/// git for one run: a git dir and a separate work tree.
#[derive(Debug, Clone)]
pub(crate) struct Git {
    pub(crate) bin: PathBuf,
    pub(crate) git_dir: PathBuf,
    pub(crate) work_tree: PathBuf,
    pub(crate) credential: GitCredential,
    pub(crate) timeout: Duration,
    pub(crate) redactor: Redactor,
}

impl Git {
    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.bin);
        cmd.env_clear();
        for key in ["PATH", "HOME", "TMPDIR"] {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ASKPASS", "/bin/false")
            .env("LC_ALL", "C")
            .args(["-c", "core.hooksPath=/dev/null"])
            .args(["-c", "core.fsmonitor=false"])
            .args(["-c", "protocol.ext.allow=never"])
            .args(["-c", &format!("user.name={SPEAKER_NAME}")])
            .args(["-c", &format!("user.email={SPEAKER_EMAIL}")]);
        cmd
    }

    async fn exec(&self, mut cmd: Command, what: &str) -> Result<String, String> {
        let out = output(&mut cmd, self.timeout).await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(self.redactor.redact(&format!(
                "git {what} failed ({}): {}",
                out.status,
                tail(&out.stderr, 3)
            )))
        }
    }

    /// git with `--git-dir`/`--work-tree` set.
    async fn run(&self, args: &[&str], what: &str) -> Result<String, String> {
        let mut cmd = self.base_command();
        cmd.current_dir(&self.work_tree)
            .arg("--git-dir")
            .arg(&self.git_dir)
            .arg("--work-tree")
            .arg(&self.work_tree)
            .args(args);
        self.exec(cmd, what).await
    }

    /// git with the credential, for commands that reach the remote.
    async fn run_remote(&self, args: &[&str], what: &str, in_repo: bool) -> Result<String, String> {
        let (opts, env) = self.credential.remote_setup();
        let mut cmd = self.base_command();
        cmd.args(&opts);
        for (k, v) in env {
            cmd.env(k, v);
        }
        if in_repo {
            cmd.arg("--git-dir")
                .arg(&self.git_dir)
                .arg("--work-tree")
                .arg(&self.work_tree);
        }
        cmd.args(args);
        self.exec(cmd, what).await
    }

    /// Clone `url` (no checkout) with the git dir apart from the work tree.
    pub(crate) async fn clone(&self, url: &str) -> Result<(), String> {
        let git_dir = self.git_dir.to_string_lossy().into_owned();
        let work = self.work_tree.to_string_lossy().into_owned();
        self.run_remote(
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                "--separate-git-dir",
                &git_dir,
                "--",
                url,
                &work,
            ],
            "clone",
            false,
        )
        .await?;
        // The `.git` file would point the agent's tools at our git dir.
        let dotgit = self.work_tree.join(".git");
        tokio::fs::remove_file(&dotgit)
            .await
            .map_err(|e| format!("removing {}: {e}", dotgit.display()))?;
        Ok(())
    }

    /// Create `branch` at `commit` and check it out.
    pub(crate) async fn start_branch(&self, branch: &str, commit: &str) -> Result<(), String> {
        let resolved = self
            .run(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{commit}^{{commit}}"),
                ],
                "rev-parse",
            )
            .await
            .map_err(|_| format!("commit {commit} is not in the repository"))?;
        self.run(
            &["checkout", "--quiet", "-b", branch, &resolved],
            "checkout",
        )
        .await?;
        Ok(())
    }

    /// Keep a path out of every commit.
    pub(crate) async fn exclude(&self, pattern: &str) -> Result<(), String> {
        let info = self.git_dir.join("info");
        tokio::fs::create_dir_all(&info)
            .await
            .map_err(|e| e.to_string())?;
        let path = info.join("exclude");
        let mut body = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        body.push_str(&format!("\n{pattern}\n"));
        tokio::fs::write(&path, body)
            .await
            .map_err(|e| e.to_string())
    }

    /// Commit everything in the work tree, if anything changed. Returns
    /// whether a commit was made.
    pub(crate) async fn commit_all(&self, message: &str) -> Result<bool, String> {
        self.run(&["add", "--all"], "add").await?;
        let staged = self
            .run(&["diff", "--cached", "--name-only"], "diff")
            .await?;
        if staged.is_empty() {
            return Ok(false);
        }
        self.run(
            &["commit", "--quiet", "--no-verify", "-m", message],
            "commit",
        )
        .await?;
        Ok(true)
    }

    pub(crate) async fn head(&self) -> Result<String, String> {
        self.rev_parse("HEAD").await
    }

    pub(crate) async fn rev_parse(&self, rev: &str) -> Result<String, String> {
        self.run(&["rev-parse", "--verify", "--quiet", rev], "rev-parse")
            .await
    }

    /// Push HEAD to `refs/heads/<branch>` on origin. Never forced.
    pub(crate) async fn push(&self, branch: &str) -> Result<(), String> {
        self.run_remote(
            &[
                "push",
                "--quiet",
                "--no-verify",
                "origin",
                &format!("HEAD:refs/heads/{branch}"),
            ],
            "push",
            true,
        )
        .await
        .map(|_| ())
    }
}

/// `Speaker run <id> for [senator::<person>]: <instruction first line>`,
/// with the details in the body. English whatever the order's locale: a
/// commit message is git history, which tools read as much as people.
pub fn commit_message(order: &WorkOrder, partial: Option<&str>) -> String {
    let requester = Address::senior(&order.requester).in_text();
    let subject = crate::stream::clip(
        &format!(
            "Speaker run {} for {requester}: {}",
            order.run_id,
            order.instruction_headline()
        ),
        120,
    );
    let mut body = format!(
        "{subject}\n\nRequested by {} ({requester}) and done by the Senate Speaker.\n\
         Committee: {}\nCheckpoint: {} of {}\n",
        order.on_behalf_of.trim(),
        order.committee,
        order.checkpoint_ref(),
        Address::senator(&order.target_agent).in_text(),
    );
    if let Some(why) = partial {
        body.push_str(&format!("Partial work: {why}\n"));
    }
    body
}

/// A unique, docker-safe container name for a run.
pub fn container_name(order: &WorkOrder) -> String {
    format!("senate-speaker-{}-{}", order.committee, order.run_id)
}

pub(crate) fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::sample_order;

    #[test]
    fn clone_urls_come_from_the_slug() {
        let mut o = sample_order();
        o.repo.url = "git@evil.example:example/refund.git".into();
        let ssh = GitCredential::SshKey {
            key_file: "/k".into(),
            known_hosts_file: None,
        };
        assert_eq!(
            ssh.clone_url(&o, "github.com"),
            "git@github.com:example/refund.git"
        );
        let tok = GitCredential::Token {
            token_file: "/t".into(),
        };
        assert_eq!(
            tok.clone_url(&o, "github.com"),
            "https://github.com/example/refund.git"
        );
        assert_eq!(GitCredential::None.clone_url(&o, "github.com"), o.repo.url);
    }

    #[test]
    fn credentials_go_by_path_only() {
        let (opts, env) = GitCredential::Token {
            token_file: "/run/secrets/it's".into(),
        }
        .remote_setup();
        assert!(env.is_empty());
        assert!(opts[3].contains(r"cat '/run/secrets/it'\''s'"));
        let (opts, env) = GitCredential::SshKey {
            key_file: "/k".into(),
            known_hosts_file: Some("/kh".into()),
        }
        .remote_setup();
        assert!(opts.is_empty());
        assert_eq!(env[0].0, "GIT_SSH_COMMAND");
        assert!(env[0].1.contains("-i '/k'"));
        assert!(env[0].1.contains("StrictHostKeyChecking=yes"));
    }

    #[test]
    fn commit_message_shape() {
        let o = sample_order();
        let m = commit_message(&o, None);
        assert!(m.starts_with("Speaker run 7 for [senator::bob]: 把 alice 的重试路径做完\n\n"));
        assert!(m.contains(
            "Checkpoint: feature/refund-v2/alice@0123456789ab of [senator::alice/claude]"
        ));
        assert!(!m.contains("user:"));
        assert!(!m.contains("Partial"));
        assert!(commit_message(&o, Some("timed out")).contains("Partial work: timed out"));
        assert_eq!(container_name(&o), "senate-speaker-refund-v2-7");
    }
}
