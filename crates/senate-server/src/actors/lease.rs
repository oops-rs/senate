//! Leases: the presence state machine of one committee, kept
//! by its [`CommitteeActor`](super::committee::CommitteeActor).
//!
//! Each agent's *activity* is the later of its bridge's last heartbeat and
//! the opening of its newest open stream ([`Presence::last_activity`]). From
//! how long it has been quiet:
//!
//! - quiet < `disconnected` → `online` (from any state: a heartbeat brings an
//!   agent back);
//! - `online` and quiet ≥ `disconnected` → `disconnected`;
//! - `online` / `disconnected` and quiet ≥ `lease` → `absent`;
//! - `taken_over` comes only from [`Leases::take_over`] (the Speaker), from
//!   `disconnected` or `absent`;
//! - `reassigned` comes only from [`Leases::reassign`] (the Speaker, for a
//!   person who confirmed it), from any state, and stands until the same
//!   call hands the work back (`online`): activity does not end it.
//!
//! [`Leases::decide`] says which transition is due and [`Leases::apply`]
//! records it once the `presence` event is committed, so a failed append is
//! simply decided again on the next tick.
//!
//! An agent's *first* `online` is not written: every bridge that connects
//! would otherwise add an event, and "online" is what an agent is until it
//! is not. The server holds that state ([`AgentPresence::seq`] is `None`);
//! the first event an agent gets is its first `disconnected`, and `online`
//! is written when it comes back from being away.
//!
//! # Restarts
//!
//! A new writer [`restore`](Leases::restore)s each agent from the stored
//! facts and the clock now: the later of its last logged contact
//! ([`AgentPresence::last_seen`]) and its last stored heartbeat is when it was
//! last heard, and the time the server was down counts as quiet like any
//! other. The state machine then runs as usual (the writer decides once at
//! start-up, not a tick later), so an agent last heard longer ago than
//! `disconnected` is written `disconnected`, and longer ago than `lease`
//! `absent`. Such a transition is dated to the moment it fell due (last
//! contact plus the threshold), not to the restart, so its derived event id
//! is the same however often the writer restarts and it is stored once. An
//! agent heard within `disconnected` stays `online` and nothing is written.
//! An agent online but never written is restored the same way
//! ([`Leases::restore_unlogged`]), from the store's record of its last
//! heartbeat.
//!
//! [`Presence::last_activity`]: super::presence::Presence::last_activity

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use senate_core::{AgentPresence, PresenceBoard, PresencePayload, PresenceState, SenatorName, Seq};

/// The two thresholds, from the server config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LeaseTiming {
    pub disconnected: Duration,
    pub lease: Duration,
}

impl LeaseTiming {
    /// How often a committee's writer re-evaluates its leases: a quarter of
    /// the shorter threshold, between 50 ms and 15 s.
    pub(crate) fn tick(&self) -> Duration {
        (self.disconnected / 4).clamp(Duration::from_millis(50), Duration::from_secs(15))
    }
}

#[derive(Debug, Clone)]
struct Agent {
    /// `None` until the first transition is committed.
    state: Option<PresenceState>,
    since: DateTime<Utc>,
    since_seq: Seq,
    seq: Option<Seq>,
    /// Latest activity, observed or restored.
    last_seen: Option<Instant>,
    last_seen_wall: Option<DateTime<Utc>>,
    cursor: Option<Seq>,
    new_owner: Option<SenatorName>,
    /// Restored from stored facts, with no activity seen since: a due
    /// transition is dated to when it fell due, so it has the same event id
    /// on every restart.
    restored: bool,
}

impl Agent {
    fn new(wall: DateTime<Utc>) -> Self {
        Self {
            state: None,
            since: wall,
            since_seq: Seq::ZERO,
            seq: None,
            last_seen: None,
            last_seen_wall: None,
            cursor: None,
            new_owner: None,
            restored: false,
        }
    }
}

pub(crate) struct Leases {
    timing: LeaseTiming,
    agents: BTreeMap<SenatorName, Agent>,
}

/// The monotonic instant of the wall time `at`, as of `now`. A time before
/// the monotonic clock's origin (a host booted since) is as old as it can be
/// said to be.
fn ago(now: Instant, wall: DateTime<Utc>, at: DateTime<Utc>) -> Instant {
    let quiet = (wall - at).to_std().unwrap_or_default();
    now.checked_sub(quiet).unwrap_or(now)
}

fn lag(head: Seq, cursor: Option<Seq>) -> Option<u64> {
    cursor.map(|c| head.0.saturating_sub(c.0))
}

impl Leases {
    pub(crate) fn new(timing: LeaseTiming) -> Self {
        Self {
            timing,
            agents: BTreeMap::new(),
        }
    }

    pub(crate) fn timing(&self) -> LeaseTiming {
        self.timing
    }

    /// Take over the log's latest state per agent, at `now` (see the module
    /// docs on restarts). `beats` are the agents' last stored heartbeats.
    pub(crate) fn restore(
        &mut self,
        board: &PresenceBoard,
        beats: &BTreeMap<SenatorName, DateTime<Utc>>,
        now: Instant,
        wall: DateTime<Utc>,
    ) {
        for p in board.agents() {
            let heard = match p.state {
                PresenceState::Online | PresenceState::Disconnected => {
                    let logged = p.last_seen.unwrap_or(p.since);
                    Some(beats.get(&p.agent).map_or(logged, |b| logged.max(*b)))
                }
                // Only activity moves these.
                PresenceState::Absent | PresenceState::TakenOver | PresenceState::Reassigned => {
                    None
                }
            };
            self.agents.insert(
                p.agent.clone(),
                Agent {
                    state: Some(p.state),
                    since: p.since,
                    since_seq: p.since_seq,
                    seq: p.seq,
                    last_seen: heard.map(|at| ago(now, wall, at)),
                    last_seen_wall: heard.or(p.last_seen),
                    cursor: p.cursor,
                    new_owner: p.new_owner.clone(),
                    restored: heard.is_some(),
                },
            );
        }
    }

    /// An agent with no logged presence whose bridge last heartbeated at
    /// `last_beat` (before a restart): online, unwritten, quiet since then.
    pub(crate) fn restore_unlogged(
        &mut self,
        agent: &SenatorName,
        cursor: Seq,
        last_beat: DateTime<Utc>,
        now: Instant,
        wall: DateTime<Utc>,
    ) {
        if self.agents.contains_key(agent) {
            return;
        }
        let mut entry = Agent::new(wall);
        entry.state = Some(PresenceState::Online);
        entry.last_seen = Some(ago(now, wall, last_beat));
        entry.last_seen_wall = Some(last_beat);
        entry.cursor = Some(cursor);
        entry.restored = true;
        self.agents.insert(agent.clone(), entry);
    }

    /// Agents the machine knows.
    pub(crate) fn agents(&self) -> impl Iterator<Item = &SenatorName> {
        self.agents.keys()
    }

    /// Note `agent`'s latest activity and cursor, and return the transition
    /// that is due, if any. Nothing changes state until [`Self::apply`].
    pub(crate) fn decide(
        &mut self,
        agent: &SenatorName,
        activity: Option<Instant>,
        cursor: Option<Seq>,
        now: Instant,
        wall: DateTime<Utc>,
        head: Seq,
    ) -> Option<PresencePayload> {
        if activity.is_none() && !self.agents.contains_key(agent) {
            return None;
        }
        let entry = self
            .agents
            .entry(agent.clone())
            .or_insert_with(|| Agent::new(wall));
        if let Some(at) = activity
            && entry.last_seen.is_none_or(|seen| at > seen)
        {
            entry.last_seen = Some(at);
            entry.restored = false;
            let ago =
                chrono::Duration::from_std(now.saturating_duration_since(at)).unwrap_or_default();
            entry.last_seen_wall = Some(wall - ago);
        }
        if cursor.is_some() {
            entry.cursor = cursor;
        }
        let quiet = now.saturating_duration_since(entry.last_seen?);
        let current = entry.state;
        if current == Some(PresenceState::Reassigned) {
            // Stands until it is handed back (module docs).
            return None;
        }
        let target = if quiet < self.timing.disconnected {
            if current.is_none() {
                // A first appearance: held, not written (module docs).
                entry.state = Some(PresenceState::Online);
                entry.since = wall;
                entry.since_seq = head;
                return None;
            }
            PresenceState::Online
        } else if quiet < self.timing.lease {
            match current {
                Some(PresenceState::Online) => PresenceState::Disconnected,
                _ => return None,
            }
        } else {
            match current {
                Some(PresenceState::Online | PresenceState::Disconnected) => PresenceState::Absent,
                _ => return None,
            }
        };
        // A transition that fell due while the server was down is dated to
        // then (see the module docs on restarts).
        let since = match (entry.restored, entry.last_seen_wall, target) {
            (true, Some(heard), PresenceState::Disconnected) => {
                heard + chrono::Duration::from_std(self.timing.disconnected).unwrap_or_default()
            }
            (true, Some(heard), PresenceState::Absent) => {
                heard + chrono::Duration::from_std(self.timing.lease).unwrap_or_default()
            }
            _ => wall,
        };
        (current != Some(target)).then(|| PresencePayload {
            agent: agent.clone(),
            state: target,
            since,
            since_seq: head,
            previous: current,
            last_seen: entry.last_seen_wall,
            cursor: entry.cursor,
            lag: lag(head, entry.cursor),
            new_owner: None,
        })
    }

    /// The Speaker picked up for `agent`: the `taken_over` transition, or
    /// `None` when it already is; an error when it is not away.
    pub(crate) fn take_over(
        &self,
        agent: &SenatorName,
        wall: DateTime<Utc>,
        head: Seq,
    ) -> Result<Option<PresencePayload>, String> {
        let Some(entry) = self.agents.get(agent).filter(|a| a.state.is_some()) else {
            return Err(format!("{agent} has no presence on this committee"));
        };
        match entry.state {
            Some(PresenceState::TakenOver) => Ok(None),
            Some(PresenceState::Disconnected | PresenceState::Absent) => {
                Ok(Some(PresencePayload {
                    agent: agent.clone(),
                    state: PresenceState::TakenOver,
                    since: wall,
                    since_seq: head,
                    previous: entry.state,
                    last_seen: entry.last_seen_wall,
                    cursor: entry.cursor,
                    lag: lag(head, entry.cursor),
                    new_owner: None,
                }))
            }
            Some(other) => Err(format!(
                "{agent} is {other}; only an away agent is taken over"
            )),
            None => unreachable!("filtered above"),
        }
    }

    /// A person hands `agent`'s work to `new_owner` (or back to the agent
    /// itself): the transition, `None` when it already is so, an error when
    /// refused ([`senate_core::reassign_transition`]).
    pub(crate) fn reassign(
        &self,
        agent: &SenatorName,
        new_owner: &SenatorName,
        wall: DateTime<Utc>,
        head: Seq,
    ) -> Result<Option<PresencePayload>, String> {
        senate_core::reassign_transition(&self.view(head), head, agent, new_owner, wall)
            .map_err(|refused| refused.to_string())
    }

    /// Record a committed transition.
    pub(crate) fn apply(&mut self, payload: &PresencePayload, seq: Seq) {
        let entry = self
            .agents
            .entry(payload.agent.clone())
            .or_insert_with(|| Agent::new(payload.since));
        entry.state = Some(payload.state);
        entry.since = payload.since;
        entry.since_seq = payload.since_seq;
        entry.seq = Some(seq);
        entry.new_owner = payload.new_owner.clone();
        entry.restored = false;
    }

    /// Every agent with a state, lag measured against `head`.
    pub(crate) fn view(&self, head: Seq) -> Vec<AgentPresence> {
        self.agents
            .iter()
            .filter_map(|(agent, a)| {
                Some(AgentPresence {
                    agent: agent.clone(),
                    state: a.state?,
                    since: a.since,
                    since_seq: a.since_seq,
                    seq: a.seq,
                    last_seen: a.last_seen_wall,
                    cursor: a.cursor,
                    lag: lag(head, a.cursor),
                    new_owner: a.new_owner.clone(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: LeaseTiming = LeaseTiming {
        disconnected: Duration::from_secs(120),
        lease: Duration::from_secs(900),
    };

    fn path(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn alice() -> SenatorName {
        path("alice/claude")
    }

    struct Clock {
        start: Instant,
        wall: DateTime<Utc>,
    }

    impl Clock {
        fn at(&self, secs: u64) -> (Instant, DateTime<Utc>) {
            (
                self.start + Duration::from_secs(secs),
                self.wall + chrono::Duration::seconds(secs as i64),
            )
        }
    }

    /// Decide and, if due, apply; the state after.
    fn step(
        l: &mut Leases,
        c: &Clock,
        secs: u64,
        activity: Option<u64>,
        seq: &mut u64,
    ) -> Option<PresenceState> {
        let (now, wall) = c.at(secs);
        let activity = activity.map(|a| c.at(a).0);
        let p = l.decide(&alice(), activity, Some(Seq(3)), now, wall, Seq(10))?;
        *seq += 1;
        l.apply(&p, Seq(*seq));
        Some(p.state)
    }

    #[test]
    fn online_disconnected_absent_and_back() {
        let c = Clock {
            start: Instant::now(),
            wall: Utc::now(),
        };
        let mut l = Leases::new(T);
        let mut seq = 0;
        assert_eq!(step(&mut l, &c, 0, None, &mut seq), None, "unknown, quiet");
        assert_eq!(
            step(&mut l, &c, 0, Some(0), &mut seq),
            None,
            "the first online is held, not written"
        );
        let first = l.view(Seq(10));
        assert_eq!(first[0].state, PresenceState::Online);
        assert_eq!(first[0].seq, None);

        assert_eq!(
            step(&mut l, &c, 60, Some(60), &mut seq),
            None,
            "still online"
        );
        assert_eq!(step(&mut l, &c, 179, None, &mut seq), None);
        assert_eq!(
            step(&mut l, &c, 180, None, &mut seq),
            Some(PresenceState::Disconnected)
        );
        assert_eq!(step(&mut l, &c, 500, None, &mut seq), None);
        assert_eq!(
            step(&mut l, &c, 960, None, &mut seq),
            Some(PresenceState::Absent)
        );
        assert_eq!(step(&mut l, &c, 2000, None, &mut seq), None, "absent stays");

        let (_, wall) = c.at(2000);
        let taken = l.take_over(&alice(), wall, Seq(10)).unwrap().unwrap();
        assert_eq!(taken.previous, Some(PresenceState::Absent));
        assert_eq!(taken.lag, Some(7));
        l.apply(&taken, Seq(99));
        assert_eq!(l.take_over(&alice(), wall, Seq(10)), Ok(None));
        assert_eq!(step(&mut l, &c, 2100, None, &mut seq), None);
        assert_eq!(
            step(&mut l, &c, 2200, Some(2200), &mut seq),
            Some(PresenceState::Online),
            "a heartbeat brings it back"
        );
        assert!(l.take_over(&alice(), wall, Seq(10)).is_err(), "online");
        let view = l.view(Seq(20));
        assert_eq!(view[0].lag, Some(17));
        assert_eq!(view[0].state, PresenceState::Online);
    }

    #[test]
    fn a_reassignment_stands_through_heartbeats_until_handed_back() {
        let c = Clock {
            start: Instant::now(),
            wall: Utc::now(),
        };
        let mut l = Leases::new(T);
        let mut seq = 0;
        step(&mut l, &c, 0, Some(0), &mut seq);
        let bob = path("bob/claude");
        let (_, wall) = c.at(10);
        let moved = l.reassign(&alice(), &bob, wall, Seq(10)).unwrap().unwrap();
        assert_eq!(moved.state, PresenceState::Reassigned);
        assert_eq!(moved.previous, Some(PresenceState::Online));
        l.apply(&moved, Seq(50));
        assert_eq!(l.reassign(&alice(), &bob, wall, Seq(10)), Ok(None));
        assert_eq!(step(&mut l, &c, 20, Some(20), &mut seq), None, "heartbeat");
        assert_eq!(step(&mut l, &c, 5_000, None, &mut seq), None, "silence");
        assert_eq!(l.view(Seq(60))[0].new_owner, Some(bob));
        let back = l
            .reassign(&alice(), &alice(), wall, Seq(10))
            .unwrap()
            .unwrap();
        assert_eq!(back.state, PresenceState::Online);
        l.apply(&back, Seq(51));
        assert_eq!(l.view(Seq(60))[0].new_owner, None);
        assert_eq!(
            step(&mut l, &c, 5_001, None, &mut seq),
            Some(PresenceState::Absent),
            "the lease machine carries on"
        );
    }

    #[test]
    fn a_restart_counts_the_downtime_and_dates_what_fell_due_then() {
        let c = Clock {
            start: Instant::now() + Duration::from_secs(10_000),
            wall: Utc::now(),
        };
        let mut board = PresenceBoard::new();
        let bob = path("bob/claude");
        let carol = path("carol/claude");
        let committee = senate_core::CommitteeId::new("f").unwrap();
        let mk = |agent: &SenatorName, state, ago: i64| PresencePayload {
            agent: agent.clone(),
            state,
            since: c.wall - chrono::Duration::seconds(ago),
            since_seq: Seq(1),
            previous: None,
            last_seen: Some(c.wall - chrono::Duration::seconds(ago + 120)),
            cursor: None,
            lag: None,
            new_owner: None,
        };
        for (i, p) in [
            mk(&alice(), PresenceState::Online, 3_000),
            mk(&bob, PresenceState::Disconnected, 600),
            mk(&carol, PresenceState::Absent, 5_000),
        ]
        .iter()
        .enumerate()
        {
            board.observe(
                &senate_core::presence_event(committee.clone(), p)
                    .into_event(Seq(i as u64 + 1), c.wall),
            );
        }
        let mut l = Leases::new(T);
        let (start, wall) = c.at(0);
        // Alice's store heartbeat is newer than her logged contact: only
        // that counts.
        let beats = BTreeMap::from([(alice(), c.wall - chrono::Duration::seconds(30))]);
        l.restore(&board, &beats, start, wall);
        assert_eq!(
            l.decide(&alice(), None, None, start, wall, Seq(3)),
            None,
            "heard 30 s ago"
        );
        assert_eq!(l.decide(&carol, None, None, start, wall, Seq(3)), None);
        // Bob was quiet for 720 s: still within the lease, nothing is due.
        assert_eq!(l.decide(&bob, None, None, start, wall, Seq(3)), None);
        // Alice goes quiet from her heartbeat on, as if the server had run.
        let (now, wall) = c.at(89);
        assert_eq!(l.decide(&alice(), None, None, now, wall, Seq(3)), None);
        let (now, wall) = c.at(90);
        let due = l.decide(&alice(), None, None, now, wall, Seq(3)).unwrap();
        assert_eq!(due.state, PresenceState::Disconnected);
        assert_eq!(due.since, c.wall - chrono::Duration::seconds(30 - 120));
        // Bob's lease runs out 180 s after the restart, dated then.
        let (now, wall) = c.at(179);
        assert_eq!(l.decide(&bob, None, None, now, wall, Seq(3)), None);
        let (now, wall) = c.at(181);
        let due = l.decide(&bob, None, None, now, wall, Seq(3)).unwrap();
        assert_eq!(due.state, PresenceState::Absent);
        assert_eq!(due.since, c.wall + chrono::Duration::seconds(180));

        // A restart long after: what is overdue is due at once, dated to
        // when it fell due, so every restart derives the same event.
        let mut late = Leases::new(T);
        late.restore(&board, &BTreeMap::new(), start, wall);
        let a = late
            .decide(&alice(), None, None, start, wall, Seq(3))
            .unwrap();
        assert_eq!(a.state, PresenceState::Absent);
        assert_eq!(a.since, c.wall - chrono::Duration::seconds(3_120 - 900));
        let mut again = Leases::new(T);
        again.restore(&board, &BTreeMap::new(), start, wall);
        let (_, later) = c.at(60);
        let b = again
            .decide(&alice(), None, None, c.at(60).0, later, Seq(3))
            .unwrap();
        assert_eq!(a.since, b.since);
        assert_eq!(
            senate_core::presence_event(committee.clone(), &a).id,
            senate_core::presence_event(committee.clone(), &b).id
        );
    }
}
