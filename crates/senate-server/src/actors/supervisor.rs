//! `SenateSupervisor`: the root of the tree . It spawns one
//! [`CommitteeActor`] per committee on first use and restarts a writer that
//! fails, which rebuilds itself from the log.
//!
//! The supervisor is the only code that creates writers, and it creates one
//! only when the committee has no live writer, so there is never more than one
//! writer per committee. The [`Registry`] is its published map from committee to
//! writer; the hub reads it to skip the supervisor's mailbox on the hot path.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};

use ractor::{
    Actor, ActorId, ActorProcessingErr, ActorRef, ActorStatus, RpcReplyPort, SupervisionEvent,
};
use senate_core::{CommitteeId, Event};
use senate_log::LogStore;
use tokio::sync::broadcast;

use super::committee::{CommitteeActor, CommitteeArgs, CommitteeMsg};
use super::lease::LeaseTiming;
use super::presence::Presence;
use crate::seats::Seats;

/// Committee → its current writer. Written only by the supervisor.
#[derive(Default)]
pub(crate) struct Registry {
    writers: RwLock<HashMap<CommitteeId, ActorRef<CommitteeMsg>>>,
}

pub(crate) fn is_alive(actor: &ActorRef<CommitteeMsg>) -> bool {
    actor.get_status() < ActorStatus::Stopping
}

impl Registry {
    /// The committee's writer, if one is registered and still running.
    pub(crate) fn get(&self, committee: &CommitteeId) -> Option<ActorRef<CommitteeMsg>> {
        let writers = self.writers.read().unwrap_or_else(PoisonError::into_inner);
        writers.get(committee).filter(|a| is_alive(a)).cloned()
    }

    fn current_id(&self, committee: &CommitteeId) -> Option<ActorId> {
        let writers = self.writers.read().unwrap_or_else(PoisonError::into_inner);
        writers.get(committee).map(|a| a.get_id())
    }

    fn insert(&self, committee: CommitteeId, actor: ActorRef<CommitteeMsg>) {
        let mut writers = self.writers.write().unwrap_or_else(PoisonError::into_inner);
        writers.insert(committee, actor);
    }

    fn remove_if(&self, committee: &CommitteeId, id: ActorId) {
        let mut writers = self.writers.write().unwrap_or_else(PoisonError::into_inner);
        if writers.get(committee).is_some_and(|a| a.get_id() == id) {
            writers.remove(committee);
        }
    }
}

pub(crate) enum SupervisorMsg {
    /// The committee's writer, spawned if it has none.
    Writer(
        CommitteeId,
        RpcReplyPort<Result<ActorRef<CommitteeMsg>, String>>,
    ),
}

#[derive(Clone)]
pub(crate) struct SupervisorArgs {
    pub store: Arc<dyn LogStore>,
    pub events: broadcast::Sender<Event>,
    pub presence: Arc<Presence>,
    pub registry: Arc<Registry>,
    pub leases: LeaseTiming,
    /// The Senate's seats, shared with the hub.
    pub seats: Arc<Seats>,
}

pub(crate) struct SupervisorState {
    args: SupervisorArgs,
    children: HashMap<ActorId, CommitteeId>,
}

pub(crate) struct SenateSupervisor;

impl SupervisorState {
    async fn spawn_writer(
        &mut self,
        myself: &ActorRef<SupervisorMsg>,
        committee: &CommitteeId,
    ) -> Result<ActorRef<CommitteeMsg>, String> {
        let args = CommitteeArgs {
            committee: committee.clone(),
            store: self.args.store.clone(),
            events: self.args.events.clone(),
            presence: self.args.presence.clone(),
            leases: self.args.leases,
            seats: self.args.seats.clone(),
        };
        let (writer, _) = Actor::spawn_linked(None, CommitteeActor, args, myself.get_cell())
            .await
            .map_err(|e| format!("starting the writer for {committee}: {e}"))?;
        self.children.insert(writer.get_id(), committee.clone());
        self.args.registry.insert(committee.clone(), writer.clone());
        Ok(writer)
    }
}

impl Actor for SenateSupervisor {
    type Msg = SupervisorMsg;
    type State = SupervisorState;
    type Arguments = SupervisorArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: SupervisorArgs,
    ) -> Result<SupervisorState, ActorProcessingErr> {
        Ok(SupervisorState {
            args,
            children: HashMap::new(),
        })
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: SupervisorMsg,
        state: &mut SupervisorState,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            SupervisorMsg::Writer(committee, reply) => {
                let result = match state.args.registry.get(&committee) {
                    Some(writer) => Ok(writer),
                    None => state.spawn_writer(&myself, &committee).await,
                };
                if reply.send(result).is_err() {
                    tracing::debug!(%committee, "writer requested by a caller that went away");
                }
            }
        }
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        myself: ActorRef<Self::Msg>,
        event: SupervisionEvent,
        state: &mut SupervisorState,
    ) -> Result<(), ActorProcessingErr> {
        match event {
            SupervisionEvent::ActorFailed(cell, error) => {
                let id = cell.get_id();
                let Some(committee) = state.children.remove(&id) else {
                    return Ok(());
                };
                // Restart only if this was still the registered writer; a
                // newer one may already have replaced it.
                if state.args.registry.current_id(&committee) != Some(id) {
                    return Ok(());
                }
                tracing::error!(%committee, %error, "committee writer failed; restarting from the log");
                state.args.registry.remove_if(&committee, id);
                if let Err(error) = state.spawn_writer(&myself, &committee).await {
                    // Left unregistered: the next request spawns it again.
                    tracing::error!(%committee, %error, "restarting the committee writer failed");
                }
            }
            SupervisionEvent::ActorTerminated(cell, _, reason) => {
                let id = cell.get_id();
                if let Some(committee) = state.children.remove(&id) {
                    tracing::debug!(%committee, ?reason, "committee writer stopped");
                    state.args.registry.remove_if(&committee, id);
                }
            }
            _ => {}
        }
        Ok(())
    }
}
