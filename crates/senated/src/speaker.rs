//! The built-in Speaker as a [`Component`], and the surface it speaks
//! through when no chat platform is mounted.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use ractor::ActorRef;
use senate_core::{ChatSurface, CommitteeId, PostTarget, Posted, SurfaceError};
use senate_speaker::{BasisBrain, ModelReport, SecretaryConfigs, Speaker, SpeakerArgs, SpeakerMsg};

use crate::component::{Component, ComponentContext};

/// The surface of a Senate with no chat platform: the platform `none`,
/// where every call is [`SurfaceError::Unsupported`].
///
/// The Speaker does everything it does through the log regardless of a
/// platform: it triages, answers for an absent agent from that agent's
/// checkpoint, hands work back, and answers agents that address it. What it
/// would have mirrored to a group has no group to go to, since no group can
/// be bound without a platform; a post that is refused as unsupported is
/// skipped, as it is on any platform that cannot do what was asked.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoChat;

/// The platform name [`NoChat`] reports.
pub const NO_CHAT_PLATFORM: &str = "none";

#[async_trait]
impl ChatSurface for NoChat {
    fn platform(&self) -> &str {
        NO_CHAT_PLATFORM
    }

    async fn post(
        &self,
        _target: PostTarget,
        _markdown: &str,
        _operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        Err(SurfaceError::Unsupported(
            "this Senate has no chat platform".into(),
        ))
    }
}

/// The running Speaker's mailbox, as long as one is running: replaced on
/// every restart.
#[derive(Clone, Default)]
pub(crate) struct SpeakerSlot(Arc<Mutex<Option<ActorRef<SpeakerMsg>>>>);

impl SpeakerSlot {
    fn set(&self, actor: Option<ActorRef<SpeakerMsg>>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = actor;
    }

    pub(crate) fn get(&self) -> Option<ActorRef<SpeakerMsg>> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The Speaker under the daemon's supervision: spawned with the same
/// arguments on every start (it rebuilds everything from the log and its
/// cursor), stopped on shutdown, and a failure when it stops on its own.
pub(crate) struct SpeakerComponent {
    pub(crate) args: SpeakerArgs,
    pub(crate) slot: SpeakerSlot,
}

#[async_trait]
impl Component for SpeakerComponent {
    fn name(&self) -> &str {
        "speaker"
    }

    async fn run(&self, ctx: ComponentContext) -> anyhow::Result<()> {
        let (actor, mut join) = Speaker::spawn(self.args.clone()).await?;
        self.slot.set(Some(actor.clone()));
        let outcome = tokio::select! {
            () = ctx.shutdown.cancelled() => {
                actor.stop(Some("shutdown".into()));
                let _ = (&mut join).await;
                Ok(())
            }
            ended = &mut join => {
                Err(anyhow::anyhow!("the Speaker stopped on its own: {ended:?}"))
            }
        };
        self.slot.set(None);
        outcome
    }
}

/// What `senated secretary show` prints: the built-in session's
/// configuration in force for `committee` (`None`: the Senate as a whole),
/// re-read from its files now, where each value came from, and the current
/// problems, in the deployment's language. A model's key is never in it:
/// only the name of its variable and whether it is set.
pub fn secretary_show(secretary: &SecretaryConfigs, committee: Option<&CommitteeId>) -> String {
    secretary.explain(committee).render(secretary.locale())
}

/// What `senated check-model` (and `senated doctor`) reads: the self-check
/// of every model the built-in session runs on for `committee` (`None`:
/// the Senate as a whole), on the Brain's own code paths
/// ([`BasisBrain::check_model`]). It reads no log; it never fails, what goes
/// wrong is in the report.
pub async fn check_model(
    secretary: Arc<SecretaryConfigs>,
    committee: Option<&CommitteeId>,
) -> ModelReport {
    BasisBrain::for_probe(secretary, crate::empty_hub::empty())
        .check_model(committee)
        .await
}

/// Log the self-check of a running built-in session's model, once: loudly
/// when a check fails. The daemon keeps running either way; the fixed
/// syntax works without a model.
pub(crate) async fn log_model_check(brain: BasisBrain) {
    let report = brain.check_model(None).await;
    if let Some(problem) = &report.problem {
        // Said where the configuration is read; nothing to probe.
        tracing::warn!(%problem, "the Speaker's model was not self-checked");
    } else if report.passed() {
        tracing::info!(
            models = report.tiers.len(),
            "the Speaker's model passed its self-check"
        );
    }
    for warning in report.warnings() {
        tracing::warn!(%warning, "the Speaker's model is slower than its triage deadline allows");
    }
    if report.problem.is_none() && !report.passed() {
        for failure in report.failures() {
            tracing::error!(%failure, "the Speaker's model failed its self-check");
        }
        tracing::error!(
            "the Speaker's model failed its self-check; people get the fixed syntax until it \
             works. Run `senated check-model` for the report:\n{}",
            report.render()
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use senate_core::Locale;
    use senate_speaker::{BaseLayer, ModelSettings, SecretaryConfigs};

    /// No model: nothing is probed, and the report says why.
    #[tokio::test]
    async fn without_a_model_the_check_fails_with_the_reason() {
        let dir = tempfile::tempdir().expect("dir");
        let secretary = Arc::new(SecretaryConfigs::new(
            None,
            BaseLayer::new(dir.path(), Locale::En),
        ));
        let report = super::check_model(secretary, None).await;
        assert!(!report.passed());
        assert!(report.tiers.is_empty());
        assert!(
            report.render().contains("no usable model"),
            "{}",
            report.render()
        );
    }

    /// An endpoint nobody listens on: every check fails with basis's own
    /// reason, on the primary and on the fallback, each reported apart.
    #[tokio::test]
    async fn an_unreachable_endpoint_fails_every_check_with_its_reason() {
        let dir = tempfile::tempdir().expect("dir");
        let unreachable = |model: &str| ModelSettings {
            base_url: Some("http://127.0.0.1:9/v1".into()),
            model: Some(model.into()),
            deadline_secs: Some(5),
            triage_deadline_secs: Some(5),
            ..ModelSettings::default()
        };
        let model = ModelSettings {
            fallback: Some(Box::new(unreachable("backup"))),
            ..unreachable("primary")
        };
        let secretary = Arc::new(SecretaryConfigs::new(
            None,
            BaseLayer::new(dir.path(), Locale::En).with_model(model),
        ));
        let report = super::check_model(secretary, None).await;
        assert!(!report.passed());
        let tiers: Vec<&str> = report.tiers.iter().map(|t| t.tier.as_str()).collect();
        assert_eq!(tiers, ["primary", "fallback"]);
        for tier in &report.tiers {
            assert_eq!(tier.checks.len(), 5);
            for check in &tier.checks {
                assert!(!check.passed, "{}", report.render());
                assert!(!check.reason.is_empty());
            }
        }
        assert_eq!(report.failures().len(), 10);
    }
}
