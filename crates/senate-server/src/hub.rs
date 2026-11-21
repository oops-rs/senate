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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use ractor::rpc::CallResult;
use ractor::{ActorRef, MessagingErr};
use senate_core::alias::{AliasMatch, AliasName, CommitteeAliases, QualifiedAlias};
use senate_core::api::{
    ChairInfo, ChildSeat, ChildSenate, CommitteeSeatInfo, Heartbeat, PresencePage, Resolved,
    Secretaries,
};
use senate_core::hub::SCAN_PAGE;
use senate_core::{
    Address, AgentPresence, Appended, Awaiting, Brief, BriefBuilder, ChairSource, CheckpointFacts,
    CheckpointRecord, Checkpoints, CommitteeId, CommitteeSeatChange, DocketItem, DocketPage,
    DutyFold, Event, EventId, Hub, HubError, Kind, Locale, Locator, MirrorRef, Name, NewEvent,
    Office, Origin, People, Principal, Roster, RosterError, SeatFacts, SenatePath, SenatorName,
    Seq, SpeakerDocket, SpeakerSeat, StatusState, TaskBoard, meta,
};
use senate_log::{CursorOwner, LogError, LogStore};
use tokio::sync::{broadcast, mpsc};

use crate::actors::committee::CommitteeMsg;
use crate::actors::presence::{ConnId, Presence};
use crate::actors::proxy::ProxyMsg;
use crate::actors::supervisor::{Registry, SupervisorMsg};
use crate::chairs::{ChairDocket, ChairView};
use crate::registry;
use crate::rules::check_may_change_state;
use crate::seats::Seats;

mod secretary_work;
pub(crate) use secretary_work::SecretaryWorkError;

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
    pub built_in: Option<SenatorName>,
    pub notes: Vec<String>,
    pub children: BTreeMap<Name, ChildSenate>,
    pub aliases: CommitteeAliases,
    pub senate: Name,
    pub parent: Option<SenatePath>,
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
    /// Who worked the desk first when the server started (the older
    /// single seat; [`ServerHub::office`] is the live view).
    speaker: Option<SpeakerSeat>,
    built_in: Option<SenatorName>,
    notes: Vec<String>,
    children: RwLock<BTreeMap<Name, ChildSenate>>,
    aliases: tokio::sync::RwLock<CommitteeAliases>,
    secretary_gate: tokio::sync::Mutex<()>,
    senate: Name,
    parent: RwLock<Option<SenatePath>>,
    next_conn: AtomicU64,
    briefs: Caches<BriefBuilder>,
    tasks: Caches<TaskBoard>,
    checkpoints: Caches<Checkpoints>,
    dockets: Caches<SpeakerDocket>,
    chairs: Caches<ChairView>,
    chair_dockets: Caches<ChairDocket>,
    duties: Caches<DutyFold>,
}

/// Something folded from a committee's log, in seq order, that ignores what it
/// already saw.
trait Fold: Send + 'static {
    fn fresh(committee: &CommitteeId, people: &People) -> Self;
    fn head(&self) -> Seq;
    fn observe(&mut self, event: &Event);
}

impl Fold for BriefBuilder {
    fn fresh(committee: &CommitteeId, _: &People) -> Self {
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
    fn fresh(_: &CommitteeId, people: &People) -> Self {
        TaskBoard::new(people.clone())
    }
    fn head(&self) -> Seq {
        TaskBoard::head(self)
    }
    fn observe(&mut self, event: &Event) {
        TaskBoard::observe(self, event);
    }
}

impl Fold for SpeakerDocket {
    fn fresh(_: &CommitteeId, people: &People) -> Self {
        SpeakerDocket::with_people(people.clone())
    }
    fn head(&self) -> Seq {
        SpeakerDocket::head(self)
    }
    fn observe(&mut self, event: &Event) {
        SpeakerDocket::observe(self, event);
    }
}

impl Fold for ChairView {
    fn fresh(_: &CommitteeId, people: &People) -> Self {
        ChairView::new(people.clone())
    }
    fn head(&self) -> Seq {
        ChairView::head(self)
    }
    fn observe(&mut self, event: &Event) {
        ChairView::observe(self, event);
    }
}

impl Fold for ChairDocket {
    fn fresh(_: &CommitteeId, people: &People) -> Self {
        ChairDocket::new(people.clone())
    }
    fn head(&self) -> Seq {
        ChairDocket::head(self)
    }
    fn observe(&mut self, event: &Event) {
        ChairDocket::observe(self, event);
    }
}

impl Fold for DutyFold {
    fn fresh(_: &CommitteeId, _: &People) -> Self {
        DutyFold::new()
    }
    fn head(&self) -> Seq {
        DutyFold::head(self)
    }
    fn observe(&mut self, event: &Event) {
        DutyFold::observe(self, event);
    }
}

impl Fold for Checkpoints {
    fn fresh(_: &CommitteeId, _: &People) -> Self {
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
            built_in,
            notes,
            children,
            aliases,
            senate,
            parent,
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
            built_in,
            notes,
            children: RwLock::new(children),
            aliases: tokio::sync::RwLock::new(aliases),
            secretary_gate: tokio::sync::Mutex::new(()),
            senate,
            parent: RwLock::new(parent),
            next_conn: AtomicU64::new(1),
            briefs: Mutex::default(),
            tasks: Mutex::default(),
            checkpoints: Mutex::default(),
            dockets: Mutex::default(),
            chairs: Mutex::default(),
            chair_dockets: Mutex::default(),
            duties: Mutex::default(),
        }
    }

    /// Who worked the Speaker's desk first when the server started, as the
    /// older single seat (see [`Roster::speaker_seat`]); `None`: nobody
    /// did. [`ServerHub::office`] and [`ServerHub::desk_seat`] are live.
    pub fn speaker_seat(&self) -> Option<&SpeakerSeat> {
        self.speaker.as_ref()
    }

    /// Who works the Speaker's desk first now, as the older single seat.
    pub fn desk_seat(&self) -> Option<SpeakerSeat> {
        self.seats
            .snapshot()
            .speaker_seat(self.built_in.is_some())
            .ok()
            .flatten()
    }

    /// The Speaker's office now: who the Speaker is and who works its desk.
    pub fn office(&self) -> Option<Office> {
        self.seats.snapshot().office()
    }

    /// The Speaker's office, and every later change of it (the secretaries
    /// set, a Speaker seated): what every stream tells its client.
    pub fn office_changes(&self) -> tokio::sync::watch::Receiver<Option<Office>> {
        self.seats.office_changes()
    }

    /// The built-in session's name, when the daemon runs it.
    pub fn built_in(&self) -> Option<&SenatorName> {
        self.built_in.as_ref()
    }

    /// What this start assumed reading an older configuration, in the
    /// server's language: for whoever shows the Senate's status.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// The child Senates registered with this one.
    pub fn children(&self) -> Vec<ChildSenate> {
        self.children
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    /// One registered child Senate.
    pub fn child(&self, name: &Name) -> Option<ChildSenate> {
        self.children
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
    }

    /// Register (or register again) a child Senate, kept in the store.
    /// The caller has checked who may.
    pub async fn register_child(&self, child: ChildSenate) -> Result<ChildSenate, HubError> {
        registry::put_child(self.store.as_ref(), &child)
            .await
            .map_err(log_error)?;
        let mut children = self
            .children
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // Registering again (a reconnect) keeps what it last declared.
        let child = match children.get(&child.name) {
            Some(known) if known.speaker == child.speaker => ChildSenate {
                seats: known.seats.clone(),
                last_seen: known.last_seen,
                ..child
            },
            _ => child,
        };
        children.insert(child.name.clone(), child.clone());
        tracing::info!(child = %child.name, speaker = %child.speaker, "child Senate registered");
        Ok(child)
    }

    /// The seats a registered child declares, replacing the last ones; the
    /// child is seen now. Kept in memory only: after a restart nothing is
    /// known of a child (it is neither online nor known to be offline,
    /// [`ChildSenate::is_offline`]) until it declares them again, on its
    /// next poll. `None`: no such child.
    pub fn set_child_seats(&self, name: &Name, seats: Vec<ChildSeat>) -> Option<ChildSenate> {
        let mut children = self
            .children
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let child = children.get_mut(name)?;
        child.seats = seats;
        child.last_seen = Some(chrono::Utc::now());
        Some(child.clone())
    }

    /// Every secretary relation of this Senate.
    pub fn secretaries(&self) -> Secretaries {
        self.seats.snapshot().secretaries().clone()
    }

    /// Register `principal`'s secretaries (empty: none), kept in the
    /// store. The roster's rules apply ([`Roster::set_secretaries`]), and a
    /// secretary is an agent seated here or the built-in session. The
    /// caller has checked who may.
    pub async fn set_secretaries(
        &self,
        principal: SenatorName,
        secretaries: Vec<SenatorName>,
    ) -> Result<Secretaries, HubError> {
        let mut check = self.seats.snapshot();
        for secretary in &secretaries {
            if !check.is_agent(secretary) && !check.is_person(secretary) {
                return Err(HubError::Rejected(format!(
                    "{} is no agent seated in this Senate: a secretary is an agent of the Senate or the built-in session",
                    Address::senator(secretary)
                )));
            }
        }
        check
            .set_secretaries(principal.clone(), secretaries.clone())
            .map_err(|e| HubError::Rejected(e.to_string()))?;
        registry::put_secretaries(self.store.as_ref(), &principal, &secretaries)
            .await
            .map_err(log_error)?;
        self.seats
            .update(|roster| roster.set_secretaries(principal, secretaries))
            .map_err(|e| HubError::Rejected(e.to_string()))?;
        Ok(self.secretaries())
    }

    pub(crate) async fn aliases(&self) -> CommitteeAliases {
        self.aliases.read().await.clone()
    }

    /// The agent `name` stands for, written from `committee` (the writer's
    /// own, when there is one): the rule for callers that cannot ask a
    /// person ([`AliasMatch::non_interactive`]). A name that is no alias
    /// stands for itself; an alias several agents have, none of them on
    /// `committee`, is refused with the qualified candidates.
    pub(crate) async fn primary_name(
        &self,
        name: &SenatorName,
        committee: Option<&CommitteeId>,
    ) -> Result<SenatorName, HubError> {
        self.aliases
            .read()
            .await
            .resolve_bare(name, committee)
            .non_interactive(name)
            .map_err(|e| HubError::Rejected(e.to_string()))
    }

    /// The committees `agent` sits in ([`Hub::committee_agents`]: seen
    /// there and not unseated).
    pub(crate) async fn committees_of(
        &self,
        agent: &SenatorName,
    ) -> Result<Vec<CommitteeId>, HubError> {
        let mut sits = Vec::new();
        for committee in self.committees().await? {
            if self.committee_agents(&committee).await?.contains(agent) {
                sits.push(committee);
            }
        }
        Ok(sits)
    }

    /// Give `primary` the alias `request.alias` on a committee. An older
    /// bridge names no committee: the agent's only committee is used, and
    /// one that sits in several (or none) is told to upgrade.
    pub(crate) async fn set_alias(
        &self,
        person: &Name,
        primary: &SenatorName,
        request: senate_core::api::SetAlias,
    ) -> Result<QualifiedAlias, HubError> {
        let committee = match request.committee {
            Some(committee) => committee,
            None => match self.committees_of(primary).await?.as_slice() {
                [only] => only.clone(),
                _ => {
                    return Err(HubError::Rejected(format!(
                        "aliases are now per committee and {primary} does not sit in exactly \
                         one; upgrade this bridge so it names the committee"
                    )));
                }
            },
        };
        let mut aliases = self.aliases.write().await;
        let roster = self.seats.snapshot();
        if roster.may_act_as(person, primary).is_err()
            || roster.built_in() == Some(primary)
            || roster.is_person(primary)
            || !primary.is_session_name()
        {
            return Err(HubError::Rejected(format!(
                "{person} cannot set an alias for {primary}"
            )));
        }
        let alias = request.alias;
        let qualified = |alias: &SenatorName| QualifiedAlias {
            committee: committee.clone(),
            alias: alias.clone(),
            primary: primary.clone(),
        };
        if let Some(current) = aliases.alias_of(&committee, primary)
            && (request.initial || current == &alias)
        {
            return Ok(qualified(current));
        }
        if roster.knows(&alias) || alias.is_session_name() {
            return Err(HubError::Rejected(format!(
                "senator name {alias} is already in use or reserved"
            )));
        }
        let mut next = aliases.clone();
        next.set(&committee, alias.clone(), primary.clone())
            .map_err(|e| HubError::Rejected(e.to_string()))?;
        // The server must keep the owner before publishing a durable alias.
        registry::put_seat(self.store.as_ref(), primary, person)
            .await
            .map_err(log_error)?;
        registry::put_aliases(self.store.as_ref(), &next)
            .await
            .map_err(log_error)?;
        self.seats
            .act_as(person, primary)
            .map_err(|error| HubError::Rejected(error.to_string()))?;
        *aliases = next;
        Ok(qualified(&alias))
    }

    /// Remove `alias` while it stands for `primary`: on `committee`, or on
    /// every committee when none is named (an older bridge rolling back a
    /// failed launch). Conditional: a newer alias, or one now standing for
    /// another agent, is left alone, so a retry cannot remove it. Allowed
    /// to the principal that runs the agent and to the Speaker's office
    /// (`acted_by` naming who in it acts; a person in it may name itself).
    pub(crate) async fn remove_alias(
        &self,
        person: &Name,
        primary: &SenatorName,
        alias: &SenatorName,
        committee: Option<&CommitteeId>,
        acted_by: Option<&SenatorName>,
    ) -> Result<(), HubError> {
        let mut aliases = self.aliases.write().await;
        let found: Vec<QualifiedAlias> = aliases
            .matches(alias)
            .into_iter()
            .filter(|q| &q.primary == primary && committee.is_none_or(|c| *c == q.committee))
            .collect();
        if found.is_empty() {
            return Ok(());
        }
        if !self.may_remove_alias(person, primary, acted_by) {
            return Err(HubError::Rejected(format!(
                "{person} cannot remove an alias for {primary}"
            )));
        }
        let mut next = aliases.clone();
        for q in &found {
            next.remove(&q.committee, &q.alias);
        }
        registry::put_aliases(self.store.as_ref(), &next)
            .await
            .map_err(log_error)?;
        *aliases = next;
        Ok(())
    }

    /// Whether `person` may remove `primary`'s aliases: it runs the agent,
    /// or acts for the Speaker's office (`acted_by`, or itself).
    pub(crate) fn may_remove_alias(
        &self,
        person: &Name,
        primary: &SenatorName,
        acted_by: Option<&SenatorName>,
    ) -> bool {
        let roster = self.seats.snapshot();
        let office = |by: &SenatorName| roster.may_act_for_office(person, by);
        roster.may_act_as(person, primary).is_ok()
            || acted_by.is_some_and(office)
            || SenatorName::try_from(person.clone()).is_ok_and(|me| office(&me))
    }

    /// Carry the Senate-wide aliases of before over to the committees each
    /// agent sits in, once the writers have seated the agents their logs
    /// have seen. The per-committee table is written before the old map is
    /// dropped, so an interrupted migration runs again at the next start.
    /// An alias whose agent sits nowhere, is no agent of this Senate any
    /// more, or would take a name already used on a committee, is dropped
    /// and named in the log.
    pub(crate) async fn migrate_global_aliases(&self) -> Result<(), HubError> {
        let Some(global) = registry::global_aliases(self.store.as_ref())
            .await
            .map_err(log_error)?
        else {
            return Ok(());
        };
        let roster = self.seats.snapshot();
        let mut sits = BTreeMap::new();
        for primary in global.values() {
            let committees = if roster.is_agent(primary) {
                self.committees_of(primary).await?
            } else {
                Vec::new()
            };
            sits.insert(primary.clone(), committees);
        }
        let (moved, mut dropped) = CommitteeAliases::from_global(&global, |primary| {
            sits.get(primary).cloned().unwrap_or_default()
        });
        let mut aliases = self.aliases.write().await;
        let mut next = aliases.clone();
        for q in moved.iter() {
            if roster.knows(&q.alias)
                || next
                    .set(&q.committee, q.alias.clone(), q.primary.clone())
                    .is_err()
            {
                dropped.push((q.alias, q.primary));
            }
        }
        registry::put_aliases(self.store.as_ref(), &next)
            .await
            .map_err(log_error)?;
        registry::drop_global_aliases(self.store.as_ref())
            .await
            .map_err(log_error)?;
        let kept: Vec<String> = next
            .iter()
            .map(|q| format!("{q} -> {}", q.primary))
            .collect();
        let dropped: Vec<String> = dropped
            .iter()
            .map(|(alias, primary)| format!("{alias} -> {primary}"))
            .collect();
        if dropped.is_empty() {
            tracing::info!(?kept, "senator aliases moved to their committees");
        } else {
            tracing::warn!(
                ?kept,
                ?dropped,
                "senator aliases moved to their committees; some dropped"
            );
        }
        *aliases = next;
        Ok(())
    }

    /// Each seat's duty on `committee` now, from its log
    /// ([`senate_core::duty`]): the latest one set per senator.
    pub async fn duties(
        &self,
        committee: &CommitteeId,
    ) -> Result<BTreeMap<SenatorName, String>, HubError> {
        Ok(self.folded(&self.duties, committee).await?.fold.duties())
    }

    /// The committee's chair, and how it was found ([`senate_core::chair`];
    /// an agent's first event of its own counts as its seat, see
    /// [`ChairView`]).
    pub async fn chair(&self, committee: &CommitteeId) -> Result<ChairInfo, HubError> {
        let cached = self.folded(&self.chairs, committee).await?;
        let found = cached.fold.chair();
        Ok(ChairInfo {
            committee: committee.clone(),
            chair: found.map(|(s, _)| s.clone()),
            source: found.map(|(_, how)| how),
        })
    }

    /// Make `senator` the committee's chair. The writer records and addresses
    /// the handover in one note carrying [`meta::CHAIR`]. The caller has
    /// checked who may set the chair.
    pub async fn set_chair(
        &self,
        committee: &CommitteeId,
        senator: SenatorName,
    ) -> Result<ChairInfo, HubError> {
        self.set_chair_as(committee, senator, Address::Committee)
            .await
    }

    pub(crate) async fn set_chair_as(
        &self,
        committee: &CommitteeId,
        senator: SenatorName,
        by: Address,
    ) -> Result<ChairInfo, HubError> {
        let senator = self.primary_name(&senator, Some(committee)).await?;
        if !self.seats.snapshot().knows(&senator) {
            return Err(HubError::Rejected(format!(
                "{} is not a senator of this Senate",
                Address::senator(&senator)
            )));
        }
        self.ask_writer(committee, |reply| CommitteeMsg::SetChair {
            senator: senator.clone(),
            by: by.clone(),
            locale: self.locale,
            reply,
        })
        .await?
        .map_err(log_error)?;
        Ok(ChairInfo {
            committee: committee.clone(),
            chair: Some(senator),
            source: Some(ChairSource::Set),
        })
    }

    /// This Senate's name.
    pub fn senate_name(&self) -> &Name {
        &self.senate
    }

    /// The path of the Senate above, when known: configured, or learned
    /// since ([`ServerHub::set_parent`]).
    pub fn parent(&self) -> Option<SenatePath> {
        self.parent
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Learn the path of the Senate above, for a Senate that started with
    /// no parent because the one above did not answer then. Ignored when
    /// one is known already (a parent is changed by configuration and a
    /// restart, not by what answers). Returns whether it was learned now.
    pub fn set_parent(&self, parent: SenatePath) -> bool {
        let mut known = self.parent.write().unwrap_or_else(PoisonError::into_inner);
        if known.is_some() {
            return false;
        }
        *known = Some(parent);
        true
    }

    /// This Senate's path from the root: the parent's, then its own name.
    /// A Senate configured with no parent is a root.
    pub fn senate_path(&self) -> SenatePath {
        match self.parent() {
            Some(parent) => parent
                .clone()
                .absolute()
                .child(self.senate.clone())
                .unwrap_or_else(|_| parent.clone().absolute()),
            None => SenatePath::top(self.senate.clone())
                .unwrap_or_else(|_| unreachable!("a configured Senate name is checked")),
        }
    }

    /// Whether the qualified name `name` is in this Senate, written from
    /// `committee` (the writer's own, which a name without one is on).
    /// This Senate answers for itself alone; [`Resolved::Elsewhere`] tells
    /// the caller to ask the Senate above.
    ///
    /// - A Senate path names this Senate when it is this Senate's path or
    ///   ends with it (`north-desk` names `acme/north-desk`); any other is
    ///   elsewhere.
    /// - With no Senate path, a committee that is not here is elsewhere
    ///   (nearest first: the writer's Senate, then the one above). With a
    ///   Senate path naming this one, it must be here.
    /// - A senator named on a committee here must be a person of this
    ///   Senate or an agent seated in it.
    pub async fn resolve(
        &self,
        name: &Locator,
        committee: Option<&CommitteeId>,
    ) -> Result<Resolved, HubError> {
        let path = self.senate_path();
        let named_here = match name.senate() {
            None => false,
            Some(senate) if path.ends_with(senate) => true,
            Some(_) => return Ok(Resolved::Elsewhere),
        };
        let Some(target) = name.committee().or(committee).cloned() else {
            return Ok(Resolved::Unknown {
                reason: format!("{name} names no committee, and none is where it is written"),
            });
        };
        // A committee exists here once it has an event, or a seat: an agent
        // connected to it before anything was said.
        let exists = Some(&target) == committee
            || self.committees().await?.contains(&target)
            || !self.presence.agents_on(&target).is_empty();
        if !exists {
            return Ok(if named_here {
                Resolved::Unknown {
                    reason: format!("{path} has no committee {target}"),
                }
            } else {
                Resolved::Elsewhere
            });
        }
        // A committee written in the name is where its alias is; without
        // one, the alias resolves from the writer's committee.
        let mut address = name.address().clone();
        if let Address::Senator(senator) = &mut address {
            let written = AliasName {
                committee: name.committee().cloned(),
                alias: senator.clone(),
            };
            *senator = self
                .aliases
                .read()
                .await
                .resolve(&written, committee)
                .non_interactive(senator)
                .map_err(|e| HubError::Rejected(e.to_string()))?;
        }
        if let Some(senator) = address.as_senator() {
            let roster = self.seats.snapshot();
            if !roster.knows(senator) {
                let like = roster.named_like(senator.as_str(), 5);
                let reason = if like.is_empty() {
                    format!("{path}::{target} has no senator {senator}")
                } else {
                    let like = like
                        .iter()
                        .map(|s| format!("[senator::{s}]"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{path}::{target} has no senator {senator}; did you mean {like}?")
                };
                return Ok(Resolved::Unknown { reason });
            }
        }
        Ok(Resolved::Here {
            locator: Locator::at(path, target.clone(), address.clone()),
            committee: target,
            address,
        })
    }

    /// The roster as it is now, without the async [`Hub::roster`].
    pub fn roster_now(&self) -> Roster {
        self.seats.snapshot()
    }

    /// What awaits the Speaker's desk on `committee`
    /// ([`senate_core::docket`]): the messages addressed to `speaker` that
    /// the desk has not dealt with, then every root question to an agent of
    /// this Senate that is not connected and has no answer, with the facts
    /// about every seated agent (connected, presence, latest checkpoint,
    /// open threads).
    pub async fn docket(&self, committee: &CommitteeId) -> Result<DocketPage, HubError> {
        let mut items = self.folded(&self.dockets, committee).await?.fold.items();
        let chair = self.chair(committee).await?.chair;
        if chair
            .as_ref()
            .is_none_or(|chair| !self.presence.is_connected(committee, chair))
        {
            // Carry the same open approval to the desk if its chair goes
            // away between the original notice and the viewer's input.
            let people = self.seats.snapshot().people();
            items.extend(
                self.folded(&self.chair_dockets, committee)
                    .await?
                    .fold
                    .items()
                    .into_iter()
                    .filter(|item| item.event.is_hosted_approval(&people)),
            );
        }
        // A question put to a child Senate that is offline, with nothing
        // said of it yet, is the office's: answered for the child, or said
        // held. Whoever works the desk sees it, the person presiding too.
        let now = chrono::Utc::now();
        let children = self.children();
        items.extend(
            self.folded(&self.dockets, committee)
                .await?
                .fold
                .of_children()
                .filter(|(_, child)| {
                    children
                        .iter()
                        .any(|c| c.name == **child && c.is_offline(now))
                })
                .map(|(event, child)| DocketItem {
                    awaiting: Awaiting::ChildOffline {
                        child: child.clone(),
                    },
                    intended: senate_core::locator::intended(event),
                    event: event.clone(),
                }),
        );
        // Covering for an absent agent is the desk's duty toward other
        // agents: what is addressed to the seat's holder is that agent's
        // own question, delivered when its session opens.
        let holder = self.desk_seat().and_then(|seat| seat.senator().cloned());
        self.docket_page(committee, items, holder).await
    }

    /// What awaits the committee's chair: the roots addressed to `chair`
    /// (picks the server put to it among them) that the chair has not
    /// dealt with, then the unanswered questions to away members, as for
    /// the desk ([`ServerHub::docket`]).
    pub async fn chair_docket(&self, committee: &CommitteeId) -> Result<DocketPage, HubError> {
        let items = self
            .folded(&self.chair_dockets, committee)
            .await?
            .fold
            .items();
        let chair = self.chair(committee).await?.chair;
        self.docket_page(committee, items, chair).await
    }

    async fn docket_page(
        &self,
        committee: &CommitteeId,
        mut items: Vec<DocketItem>,
        holder: Option<SenatorName>,
    ) -> Result<DocketPage, HubError> {
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
        let duties = self.duties(committee).await?;
        let mut seats = Vec::new();
        for agent in &roster.agents() {
            if self.presence.is_unseated(committee, agent) {
                continue;
            }
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
                duty: duties.get(agent).cloned(),
            });
        }
        let mut covered = BTreeMap::new();
        for thread in board.threads(Some(StatusState::Processing)) {
            if thread.kind != Kind::Question || thread.answered {
                continue;
            }
            let agents: Vec<SenatorName> = thread
                .agents()
                .filter(|a| Some(*a) != holder.as_ref() && !self.presence.is_unseated(committee, a))
                .cloned()
                .collect();
            let away = !agents.is_empty()
                && agents
                    .iter()
                    .all(|a| roster.knows(a) && !self.presence.is_connected(committee, a));
            if !away {
                continue;
            }
            let Some(event) = self.get(committee, thread.root).await? else {
                continue;
            };
            if event.is_chair_discussion() {
                continue;
            }
            // An agent secretary of the away agent with a live session
            // covers it first: the chain reaches the chair and the desk
            // only when none is present.
            if let [only] = agents.as_slice()
                && let Some(secretary) = self.present_secretary(&roster, committee, only)
            {
                covered.insert(thread.root, secretary);
                continue;
            }
            items.push(DocketItem {
                event,
                awaiting: Awaiting::Away { agents },
                intended: None,
            });
        }
        items.sort_by_key(|item| item.event.seq);
        Ok(DocketPage {
            committee: committee.clone(),
            speaker: roster.speaker_seat(self.built_in.is_some()).ok().flatten(),
            office: roster.office(),
            items,
            covered,
            seats,
            head,
        })
    }

    /// The agent secretary that covers for `agent` now: the first of its
    /// secretaries, in order, that is an agent of this Senate with a live
    /// connection on `committee`. The built-in session ends the search
    /// (it covers by its own timers, after any agent listed before it has
    /// had its turn), as does anyone who is no agent.
    fn present_secretary(
        &self,
        roster: &Roster,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Option<SenatorName> {
        for secretary in roster.secretaries_of(agent) {
            if self.built_in.as_ref() == Some(secretary) || !roster.is_agent(secretary) {
                return None;
            }
            if self.presence.is_connected(committee, secretary) {
                return Some(secretary.clone());
            }
        }
        None
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
                        fold: T::fresh(committee, &self.seats.snapshot().people()),
                    }))
                })
                .clone()
        };
        let mut cached = cache.lock_owned().await;
        let epoch = self.store.epoch().await.map_err(log_error)?;
        if cached.epoch != epoch {
            *cached = Cached {
                epoch,
                fold: T::fresh(committee, &self.seats.snapshot().people()),
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
    /// [`Roster::may_act_as`] and [`Roster::seat`]. Nobody acts as the
    /// built-in session.
    ///
    /// Who runs the agent is kept in the store's registry, so a restarted
    /// server seats it again under the same principal.
    pub async fn act_as(&self, person: &Name, senator: &SenatorName) -> Result<bool, RosterError> {
        let aliases = self.aliases.read().await;
        if aliases.is_alias(senator) {
            return Err(RosterError::NameTaken {
                name: senator.name().clone(),
                taken: "an alias",
            });
        }
        let seated = self.seats.act_as(person, senator)?;
        if seated.new {
            tracing::info!(%person, agent = %senator, "agent seated on first contact");
        }
        if seated.owner_set
            && let Err(e) = registry::put_seat(self.store.as_ref(), senator, person).await
        {
            tracing::warn!(%person, agent = %senator, "who runs the agent is not kept: {e}");
        }
        Ok(seated.new)
    }

    /// An agent took a seat on `committee` (first contact there: a cursor;
    /// a stream and a heartbeat do it themselves, and a write is its own seat).
    /// The first agent's seat is written to the log, which makes it the
    /// chair. A person has no seat to take. A seat that cannot be recorded
    /// now is not the caller's failure: the agent's next contact records it.
    pub async fn take_seat(&self, committee: &CommitteeId, senator: &SenatorName) {
        if self.seats.snapshot().is_person(senator) {
            return;
        }
        if let Err(e) = self
            .ask_writer(committee, |reply| {
                CommitteeMsg::Contact(senator.clone(), reply)
            })
            .await
        {
            tracing::warn!(%committee, agent = %senator, "a seat was not recorded: {e}");
        }
    }

    /// Replace the people this Senate knows, for a layer whose people come
    /// from somewhere other than configuration. Seated agents whose person
    /// is still listed stay seated; the named Speaker is kept. Refused,
    /// changing nothing, when the new people break the roster's rules.
    pub async fn set_principals(&self, principals: Vec<Principal>) -> Result<(), HubError> {
        let _guard = self.secretary_gate.lock().await;
        let mut aliases = self.aliases.write().await;
        if let Some(p) = principals.iter().find(|p| aliases.is_alias(&p.path())) {
            return Err(HubError::Rejected(format!(
                "{} is already a senator alias",
                p.handle
            )));
        }
        let preview = Seats::new(self.seats.snapshot());
        preview
            .replace_principals(principals.clone())
            .map_err(|e| HubError::Rejected(e.to_string()))?;
        let next_roster = preview.snapshot();
        let mut next = aliases.clone();
        next.retain(|primary| next_roster.owner_of(primary).is_some());
        if next != *aliases {
            registry::put_aliases(self.store.as_ref(), &next)
                .await
                .map_err(log_error)?;
            *aliases = next;
        }
        self.seats
            .replace_principals(principals)
            .map_err(|e| HubError::Rejected(e.to_string()))
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
            .await??;
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

    /// Record activity in the committee writer's seat-change order.
    pub async fn record_heartbeat(
        &self,
        committee: &CommitteeId,
        heartbeat: Heartbeat,
    ) -> Result<(), HubError> {
        self.ask_writer(committee, |reply| {
            CommitteeMsg::Heartbeat(heartbeat.clone(), reply)
        })
        .await?
        .map_err(log_error)
    }

    /// Read the seat fence in the same order as explicit changes.
    pub(crate) async fn is_explicitly_unseated(
        &self,
        committee: &CommitteeId,
        senator: &SenatorName,
    ) -> Result<bool, HubError> {
        self.ask_writer(committee, |reply| {
            CommitteeMsg::IsUnseated(senator.clone(), reply)
        })
        .await
    }

    pub(crate) async fn set_committee_seat(
        &self,
        committee: &CommitteeId,
        change: CommitteeSeatChange,
        id: EventId,
    ) -> Result<CommitteeSeatInfo, HubError> {
        self.ask_writer(committee, |reply| CommitteeMsg::SetSeat {
            change: change.clone(),
            id: id.clone(),
            locale: self.locale,
            reply,
        })
        .await?
        .map_err(log_error)
    }

    /// Send `event` to its committee's writer.
    async fn commit(&self, event: NewEvent) -> Result<Appended, HubError> {
        let committee = event.committee.clone();
        self.ask_writer(&committee, |reply| {
            CommitteeMsg::Append(Box::new(event.clone()), reply)
        })
        .await?
        .map_err(log_error)
    }

    /// An answer in a thread the Speaker's desk forwarded from another
    /// committee of this Senate (its root is `from: speaker` with
    /// [`meta::ORIGIN`]) goes back into the asker's thread: an `answer` from
    /// `speaker` there, [`meta::ANSWERED_BY`] naming who answered and
    /// [`meta::ACTED_BY`] the desk worker that forwarded it (else the
    /// Speaker). Its id is derived from the answer's, so it is stored once.
    /// A provisional answer leaves the question open, and is not relayed.
    async fn relay_answer(&self, event: &Event) -> Result<(), HubError> {
        if event.kind != Kind::Answer
            || senate_core::is_provisional(event)
            || event.meta.contains_key(meta::ANSWERED_BY)
        {
            return Ok(());
        }
        let Some(root) = event.thread else {
            return Ok(());
        };
        let Some(root) = self.get(&event.committee, root).await? else {
            return Ok(());
        };
        if root.from != Address::Speaker {
            return Ok(());
        }
        let Some(origin) = Origin::of(&root) else {
            return Ok(());
        };
        let path = self.senate_path();
        if origin.locator.senate().is_some_and(|s| !path.ends_with(s)) {
            // Another Senate's: the machine's uplink relays across levels.
            return Ok(());
        }
        let Some(asked_on) = origin.locator.committee().cloned() else {
            return Ok(());
        };
        let acted_by = root
            .meta
            .get(meta::ACTED_BY)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .or_else(|| self.office().map(|o| o.speaker.name().to_string()));
        let answered_by = Locator::at(path, event.committee.clone(), event.from.clone());
        let mut relay = NewEvent::new(asked_on, Kind::Answer, Address::Speaker, &event.body)
            .with_id(EventId::derived(
                u64::try_from(event.ts.timestamp_millis()).unwrap_or_default(),
                &format!("relay:{}", event.id),
            ))
            .in_thread(origin.thread)
            .with_meta(
                meta::ANSWERED_BY,
                serde_json::Value::String(answered_by.to_string()),
            );
        relay.to = vec![origin.locator.address().clone()];
        relay.refs = event.refs.clone();
        if let Some(acted_by) = acted_by {
            relay = relay.with_meta(meta::ACTED_BY, serde_json::Value::String(acted_by));
        }
        let relayed = self.commit(relay).await?;
        tracing::info!(
            from = %event.committee,
            seq = %event.seq,
            to = %relayed.event.committee,
            thread = %origin.thread,
            "answer relayed into the thread it was asked in"
        );
        Ok(())
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
    /// `senator::*` is rejected (use [`Hub::resolve_and_append`]),
    /// and a sender that may not change state (a guest) cannot write a kind
    /// or a meta key that does.
    async fn append(&self, mut event: NewEvent) -> Result<Appended, HubError> {
        {
            // An alias stands for its agent, resolved from the committee the
            // event is written on; one several agents have, none of them
            // here, is refused with its candidates.
            let aliases = self.aliases.read().await;
            let mut changed = false;
            for recipient in &mut event.to {
                if let Address::Senator(name) = recipient {
                    let resolved = aliases.resolve_bare(name, Some(&event.committee));
                    if resolved != AliasMatch::NotAlias {
                        *name = resolved
                            .non_interactive(name)
                            .map_err(|e| HubError::Rejected(e.to_string()))?;
                        changed = true;
                    }
                }
            }
            if changed {
                let mut seen = std::collections::BTreeSet::new();
                event.to.retain(|address| seen.insert(address.clone()));
            }
        }
        if !event.is_resolved() {
            return Err(HubError::Rejected(
                "an address is still senator::*; resolve it before appending".into(),
            ));
        }
        check_may_change_state(&event).map_err(HubError::Rejected)?;
        let appended = self.commit(event).await?;
        if !appended.duplicate
            && let Err(e) = self.relay_answer(&appended.event).await
        {
            tracing::warn!(
                committee = %appended.event.committee,
                seq = %appended.event.seq,
                "relaying the answer back to where it was asked: {e}"
            );
        }
        Ok(appended)
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

    /// The agents with a seat on the committee: seated in this Senate, and
    /// with a presence there (in any state) or a stream or heartbeat on it
    /// now. `senator::*` reaches one of them.
    async fn committee_agents(
        &self,
        committee: &CommitteeId,
    ) -> Result<Vec<SenatorName>, HubError> {
        let mut on: BTreeSet<SenatorName> =
            self.presence.agents_on(committee).into_iter().collect();
        on.extend(
            self.presence(committee)
                .await?
                .agents
                .into_iter()
                .map(|p| p.agent),
        );
        Ok(self
            .seats
            .snapshot()
            .agents()
            .into_iter()
            .filter(|a| on.contains(a) && !self.presence.is_unseated(committee, a))
            .collect())
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

    /// The registered child Senates: the built-in Speaker reads them to
    /// relay to a child, so the trait's empty default must not stand here.
    async fn children(&self) -> Result<Vec<ChildSenate>, HubError> {
        Ok(ServerHub::children(self))
    }

    async fn aliases(&self) -> Result<CommitteeAliases, HubError> {
        Ok(ServerHub::aliases(self).await)
    }

    /// This Senate's path from the root: the built-in session names a seat
    /// in a child Senate in full with it, so the trait's `None` must not
    /// stand here either.
    fn senate_path(&self) -> Option<SenatePath> {
        Some(ServerHub::senate_path(self))
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

    /// The writer's leases, `lag` measured against its head now, with each
    /// seat's duty from the log.
    async fn presence(&self, committee: &CommitteeId) -> Result<PresencePage, HubError> {
        let (head, mut agents): (Seq, Vec<AgentPresence>) =
            self.ask_writer(committee, CommitteeMsg::Presence).await?;
        let duties = self.duties(committee).await?;
        for p in &mut agents {
            p.duty = duties.get(&p.agent).cloned();
        }
        Ok(PresencePage {
            agents,
            head,
            duties,
        })
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
