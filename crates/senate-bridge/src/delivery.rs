//! Delivery adapters: how a batch of directed events reaches a
//! session.
//!
//! `deliver(session, batch) -> Injected | Queued | Spawned | Unavailable`.
//! The daemon's ladder ([`Ladder::new_default`]) is:
//!
//! 1. [`ClaudeInboxAdapter`] — a Claude Code session: write the batch into
//!    its inbox socket ([`inbox`]). → `Injected`.
//! 2. [`codex::CodexAppServerAdapter`] — a Codex session: start a turn in
//!    its thread, or steer the running one, through the app-server
//!    ([`codex`]). → `Injected`.
//! 3. [`HookQueueAdapter`] — hold it for the session's next SessionStart /
//!    UserPromptSubmit hook. → `Queued`.
//! 4. Nothing else: with no session at all the batch is `Unavailable`.
//!
//! The companion session ([`crate::companion`]) sits beside the
//! ladder rather than on it: it is for a question no session can take, so
//! there is no session to hand a batch to. The daemon starts it and reports
//! `Spawned`.
//!
//! Each of the first two answers `Unavailable` (not unreachable) for the
//! other runtime's sessions, so a session only ever meets its own adapter.
//! Claude Channels would be a further implementation of [`DeliveryAdapter`].

pub mod codex;
pub mod inbox;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use senate_core::{CommitteeId, Event};
use serde::{Deserialize, Serialize};

use crate::session::{Runtime, Session};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DeliveryOutcome {
    /// The session has the batch now.
    Injected,
    /// The session will get the batch at its next hook.
    Queued,
    /// A new session was started to receive it: a companion session.
    Spawned,
    /// This adapter could not deliver. `unreachable` means the session itself
    /// is gone (its socket refused us), so lower rungs should not hold the
    /// batch for it either.
    Unavailable { reason: String, unreachable: bool },
}

impl DeliveryOutcome {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
            unreachable: false,
        }
    }
}

/// Directed events for one session, already rendered as one message.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub committee: CommitteeId,
    /// In `seq` order.
    pub events: Vec<Event>,
    pub text: String,
}

#[async_trait]
pub trait DeliveryAdapter: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    async fn deliver(&self, session: &Session, batch: &Batch) -> DeliveryOutcome;
}

/// Rung 1: the Claude Code session inbox socket.
#[derive(Debug, Clone)]
pub struct ClaudeInboxAdapter {
    pub options: inbox::PushOptions,
}

#[async_trait]
impl DeliveryAdapter for ClaudeInboxAdapter {
    fn name(&self) -> &'static str {
        "claude-inbox"
    }

    async fn deliver(&self, session: &Session, batch: &Batch) -> DeliveryOutcome {
        if session.runtime != Runtime::Claude {
            return DeliveryOutcome::unavailable("not a Claude Code session");
        }
        let Some(socket) = &session.socket else {
            return DeliveryOutcome::unavailable("session reported no inbox socket");
        };
        match inbox::push(socket, session.token.as_deref(), &batch.text, &self.options).await {
            Ok(None) => DeliveryOutcome::Injected,
            // Silence is the only reply known to mean "taken". Anything else
            // may be a refusal (the inbox refuses bursts), so the batch is
            // not counted as delivered; the reply is logged as sent, so a
            // live run shows its format.
            Ok(Some(reply)) => {
                tracing::warn!(
                    session = %session.session_id,
                    "the inbox answered a push instead of staying silent; treating the batch as \
                     not delivered. Reply verbatim: {reply}"
                );
                DeliveryOutcome::unavailable(format!("the inbox replied: {reply}"))
            }
            Err(e) => DeliveryOutcome::Unavailable {
                unreachable: e.is_unreachable(),
                reason: e.to_string(),
            },
        }
    }
}

/// Rung 2: hold the batch for the session's hooks. The daemon records the
/// hold; the adapter only accepts.
#[derive(Debug, Clone, Copy, Default)]
pub struct HookQueueAdapter;

#[async_trait]
impl DeliveryAdapter for HookQueueAdapter {
    fn name(&self) -> &'static str {
        "hook-queue"
    }

    async fn deliver(&self, _session: &Session, _batch: &Batch) -> DeliveryOutcome {
        DeliveryOutcome::Queued
    }
}

/// What the ladder tried, rung by rung, and where it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderReport {
    pub outcome: DeliveryOutcome,
    pub attempts: Vec<(&'static str, DeliveryOutcome)>,
}

/// Adapters tried in order until one does not answer `Unavailable`.
#[derive(Clone)]
pub struct Ladder {
    rungs: Vec<Arc<dyn DeliveryAdapter>>,
}

impl Ladder {
    pub fn new(rungs: Vec<Arc<dyn DeliveryAdapter>>) -> Self {
        Self { rungs }
    }

    /// Claude Code sessions only: the inbox, then the hooks.
    pub fn claude_only(inbox_timeout: Duration) -> Self {
        Self::new(vec![
            Arc::new(Self::inbox(inbox_timeout)),
            Arc::new(HookQueueAdapter),
        ])
    }

    /// The inbox for Claude Code, the app-server for Codex, then the hooks.
    pub fn new_default(inbox_timeout: Duration, codex: codex::CodexAppServerAdapter) -> Self {
        Self::new(vec![
            Arc::new(Self::inbox(inbox_timeout)),
            Arc::new(codex),
            Arc::new(HookQueueAdapter),
        ])
    }

    fn inbox(timeout: Duration) -> ClaudeInboxAdapter {
        ClaudeInboxAdapter {
            options: inbox::PushOptions {
                timeout,
                linger: Duration::from_millis(200),
            },
        }
    }

    pub async fn deliver(&self, session: &Session, batch: &Batch) -> LadderReport {
        let mut attempts = Vec::new();
        for rung in &self.rungs {
            let outcome = rung.deliver(session, batch).await;
            attempts.push((rung.name(), outcome.clone()));
            match outcome {
                DeliveryOutcome::Unavailable {
                    unreachable: false, ..
                } => continue,
                other => {
                    return LadderReport {
                        outcome: other,
                        attempts,
                    };
                }
            }
        }
        LadderReport {
            outcome: DeliveryOutcome::unavailable("no delivery adapter could take the batch"),
            attempts,
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::session::SessionState;

    fn session(socket: Option<&str>) -> Session {
        Session {
            session_id: "s".into(),
            runtime: Default::default(),
            agent: "claude".parse().unwrap(),
            agent_source: crate::agent::AgentSource::Harness,
            cwd: "/".into(),
            git_root: None,
            branch: None,
            inferred_committee: None,
            committee_source: None,
            joined_committee: None,
            socket: socket.map(Into::into),
            token: Some("t".into()),
            claude_pid: None,
            transcript_path: None,
            state: SessionState::Live,
            started_at: Utc::now(),
            last_seen: Utc::now(),
            socket_ok: None,
        }
    }

    fn batch() -> Batch {
        Batch {
            committee: CommitteeId::new("f").unwrap(),
            events: Vec::new(),
            text: "hi".into(),
        }
    }

    #[tokio::test]
    async fn no_socket_falls_through_to_the_hook_queue() {
        let report = Ladder::claude_only(Duration::from_secs(1))
            .deliver(&session(None), &batch())
            .await;
        assert_eq!(report.outcome, DeliveryOutcome::Queued);
        assert_eq!(report.attempts.len(), 2);
    }

    #[tokio::test]
    async fn an_inbox_that_answers_falls_through_to_the_hook_queue() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut sink = Vec::new();
            stream.read_to_end(&mut sink).await.unwrap();
            stream.write_all(b"queue full\n").await.unwrap();
        });
        let report = Ladder::claude_only(Duration::from_secs(1))
            .deliver(&session(path.to_str()), &batch())
            .await;
        assert_eq!(report.outcome, DeliveryOutcome::Queued);
        assert!(matches!(
            &report.attempts[0].1,
            DeliveryOutcome::Unavailable { reason, unreachable: false } if reason.contains("queue full")
        ));
    }

    #[tokio::test]
    async fn refused_socket_stops_the_ladder() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.sock");
        let report = Ladder::claude_only(Duration::from_secs(1))
            .deliver(&session(missing.to_str()), &batch())
            .await;
        assert!(matches!(
            report.outcome,
            DeliveryOutcome::Unavailable {
                unreachable: true,
                ..
            }
        ));
        assert_eq!(report.attempts.len(), 1);
    }
}
