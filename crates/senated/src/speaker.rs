//! The built-in Speaker as a [`Component`], and the surface it speaks
//! through when no chat platform is mounted.

use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use ractor::ActorRef;
use senate_core::{ChatSurface, CommitteeId, PostTarget, Posted, SurfaceError};
use senate_speaker::{SecretaryConfigs, Speaker, SpeakerArgs, SpeakerMsg};

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
