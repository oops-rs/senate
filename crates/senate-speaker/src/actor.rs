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

/// What the Speaker needs. Cloneable so a supervisor can respawn it: the
/// unbound-mention receiver is shared, and a respawned Speaker picks it up
/// where the last one left it.
///
/// There is no roster here: who sits in the Senate is the hub's
/// ([`Hub::roster`]), which knows its presider and seats agents as they
/// connect, so the Speaker asks it rather than hold a copy that goes stale.
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
        Self {
            hub,
            surface,
            brain,
            config,
            runner: None,
            forge: None,
            unbound: Arc::new(Mutex::new(unbound)),
        }
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
