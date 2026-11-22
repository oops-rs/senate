//! The Speaker as a ractor actor.
//!
//! One mailbox serializes everything — live log events, lag recovery,
//! unbound mentions, fallback, brief-top and card timers, finished Brain
//! calls —
//! so the engine sees one thing at a time per process and needs no locks.
//! Nothing slow runs on it: Brain calls are spawned and report back as
//! [`SpeakerMsg::Thought`] (see the `engine` module docs), and every post is
//! bounded in time. Two pump tasks feed the mailbox: the log subscription (a
//! lag becomes [`SpeakerMsg::Resync`]) and the gateway's unbound-mention
//! channel.
//!
//! Every task the actor spawns is held by its state, which ractor drops
//! however the actor ends, so a panicked Speaker's pumps and Brain calls die
//! with it instead of competing with its respawned successor.

use std::sync::Arc;

use ractor::{Actor, ActorProcessingErr, ActorRef, RpcReplyPort, SpawnErr};
use senate_core::{ChatSurface, CommitteeId, Event, Hub, SenatorName, Seq, UnboundMention};
use senate_work::{Forge, Runner};
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio::task::JoinHandle;
use tracing::warn;

use crate::brain::Brain;
use crate::config::SpeakerConfig;
use crate::engine::{Engine, RunEvent, ThoughtKind};
use crate::secretary::SecretaryConfigs;

/// What the Speaker needs. Cloneable so a supervisor can respawn it: the
/// unbound-mention receiver is shared, and a respawned Speaker picks it up
/// where the last one left it.
///
/// There is no roster here: who sits in the Senate is the hub's
/// ([`Hub::roster`]), which seats agents (under the person who runs them) as
/// they connect, so the Speaker asks it rather than hold a copy that goes stale.
#[derive(Clone)]
pub struct SpeakerArgs {
    pub hub: Arc<dyn Hub>,
    pub surface: Arc<dyn ChatSurface>,
    pub brain: Arc<dyn Brain>,
    pub config: SpeakerConfig,
    /// What does coding work on request. `None`: the Speaker says it cannot
    /// ([`SpeakerArgs::with_work`]).
    pub runner: Option<Arc<dyn Runner>>,
    /// What opens a run's PR. `None` with a runner: the branch is the
    /// deliverable ([`senate_work::NoForge`]).
    pub forge: Option<Arc<dyn Forge>>,
    /// The built-in session's configuration, re-read before it acts on a
    /// committee. [`SpeakerArgs::new`] makes a fixed one of `config`;
    /// [`SpeakerArgs::with_secretary`] gives the files'.
    pub secretary: Arc<SecretaryConfigs>,
    unbound: Arc<Mutex<mpsc::Receiver<UnboundMention>>>,
}

impl SpeakerArgs {
    pub fn new(
        hub: Arc<dyn Hub>,
        surface: Arc<dyn ChatSurface>,
        brain: Arc<dyn Brain>,
        config: SpeakerConfig,
        unbound: mpsc::Receiver<UnboundMention>,
    ) -> Self {
        let secretary = Arc::new(SecretaryConfigs::fixed(config.clone()));
        Self {
            hub,
            surface,
            brain,
            config,
            runner: None,
            forge: None,
            secretary,
            unbound: Arc::new(Mutex::new(unbound)),
        }
    }

    /// Run on `secretary`'s configuration (its files, re-read as they
    /// change) instead of the fixed `config`. The Brain should read the same
    /// one ([`BasisBrain::on_secretary`](crate::BasisBrain::on_secretary)).
    pub fn with_secretary(mut self, secretary: Arc<SecretaryConfigs>) -> Self {
        self.config = secretary.effective(None).speaker.clone();
        self.secretary = secretary;
        self
    }

    /// Give the Speaker hands: a runner for work runs, and the
    /// forge that opens their PRs (`None`: branch only). Work also needs
    /// `config.work`.
    pub fn with_work(mut self, runner: Arc<dyn Runner>, forge: Option<Arc<dyn Forge>>) -> Self {
        self.runner = Some(runner);
        self.forge = forge;
        self
    }
}

/// The Speaker actor. Spawn it with [`Speaker::spawn`] or, under a
/// supervisor, `Actor::spawn_linked(name, Speaker, args, supervisor)`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Speaker;

impl Speaker {
    pub async fn spawn(
        args: SpeakerArgs,
    ) -> Result<(ActorRef<SpeakerMsg>, ractor::concurrency::JoinHandle<()>), SpawnErr> {
        Actor::spawn(None, Speaker, args).await
    }
}

pub enum SpeakerMsg {
    /// One event off the log subscription.
    Event(Box<Event>),
    /// Catch every committee up from its cursor (start, and after a lag).
    Resync,
    /// Retry a committee that stalled on a transient failure.
    Retry(CommitteeId),
    /// The Speaker was mentioned in a group bound to no committee.
    Unbound(Box<UnboundMention>),
    /// A directed question's answer window closed.
    FallbackDue {
        committee: CommitteeId,
        root: Seq,
        agent: SenatorName,
    },
    /// A committee's brief top is due for a rewrite (debounced after a
    /// decision).
    TopDue(CommitteeId),
    /// A task card is due for its in-place update (debounced).
    CardDue { committee: CommitteeId, root: Seq },
    /// The Agent group's pinned cards are due for a refresh (debounced, and
    /// at midnight).
    PinnedDue(CommitteeId),
    /// A people group's announcement is due for a refresh (debounced, and
    /// at most hourly).
    AnnouncementDue {
        committee: CommitteeId,
        group: String,
    },
    /// A committee's digest of its discussion is due: its daily time came,
    /// or a busy stretch went quiet.
    DigestDue {
        committee: CommitteeId,
        trigger: senate_core::DigestTrigger,
    },
    /// A burst of discussion in `group` (of simulated messages, or of
    /// real ones), last at `seq`, has gone quiet: check it for an agreement
    /// to propose recording.
    AgreementDue {
        committee: CommitteeId,
        group: String,
        simulated: bool,
        seq: senate_core::Seq,
    },
    /// A Brain call the Speaker spawned has finished. Only the Speaker
    /// makes these.
    Thought(Thought),
    /// A work run the Speaker started reports progress or its end, or its
    /// debounced card update is due. Only the Speaker makes these.
    Run(RunReport),
    /// Catch every committee up and wait until no Brain call is in flight,
    /// then reply. For tests and orderly shutdown.
    Settle(RpcReplyPort<()>),
}

/// The result of one Brain call, on its way back to the mailbox. Opaque:
/// only the Speaker makes one.
pub struct Thought(pub(crate) ThoughtKind);

/// News of one work run, on its way back to the mailbox. Opaque: only the
/// Speaker makes one.
pub struct RunReport(pub(crate) RunEvent);

pub struct SpeakerState {
    engine: Engine,
    /// Settle calls waiting for the engine to go idle.
    settles: Vec<RpcReplyPort<()>>,
    /// The two pumps; the engine holds its own Brain tasks the same way.
    _pumps: AbortOnDrop,
}

impl Actor for Speaker {
    type Msg = SpeakerMsg;
    type State = SpeakerState;
    type Arguments = SpeakerArgs;

    async fn pre_start(
        &self,
        myself: ActorRef<Self::Msg>,
        args: Self::Arguments,
    ) -> Result<Self::State, ActorProcessingErr> {
        // Subscribe before the first catch-up so nothing falls between them.
        let log = args.hub.subscribe();
        // Fetched once; if the log cannot say yet, the first event that
        // needs it fetches it (and stalls until it can).
        let epoch = match args.hub.epoch().await {
            Ok(epoch) => Some(epoch),
            Err(error) => {
                warn!(%error, "cannot read the log's epoch yet; will retry before posting");
                None
            }
        };
        let roster = match args.hub.roster().await {
            Ok(roster) => roster,
            Err(error) => {
                warn!(%error, "cannot read the roster yet; will read it again before acting");
                senate_core::Roster::default()
            }
        };
        let unbound = args.unbound.clone();
        let engine = Engine::new(args, epoch, roster, myself.clone());
        let mut pumps = AbortOnDrop::default();
        pumps.push(tokio::spawn(pump_log(log, myself.clone())));
        pumps.push(tokio::spawn(pump_unbound(unbound, myself.clone())));
        myself
            .send_message(SpeakerMsg::Resync)
            .map_err(|_| "the speaker's mailbox closed during start")?;
        Ok(SpeakerState {
            engine,
            settles: Vec::new(),
            _pumps: pumps,
        })
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        state.engine.shutdown();
        Ok(())
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: Self::Msg,
        state: &mut Self::State,
    ) -> Result<(), ActorProcessingErr> {
        let engine = &mut state.engine;
        let about = engine.prepare(&message);
        if let Some(committee) = &about {
            engine.tell_speaker_of_config_problems(committee).await;
        }
        match message {
            SpeakerMsg::Event(event) => engine.on_event(*event).await,
            SpeakerMsg::Resync => engine.resync_all().await,
            SpeakerMsg::Retry(committee) => engine.retry(&committee).await,
            SpeakerMsg::Unbound(mention) => engine.on_unbound(*mention).await,
            SpeakerMsg::FallbackDue {
                committee,
                root,
                agent,
            } => engine.on_fallback_due(committee, root, agent).await,
            SpeakerMsg::TopDue(committee) => engine.on_top_due(committee).await,
            SpeakerMsg::CardDue { committee, root } => engine.on_card_due(committee, root).await,
            SpeakerMsg::PinnedDue(committee) => engine.on_pinned_due(committee).await,
            SpeakerMsg::AnnouncementDue { committee, group } => {
                engine.on_announcement_due(committee, group).await
            }
            SpeakerMsg::DigestDue { committee, trigger } => {
                engine.on_digest_due(committee, trigger).await;
            }
            SpeakerMsg::AgreementDue {
                committee,
                group,
                simulated,
                seq,
            } => {
                engine
                    .on_agreement_due(committee, group, simulated, seq)
                    .await
            }
            SpeakerMsg::Thought(thought) => engine.on_thought(thought.0).await,
            SpeakerMsg::Run(report) => engine.on_run(report.0).await,
            SpeakerMsg::Settle(reply) => state.settles.push(reply),
        }
        engine.reap();
        if !state.settles.is_empty() && engine.idle() {
            // What a finished Brain call appended is committed but may not
            // have come off the subscription yet: read it from the log.
            engine.resync_all().await;
            if engine.idle() {
                for reply in state.settles.drain(..) {
                    // The caller may have stopped waiting; nothing to undo.
                    let _ = reply.send(());
                }
            }
        }
        Ok(())
    }
}

async fn pump_log(mut log: broadcast::Receiver<Event>, speaker: ActorRef<SpeakerMsg>) {
    loop {
        let message = match log.recv().await {
            Ok(event) => SpeakerMsg::Event(Box::new(event)),
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!(skipped, "log subscription lagged; catching up from cursors");
                SpeakerMsg::Resync
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        if speaker.send_message(message).is_err() {
            return;
        }
    }
}

async fn pump_unbound(
    unbound: Arc<Mutex<mpsc::Receiver<UnboundMention>>>,
    speaker: ActorRef<SpeakerMsg>,
) {
    let mut unbound = unbound.lock().await;
    while let Some(mention) = unbound.recv().await {
        if speaker
            .send_message(SpeakerMsg::Unbound(Box::new(mention)))
            .is_err()
        {
            return;
        }
    }
}

/// Aborts every task it holds when dropped. Kept in actor state, which
/// ractor drops even after a panic (when it skips `post_stop`).
#[derive(Debug, Default)]
struct AbortOnDrop(Vec<JoinHandle<()>>);

impl AbortOnDrop {
    fn push(&mut self, task: JoinHandle<()>) {
        self.0.push(task);
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}
