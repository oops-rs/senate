//! [`ServerHub`]: the server's [`Hub`], over the actor tree and the store.
//!
//! Appends go to the committee's single writer; reads, queries, cursors and
//! mirrors go straight to the store, which needs no serialization for them
//! (and answers [`Hub::thread`], [`Hub::bindings`] and [`Hub::search`] from
//! its indexes instead of the trait's scanning defaults). The HTTP layer uses
//! the same hub, plus the few agent-scoped operations below that are not on
//! the in-process [`Hub`] trait.
//!
//! [`Hub::brief`] keeps one [`BriefBuilder`] per committee and folds only the
//! events committed since its last call, so a brief costs a read of what is
//! new rather than a scan of the log. A builder remembers the epoch it was
//! built under and is dropped if the store's differs. [`Hub::tasks`] and
//! [`Hub::latest_checkpoint`] are cached the same way.
//!
//! Presence ([`Hub::presence`], [`Hub::mark_taken_over`], [`Hub::reassign`]) is answered by the
//! committee's writer, which owns the leases; the brief's presence section is
//! replaced by that live view, so its `lag` is measured now.
//!
//! The epoch is read from the store once, at start: a log is not reset under
//! a running server, and every response can then carry it without a store
//! round trip.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use ractor::rpc::CallResult;
use ractor::{ActorRef, MessagingErr};
use senate_core::api::{Heartbeat, PresencePage};
use senate_core::hub::SCAN_PAGE;
use senate_core::{
    Appended, Awaiting, Brief, BriefBuilder, CheckpointFacts, CheckpointRecord, Checkpoints,
    CommitteeId, DocketItem, DocketPage, Event, Hub, HubError, Kind, Locale, MirrorRef, Name,
    NewEvent, Roster, RosterError, SeatFacts, SenatorName, Senior, Seq, SpeakerDocket, SpeakerSeat,
    StatusState, TaskBoard,
};
use senate_log::{CursorOwner, LogError, LogStore};
use tokio::sync::{broadcast, mpsc};

use crate::actors::committee::CommitteeMsg;
use crate::actors::presence::{ConnId, Presence};
use crate::actors::proxy::ProxyMsg;
use crate::actors::supervisor::{Registry, SupervisorMsg};
use crate::rules::check_may_change_state;
use crate::seats::Seats;

/// Capacity of the process-wide broadcast behind [`Hub::subscribe`]. A
/// receiver that falls further behind gets `Lagged` and recovers with
/// [`Hub::read`].
pub const BROADCAST_CAPACITY: usize = 4096;

/// Events buffered per SSE connection before the connection is dropped (and
/// the client replays from its `Last-Event-ID`).
pub const STREAM_BUFFER: usize = 1024;

const CALL_TIMEOUT: Duration = Duration::from_secs(30);

fn log_error(error: LogError) -> HubError {
    match error {
        LogError::Invalid(message) => HubError::Rejected(message),
        other => HubError::Other(Box::new(other)),
    }
}

/// What the server assembles a [`ServerHub`] from.
pub(crate) struct HubParts {
    pub store: Arc<dyn LogStore>,
    pub epoch: String,
    pub supervisor: ActorRef<SupervisorMsg>,
    pub registry: Arc<Registry>,
    pub events: broadcast::Sender<Event>,
    pub presence: Arc<Presence>,
    pub seats: Arc<Seats>,
    pub locale: Locale,
    pub speaker: Option<SpeakerSeat>,
}

pub struct ServerHub {
    store: Arc<dyn LogStore>,
    epoch: String,
    supervisor: ActorRef<SupervisorMsg>,
    registry: Arc<Registry>,
    events: broadcast::Sender<Event>,
    presence: Arc<Presence>,
    seats: Arc<Seats>,
    locale: Locale,
    speaker: Option<SpeakerSeat>,
    next_conn: AtomicU64,
    briefs: Caches<BriefBuilder>,
    tasks: Caches<TaskBoard>,
    checkpoints: Caches<Checkpoints>,
    dockets: Caches<SpeakerDocket>,
}

/// Something folded from a committee's log, in seq order, that ignores what it
/// already saw.
trait Fold: Send + 'static {
    fn fresh(committee: &CommitteeId) -> Self;
    fn head(&self) -> Seq;
    fn observe(&mut self, event: &Event);
}

impl Fold for BriefBuilder {
    fn fresh(committee: &CommitteeId) -> Self {
        BriefBuilder::new(committee.clone())
    }
    fn head(&self) -> Seq {
        BriefBuilder::head(self)
    }
    fn observe(&mut self, event: &Event) {
        BriefBuilder::observe(self, event);
    }
}

impl Fold for TaskBoard {
    fn fresh(_: &CommitteeId) -> Self {
        TaskBoard::new()
    }
    fn head(&self) -> Seq {
        TaskBoard::head(self)
    }
    fn observe(&mut self, event: &Event) {
        TaskBoard::observe(self, event);
    }
}

impl Fold for SpeakerDocket {
    fn fresh(_: &CommitteeId) -> Self {
        SpeakerDocket::new()
    }
    fn head(&self) -> Seq {
        SpeakerDocket::head(self)
    }
    fn observe(&mut self, event: &Event) {
        SpeakerDocket::observe(self, event);
    }
}

impl Fold for Checkpoints {
    fn fresh(_: &CommitteeId) -> Self {
        Checkpoints::new()
    }
    fn head(&self) -> Seq {
        Checkpoints::head(self)
    }
    fn observe(&mut self, event: &Event) {
        Checkpoints::observe(self, event);
    }
}

/// A committee's fold and the epoch of the log it folded. The async mutex
/// around it is held while new events are read and folded, so two
/// concurrent readers of one committee fold each event once.
struct Cached<T> {
    epoch: String,
    fold: T,
}

type Caches<T> = Mutex<HashMap<CommitteeId, Arc<tokio::sync::Mutex<Cached<T>>>>>;

/// A registered live stream: every event the committee commits after
/// registration arrives on `events`, in commit order. The channel closes when
/// the server drops the connection (slow client, writer restart, shutdown).
/// Dropping the stream detaches it.
pub struct LiveStream {
    pub events: mpsc::Receiver<Arc<Event>>,
    /// Absent for a stream that is only reading.
    _detach: Option<Detach>,
}

struct Detach {
    proxy: ActorRef<ProxyMsg>,
    conn: ConnId,
}

impl Drop for Detach {
    fn drop(&mut self) {
        if self.proxy.cast(ProxyMsg::Detach(self.conn)).is_err() {
            // The proxy already stopped, and its senders went with it.
            tracing::trace!(conn = self.conn, "stream detached after its proxy stopped");
        }
    }
}

impl ServerHub {
    pub(crate) fn new(parts: HubParts) -> Self {
        let HubParts {
            store,
            epoch,
            supervisor,
            registry,
            events,
            presence,
            seats,
            locale,
            speaker,
        } = parts;
        Self {
            store,
            epoch,
            supervisor,
            registry,
            events,
            presence,
            seats,
            locale,
            speaker,
            next_conn: AtomicU64::new(1),
            briefs: Mutex::default(),
            tasks: Mutex::default(),
            checkpoints: Mutex::default(),
            dockets: Mutex::default(),
        }
    }

    /// Who holds this Senate's Speaker's seat (see
    /// [`Roster::speaker_seat`]); `None`: nobody presides.
    pub fn speaker_seat(&self) -> Option<&SpeakerSeat> {
        self.speaker.as_ref()
    }

    /// What awaits the Speaker on `committee` ([`senate_core::docket`]):
    /// the messages addressed to it that it has not dealt with, then every
    /// root question to an agent of this Senate that is not connected and
    /// has no answer, with the facts about every seated agent (connected,
    /// presence, latest checkpoint, open threads).
    pub async fn docket(&self, committee: &CommitteeId) -> Result<DocketPage, HubError> {
        let mut items = self.folded(&self.dockets, committee).await?.fold.items();
        let head = self.head(committee).await?;
        let board = self.tasks(committee).await?;
        let roster = self.seats.snapshot();
        let latest: HashMap<SenatorName, CheckpointFacts> = {
            let cached = self.folded(&self.checkpoints, committee).await?;
            roster
                .agents()
                .iter()
                .filter_map(|agent| {
                    let c = cached.fold.latest(agent)?;
                    Some((
                        agent.clone(),
                        CheckpointFacts {
                            seq: c.seq,
                            ts: c.ts,
                            summary: c.body.clone(),
                            branch: c.checkpoint.branch.clone(),
                        },
                    ))
                })
                .collect()
        };
        let presence = match self.presence(committee).await {
            Ok(page) => page.agents,
            Err(e) => {
                tracing::debug!(%committee, "docket without presence: {e}");
                Vec::new()
            }
        };
        let mut seats = Vec::new();
        for agent in roster.agents() {
            seats.push(SeatFacts {
                agent: agent.clone(),
                connected: self.presence.is_connected(committee, agent),
                presence: presence.iter().find(|p| &p.agent == agent).map(|p| p.state),
                last_seen: presence
                    .iter()
                    .find(|p| &p.agent == agent)
                    .and_then(|p| p.last_seen),
                checkpoint: latest.get(agent).cloned(),
                open_threads: board
                    .threads_for(agent, None)
                    .into_iter()
                    .filter(|t| t.waits().is_some())
                    .count(),
            });
        }
        for thread in board.threads(Some(StatusState::Processing)) {
            if thread.kind != Kind::Question || thread.answered {
                continue;
            }
            // Covering for an absent agent is the Speaker's duty toward
            // other agents: what is addressed to the seat's holder is that
            // agent's own question, delivered when its session opens.
            let holder = self.speaker.as_ref().and_then(SpeakerSeat::senator);
            let agents: Vec<SenatorName> = thread
                .agents()
                .filter(|a| Some(*a) != holder)
                .cloned()
                .collect();
            let away = !agents.is_empty()
                && agents
                    .iter()
                    .all(|a| roster.knows(a) && !self.presence.is_connected(committee, a));
            if !away {
                continue;
            }
            if let Some(event) = self.get(committee, thread.root).await? {
                items.push(DocketItem {
                    event,
                    awaiting: Awaiting::Away { agents },
                });
            }
        }
        items.sort_by_key(|item| item.event.seq);
        Ok(DocketPage {
            committee: committee.clone(),
            speaker: self.speaker.clone(),
            items,
            seats,
            head,
        })
    }

    /// The committee's fold in `caches`, brought up to the log's head. A failed
    /// read leaves it at the last page it folded, which is still a correct
    /// prefix.
    async fn folded<T: Fold>(
        &self,
        caches: &Caches<T>,
        committee: &CommitteeId,
    ) -> Result<tokio::sync::OwnedMutexGuard<Cached<T>>, HubError> {
        let cache = {
            let mut caches = caches.lock().unwrap_or_else(PoisonError::into_inner);
            caches
                .entry(committee.clone())
                .or_insert_with(|| {
                    Arc::new(tokio::sync::Mutex::new(Cached {
                        epoch: String::new(),
                        fold: T::fresh(committee),
                    }))
                })
                .clone()
        };
        let mut cached = cache.lock_owned().await;
        let epoch = self.store.epoch().await.map_err(log_error)?;
        if cached.epoch != epoch {
            *cached = Cached {
                epoch,
                fold: T::fresh(committee),
            };
        }
        loop {
            let after = cached.fold.head();
            let page = self.read(committee, after, SCAN_PAGE).await?;
            for event in &page {
                cached.fold.observe(event);
            }
            if page.len() < SCAN_PAGE {
                break;
            }
        }
        Ok(cached)
    }

    /// Start the writer of every committee in the log (see [`Server::start`]).
    ///
    /// [`Server::start`]: crate::Server::start
    pub(crate) async fn start_writers(self: Arc<Self>) {
        let committees = match self.committees().await {
            Ok(committees) => committees,
            Err(e) => {
                tracing::warn!("listing committees to start their writers: {e}");
                return;
            }
        };
        for committee in committees {
            if let Err(e) = self.writer(&committee).await {
                tracing::warn!(%committee, "starting the committee's writer: {e}");
            }
        }
    }

    /// The committee's live writer, from the registry or the supervisor.
    async fn writer(&self, committee: &CommitteeId) -> Result<ActorRef<CommitteeMsg>, HubError> {
        if let Some(writer) = self.registry.get(committee) {
            return Ok(writer);
        }
        let committee = committee.clone();
        match self
            .supervisor
            .call(
                |reply| SupervisorMsg::Writer(committee, reply),
                Some(CALL_TIMEOUT),
            )
            .await
        {
            Ok(CallResult::Success(result)) => result.map_err(HubError::Unavailable),
            Ok(CallResult::Timeout) => Err(HubError::Unavailable("supervisor timed out".into())),
            Ok(CallResult::SenderError) | Err(_) => {
                Err(HubError::Unavailable("the server is shutting down".into()))
            }
        }
    }

    /// Send a request to the committee's writer. A writer that died before
    /// receiving the message is replaced once and the message resent (it was
    /// never processed); a writer that died while handling it is reported as
    /// unavailable, since the request may or may not have committed.
    async fn ask_writer<T, F>(&self, committee: &CommitteeId, build: F) -> Result<T, HubError>
    where
        T: Send + 'static,
        F: Fn(ractor::RpcReplyPort<T>) -> CommitteeMsg,
    {
        for _ in 0..2 {
            let writer = self.writer(committee).await?;
            match writer.call(&build, Some(CALL_TIMEOUT)).await {
                Ok(CallResult::Success(value)) => return Ok(value),
                Ok(CallResult::Timeout) => {
                    return Err(HubError::Unavailable(format!(
                        "the writer for {committee} timed out"
                    )));
                }
                Ok(CallResult::SenderError) => {
                    return Err(HubError::Unavailable(format!(
                        "the writer for {committee} stopped while handling the request"
                    )));
                }
                Err(MessagingErr::SendErr(_) | MessagingErr::ChannelClosed) => continue,
                Err(MessagingErr::InvalidActorType) => {
                    return Err(HubError::Unavailable("writer has the wrong type".into()));
                }
            }
        }
        Err(HubError::Unavailable(format!(
            "the writer for {committee} keeps stopping"
        )))
    }

    /// The committee's highest committed seq; [`Seq::ZERO`] when it has none.
    pub async fn head(&self, committee: &CommitteeId) -> Result<Seq, HubError> {
        self.store.head(committee).await.map_err(log_error)
    }

    /// The language the server renders text for people in, from its
    /// configuration. It renders little itself (errors for bridges are
    /// English, for whoever debugs the client); components that do render,
    /// the Speaker and gateways, take their locale from here.
    pub fn locale(&self) -> Locale {
        self.locale
    }

    /// May `person`, authenticated, act as `senator` in this Senate? If it is
    /// an agent that is not yet seated, it is seated now: this is the first
    /// contact. `Ok(true)` when this call seated it. See
    /// [`Roster::may_act_as`] and [`Roster::seat`].
    pub fn act_as(&self, person: &Name, senator: &SenatorName) -> Result<bool, RosterError> {
        let seated = self.seats.act_as(person, senator)?;
        if seated {
            tracing::info!(%person, agent = %senator, "agent seated on first contact");
        }
        Ok(seated)
    }

    /// Replace the people this Senate knows, for a layer whose people come
    /// from somewhere other than configuration. Seated agents whose person
    /// is still listed stay seated; the presider is kept. Refused, changing
    /// nothing, when the new people break the roster's rules.
    pub fn set_seniors(&self, seniors: Vec<Senior>) -> Result<(), RosterError> {
        self.seats.replace_seniors(seniors)
    }

    /// Register a live stream of the committee's commits. With an `agent`,
    /// the stream is that agent's connection and counts toward its presence
    /// while it is open; with none, a person is only reading and no
    /// presence follows.
    pub async fn open_stream(
        &self,
        committee: &CommitteeId,
        agent: Option<&SenatorName>,
    ) -> Result<LiveStream, HubError> {
        let (sender, events) = mpsc::channel(STREAM_BUFFER);
        let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let Some(agent) = agent else {
            self.ask_writer(committee, |reply| CommitteeMsg::Watch {
                conn,
                sender: sender.clone(),
                reply,
            })
            .await?;
            // Only the writer may hold a strong sender, or the stream never
            // closes.
            drop(sender);
            return Ok(LiveStream {
                events,
                _detach: None,
            });
        };
        let proxy = self
            .ask_writer(committee, |reply| CommitteeMsg::Attach {
                agent: agent.clone(),
                conn,
                sender: sender.clone(),
                reply,
            })
            .await?
            .map_err(HubError::Unavailable)?;
        // Only the proxy may hold a strong sender, or the stream never closes.
        drop(sender);
        Ok(LiveStream {
            events,
            _detach: Some(Detach { proxy, conn }),
        })
    }

    /// One senator's named cursor. A senator is a path: two agents of one
    /// person have two cursors, and the person has a third.
    pub async fn owned_cursor(
        &self,
        committee: &CommitteeId,
        owner: &SenatorName,
        name: &str,
    ) -> Result<Seq, HubError> {
        Ok(self
            .store
            .cursor(committee, &CursorOwner::from(owner), name)
            .await
            .map_err(log_error)?
            .unwrap_or(Seq::ZERO))
    }

    pub async fn set_owned_cursor(
        &self,
        committee: &CommitteeId,
        owner: &SenatorName,
        name: &str,
        seq: Seq,
    ) -> Result<(), HubError> {
        self.store
            .set_cursor(committee, &CursorOwner::from(owner), name, seq)
            .await
            .map_err(log_error)
    }

    /// Record an agent's bridge heartbeat ([`Heartbeat::agent`]), stamped
    /// now. It keeps the agent's open stream counting as connected (see
    /// [`Hub::is_agent_connected`]) even if storing it fails: the bridge is
    /// alive either way.
    pub async fn record_heartbeat(
        &self,
        committee: &CommitteeId,
        heartbeat: Heartbeat,
    ) -> Result<(), HubError> {
        let agent = heartbeat.agent.clone();
        self.presence.heartbeat(committee, &agent, heartbeat.cursor);
        // The lease may change now (a returning agent is `online` at once).
        match self.writer(committee).await {
            Ok(writer) => {
                if writer.cast(CommitteeMsg::PresenceCheck(agent)).is_err() {
                    tracing::debug!(%committee, "writer stopped; its next tick checks the lease");
                }
            }
            Err(e) => tracing::warn!(%committee, "no writer to check the lease: {e}"),
        }
        self.store
            .record_heartbeat(committee, heartbeat, senate_log::now())
            .await
            .map_err(log_error)
    }

    /// The agent's latest heartbeat for the committee.
    pub async fn last_heartbeat(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<senate_log::HeartbeatRecord>, HubError> {
        self.store
            .last_heartbeat(committee, agent)
            .await
            .map_err(log_error)
    }
}

#[async_trait]
impl Hub for ServerHub {
    /// A stored event names concrete senators, so one still holding
    /// `senator::<person>/*` is rejected (use [`Hub::resolve_and_append`]),
    /// and a sender that may not change state (a guest) cannot write a kind
    /// or a meta key that does.
    async fn append(&self, event: NewEvent) -> Result<Appended, HubError> {
        if !event.is_resolved() {
            return Err(HubError::Rejected(
                "an address is still senator::<person>/*; resolve it before appending".into(),
            ));
        }
        check_may_change_state(&event).map_err(HubError::Rejected)?;
        let committee = event.committee.clone();
        self.ask_writer(&committee, |reply| {
            CommitteeMsg::Append(Box::new(event.clone()), reply)
        })
        .await?
        .map_err(log_error)
    }

    async fn read(
        &self,
        committee: &CommitteeId,
        after: Seq,
        limit: usize,
    ) -> Result<Vec<Event>, HubError> {
        self.store
            .read(committee, after, limit)
            .await
            .map_err(log_error)
    }

    async fn get(&self, committee: &CommitteeId, seq: Seq) -> Result<Option<Event>, HubError> {
        self.store.get(committee, seq).await.map_err(log_error)
    }

    fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    async fn committees(&self) -> Result<Vec<CommitteeId>, HubError> {
        self.store.committees().await.map_err(log_error)
    }

    async fn roster(&self) -> Result<Roster, HubError> {
        Ok(self.seats.snapshot())
    }

    /// An open stream, and either a heartbeat or the stream's opening within
    /// `presence_grace_secs` (so a sleeping laptop's half-open stream stops
    /// counting once its heartbeats stop).
    async fn is_agent_connected(&self, committee: &CommitteeId, agent: &SenatorName) -> bool {
        self.presence.is_connected(committee, agent)
    }

    async fn cursor(&self, committee: &CommitteeId, name: &str) -> Result<Seq, HubError> {
        Ok(self
            .store
            .cursor(committee, &CursorOwner::Hub, name)
            .await
            .map_err(log_error)?
            .unwrap_or(Seq::ZERO))
    }

    async fn set_cursor(
        &self,
        committee: &CommitteeId,
        name: &str,
        seq: Seq,
    ) -> Result<(), HubError> {
        self.store
            .set_cursor(committee, &CursorOwner::Hub, name, seq)
            .await
            .map_err(log_error)
    }

    async fn epoch(&self) -> Result<String, HubError> {
        Ok(self.epoch.clone())
    }

    async fn put_mirror(&self, mirror: MirrorRef) -> Result<(), HubError> {
        self.store.put_mirror(mirror).await.map_err(log_error)
    }

    async fn mirrors_of_seq(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        platform: &str,
    ) -> Result<Vec<MirrorRef>, HubError> {
        self.store
            .mirrors_of_seq(committee, seq, platform)
            .await
            .map_err(log_error)
    }

    async fn mirror_of_message(
        &self,
        platform: &str,
        message_ref: &str,
    ) -> Result<Option<MirrorRef>, HubError> {
        self.store
            .mirror_of_message(platform, message_ref)
            .await
            .map_err(log_error)
    }

    async fn thread(&self, committee: &CommitteeId, root: Seq) -> Result<Vec<Event>, HubError> {
        self.store.thread(committee, root).await.map_err(log_error)
    }

    async fn bindings(&self) -> Result<Vec<Event>, HubError> {
        self.store.bindings().await.map_err(log_error)
    }

    async fn search(
        &self,
        committee: &CommitteeId,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Event>, HubError> {
        self.store
            .search(committee, query, limit)
            .await
            .map_err(log_error)
    }

    /// Folds what the committee committed since the cached builder's head;
    /// the presence section is the writer's live view.
    async fn brief(&self, committee: &CommitteeId) -> Result<Brief, HubError> {
        let mut brief = self.folded(&self.briefs, committee).await?.fold.brief();
        match self.presence(committee).await {
            Ok(page) => brief.presence = page.agents,
            Err(e) => tracing::debug!(%committee, "brief with the logged presence: {e}"),
        }
        Ok(brief)
    }

    /// Folds what the committee committed since the cached board's head, like
    /// [`Hub::brief`].
    async fn tasks(&self, committee: &CommitteeId) -> Result<TaskBoard, HubError> {
        Ok(self.folded(&self.tasks, committee).await?.fold.clone())
    }

    async fn latest_checkpoint(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<CheckpointRecord>, HubError> {
        let cached = self.folded(&self.checkpoints, committee).await?;
        Ok(cached.fold.latest(agent).cloned())
    }

    /// The writer's leases, `lag` measured against its head now.
    async fn presence(&self, committee: &CommitteeId) -> Result<PresencePage, HubError> {
        let (head, agents) = self.ask_writer(committee, CommitteeMsg::Presence).await?;
        Ok(PresencePage { agents, head })
    }

    async fn mark_taken_over(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<Event>, HubError> {
        self.ask_writer(committee, |reply| CommitteeMsg::TakeOver {
            agent: agent.clone(),
            reply,
        })
        .await?
        .map_err(HubError::Rejected)
    }

    async fn reassign(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
        new_owner: &SenatorName,
        by: &Name,
    ) -> Result<Option<Event>, HubError> {
        self.ask_writer(committee, |reply| CommitteeMsg::Reassign {
            agent: agent.clone(),
            new_owner: new_owner.clone(),
            by: by.clone(),
            reply,
        })
        .await?
        .map_err(HubError::Rejected)
    }
}
