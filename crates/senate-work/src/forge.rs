//! The [`Forge`] seam: opening the PR for a pushed Speaker branch.
//! [`GhForge`] is GitHub through `gh`; [`NoForge`] just names the branch.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use senate_core::{Address, Locale};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::order::{RepoAllowlist, RepoRef, WorkError, WorkOrder, valid_branch_name};
use crate::proc::{Redactor, output, tail};
use crate::runner::{RunOutcome, RunStatus};
use crate::stream::clip;

/// A pull request to open (or find) for a speaker branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRequest {
    pub repo: RepoRef,
    /// What it merges into (the checkpoint branch's upstream base).
    pub base: String,
    /// The Speaker branch.
    pub head: String,
    pub title: String,
    pub body: String,
    /// Open as a draft (a run that did not succeed).
    #[serde(default)]
    pub draft: bool,
}

impl PrRequest {
    /// The PR for a finished run, in the order's locale: title and body
    /// say who asked, whose checkpoint it started from, the instruction,
    /// and that the Speaker opened it. Senators are named by their
    /// bracketed mentions, the same in either language.
    pub fn for_run(order: &WorkOrder, outcome: &RunOutcome) -> Self {
        let locale = order.locale;
        let requester = Address::senior(&order.requester).in_text();
        let agent = Address::senator(&order.target_agent).in_text();
        let title = clip(
            &match locale {
                Locale::Zh => format!(
                    "[speaker] {}：{}（代 {requester}）",
                    order.committee,
                    order.instruction_headline()
                ),
                Locale::En => format!(
                    "[speaker] {}: {} (for {requester})",
                    order.committee,
                    order.instruction_headline()
                ),
            },
            200,
        );
        let state = match (&outcome.status, locale) {
            (RunStatus::Succeeded, _) => locale.pick("已完成", "finished").to_owned(),
            (RunStatus::Failed { reason }, Locale::Zh) => format!("提前停止，部分完成（{reason}）"),
            (RunStatus::Failed { reason }, Locale::En) => {
                format!("stopped early, partial work ({reason})")
            }
            (RunStatus::TimedOut, _) => locale
                .pick("到达时限，部分完成", "hit its time limit, partial work")
                .to_owned(),
            (RunStatus::Refused { reason }, Locale::Zh) => format!("被拒绝（{reason}）"),
            (RunStatus::Refused { reason }, Locale::En) => format!("refused ({reason})"),
        };
        let quoted: String = order
            .instruction
            .trim()
            .lines()
            .map(|l| format!("> {l}\n"))
            .collect();
        let (checkpoint, committee, run, summary) = (
            order.checkpoint_ref(),
            &order.committee,
            order.run_id,
            outcome.summary.trim(),
        );
        let person = order.on_behalf_of.trim();
        let mut body = match locale {
            Locale::Zh => format!(
                "由 Senate 的 Speaker 代 {requester}（{person}）开启。\n\n\
                 **指令**\n\n{quoted}\n\
                 **检查点：** {agent} 的 `{checkpoint}`（委员会 `{committee}`）\n\
                 **运行：** {run} — {state}\n\n\
                 **总结**\n\n{summary}\n"
            ),
            Locale::En => format!(
                "Opened by the Senate Speaker for {requester} ({person}).\n\n\
                 **Instruction**\n\n{quoted}\n\
                 **Checkpoint:** `{checkpoint}` of {agent} (committee `{committee}`)\n\
                 **Run:** {run} — {state}\n\n\
                 **Summary**\n\n{summary}\n"
            ),
        };
        if !outcome.next_steps.is_empty() {
            body.push_str(locale.pick("\n**下一步**\n\n", "\n**Next steps**\n\n"));
            for s in &outcome.next_steps {
                body.push_str(&format!("- {s}\n"));
            }
        }
        body.push_str(locale.pick(
            "\n---\n由 Senate 的 Speaker 开启；开发者的分支没有被改动。\n",
            "\n---\nopened by the Senate Speaker; the developer's branch was not changed.\n",
        ));
        Self {
            repo: order.repo.clone(),
            base: order.pr_base.clone(),
            head: outcome.branch.clone(),
            title,
            body,
            draft: !outcome.status.is_success(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrRef {
    pub url: String,
    pub number: u64,
}

/// What delivery produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Delivery {
    PullRequest(PrRef),
    /// No forge: the pushed branch is the deliverable.
    Branch {
        repo: RepoRef,
        branch: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ForgeError {
    #[error(transparent)]
    Refused(#[from] WorkError),
    /// `gh` failed; the message is scrubbed of the token.
    #[error("{0}")]
    Command(String),
    #[error("unexpected output from the forge: {0}")]
    Parse(String),
}

#[async_trait]
pub trait Forge: Send + Sync {
    /// Open the PR, or return the open one for `req.head` (idempotent).
    async fn open_pr(&self, req: &PrRequest) -> Result<Delivery, ForgeError>;
    /// The open PR whose head is `head`, if any.
    async fn find_pr(&self, repo: &RepoRef, head: &str) -> Result<Option<PrRef>, ForgeError>;
}

/// No forge: the pushed branch is the deliverable, and its name is posted.
#[derive(Debug, Clone, Default)]
pub struct NoForge;

#[async_trait]
impl Forge for NoForge {
    async fn open_pr(&self, req: &PrRequest) -> Result<Delivery, ForgeError> {
        Ok(Delivery::Branch {
            repo: req.repo.clone(),
            branch: req.head.clone(),
        })
    }

    async fn find_pr(&self, _repo: &RepoRef, _head: &str) -> Result<Option<PrRef>, ForgeError> {
        Ok(None)
    }
}

/// ```toml
/// [forge]
/// kind = "github"
/// repos = ["example/refund"]
/// token_file = "/run/secrets/gh_token"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GhForgeConfig {
    #[serde(default = "default_gh_bin")]
    pub gh_bin: PathBuf,
    /// A fine-grained token (contents + pull requests on the listed repos).
    /// Read at each call and given to `gh` as `GH_TOKEN`, for the child only.
    pub token_file: PathBuf,
    /// Only these repositories (`owner/name`).
    #[serde(default, alias = "repos")]
    pub allowed_repos: RepoAllowlist,
    /// `GH_HOST`, for GitHub Enterprise; else github.com.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default = "default_gh_timeout_secs")]
    pub timeout_secs: u64,
    /// Open every PR as a draft, finished runs included (a person marks it
    /// ready). Failed runs are drafts either way.
    #[serde(default)]
    pub always_draft: bool,
}

fn default_gh_bin() -> PathBuf {
    "gh".into()
}

fn default_gh_timeout_secs() -> u64 {
    60
}

impl GhForgeConfig {
    pub fn new(token_file: impl Into<PathBuf>, allowed_repos: RepoAllowlist) -> Self {
        Self {
            gh_bin: default_gh_bin(),
            token_file: token_file.into(),
            allowed_repos,
            host: None,
            timeout_secs: default_gh_timeout_secs(),
            always_draft: false,
        }
    }
}

/// GitHub through the `gh` CLI.
#[derive(Debug, Clone)]
pub struct GhForge {
    config: GhForgeConfig,
}

impl GhForge {
    pub fn new(config: GhForgeConfig) -> Self {
        Self { config }
    }

    /// `gh pr list` arguments to find the open PR for `head`.
    pub fn find_args(repo: &RepoRef, head: &str) -> Vec<String> {
        [
            "pr",
            "list",
            "--repo",
            &repo.slug,
            "--head",
            head,
            "--state",
            "open",
            "--json",
            "number,url",
            "--limit",
            "1",
        ]
        .map(String::from)
        .to_vec()
    }

    /// `gh pr create` arguments.
    pub fn create_args(req: &PrRequest) -> Vec<String> {
        let mut args: Vec<String> = [
            "pr",
            "create",
            "--repo",
            &req.repo.slug,
            "--base",
            &req.base,
            "--head",
            &req.head,
            "--title",
            &req.title,
            "--body",
            &req.body,
        ]
        .map(String::from)
        .to_vec();
        if req.draft {
            args.push("--draft".into());
        }
        args
    }

    fn check(&self, repo: &RepoRef, branches: &[&str]) -> Result<(), ForgeError> {
        self.config.allowed_repos.check(&repo.slug)?;
        for b in branches {
            if !valid_branch_name(b) {
                return Err(WorkError::BadBranch((*b).to_owned()).into());
            }
        }
        Ok(())
    }

    async fn gh(&self, args: &[String]) -> Result<String, ForgeError> {
        let token = tokio::fs::read_to_string(&self.config.token_file)
            .await
            .map_err(|e| {
                ForgeError::Command(format!(
                    "cannot read the token file {}: {}",
                    self.config.token_file.display(),
                    e.kind()
                ))
            })?;
        let token = token.trim();
        let redactor = Redactor::default().with(token);
        let mut cmd = Command::new(&self.config.gh_bin);
        cmd.env_clear();
        for key in ["PATH", "HOME", "TMPDIR"] {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        cmd.env("GH_TOKEN", token)
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("NO_COLOR", "1")
            .env("LC_ALL", "C")
            .args(args);
        if let Some(host) = &self.config.host {
            cmd.env("GH_HOST", host);
        }
        let out = output(&mut cmd, Duration::from_secs(self.config.timeout_secs))
            .await
            .map_err(|e| ForgeError::Command(redactor.redact(&e)))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
        } else {
            Err(ForgeError::Command(redactor.redact(&format!(
                "gh {} {} failed ({}): {}",
                args.first().map(String::as_str).unwrap_or(""),
                args.get(1).map(String::as_str).unwrap_or(""),
                out.status,
                tail(&out.stderr, 3)
            ))))
        }
    }
}

/// The PR number at the end of a PR URL (`…/pull/42`).
pub fn pr_number_from_url(url: &str) -> Option<u64> {
    url.trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()?
        .parse()
        .ok()
}

#[async_trait]
impl Forge for GhForge {
    async fn open_pr(&self, req: &PrRequest) -> Result<Delivery, ForgeError> {
        self.check(&req.repo, &[&req.base, &req.head])?;
        if let Some(existing) = self.find_pr(&req.repo, &req.head).await? {
            return Ok(Delivery::PullRequest(existing));
        }
        let mut args = Self::create_args(req);
        if self.config.always_draft && !req.draft {
            args.push("--draft".into());
        }
        match self.gh(&args).await {
            Ok(out) => {
                let url = out
                    .lines()
                    .map(str::trim)
                    .rfind(|l| l.starts_with("http"))
                    .ok_or_else(|| ForgeError::Parse(clip(&out, 200)))?;
                let number =
                    pr_number_from_url(url).ok_or_else(|| ForgeError::Parse(url.to_owned()))?;
                tracing::info!(repo = %req.repo, head = %req.head, %url, "opened PR");
                Ok(Delivery::PullRequest(PrRef {
                    url: url.to_owned(),
                    number,
                }))
            }
            // A PR opened between the lookup and the create (or `gh` saying it already exists).
            Err(e) => match self.find_pr(&req.repo, &req.head).await {
                Ok(Some(existing)) => Ok(Delivery::PullRequest(existing)),
                _ => Err(e),
            },
        }
    }

    async fn find_pr(&self, repo: &RepoRef, head: &str) -> Result<Option<PrRef>, ForgeError> {
        self.check(repo, &[head])?;
        let out = self.gh(&Self::find_args(repo, head)).await?;
        let found: Vec<PrRef> = serde_json::from_str(if out.is_empty() { "[]" } else { &out })
            .map_err(|_| ForgeError::Parse(clip(&out, 200)))?;
        Ok(found.into_iter().next())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::sample_order;

    fn outcome(status: RunStatus) -> RunOutcome {
        RunOutcome {
            status,
            branch: "feature/refund-v2/speaker-7".into(),
            pushed_commit: Some("abc".into()),
            summary: "Retry path done.".into(),
            next_steps: vec!["Review the cap".into()],
        }
    }

    #[test]
    fn pr_request_for_run() {
        let o = sample_order();
        let r = PrRequest::for_run(&o, &outcome(RunStatus::Succeeded));
        assert_eq!(r.base, "main");
        assert_eq!(r.head, "feature/refund-v2/speaker-7");
        assert!(!r.draft);
        assert_eq!(
            r.title,
            "[speaker] refund-v2：把 alice 的重试路径做完（代 [senator::bob]）"
        );
        for want in [
            "代 [senator::bob]（Bob）开启",
            "> 把 alice 的重试路径做完\n> with tests\n",
            "[senator::alice/claude] 的 `feature/refund-v2/alice@0123456789ab`",
            "由 Senate 的 Speaker 开启",
            "- Review the cap",
        ] {
            assert!(r.body.contains(want), "{want:?} not in {}", r.body);
        }
        let partial = PrRequest::for_run(&o, &outcome(RunStatus::TimedOut));
        assert!(partial.draft);
        assert!(partial.body.contains("部分完成"));
    }

    #[test]
    fn pr_request_in_english() {
        let mut o = sample_order();
        o.locale = Locale::En;
        let r = PrRequest::for_run(&o, &outcome(RunStatus::TimedOut));
        assert_eq!(
            r.title,
            "[speaker] refund-v2: 把 alice 的重试路径做完 (for [senator::bob])"
        );
        for want in [
            "Opened by the Senate Speaker for [senator::bob] (Bob).",
            "`feature/refund-v2/alice@0123456789ab` of [senator::alice/claude]",
            "hit its time limit, partial work",
            "opened by the Senate Speaker; the developer's branch was not changed.",
        ] {
            assert!(r.body.contains(want), "{want:?} not in {}", r.body);
        }
        assert!(!r.body.contains("user:"));
    }

    #[test]
    fn gh_args() {
        let o = sample_order();
        let r = PrRequest::for_run(&o, &outcome(RunStatus::TimedOut));
        let a = GhForge::create_args(&r);
        assert_eq!(
            &a[..8],
            [
                "pr",
                "create",
                "--repo",
                "example/refund",
                "--base",
                "main",
                "--head",
                "feature/refund-v2/speaker-7"
            ]
        );
        assert_eq!(a.last().unwrap(), "--draft");
        assert_eq!(
            GhForge::find_args(&o.repo, "feature/refund-v2/speaker-7").join(" "),
            "pr list --repo example/refund --head feature/refund-v2/speaker-7 --state open --json number,url --limit 1"
        );
        assert_eq!(
            pr_number_from_url("https://github.com/o/n/pull/42\n"),
            Some(42)
        );
        assert_eq!(pr_number_from_url("nope"), None);
    }

    #[tokio::test]
    async fn no_forge_returns_the_branch() {
        let o = sample_order();
        let r = PrRequest::for_run(&o, &outcome(RunStatus::Succeeded));
        assert_eq!(
            NoForge.open_pr(&r).await.unwrap(),
            Delivery::Branch {
                repo: o.repo.clone(),
                branch: "feature/refund-v2/speaker-7".into()
            }
        );
    }

    #[tokio::test]
    async fn gh_forge_refuses_unlisted_repo_without_running_gh() {
        let forge = GhForge::new(GhForgeConfig {
            gh_bin: "/nonexistent/gh".into(),
            ..GhForgeConfig::new("/nonexistent/token", RepoAllowlist::new(["o/other"]))
        });
        let o = sample_order();
        let r = PrRequest::for_run(&o, &outcome(RunStatus::Succeeded));
        assert_eq!(
            forge.open_pr(&r).await,
            Err(ForgeError::Refused(WorkError::RepoNotAllowed(
                "example/refund".into()
            )))
        );
    }
}
