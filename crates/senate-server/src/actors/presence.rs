//! Whether an agent is connected: it holds an open stream, and either its
//! bridge heartbeated within the grace period or that stream is younger than
//! it.
//!
//! An open stream alone is not enough. A laptop that goes to sleep sends no
//! FIN, so its SSE response stays "open" on the server, which keeps writing
//! pings into the socket for minutes. Its bridge stops heartbeating, though,
//! so once the grace period passes without a heartbeat the agent counts as
//! gone, even before the server notices the dead socket. A stream younger than
//! the grace period counts by itself, so a bridge is connected from the moment
//! it opens its stream, not from its first heartbeat.
//!
//! Streams are reported by their [`AgentProxy`]s. A proxy registers a *weak*
//! handle to each stream sender it owns. A stream counts as open while its
//! proxy still owns the sender (the weak handle upgrades) and the HTTP
//! response still owns the receiver (the channel is not closed). So presence
//! goes false by itself when a client disconnects or a proxy dies — killed
//! with its crashed [`CommitteeActor`], say — without relying on any cleanup
//! message arriving, and two generations of a proxy can never overwrite each
//! other's entries: each entry is keyed by its own connection id.
//!
//! [`AgentProxy`]: super::proxy::AgentProxy
//! [`CommitteeActor`]: super::committee::CommitteeActor

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use senate_core::{CommitteeId, Event, SenatorPath, Seq};

use tokio::sync::mpsc::WeakSender;

/// Identifies one SSE connection for the life of the process.
pub(crate) type ConnId = u64;

type Key = (CommitteeId, SenatorPath);

struct Stream {
    conn: ConnId,
    sender: WeakSender<Arc<Event>>,
    opened: Instant,
}

impl Stream {
    fn is_open(&self) -> bool {
        self.sender.upgrade().is_some_and(|s| !s.is_closed())
    }
}

#[derive(Default)]
struct State {
    streams: HashMap<Key, Vec<Stream>>,
    /// When each agent's bridge last heartbeated the committee, and the
    /// delivered cursor it reported.
    heartbeats: HashMap<Key, (Instant, Seq)>,
}

pub(crate) struct Presence {
    grace: Duration,
    state: Mutex<State>,
}

impl Presence {
    pub(crate) fn new(grace: Duration) -> Self {
        Self {
            grace,
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // Each mutation is a single insert/push/retain; a poisoned map is
        // consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn add(
        &self,
        committee: &CommitteeId,
        agent: &SenatorPath,
        conn: ConnId,
        sender: WeakSender<Arc<Event>>,
    ) {
        self.add_at(committee, agent, conn, sender, Instant::now());
    }

    fn add_at(
        &self,
        committee: &CommitteeId,
        agent: &SenatorPath,
        conn: ConnId,
        sender: WeakSender<Arc<Event>>,
        now: Instant,
    ) {
        let mut state = self.lock();
        let entry = state
            .streams
            .entry((committee.clone(), agent.clone()))
            .or_default();
        entry.retain(Stream::is_open);
        entry.push(Stream {
            conn,
            sender,
            opened: now,
        });
    }

    pub(crate) fn remove(&self, committee: &CommitteeId, agent: &SenatorPath, conn: ConnId) {
        let mut state = self.lock();
        let key = (committee.clone(), agent.clone());
        if let Some(entry) = state.streams.get_mut(&key) {
            entry.retain(|s| s.conn != conn && s.is_open());
            if entry.is_empty() {
                state.streams.remove(&key);
            }
        }
    }

    /// The agent's bridge heartbeated the committee just now, reporting its
    /// delivered cursor.
    pub(crate) fn heartbeat(&self, committee: &CommitteeId, agent: &SenatorPath, cursor: Seq) {
        self.heartbeat_at(committee, agent, cursor, Instant::now());
    }

    fn heartbeat_at(
        &self,
        committee: &CommitteeId,
        agent: &SenatorPath,
        cursor: Seq,
        now: Instant,
    ) {
        self.lock()
            .heartbeats
            .insert((committee.clone(), agent.clone()), (now, cursor));
    }

    /// The later of the agent's last heartbeat and the opening of its
    /// newest open stream on the committee: what its lease runs from
    /// ([`super::lease`]). Its last reported cursor comes with it.
    pub(crate) fn last_activity(
        &self,
        committee: &CommitteeId,
        agent: &SenatorPath,
    ) -> (Option<Instant>, Option<Seq>) {
        let state = self.lock();
        let key = (committee.clone(), agent.clone());
        let opened = state
            .streams
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|s| s.is_open())
            .map(|s| s.opened)
            .max();
        let beat = state.heartbeats.get(&key).copied();
        let last = opened.max(beat.map(|(at, _)| at));
        (last, beat.map(|(_, cursor)| cursor))
    }

    /// Agents with a stream or a heartbeat on the committee.
    pub(crate) fn agents_on(&self, committee: &CommitteeId) -> Vec<SenatorPath> {
        let state = self.lock();
        let mut out: Vec<SenatorPath> = state
            .streams
            .keys()
            .chain(state.heartbeats.keys())
            .filter(|(f, _)| f == committee)
            .map(|(_, agent)| agent.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub(crate) fn is_connected(&self, committee: &CommitteeId, agent: &SenatorPath) -> bool {
        self.is_connected_at(committee, agent, Instant::now())
    }

    fn is_connected_at(&self, committee: &CommitteeId, agent: &SenatorPath, now: Instant) -> bool {
        let state = self.lock();
        let key = (committee.clone(), agent.clone());
        let Some(newest) = state
            .streams
            .get(&key)
            .into_iter()
            .flatten()
            .filter(|s| s.is_open())
            .map(|s| s.opened)
            .max()
        else {
            return false;
        };
        let fresh = |at: Instant| now.saturating_duration_since(at) <= self.grace;
        fresh(newest) || state.heartbeats.get(&key).is_some_and(|(at, _)| fresh(*at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    const GRACE: Duration = Duration::from_secs(150);

    fn path(s: &str) -> SenatorPath {
        s.parse().unwrap()
    }

    fn ids() -> (CommitteeId, SenatorPath) {
        (CommitteeId::new("refund-v2").unwrap(), path("alice/claude"))
    }

    #[test]
    fn presence_follows_sender_and_receiver_lifetimes() {
        let presence = Presence::new(GRACE);
        let (committee, alice) = ids();
        assert!(!presence.is_connected(&committee, &alice));

        let (tx, rx) = mpsc::channel::<Arc<Event>>(1);
        presence.add(&committee, &alice, 1, tx.downgrade());
        assert!(presence.is_connected(&committee, &alice));
        drop(rx);
        assert!(
            !presence.is_connected(&committee, &alice),
            "client went away"
        );

        let (tx2, _rx2) = mpsc::channel::<Arc<Event>>(1);
        presence.add(&committee, &alice, 2, tx2.downgrade());
        assert!(presence.is_connected(&committee, &alice));
        drop(tx2);
        assert!(
            !presence.is_connected(&committee, &alice),
            "proxy went away"
        );

        let (tx3, _rx3) = mpsc::channel::<Arc<Event>>(1);
        presence.add(&committee, &alice, 3, tx3.downgrade());
        presence.remove(&committee, &alice, 3);
        assert!(!presence.is_connected(&committee, &alice), "detached");
        drop(tx);
    }

    #[test]
    fn an_open_stream_needs_heartbeats_once_the_grace_passes() {
        let presence = Presence::new(GRACE);
        let (committee, alice) = ids();
        let bob = path("bob/claude");
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let connected = |f: &CommitteeId, who: &SenatorPath, secs: u64| {
            presence.is_connected_at(f, who, at(secs))
        };

        // A heartbeat without a stream is not a connection.
        presence.heartbeat_at(&committee, &alice, Seq(1), at(0));
        assert!(!connected(&committee, &alice, 0));

        let (tx, _rx) = mpsc::channel::<Arc<Event>>(1);
        presence.add_at(&committee, &alice, 1, tx.downgrade(), at(0));
        assert!(connected(&committee, &alice, 0), "a new stream counts");
        assert!(connected(&committee, &alice, 150), "for the whole grace");

        presence.heartbeat_at(&committee, &alice, Seq(1), at(100));
        presence.heartbeat_at(&committee, &bob, Seq(1), at(100));
        assert!(
            connected(&committee, &alice, 200),
            "the stream is old, the heartbeat fresh"
        );

        // The laptop sleeps: the stream stays open, the heartbeats stop.
        assert!(!connected(&committee, &alice, 251));

        // It wakes and heartbeats again on the same stream.
        presence.heartbeat_at(&committee, &alice, Seq(1), at(300));
        assert!(connected(&committee, &alice, 300));

        // A reconnect (a fresh stream) counts before its first heartbeat.
        assert!(!connected(&committee, &alice, 600));
        let (tx2, _rx2) = mpsc::channel::<Arc<Event>>(1);
        presence.add_at(&committee, &alice, 2, tx2.downgrade(), at(600));
        assert!(connected(&committee, &alice, 600));

        // Heartbeats are per agent and per committee.
        let other = CommitteeId::new("search").unwrap();
        let (tx3, _rx3) = mpsc::channel::<Arc<Event>>(1);
        presence.add_at(&other, &alice, 3, tx3.downgrade(), at(600));
        presence.heartbeat_at(&committee, &alice, Seq(1), at(800));
        assert!(connected(&committee, &alice, 800));
        assert!(!connected(&other, &alice, 800));
        assert!(!connected(&committee, &bob, 800), "bob has no stream");
    }

    #[test]
    fn two_agents_of_one_person_are_present_separately() {
        let presence = Presence::new(GRACE);
        let committee = CommitteeId::new("refund-v2").unwrap();
        let (claude, codex) = (path("alice/claude"), path("alice/codex"));
        let (tx, _rx) = mpsc::channel::<Arc<Event>>(1);
        presence.add(&committee, &claude, 1, tx.downgrade());
        assert!(presence.is_connected(&committee, &claude));
        assert!(!presence.is_connected(&committee, &codex));
        assert!(!presence.is_connected(&committee, &path("alice")));
        presence.heartbeat(&committee, &codex, Seq(4));
        assert!(!presence.is_connected(&committee, &codex), "no stream");
        assert_eq!(presence.agents_on(&committee), [claude, codex]);
    }
}
