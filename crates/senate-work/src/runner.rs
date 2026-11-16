//! The [`Runner`] seam: something that does a [`WorkOrder`] on its Speaker
//! branch and reports progress for the card.

use async_trait::async_trait;
use senate_core::Locale;
use senate_core::task::{MAX_LABEL_CHARS, Step};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::order::WorkOrder;

/// One step of a run, for the task card: a later progress with the same
/// `id` advances the same step (`done: true`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProgress {
    pub id: String,
    /// Short and safe to show: never code, command lines or secrets.
    pub label: String,
    pub done: bool,
}

impl RunProgress {
    pub fn started(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: clip_label(&label.into()),
            done: false,
        }
    }

    pub fn finished(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            done: true,
            ..Self::started(id, label)
        }
    }

    /// As a card step.
    pub fn to_step(&self) -> Step {
        Step {
            id: self.id.clone(),
            label: self.label.clone(),
            done: self.done,
        }
    }
}

impl From<RunProgress> for Step {
    fn from(p: RunProgress) -> Self {
        Step {
            id: p.id,
            label: p.label,
            done: p.done,
        }
    }
}

/// At most [`MAX_LABEL_CHARS`] characters (a card step's limit).
pub fn clip_label(label: &str) -> String {
    let label = label.trim();
    if label.chars().count() <= MAX_LABEL_CHARS {
        label.to_owned()
    } else {
        let mut s: String = label.chars().take(MAX_LABEL_CHARS - 1).collect();
        s.push('…');
        s
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunStatus {
    Succeeded,
    /// The agent or the runner failed. Any work is still committed and pushed.
    Failed {
        reason: String,
    },
    /// The wall clock ran out. Any work is still committed and pushed.
    TimedOut,
    /// The order broke a rule ([`crate::WorkError`]); nothing ran, nothing was pushed.
    Refused {
        reason: String,
    },
}

impl RunStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// What a run ended with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOutcome {
    #[serde(flatten)]
    pub status: RunStatus,
    /// The Speaker branch (pushed iff `pushed_commit` is set).
    pub branch: String,
    /// The branch head after the push; `None` when nothing changed or the push failed.
    pub pushed_commit: Option<String>,
    /// What was done, in a sentence or two (the agent's own summary when it gave one).
    pub summary: String,
    /// What a person could do next; on failure, two or three of retry /
    /// change the approach / hand to a person.
    pub next_steps: Vec<String>,
}

impl RunOutcome {
    /// An order refused before anything ran. The summary is in the order's
    /// locale; the reason is a diagnostic and stays as given.
    pub fn refused(order: &WorkOrder, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            summary: match order.locale {
                Locale::Zh => format!("没有运行：{reason}"),
                Locale::En => format!("Not run: {reason}"),
            },
            status: RunStatus::Refused { reason },
            branch: order.branch(),
            pushed_commit: None,
            next_steps: Vec::new(),
        }
    }
}

/// The next steps offered after a run that did not succeed, when the agent
/// gave none: what was done is saved first, then a person is asked what
/// next.
pub fn default_next_steps(branch: &str, pushed: bool, locale: Locale) -> Vec<String> {
    let mut steps = vec![
        locale
            .pick(
                "重试：让 Speaker 再跑一次",
                "Retry: have the Speaker run it again",
            )
            .to_owned(),
        locale
            .pick(
                "换个做法：说明思路后让 Speaker 重做",
                "Change the approach: explain how, and have the Speaker redo it",
            )
            .to_owned(),
    ];
    steps.push(match (pushed, locale) {
        (true, Locale::Zh) => format!("交给一个人：从分支 {branch} 接着做"),
        (true, Locale::En) => format!("Hand it to a person: continue from branch {branch}"),
        (false, _) => locale
            .pick(
                "交给一个人：从检查点分支接着做",
                "Hand it to a person: continue from the checkpoint branch",
            )
            .to_owned(),
    });
    steps
}

/// Does a [`WorkOrder`] on `order.branch()`.
///
/// A runner never returns an error: every failure is a [`RunOutcome`] whose
/// status says what went wrong, and work done before a failure or timeout
/// is committed and pushed: save first, then ask. `progress` may be dropped by
/// the receiver at any time; the run goes on.
#[async_trait]
pub trait Runner: Send + Sync {
    async fn run(&self, order: &WorkOrder, progress: mpsc::Sender<RunProgress>) -> RunOutcome;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_clipped_to_card_limit() {
        let p = RunProgress::started("a", "x".repeat(200));
        assert_eq!(p.label.chars().count(), MAX_LABEL_CHARS);
        assert!(p.label.ends_with('…'));
        assert_eq!(RunProgress::finished("a", " ok ").label, "ok");
    }

    #[test]
    fn outcome_serializes_flat() {
        let o = RunOutcome {
            status: RunStatus::Failed {
                reason: "boom".into(),
            },
            branch: "b".into(),
            pushed_commit: Some("abc".into()),
            summary: "s".into(),
            next_steps: vec![],
        };
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(v["status"], "failed");
        assert_eq!(v["reason"], "boom");
        let back: RunOutcome = serde_json::from_value(v).unwrap();
        assert_eq!(back, o);
    }

    #[test]
    fn next_steps_are_written_in_the_orders_language() {
        let zh = default_next_steps("feature/x/speaker-1", true, Locale::Zh);
        assert_eq!(zh[0], "重试：让 Speaker 再跑一次");
        assert_eq!(zh[2], "交给一个人：从分支 feature/x/speaker-1 接着做");
        let en = default_next_steps("feature/x/speaker-1", false, Locale::En);
        assert_eq!(en.len(), 3);
        assert_eq!(en[0], "Retry: have the Speaker run it again");
        assert_eq!(
            en[2],
            "Hand it to a person: continue from the checkpoint branch"
        );
        let mut order = crate::order::sample_order();
        assert!(
            RunOutcome::refused(&order, "x")
                .summary
                .starts_with("没有运行")
        );
        order.locale = Locale::En;
        assert_eq!(RunOutcome::refused(&order, "x").summary, "Not run: x");
    }
}
