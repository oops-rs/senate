//! The machine under a parent: its Senate registered with the Senate above
//! as a child, and what crosses between the two logs.
//!
//! The two logs stay apart: nothing of either is copied wholesale into the
//! other. Only what one addresses to the other crosses, each time as a new
//! event written where it lands (with an id derived from the one it came
//! from, so crossing again stores nothing new):
//!
//! - **Registration.** `PUT <parent>/v1/children/<name>` with the
//!   machine's Speaker (`/<parent>::senator::<person>`), authenticated with
//!   the person's token there; on every (re)connect: at start, and after
//!   the Senate above stopped answering. A refusal is logged once and kept
//!   for `senate status` ([`UplinkStatus`], in the hosted Senate's
//!   directory).
//! - **Down: the parent's items for the desk.** An event above addressed
//!   to `<name>::<committee>::speaker` becomes, on that committee here, a
//!   message from `committee` to `speaker` (the desk) of the same kind and
//!   body, with `meta.origin` (the sender above, as a complete qualified
//!   name, and the thread there) and `meta.intended` (the seat it was
//!   meant for, as the parent completed it, else this desk). The first
//!   such item of a thread above is a new root here; later ones are
//!   replies in that root's thread. When the seat it is meant for (named
//!   by `meta.intended`, or the one its thread here was brought to) sits
//!   on that committee here and is online, the item goes to that seat
//!   directly (`to: senator::<seat>`), so its answer goes up as the
//!   seat's; an offline or unknown seat, or none named, leaves it to the
//!   desk.
//! - **Up: the desk's answers.** An `answer` here in a thread whose root
//!   came from above is posted above, in the origin thread, from
//!   `<name>::<committee>::speaker`, `meta.answered_by` naming who answered
//!   here (and `meta.acted_by` when the answer carries it). A provisional
//!   answer stays here.
//! - **Up and back down: forwards above.** The desk's forward of an item
//!   meant for a seat above is posted there by the bridge (from
//!   `<name>::<committee>::speaker`, `meta.origin` naming the asker here);
//!   an answer above in that root's thread is relayed down into the
//!   asker's thread here, from `speaker`, `meta.answered_by` naming who
//!   answered above.
//!
//! # Polling
//!
//! Every [`POLL`] (2 s), or the bridge's sweep period when that is shorter
//! (tests), the uplink reads what is new: above, every committee `/v1/me`
//! names (`GET .../events?after=`); here, every committee of the log. The
//! cursors are kept in the hosted Senate's store registry, so a restart
//! goes on where it stopped:
//!
//! | Key | Value |
//! | --- | --- |
//! | `uplink:cursor:<committee>` | the last seq read above |
//! | `uplink:child-cursor:<committee>` | the last seq read here |
//! | `uplink:down:<committee>:<thread>` | the root here a thread above was brought to (`{committee, seq}`) |
//! | `uplink:up:<committee>:<seq>` | a root above the desk forwarded, and where it was asked here ([`Origin`]) |
//! | `uplink:epoch` | the epoch of the log above that the `cursor`, `down` and `up` entries belong to |
//!
//! # A log above that was reset
//!
//! A seq above means something within one epoch of that log only
//! ([`senate_core::api`]), and the cursor, `down` and `up` entries are all
//! seqs above. So the uplink follows the epoch the way the bridge daemon
//! does for its own cursors: every answer of the Senate above names its
//! epoch, and one that is not the recorded one means the log there was
//! reset. Everything kept about the old log is dropped, the new epoch is
//! recorded, and the uplink registers again (the registration went with
//! the log) and reads from the start. The first epoch seen is recorded as
//! it is, unless entries are already on file with no epoch beside them
//! (kept by a build from before this): nothing says which log those are
//! about, so that counts as a reset too. Reading a log again brings
//! nothing down twice, the ids being derived.
//!
//! A cursor beyond a committee's head (409 `cursor_ahead`) is the same
//! fact found from the other side, by a Senate above that names no epoch:
//! what is kept about that committee is dropped and it is read from the
//! start.

use std::sync::Arc;
use std::time::Duration;

use senate_bridge::client::{ApiClient, ApiClientError, Decoded, decode_event};
use senate_bridge::ipc::{Registration, UplinkStatus};
use senate_core::{
    Address, ChildSeat, ChildSeats, CommitteeId, Event, EventId, Hub, Kind, Locale, Locator, Name,
    NewEvent, Origin, PresenceState, SenatePath, SenatorId, SenatorName, Seq, is_provisional, meta,
};
use senate_log::LogStore;
use senate_server::ServerHub;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub use senate_core::api::{MAX_BACKOFF, UPLINK_POLL as POLL};

/// Bound on one request to the Senate above.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Events read per page.
const PAGE: usize = 200;

/// Registry key prefixes.
const CURSOR: &str = "uplink:cursor:";
const CHILD_CURSOR: &str = "uplink:child-cursor:";
const DOWN: &str = "uplink:down:";
const UP: &str = "uplink:up:";
/// The epoch of the log above that the [`CURSOR`], [`DOWN`] and [`UP`]
/// entries belong to.
const EPOCH: &str = "uplink:epoch";

/// What the uplink needs to run.
pub struct UplinkConfig {
    /// The Senate above: its address and the person's token there.
    pub url: String,
    pub token: String,
    /// The machine's Senate's name, which it registers under.
    pub name: Name,
    /// The Speaker's name as a senator of the Senate above (the person,
    /// unless `[local] speaker` names another principal there).
    pub speaker: senate_core::SenatorName,
    /// The parent's path, when it answered at start (the hosted Senate's
    /// `parent`). `None`: learned at the first answer, which registers and
    /// tells the hosted Senate ([`ServerHub::set_parent`]).
    pub parent: Option<SenatePath>,
    /// Where [`UplinkStatus`] is written.
    pub dir: std::path::PathBuf,
    pub poll: Duration,
    pub locale: Locale,
}

/// The uplink, running.
pub struct Uplink {
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Uplink {
    /// Start the uplink of the hosted Senate `hub`.
    pub fn spawn(
        hub: Arc<ServerHub>,
        store: Arc<dyn LogStore>,
        config: UplinkConfig,
    ) -> Result<Self, ApiClientError> {
        let above = ApiClient::new(&config.url, &config.token)?;
        let stop = CancellationToken::new();
        let mut worker = Worker {
            hub,
            store,
            above,
            config,
            registered: false,
            shared: Vec::new(),
            last: None,
            backoff: Duration::ZERO,
        };
        let token = stop.clone();
        let task = tokio::spawn(async move { worker.run(token).await });
        Ok(Self { stop, task })
    }

    /// Stop it and wait.
    pub async fn shutdown(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

/// The wait after one more failed try to register: the poll period after
/// the first, then doubling, never above [`MAX_BACKOFF`] (or the poll
/// period, when that is longer).
pub fn next_backoff(previous: Duration, poll: Duration) -> Duration {
    (previous * 2).max(poll).min(MAX_BACKOFF.max(poll))
}

/// Where a thread above was brought to here.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Brought {
    committee: CommitteeId,
    seq: Seq,
}

struct Worker {
    hub: Arc<ServerHub>,
    /// The hosted Senate's store: its registry keeps the cursors.
    store: Arc<dyn LogStore>,
    above: ApiClient,
    config: UplinkConfig,
    registered: bool,
    /// The committees the Senate above shares with this one, as its last
    /// `/v1/me` listed them: the ones whose seats are declared up.
    shared: Vec<CommitteeId>,
    /// The registration last written, to log a change once.
    last: Option<Registration>,
    /// How long to wait before the next try to register: the poll period
    /// after a first failure, doubled by each next one up to
    /// [`MAX_BACKOFF`]; zero once registered.
    backoff: Duration,
}

/// Why a pass stopped.
enum Pass {
    /// The Senate above did not answer: register again when it does.
    Lost(String),
    /// Anything else: logged, tried again next time.
    Failed(String),
}

impl Worker {
    async fn run(&mut self, stop: CancellationToken) {
        loop {
            let mut wait = self.config.poll;
            if !self.registered {
                self.register().await;
                if self.registered {
                    self.backoff = Duration::ZERO;
                } else {
                    // The Senate above is not there (or will not have us):
                    // ask less and less often, this Senate runs meanwhile.
                    self.backoff = next_backoff(self.backoff, self.config.poll);
                    wait = self.backoff;
                }
            }
            if self.registered {
                match self.pass().await {
                    Ok(()) => {}
                    Err(Pass::Lost(e)) => {
                        tracing::info!("the Senate above stopped answering: {e}");
                        self.registered = false;
                        self.note(Registration::Unreachable { error: e });
                    }
                    Err(Pass::Failed(e)) => tracing::warn!("uplink: {e}"),
                }
            }
            tokio::select! {
                () = stop.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// The parent's path: as it was at start, or as it answers now.
    fn parent(&self) -> Option<&SenatePath> {
        self.config.parent.as_ref()
    }

    /// The Speaker, as the Senate above knows it.
    fn speaker(&self) -> SenatorId {
        let name = self.config.speaker.clone();
        match self.parent() {
            Some(parent) => SenatorId::of(parent.clone(), name),
            None => SenatorId::here(name),
        }
    }

    /// Record where the registration stands, for `senate status`; a change
    /// is logged once.
    fn note(&mut self, registration: Registration) {
        let status = UplinkStatus::now(
            &self.config.url,
            self.parent().cloned(),
            self.config.name.clone(),
            self.speaker(),
            registration.clone(),
        );
        status.write(&self.config.dir);
        if self.last.as_ref() != Some(&registration) {
            match &registration {
                Registration::Refused { reason } => tracing::warn!(
                    senate = %self.config.name,
                    "the Senate above refused this child Senate: {reason}"
                ),
                Registration::Registered => tracing::info!(
                    senate = %self.config.name,
                    above = %self.config.url,
                    "registered as a child Senate"
                ),
                _ => {}
            }
            self.last = Some(registration);
        }
    }

    /// Ask the Senate above who it is, and register there.
    async fn register(&mut self) {
        let me = match tokio::time::timeout(TIMEOUT, self.above.me()).await {
            Ok(Ok(me)) => me,
            Ok(Err(e)) if e.status().is_some() => {
                self.note(Registration::Refused {
                    reason: e.to_string(),
                });
                return;
            }
            Ok(Err(e)) => {
                self.note(Registration::Unreachable {
                    error: e.to_string(),
                });
                return;
            }
            Err(_) => {
                self.note(Registration::Unreachable {
                    error: "timed out".into(),
                });
                return;
            }
        };
        if self.config.parent.is_none() {
            let parent = me.senate_path();
            self.config.parent = Some(parent.clone());
            // This Senate started without it: what it reports (`Me`, and
            // so `senate status`) names its parent from now on.
            if self.hub.set_parent(parent.clone()) {
                tracing::info!(
                    senate = %self.hub.senate_path(),
                    parent = %parent,
                    "the Senate above answered; this Senate now knows its parent"
                );
            }
        }
        let speaker = self.speaker();
        match tokio::time::timeout(
            TIMEOUT,
            self.above.register_child(&self.config.name, &speaker),
        )
        .await
        {
            Ok(Ok(_)) => {
                self.registered = true;
                self.note(Registration::Registered);
            }
            Ok(Err(ApiClientError::Api { message, .. })) => {
                self.note(Registration::Refused { reason: message });
            }
            Ok(Err(e)) => self.note(Registration::Unreachable {
                error: e.to_string(),
            }),
            Err(_) => self.note(Registration::Unreachable {
                error: "timed out".into(),
            }),
        }
    }

    fn store(&self) -> &dyn LogStore {
        self.store.as_ref()
    }

    async fn get<T: for<'de> Deserialize<'de>>(&self, key: &str) -> Option<T> {
        let found = self.store().registry(key).await.ok()?;
        found
            .into_iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| serde_json::from_value(v).ok())
    }

    async fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<(), Pass> {
        let value = serde_json::to_value(value).map_err(|e| Pass::Failed(e.to_string()))?;
        self.store()
            .put_registry(key, Some(value))
            .await
            .map_err(|e| Pass::Failed(format!("keeping {key}: {e}")))
    }

    /// Remove every registry entry whose key starts with `prefix`; how
    /// many there were.
    async fn forget(&self, prefix: &str) -> Result<usize, Pass> {
        let found = self
            .store()
            .registry(prefix)
            .await
            .map_err(|e| Pass::Failed(format!("reading {prefix}*: {e}")))?;
        for (key, _) in &found {
            self.store()
                .put_registry(key, None)
                .await
                .map_err(|e| Pass::Failed(format!("dropping {key}: {e}")))?;
        }
        Ok(found.len())
    }

    /// Compare an epoch the Senate above named with the recorded one, and
    /// when its log is another one now, drop what is kept about the old
    /// one. True when it was: this pass is over, and the next registers
    /// again and reads from the start.
    ///
    /// The first epoch seen is recorded, unless entries about the log
    /// above are on file with no epoch (then it counts as a change).
    /// `None` (a Senate from before epochs) tells nothing.
    async fn follow(&mut self, observed: Option<&str>) -> Result<bool, Pass> {
        let Some(observed) = observed.filter(|e| !e.is_empty()) else {
            return Ok(false);
        };
        let known = self
            .store()
            .registry_value(EPOCH)
            .await
            .map_err(|e| Pass::Failed(format!("reading {EPOCH}: {e}")))?
            .and_then(|v| v.as_str().map(String::from));
        if known.as_deref() == Some(observed) {
            return Ok(false);
        }
        // The entries go first and the epoch is recorded last: stopped in
        // between, the next pass finds entries with no epoch and finishes.
        let mut dropped = 0;
        for prefix in [CURSOR, DOWN, UP] {
            dropped += self.forget(prefix).await?;
        }
        self.put(EPOCH, &observed).await?;
        if known.is_none() && dropped == 0 {
            tracing::info!(
                epoch = observed,
                "recorded the log epoch of the Senate above"
            );
            return Ok(false);
        }
        tracing::warn!(
            old = known.as_deref().unwrap_or("none recorded"),
            new = observed,
            dropped,
            "the log of the Senate above was reset (its epoch changed): dropped what was kept \
             about the old one; registering again and reading from the start"
        );
        // The registration there went with the log.
        self.registered = false;
        Ok(true)
    }

    /// The Senate above says the cursor on `committee` is beyond its log:
    /// drop what is kept about that committee.
    async fn forget_committee(&self, committee: &CommitteeId) -> Result<(), Pass> {
        let mut dropped = self.forget(&format!("{DOWN}{committee}:")).await?;
        dropped += self.forget(&format!("{UP}{committee}:")).await?;
        let key = format!("{CURSOR}{committee}");
        self.store()
            .put_registry(&key, None)
            .await
            .map_err(|e| Pass::Failed(format!("dropping {key}: {e}")))?;
        tracing::warn!(
            %committee,
            dropped,
            "the Senate above says the uplink's cursor is beyond the committee's log \
             (cursor_ahead), so the log was reset: dropped the committee's cursor and what was \
             kept about its threads; reading it from the start"
        );
        Ok(())
    }

    /// One pass: down, then up. Nothing goes up to a Senate this one has
    /// to register with again first.
    async fn pass(&mut self) -> Result<(), Pass> {
        self.down().await?;
        if !self.registered {
            return Ok(());
        }
        self.declare().await?;
        self.up().await
    }

    /// Declare this Senate's seats above: every senator on each committee
    /// the Senate above shares with it, with its duty and presence here.
    /// The whole set every pass; it also tells the Senate above this one
    /// is online. Only seats cross, never content.
    async fn declare(&self) -> Result<(), Pass> {
        let here = self
            .hub
            .committees()
            .await
            .map_err(|e| Pass::Failed(format!("listing the committees here: {e}")))?;
        let aliases = Hub::aliases(self.hub.as_ref())
            .await
            .map_err(|e| Pass::Failed(format!("reading the aliases here: {e}")))?;
        let mut seats = Vec::new();
        for committee in self.shared.iter().filter(|c| here.contains(c)) {
            let page =
                self.hub.presence(committee).await.map_err(|e| {
                    Pass::Failed(format!("reading the presence on {committee}: {e}"))
                })?;
            let duties = page.duties;
            seats.extend(page.agents.into_iter().map(|p| ChildSeat {
                committee: committee.clone(),
                duty: p.duty.or_else(|| duties.get(&p.agent).cloned()),
                alias: aliases.alias_of(committee, &p.agent).cloned(),
                senator: p.agent,
                presence: p.state,
                paused: p.paused,
            }));
        }
        tokio::time::timeout(
            TIMEOUT,
            self.above
                .put_child_seats(&self.config.name, &ChildSeats { seats }),
        )
        .await
        .map_err(|_| Pass::Lost("timed out".into()))?
        .map_err(|e| lost_or_failed(&e))?;
        Ok(())
    }

    /// This Senate's path.
    fn here(&self) -> SenatePath {
        self.hub.senate_path()
    }

    /// What is new above.
    async fn down(&mut self) -> Result<(), Pass> {
        let me = tokio::time::timeout(TIMEOUT, self.above.me())
            .await
            .map_err(|_| Pass::Lost("timed out".into()))?
            .map_err(|e| lost_or_failed(&e))?;
        if self.follow(Some(&me.epoch)).await? {
            return Ok(());
        }
        let Some(parent) = self.parent().cloned() else {
            return Ok(());
        };
        self.shared.clone_from(&me.committees);
        for committee in me.committees {
            let key = format!("{CURSOR}{committee}");
            let mut cursor: Seq = self.get(&key).await.unwrap_or(Seq::ZERO);
            loop {
                let page = match tokio::time::timeout(
                    TIMEOUT,
                    self.above.events(&committee, cursor, Some(PAGE)),
                )
                .await
                {
                    Err(_) => return Err(Pass::Lost("timed out".into())),
                    Ok(Ok(page)) => page,
                    Ok(Err(e)) => {
                        if self.follow(e.epoch()).await? {
                            return Ok(());
                        }
                        if e.is_cursor_ahead() && cursor > Seq::ZERO {
                            self.forget_committee(&committee).await?;
                            cursor = Seq::ZERO;
                            continue;
                        }
                        return Err(lost_or_failed(&e));
                    }
                };
                if self.follow(page.epoch.as_deref()).await? {
                    return Ok(());
                }
                for raw in page.events {
                    let seq = match decode_event(raw) {
                        Decoded::Event(event) => {
                            self.on_above(&parent, &event).await?;
                            event.seq
                        }
                        Decoded::Undecodable { seq, error } => {
                            tracing::debug!(%committee, "an event above this build cannot read: {error}");
                            seq.unwrap_or(cursor)
                        }
                    };
                    cursor = cursor.max(seq);
                }
                self.put(&key, &cursor).await?;
                if !page.has_more {
                    break;
                }
            }
        }
        Ok(())
    }

    /// One event above.
    async fn on_above(&self, parent: &SenatePath, event: &Event) -> Result<(), Pass> {
        let name = &self.config.name;
        let ours = matches!(&event.from, Address::Child { senate, .. } if senate == name);
        if ours {
            // A root the desk forwarded up: remember where it was asked.
            if let Some(origin) = Origin::of(event)
                && event.thread.is_none()
                && origin
                    .locator
                    .senate()
                    .is_some_and(|s| self.here().ends_with(s))
            {
                let key = format!("{UP}{}:{}", event.committee, event.seq);
                self.put(&key, &origin).await?;
            }
            return Ok(());
        }
        // An answer in a root the desk forwarded up goes to the asker's
        // thread, whoever it is addressed to above.
        if event.kind == Kind::Answer
            && let Some(root) = event.thread
        {
            let key = format!("{UP}{}:{root}", event.committee);
            if let Some(origin) = self.get::<Origin>(&key).await {
                if is_provisional(event) {
                    return Ok(());
                }
                return self.relay_down(parent, event, &origin).await;
            }
        }
        let for_us = event.to.iter().find_map(|a| match a {
            Address::Child { senate, committee } if senate == name => Some(committee.clone()),
            _ => None,
        });
        if let Some(committee) = for_us {
            return self.bring_down(parent, event, committee).await;
        }
        Ok(())
    }

    /// An item above for this Senate's desk: a message on `committee` here
    /// from `committee` to `speaker`.
    async fn bring_down(
        &self,
        parent: &SenatePath,
        event: &Event,
        committee: CommitteeId,
    ) -> Result<(), Pass> {
        let thread = event.thread.unwrap_or(event.seq);
        let origin = Origin {
            locator: Locator::at(
                parent.clone().absolute(),
                event.committee.clone(),
                event.from.clone(),
            ),
            thread,
        };
        let intended = senate_core::locator::intended(event)
            .unwrap_or_else(|| Locator::at(self.here(), committee.clone(), Address::Speaker));
        let down_key = format!("{DOWN}{}:{thread}", event.committee);
        let kept: Option<Brought> = self.get(&down_key).await;
        let brought = kept.as_ref().filter(|b| b.committee == committee);
        // The seat it is for: the one named, else the one its thread here
        // was brought to.
        let seat = match self.seat_of(&intended, &committee) {
            Some(seat) => Some(seat),
            None => match brought {
                Some(b) => self.seat_of_root(&committee, b.seq).await?,
                None => None,
            },
        };
        let to = match seat {
            Some(seat) if self.online_here(&committee, &seat).await? => Address::Senator(seat),
            _ => Address::Speaker,
        };
        let mut here = NewEvent::new(
            committee.clone(),
            event.kind.clone(),
            Address::Committee,
            &event.body,
        )
        .to(to)
        .with_id(derived(event, "uplink:down"))
        .with_meta(meta::ORIGIN, origin.value())
        .with_meta(
            meta::INTENDED,
            serde_json::Value::String(intended.to_string()),
        );
        here.refs = event.refs.clone();
        if let Some(b) = brought {
            here = here.in_thread(b.seq);
        }
        let stored = self
            .hub
            .append(here)
            .await
            .map_err(|e| Pass::Failed(format!("bringing {} down: {e}", event.id)))?;
        if kept.is_none() {
            // Read again after a reset, it is stored already, and where it
            // was put then is where its thread stays.
            let b = Brought {
                committee,
                seq: stored.event.thread.unwrap_or(stored.event.seq),
            };
            self.put(&down_key, &b).await?;
        }
        Ok(())
    }

    /// The seat of this Senate on `committee` that `intended` names, if it
    /// names one.
    fn seat_of(&self, intended: &Locator, committee: &CommitteeId) -> Option<SenatorName> {
        let here = intended.senate().is_some_and(|s| self.here().ends_with(s));
        match intended.address() {
            Address::Senator(seat) if here && intended.committee() == Some(committee) => {
                Some(seat.clone())
            }
            _ => None,
        }
    }

    /// The seat the root `seq` on `committee` here was brought to, when it
    /// was brought to one rather than to the desk.
    async fn seat_of_root(
        &self,
        committee: &CommitteeId,
        seq: Seq,
    ) -> Result<Option<SenatorName>, Pass> {
        let root = self
            .hub
            .get(committee, seq)
            .await
            .map_err(|e| Pass::Failed(format!("reading the root {seq} here: {e}")))?;
        Ok(root.and_then(|r| match r.to.as_slice() {
            [Address::Senator(seat)] => Some(seat.clone()),
            _ => None,
        }))
    }

    /// Whether `seat` is connected on `committee` here now.
    async fn online_here(&self, committee: &CommitteeId, seat: &SenatorName) -> Result<bool, Pass> {
        let page = self
            .hub
            .presence(committee)
            .await
            .map_err(|e| Pass::Failed(format!("reading the presence on {committee}: {e}")))?;
        Ok(page
            .agents
            .iter()
            .any(|p| &p.agent == seat && p.state == PresenceState::Online))
    }

    /// An answer above in a root the desk forwarded up: into the asker's
    /// thread here.
    async fn relay_down(
        &self,
        parent: &SenatePath,
        event: &Event,
        origin: &Origin,
    ) -> Result<(), Pass> {
        let Some(committee) = origin.locator.committee().cloned() else {
            return Ok(());
        };
        let answered_by = Locator::at(
            parent.clone().absolute(),
            event.committee.clone(),
            event.from.clone(),
        );
        let mut here = NewEvent::new(committee, Kind::Answer, Address::Speaker, &event.body)
            .to(origin.locator.address().clone())
            .in_thread(origin.thread)
            .with_id(derived(event, "uplink:relay-down"))
            .with_meta(
                meta::ANSWERED_BY,
                serde_json::Value::String(answered_by.to_string()),
            );
        here.refs = event.refs.clone();
        self.hub
            .append(here)
            .await
            .map_err(|e| Pass::Failed(format!("relaying {} down: {e}", event.id)))?;
        Ok(())
    }

    /// What is new here.
    async fn up(&mut self) -> Result<(), Pass> {
        let committees = self
            .hub
            .committees()
            .await
            .map_err(|e| Pass::Failed(e.to_string()))?;
        for committee in committees {
            let key = format!("{CHILD_CURSOR}{committee}");
            let mut cursor: Seq = self.get(&key).await.unwrap_or(Seq::ZERO);
            loop {
                let events = self
                    .hub
                    .read(&committee, cursor, PAGE)
                    .await
                    .map_err(|e| Pass::Failed(e.to_string()))?;
                let full = events.len() == PAGE;
                for event in events {
                    self.on_here(&event).await?;
                    cursor = cursor.max(event.seq);
                    self.put(&key, &cursor).await?;
                }
                if !full {
                    break;
                }
            }
        }
        Ok(())
    }

    /// One event here: an answer in a thread that came from above goes up.
    async fn on_here(&self, event: &Event) -> Result<(), Pass> {
        let Some(parent) = self.parent() else {
            return Ok(());
        };
        let approval = event
            .meta
            .get(meta::APPROVAL_WAIT)
            .filter(|_| event.is_hosted_approval(&self.hub.roster_now().people()));
        let (root_seq, kind, body) = if let Some(wait) = approval {
            let (Some(root), Some(label)) = (wait["thread"].as_u64(), wait["label"].as_str())
            else {
                return Ok(());
            };
            let prefix = self
                .config
                .locale
                .pick("等待授权：", "Waiting for approval: ");
            (Seq(root), Kind::Note, format!("{prefix}{label}"))
        } else if event.kind == Kind::Answer && !is_provisional(event) {
            let Some(root) = event.thread else {
                return Ok(());
            };
            (root, Kind::Answer, event.body.clone())
        } else {
            return Ok(());
        };
        // What came down as an answer is not sent back up.
        let from_above = |key: &str| {
            event
                .meta
                .get(key)
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<Locator>().ok())
                .and_then(|l| l.senate().cloned())
                .is_some_and(|s| !self.here().ends_with(&s))
        };
        if from_above(meta::ANSWERED_BY) {
            return Ok(());
        }
        // Nor is anything the uplink itself brought down (it carries where
        // it started above): an answer given above in this Senate's name
        // while it was offline lands in the desk's thread as an answer,
        // and is not this Senate's to say again.
        let brought = Origin::of(event)
            .and_then(|origin| origin.locator.senate().cloned())
            .is_some_and(|senate| !self.here().ends_with(&senate));
        if brought {
            return Ok(());
        }
        let Some(mut root) = self
            .hub
            .get(&event.committee, root_seq)
            .await
            .map_err(|e| Pass::Failed(e.to_string()))?
        else {
            return Ok(());
        };
        if approval.is_some() {
            // Accept a link only from the seat that received the task, or its desk worker.
            let at_desk = root.to.contains(&Address::Speaker)
                && self.hub.office().is_some_and(|o| {
                    o.agent_workers()
                        .any(|a| Some(a) == event.from.as_senator())
                });
            if root.thread.is_some() || (!root.to.contains(&event.from) && !at_desk) {
                return Ok(());
            }
            // A desk-forwarded task names the original incoming thread locally.
            if root.from == Address::Speaker
                && let Some(origin) = Origin::of(&root)
                && origin.locator.senate() == Some(&self.here())
                && let Some(committee) = origin.locator.committee()
            {
                let Some(incoming) = self
                    .hub
                    .get(committee, origin.thread)
                    .await
                    .map_err(|e| Pass::Failed(e.to_string()))?
                else {
                    return Ok(());
                };
                root = incoming;
            }
        }
        let Some(origin) = Origin::of(&root) else {
            return Ok(());
        };
        let above = origin
            .locator
            .senate()
            .is_some_and(|s| parent.clone().absolute().ends_with(s) && !self.here().ends_with(s));
        if !above || root.from != Address::Committee {
            return Ok(());
        }
        let Some(asked_on) = origin.locator.committee().cloned() else {
            return Ok(());
        };
        let answered_by = local_author(
            &self.here(),
            &event.committee,
            &event.from,
            event.meta.get(meta::ANSWERED_BY),
        );
        let mut up = NewEvent::new(
            asked_on,
            kind,
            Address::child(self.config.name.clone(), event.committee.clone()),
            body,
        )
        .to(origin.locator.address().clone())
        .in_thread(origin.thread)
        .with_id(derived(event, "uplink:up"))
        .with_meta(
            meta::ANSWERED_BY,
            serde_json::Value::String(answered_by.to_string()),
        );
        if approval.is_some() {
            up = up.with_meta(meta::APPROVAL_WAIT, serde_json::Value::Bool(true));
        }
        if let Some(acted_by) = event.meta.get(meta::ACTED_BY) {
            up = up.with_meta(meta::ACTED_BY, acted_by.clone());
        }
        up.refs = event.refs.clone();
        match tokio::time::timeout(TIMEOUT, self.above.append(&up)).await {
            Err(_) => Err(Pass::Lost("timed out".into())),
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => match lost_or_failed(&e) {
                lost @ Pass::Lost(_) => Err(lost),
                Pass::Failed(why) => {
                    // Refused above: it will be refused again. Said, and
                    // passed over.
                    tracing::warn!(id = %event.id, "the Senate above refused the thread update: {why}");
                    Ok(())
                }
            },
        }
    }
}

/// A desk-forwarded answer already carries the seat that wrote it.
fn local_author(
    here: &SenatePath,
    committee: &CommitteeId,
    sender: &Address,
    by: Option<&serde_json::Value>,
) -> Locator {
    if sender == &Address::Speaker
        && let Some(seat) = by
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Locator>().ok())
        && seat.senate() == Some(here)
        && seat.address().as_senator().is_some()
    {
        return seat;
    }
    Locator::at(here.clone(), committee.clone(), sender.clone())
}

/// A connection failure (register again), or a refusal.
fn lost_or_failed(e: &ApiClientError) -> Pass {
    match e {
        ApiClientError::Http(_) => Pass::Lost(e.to_string()),
        ApiClientError::Api { status: 401, .. } => Pass::Lost(e.to_string()),
        _ => Pass::Failed(e.to_string()),
    }
}

/// The id of what `event` becomes on the other side: derived, so crossing
/// it again stores nothing new.
fn derived(event: &Event, what: &str) -> EventId {
    EventId::derived(
        u64::try_from(event.ts.timestamp_millis()).unwrap_or_default(),
        &format!("{what}:{}", event.id),
    )
}

#[cfg(test)]
mod attribution_tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn desk_forwarded_answer_keeps_the_original_local_seat() {
        let here = "/senate/child".parse().unwrap();
        let committee = CommitteeId::new("desk").unwrap();
        let by = serde_json::json!("/senate/child::work::senator::agy");
        assert_eq!(
            local_author(&here, &committee, &Address::Speaker, Some(&by)).to_string(),
            by.as_str().unwrap()
        );
    }
}
