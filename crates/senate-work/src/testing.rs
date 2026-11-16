//! Scriptable fakes for the Speaker's tests and the e2e suite (feature
//! `testing`). No git, no Docker, no network.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::forge::{Delivery, Forge, ForgeError, PrRef, PrRequest};
use crate::order::{RepoAllowlist, RepoRef, WorkOrder};
use crate::runner::{RunOutcome, RunProgress, RunStatus, Runner, default_next_steps};

/// One scripted run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeRun {
    /// Sent in order before the outcome.
    pub steps: Vec<RunProgress>,
    /// Pause after each step (lets a test watch the card advance).
    pub step_delay: Duration,
    pub status: RunStatus,
    /// `None` = nothing pushed.
    pub pushed_commit: Option<String>,
    pub summary: String,
    /// Empty and not succeeded: the default next steps are used.
    pub next_steps: Vec<String>,
}

impl FakeRun {
    /// Two steps, succeeded, pushed `fake-commit`.
    pub fn succeeded() -> Self {
        Self {
            steps: vec![
                RunProgress::started("tool-1", "读 .senate/progress.md"),
                RunProgress::finished("tool-1", "读 .senate/progress.md"),
                RunProgress::started("tool-2", "改 src/lib.rs"),
                RunProgress::finished("tool-2", "改 src/lib.rs"),
            ],
            step_delay: Duration::ZERO,
            status: RunStatus::Succeeded,
            pushed_commit: Some("fake-commit".into()),
            summary: "Done.".into(),
            next_steps: Vec::new(),
        }
    }

    pub fn failed(reason: impl Into<String>) -> Self {
        Self {
            status: RunStatus::Failed {
                reason: reason.into(),
            },
            summary: "Partly done.".into(),
            ..Self::succeeded()
        }
    }

    /// Timed out with partial work pushed.
    pub fn timed_out() -> Self {
        Self {
            status: RunStatus::TimedOut,
            summary: "Stopped at the time limit.".into(),
            ..Self::succeeded()
        }
    }

    pub fn with_steps(mut self, steps: Vec<RunProgress>) -> Self {
        self.steps = steps;
        self
    }

    pub fn with_step_delay(mut self, d: Duration) -> Self {
        self.step_delay = d;
        self
    }

    pub fn with_pushed_commit(mut self, c: Option<&str>) -> Self {
        self.pushed_commit = c.map(str::to_owned);
        self
    }

    pub fn with_summary(mut self, s: impl Into<String>) -> Self {
        self.summary = s.into();
        self
    }

    pub fn with_next_steps(mut self, steps: Vec<String>) -> Self {
        self.next_steps = steps;
        self
    }
}

#[derive(Debug, Default)]
struct RunnerState {
    script: VecDeque<FakeRun>,
    calls: Vec<WorkOrder>,
}

/// A [`Runner`] that plays scripted runs in order (then [`FakeRun::succeeded`]),
/// records every order, and applies the real refusal rules when given an
/// allowlist.
#[derive(Debug, Clone, Default)]
pub struct FakeRunner {
    state: Arc<Mutex<RunnerState>>,
    allowed: Option<RepoAllowlist>,
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Validate orders against `allowed`, as the real runner does.
    pub fn with_allowlist(mut self, allowed: RepoAllowlist) -> Self {
        self.allowed = Some(allowed);
        self
    }

    pub fn push(&self, run: FakeRun) -> &Self {
        if let Ok(mut s) = self.state.lock() {
            s.script.push_back(run);
        }
        self
    }

    /// Every order run so far.
    pub fn calls(&self) -> Vec<WorkOrder> {
        self.state
            .lock()
            .map(|s| s.calls.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Runner for FakeRunner {
    async fn run(&self, order: &WorkOrder, progress: mpsc::Sender<RunProgress>) -> RunOutcome {
        let run = match self.state.lock() {
            Ok(mut s) => {
                s.calls.push(order.clone());
                s.script.pop_front().unwrap_or_else(FakeRun::succeeded)
            }
            Err(_) => FakeRun::succeeded(),
        };
        if let Some(allowed) = &self.allowed
            && let Err(e) = order.validate(allowed)
        {
            return RunOutcome::refused(order, e.to_string());
        }
        for step in run.steps {
            let _ = progress.send(step).await;
            if !run.step_delay.is_zero() {
                tokio::time::sleep(run.step_delay).await;
            }
        }
        let branch = order.branch();
        let next_steps = if run.next_steps.is_empty() && !run.status.is_success() {
            default_next_steps(&branch, run.pushed_commit.is_some(), order.locale)
        } else {
            run.next_steps
        };
        RunOutcome {
            status: run.status,
            branch,
            pushed_commit: run.pushed_commit,
            summary: run.summary,
            next_steps,
        }
    }
}

#[derive(Debug, Default)]
struct ForgeState {
    opened: Vec<PrRequest>,
    open: HashMap<(String, String), PrRef>,
    next_number: u64,
    fail_next: VecDeque<ForgeError>,
}

/// A [`Forge`] that "opens" PRs at `https://github.com/<slug>/pull/<n>`,
/// idempotently per head branch, and records requests. Can be scripted to
/// fail, to refuse repos outside an allowlist, or to act as [`crate::NoForge`].
#[derive(Debug, Clone, Default)]
pub struct FakeForge {
    state: Arc<Mutex<ForgeState>>,
    allowed: Option<RepoAllowlist>,
    branch_only: bool,
}

impl FakeForge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuse repos outside `allowed`, as [`crate::GhForge`] does.
    pub fn with_allowlist(mut self, allowed: RepoAllowlist) -> Self {
        self.allowed = Some(allowed);
        self
    }

    /// Deliver branches only, like [`crate::NoForge`].
    pub fn branch_only(mut self) -> Self {
        self.branch_only = true;
        self
    }

    /// The next call fails with `err`.
    pub fn fail_next(&self, err: ForgeError) -> &Self {
        if let Ok(mut s) = self.state.lock() {
            s.fail_next.push_back(err);
        }
        self
    }

    /// Every request that opened a new PR.
    pub fn opened(&self) -> Vec<PrRequest> {
        self.state
            .lock()
            .map(|s| s.opened.clone())
            .unwrap_or_default()
    }

    fn take_failure(&self) -> Option<ForgeError> {
        self.state
            .lock()
            .ok()
            .and_then(|mut s| s.fail_next.pop_front())
    }
}

#[async_trait]
impl Forge for FakeForge {
    async fn open_pr(&self, req: &PrRequest) -> Result<Delivery, ForgeError> {
        if let Some(allowed) = &self.allowed {
            allowed.check(&req.repo.slug)?;
        }
        if let Some(e) = self.take_failure() {
            return Err(e);
        }
        if self.branch_only {
            return Ok(Delivery::Branch {
                repo: req.repo.clone(),
                branch: req.head.clone(),
            });
        }
        let mut s = self
            .state
            .lock()
            .map_err(|_| ForgeError::Command("fake forge poisoned".into()))?;
        let key = (req.repo.slug.clone(), req.head.clone());
        if let Some(existing) = s.open.get(&key) {
            return Ok(Delivery::PullRequest(existing.clone()));
        }
        s.next_number += 1;
        let pr = PrRef {
            url: format!(
                "https://github.com/{}/pull/{}",
                req.repo.slug, s.next_number
            ),
            number: s.next_number,
        };
        s.open.insert(key, pr.clone());
        s.opened.push(req.clone());
        Ok(Delivery::PullRequest(pr))
    }

    async fn find_pr(&self, repo: &RepoRef, head: &str) -> Result<Option<PrRef>, ForgeError> {
        if let Some(allowed) = &self.allowed {
            allowed.check(&repo.slug)?;
        }
        if let Some(e) = self.take_failure() {
            return Err(e);
        }
        Ok(self
            .state
            .lock()
            .ok()
            .and_then(|s| s.open.get(&(repo.slug.clone(), head.to_owned())).cloned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::sample_order;

    #[tokio::test]
    async fn fake_runner_plays_script_and_records() {
        let runner = FakeRunner::new();
        runner.push(FakeRun::timed_out());
        let (tx, mut rx) = mpsc::channel(16);
        let o = sample_order();
        let out = runner.run(&o, tx).await;
        assert_eq!(out.status, RunStatus::TimedOut);
        assert_eq!(out.branch, "feature/refund-v2/speaker-7");
        assert_eq!(out.next_steps.len(), 3);
        let mut steps = Vec::new();
        while let Ok(p) = rx.try_recv() {
            steps.push(p);
        }
        assert_eq!(steps.len(), 4);
        assert_eq!(runner.calls(), vec![o.clone()]);
        // Script exhausted: succeeded.
        let (tx, _rx) = mpsc::channel(16);
        assert!(runner.run(&o, tx).await.status.is_success());
    }

    #[tokio::test]
    async fn fake_runner_refuses_like_the_real_one() {
        let runner = FakeRunner::new().with_allowlist(RepoAllowlist::new(["o/other"]));
        let (tx, _rx) = mpsc::channel(16);
        let out = runner.run(&sample_order(), tx).await;
        assert!(matches!(out.status, RunStatus::Refused { .. }));
        assert_eq!(out.pushed_commit, None);
    }

    #[tokio::test]
    async fn fake_forge_is_idempotent() {
        let forge = FakeForge::new();
        let o = sample_order();
        let req = PrRequest::for_run(&o, &RunOutcome::refused(&o, "x"));
        let a = forge.open_pr(&req).await.unwrap();
        let b = forge.open_pr(&req).await.unwrap();
        assert_eq!(a, b);
        assert_eq!(forge.opened().len(), 1);
        assert!(forge.find_pr(&o.repo, &req.head).await.unwrap().is_some());
        forge.fail_next(ForgeError::Command("down".into()));
        assert!(forge.open_pr(&req).await.is_err());
        let b = FakeForge::new().branch_only();
        assert!(matches!(
            b.open_pr(&req).await.unwrap(),
            Delivery::Branch { .. }
        ));
    }
}
