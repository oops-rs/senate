//! `senate cleanup`: the plan of one run, the rules that make it, and what
//! the run did.
//!
//! The daemon derives a [`Plan`] from the Senate's server and its own
//! state, the CLI shows it, and the person approves it once. The approved
//! plan travels back in the request that applies it. The daemon then derives
//! the plan again and does only what is in both ([`Plan::agreed`]): nothing
//! the person was not shown, and nothing that stopped being true while they
//! read. Nothing is kept between the two requests.
//!
//! # A dead senator
//!
//! [`standing`] is the one rule. A seat is removed only from a senator that
//! is [`Standing::Dead`]: a coding session's own identity that the person
//! runs, with no session alive anywhere, quiet for at least
//! [`Scope::ended_for_secs`], not named to be kept, and holding no role
//! (unless the run includes roles).
//!
//! # A thread nobody will answer
//!
//! [`thread_cause`] is the other. An open thread is archived when whoever
//! must act has no seat once the run is done, or, when the person asks for
//! it, when nothing was said in it for long enough. Archiving is a `status`
//! event in the thread; every event stays in the log and in search.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate_core::alias::AliasName;
use senate_core::{Address, CommitteeId, EventId, Locale, Name, SenatorName, Seq};
use serde::{Deserialize, Serialize};

/// How long a plan may wait for its approval. An older one is refused: the
/// person approved a picture that is no longer the Senate.
pub const PLAN_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// A senator that was active this recently is left alone unless the person
/// says otherwise: a chair that stopped one minutes ago may restore it.
pub const DEFAULT_ENDED_FOR: Duration = Duration::from_secs(60 * 60);

/// The key under which a cleanup's archive status says why
/// (`{"cause": <word>, "by": <person>}`).
pub const META: &str = "cleanup";

fn is_false(value: &bool) -> bool {
    !*value
}

/// What a run covers and how it judges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    /// The committees acted on; empty for all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committees: Vec<CommitteeId>,
    /// Also archive open threads with no event for this many seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older_than_secs: Option<u64>,
    /// A senator is dead only when its last activity is this old.
    pub ended_for_secs: u64,
    /// Senators never touched, as written: an alias, `committee::alias`
    /// or a senator's own name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keep: Vec<AliasName>,
    /// Also remove dead chairs and secretaries.
    #[serde(default, skip_serializing_if = "is_false")]
    pub include_roles: bool,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            committees: Vec::new(),
            older_than_secs: None,
            ended_for_secs: DEFAULT_ENDED_FOR.as_secs(),
            keep: Vec::new(),
            include_roles: false,
        }
    }
}

impl Scope {
    /// Whether the run acts on `committee`.
    pub fn covers(&self, committee: &CommitteeId) -> bool {
        self.committees.is_empty() || self.committees.contains(committee)
    }
}

/// A role a senator holds: its seat is not an ordinary member's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Chair,
    Secretary,
}

/// What the daemon knows of one senator when it plans.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Facts {
    /// A coding session's own identity (`claude-…`, `codex-…`, `agy-…`,
    /// `acp-…`), not a name a later session takes again.
    pub session_name: bool,
    /// An agent the person runs.
    pub owned: bool,
    /// The Speaker, or the daemon's built-in session.
    pub office: bool,
    /// A session of it is live, its host runs, it is being launched, or a
    /// stale session's process is still there.
    pub live: bool,
    /// Paused: to be resumed.
    pub paused: bool,
    /// The server sees it connected.
    pub online: bool,
    /// The latest sign of it, here or on the server.
    pub last_active: Option<DateTime<Utc>>,
    /// It chairs a committee or works the Speaker's desk.
    pub role: bool,
    /// The person named it with `--keep`.
    pub kept: bool,
}

/// Where a senator stands for a run. Only [`Standing::Dead`] loses a seat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Not a cleanup's to judge: a person, the Speaker, the built-in
    /// session, a named agent, or someone else's agent.
    Exempt,
    /// A session of it is alive or paused.
    Alive,
    /// Dead, and named with `--keep`.
    Kept,
    /// Dead, and a chair or a secretary in a run that leaves roles.
    Role,
    /// Dead, but active within `--ended-for`. The only thing in its way.
    Recent,
    Dead,
}

/// The one rule for a dead senator (see the module's notes). The order is
/// part of it: a senator is [`Standing::Recent`] only when a longer wait is
/// all that keeps it.
pub fn standing(facts: &Facts, scope: &Scope, now: DateTime<Utc>) -> Standing {
    if !facts.session_name || !facts.owned || facts.office {
        return Standing::Exempt;
    }
    if facts.live || facts.paused || facts.online {
        return Standing::Alive;
    }
    if facts.kept {
        return Standing::Kept;
    }
    if facts.role && !scope.include_roles {
        return Standing::Role;
    }
    let quiet = chrono::Duration::seconds(i64::try_from(scope.ended_for_secs).unwrap_or(i64::MAX));
    // A senator with no sign of it at all has been quiet for ever.
    if facts
        .last_active
        .is_some_and(|last| now.signed_duration_since(last) < quiet)
    {
        return Standing::Recent;
    }
    Standing::Dead
}

/// Why a thread is archived, one word in the log (`meta.cleanup.cause`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// A permission notice whose session is gone.
    SessionEnded,
    /// It waits on agents, and none of them has a seat any more.
    AddresseeGone,
    /// Whoever asked and everyone asked are gone.
    AllGone,
    /// Nothing was said in it for as long as the person named.
    Stale,
}

impl Cause {
    pub const ALL: [Self; 4] = [
        Self::SessionEnded,
        Self::AddresseeGone,
        Self::AllGone,
        Self::Stale,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionEnded => "session_ended",
            Self::AddresseeGone => "addressee_gone",
            Self::AllGone => "all_gone",
            Self::Stale => "stale",
        }
    }

    /// The reason written into the archive status.
    pub fn words(self, locale: Locale) -> &'static str {
        match self {
            Self::SessionEnded => locale.pick(
                "清理：提出它的会话已经结束。",
                "Cleanup: the session that raised it has ended.",
            ),
            Self::AddresseeGone => locale.pick(
                "清理：被问到的参议员都已没有席位。",
                "Cleanup: nobody it was addressed to has a seat any more.",
            ),
            Self::AllGone => locale.pick(
                "清理：提问方和被问方都已没有席位。",
                "Cleanup: whoever asked and everyone asked have no seat any more.",
            ),
            Self::Stale => locale.pick(
                "清理：很久没有新的内容。",
                "Cleanup: nothing was said here for a long time.",
            ),
        }
    }
}

/// What the daemon knows of one open thread when it plans.
#[derive(Debug, Clone)]
pub struct ThreadFacts<'a> {
    /// Its root is a hosted permission notice.
    pub notice: bool,
    /// The daemon still follows that notice's prompt.
    pub tracked: bool,
    pub from: &'a Address,
    pub on_behalf_of: Option<&'a Address>,
    pub to: &'a [Address],
    /// It waits on the agents it was addressed to.
    pub waits_on_agents: bool,
    /// It waits on a person: escalated to one, or taken over by one.
    pub waits_on_person: bool,
    /// The last event in it.
    pub last_at: DateTime<Utc>,
}

/// Why the open thread `facts` describes is archived, if it is. `gone`
/// says whether a senator has no seat once the run is done. A person, the
/// chair and the Speaker are never gone: a thread that one of them can
/// still act on, or that waits on a person, is archived only for being
/// [`Cause::Stale`], and a notice the daemon still follows never is.
pub fn thread_cause(
    facts: &ThreadFacts<'_>,
    gone: impl Fn(&SenatorName) -> bool,
    older_than: Option<Duration>,
    now: DateTime<Utc>,
) -> Option<Cause> {
    let gone_address = |address: &Address| address.as_senator().is_some_and(&gone);
    let stale = older_than.is_some_and(|limit| {
        now.signed_duration_since(facts.last_at)
            >= chrono::Duration::from_std(limit).unwrap_or(chrono::Duration::MAX)
    });
    if facts.notice {
        if gone_address(facts.from) {
            return Some(Cause::SessionEnded);
        }
        return (stale && !facts.tracked).then_some(Cause::Stale);
    }
    if facts.waits_on_person {
        return stale.then_some(Cause::Stale);
    }
    let everyone_asked_gone = !facts.to.is_empty() && facts.to.iter().all(gone_address);
    if facts.waits_on_agents && everyone_asked_gone {
        return Some(Cause::AddresseeGone);
    }
    if everyone_asked_gone && gone_address(facts.on_behalf_of.unwrap_or(facts.from)) {
        return Some(Cause::AllGone);
    }
    stale.then_some(Cause::Stale)
}

/// One senator's place on one committee, and what goes with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatPlan {
    pub senator: SenatorName,
    /// What a person calls it there.
    pub shown: String,
    /// Whether it holds the seat now. An alias or a queue can outlive one.
    pub seated: bool,
    /// Its aliases on the committee: freed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<SenatorName>,
    /// Messages queued for it there: dropped.
    #[serde(default)]
    pub deliveries: usize,
    /// This machine still keeps a delivery cursor or a queue for it there
    /// (what `senate status --all` lists): forgotten.
    #[serde(default, skip_serializing_if = "is_false")]
    pub followed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<Role>,
}

/// One open thread to archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadPlan {
    pub root: Seq,
    /// The root's event id: pins the thread across log epochs.
    pub id: EventId,
    /// The last seq in it when planned. One that moved since is left.
    pub updated: Seq,
    pub cause: Cause,
    /// A hosted permission notice: closed as the notice's own close is.
    #[serde(default, skip_serializing_if = "is_false")]
    pub notice: bool,
    /// A task thread, which the person archives in their own name. Any
    /// other root the server lets only the Speaker's office close.
    #[serde(default, skip_serializing_if = "is_false")]
    pub task: bool,
    pub kind: String,
    pub from: String,
    pub title: String,
    pub last_at: DateTime<Utc>,
}

/// What is left of a committee once the run is done.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Remaining {
    pub seats: usize,
    /// Seats whose senator is alive.
    pub live: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chair: Option<String>,
    #[serde(default)]
    pub chair_live: bool,
}

/// One committee's part of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteePlan {
    pub committee: CommitteeId,
    #[serde(default, skip_serializing_if = "is_false")]
    pub archived: bool,
    /// Dead members: unseated, aliases freed, queue dropped.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seats: Vec<SeatPlan>,
    /// Dead chairs and secretaries: the same, only when the scope
    /// includes roles, and after the members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<SeatPlan>,
    /// Dead but active within `ended_for`: left, and never acted on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent: Vec<SeatPlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<ThreadPlan>,
    #[serde(default)]
    pub remaining: Remaining,
}

impl CommitteePlan {
    pub fn is_empty(&self) -> bool {
        self.seats.is_empty()
            && self.roles.is_empty()
            && self.recent.is_empty()
            && self.threads.is_empty()
    }
}

/// One local session record to forget: it stays in History, and is no
/// longer listed, attached to or restored as it stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPlan {
    pub session_id: String,
    pub senator: SenatorName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committee: Option<CommitteeId>,
    /// It ran in a terminal Senate hosts.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hosted: bool,
    /// Recorded stale, not ended.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale: bool,
}

/// What one run would do, as derived at `derived_at` for `person` on
/// `senate`, whose log was in `epoch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub scope: Scope,
    pub senate: String,
    pub person: Name,
    pub epoch: String,
    pub derived_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committees: Vec<CommitteePlan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionPlan>,
}

/// The sums of a plan: what the approval prompt and the last line say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    pub senators: usize,
    /// Places acted on: a seat, or what one left behind.
    pub places: usize,
    pub seats: usize,
    pub aliases: usize,
    pub deliveries: usize,
    pub threads: BTreeMap<Cause, usize>,
    pub sessions: usize,
}

impl Totals {
    pub fn threads(&self) -> usize {
        self.threads.values().sum()
    }

    pub fn is_empty(&self) -> bool {
        self.places + self.threads() + self.sessions == 0
    }
}

impl Plan {
    /// The seats a run acts on in `committee`: the members, then the role
    /// holders when the scope includes them.
    pub fn acted<'a>(&self, committee: &'a CommitteePlan) -> impl Iterator<Item = &'a SeatPlan> {
        let roles: &[SeatPlan] = if self.scope.include_roles {
            &committee.roles
        } else {
            &[]
        };
        committee.seats.iter().chain(roles)
    }

    pub fn totals(&self) -> Totals {
        let mut totals = Totals {
            sessions: self.sessions.len(),
            ..Totals::default()
        };
        let mut senators = BTreeSet::new();
        for committee in &self.committees {
            for seat in self.acted(committee) {
                senators.insert(&seat.senator);
                totals.places += 1;
                totals.seats += usize::from(seat.seated);
                totals.aliases += seat.aliases.len();
                totals.deliveries += seat.deliveries;
            }
            for thread in &committee.threads {
                *totals.threads.entry(thread.cause).or_default() += 1;
            }
        }
        senators.extend(self.sessions.iter().map(|session| &session.senator));
        totals.senators = senators.len();
        totals
    }

    /// One line for the approval prompt, which shows 110 characters.
    pub fn summary(&self) -> String {
        let totals = self.totals();
        format!(
            "Senate cleanup: unseat {}, free {} aliases, drop {} queued, archive {} threads, forget {} sessions",
            totals.seats,
            totals.aliases,
            totals.deliveries,
            totals.threads(),
            totals.sessions
        )
    }

    /// What a run does: the part of this plan, as the person approved it,
    /// that `fresh` (derived again just now) still holds. The second value
    /// lists what was approved and no longer holds, one line each; the
    /// third counts what `fresh` holds that was never shown.
    pub fn agreed(&self, fresh: &Plan) -> (Plan, Vec<String>, usize) {
        let mut dropped = Vec::new();
        let mut new = 0;
        let mut committees = Vec::new();
        for approved in &self.committees {
            let now = fresh
                .committees
                .iter()
                .find(|c| c.committee == approved.committee);
            let c = &approved.committee;
            let seats = |approved: &[SeatPlan], now: &[SeatPlan], dropped: &mut Vec<String>| {
                let mut kept = Vec::new();
                for seat in approved {
                    match now.iter().find(|s| s.senator == seat.senator) {
                        // As it is now: an alias given since is not freed,
                        // and one gone already is not asked for.
                        Some(now) => {
                            let mut now = now.clone();
                            now.aliases.retain(|alias| seat.aliases.contains(alias));
                            kept.push(now);
                        }
                        None => dropped.push(format!("{c}: {}", seat.shown)),
                    }
                }
                kept
            };
            let (now_seats, now_roles, now_threads) = now
                .map_or((&[][..], &[][..], &[][..]), |c| {
                    (&c.seats[..], &c.roles[..], &c.threads[..])
                });
            let members = seats(&approved.seats, now_seats, &mut dropped);
            let roles = seats(&approved.roles, now_roles, &mut dropped);
            let mut threads = Vec::new();
            for thread in &approved.threads {
                // One that moved since it was shown (a new event, another
                // cause) is not the thread the person approved.
                if now_threads.contains(thread) {
                    threads.push(thread.clone());
                } else {
                    dropped.push(format!("{c}: #{}", thread.root));
                }
            }
            new += now_seats.len() + now_roles.len() + now_threads.len();
            new -= members.len() + roles.len() + threads.len();
            committees.push(CommitteePlan {
                committee: c.clone(),
                archived: approved.archived,
                seats: members,
                roles,
                recent: Vec::new(),
                threads,
                remaining: now.map(|c| c.remaining.clone()).unwrap_or_default(),
            });
        }
        for now in &fresh.committees {
            if !self.committees.iter().any(|c| c.committee == now.committee) {
                new += now.seats.len() + now.roles.len() + now.threads.len();
            }
        }
        let mut sessions = Vec::new();
        for session in &self.sessions {
            if fresh.sessions.contains(session) {
                sessions.push(session.clone());
            } else {
                dropped.push(format!("session {}", session.session_id));
            }
        }
        new += fresh.sessions.len() - sessions.len();
        let agreed = Plan {
            scope: self.scope.clone(),
            senate: self.senate.clone(),
            person: self.person.clone(),
            epoch: self.epoch.clone(),
            derived_at: self.derived_at,
            committees,
            sessions,
        };
        (agreed, dropped, new)
    }
}

/// What a run did on one committee.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteeDone {
    pub committee: Option<CommitteeId>,
    #[serde(default)]
    pub unseated: usize,
    #[serde(default)]
    pub aliases: usize,
    #[serde(default)]
    pub deliveries: usize,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub threads: BTreeMap<Cause, usize>,
}

/// Something a run did not do, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotDone {
    pub what: String,
    pub why: String,
}

/// What a run did. A second run finishes what is in `failed`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committees: Vec<CommitteeDone>,
    #[serde(default)]
    pub sessions: usize,
    /// Refused by the server, or failed here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<NotDone>,
    /// Approved, but no longer true when the run started: left alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dropped: Vec<String>,
    /// True now, but never shown: left for the next run.
    #[serde(default)]
    pub new: usize,
}

impl Report {
    pub fn committee(&mut self, committee: &CommitteeId) -> &mut CommitteeDone {
        let at = self
            .committees
            .iter()
            .position(|c| c.committee.as_ref() == Some(committee))
            .unwrap_or_else(|| {
                self.committees.push(CommitteeDone {
                    committee: Some(committee.clone()),
                    ..CommitteeDone::default()
                });
                self.committees.len() - 1
            });
        &mut self.committees[at]
    }

    pub fn fail(&mut self, what: impl Into<String>, why: impl Into<String>) {
        self.failed.push(NotDone {
            what: what.into(),
            why: why.into(),
        });
    }
}

/// `1h`, `90m`, `2d`, `45s`, `0s`: a whole number and a unit.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: u64 = number.parse().map_err(|_| {
        format!("`{text}` is not a duration: write a number and a unit, as in 1h, 90m, 2d or 0s")
    })?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => {
            return Err(format!(
                "`{text}` needs a unit: s, m, h, d or w (as in 1h, 90m, 2d)"
            ));
        }
    };
    number
        .checked_mul(seconds)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("`{text}` is too long a time"))
}

/// A whole number of the largest unit that fits: `3d`, `5h`, `12m`, `40s`.
pub fn short_duration(seconds: u64) -> String {
    match seconds {
        s if s >= 24 * 60 * 60 => format!("{}d", s / (24 * 60 * 60)),
        s if s >= 60 * 60 => format!("{}h", s / (60 * 60)),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    short_duration(u64::try_from(now.signed_duration_since(at).num_seconds()).unwrap_or(0))
}

/// Names in a row: all of them with `all`, else the first few and a count.
fn names(seats: &[&SeatPlan], all: bool) -> String {
    const SHOWN: usize = 8;
    let shown = if all { seats.len() } else { SHOWN };
    let mut out = seats
        .iter()
        .take(shown)
        .map(|seat| seat.shown.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    if seats.len() > shown {
        let _ = write!(out, ", +{}", seats.len() - shown);
    }
    out
}

fn causes(threads: &BTreeMap<Cause, usize>) -> String {
    threads
        .iter()
        .map(|(cause, count)| format!("{count} {}", cause.as_str()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The plan as the person reads it, grouped by committee. `all` lists
/// every name and every thread; otherwise long rows are cut with a count.
pub fn render_plan(plan: &Plan, locale: Locale, all: bool, now: DateTime<Utc>) -> String {
    let mut out = String::new();
    let scope = &plan.scope;
    let ended_for = short_duration(scope.ended_for_secs);
    let _ = writeln!(
        out,
        "{}",
        locale.pick(
            format!("Senate {} 的清理计划（{}）", plan.senate, plan.person),
            format!(
                "Cleanup plan for the Senate {} ({})",
                plan.senate, plan.person
            ),
        )
    );
    for committee in &plan.committees {
        if committee.is_empty() {
            continue;
        }
        let left = &committee.remaining;
        let chair = match (&left.chair, left.chair_live) {
            (Some(chair), true) => locale.pick(
                format!("主席 {chair} 在线"),
                format!("chair {chair} is live"),
            ),
            (Some(chair), false) => locale.pick(
                format!("主席 {chair} 不在线"),
                format!("chair {chair} is not live"),
            ),
            (None, _) => locale.pick("没有主席", "no chair").to_owned(),
        };
        let archived = if committee.archived {
            locale.pick("，已归档", ", archived")
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "\n{}{archived}  ({})",
            committee.committee,
            locale.pick(
                format!("剩 {} 个席位，{} 个在线；{chair}", left.seats, left.live),
                format!("{} seat(s) remain, {} live; {chair}", left.seats, left.live),
            )
        );
        let row = |out: &mut String, label: String, seats: &[SeatPlan]| {
            let seated: Vec<&SeatPlan> = seats.iter().filter(|s| s.seated).collect();
            if !seated.is_empty() {
                let _ = writeln!(out, "  {label} {}: {}", seated.len(), names(&seated, all));
            }
            let aliases: Vec<String> = seats
                .iter()
                .flat_map(|s| s.aliases.iter().map(ToString::to_string))
                .collect();
            if !aliases.is_empty() {
                let shown = if all { aliases.len() } else { 8 };
                let mut list = aliases[..aliases.len().min(shown)].join(", ");
                if aliases.len() > shown {
                    let _ = write!(list, ", +{}", aliases.len() - shown);
                }
                let _ = writeln!(
                    out,
                    "  {} {}: {list}",
                    locale.pick("释放别名", "free aliases"),
                    aliases.len()
                );
            }
            let deliveries: usize = seats.iter().map(|s| s.deliveries).sum();
            if deliveries > 0 {
                let _ = writeln!(
                    out,
                    "  {} {deliveries}",
                    locale.pick("丢弃排队的消息", "drop queued messages")
                );
            }
        };
        row(
            &mut out,
            locale.pick("除席", "unseat").to_owned(),
            &committee.seats,
        );
        if scope.include_roles {
            row(
                &mut out,
                locale
                    .pick("除席（主席、secretary）", "unseat (chair, secretary)")
                    .to_owned(),
                &committee.roles,
            );
        } else if !committee.roles.is_empty() {
            let roles: Vec<&SeatPlan> = committee.roles.iter().collect();
            let _ = writeln!(
                out,
                "  {} {}: {}",
                locale.pick(
                    "不在线的主席或 secretary（--include-roles 才移除）",
                    "dead chair or secretary (removed only with --include-roles)"
                ),
                roles.len(),
                names(&roles, all)
            );
        }
        if !committee.recent.is_empty() {
            let recent: Vec<&SeatPlan> = committee.recent.iter().collect();
            let _ = writeln!(
                out,
                "  {} {}: {}",
                locale.pick(
                    format!("{ended_for} 内还活动过，这次不动（--ended-for 0s 才包括）"),
                    format!(
                        "active within {ended_for}, left this time (--ended-for 0s takes them)"
                    ),
                ),
                recent.len(),
                names(&recent, all)
            );
        }
        if !committee.threads.is_empty() {
            let mut by_cause = BTreeMap::new();
            for thread in &committee.threads {
                *by_cause.entry(thread.cause).or_default() += 1;
            }
            let _ = writeln!(
                out,
                "  {} {}: {}",
                locale.pick("归档讨论串", "archive threads"),
                committee.threads.len(),
                causes(&by_cause)
            );
            let shown = if all { committee.threads.len() } else { 5 };
            for thread in committee.threads.iter().take(shown) {
                let _ = writeln!(
                    out,
                    "    #{} {} · {} · {} · {} · {}",
                    thread.root,
                    thread.cause.as_str(),
                    thread.kind,
                    thread.from,
                    age(thread.last_at, now),
                    thread.title
                );
            }
            if committee.threads.len() > shown {
                let _ = writeln!(
                    out,
                    "    +{} ({})",
                    committee.threads.len() - shown,
                    locale.pick("-v 全部列出", "-v lists them all")
                );
            }
        }
    }
    let totals = plan.totals();
    out.push('\n');
    if totals.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            locale.pick("没有可清理的。", "Nothing to clean up.")
        );
    } else {
        let hosted = plan.sessions.iter().filter(|s| s.hosted).count();
        let stale = plan.sessions.iter().filter(|s| s.stale).count();
        let threads = if totals.threads.is_empty() {
            String::new()
        } else {
            format!(" ({})", causes(&totals.threads))
        };
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                format!(
                    "合计：{} 位参议员；除席 {}，释放别名 {}，丢弃排队消息 {}，归档讨论串 {}{threads}，遗忘会话记录 {}（托管 {hosted}，stale {stale}）。",
                    totals.senators,
                    totals.seats,
                    totals.aliases,
                    totals.deliveries,
                    totals.threads(),
                    totals.sessions
                ),
                format!(
                    "In all: {} senator(s); unseat {}, free {} alias(es), drop {} queued message(s), archive {} thread(s){threads}, forget {} session record(s) ({hosted} hosted, {stale} stale).",
                    totals.senators,
                    totals.seats,
                    totals.aliases,
                    totals.deliveries,
                    totals.threads(),
                    totals.sessions
                ),
            )
        );
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                "保留：消息日志、会话历史（senate session history / show）、对话记录和搜索。被遗忘的会话不再列出，也不能直接恢复；需要时先 `senate senator seat <id> --committee <c>`，再 `senate session restore <id>`。",
                "Kept: the message log, session history (senate session history / show), transcripts and search. A forgotten session is no longer listed and is not restored as it stands; to bring one back, `senate senator seat <id> --committee <c>`, then `senate session restore <id>`.",
            )
        );
    }
    let roles: usize = plan.committees.iter().map(|c| c.roles.len()).sum();
    if roles > 0 && !scope.include_roles {
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                format!("另有 {roles} 个不在线的主席或 secretary 席位未动；--include-roles 一并移除。"),
                format!("{roles} seat(s) of dead chairs or secretaries are left; --include-roles removes them too."),
            )
        );
    }
    let recent: usize = plan.committees.iter().map(|c| c.recent.len()).sum();
    if recent > 0 {
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                format!("另有 {recent} 个席位在 {ended_for} 内还活动过，未动；--ended-for 0s 一并移除。"),
                format!("{recent} seat(s) were active within {ended_for} and are left; --ended-for 0s takes them too."),
            )
        );
    }
    let empty: Vec<String> = plan
        .committees
        .iter()
        .filter(|c| !c.archived && c.remaining.live == 0)
        .map(|c| c.committee.to_string())
        .collect();
    if !empty.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                format!(
                    "清理后没有在线席位的委员会（不会被移除）：{}。要隐藏它们：senate committees archive {}",
                    empty.join(", "),
                    empty.join(" ")
                ),
                format!(
                    "Committees with no live seat (none is removed): {}. To hide them: senate committees archive {}",
                    empty.join(", "),
                    empty.join(" ")
                ),
            )
        );
    }
    out
}

/// What a run did, as the person reads it.
pub fn render_report(report: &Report, locale: Locale) -> String {
    let mut out = String::new();
    let mut sum = CommitteeDone::default();
    for done in &report.committees {
        sum.unseated += done.unseated;
        sum.aliases += done.aliases;
        sum.deliveries += done.deliveries;
        for (cause, count) in &done.threads {
            *sum.threads.entry(*cause).or_default() += count;
        }
        let threads: usize = done.threads.values().sum();
        if done.unseated + done.aliases + done.deliveries + threads == 0 {
            continue;
        }
        let name = done
            .committee
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                format!(
                    "{name}：除席 {}，释放别名 {}，丢弃排队消息 {}，归档讨论串 {threads}",
                    done.unseated, done.aliases, done.deliveries
                ),
                format!(
                    "{name}: unseated {}, freed {} alias(es), dropped {} queued message(s), archived {threads} thread(s)",
                    done.unseated, done.aliases, done.deliveries
                ),
            )
        );
    }
    let threads: usize = sum.threads.values().sum();
    let by_cause = if sum.threads.is_empty() {
        String::new()
    } else {
        format!(" ({})", causes(&sum.threads))
    };
    let _ = writeln!(
        out,
        "{}",
        locale.pick(
            format!(
                "完成：除席 {}，释放别名 {}，丢弃排队消息 {}，归档讨论串 {threads}{by_cause}，遗忘会话记录 {}。",
                sum.unseated, sum.aliases, sum.deliveries, report.sessions
            ),
            format!(
                "Done: unseated {}, freed {} alias(es), dropped {} queued message(s), archived {threads} thread(s){by_cause}, forgot {} session record(s).",
                sum.unseated, sum.aliases, sum.deliveries, report.sessions
            ),
        )
    );
    if !report.dropped.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                format!(
                    "计划里有 {} 项在执行时已不成立，未动：{}",
                    report.dropped.len(),
                    report.dropped.join("; ")
                ),
                format!(
                    "{} item(s) of the plan no longer held when it ran, and were left: {}",
                    report.dropped.len(),
                    report.dropped.join("; ")
                ),
            )
        );
    }
    if !report.failed.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                format!("有 {} 项没有完成：", report.failed.len()),
                format!("{} item(s) were not done:", report.failed.len()),
            )
        );
        for failed in &report.failed {
            let _ = writeln!(out, "  {}: {}", failed.what, failed.why);
        }
    }
    if report.new > 0 || !report.failed.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            locale.pick(
                format!(
                    "再运行一次 `senate cleanup` 可完成剩下的（计划之后新出现 {} 项）。",
                    report.new
                ),
                format!(
                    "Run `senate cleanup` again to finish the rest ({} new item(s) appeared after the plan).",
                    report.new
                ),
            )
        );
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn now() -> DateTime<Utc> {
        "2026-11-26T12:00:00Z".parse().unwrap()
    }

    /// A coding session's senator the person runs, quiet for a day.
    fn dead() -> Facts {
        Facts {
            session_name: true,
            owned: true,
            last_active: Some(now() - chrono::Duration::days(1)),
            ..Facts::default()
        }
    }

    fn of(facts: Facts) -> Standing {
        standing(&facts, &Scope::default(), now())
    }

    #[test]
    fn a_senator_with_every_session_ended_long_ago_is_dead() {
        assert_eq!(of(dead()), Standing::Dead);
        // No sign of it at all: quiet for ever.
        assert_eq!(
            of(Facts {
                last_active: None,
                ..dead()
            }),
            Standing::Dead
        );
    }

    #[test]
    fn a_person_the_office_a_named_agent_and_another_persons_agent_are_never_judged() {
        for facts in [
            // A person, or an agent under a name a later session takes again.
            Facts {
                session_name: false,
                ..dead()
            },
            // The Speaker (an agent holding the office) or the built-in session.
            Facts {
                office: true,
                ..dead()
            },
            // Somebody else's agent.
            Facts {
                owned: false,
                ..dead()
            },
        ] {
            assert_eq!(of(facts.clone()), Standing::Exempt, "{facts:?}");
            // Nothing the person passes changes that.
            let everything = Scope {
                include_roles: true,
                ended_for_secs: 0,
                ..Scope::default()
            };
            assert_eq!(standing(&facts, &everything, now()), Standing::Exempt);
        }
    }

    #[test]
    fn a_live_paused_or_connected_senator_is_alive_whatever_else_is_true() {
        for facts in [
            Facts {
                live: true,
                ..dead()
            },
            Facts {
                paused: true,
                ..dead()
            },
            Facts {
                online: true,
                ..dead()
            },
        ] {
            assert_eq!(of(facts.clone()), Standing::Alive, "{facts:?}");
            // A chair or secretary with a live session is never removed,
            // with the flag or without it.
            let chair = Facts {
                role: true,
                ..facts
            };
            let with_roles = Scope {
                include_roles: true,
                ended_for_secs: 0,
                ..Scope::default()
            };
            assert_eq!(standing(&chair, &with_roles, now()), Standing::Alive);
        }
    }

    #[test]
    fn a_dead_chair_or_secretary_goes_only_when_roles_are_included() {
        let chair = Facts {
            role: true,
            ..dead()
        };
        assert_eq!(of(chair.clone()), Standing::Role);
        let with_roles = Scope {
            include_roles: true,
            ..Scope::default()
        };
        assert_eq!(standing(&chair, &with_roles, now()), Standing::Dead);
    }

    #[test]
    fn recent_means_only_the_wait_is_in_the_way() {
        let recent = Facts {
            last_active: Some(now() - chrono::Duration::minutes(10)),
            ..dead()
        };
        assert_eq!(of(recent.clone()), Standing::Recent);
        let at_once = Scope {
            ended_for_secs: 0,
            ..Scope::default()
        };
        assert_eq!(standing(&recent, &at_once, now()), Standing::Dead);
        // Exactly as old as asked is old enough.
        let exactly = Facts {
            last_active: Some(now() - chrono::Duration::hours(1)),
            ..dead()
        };
        assert_eq!(of(exactly), Standing::Dead);
        // One that is kept, or holds a role, is not listed as waiting:
        // `--ended-for 0s` would not take it.
        assert_eq!(
            of(Facts {
                kept: true,
                ..recent.clone()
            }),
            Standing::Kept
        );
        assert_eq!(
            of(Facts {
                role: true,
                ..recent
            }),
            Standing::Role
        );
    }

    #[test]
    fn a_kept_senator_stays_even_with_roles_included() {
        let kept = Facts {
            kept: true,
            role: true,
            ..dead()
        };
        let with_roles = Scope {
            include_roles: true,
            ended_for_secs: 0,
            ..Scope::default()
        };
        assert_eq!(standing(&kept, &with_roles, now()), Standing::Kept);
    }

    fn senator(name: &str) -> Address {
        Address::senator(&name.parse::<SenatorName>().unwrap())
    }

    fn gone(name: &SenatorName) -> bool {
        name.as_str().starts_with("claude-dead")
    }

    fn thread<'a>(from: &'a Address, to: &'a [Address]) -> ThreadFacts<'a> {
        ThreadFacts {
            notice: false,
            tracked: false,
            from,
            on_behalf_of: None,
            to,
            waits_on_agents: true,
            waits_on_person: false,
            last_at: now() - chrono::Duration::days(3),
        }
    }

    #[test]
    fn a_thread_that_waits_on_a_person_is_theirs_whoever_else_is_gone() {
        let to = [senator("claude-dead-1")];
        let asker = senator("claude-dead-2");
        // Escalated to a person: two dead senators' thread all the same,
        // and not archived for that.
        let escalated = ThreadFacts {
            waits_on_agents: false,
            waits_on_person: true,
            ..thread(&asker, &to)
        };
        assert_eq!(thread_cause(&escalated, gone, None, now()), None);
        let alice = senator("alice");
        let taken = ThreadFacts {
            waits_on_person: true,
            ..thread(&alice, &to)
        };
        assert_eq!(thread_cause(&taken, gone, None, now()), None);
        // Only the person's own word for old threads takes it.
        let day = Some(Duration::from_secs(24 * 60 * 60));
        assert_eq!(
            thread_cause(&escalated, gone, day, now()),
            Some(Cause::Stale)
        );
    }

    #[test]
    fn a_question_to_senators_who_are_all_gone_is_archived() {
        let alice = senator("alice");
        let to = [senator("claude-dead-1"), senator("claude-dead-2")];
        assert_eq!(
            thread_cause(&thread(&alice, &to), gone, None, now()),
            Some(Cause::AddresseeGone)
        );
        // One of them still has a seat: it may answer.
        let to = [senator("claude-dead-1"), senator("claude-live-1")];
        assert_eq!(thread_cause(&thread(&alice, &to), gone, None, now()), None);
        // The chair, the Speaker or a person among them can still act.
        for other in [Address::Chair, Address::Speaker, senator("bob")] {
            let to = [senator("claude-dead-1"), other];
            assert_eq!(thread_cause(&thread(&alice, &to), gone, None, now()), None);
        }
        // Addressed to nobody: nothing says who must act.
        assert_eq!(thread_cause(&thread(&alice, &[]), gone, None, now()), None);
    }

    #[test]
    fn a_settled_thread_goes_only_when_its_asker_is_gone_too() {
        let to = [senator("claude-dead-1")];
        let alice = senator("alice");
        let settled = ThreadFacts {
            waits_on_agents: false,
            ..thread(&alice, &to)
        };
        // A person asked and was answered: theirs to keep.
        assert_eq!(thread_cause(&settled, gone, None, now()), None);
        let asker = senator("claude-dead-2");
        let settled = ThreadFacts {
            waits_on_agents: false,
            ..thread(&asker, &to)
        };
        assert_eq!(
            thread_cause(&settled, gone, None, now()),
            Some(Cause::AllGone)
        );
        // Written by the Speaker on a live agent's behalf: the asker lives.
        let live = senator("claude-live-1");
        let relayed = ThreadFacts {
            waits_on_agents: false,
            on_behalf_of: Some(&live),
            ..thread(&Address::Speaker, &to)
        };
        assert_eq!(thread_cause(&relayed, gone, None, now()), None);
    }

    #[test]
    fn a_notice_closes_when_its_session_is_gone_and_never_while_followed() {
        let chair = [Address::Chair];
        let dead = senator("claude-dead-1");
        let notice = ThreadFacts {
            notice: true,
            waits_on_agents: false,
            ..thread(&dead, &chair)
        };
        assert_eq!(
            thread_cause(&notice, gone, None, now()),
            Some(Cause::SessionEnded)
        );
        // A live session's notice: only for being old, and only when the
        // daemon no longer follows its prompt.
        let live = senator("claude-live-1");
        let notice = ThreadFacts {
            notice: true,
            waits_on_agents: false,
            ..thread(&live, &chair)
        };
        let day = Some(Duration::from_secs(24 * 60 * 60));
        assert_eq!(thread_cause(&notice, gone, None, now()), None);
        assert_eq!(thread_cause(&notice, gone, day, now()), Some(Cause::Stale));
        let followed = ThreadFacts {
            tracked: true,
            ..notice
        };
        assert_eq!(thread_cause(&followed, gone, day, now()), None);
    }

    #[test]
    fn stale_needs_the_persons_word_and_the_time() {
        let alice = senator("alice");
        let to = [senator("claude-live-1")];
        let open = thread(&alice, &to);
        assert_eq!(thread_cause(&open, gone, None, now()), None);
        let week = Some(Duration::from_secs(7 * 24 * 60 * 60));
        assert_eq!(thread_cause(&open, gone, week, now()), None);
        let day = Some(Duration::from_secs(24 * 60 * 60));
        assert_eq!(thread_cause(&open, gone, day, now()), Some(Cause::Stale));
        // A cause that names who is gone wins over the time.
        let to = [senator("claude-dead-1")];
        assert_eq!(
            thread_cause(&thread(&alice, &to), gone, day, now()),
            Some(Cause::AddresseeGone)
        );
    }

    fn seat(name: &str, aliases: &[&str]) -> SeatPlan {
        SeatPlan {
            senator: name.parse().unwrap(),
            shown: name.to_owned(),
            seated: true,
            aliases: aliases.iter().map(|a| a.parse().unwrap()).collect(),
            deliveries: 0,
            followed: false,
            last_active: None,
            roles: Vec::new(),
        }
    }

    fn open_thread(root: u64, updated: u64) -> ThreadPlan {
        ThreadPlan {
            root: Seq(root),
            id: EventId::derived(root, "thread"),
            updated: Seq(updated),
            cause: Cause::AddresseeGone,
            notice: false,
            task: true,
            kind: "question".into(),
            from: "alice".into(),
            title: "t".into(),
            last_at: now(),
        }
    }

    fn plan(seats: Vec<SeatPlan>, threads: Vec<ThreadPlan>, sessions: &[&str]) -> Plan {
        Plan {
            scope: Scope::default(),
            senate: "/acme".into(),
            person: Name::new("alice").unwrap(),
            epoch: "e1".into(),
            derived_at: now(),
            committees: vec![CommitteePlan {
                committee: CommitteeId::new("ledger").unwrap(),
                archived: false,
                seats,
                roles: Vec::new(),
                recent: Vec::new(),
                threads,
                remaining: Remaining::default(),
            }],
            sessions: sessions
                .iter()
                .map(|id| SessionPlan {
                    session_id: (*id).to_owned(),
                    senator: "claude-dead-1".parse().unwrap(),
                    committee: None,
                    hosted: true,
                    stale: false,
                })
                .collect(),
        }
    }

    #[test]
    fn a_run_does_what_was_approved_and_still_holds_and_nothing_else() {
        let approved = plan(
            vec![
                seat("claude-dead-1", &["reviewer"]),
                seat("claude-dead-2", &[]),
            ],
            vec![open_thread(4, 9), open_thread(12, 12)],
            &["s1", "s2"],
        );
        let fresh = plan(
            vec![
                // Given a second alias since: that one was never shown.
                seat("claude-dead-1", &["reviewer", "lead"]),
                // claude-dead-2 came back: no longer in the plan.
                // A third died since: never shown.
                seat("claude-dead-3", &[]),
            ],
            // Thread 4 got a new event; 12 is as it was; 20 is new.
            vec![open_thread(4, 10), open_thread(12, 12), open_thread(20, 20)],
            &["s1", "s3"],
        );
        let (agreed, dropped, new) = approved.agreed(&fresh);
        let committee = &agreed.committees[0];
        assert_eq!(committee.seats, [seat("claude-dead-1", &["reviewer"])]);
        assert_eq!(committee.threads, [open_thread(12, 12)]);
        assert_eq!(
            agreed
                .sessions
                .iter()
                .map(|s| s.session_id.as_str())
                .collect::<Vec<_>>(),
            ["s1"]
        );
        assert_eq!(
            dropped,
            ["ledger: claude-dead-2", "ledger: #4", "session s2"]
        );
        // claude-dead-3, thread 4 as it is now, thread 20, session s3.
        assert_eq!(new, 4);
        // The same plan again changes nothing.
        let (again, dropped, new) = approved.agreed(&approved);
        assert_eq!(again, approved);
        assert!(dropped.is_empty());
        assert_eq!(new, 0);
    }

    #[test]
    fn roles_count_only_when_the_scope_includes_them() {
        let mut plan = plan(vec![seat("claude-dead-1", &["a"])], Vec::new(), &[]);
        plan.committees[0].roles = vec![seat("claude-dead-9", &["lead"])];
        assert_eq!(plan.totals().seats, 1);
        assert_eq!(plan.totals().aliases, 1);
        plan.scope.include_roles = true;
        assert_eq!(plan.totals().seats, 2);
        assert_eq!(plan.totals().aliases, 2);
        assert_eq!(plan.totals().senators, 2);
    }

    #[test]
    fn the_plan_says_what_stays_and_what_a_wider_run_would_add() {
        let mut plan = plan(
            vec![seat("claude-dead-1", &["reviewer"])],
            vec![open_thread(4, 9)],
            &["s1"],
        );
        let ledger = &mut plan.committees[0];
        ledger.seats[0].deliveries = 3;
        ledger.roles = vec![seat("claude-dead-9", &[])];
        ledger.recent = vec![seat("claude-dead-7", &[])];
        ledger.remaining = Remaining {
            seats: 2,
            live: 0,
            chair: Some("lead".into()),
            chair_live: false,
        };
        let text = render_plan(&plan, Locale::En, false, now());
        for line in [
            "ledger  (2 seat(s) remain, 0 live; chair lead is not live)",
            "  unseat 1: claude-dead-1",
            "  free aliases 1: reviewer",
            "  drop queued messages 3",
            "  dead chair or secretary (removed only with --include-roles) 1: claude-dead-9",
            "  active within 1h, left this time (--ended-for 0s takes them) 1: claude-dead-7",
            "  archive threads 1: 1 addressee_gone",
            "In all: 1 senator(s); unseat 1, free 1 alias(es), drop 3 queued message(s), archive 1 thread(s) (1 addressee_gone), forget 1 session record(s) (1 hosted, 0 stale).",
            "1 seat(s) of dead chairs or secretaries are left; --include-roles removes them too.",
            "1 seat(s) were active within 1h and are left; --ended-for 0s takes them too.",
            "Committees with no live seat (none is removed): ledger. To hide them: senate committees archive ledger",
        ] {
            assert!(text.contains(line), "missing {line:?} in\n{text}");
        }
        // With roles included they are unseated like the rest, and said so.
        plan.scope.include_roles = true;
        let text = render_plan(&plan, Locale::En, false, now());
        assert!(
            text.contains("  unseat (chair, secretary) 1: claude-dead-9"),
            "{text}"
        );
        assert!(!text.contains("--include-roles"), "{text}");
        // A committee with nothing to do is not printed, and an empty plan
        // says so.
        let mut empty = plan.clone();
        empty.sessions.clear();
        let ledger = &mut empty.committees[0];
        (ledger.seats, ledger.roles, ledger.recent, ledger.threads) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let text = render_plan(&empty, Locale::En, false, now());
        assert!(text.contains("Nothing to clean up."), "{text}");
        assert!(!text.contains("ledger  ("), "{text}");
    }

    #[test]
    fn the_approval_prompt_fits_its_line() {
        let mut big = plan(Vec::new(), Vec::new(), &[]);
        big.committees[0].seats = (0..2000)
            .map(|n| seat(&format!("claude-dead-{n}"), &["a", "b", "c", "d", "e"]))
            .collect();
        big.committees[0].threads = (1..=5000).map(|n| open_thread(n, n)).collect();
        let summary = big.summary();
        assert_eq!(
            summary,
            "Senate cleanup: unseat 2000, free 10000 aliases, drop 0 queued, archive 5000 threads, forget 0 sessions"
        );
        assert!(summary.len() <= 110);
    }

    #[test]
    fn durations_read_as_written() {
        assert_eq!(parse_duration("0s"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("90m"), Ok(Duration::from_secs(5400)));
        assert_eq!(parse_duration("1h"), Ok(Duration::from_secs(3600)));
        assert_eq!(parse_duration("2d"), Ok(Duration::from_secs(172_800)));
        assert_eq!(parse_duration("1w"), Ok(Duration::from_secs(604_800)));
        for wrong in ["", "1", "h", "1.5h", "-1h", "1 h", "1y"] {
            assert!(parse_duration(wrong).is_err(), "{wrong}");
        }
        assert_eq!(short_duration(3600), "1h");
        assert_eq!(short_duration(0), "0s");
        assert_eq!(short_duration(200_000), "2d");
    }
}
