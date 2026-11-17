//! The platform-neutral half of a chat gateway.
//!
//! A chat gateway has two halves. The platform half is a private layer's: it
//! talks to one chat platform, implements [`ChatSurface`] for the Speaker to
//! post through, and receives what people write. The other half is the same
//! for every platform, and is [`ChatGateway`]: given each message the
//! platform delivers ([`Inbound`]), it writes what the log needs.
//!
//! - **Who said it.** A platform user is `senator::<handle>` when the roster
//!   knows that account ([`Roster::by_account`]), and
//!   `guest::<platform>/<id>` otherwise. Platform ids stay out of the log.
//! - **Bound groups** (the log's `binding` events from the Speaker, for this
//!   platform): a message becomes a `chat` event with `source`, `mirror`,
//!   `meta.addressing`, `to: [speaker]` when the bot was mentioned, and,
//!   when the message sits in a platform thread that mirrors a log thread,
//!   `thread` = that thread's root with `to` widened to the thread's agents.
//!   A person writing after an answer in such a thread corrects it
//!   (`meta.overrides`). Its event id is derived from the message, so a
//!   redelivery is stored once.
//! - **Unbound groups**: a message addressed to the Speaker (a mention, a
//!   reply to the bot, or anything in reply to or in the thread of one of the
//!   Speaker's own posts) is handed to the Speaker as an [`UnboundMention`];
//!   anything else is dropped.
//! - **Bind = grant.** When the log binds a group, the platform is asked
//!   for its full use ([`ChatSurface::adopt_group`], named by the latest
//!   message it delivered from that group); when the log leaves a group
//!   bound to nothing, the grant is given back
//!   ([`ChatSurface::release_group`]). The log decides, and the gateway
//!   keeps these guarantees ([`GrantPolicy`] sets the numbers):
//!   - *Grace.* A release waits [`GrantPolicy::release_grace`] (3 seconds)
//!     and is cancelled when the group is bound again within it, so a group
//!     moved from one committee to another is neither released nor adopted
//!     a second time.
//!   - *Retry.* A failed or unanswered adopt or release is tried again with
//!     a pause that doubles from 1 second to 30, with no end, until it
//!     succeeds or the log's bindings no longer want it (an unbind stops the
//!     retries of an adopt; a bind cancels a release). A failure is logged,
//!     never fatal.
//!   - *Never in the way.* Every call runs in a task of its own and is cut
//!     off after [`GrantPolicy::call_timeout`]. A slow or hanging platform
//!     delays no append, no delivery, and not the Speaker. Calls for one
//!     group are made one at a time, in the order wanted.
//!   - *After a restart* nothing is stored: the log's bindings are replayed,
//!     every group it says is bound is adopted again (idempotent; named by
//!     the latest chat the log holds from the group, else by its first new
//!     message), and every group it ever bound that is bound to nothing is
//!     released again (idempotent) once the grace has passed.
//!   - *Self-heal.* A post into a bound group that the platform refuses
//!     ([`SurfaceError::Rejected`]) while the group's grant is not confirmed
//!     adopts the group (waiting for an adopt already under way) and is
//!     tried once more. Posts do not otherwise wait for a grant, and a
//!     platform layer needs no heal of its own. A *root* post names its
//!     group. A *reply* names only the message it replies to, so the gateway
//!     finds that message's group from what it records: the log's mirror
//!     refs for a message the Speaker posted (the copy's target is the
//!     group, with an optional `#role` suffix), and the messages it delivered
//!     from the platform (each becomes a `chat` event whose `source` is the
//!     group and whose `mirror` is the message, and so does a message in an
//!     unbound group, so the bind message itself is known). When the group
//!     cannot be found (a message the Speaker did not post and the gateway
//!     has not been given since it started), or is not bound, nothing is
//!     healed and the refusal stands. One heal and one retry per post, never
//!     a loop.
//!   - A platform without grants answers `Unsupported`, which is noted once
//!     and never asked again: no retries, no work.
//!
//! The gateway is a [`Component`]: mount it with
//! [`Assembly::chat_gateway`](crate::Assembly::chat_gateway), which also
//! hands the Speaker the surface to post through ([`ChatGateway::mount`]).
//! That surface is the platform's, recording what the Speaker posted so a
//! reply to it in an unbound group is recognised.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use senate_core::{
    Address, AdoptedGroup, BindingAction, BindingPayload, Card, ChatSurface, CommitteeId,
    CreatedGroup, Event, EventId, Guest, Hub, HubError, Kind, Mirror, NewEvent, NewGroup,
    PostTarget, Posted, Roster, Seq, Source, SurfaceError, UnboundMention, is_provisional, meta,
    thread_participants,
};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::component::{Component, ComponentContext};

mod grants;

pub use grants::GrantPolicy;
use grants::{Grants, Want};

/// Capacity of the channel of mentions in unbound groups, gateway to
/// Speaker.
pub const UNBOUND_CAPACITY: usize = 64;
/// Appends retried before a message is given up on: the platform will not
/// deliver it again, so the gateway is patient, but not forever.
const APPEND_ATTEMPTS: u32 = 6;
const APPEND_RETRY: Duration = Duration::from_millis(200);
/// Platform messages whose group, or whose authorship by the Speaker, is
/// remembered.
const REMEMBERED_REFS: usize = 4_096;

/// Why the platform delivered a message, as `meta.addressing` spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addressing {
    /// The bot was mentioned.
    Mention,
    /// A reply the platform attributes to the bot.
    Reply,
    /// Group chat that mentions nobody in particular.
    Ambient,
}

impl Addressing {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::Reply => "reply",
            Self::Ambient => "ambient",
        }
    }
}

/// Someone a message mentions, by the platform's id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformMention {
    pub user: String,
    pub name: String,
}

/// One message a chat platform delivered from a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// The platform's stable id of the group.
    pub group_id: String,
    pub group_name: Option<String>,
    /// The platform's id of the sender.
    pub sender: String,
    pub message_ref: String,
    /// The platform thread holding the message, and that thread's root.
    pub thread_ref: Option<String>,
    pub thread_root: Option<String>,
    /// The message this one replies to.
    pub reply_to: Option<String>,
    pub addressing: Addressing,
    pub mentions: Vec<PlatformMention>,
    pub text: String,
}

impl Inbound {
    fn refs(&self) -> impl Iterator<Item = &str> {
        [&self.reply_to, &self.thread_root, &self.thread_ref]
            .into_iter()
            .flatten()
            .map(String::as_str)
    }
}

/// What became of one [`Inbound`].
#[derive(Debug, Clone, PartialEq)]
pub enum Delivered {
    /// A `chat` event in the bound committee's log.
    Recorded(Box<Event>),
    /// Handed to the Speaker as an [`UnboundMention`].
    Unbound,
    /// Nothing: empty text, or chat in an unbound group not addressed to the
    /// Speaker.
    Dropped,
}

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error(transparent)]
    Hub(#[from] HubError),
    #[error("the Speaker is not listening for mentions in unbound groups")]
    SpeakerGone,
    #[error("the gateway is not running")]
    NotRunning,
}

/// The surface and the channel the Speaker needs from one chat platform.
pub struct ChatMount {
    pub surface: Arc<dyn ChatSurface>,
    pub unbound: mpsc::Receiver<UnboundMention>,
}

impl ChatMount {
    /// A surface and a fresh channel; the sending end is for whatever turns
    /// the platform's messages into [`UnboundMention`]s.
    pub fn new(surface: Arc<dyn ChatSurface>) -> (Self, mpsc::Sender<UnboundMention>) {
        let (tx, unbound) = mpsc::channel(UNBOUND_CAPACITY);
        (Self { surface, unbound }, tx)
    }
}

/// The platform-neutral half of a chat gateway. Cheap to clone: every clone
/// is the same gateway.
#[derive(Clone)]
pub struct ChatGateway {
    inner: Arc<Inner>,
}

struct Inner {
    surface: Arc<dyn ChatSurface>,
    platform: String,
    hub: watch::Sender<Option<Arc<dyn Hub>>>,
    unbound_tx: mpsc::Sender<UnboundMention>,
    unbound_rx: Mutex<Option<mpsc::Receiver<UnboundMention>>>,
    state: Mutex<State>,
    /// A subscription of the delivering side, drained before each message so
    /// a binding the log just committed is never missed.
    fresh: Mutex<Option<broadcast::Receiver<Event>>>,
    policy: Mutex<GrantPolicy>,
    /// Cancelled when the daemon shuts down: grant tasks end.
    stop: Mutex<CancellationToken>,
}

#[derive(Default)]
struct State {
    table: BindingTable,
    /// Messages the Speaker posted, and the threads they are in.
    posted: Remembered,
    /// The latest message the platform delivered from each group: what names
    /// a group to adopt.
    latest_ref: HashMap<String, String>,
    /// What the platform's grants should be, and what it has confirmed.
    grants: Grants,
    /// The group of each message (and thread) the platform delivered, so a
    /// reply to one can be healed. Bounded; lost on restart.
    delivered: MessageGroups,
}

/// Platform message refs and the group they were delivered in.
#[derive(Default)]
struct MessageGroups {
    groups: HashMap<String, String>,
    order: VecDeque<String>,
}

impl MessageGroups {
    fn insert(&mut self, message_ref: &str, group_id: &str) {
        if self
            .groups
            .insert(message_ref.to_owned(), group_id.to_owned())
            .is_none()
        {
            self.order.push_back(message_ref.to_owned());
            while self.order.len() > REMEMBERED_REFS {
                if let Some(old) = self.order.pop_front() {
                    self.groups.remove(&old);
                }
            }
        }
    }

    fn get(&self, message_ref: &str) -> Option<&String> {
        self.groups.get(message_ref)
    }
}

#[derive(Default)]
struct Remembered {
    refs: HashSet<String>,
    order: VecDeque<String>,
}

impl Remembered {
    fn insert(&mut self, value: &str) {
        if self.refs.insert(value.to_owned()) {
            self.order.push_back(value.to_owned());
            while self.order.len() > REMEMBERED_REFS {
                if let Some(old) = self.order.pop_front() {
                    self.refs.remove(&old);
                }
            }
        }
    }

    fn contains(&self, value: &str) -> bool {
        self.refs.contains(value)
    }
}

impl ChatGateway {
    /// A gateway for the platform `surface` speaks to.
    pub fn new(surface: Arc<dyn ChatSurface>) -> Self {
        let (unbound_tx, unbound_rx) = mpsc::channel(UNBOUND_CAPACITY);
        let platform = surface.platform().to_owned();
        Self {
            inner: Arc::new(Inner {
                surface,
                platform,
                hub: watch::channel(None).0,
                unbound_tx,
                unbound_rx: Mutex::new(Some(unbound_rx)),
                state: Mutex::default(),
                fresh: Mutex::new(None),
                policy: Mutex::default(),
                stop: Mutex::new(CancellationToken::new()),
            }),
        }
    }

    /// Pace the platform's grants differently from the default
    /// ([`GrantPolicy::default`]). Set it before the gateway runs.
    #[must_use]
    pub fn with_grant_policy(self, policy: GrantPolicy) -> Self {
        *lock(&self.inner.policy) = policy;
        self
    }

    pub fn platform(&self) -> &str {
        &self.inner.platform
    }

    /// What the Speaker is handed: the platform's surface, recording what
    /// is posted through it, and the mentions this gateway finds in unbound
    /// groups. `None` after the first call: one gateway serves one Speaker.
    pub fn mount(&self) -> Option<ChatMount> {
        let unbound = lock(&self.inner.unbound_rx).take()?;
        Some(ChatMount {
            surface: Arc::new(RecordingSurface {
                inner: self.inner.clone(),
            }),
            unbound,
        })
    }

    /// Whether the gateway currently has `group_id` bound to a committee.
    pub fn committee_of(&self, group_id: &str) -> Option<CommitteeId> {
        self.inner.drain_fresh();
        lock(&self.inner.state)
            .table
            .committee_of(group_id)
            .cloned()
    }

    /// Handle one message the platform delivered. Waits until the gateway
    /// runs (it needs the log).
    pub async fn deliver(&self, inbound: Inbound) -> Result<Delivered, GatewayError> {
        let hub = self.inner.hub().await?;
        let committee = {
            self.inner.drain_fresh();
            let mut state = lock(&self.inner.state);
            state
                .latest_ref
                .insert(inbound.group_id.clone(), inbound.message_ref.clone());
            for message in std::iter::once(inbound.message_ref.as_str()).chain(inbound.refs()) {
                state.delivered.insert(message, &inbound.group_id);
            }
            let committee = state.table.committee_of(&inbound.group_id).cloned();
            if committee.is_some() {
                // A message from a bound group: if its grant is not
                // confirmed yet and nothing is working on it (a restart
                // that found nothing to name the group by), this names it.
                let State { grants, .. } = &mut *state;
                self.inner.want_grant(grants, &inbound.group_id, Want::Held);
            }
            committee
        };
        if inbound.text.trim().is_empty() {
            return Ok(Delivered::Dropped);
        }
        let roster = hub.roster().await?;
        match committee {
            Some(committee) => {
                let event = self
                    .inner
                    .record(hub.as_ref(), &roster, &committee, &inbound)
                    .await?;
                Ok(Delivered::Recorded(Box::new(event)))
            }
            None => self.inner.route_unbound(&roster, inbound).await,
        }
    }
}

#[async_trait]
impl Component for ChatGateway {
    fn name(&self) -> &str {
        "chat-gateway"
    }

    /// Keep the binding table current from the log and the platform's grants
    /// in line with it, until shutdown.
    async fn run(&self, ctx: ComponentContext) -> anyhow::Result<()> {
        // Subscribe before reading, so no binding falls between the two.
        let mut log = ctx.hub.subscribe();
        *lock(&self.inner.fresh) = Some(ctx.hub.subscribe());
        *lock(&self.inner.stop) = ctx.shutdown.clone();
        let table = BindingTable::rebuild(ctx.hub.as_ref(), &self.inner.platform).await?;
        self.inner.replace_table(table);
        self.inner.hub.send_replace(Some(ctx.hub.clone()));
        loop {
            let received = tokio::select! {
                () = ctx.shutdown.cancelled() => return Ok(()),
                received = log.recv() => received,
            };
            match received {
                Ok(event) if event.kind == Kind::Binding => self.inner.observe(&event),
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "log subscription lagged; rebuilding bindings");
                    let table =
                        BindingTable::rebuild(ctx.hub.as_ref(), &self.inner.platform).await?;
                    self.inner.replace_table(table);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    anyhow::bail!("the log subscription closed")
                }
            }
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a platform user is in the log.
pub fn sender_address(roster: &Roster, platform: &str, user: &str) -> Address {
    if let Some(senior) = roster.by_account(platform, user) {
        return Address::senior(&senior.handle);
    }
    match Guest::new(platform, user) {
        Ok(guest) => Address::Guest(guest),
        // An id that cannot stand in an address is named by its hash: the
        // same person is still the same guest.
        Err(_) => {
            let id = EventId::derived(0, &format!("{platform}\0{user}")).to_string();
            Guest::new(platform, &id.to_lowercase())
                .map(Address::Guest)
                .unwrap_or(Address::Committee)
        }
    }
}

impl Inner {
    /// The group a platform message belongs to, from what is recorded: a
    /// message the platform delivered, else the log's mirror ref of a
    /// message the Speaker posted. `None` when neither knows it, or the
    /// log cannot be read now.
    async fn group_of_message(&self, message_ref: &str) -> Option<String> {
        if let Some(group) = lock(&self.state).delivered.get(message_ref) {
            return Some(group.clone());
        }
        let hub = self.hub.borrow().clone()?;
        let mirror = hub
            .mirror_of_message(&self.platform, message_ref)
            .await
            .ok()??;
        lock(&self.state)
            .table
            .group_named_by(&mirror.target)
            .map(str::to_owned)
    }

    /// A reply to `message_ref` was refused: heal the group the message is
    /// in, when it can be found ([`Inner::heal`] says when to retry).
    async fn heal_reply(self: &Arc<Self>, message_ref: &str) -> bool {
        match self.group_of_message(message_ref).await {
            Some(group) => self.heal(&group).await,
            None => false,
        }
    }

    async fn hub(&self) -> Result<Arc<dyn Hub>, GatewayError> {
        let mut hub = self.hub.subscribe();
        let ready = hub
            .wait_for(Option::is_some)
            .await
            .map_err(|_| GatewayError::NotRunning)?;
        ready.clone().ok_or(GatewayError::NotRunning)
    }

    /// Apply every binding the delivering side's subscription holds.
    fn drain_fresh(self: &Arc<Self>) {
        let mut fresh = lock(&self.fresh);
        let Some(rx) = fresh.as_mut() else { return };
        loop {
            match rx.try_recv() {
                Ok(event) if event.kind == Kind::Binding => self.observe(&event),
                Ok(_) => {}
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    // The pump rebuilds on its own lag; this side only needs
                    // what is newest, which keeps coming.
                }
                Err(_) => return,
            }
        }
    }

    /// A live binding event: apply it, then grant the group a bind leaves
    /// bound, or (after the grace) release the one an unbind leaves bound to
    /// nothing.
    fn observe(self: &Arc<Self>, event: &Event) {
        let mut state = lock(&self.state);
        let Some(binding) = state.table.observe(event, &self.platform) else {
            return;
        };
        let group = binding.group_id;
        let bound = state.table.committee_of(&group).is_some();
        let want = match binding.action {
            BindingAction::Bind if bound => Want::Held,
            BindingAction::Unbind if !bound => Want::Released,
            _ => return,
        };
        let State { grants, .. } = &mut *state;
        self.want_grant(grants, &group, want);
    }

    /// Replace the table with one rebuilt from the log, then bring the
    /// grants in line with it: every group the log ever bound is adopted
    /// while bound and released once bound to nothing. What the platform
    /// already confirmed this run is not asked again; after a restart
    /// nothing is confirmed, so every group is (idempotently).
    fn replace_table(self: &Arc<Self>, table: BindingTable) {
        let mut state = lock(&self.state);
        state.table = table;
        let groups: BTreeSet<String> = state.table.ever.iter().cloned().collect();
        for group in groups {
            let want = if state.table.committee_of(&group).is_some() {
                Want::Held
            } else {
                Want::Released
            };
            let State { grants, .. } = &mut *state;
            self.want_grant(grants, &group, want);
        }
    }

    async fn route_unbound(
        &self,
        roster: &Roster,
        inbound: Inbound,
    ) -> Result<Delivered, GatewayError> {
        // A mention, or a reply the platform attributes to the bot, is for
        // the Speaker. So is anything answering one of its posts, however
        // the platform classed it: the Speaker replies in the thread of the
        // person's message, so the thread is the Speaker's while its root is
        // the person's.
        let answers_us = {
            let state = lock(&self.state);
            inbound.refs().any(|r| state.posted.contains(r))
        };
        if !(answers_us || matches!(inbound.addressing, Addressing::Mention | Addressing::Reply)) {
            return Ok(Delivered::Dropped);
        }
        let mention = UnboundMention {
            platform: self.platform.clone(),
            group_id: inbound.group_id,
            group_name: inbound.group_name,
            message_ref: inbound.message_ref,
            thread_ref: inbound.thread_ref,
            reply_to_message_ref: inbound.reply_to,
            thread_root_message_ref: inbound.thread_root,
            addressing: inbound.addressing.as_str().to_owned(),
            sender: sender_address(roster, &self.platform, &inbound.sender),
            text: inbound.text,
        };
        self.unbound_tx
            .send(mention)
            .await
            .map_err(|_| GatewayError::SpeakerGone)?;
        Ok(Delivered::Unbound)
    }

    async fn record(
        &self,
        hub: &dyn Hub,
        roster: &Roster,
        committee: &CommitteeId,
        inbound: &Inbound,
    ) -> Result<Event, GatewayError> {
        let id = EventId::derived(
            0,
            &format!(
                "chat\0{}\0{committee}\0{}",
                self.platform, inbound.message_ref
            ),
        );
        let mut pause = APPEND_RETRY;
        let mut attempt = 1;
        loop {
            let result = match self.chat_event(hub, roster, committee, inbound).await {
                Ok(chat) => hub.append(chat.with_id(id.clone())).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(appended) => return Ok(appended.event),
                Err(error @ (HubError::Unavailable(_) | HubError::Other(_)))
                    if attempt < APPEND_ATTEMPTS =>
                {
                    tracing::warn!(%committee, %error, attempt, "cannot record chat yet; retrying");
                    tokio::time::sleep(pause).await;
                    pause = pause.saturating_mul(2);
                    attempt += 1;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    async fn chat_event(
        &self,
        hub: &dyn Hub,
        roster: &Roster,
        committee: &CommitteeId,
        inbound: &Inbound,
    ) -> Result<NewEvent, HubError> {
        let from = sender_address(roster, &self.platform, &inbound.sender);
        let mut chat = NewEvent::new(committee.clone(), Kind::Chat, from, inbound.text.clone())
            .with_source(Source {
                platform: self.platform.clone(),
                group_id: inbound.group_id.clone(),
                group_name: inbound.group_name.clone(),
            })
            .with_mirror(Mirror {
                platform: self.platform.clone(),
                message_ref: inbound.message_ref.clone(),
                thread_ref: inbound.thread_ref.clone(),
            })
            .with_meta(meta::ADDRESSING, json!(inbound.addressing.as_str()));
        if let Some(reply_to) = &inbound.reply_to {
            chat = chat.with_meta(meta::REPLY_TO, json!(reply_to));
        }
        if let Some(root) = &inbound.thread_root {
            chat = chat.with_meta(meta::THREAD_ROOT, json!(root));
        }
        if !inbound.mentions.is_empty() {
            let mentioned: Vec<Value> = inbound
                .mentions
                .iter()
                .map(|m| {
                    json!({
                        "address": sender_address(roster, &self.platform, &m.user).to_string(),
                        "name": m.name,
                    })
                })
                .collect();
            chat = chat.with_meta(meta::MENTIONS, Value::Array(mentioned));
        }
        let mut to = Vec::new();
        if let Some(thread) = self.mirrored_thread(hub, committee, inbound).await? {
            chat = chat.in_thread(thread.root);
            // A person speaking after an answer corrects it: the agents are
            // told whose answer this overrides.
            if let Some(answer) = thread.latest_answer
                && chat.from.as_senator().is_some()
            {
                chat = chat.with_meta(meta::OVERRIDES, json!(answer.0));
            }
            to.extend(thread.agents);
        }
        if inbound.addressing == Addressing::Mention {
            to.push(Address::Speaker);
        }
        Ok(chat.with_to(to))
    }

    /// The log thread a platform message belongs to: the one whose copy is
    /// the message's thread root or, failing that, the message it replies
    /// to.
    async fn mirrored_thread(
        &self,
        hub: &dyn Hub,
        committee: &CommitteeId,
        inbound: &Inbound,
    ) -> Result<Option<MirroredThread>, HubError> {
        for candidate in [&inbound.thread_root, &inbound.reply_to]
            .into_iter()
            .flatten()
        {
            let Some(mirror) = hub.mirror_of_message(&self.platform, candidate).await? else {
                continue;
            };
            if &mirror.committee != committee {
                continue;
            }
            // The Speaker's pinned cards are copies of no thread: a reply to
            // one is plain chat in the group.
            if mirror.target.contains("#pinned-") {
                continue;
            }
            let Some(hit) = hub.get(committee, mirror.seq).await? else {
                continue;
            };
            let root = hit.thread.unwrap_or(hit.seq);
            let thread = hub.thread(committee, root).await?;
            let people = hub.roster().await?.people();
            let latest_answer = thread
                .iter()
                .filter(|e| e.kind == Kind::Answer && !is_provisional(e))
                .map(|e| e.seq)
                .max();
            return Ok(Some(MirroredThread {
                root,
                agents: thread_agents(&thread, root, &people),
                latest_answer,
            }));
        }
        Ok(None)
    }
}

/// A log thread a platform reply landed in.
struct MirroredThread {
    root: Seq,
    /// The agents taking part in it.
    agents: Vec<Address>,
    /// Its latest answer, if any (a provisional one is none).
    latest_answer: Option<Seq>,
}

fn thread_agents(thread: &[Event], root: Seq, people: &senate_core::People) -> Vec<Address> {
    let participants = match thread.split_first() {
        Some((first, replies)) if first.seq == root => thread_participants(first, replies),
        // The root is not stored (it always should be): its replies'
        // authors still take part.
        _ => {
            let mut from: Vec<Address> = Vec::new();
            for event in thread {
                if !from.contains(&event.from) {
                    from.push(event.from.clone());
                }
            }
            from
        }
    };
    participants
        .into_iter()
        .filter(|a| people.is_agent_address(a))
        .collect()
}

/// Group to committee, as the log's `binding` events say.
#[derive(Debug, Default)]
struct BindingTable {
    groups: HashMap<String, CommitteeId>,
    /// Every group any binding named, bound now or not.
    ever: HashSet<String>,
    /// Highest binding seq applied per committee, so an event seen twice
    /// (the replay and the subscription) is applied once.
    applied: HashMap<CommitteeId, Seq>,
}

impl BindingTable {
    async fn rebuild(hub: &dyn Hub, platform: &str) -> Result<Self, HubError> {
        let mut bindings = Vec::new();
        let mut applied: HashMap<CommitteeId, Seq> = HashMap::new();
        for event in hub.bindings().await? {
            let seen = applied.entry(event.committee.clone()).or_default();
            *seen = (*seen).max(event.seq);
            if let Some(binding) = binding_of(&event, platform) {
                bindings.push((event.ts, event.seq, event.committee, binding));
            }
        }
        // Bindings of different committees interleave: replay in time order.
        bindings.sort_by_key(|(ts, seq, _, _)| (*ts, *seq));
        let mut table = Self {
            applied,
            ..Self::default()
        };
        for (_, _, committee, binding) in bindings {
            table.apply(&committee, &binding);
        }
        Ok(table)
    }

    /// Apply a live binding event; the binding applied, if it was new.
    fn observe(&mut self, event: &Event, platform: &str) -> Option<BindingPayload> {
        let applied = self.applied.entry(event.committee.clone()).or_default();
        if event.seq <= *applied {
            return None;
        }
        *applied = event.seq;
        let binding = binding_of(event, platform)?;
        self.apply(&event.committee, &binding);
        Some(binding)
    }

    fn apply(&mut self, committee: &CommitteeId, binding: &BindingPayload) {
        self.ever.insert(binding.group_id.clone());
        match binding.action {
            BindingAction::Bind => {
                self.groups
                    .insert(binding.group_id.clone(), committee.clone());
            }
            BindingAction::Unbind => {
                if self.groups.get(&binding.group_id) == Some(committee) {
                    self.groups.remove(&binding.group_id);
                }
            }
        }
    }

    fn committee_of(&self, group_id: &str) -> Option<&CommitteeId> {
        self.groups.get(group_id)
    }

    /// The bound group a mirror's target names: the group itself, or the
    /// group with a `#role` suffix (the longest match wins).
    fn group_named_by(&self, target: &str) -> Option<&str> {
        self.groups
            .keys()
            .filter(|g| {
                target == g.as_str()
                    || target
                        .strip_prefix(g.as_str())
                        .is_some_and(|rest| rest.starts_with('#'))
            })
            .max_by_key(|g| g.len())
            .map(String::as_str)
    }
}

/// A binding on this platform the table may act on. Only the Speaker binds
/// groups: a `binding` from anyone else is ignored, whatever it says (the
/// server refuses the kind from senators too; a wrong row here would route
/// a group's chat into another committee's log).
fn binding_of(event: &Event, platform: &str) -> Option<BindingPayload> {
    if event.kind != Kind::Binding || event.from != Address::Speaker {
        return None;
    }
    event.binding().filter(|b| b.platform == platform)
}

/// The platform's surface as the Speaker gets it: every call goes through,
/// and what was posted is remembered.
struct RecordingSurface {
    inner: Arc<Inner>,
}

impl RecordingSurface {
    /// A post into a bound group was refused and the platform has not
    /// confirmed the group's grant (a bind and the Speaker's first posts
    /// there happen together, while the adopt is still under way): adopt it
    /// now. A reply names a message, not a group: the group is found from
    /// the message. True when the post should be tried once more.
    async fn refused_for_want_of_grant(
        &self,
        target: &PostTarget,
        result: &Result<Posted, SurfaceError>,
    ) -> bool {
        match (target, result) {
            (PostTarget::NewRoot { group_id }, Err(SurfaceError::Rejected(_))) => {
                self.inner.heal(group_id).await
            }
            (PostTarget::Reply { message_ref, .. }, Err(SurfaceError::Rejected(_))) => {
                self.inner.heal_reply(message_ref).await
            }
            _ => false,
        }
    }

    fn posted(&self, result: Result<Posted, SurfaceError>) -> Result<Posted, SurfaceError> {
        if let Ok(posted) = &result {
            let mut state = lock(&self.inner.state);
            state.posted.insert(&posted.message_ref);
            if let Some(thread) = &posted.thread_ref {
                state.posted.insert(thread);
            }
        }
        result
    }
}

#[async_trait]
impl ChatSurface for RecordingSurface {
    fn platform(&self) -> &str {
        &self.inner.platform
    }

    async fn post(
        &self,
        target: PostTarget,
        markdown: &str,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        let mut posted = self
            .inner
            .surface
            .post(target.clone(), markdown, operation_id)
            .await;
        if self.refused_for_want_of_grant(&target, &posted).await {
            posted = self
                .inner
                .surface
                .post(target, markdown, operation_id)
                .await;
        }
        self.posted(posted)
    }

    async fn post_mentioning(
        &self,
        target: PostTarget,
        markdown: &str,
        user: &str,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        let mut posted = self
            .inner
            .surface
            .post_mentioning(target.clone(), markdown, user, operation_id)
            .await;
        if self.refused_for_want_of_grant(&target, &posted).await {
            posted = self
                .inner
                .surface
                .post_mentioning(target, markdown, user, operation_id)
                .await;
        }
        self.posted(posted)
    }

    async fn post_card(
        &self,
        target: PostTarget,
        card: &Card,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        let mut posted = self
            .inner
            .surface
            .post_card(target.clone(), card, operation_id)
            .await;
        if self.refused_for_want_of_grant(&target, &posted).await {
            posted = self
                .inner
                .surface
                .post_card(target, card, operation_id)
                .await;
        }
        self.posted(posted)
    }

    async fn update_card(
        &self,
        message_ref: &str,
        card: &Card,
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        self.inner
            .surface
            .update_card(message_ref, card, operation_id)
            .await
    }

    async fn pin(&self, message_ref: &str, operation_id: &str) -> Result<(), SurfaceError> {
        self.inner.surface.pin(message_ref, operation_id).await
    }

    async fn create_group(
        &self,
        group: &NewGroup,
        operation_id: &str,
    ) -> Result<CreatedGroup, SurfaceError> {
        self.inner.surface.create_group(group, operation_id).await
    }

    async fn invite(
        &self,
        group_id: &str,
        users: &[String],
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        self.inner
            .surface
            .invite(group_id, users, operation_id)
            .await
    }

    async fn rename_group(
        &self,
        group_id: &str,
        name: &str,
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        self.inner
            .surface
            .rename_group(group_id, name, operation_id)
            .await
    }

    async fn adopt_group(&self, message_ref: &str) -> Result<AdoptedGroup, SurfaceError> {
        self.inner.surface.adopt_group(message_ref).await
    }

    async fn release_group(&self, group_id: &str) -> Result<bool, SurfaceError> {
        self.inner.surface.release_group(group_id).await
    }
}

#[cfg(test)]
mod tests {
    use senate_core::{Account, Name, Senior};

    use super::*;

    fn roster() -> Roster {
        let mut alice = Senior::new(Name::new("alice").unwrap());
        alice.accounts.push(Account {
            platform: "chat".into(),
            id: "u_alice".into(),
        });
        Roster::new(vec![alice], Vec::new()).unwrap()
    }

    #[test]
    fn people_are_found_by_account_and_everyone_else_is_a_guest() {
        let roster = roster();
        assert_eq!(
            sender_address(&roster, "chat", "u_alice").to_string(),
            "senator::alice"
        );
        assert_eq!(
            sender_address(&roster, "chat", "u_9f").to_string(),
            "guest::chat/u_9f"
        );
        // The same account on another platform is someone else.
        assert_eq!(
            sender_address(&roster, "other", "u_alice").to_string(),
            "guest::other/u_alice"
        );
        // An id no address can hold is named by a stable hash.
        let odd = sender_address(&roster, "chat", "has spaces / and slashes");
        assert!(odd.as_guest().is_some(), "{odd}");
        assert_eq!(
            odd,
            sender_address(&roster, "chat", "has spaces / and slashes")
        );
    }
}
