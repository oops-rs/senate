//! `AgentProxy`: one per `(committee, agent)` with a live stream. It owns the
//! sending half of each of that agent's SSE connections, forwards every
//! committed event to them, and reports connected/disconnected through
//! [`Presence`].
//!
//! A slow client never blocks the committee's writer: a full stream buffer
//! drops that connection, the client reconnects with `Last-Event-ID`, and the
//! replay serves what it missed.

use std::sync::Arc;

use ractor::{Actor, ActorProcessingErr, ActorRef};
use senate_core::{CommitteeId, Event, SenatorPath};
use tokio::sync::mpsc::{self, error::TrySendError};

use super::presence::{ConnId, Presence};

pub(crate) enum ProxyMsg {
    Attach {
        conn: ConnId,
        sender: mpsc::Sender<Arc<Event>>,
    },
    Detach(ConnId),
    Deliver(Arc<Event>),
}

pub(crate) struct ProxyArgs {
    pub committee: CommitteeId,
    pub agent: SenatorPath,
    pub presence: Arc<Presence>,
}

pub(crate) struct ProxyState {
    args: ProxyArgs,
    senders: Vec<(ConnId, mpsc::Sender<Arc<Event>>)>,
}

pub(crate) struct AgentProxy;

impl ProxyState {
    fn drop_conn(&mut self, conn: ConnId) {
        let before = self.senders.len();
        self.senders.retain(|(c, _)| *c != conn);
        if self.senders.len() != before {
            self.after_drop(conn);
        }
    }

    fn after_drop(&mut self, conn: ConnId) {
        let ProxyArgs {
            committee,
            agent,
            presence,
        } = &self.args;
        presence.remove(committee, agent, conn);
        if self.senders.is_empty() {
            tracing::info!(%committee, %agent, "agent disconnected");
        }
    }
}

impl Actor for AgentProxy {
    type Msg = ProxyMsg;
    type State = ProxyState;
    type Arguments = ProxyArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: ProxyArgs,
    ) -> Result<ProxyState, ActorProcessingErr> {
        Ok(ProxyState {
            args,
            senders: Vec::new(),
        })
    }

    async fn handle(
        &self,
        _myself: ActorRef<Self::Msg>,
        message: ProxyMsg,
        state: &mut ProxyState,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            ProxyMsg::Attach { conn, sender } => {
                let ProxyArgs {
                    committee,
                    agent,
                    presence,
                } = &state.args;
                if state.senders.is_empty() {
                    tracing::info!(%committee, %agent, "agent connected");
                }
                presence.add(committee, agent, conn, sender.downgrade());
                state.senders.push((conn, sender));
            }
            ProxyMsg::Detach(conn) => state.drop_conn(conn),
            ProxyMsg::Deliver(event) => {
                let mut dropped = Vec::new();
                for (conn, sender) in &state.senders {
                    match sender.try_send(event.clone()) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => {
                            tracing::warn!(
                                committee = %state.args.committee,
                                agent = %state.args.agent,
                                conn,
                                "stream buffer full; dropping the connection so it replays"
                            );
                            dropped.push(*conn);
                        }
                        Err(TrySendError::Closed(_)) => dropped.push(*conn),
                    }
                }
                for conn in dropped {
                    state.drop_conn(conn);
                }
            }
        }
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut ProxyState,
    ) -> Result<(), ActorProcessingErr> {
        let senders = std::mem::take(&mut state.senders);
        let ProxyArgs {
            committee,
            agent,
            presence,
        } = &state.args;
        for (conn, _) in &senders {
            presence.remove(committee, agent, *conn);
        }
        if !senders.is_empty() {
            tracing::info!(%committee, %agent, "agent disconnected (proxy stopped)");
        }
        Ok(())
    }
}
