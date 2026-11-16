//! `CommitteeActor`: the single writer of one committee's log.
//!
//! Every append for the committee goes through this actor's mailbox, so appends
//! commit one at a time, in mailbox order. After a commit the actor fans the
//! event out — to each [`AgentProxy`], to every watcher (a stream opened by
//! a person who is only reading, which has no presence), and to the
//! process-wide broadcast behind `Hub::subscribe` — before it handles the next message. That ordering is
//! what makes the SSE replay gap-free: once an `Attach` has been handled,
//! every later commit reaches the new stream, and every earlier one is already
//! in the store.
//!
//! An append whose commit succeeded but whose result was lost (the store
//! reported an error after the commit) is not fanned out then. Its retry
//! comes back `duplicate`, and the actor fans it out at that point if its seq
//! is beyond everything fanned out so far, so live consumers see it without
//! waiting for the next gap. `fanned` is that high-water mark, so nothing is
//! fanned out twice.
//!
//! The actor keeps no state the store cannot rebuild (the leases included): `fanned` is reloaded
//! from the store's head in `pre_start` (a restarted writer cannot tell what
//! its predecessor fanned out, and its clients replay from the store anyway),
//! and the proxies (with their live connections) die with it, so their clients
//! reconnect and replay.
//!
//! # Presence
//!
//! The writer also owns its committee's leases ([`super::lease`]): every
//! [`LeaseTiming::tick`], after a heartbeat and after a stream attaches, it
//! asks [`Leases`] which presence transitions are due and commits each as a
//! `presence` event through the same path as an append. Its leases are
//! rebuilt from the log's `presence` events in `pre_start`.
//!
//! [`AgentProxy`]: super::proxy::AgentProxy

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use ractor::{Actor, ActorProcessingErr, ActorRef, MessagingErr, RpcReplyPort, SupervisionEvent};
use senate_core::hub::SCAN_PAGE;
use senate_core::{
    AgentPresence, Appended, CommitteeId, Event, Kind, Name, NewEvent, PresenceBoard,
    PresencePayload, SenatorPath, Seq, presence_event, reassign_event,
};
use senate_log::{LogError, LogStore};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use super::lease::{LeaseTiming, Leases};
use super::presence::{ConnId, Presence};
use super::proxy::{AgentProxy, ProxyArgs, ProxyMsg};
use crate::seats::Seats;

pub(crate) enum CommitteeMsg {
    Append(Box<NewEvent>, RpcReplyPort<Result<Appended, LogError>>),
    /// Register a live stream for `agent`. Replies with the proxy that owns it
    /// once every later commit is guaranteed to reach `sender`.
    Attach {
        agent: SenatorPath,
        conn: ConnId,
        sender: mpsc::Sender<Arc<Event>>,
        reply: RpcReplyPort<Result<ActorRef<ProxyMsg>, String>>,
    },
    /// Register a live stream with no agent behind it: nothing in presence
    /// follows from it. Replies once every later commit reaches `sender`.
    Watch {
        conn: ConnId,
        sender: mpsc::Sender<Arc<Event>>,
        reply: RpcReplyPort<()>,
    },
    /// Re-evaluate every lease.
    PresenceTick,
    /// `agent` just heartbeated: re-evaluate its lease now.
    PresenceCheck(SenatorPath),
    /// Each agent's presence, lag measured against the head.
    Presence(RpcReplyPort<(Seq, Vec<AgentPresence>)>),
    /// The Speaker picked up for an away agent.
    TakeOver {
        agent: SenatorPath,
        reply: RpcReplyPort<Result<Option<Event>, String>>,
    },
    /// A person, through the Speaker, hands `agent`'s work to `new_owner`
    /// (or back to the agent itself).
    Reassign {
        agent: SenatorPath,
        new_owner: SenatorPath,
        by: Name,
        reply: RpcReplyPort<Result<Option<Event>, String>>,
    },
}

pub(crate) struct CommitteeArgs {
    pub committee: CommitteeId,
    pub store: Arc<dyn LogStore>,
    pub events: broadcast::Sender<Event>,
    pub presence: Arc<Presence>,
    pub leases: LeaseTiming,
    /// The Senate's seats: agents found in the log at start are seated again.
    pub seats: Arc<Seats>,
}

pub(crate) struct CommitteeState {
    args: CommitteeArgs,
    /// Highest seq fanned out, or the store's head when this writer started.
    fanned: Seq,
    proxies: HashMap<SenatorPath, ActorRef<ProxyMsg>>,
    /// Streams of people who are only reading.
    watchers: Vec<(ConnId, mpsc::Sender<Arc<Event>>)>,
    leases: Leases,
    ticker: Option<JoinHandle<()>>,
}

pub(crate) struct CommitteeActor;

impl CommitteeState {
    async fn proxy(
        &mut self,
        myself: &ActorRef<CommitteeMsg>,
        agent: &SenatorPath,
    ) -> Result<ActorRef<ProxyMsg>, ActorProcessingErr> {
        if let Some(proxy) = self.proxies.get(agent) {
            return Ok(proxy.clone());
        }
        let (proxy, _) = Actor::spawn_linked(
            None,
            AgentProxy,
            ProxyArgs {
                committee: self.args.committee.clone(),
                agent: agent.clone(),
                presence: self.args.presence.clone(),
            },
            myself.get_cell(),
        )
        .await?;
        self.proxies.insert(agent.clone(), proxy.clone());
        Ok(proxy)
    }

    async fn attach(
        &mut self,
        myself: &ActorRef<CommitteeMsg>,
        agent: SenatorPath,
        conn: ConnId,
        sender: mpsc::Sender<Arc<Event>>,
    ) -> Result<ActorRef<ProxyMsg>, ActorProcessingErr> {
        let proxy = self.proxy(myself, &agent).await?;
        match proxy.cast(ProxyMsg::Attach { conn, sender }) {
            Ok(()) => Ok(proxy),
            // The proxy stopped and its exit notice is still queued behind
            // this message: replace it now.
            Err(MessagingErr::SendErr(message)) => {
                self.proxies.remove(&agent);
                let proxy = self.proxy(myself, &agent).await?;
                proxy
                    .cast(message)
                    .map_err(|_| "a freshly spawned agent proxy refused its first attach")?;
                Ok(proxy)
            }
            Err(_) => Err("agent proxy mailbox is closed".into()),
        }
    }

    fn fan_out(&mut self, event: &Event) {
        let shared = Arc::new(event.clone());
        self.proxies
            .retain(|_, proxy| proxy.cast(ProxyMsg::Deliver(shared.clone())).is_ok());
        // A watcher that is full or gone is dropped; a full one replays from
        // its `Last-Event-ID` like a slow agent stream.
        self.watchers
            .retain(|(_, sender)| sender.try_send(shared.clone()).is_ok());
        // `send` only fails when nobody subscribed, which is not an error.
        if self.args.events.send(event.clone()).is_err() {
            tracing::trace!(committee = %self.args.committee, "no hub subscribers");
        }
    }

    /// Commit one event and fan it out (see the module docs on `fanned`).
    async fn commit(&mut self, event: NewEvent) -> Result<Appended, LogError> {
        if event.committee != self.args.committee {
            return Err(LogError::Invalid(format!(
                "event for {} sent to the writer of {}",
                event.committee, self.args.committee
            )));
        }
        let result = self.args.store.append(event).await;
        if let Ok(appended) = &result {
            let seq = appended.event.seq;
            if !appended.duplicate && seq != self.fanned.next() {
                tracing::warn!(
                    committee = %self.args.committee,
                    fanned = %self.fanned,
                    %seq,
                    "seq skipped ahead of the last fanned-out event \
                     (a commit whose result was lost, or another server writing this log)"
                );
            }
            // Every new event, and a duplicate beyond the high-water
            // mark: the retry of a commit whose result was lost.
            if !appended.duplicate || seq > self.fanned {
                if appended.duplicate {
                    tracing::info!(
                        committee = %self.args.committee,
                        %seq,
                        "fanning out a retried append whose commit result was lost"
                    );
                }
                self.fanned = self.fanned.max(seq);
                self.fan_out(&appended.event);
            }
        }
        result
    }

    /// Commit a presence transition and record it in the leases.
    async fn commit_presence(&mut self, payload: PresencePayload) -> Result<Event, String> {
        let event = presence_event(self.args.committee.clone(), &payload);
        self.commit_presence_event(payload, event).await
    }

    async fn commit_presence_event(
        &mut self,
        payload: PresencePayload,
        event: NewEvent,
    ) -> Result<Event, String> {
        match self.commit(event).await {
            Ok(appended) => {
                tracing::info!(
                    committee = %self.args.committee,
                    agent = %payload.agent,
                    state = %payload.state,
                    seq = %appended.event.seq,
                    "presence"
                );
                self.leases.apply(&payload, appended.event.seq);
                Ok(appended.event)
            }
            Err(e) => {
                tracing::warn!(
                    committee = %self.args.committee,
                    agent = %payload.agent,
                    state = %payload.state,
                    "recording a presence transition failed (retried next tick): {e}"
                );
                Err(e.to_string())
            }
        }
    }

    /// Commit whatever transition of `agent`'s lease is due.
    async fn check_lease(&mut self, agent: &SenatorPath) {
        let (activity, cursor) = self
            .args
            .presence
            .last_activity(&self.args.committee, agent);
        let due = self.leases.decide(
            agent,
            activity,
            cursor,
            Instant::now(),
            chrono::Utc::now(),
            self.fanned,
        );
        if let Some(payload) = due {
            // Logged inside; the next tick retries a failure.
            let _ = self.commit_presence(payload).await;
        }
    }

    async fn check_leases(&mut self) {
        let mut agents: Vec<SenatorPath> = self.leases.agents().cloned().collect();
        agents.extend(self.args.presence.agents_on(&self.args.committee));
        agents.sort();
        agents.dedup();
        for agent in agents {
            self.check_lease(&agent).await;
        }
    }
}

/// The latest `presence` per agent in the committee's log.
async fn presence_board(
    store: &dyn LogStore,
    committee: &CommitteeId,
) -> Result<PresenceBoard, LogError> {
    let mut board = PresenceBoard::new();
    let mut after = Seq::ZERO;
    loop {
        let page = store.read(committee, after, SCAN_PAGE).await?;
        let Some(last) = page.last() else { break };
        after = last.seq;
        for event in page.iter().filter(|e| e.kind == Kind::Presence) {
            board.observe(event);
        }
    }
    Ok(board)
}

impl Actor for CommitteeActor {
    type Msg = CommitteeMsg;
    type State = CommitteeState;
    type Arguments = CommitteeArgs;

    async fn pre_start(
        &self,
        _myself: ActorRef<Self::Msg>,
        args: CommitteeArgs,
    ) -> Result<CommitteeState, ActorProcessingErr> {
        let head = args.store.head(&args.committee).await?;
        let mut leases = Leases::new(args.leases);
        let board = presence_board(args.store.as_ref(), &args.committee).await?;
        let (now, wall) = (Instant::now(), chrono::Utc::now());
        leases.restore(&board, now, wall);
        // An agent online but never written is restored from its last stored
        // heartbeat.
        for beat in args.store.heartbeats(&args.committee).await? {
            if board.get(&beat.agent).is_some() {
                continue;
            }
            let quiet = (wall - beat.at).to_std().unwrap_or_default();
            if quiet < args.leases.lease {
                leases.restore_unlogged(&beat.agent, beat.heartbeat.cursor, beat.at, now, wall);
            }
        }
        // Seats are not stored, so the agents the log has seen sit again.
        let seen = board
            .agents()
            .into_iter()
            .map(|p| p.agent)
            .chain(leases.agents().cloned().collect::<Vec<_>>());
        for agent in seen {
            if let Err(e) = args.seats.seat_again(agent.clone()) {
                tracing::debug!(%agent, "not seated again: {e}");
            }
        }
        tracing::debug!(committee = %args.committee, %head, "committee writer started");
        Ok(CommitteeState {
            args,
            fanned: head,
            proxies: HashMap::new(),
            watchers: Vec::new(),
            leases,
            ticker: None,
        })
    }

    async fn post_start(
        &self,
        myself: ActorRef<Self::Msg>,
        state: &mut CommitteeState,
    ) -> Result<(), ActorProcessingErr> {
        let tick = state.leases.timing().tick();
        state.ticker = Some(myself.send_interval(tick, || CommitteeMsg::PresenceTick));
        Ok(())
    }

    async fn post_stop(
        &self,
        _myself: ActorRef<Self::Msg>,
        state: &mut CommitteeState,
    ) -> Result<(), ActorProcessingErr> {
        if let Some(ticker) = state.ticker.take() {
            ticker.abort();
        }
        Ok(())
    }

    async fn handle(
        &self,
        myself: ActorRef<Self::Msg>,
        message: CommitteeMsg,
        state: &mut CommitteeState,
    ) -> Result<(), ActorProcessingErr> {
        match message {
            CommitteeMsg::Append(event, reply) => {
                let result = state.commit(*event).await;
                reply_or_log(reply, result);
            }
            CommitteeMsg::Attach {
                agent,
                conn,
                sender,
                reply,
            } => {
                let result = state
                    .attach(&myself, agent.clone(), conn, sender)
                    .await
                    .map_err(|e| e.to_string());
                let attached = result.is_ok();
                reply_or_log(reply, result);
                if attached {
                    state.check_lease(&agent).await;
                }
            }
            CommitteeMsg::Watch {
                conn,
                sender,
                reply,
            } => {
                state.watchers.push((conn, sender));
                reply_or_log(reply, ());
            }
            CommitteeMsg::PresenceTick => state.check_leases().await,
            CommitteeMsg::PresenceCheck(agent) => state.check_lease(&agent).await,
            CommitteeMsg::Presence(reply) => {
                reply_or_log(reply, (state.fanned, state.leases.view(state.fanned)));
            }
            CommitteeMsg::TakeOver { agent, reply } => {
                let result = match state
                    .leases
                    .take_over(&agent, chrono::Utc::now(), state.fanned)
                {
                    Ok(Some(payload)) => state.commit_presence(payload).await.map(Some),
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                };
                reply_or_log(reply, result);
            }
            CommitteeMsg::Reassign {
                agent,
                new_owner,
                by,
                reply,
            } => {
                let result = match state.leases.reassign(
                    &agent,
                    &new_owner,
                    chrono::Utc::now(),
                    state.fanned,
                ) {
                    Ok(Some(payload)) => {
                        let event = reassign_event(state.args.committee.clone(), &payload, &by);
                        state.commit_presence_event(payload, event).await.map(Some)
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                };
                reply_or_log(reply, result);
            }
        }
        Ok(())
    }

    async fn handle_supervisor_evt(
        &self,
        _myself: ActorRef<Self::Msg>,
        event: SupervisionEvent,
        state: &mut CommitteeState,
    ) -> Result<(), ActorProcessingErr> {
        // A proxy that stops takes its connections with it; its clients
        // reconnect and a fresh proxy is spawned on their next attach. The
        // writer itself carries on.
        match event {
            SupervisionEvent::ActorFailed(cell, error) => {
                tracing::error!(committee = %state.args.committee, %error, "agent proxy failed");
                let id = cell.get_id();
                state.proxies.retain(|_, p| p.get_id() != id);
            }
            SupervisionEvent::ActorTerminated(cell, _, _) => {
                let id = cell.get_id();
                state.proxies.retain(|_, p| p.get_id() != id);
            }
            _ => {}
        }
        Ok(())
    }
}

fn reply_or_log<T: Send + 'static>(reply: RpcReplyPort<T>, value: T) {
    if reply.send(value).is_err() {
        tracing::debug!("caller went away before the reply");
    }
}
