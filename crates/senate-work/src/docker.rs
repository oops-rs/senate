//! [`DockerRunner`]: Claude Code headless (`claude -p`) in a throwaway
//! container over a throwaway checkout.
//!
//! 1. Check the order ([`WorkOrder::validate`]): listed repo, speaker branch.
//! 2. On the host: clone the repo with the credential into a fresh directory
//!    under `work_root`, the git dir apart from the work tree; branch
//!    `<prefix>/<committee>/speaker-<n>` at the checkpoint commit; write the
//!    checkpoint notes to `.senate/` (excluded from commits).
//! 3. In a container: mount only the work tree at `/work` and run `claude -p`
//!    with the coding tools, the turn cap and the model gateway. The
//!    container gets no git credential and no git directory.
//! 4. Stream its `stream-json` into [`RunProgress`] steps. At the wall clock,
//!    kill the container.
//! 5. On the host: commit whatever changed (author Senate Speaker), and
//!    push the branch — on success, on failure and on timeout alike.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::git::{
    Git, GitCredential, SPEAKER_EMAIL, SPEAKER_NAME, commit_message, container_name, path_str,
};
use crate::order::{RepoAllowlist, WorkOrder, check_push_target, checkpoint_notes};
use crate::proc::{Redactor, output};
use crate::prompt::{NOTES_DIR, build_prompt};
use crate::runner::{RunOutcome, RunProgress, RunStatus, Runner, default_next_steps};
use crate::stream::{
    AgentResult, CONTAINER_WORKDIR, StreamItem, StreamParser, clip, parse_closing,
};

/// ```toml
/// [runner]
/// image = "senate-runner:latest"
/// work_root = "/var/lib/senate/runs"        # must be the same path for the docker daemon
/// model = "claude-sonnet-4-5"
/// gateway_base_url = "http://gateway.lan:4000"
/// gateway_key_file = "/run/secrets/runner_gateway_key"
/// allowed_repos = ["example/refund"]
/// git_credential = { kind = "ssh_key", key_file = "/run/secrets/deploy_key" }
/// docker_args = ["--network", "senate-runner", "--memory", "4g", "--cpus", "2"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerRunnerConfig {
    /// The runner image: one with Claude Code installed as `claude`.
    pub image: String,
    /// Fresh checkouts go here, one directory per run, removed afterwards.
    /// When the runner itself runs in a container, the docker daemon must
    /// see this directory at the same path (bind it at the same path).
    pub work_root: PathBuf,
    #[serde(default = "default_docker_bin")]
    pub docker_bin: PathBuf,
    #[serde(default = "default_git_bin")]
    pub git_bin: PathBuf,
    /// `--model` for Claude Code; else its default.
    #[serde(default)]
    pub model: Option<String>,
    /// `ANTHROPIC_BASE_URL` in the container: the same gateway as the Speaker.
    #[serde(default)]
    pub gateway_base_url: Option<String>,
    /// The gateway key, read at each run and passed to the container through
    /// the environment of the `docker` process (never on its command line).
    #[serde(default)]
    pub gateway_key_file: Option<PathBuf>,
    /// The variable the key goes in: `ANTHROPIC_API_KEY` (x-api-key) or
    /// `ANTHROPIC_AUTH_TOKEN` (bearer), depending on the gateway.
    #[serde(default = "default_gateway_key_env")]
    pub gateway_key_env: String,
    /// The host's credential for cloning and pushing.
    #[serde(default)]
    pub git_credential: GitCredential,
    /// Host for clone URLs derived from a repo slug.
    #[serde(default = "default_git_host")]
    pub git_host: String,
    /// Only these repositories may be worked on (`owner/name`).
    #[serde(default)]
    pub allowed_repos: RepoAllowlist,
    /// `--allowedTools`.
    #[serde(default = "default_allowed_tools")]
    pub allowed_tools: Vec<String>,
    /// `--disallowedTools` (deny wins over allow).
    #[serde(default = "default_disallowed_tools")]
    pub disallowed_tools: Vec<String>,
    /// Extra arguments for `claude`, after the ones the runner sets.
    #[serde(default)]
    pub claude_args: Vec<String>,
    /// Extra arguments for `docker run`, before the image (network, limits).
    #[serde(default)]
    pub docker_args: Vec<String>,
    /// `--user` for the container (e.g. the host's `uid:gid`, so the agent
    /// can write the checkout); else the image's user.
    #[serde(default)]
    pub docker_user: Option<String>,
    /// Bound on each git clone/commit/push.
    #[serde(default = "default_git_timeout_secs")]
    pub git_timeout_secs: u64,
}

fn default_docker_bin() -> PathBuf {
    "docker".into()
}

fn default_git_bin() -> PathBuf {
    "git".into()
}

fn default_gateway_key_env() -> String {
    "ANTHROPIC_API_KEY".into()
}

fn default_git_host() -> String {
    "github.com".into()
}

fn default_git_timeout_secs() -> u64 {
    300
}

/// The coding tools. Bash is needed to build and test; the deny list below
/// and the absence of any git credential in the container bound it.
pub fn default_allowed_tools() -> Vec<String> {
    [
        "Read",
        "Edit",
        "MultiEdit",
        "Write",
        "Glob",
        "Grep",
        "LS",
        "NotebookEdit",
        "TodoWrite",
        "Bash",
    ]
    .map(String::from)
    .to_vec()
}

/// No git or forge, no ad-hoc network clients, no environment dumps, no web
/// tools. The runner does the one push itself, from the host.
pub fn default_disallowed_tools() -> Vec<String> {
    [
        "Bash(git:*)",
        "Bash(gh:*)",
        "Bash(curl:*)",
        "Bash(wget:*)",
        "Bash(ssh:*)",
        "Bash(scp:*)",
        "Bash(rsync:*)",
        "Bash(nc:*)",
        "Bash(env:*)",
        "Bash(printenv:*)",
        "Bash(npm publish:*)",
        "Bash(cargo publish:*)",
        "Bash(docker:*)",
        "WebFetch",
        "WebSearch",
    ]
    .map(String::from)
    .to_vec()
}

impl DockerRunnerConfig {
    /// A config with every default, for `image` and `work_root`.
    pub fn new(image: impl Into<String>, work_root: impl Into<PathBuf>) -> Self {
        Self {
            image: image.into(),
            work_root: work_root.into(),
            docker_bin: default_docker_bin(),
            git_bin: default_git_bin(),
            model: None,
            gateway_base_url: None,
            gateway_key_file: None,
            gateway_key_env: default_gateway_key_env(),
            git_credential: GitCredential::None,
            git_host: default_git_host(),
            allowed_repos: RepoAllowlist::default(),
            allowed_tools: default_allowed_tools(),
            disallowed_tools: default_disallowed_tools(),
            claude_args: Vec::new(),
            docker_args: Vec::new(),
            docker_user: None,
            git_timeout_secs: default_git_timeout_secs(),
        }
    }
}

/// See the module docs.
#[derive(Debug, Clone)]
pub struct DockerRunner {
    config: DockerRunnerConfig,
}

impl DockerRunner {
    pub fn new(config: DockerRunnerConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &DockerRunnerConfig {
        &self.config
    }

    /// The `docker run …` arguments for `order` with the work tree at
    /// `work_tree`. Carries no secret: the gateway key is passed by name
    /// (`-e ANTHROPIC_API_KEY`) and its value set in the docker process's
    /// environment only.
    pub fn docker_run_args(&self, order: &WorkOrder, work_tree: &Path) -> Vec<String> {
        let c = &self.config;
        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "--init".into(),
            "--name".into(),
            container_name(order),
            "--cap-drop".into(),
            "ALL".into(),
            "--security-opt".into(),
            "no-new-privileges".into(),
            "--volume".into(),
            format!("{}:{CONTAINER_WORKDIR}", path_str(work_tree)),
            "--workdir".into(),
            CONTAINER_WORKDIR.into(),
        ];
        if let Some(user) = &c.docker_user {
            args.extend([
                "--user".into(),
                user.clone(),
                "--env".into(),
                "HOME=/tmp".into(),
            ]);
        }
        let mut env = vec![
            "DISABLE_AUTOUPDATER=1".to_owned(),
            "DISABLE_TELEMETRY=1".to_owned(),
            "DISABLE_ERROR_REPORTING=1".to_owned(),
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1".to_owned(),
            format!("GIT_AUTHOR_NAME={SPEAKER_NAME}"),
            format!("GIT_AUTHOR_EMAIL={SPEAKER_EMAIL}"),
            format!("GIT_COMMITTER_NAME={SPEAKER_NAME}"),
            format!("GIT_COMMITTER_EMAIL={SPEAKER_EMAIL}"),
        ];
        if let Some(url) = &c.gateway_base_url {
            env.push(format!("ANTHROPIC_BASE_URL={url}"));
        }
        if c.gateway_key_file.is_some() {
            // Name only: docker copies the value from its own environment.
            env.push(c.gateway_key_env.clone());
        }
        for e in env {
            args.extend(["--env".into(), e]);
        }
        args.extend(c.docker_args.iter().cloned());
        args.push(c.image.clone());
        args.extend(self.claude_args(order));
        args
    }

    /// `claude -p <prompt> …`.
    pub fn claude_args(&self, order: &WorkOrder) -> Vec<String> {
        let c = &self.config;
        let mut args: Vec<String> = vec![
            "claude".into(),
            "-p".into(),
            build_prompt(order),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--max-turns".into(),
            order.budget.max_turns.to_string(),
        ];
        if let Some(model) = &c.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if !c.allowed_tools.is_empty() {
            args.push("--allowedTools".into());
            args.push(c.allowed_tools.join(","));
        }
        if !c.disallowed_tools.is_empty() {
            args.push("--disallowedTools".into());
            args.push(c.disallowed_tools.join(","));
        }
        args.extend(c.claude_args.iter().cloned());
        args
    }

    async fn read_key(&self) -> Result<Option<String>, String> {
        let Some(path) = &self.config.gateway_key_file else {
            return Ok(None);
        };
        let key = tokio::fs::read_to_string(path).await.map_err(|e| {
            format!(
                "cannot read the gateway key file {}: {}",
                path.display(),
                e.kind()
            )
        })?;
        Ok(Some(key.trim().to_owned()))
    }
}

/// How the agent phase ended.
enum AgentEnd {
    Finished {
        exit_ok: bool,
        result: Option<AgentResult>,
        stderr: String,
    },
    TimedOut,
    CouldNotStart(String),
}

async fn send(progress: &mpsc::Sender<RunProgress>, p: RunProgress) {
    // The receiver may be gone; the run goes on.
    let _ = progress.send(p).await;
}

#[async_trait]
impl Runner for DockerRunner {
    async fn run(&self, order: &WorkOrder, progress: mpsc::Sender<RunProgress>) -> RunOutcome {
        if let Err(e) = order.validate(&self.config.allowed_repos) {
            tracing::warn!(committee = %order.committee, run = order.run_id, error = %e, "work order refused");
            return RunOutcome::refused(order, e.to_string());
        }
        let branch = order.branch();
        let key = match self.read_key().await {
            Ok(k) => k,
            Err(e) => return failed_before_start(order, e),
        };
        let mut redactor = Redactor::default();
        if let Some(k) = &key {
            redactor = redactor.with(k);
        }

        if let Err(e) = tokio::fs::create_dir_all(&self.config.work_root).await {
            return failed_before_start(order, format!("work_root: {e}"));
        }
        let run_dir = match tempfile::Builder::new()
            .prefix(&format!("run-{}-{}-", order.committee, order.run_id))
            .tempdir_in(&self.config.work_root)
        {
            Ok(d) => d,
            Err(e) => {
                return failed_before_start(order, format!("creating the run directory: {e}"));
            }
        };
        let git = Git {
            bin: self.config.git_bin.clone(),
            git_dir: run_dir.path().join("git"),
            work_tree: run_dir.path().join("work"),
            credential: self.config.git_credential.clone(),
            timeout: Duration::from_secs(self.config.git_timeout_secs),
            redactor: redactor.clone(),
        };

        // Setup.
        let setup_label = labels::setup(order);
        send(&progress, RunProgress::started("setup", &setup_label)).await;
        if let Err(e) = setup(&git, &self.config, order, &branch).await {
            return failed_before_start(order, e);
        }
        send(&progress, RunProgress::finished("setup", setup_label)).await;

        // The agent.
        let locale = order.locale;
        send(
            &progress,
            RunProgress::started(
                "agent",
                locale.pick("编码代理工作中", "Coding agent at work"),
            ),
        )
        .await;
        let end = self
            .run_agent(order, &git.work_tree, key.as_deref(), &progress)
            .await;
        send(
            &progress,
            RunProgress::finished("agent", locale.pick("编码代理结束", "Coding agent done")),
        )
        .await;

        let (status, summary, mut next_steps, partial) = match end {
            AgentEnd::CouldNotStart(e) => {
                let reason = redactor.redact(&e);
                (
                    RunStatus::Failed {
                        reason: reason.clone(),
                    },
                    locale
                        .pick("编码代理没能启动。", "The coding agent could not start.")
                        .to_owned(),
                    Vec::new(),
                    Some(reason),
                )
            }
            AgentEnd::TimedOut => (
                RunStatus::TimedOut,
                labels::timed_out(order),
                Vec::new(),
                Some(format!(
                    "stopped at the {}s wall clock",
                    order.budget.wall_secs
                )),
            ),
            AgentEnd::Finished {
                exit_ok,
                result,
                stderr,
            } => {
                let closing = result
                    .as_ref()
                    .map(|r| parse_closing(&r.text))
                    .unwrap_or_default();
                let summary = closing
                    .summary
                    .clone()
                    .or_else(|| {
                        result
                            .as_ref()
                            .map(|r| first_paragraph(&r.text))
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or_else(|| {
                        locale
                            .pick(
                                "编码代理没有留下总结。",
                                "The coding agent left no summary.",
                            )
                            .to_owned()
                    });
                let summary = redactor.redact(&summary);
                let steps: Vec<String> = closing
                    .next_steps
                    .iter()
                    .map(|s| redactor.redact(s))
                    .collect();
                match result {
                    Some(r) if r.succeeded() && exit_ok => {
                        (RunStatus::Succeeded, summary, steps, None)
                    }
                    Some(r) => {
                        let reason = if r.subtype == "error_max_turns" {
                            format!("hit the turn cap ({})", order.budget.max_turns)
                        } else {
                            format!(
                                "the agent ended with {}",
                                if r.subtype.is_empty() {
                                    "an error"
                                } else {
                                    &r.subtype
                                }
                            )
                        };
                        (
                            RunStatus::Failed {
                                reason: reason.clone(),
                            },
                            summary,
                            steps,
                            Some(reason),
                        )
                    }
                    None => {
                        let reason = redactor
                            .redact(&format!("the agent exited without a result: {stderr}"));
                        (
                            RunStatus::Failed {
                                reason: reason.clone(),
                            },
                            summary,
                            steps,
                            Some(reason),
                        )
                    }
                }
            }
        };

        // Save: commit and push whatever there is, whatever happened.
        let save_label = labels::save(order, &branch);
        send(&progress, RunProgress::started("save", &save_label)).await;
        let saved = save(&git, order, &branch, partial.as_deref()).await;
        let (status, pushed_commit) = match saved {
            Ok(commit) => {
                send(&progress, RunProgress::finished("save", save_label)).await;
                (status, commit)
            }
            Err(e) => {
                // Keep the checkout so nothing done is lost.
                let kept = run_dir.keep();
                tracing::error!(committee = %order.committee, run = order.run_id, dir = %kept.display(), error = %e, "could not save the run's work; checkout kept");
                let reason = match status {
                    RunStatus::Failed { reason } => format!("{reason}; then saving failed: {e}"),
                    RunStatus::TimedOut => format!("timed out; then saving failed: {e}"),
                    _ => format!("saving failed: {e}"),
                };
                return RunOutcome {
                    status: RunStatus::Failed { reason },
                    branch,
                    pushed_commit: None,
                    summary,
                    next_steps: default_next_steps(&order.branch(), false, locale),
                };
            }
        };
        if !status.is_success() && next_steps.is_empty() {
            next_steps = default_next_steps(&branch, pushed_commit.is_some(), locale);
        }
        tracing::info!(committee = %order.committee, run = order.run_id, ?status, pushed = ?pushed_commit, "run finished");
        RunOutcome {
            status,
            branch,
            pushed_commit,
            summary,
            next_steps,
        }
    }
}

impl DockerRunner {
    async fn run_agent(
        &self,
        order: &WorkOrder,
        work_tree: &Path,
        key: Option<&str>,
        progress: &mpsc::Sender<RunProgress>,
    ) -> AgentEnd {
        let mut cmd = Command::new(&self.config.docker_bin);
        cmd.args(self.docker_run_args(order, work_tree))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(k) = key {
            cmd.env(&self.config.gateway_key_env, k);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return AgentEnd::CouldNotStart(format!(
                    "could not start {}: {e}",
                    self.config.docker_bin.display()
                ));
            }
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return AgentEnd::CouldNotStart("no pipes to the container".into());
        };
        let stderr_tail = Arc::new(Mutex::new(VecDeque::<String>::new()));
        let tail_writer = Arc::clone(&stderr_tail);
        let stderr_task = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut t) = tail_writer.lock() {
                    t.push_back(line);
                    if t.len() > 5 {
                        t.pop_front();
                    }
                }
            }
        });

        let mut result = None;
        let work = async {
            let mut parser = StreamParser::new(order.locale);
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                for item in parser.push_line(&line) {
                    match item {
                        StreamItem::Progress(p) => send(progress, p).await,
                        StreamItem::Result(r) => result = Some(r),
                    }
                }
            }
            child.wait().await
        };
        let wall = Duration::from_secs(order.budget.wall_secs);
        let waited = tokio::time::timeout(wall, work).await;
        match waited {
            Ok(status) => {
                let _ = stderr_task.await;
                let stderr = stderr_tail
                    .lock()
                    .map(|t| t.iter().cloned().collect::<Vec<_>>().join(" | "))
                    .unwrap_or_default();
                AgentEnd::Finished {
                    exit_ok: status.map(|s| s.success()).unwrap_or(false),
                    result,
                    stderr: clip(&stderr, 400),
                }
            }
            Err(_) => {
                tracing::warn!(committee = %order.committee, run = order.run_id, "run hit its wall clock; killing the container");
                let mut kill = Command::new(&self.config.docker_bin);
                kill.args(["kill", &container_name(order)]);
                if let Err(e) = output(&mut kill, Duration::from_secs(30)).await {
                    tracing::warn!(error = %e, "docker kill");
                }
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
                stderr_task.abort();
                AgentEnd::TimedOut
            }
        }
    }
}

async fn setup(
    git: &Git,
    config: &DockerRunnerConfig,
    order: &WorkOrder,
    branch: &str,
) -> Result<(), String> {
    let url = config.git_credential.clone_url(order, &config.git_host);
    git.clone(&url).await?;
    git.start_branch(branch, &order.base_commit).await?;
    git.exclude(&format!("/{NOTES_DIR}/")).await?;
    let notes = checkpoint_notes(&order.checkpoint_files);
    if !notes.is_empty() {
        let dir = git.work_tree.join(NOTES_DIR);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| e.to_string())?;
        for (name, body) in notes {
            tokio::fs::write(dir.join(name), body)
                .await
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Commit what changed and push the branch if it moved past the checkpoint.
async fn save(
    git: &Git,
    order: &WorkOrder,
    branch: &str,
    partial: Option<&str>,
) -> Result<Option<String>, String> {
    // The agent cannot reach this, but check again right before the push.
    check_push_target(
        &order.branches,
        &order.committee,
        &order.base_branch,
        branch,
    )
    .map_err(|e| e.to_string())?;
    // The notes are excluded; drop them anyway so a renamed copy is not all that is left.
    let _ = tokio::fs::remove_dir_all(git.work_tree.join(NOTES_DIR)).await;
    git.commit_all(&commit_message(order, partial)).await?;
    let head = git.head().await?;
    let base = git
        .rev_parse(&format!("{}^{{commit}}", order.base_commit))
        .await?;
    if head == base {
        return Ok(None);
    }
    git.push(branch).await?;
    Ok(Some(head))
}

fn failed_before_start(order: &WorkOrder, reason: String) -> RunOutcome {
    tracing::warn!(committee = %order.committee, run = order.run_id, %reason, "run failed before the agent started");
    RunOutcome {
        status: RunStatus::Failed {
            reason: reason.clone(),
        },
        branch: order.branch(),
        pushed_commit: None,
        summary: match order.locale {
            senate_core::Locale::Zh => format!("没能开始：{reason}"),
            senate_core::Locale::En => format!("Could not start: {reason}"),
        },
        next_steps: default_next_steps(&order.branch(), false, order.locale),
    }
}

/// The run's own card steps and summaries, in the order's language.
mod labels {
    use senate_core::Locale;

    use crate::order::WorkOrder;

    pub(super) fn setup(order: &WorkOrder) -> String {
        match order.locale {
            Locale::Zh => format!("从检查点 {} 拉分支", order.checkpoint_ref()),
            Locale::En => format!("Branch from checkpoint {}", order.checkpoint_ref()),
        }
    }

    pub(super) fn save(order: &WorkOrder, branch: &str) -> String {
        match order.locale {
            Locale::Zh => format!("提交并推送 {branch}"),
            Locale::En => format!("Commit and push {branch}"),
        }
    }

    pub(super) fn timed_out(order: &WorkOrder) -> String {
        let minutes = order.budget.wall_secs.div_ceil(60);
        match order.locale {
            Locale::Zh => format!("运行超过 {minutes} 分钟的时限，已停止。"),
            Locale::En => format!("Stopped at the {minutes}-minute time limit."),
        }
    }
}

/// The first paragraph of the agent's final message, clipped.
fn first_paragraph(text: &str) -> String {
    let para = text.trim().split("\n\n").next().unwrap_or("").trim();
    clip(para, 400)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::sample_order;

    fn runner() -> DockerRunner {
        let mut c = DockerRunnerConfig::new("senate-runner:test", "/tmp/runs");
        c.model = Some("claude-sonnet-4-5".into());
        c.gateway_base_url = Some("http://gw:4000".into());
        c.gateway_key_file = Some("/run/secrets/key".into());
        c.docker_args = vec!["--network".into(), "runner".into()];
        DockerRunner::new(c)
    }

    #[test]
    fn docker_args_shape() {
        let o = sample_order();
        let args = runner().docker_run_args(&o, Path::new("/tmp/runs/r1/work"));
        let joined = args.join(" ");
        assert_eq!(&args[..3], ["run", "--rm", "--init"]);
        assert!(joined.contains("--name senate-speaker-refund-v2-7"));
        assert!(joined.contains("--volume /tmp/runs/r1/work:/work --workdir /work"));
        assert!(joined.contains("--cap-drop ALL"));
        assert!(joined.contains("--env ANTHROPIC_BASE_URL=http://gw:4000"));
        // The key by name only.
        assert!(args.windows(2).any(|w| w == ["--env", "ANTHROPIC_API_KEY"]));
        let image = args.iter().position(|a| a == "senate-runner:test").unwrap();
        assert_eq!(&args[image - 2..image], ["--network", "runner"]);
        assert_eq!(&args[image + 1..image + 3], ["claude", "-p"]);
        let rest = &args[image + 4..];
        assert_eq!(
            &rest[..6],
            [
                "--output-format",
                "stream-json",
                "--verbose",
                "--max-turns",
                "200",
                "--model"
            ]
        );
        assert!(rest.iter().any(|a| a.contains("Bash(git:*)")));
        assert!(rest.iter().any(|a| a.starts_with("Read,Edit")));
        // Nothing mounts a credential or the git dir.
        assert!(!joined.contains("/git:"));
        assert!(!joined.contains("/run/secrets"));
    }

    #[test]
    fn config_from_toml_defaults() {
        let c: DockerRunnerConfig = serde_json::from_value(serde_json::json!({
            "image": "i", "work_root": "/w",
            "git_credential": {"kind": "ssh_key", "key_file": "/k"},
            "allowed_repos": ["o/n"]
        }))
        .unwrap();
        assert_eq!(c.docker_bin, PathBuf::from("docker"));
        assert_eq!(c.gateway_key_env, "ANTHROPIC_API_KEY");
        assert_eq!(c.allowed_tools, default_allowed_tools());
        assert!(c.allowed_repos.allows("o/n"));
        assert!(matches!(c.git_credential, GitCredential::SshKey { .. }));
    }

    #[test]
    fn first_paragraph_clips() {
        assert_eq!(first_paragraph("a\nb\n\nc"), "a\nb");
    }
}
