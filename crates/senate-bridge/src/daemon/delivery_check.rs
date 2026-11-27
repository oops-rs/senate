//! The delivery check: whether what Senate handed a session reached its
//! agent, and what Senate does when it did not.
//!
//! The queue's `delivered` only says a runtime took a message
//! ([`crate::delivery`]). One mechanism, the same for every runtime, follows
//! it from there ([`crate::state::Reach`]):
//!
//! - **queued → injected**: the runtime took it. Claude Code's inbox socket
//!   accepted the write, Codex's app-server started or steered a turn, an
//!   ACP client acknowledged its prompt, or the message went out in the
//!   reply to a Claude Code or agy hook.
//! - **injected → seen**: the message's `[senate · committee · seq N` header
//!   is in the session's own record of its conversation as a user turn,
//!   read with the timeline reader the app already uses
//!   ([`crate::timeline`]): Claude Code's JSONL transcript (the `user`
//!   record written once Claude takes the message off its queue, not the
//!   `enqueue` before it), Codex's rollout file, agy's conversation store.
//!   `catch_up` counts too: the agent read it itself. A session with no
//!   record Senate can read (ACP, one that never reported a transcript)
//!   stays `injected`: Senate neither wakes it nor reports it on no
//!   evidence.
//! - **seen → acted**: the agent wrote in that thread afterwards
//!   ([`Shared::note_acted`]). A note that needs no reply ends at seen.
//!
//! # Waking
//!
//! When something waits for a hosted session unseen and the session sits
//! idle, its host types Senate's own fixed line and Enter into its terminal
//! ([`crate::host::control::wake`]). The runtime's prompt hook then takes
//! what waits, as at any prompt a person types. What is due for a wake:
//!
//! - an item held for the session's hooks, at once when nothing can be
//!   pushed to it (agy, a Claude Code without an inbox) and once the push
//!   retries are used up when something can;
//! - an item a runtime took that the record still does not show
//!   [`CheckTiming::seen_within`] later. For Claude Code and Codex the push
//!   is always the first move and the typed line the fallback. Such items
//!   are offered again with the wake, flagged re-sent, after the record is
//!   read once more so nothing seen in the meantime is sent twice.
//!
//! Idle is what the terminal shows, not what a hook last said: the TUI
//! wrote nothing for [`CheckTiming::wake_quiet`], no prompt or choice is on
//! its screen, no line a person left unsent is in its input and nobody
//! typed lately. The host checks all four under the lock it types with. So
//! a daemon restart or a turn that ended without its Stop hook cannot hide
//! an idle session, and nothing is typed over a draft, into a prompt or
//! into a running turn. At most [`WAKES`] wakes go unanswered,
//! [`CheckTiming::wake_spacing`] apart; a hook of the session or a person
//! typing there starts the count again.
//!
//! # Saying so
//!
//! When a message is not getting through, one note in its thread tells the
//! sender and the chair (the Speaker when the chair is the one stuck), from
//! the senator it is for: after the wakes went unanswered, or after
//! [`CheckTiming::stuck_after`] behind a prompt, a person's draft, or in a
//! terminal Senate does not host. A busy session is not stuck: it is told
//! of only after [`CheckTiming::busy_after`] of work without a pause. A
//! note of this kind is never itself reported, and one follows when the
//! message gets through.

use super::session_timeline::Source;
use super::*;
use crate::host::control::Wake;
use crate::ipc::{SessionActivity, TimelineKind};
use crate::state::Tracked;

/// How long an item a runtime took is looked for in the session's record.
const FOLLOWED: Duration = Duration::from_secs(24 * 3600);
/// Wakes typed into one session with no sign of life in answer.
pub(super) const WAKES: u32 = 3;
/// A turn its hooks say is running, in a terminal silent this long, ended
/// without its Stop hook: a running turn keeps repainting.
const TURN_SILENT: Duration = Duration::from_secs(120);
/// A record's own timestamps may trail the daemon's clock by this much.
const CLOCK_SLACK: chrono::Duration = chrono::Duration::seconds(5);
/// When to look again at a session that is busy or blocked.
const RECHECK: Duration = Duration::from_secs(10);
/// How soon after a runtime took an item its record is first looked at
/// again: long enough for the runtime to have written it.
const FIRST_LOOK: Duration = Duration::from_secs(2);

/// Why a message is not getting through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Why {
    /// Idle, and every wake went unanswered.
    NoResponse,
    /// A prompt or a choice waits in its terminal.
    Prompt,
    /// A line a person left unsent is in its input.
    Draft,
    /// It runs in a terminal Senate does not host.
    NotHosted,
    /// Its terminal host is from before the wake request.
    OldHost,
    /// It has worked without a pause for [`CheckTiming::busy_after`].
    Busy,
}

impl Why {
    fn as_str(self) -> &'static str {
        match self {
            Self::NoResponse => "no_response",
            Self::Prompt => "prompt",
            Self::Draft => "draft",
            Self::NotHosted => "not_hosted",
            Self::OldHost => "old_host",
            Self::Busy => "busy",
        }
    }
}

/// What the check remembers of one session between passes. Lost at a
/// restart, which costs at most [`WAKES`] more wakes.
#[derive(Default)]
struct Watch {
    wakes: u32,
    last_wake: Option<Instant>,
    /// Since when it has been busy with something unseen, without a pause.
    busy_since: Option<Instant>,
    /// The items whose sender was told it is busy.
    busy_told: BTreeSet<ItemKey>,
    /// It keeps a record Senate reads.
    readable: bool,
}

/// A session's record, read as it grows, and the Senate messages in it.
struct Reader {
    path: PathBuf,
    runtime: Runtime,
    source: Source,
    after: u64,
    /// `(committee, seq)` of each message the record shows, with the
    /// latest time the runtime stamped one.
    found: HashMap<(String, u64), Option<chrono::DateTime<chrono::Utc>>>,
}

impl Reader {
    /// Read what the record gained; `None` when it cannot be read (it is
    /// opened afresh the next time).
    fn scan(reader: Option<Self>, path: PathBuf, runtime: Runtime) -> Option<Self> {
        let mut reader = match reader {
            Some(reader) => reader,
            None => Self {
                source: Source::open(path.clone(), runtime, 0).ok()?,
                path,
                runtime,
                after: 0,
                found: HashMap::new(),
            },
        };
        loop {
            let items = reader
                .source
                .updates(&reader.path, reader.runtime, reader.after)?;
            if items.is_empty() {
                return Some(reader);
            }
            for item in items {
                reader.after = reader.after.max(item.seq);
                if let TimelineKind::SenateMessage {
                    committee: Some(committee),
                    seq: Some(seq),
                    ..
                } = item.kind
                {
                    let at = reader.found.entry((committee, seq)).or_insert(item.ts);
                    *at = (*at).max(item.ts);
                }
            }
        }
    }

    fn shows(&self, row: &Tracked) -> bool {
        let key = (row.item.event.committee.to_string(), row.item.event.seq.0);
        self.found
            .get(&key)
            .is_some_and(|at| at.is_none_or(|at| at + CLOCK_SLACK >= row.item.enqueued_at))
    }
}

#[derive(Default)]
pub(super) struct Checks {
    watches: Mutex<HashMap<String, Watch>>,
    readers: tokio::sync::Mutex<HashMap<String, Reader>>,
}

impl Checks {
    /// The session showed life (one of its hooks ran): wakes count from
    /// nothing again.
    pub(super) fn alive(&self, session: &str) {
        if let Some(watch) = lock(&self.watches).get_mut(session) {
            watch.wakes = 0;
            watch.last_wake = None;
        }
    }
}

/// What waits for one session, for the lists that show sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Standing {
    /// Messages held for it, or handed to it and not in its record.
    pub(super) waiting: u32,
    pub(super) since: Option<chrono::DateTime<chrono::Utc>>,
    /// The sender and the chair were told one of them is not getting
    /// through.
    pub(super) stuck: bool,
}

/// How long `row` has been where it is: handed over, or else received.
fn age(row: &Tracked) -> Duration {
    (chrono::Utc::now() - row.waiting_since())
        .to_std()
        .unwrap_or_default()
}

fn keys_of_rows<'a>(rows: impl IntoIterator<Item = &'a Tracked>) -> Vec<ItemKey> {
    rows.into_iter().map(|row| row.item.key()).collect()
}

fn root_of(event: &Event) -> Seq {
    event.thread.unwrap_or(event.seq)
}

/// `seq 4`, `seq 4 and 7`, `seq 4, 7 and 9`.
fn seqs_in_words(seqs: &[Seq], locale: Locale) -> String {
    let words: Vec<String> = seqs.iter().map(ToString::to_string).collect();
    let (and, comma) = (locale.pick("、", " and "), locale.pick("、", ", "));
    match words.as_slice() {
        [] => String::new(),
        [one] => format!("seq {one}"),
        [head @ .., last] => format!("seq {}{and}{last}", head.join(comma)),
    }
}

/// Recheck cadence once a blocked message has passed `stuck_after`.
fn blocked_recheck(why: Why, after: Duration) -> Duration {
    match why {
        Why::Draft | Why::Prompt => RECHECK.min(after),
        _ => after,
    }
}

/// `45s`, `12m`, `3h`: the largest whole unit.
fn span(d: Duration) -> String {
    crate::sessions::short(d.as_secs())
}

impl Shared {
    /// The agent wrote in thread `root`: what it was handed there is acted
    /// on.
    pub(super) fn note_acted(&self, event: &Event) {
        let Some(agent) = event.from.as_senator() else {
            return;
        };
        if event.meta.contains_key(meta::DELIVERY_CHECK) {
            return;
        }
        match self
            .store
            .mark_acted(agent, &event.committee, root_of(event))
        {
            // What was reported stuck and is now acted on is said so.
            Ok(marked) if marked > 0 => self.work.notify_one(),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, %agent, "recording that an agent acted on a thread");
            }
        }
    }

    /// What waits for each session and is not known seen.
    pub(super) fn standings(&self) -> HashMap<String, Standing> {
        let rows = match self.store.unseen(FOLLOWED) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "reading what waits for each session");
                return HashMap::new();
            }
        };
        let watches = lock(&self.checks.watches);
        let mut standings: HashMap<String, Standing> = HashMap::new();
        for row in rows {
            let Some(session) = row.item.session_id.as_deref() else {
                continue;
            };
            // Handed over with no record to look in: nothing says it waits.
            if row.item.status == ItemStatus::Delivered
                && !watches.get(session).is_some_and(|watch| watch.readable)
            {
                continue;
            }
            let standing = standings.entry(session.to_owned()).or_default();
            standing.waiting += 1;
            let since = row.waiting_since();
            standing.since = Some(standing.since.map_or(since, |at| at.min(since)));
            standing.stuck |= row.stuck();
        }
        standings
    }

    /// What the lists say of one session. `quiet_for`: how long its hosted
    /// TUI has written nothing.
    pub(super) fn session_delivery(
        &self,
        running: bool,
        working: bool,
        prompt: bool,
        quiet_for: Option<Duration>,
        standing: Option<&Standing>,
    ) -> crate::ipc::SessionDelivery {
        let standing = standing.copied().unwrap_or_default();
        crate::ipc::SessionDelivery {
            activity: running.then(|| self.activity(working, prompt, quiet_for, Some(&standing))),
            waiting: standing.waiting,
            waiting_since: standing.since,
            stuck: standing.stuck,
        }
    }

    /// What a session is doing, as the lists show it.
    fn activity(
        &self,
        working: bool,
        prompt: bool,
        quiet_for: Option<Duration>,
        standing: Option<&Standing>,
    ) -> SessionActivity {
        if prompt {
            SessionActivity::Prompt
        } else if self.busy(working, quiet_for) {
            SessionActivity::Active
        } else if standing.is_some_and(|standing| standing.stuck) {
            SessionActivity::NotResponding
        } else {
            SessionActivity::Idle
        }
    }

    /// A turn is running: the TUI wrote lately, or its hooks say so and
    /// its terminal has not gone silent since.
    fn busy(&self, working: bool, quiet_for: Option<Duration>) -> bool {
        match quiet_for {
            Some(quiet) => quiet < self.timing.check.wake_quiet || (working && quiet < TURN_SILENT),
            None => working,
        }
    }

    /// Look for `rows` in `session`'s own record and mark the ones it
    /// shows seen; `rows` keeps the rest. `false`: the session has no
    /// record Senate can read.
    async fn confirm_seen(&self, session: &Session, rows: &mut Vec<Tracked>) -> bool {
        let Some(path) = crate::timeline::path(session) else {
            return false;
        };
        let runtime = session.runtime;
        let mut readers = self.checks.readers.lock().await;
        let reader = readers
            .remove(&session.session_id)
            .filter(|reader| reader.path == path && reader.runtime == runtime);
        let scanned = tokio::task::spawn_blocking(move || Reader::scan(reader, path, runtime))
            .await
            .ok()
            .flatten();
        let Some(reader) = scanned else {
            return false;
        };
        let (seen, rest): (Vec<Tracked>, Vec<Tracked>) = std::mem::take(rows)
            .into_iter()
            .partition(|row| reader.shows(row));
        *rows = rest;
        readers.insert(session.session_id.clone(), reader);
        drop(readers);
        if !seen.is_empty() {
            match self.store.agent_saw(&keys_of_rows(&seen)) {
                Ok(_) => {
                    tracing::info!(
                        session = %session.session_id,
                        count = seen.len(),
                        "the session's own record shows what it was handed"
                    );
                    self.publish(WatchEvent::Sessions);
                }
                Err(error) => tracing::warn!(%error, "recording what a session has seen"),
            }
        }
        true
    }

    /// One pass of the check, after each delivery pass. Returns when to
    /// look again, if something waits on time alone.
    pub(super) async fn check_deliveries(&self, sessions: &[Session]) -> Option<Instant> {
        match self.check_deliveries_once(sessions).await {
            Ok(due) => due,
            Err(error) => {
                tracing::warn!("delivery check failed: {error:#}");
                Some(Instant::now() + RECHECK)
            }
        }
    }

    async fn check_deliveries_once(&self, sessions: &[Session]) -> anyhow::Result<Option<Instant>> {
        let mut by_session: BTreeMap<String, Vec<Tracked>> = BTreeMap::new();
        for row in self.store.unseen(FOLLOWED)? {
            if let Some(session) = row.item.session_id.clone() {
                by_session.entry(session).or_default().push(row);
            }
        }
        lock(&self.checks.watches).retain(|session, _| by_session.contains_key(session));
        self.checks
            .readers
            .lock()
            .await
            .retain(|session, _| by_session.contains_key(session));
        let mut due: Option<Instant> = None;
        for (sid, rows) in by_session {
            let Some(session) = sessions.iter().find(|s| s.session_id == sid) else {
                continue;
            };
            // A runtime writes what it took into its record a moment after
            // taking it: look once more soon, whatever else is due later.
            let first_look = rows
                .iter()
                .filter(|row| row.item.status == ItemStatus::Delivered)
                .filter_map(|row| FIRST_LOOK.checked_sub(age(row)))
                .min()
                .map(|wait| Instant::now() + wait.max(Duration::from_millis(50)));
            let next = self.check_session(session, rows).await?;
            for at in [first_look, next].into_iter().flatten() {
                due = Some(due.map_or(at, |d| d.min(at)));
            }
        }
        // After the sessions were looked at: what this pass found seen is
        // said in it, with no later pass to wait for.
        self.tell_recovered().await?;
        Ok(due)
    }

    /// Check one session with something not known seen. Returns when to
    /// look at it again.
    async fn check_session(
        &self,
        session: &Session,
        rows: Vec<Tracked>,
    ) -> anyhow::Result<Option<Instant>> {
        let sid = session.session_id.as_str();
        let timing = self.timing.check;
        let now = Instant::now();
        let (held, mut injected): (Vec<Tracked>, Vec<Tracked>) = rows
            .into_iter()
            .partition(|row| row.item.status == ItemStatus::Queued);
        let readable = !injected.is_empty() && self.confirm_seen(session, &mut injected).await;
        if !injected.is_empty() || readable {
            lock(&self.checks.watches)
                .entry(sid.to_owned())
                .or_default()
                .readable = readable;
        }
        if !readable {
            // No record to look in: what a runtime took is not followed.
            injected.clear();
        }
        if held.is_empty() && injected.is_empty() {
            lock(&self.checks.watches).remove(sid);
            return Ok(None);
        }
        let waited = age;
        let longest = held.iter().chain(&injected).map(waited).max();
        let longest = longest.unwrap_or_default();
        let all = || held.iter().chain(&injected).collect::<Vec<_>>();

        let paths = Paths::new(&self.home);
        let host = match self.store.session_host(sid)? {
            Some(host) if crate::host::running(&paths, &host).unwrap_or(false) => host,
            // Its own terminal: all Senate knows of a turn there is its hooks.
            _ if self.session_working(session) => {
                return self.while_busy(session, all(), longest).await;
            }
            _ => return Ok(self.blocked(session, Why::NotHosted, all(), longest).await),
        };
        if crate::host::unbound_context(&paths, &host).is_some() {
            return Ok(None);
        }

        // What a wake is due for. A push comes first wherever one can be
        // made: held items wait for its retries, and an item a runtime
        // took for the record to show it.
        let pushable = self.can_push(session)?;
        let retried = matches!(self.hold(sid, true, now), Hold::Indefinitely);
        let held_due = !held.is_empty() && (!pushable || retried);
        let unseen_due: Vec<&Tracked> = injected
            .iter()
            .filter(|row| waited(row) >= timing.seen_within)
            .collect();
        if !held_due && unseen_due.is_empty() {
            let next = injected
                .iter()
                .map(|row| timing.seen_within.saturating_sub(waited(row)))
                .min();
            return Ok(next.map(|wait| now + wait.max(Duration::from_millis(50))));
        }

        if self.hosted_prompts().contains_key(&host) {
            return Ok(self.blocked(session, Why::Prompt, all(), longest).await);
        }
        let facts = crate::host::facts(&paths, &host)?;
        // A person typed after the last wake: they are there, and the
        // count starts again.
        {
            let mut watches = lock(&self.checks.watches);
            let watch = watches.entry(sid.to_owned()).or_default();
            if let (Some(typed), Some(woken)) = (facts.typed_secs_ago, watch.last_wake)
                && Duration::from_secs(typed) < woken.elapsed()
            {
                watch.wakes = 0;
                watch.last_wake = None;
            }
        }
        if self.busy(self.session_working(session), facts.quiet_for) {
            return self.while_busy(session, all(), longest).await;
        }
        let (wakes, last_wake) = {
            let mut watches = lock(&self.checks.watches);
            let watch = watches.entry(sid.to_owned()).or_default();
            watch.busy_since = None;
            (watch.wakes, watch.last_wake)
        };
        let spaced = last_wake.map(|at| at + timing.wake_spacing);
        if let Some(at) = spaced
            && now < at
        {
            return Ok(Some(at));
        }
        if wakes >= WAKES {
            self.tell_stuck(session, Why::NoResponse, all(), longest)
                .await;
            return Ok(Some(now + timing.stuck_after));
        }
        let wake = crate::host::control::wake(
            &paths,
            &host,
            timing.wake_quiet,
            session.runtime == Runtime::Agy,
        )
        .await;
        match wake {
            Ok(Wake::Typed) => {
                {
                    let mut watches = lock(&self.checks.watches);
                    let watch = watches.entry(sid.to_owned()).or_default();
                    watch.wakes += 1;
                    watch.last_wake = Some(Instant::now());
                }
                // Offer again what the record still does not show, having
                // read it once more: what it gained since the pass began
                // must not be sent twice.
                let mut again: Vec<Tracked> = unseen_due.into_iter().cloned().collect();
                if !again.is_empty() {
                    self.confirm_seen(session, &mut again).await;
                }
                let offered = self.store.offer_again(&keys_of_rows(&again), sid)?;
                tracing::info!(
                    session = sid,
                    runtime = session.runtime.as_str(),
                    wake = wakes + 1,
                    held = held.len(),
                    offered_again = offered.len(),
                    "typed the wake line into an idle hosted session"
                );
                if !offered.is_empty() {
                    self.work.notify_one();
                }
                Ok(Some(Instant::now() + timing.wake_spacing))
            }
            Ok(Wake::Typing(wait) | Wake::Busy(wait)) => {
                Ok(Some(now + wait.max(Duration::from_millis(50))))
            }
            Ok(Wake::Draft) => Ok(self.blocked(session, Why::Draft, all(), longest).await),
            Ok(Wake::Prompt) => Ok(self.blocked(session, Why::Prompt, all(), longest).await),
            Ok(Wake::Unsupported) => Ok(self.blocked(session, Why::OldHost, all(), longest).await),
            Err(error) => {
                // Counted, so a host that cannot type is not asked forever.
                tracing::warn!(%error, session = sid, "waking an idle hosted session");
                let mut watches = lock(&self.checks.watches);
                let watch = watches.entry(sid.to_owned()).or_default();
                watch.wakes += 1;
                watch.last_wake = Some(Instant::now());
                Ok(Some(now + timing.wake_spacing))
            }
        }
    }

    /// Held back for a reason a person must clear: told once what waits
    /// has waited [`CheckTiming::stuck_after`], looked at again meanwhile.
    /// Once a draft or prompt block has been reported, keep rechecking at
    /// [`RECHECK`] so clearing the terminal resumes delivery promptly rather
    /// than stalling for another full `stuck_after` window.
    async fn blocked(
        &self,
        session: &Session,
        why: Why,
        rows: Vec<&Tracked>,
        longest: Duration,
    ) -> Option<Instant> {
        let after = self.timing.check.stuck_after;
        if longest >= after {
            self.tell_stuck(session, why, rows, longest).await;
            Some(Instant::now() + blocked_recheck(why, after))
        } else {
            Some(Instant::now() + RECHECK.min(after - longest))
        }
    }

    /// A turn is running: nothing is typed and nothing is wrong. Only
    /// work without a pause for [`CheckTiming::busy_after`] is told.
    async fn while_busy(
        &self,
        session: &Session,
        rows: Vec<&Tracked>,
        longest: Duration,
    ) -> anyhow::Result<Option<Instant>> {
        let now = Instant::now();
        let since = *lock(&self.checks.watches)
            .entry(session.session_id.clone())
            .or_default()
            .busy_since
            .get_or_insert(now);
        if now.duration_since(since) >= self.timing.check.busy_after {
            self.tell_stuck(session, Why::Busy, rows, longest).await;
        }
        Ok(Some(now + RECHECK.min(self.timing.check.wake_quiet)))
    }

    /// Who is told about `agent`'s messages from `senders`: each sender,
    /// and the chair, or the Speaker when the chair is `agent` itself or
    /// unknown.
    fn told(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
        senders: impl IntoIterator<Item = Address>,
    ) -> Vec<Address> {
        told(agent, self.chair_of(committee).as_ref(), senders)
    }

    /// Tell the sender and the chair that `rows` are not getting through to
    /// `session`, one note per thread, once per item. A note of this kind
    /// is never itself reported.
    async fn tell_stuck(&self, session: &Session, why: Why, rows: Vec<&Tracked>, waited: Duration) {
        let agent = &session.agent;
        let mut threads: BTreeMap<(CommitteeId, Seq), Vec<&Tracked>> = BTreeMap::new();
        // Busy is said once and is not stuck: the item stays unreported, so
        // if the session then fails to answer its wakes that is still told.
        let busy_told: BTreeSet<ItemKey> = lock(&self.checks.watches)
            .get(&session.session_id)
            .map(|watch| watch.busy_told.clone())
            .unwrap_or_default();
        for row in rows {
            let event = &row.item.event;
            if row.stuck()
                || event.meta.contains_key(meta::DELIVERY_CHECK)
                || (why == Why::Busy && busy_told.contains(&row.item.key()))
            {
                continue;
            }
            threads
                .entry((event.committee.clone(), root_of(event)))
                .or_default()
                .push(row);
        }
        for ((committee, root), rows) in threads {
            let seqs: Vec<Seq> = rows.iter().map(|row| row.item.event.seq).collect();
            let to = self.told(
                &committee,
                agent,
                rows.iter().map(|row| row.item.event.from.clone()),
            );
            if to.is_empty() {
                continue;
            }
            let first = rows[0];
            let id = EventId::derived(
                u64::try_from(first.item.enqueued_at.timestamp_millis()).unwrap_or(0),
                &format!(
                    "delivery-check/stuck/{committee}/{agent}/{}/{}",
                    first.item.event.seq,
                    why.as_str()
                ),
            );
            let event = NewEvent::new(
                committee.clone(),
                Kind::Note,
                Address::senator(agent),
                stuck_body(agent, &seqs, why, waited, self.locale),
            )
            .with_id(id)
            .in_thread(root)
            .with_to(to)
            .with_meta(
                meta::DELIVERY_CHECK,
                serde_json::json!({
                    "state": if why == Why::Busy { "busy" } else { "stuck" },
                    "why": why.as_str(),
                    "seqs": seqs,
                    "session": session.session_id,
                }),
            );
            match self.api.append(&event).await {
                Ok(posted) => {
                    tracing::warn!(
                        session = %session.session_id,
                        %agent,
                        %committee,
                        why = why.as_str(),
                        count = seqs.len(),
                        note = %posted.event.seq,
                        "messages are not getting through; told the sender and the chair"
                    );
                    let keys = keys_of_rows(rows);
                    if why == Why::Busy {
                        lock(&self.checks.watches)
                            .entry(session.session_id.clone())
                            .or_default()
                            .busy_told
                            .extend(keys);
                    } else if let Err(error) = self.store.mark_stuck(&keys) {
                        tracing::warn!(%error, "recording a reported delivery");
                    }
                    self.publish(WatchEvent::Sessions);
                }
                Err(error) => {
                    tracing::warn!(%error, %agent, %committee, "posting that messages are not getting through");
                }
            }
        }
    }

    /// Say of each reported item that has got through since that it has,
    /// one note per thread.
    async fn tell_recovered(&self) -> anyhow::Result<()> {
        let mut threads: BTreeMap<(SenatorName, CommitteeId, Seq), Vec<Tracked>> = BTreeMap::new();
        for row in self.store.recovered()? {
            let event = &row.item.event;
            threads
                .entry((
                    row.item.agent.clone(),
                    event.committee.clone(),
                    root_of(event),
                ))
                .or_default()
                .push(row);
        }
        for ((agent, committee, root), rows) in threads {
            let seqs: Vec<Seq> = rows.iter().map(|row| row.item.event.seq).collect();
            let to = self.told(
                &committee,
                &agent,
                rows.iter().map(|row| row.item.event.from.clone()),
            );
            let first = &rows[0];
            let stuck_at = first.stuck_at.map_or(0, |at| at.timestamp_millis());
            let id = EventId::derived(
                u64::try_from(stuck_at).unwrap_or(0),
                &format!(
                    "delivery-check/through/{committee}/{agent}/{}",
                    first.item.event.seq
                ),
            );
            let seen = rows.iter().all(|row| row.seen_at.is_some());
            let event = NewEvent::new(
                committee.clone(),
                Kind::Note,
                Address::senator(&agent),
                through_body(&agent, &seqs, seen, self.locale),
            )
            .with_id(id)
            .in_thread(root)
            .with_to(to.clone())
            .with_meta(
                meta::DELIVERY_CHECK,
                serde_json::json!({"state": "through", "seqs": seqs}),
            );
            if !to.is_empty()
                && let Err(error) = self.api.append(&event).await
            {
                // Said at a later pass: the items stay reported until then.
                tracing::warn!(%error, %agent, %committee, "posting that messages got through");
                continue;
            }
            self.store.clear_stuck(&keys_of_rows(&rows))?;
            self.publish(WatchEvent::Sessions);
        }
        Ok(())
    }
}

/// Who hears about `agent`'s messages from `senders`: each sender but
/// `agent` itself, then the chair, or the Speaker when `agent` is the chair
/// or there is none. A chair who sent one is told once.
fn told(
    agent: &SenatorName,
    chair: Option<&SenatorName>,
    senders: impl IntoIterator<Item = Address>,
) -> Vec<Address> {
    let me = Address::senator(agent);
    let mut to: Vec<Address> = Vec::new();
    for sender in senders {
        if sender != me && !to.contains(&sender) {
            to.push(sender);
        }
    }
    let lead = match chair {
        Some(chair) if chair != agent => {
            (!to.contains(&Address::senator(chair))).then_some(Address::Chair)
        }
        _ => Some(Address::Speaker),
    };
    if let Some(lead) = lead
        && !to.contains(&lead)
    {
        to.push(lead);
    }
    to
}

fn stuck_body(
    agent: &SenatorName,
    seqs: &[Seq],
    why: Why,
    waited: Duration,
    locale: Locale,
) -> String {
    let who = Address::senator(agent).in_text();
    let what = seqs_in_words(seqs, locale);
    let waited = span(waited);
    let attach = format!("senate attach {agent}");
    if why == Why::Busy {
        return match locale {
            Locale::En => format!(
                "[senate] {who} is busy and has not taken in {what} yet (waiting {waited}). It has been working without a pause; it takes it when its turn allows. Nothing needs doing unless this goes on."
            ),
            Locale::Zh => format!(
                "[senate] {who} 正忙，尚未读到 {what}（已等待 {waited}）。它一直在工作没有停顿，轮到时会读取。除非持续如此，否则无需处理。"
            ),
        };
    }
    let (because, then) = match (why, locale) {
        (Why::NoResponse, Locale::En) => (
            format!(
                "Its session is idle and did not respond to {WAKES} wake attempts."
            ),
            format!("Ask a person to run `{attach}` and look at its terminal."),
        ),
        (Why::NoResponse, Locale::Zh) => (
            format!("它的会话空闲，{WAKES} 次唤醒都没有回应。"),
            format!("请让人员运行 `{attach}` 查看它的终端。"),
        ),
        (Why::Prompt, Locale::En) => (
            "Its terminal is waiting on a prompt or a choice, which Senate does not answer."
                .to_owned(),
            format!("Ask a person to run `{attach}` and decide there."),
        ),
        (Why::Prompt, Locale::Zh) => (
            "它的终端停在一个提示或选择上，Senate 不会代为回答。".to_owned(),
            format!("请让人员运行 `{attach}` 并在终端内决定。"),
        ),
        (Why::Draft, Locale::En) => (
            "A person left text unsent in its input, and Senate does not type over it.".to_owned(),
            format!("Ask them to send or clear it: `{attach}`."),
        ),
        (Why::Draft, Locale::Zh) => (
            "有人在它的输入框里留下了未发送的文字，Senate 不会在上面输入。".to_owned(),
            format!("请让他们发送或清除：`{attach}`。"),
        ),
        (Why::NotHosted, Locale::En) => (
            "It runs in a terminal Senate does not host, so it takes messages only at its next prompt."
                .to_owned(),
            "Ask its person to type in that terminal.".to_owned(),
        ),
        (Why::NotHosted, Locale::Zh) => (
            "它运行在 Senate 未托管的终端里，只在下一次提示时读取消息。".to_owned(),
            "请让它的使用者在那个终端里输入。".to_owned(),
        ),
        (Why::OldHost | Why::Busy, Locale::En) => (
            "Its terminal was started by an older Senate that cannot wake it.".to_owned(),
            format!("Ask a person to type in it (`{attach}`) or restart the session."),
        ),
        (Why::OldHost | Why::Busy, Locale::Zh) => (
            "它的终端由较旧的 Senate 启动，无法被唤醒。".to_owned(),
            format!("请让人员在其中输入（`{attach}`）或重启该会话。"),
        ),
    };
    match locale {
        Locale::En => {
            format!("[senate] {who} has not taken in {what} (waiting {waited}). {because}\n{then}")
        }
        Locale::Zh => {
            format!("[senate] {who} 尚未读到 {what}（已等待 {waited}）。{because}\n{then}")
        }
    }
}

fn through_body(agent: &SenatorName, seqs: &[Seq], seen: bool, locale: Locale) -> String {
    let who = Address::senator(agent).in_text();
    let what = seqs_in_words(seqs, locale);
    match (seen, locale) {
        (true, Locale::En) => format!("[senate] {who} has now read {what}."),
        (true, Locale::Zh) => format!("[senate] {who} 现已读到 {what}。"),
        (false, Locale::En) => format!("[senate] {who} has now taken {what} at its prompt."),
        (false, Locale::Zh) => format!("[senate] {who} 现已在提示时取走 {what}。"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sender and the chair; the Speaker when the chair is the one
    /// stuck or there is none; nobody twice, and never the stuck one.
    #[test]
    fn the_sender_and_the_chair_are_told_and_the_speaker_when_the_chair_is_stuck() {
        let name = |s: &str| -> SenatorName { s.parse().unwrap() };
        let (stuck, chair, sender) = (name("agy-1"), name("main"), name("codex-2"));
        let from = |s: &SenatorName| Address::senator(s);
        assert_eq!(
            told(&stuck, Some(&chair), [from(&sender)]),
            [from(&sender), Address::Chair]
        );
        assert_eq!(
            told(
                &stuck,
                Some(&chair),
                [from(&chair), from(&sender), from(&chair)]
            ),
            [from(&chair), from(&sender)],
            "the chair sent one: told once, as its sender"
        );
        assert_eq!(
            told(&chair, Some(&chair), [from(&sender)]),
            [from(&sender), Address::Speaker],
            "the chair is the one stuck"
        );
        assert_eq!(
            told(&stuck, None, [from(&sender)]),
            [from(&sender), Address::Speaker]
        );
        assert_eq!(
            told(&stuck, Some(&chair), [from(&stuck)]),
            [Address::Chair],
            "never the stuck senator itself"
        );
        assert_eq!(told(&stuck, None, [Address::Speaker]), [Address::Speaker]);
    }

    /// A record shows a message only as a Senate message of that seq on
    /// that committee, written no earlier than Senate received it.
    #[test]
    fn a_record_shows_a_message_by_committee_seq_and_time() {
        use std::io::Write as _;
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_owned();
        let delivery = |committee: &str, seq: u64| {
            format!(
                "[senate] The messages below come from the Senate committee log for {committee}.\n\n\
                 [senate · {committee} · seq {seq} · from [senator::alex] · note]\n│ body\n"
            )
        };
        let now = chrono::Utc::now();
        let record = |row: serde_json::Value| {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(file, "{row}").unwrap();
        };
        // Pushed while idle; taken mid-turn; handed over by a hook; and one
        // Claude only queued, which it does not have yet.
        record(
            serde_json::json!({"type": "user", "isMeta": true, "timestamp": now,
            "message": {"role": "user", "content": delivery("refund", 4)}}),
        );
        record(serde_json::json!({"type": "attachment", "timestamp": now,
            "attachment": {"type": "queued_command", "prompt": delivery("refund", 5)}}));
        record(serde_json::json!({"type": "attachment", "timestamp": now,
            "attachment": {"type": "hook_additional_context", "content": [delivery("refund", 6)]}}));
        record(
            serde_json::json!({"type": "queue-operation", "operation": "enqueue",
            "content": delivery("refund", 7)}),
        );
        // An old log's seq 8, from before this one was received.
        record(serde_json::json!({"type": "user", "isMeta": true,
            "timestamp": now - chrono::Duration::hours(3),
            "message": {"role": "user", "content": delivery("refund", 8)}}));
        let reader = Reader::scan(None, path.clone(), Runtime::Claude).unwrap();
        let row = |committee: &str, seq: u64| {
            let event: Event = serde_json::from_value(serde_json::json!({
                "id": EventId::new(), "committee": committee, "seq": seq, "ts": now,
                "kind": "note", "from": "senator::alex", "to": ["senator::claude"], "body": "body",
            }))
            .unwrap();
            Tracked {
                item: QueueItem {
                    agent: "claude".parse().unwrap(),
                    event,
                    status: ItemStatus::Delivered,
                    session_id: Some("s".into()),
                    enqueued_at: now - chrono::Duration::seconds(1),
                    redelivered: false,
                },
                via: Some("inbox".into()),
                injected_at: Some(now),
                seen_at: None,
                acted_at: None,
                stuck_at: None,
            }
        };
        for seq in [4, 5, 6] {
            assert!(reader.shows(&row("refund", seq)), "seq {seq}");
        }
        assert!(!reader.shows(&row("refund", 7)), "queued is not taken");
        assert!(
            !reader.shows(&row("refund", 8)),
            "written before it was received"
        );
        assert!(
            !reader.shows(&row("ledger", 4)),
            "another committee's seq 4"
        );

        // Read on as the record grows; a record that is gone reads as none.
        let reader = {
            record(
                serde_json::json!({"type": "user", "isMeta": true, "timestamp": now,
                "message": {"role": "user", "content": delivery("refund", 7)}}),
            );
            Reader::scan(Some(reader), path.clone(), Runtime::Claude).unwrap()
        };
        assert!(reader.shows(&row("refund", 7)));
        assert!(Reader::scan(None, "/nonexistent/record.jsonl".into(), Runtime::Claude).is_none());
    }

    /// Which sessions keep a record to look in: Claude Code, Codex and agy.
    /// An ACP client keeps none Senate reads, whatever path it names, so
    /// what it took stays `injected` and is never woken for or reported.
    // Unix paths and programs; remote authority is Unix only.
    #[cfg(unix)]
    #[test]
    fn an_acp_session_has_no_record_to_look_in() {
        let now = chrono::Utc::now();
        let session = |runtime: &str, id: &str| -> Session {
            serde_json::from_value(serde_json::json!({
                "session_id": id, "agent": "tester", "agent_source": "session",
                "runtime": runtime, "cwd": "/work", "state": "live", "started_at": now,
                "last_seen": now, "transcript_path": "/work/record.jsonl",
            }))
            .unwrap()
        };
        assert!(crate::timeline::path(&session("acp", "acp-1")).is_none());
        for runtime in ["claude", "codex"] {
            assert_eq!(
                crate::timeline::path(&session(runtime, "s-1")).as_deref(),
                Some(Path::new("/work/record.jsonl"))
            );
        }
        let agy = session("agy", "agy:7c1d2e3f-4a5b-4c6d-8e9f-0a1b2c3d4e5f");
        assert!(crate::timeline::path(&agy).is_some_and(|path| {
            path.ends_with("conversations/7c1d2e3f-4a5b-4c6d-8e9f-0a1b2c3d4e5f.db")
        }));
    }

    #[test]
    fn the_note_says_who_what_how_long_and_why() {
        let agent: SenatorName = "agy-7c1d".parse().unwrap();
        let body = stuck_body(
            &agent,
            &[Seq(4), Seq(7), Seq(9)],
            Why::NoResponse,
            Duration::from_secs(250),
            Locale::En,
        );
        assert_eq!(
            body,
            "[senate] [senator::agy-7c1d] has not taken in seq 4, 7 and 9 (waiting 4m). Its \
             session is idle and did not respond to 3 wake attempts.\nAsk a person to run \
             `senate attach agy-7c1d` and look at its terminal."
        );
        let draft = stuck_body(
            &agent,
            &[Seq(4)],
            Why::Draft,
            Duration::from_secs(300),
            Locale::En,
        );
        assert!(
            draft.contains("has not taken in seq 4 (waiting 5m)")
                && draft.contains("Senate does not type over it"),
            "{draft}"
        );
        let busy = stuck_body(
            &agent,
            &[Seq(4), Seq(5)],
            Why::Busy,
            Duration::from_secs(1800),
            Locale::En,
        );
        assert!(
            busy.contains("is busy and has not taken in seq 4 and 5 yet (waiting 30m)"),
            "{busy}"
        );
        for why in [
            Why::NoResponse,
            Why::Prompt,
            Why::Draft,
            Why::NotHosted,
            Why::OldHost,
            Why::Busy,
        ] {
            let zh = stuck_body(&agent, &[Seq(1)], why, Duration::from_secs(60), Locale::Zh);
            assert!(zh.starts_with("[senate] [senator::agy-7c1d]"), "{zh}");
        }
        assert_eq!(
            through_body(&agent, &[Seq(4)], true, Locale::En),
            "[senate] [senator::agy-7c1d] has now read seq 4."
        );
        assert_eq!(
            through_body(&agent, &[Seq(4), Seq(7)], false, Locale::En),
            "[senate] [senator::agy-7c1d] has now taken seq 4 and 7 at its prompt."
        );
    }

    #[test]
    fn blocked_draft_and_prompt_recheck_promptly_after_stuck_after() {
        let five_min = Duration::from_secs(300);
        assert_eq!(blocked_recheck(Why::Draft, five_min), RECHECK);
        assert_eq!(blocked_recheck(Why::Prompt, five_min), RECHECK);
        assert_eq!(
            blocked_recheck(Why::Draft, Duration::from_secs(3)),
            Duration::from_secs(3)
        );
        assert_eq!(blocked_recheck(Why::NotHosted, five_min), five_min);
        assert_eq!(blocked_recheck(Why::OldHost, five_min), five_min);
    }
}
