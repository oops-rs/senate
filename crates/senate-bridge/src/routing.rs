//! Which session, if any, receives a directed event.
//!
//! The bridge seats several agents of one person, and each agent has its own
//! stream. An agent's stream decides what is pushed to that agent; delivery
//! then picks one of that agent's sessions. A message for
//! `senator::claude` therefore reaches a session that speaks as `claude`
//! (by default a Claude Code session) and never a session of `codex`, and
//! vice versa.
//!
//! 1. Only events with `senator::<the agent>` in `to`, and not written by
//!    that agent, are pushed to it. An event whose `from` or `to` holds an
//!    address form this build does not know ([`Address::Other`]) is never
//!    pushed. Everything else waits for `catch_up`. A message to the person
//!    (`senator::wendell`) is not a message to any of their agents.
//! 2. Candidates are the live sessions of that agent bound to the event's
//!    committee.
//! 3. A follow-up in a thread goes to the session that owns the thread
//!    ([`TaskOwner`](senate_core::TaskOwner)) while it is live. Otherwise one
//!    candidate gets it: among several, the ones whose cwd contains
//!    `refs.path` win; ties (or no path) go to the most recently active.
//! 4. No candidate: the event stays queued for the agent's next session
//!    that binds.
//!
//! An event is delivered to exactly one session per agent it is addressed
//! to.
//!
//! # Reassigned work ("covering")
//!
//! While `senator::copilot`'s work on a committee is reassigned to one of
//! my agents (a server-written `presence` `reassigned` with `new_owner` =
//! that agent), rule 1 also gives that agent the events directed to
//! `senator::copilot` ([`should_push_covering`]). Both sides are agents:
//! reassigning `aider`'s work to `claude` makes `claude` cover `aider` and
//! nothing else. The stored `to` is
//! never rewritten; the push says whose message it was. [`Covering`] folds
//! the presence events off each agent's stream, and is seeded from the
//! server's presence when a stream opens, so a restarted bridge still knows.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use senate_core::{
    Address, AgentPresence, CommitteeId, Event, Locale, People, PresencePayload, PresenceState,
    SenatorName,
};

use crate::session::Session;

/// Rule 1, for `me`, one of this bridge's agents.
pub fn should_push(event: &Event, me: &SenatorName) -> bool {
    let addr = Address::senator(me);
    event.is_directed_to(&addr) && event.from != addr && !names_unknown_address(event)
}

/// Rule 1 with reassigned work: also an event directed to an agent whose
/// work on the committee is `me`'s now, unless `me` or that agent wrote it.
pub fn should_push_covering(event: &Event, me: &SenatorName, covering: &Covering) -> bool {
    should_push(event, me)
        || (event.from != Address::senator(me)
            && !names_unknown_address(event)
            && covering.covered_for(event, me).is_some())
}

/// Whose work each of this bridge's agents covers, per committee (module
/// docs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Covering {
    /// (my agent, committee) → the agents whose work it has.
    by_seat: BTreeMap<(SenatorName, CommitteeId), BTreeSet<SenatorName>>,
}

impl Covering {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one event off `me`'s stream: a server `presence` reassigning
    /// work to `me` adds its agent; any later presence of that agent (a
    /// hand-back, or a reassignment to someone else) removes it.
    pub fn observe(&mut self, event: &Event, me: &SenatorName) {
        if event.from != Address::Committee {
            return;
        }
        let Some(p) = PresencePayload::of(event) else {
            return;
        };
        self.set(
            me,
            &event.committee,
            &p.agent,
            p.state,
            p.new_owner.as_ref(),
        );
    }

    /// Replace `me`'s view of a committee with the server's presence (a
    /// stream opened).
    pub fn seed(&mut self, me: &SenatorName, committee: &CommitteeId, agents: &[AgentPresence]) {
        self.by_seat.remove(&(me.clone(), committee.clone()));
        for p in agents {
            self.set(me, committee, &p.agent, p.state, p.new_owner.as_ref());
        }
    }

    fn set(
        &mut self,
        me: &SenatorName,
        committee: &CommitteeId,
        agent: &SenatorName,
        state: PresenceState,
        new_owner: Option<&SenatorName>,
    ) {
        let seat = (me.clone(), committee.clone());
        let mine = state == PresenceState::Reassigned && new_owner == Some(me) && agent != me;
        if mine {
            self.by_seat.entry(seat).or_default().insert(agent.clone());
        } else if let Some(set) = self.by_seat.get_mut(&seat) {
            set.remove(agent);
            if set.is_empty() {
                self.by_seat.remove(&seat);
            }
        }
    }

    /// Whether `me` covers `agent`'s work on `committee`.
    pub fn covers(&self, me: &SenatorName, committee: &CommitteeId, agent: &SenatorName) -> bool {
        self.by_seat
            .get(&(me.clone(), committee.clone()))
            .is_some_and(|set| set.contains(agent))
    }

    /// The agent `me` covers that `event` is directed to (and did not
    /// write), if any.
    pub fn covered_for(&self, event: &Event, me: &SenatorName) -> Option<SenatorName> {
        let set = self.by_seat.get(&(me.clone(), event.committee.clone()))?;
        event.to.iter().find_map(|a| match a.as_senator() {
            Some(agent) if set.contains(agent) && event.from != *a => Some(agent.clone()),
            _ => None,
        })
    }
}

/// `from` or one of `to` is an address form this build does not know.
pub fn names_unknown_address(event: &Event) -> bool {
    matches!(event.from, Address::Other(_))
        || event.to.iter().any(|a| matches!(a, Address::Other(_)))
}

/// Whom `ask` may address: an agent (`senator::codex`) or whichever agent
/// on the committee (`senator::*`). A question to the Speaker, the
/// committee, a person (one of `people`) or a guest would never be answered
/// by an agent, so it is refused here. `Err` is written for the asking
/// agent, in `locale`. With no `people` known, only the forms are checked.
pub fn check_ask_recipients(to: &[Address], people: &People, locale: Locale) -> Result<(), String> {
    if to.is_empty() {
        return Err(locale
            .pick(
                "`to` 没有指定要问谁；请写队友的 agent，例如 senator::codex，或 *（这个委员会上的任意一个 agent）",
                "`to` names nobody to ask; name a teammate's agent, e.g. senator::codex, or * for \
                 whichever agent is on this committee",
            )
            .to_string());
    }
    let refused: Vec<String> = to
        .iter()
        .filter(|a| !(people.is_agent_address(a) || a.is_wildcard()))
        .map(Address::in_text)
        .collect();
    if refused.is_empty() {
        return Ok(());
    }
    let refused = refused.join(", ");
    Err(match locale {
        Locale::Zh => format!(
            "`ask` 只能问队友的 agent：写 senator::<name>（例如 senator::codex），或 senator::* \
             表示这个委员会上的任意一个 agent；{refused} 不会回答。\
             Speaker 不接受提问，问题也不会直接送到某个人本人。要找人，就问他的 agent，或者告诉你的用户。"
        ),
        Locale::En => format!(
            "`ask` can only reach a teammate's agent: write senator::<name> (for example \
             senator::codex), or senator::* for whichever agent is on this committee; {refused} \
             would never answer. The Speaker does not take questions, and a question does not \
             reach a person directly. To involve a person, ask their agent or tell your user."
        ),
    })
}

/// Rule 2: live sessions of `agent` whose committee is `committee`.
pub fn candidates<'a>(
    sessions: &'a [Session],
    agent: &SenatorName,
    committee: &CommitteeId,
) -> Vec<&'a Session> {
    sessions
        .iter()
        .filter(|s| s.is_live() && &s.agent == agent && s.committee() == Some(committee))
        .collect()
}

/// Rule 3. `contains(session, path)` decides whether a session's cwd
/// contains `refs.path`; the daemon passes [`cwd_contains`].
pub fn choose<'a, F>(event: &Event, candidates: &[&'a Session], contains: F) -> Option<&'a Session>
where
    F: Fn(&Session, &str) -> bool,
{
    if let [only] = candidates {
        return Some(*only);
    }
    let path = event.refs.as_ref().and_then(|r| r.path.as_deref());
    let by_path: Vec<&'a Session> = match path {
        Some(p) => candidates
            .iter()
            .copied()
            .filter(|s| contains(s, p))
            .collect(),
        None => Vec::new(),
    };
    let pool = if by_path.is_empty() {
        candidates
    } else {
        &by_path[..]
    };
    pool.iter()
        .copied()
        .max_by_key(|s| (s.last_seen, s.started_at))
}

/// Whether `path` (repo-relative, resolved against the session's git root,
/// or absolute) names something inside the session's cwd. A relative path
/// must also exist there, which tells two checkouts of different repos apart.
/// Paths are compared canonicalized: git reports its toplevel with symlinks
/// resolved (`/private/var/…` on macOS) while a cwd may keep them.
pub fn cwd_contains(session: &Session, path: &str) -> bool {
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let cwd = canonical(&session.cwd);
    let p = Path::new(path);
    if p.is_absolute() {
        return canonical(p).starts_with(&cwd);
    }
    let base = session.git_root.as_deref().unwrap_or(&session.cwd);
    let abs = base.join(p);
    abs.exists() && canonical(&abs).starts_with(&cwd)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone, Utc};
    use senate_core::{Kind, NewEvent, Refs, Seq};

    use super::*;
    use crate::agent::AgentSource;
    use crate::session::{Runtime, SessionState};

    fn committee() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn path(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn me() -> SenatorName {
        path("copilot")
    }

    fn session(id: &str, cwd: &str, age_secs: i64) -> Session {
        let t0 = Utc.with_ymd_and_hms(2026, 10, 26, 12, 0, 0).unwrap();
        Session {
            session_id: id.into(),
            runtime: Runtime::Claude,
            agent: me(),
            agent_source: AgentSource::Harness,
            cwd: cwd.into(),
            git_root: None,
            branch: None,
            inferred_committee: Some(committee()),
            committee_source: None,
            joined_committee: None,
            socket: None,
            token: None,
            claude_pid: None,
            transcript_path: None,
            state: SessionState::Live,
            started_at: t0,
            last_seen: t0 - Duration::seconds(age_secs),
            socket_ok: None,
        }
    }

    fn event(path: Option<&str>) -> Event {
        let mut e = NewEvent::new(committee(), Kind::Question, Address::Speaker, "q")
            .to(Address::senator(&me()));
        if let Some(p) = path {
            e = e.with_refs(Refs {
                path: Some(p.into()),
                ..Refs::default()
            });
        }
        e.into_event(Seq(1), Utc::now())
    }

    #[test]
    fn pushes_only_directed_and_not_own() {
        let e = event(None);
        assert!(should_push(&e, &me()));
        let own = NewEvent::new(committee(), Kind::Note, Address::senator(&me()), "x")
            .to(Address::senator(&me()))
            .into_event(Seq(2), Utc::now());
        assert!(!should_push(&own, &me()));
        let broadcast = NewEvent::new(committee(), Kind::Note, Address::Speaker, "x")
            .to(Address::Committee)
            .into_event(Seq(3), Utc::now());
        assert!(!should_push(&broadcast, &me()));
        let odd_to = NewEvent::new(committee(), Kind::Question, Address::Speaker, "x")
            .with_to(vec![
                Address::senator(&me()),
                Address::Other("role:reviewer".into()),
            ])
            .into_event(Seq(4), Utc::now());
        assert!(!should_push(&odd_to, &me()));
        let odd_from = NewEvent::new(
            committee(),
            Kind::Question,
            Address::Other("group:oc_1".into()),
            "x",
        )
        .to(Address::senator(&me()))
        .into_event(Seq(5), Utc::now());
        assert!(!should_push(&odd_from, &me()));
    }

    #[test]
    fn a_message_for_one_agent_is_not_for_the_persons_other_agents() {
        let to_claude = event(None);
        assert!(should_push(&to_claude, &path("copilot")));
        assert!(!should_push(&to_claude, &path("cline")));
        // A message to the person is not a message to any of their agents.
        let to_person = NewEvent::new(committee(), Kind::Question, Address::Speaker, "q")
            .to(addr("senator::alice"))
            .into_event(Seq(2), Utc::now());
        assert!(!should_push(&to_person, &path("copilot")));
        // One agent writing to the other is a push to the other.
        let between = NewEvent::new(committee(), Kind::Note, addr("senator::cline"), "n")
            .to(addr("senator::copilot"))
            .into_event(Seq(3), Utc::now());
        assert!(should_push(&between, &path("copilot")));
        assert!(!should_push(&between, &path("cline")));
    }

    fn reassigned(agent: &str, owner: Option<&str>, seq: u64) -> Event {
        let payload = PresencePayload {
            agent: path(agent),
            state: if owner.is_some() {
                PresenceState::Reassigned
            } else {
                PresenceState::Online
            },
            since: Utc::now() + Duration::milliseconds(seq as i64),
            since_seq: Seq(seq),
            previous: None,
            last_seen: None,
            cursor: None,
            lag: None,
            new_owner: owner.map(path),
        };
        senate_core::presence_event(committee(), &payload).into_event(Seq(seq), Utc::now())
    }

    #[test]
    fn reassigned_work_is_pushed_to_the_new_owner_until_handed_back() {
        let bob = path("devin");
        let to_alice = || {
            NewEvent::new(committee(), Kind::Question, Address::Speaker, "q")
                .to(Address::senator(&me()))
                .into_event(Seq(10), Utc::now())
        };
        let mut covering = Covering::new();
        assert!(!should_push_covering(&to_alice(), &bob, &covering));

        covering.observe(&reassigned("copilot", Some("devin"), 5), &bob);
        assert!(covering.covers(&bob, &committee(), &me()));
        assert!(should_push_covering(&to_alice(), &bob, &covering));
        assert_eq!(covering.covered_for(&to_alice(), &bob), Some(me()));
        // Keyed by agent on both sides: bob's other agent covers nothing,
        // and alice's other agent is not covered.
        assert!(!should_push_covering(
            &to_alice(),
            &path("aider"),
            &covering
        ));
        let to_alice_codex = NewEvent::new(committee(), Kind::Question, Address::Speaker, "q")
            .to(addr("senator::cline"))
            .into_event(Seq(14), Utc::now());
        assert!(!should_push_covering(&to_alice_codex, &bob, &covering));
        // Not what I wrote, nor what alice wrote to herself.
        let mine = NewEvent::new(committee(), Kind::Answer, Address::senator(&bob), "a")
            .to(Address::senator(&me()))
            .into_event(Seq(11), Utc::now());
        assert!(!should_push_covering(&mine, &bob, &covering));
        let hers = NewEvent::new(committee(), Kind::Note, Address::senator(&me()), "n")
            .to(Address::senator(&me()))
            .into_event(Seq(12), Utc::now());
        assert!(!should_push_covering(&hers, &bob, &covering));
        // Another committee is not covered.
        let elsewhere = NewEvent::new(
            CommitteeId::new("other").unwrap(),
            Kind::Question,
            Address::Speaker,
            "q",
        )
        .to(Address::senator(&me()))
        .into_event(Seq(13), Utc::now());
        assert!(!should_push_covering(&elsewhere, &bob, &covering));
        // Alice's own bridge is unaffected by a reassignment to bob.
        let mut hers_view = Covering::new();
        hers_view.observe(&reassigned("copilot", Some("devin"), 5), &me());
        assert_eq!(hers_view, Covering::new());
        // A senator's forged presence does not count.
        let mut forged = reassigned("copilot", Some("devin"), 6);
        forged.from = Address::senator(&bob);
        let mut fresh = Covering::new();
        fresh.observe(&forged, &bob);
        assert_eq!(fresh, Covering::new());

        // Handed back.
        covering.observe(&reassigned("copilot", None, 20), &bob);
        assert!(!should_push_covering(&to_alice(), &bob, &covering));

        // Seeded from the server's view.
        let page = vec![senate_core::AgentPresence::from_payload(
            &PresencePayload::of(&reassigned("copilot", Some("devin"), 30)).unwrap(),
            Some(Seq(30)),
        )];
        covering.seed(&bob, &committee(), &page);
        assert!(should_push_covering(&to_alice(), &bob, &covering));
        covering.seed(&bob, &committee(), &[]);
        assert_eq!(covering, Covering::new());
    }

    #[test]
    fn only_agents_can_be_asked() {
        let en = Locale::En;
        let people =
            People::new(["alice", "bob", "pm"].map(|h| senate_core::Name::new(h).unwrap()));
        assert!(check_ask_recipients(&[Address::senator(&me())], &people, en).is_ok());
        assert!(check_ask_recipients(&[addr("senator::*")], &people, en).is_ok());
        assert!(check_ask_recipients(&[], &people, en).is_err());
        let refused = [
            Address::senator(&me()),
            Address::Speaker,
            addr("senator::pm"),
            addr("guest::chat/u_1"),
        ];
        let err = check_ask_recipients(&refused, &people, en).unwrap_err();
        assert!(
            err.contains("[speaker], [senator::pm], [guest::chat/u_1] would never answer"),
            "{err}"
        );
        let zh = check_ask_recipients(&refused, &people, Locale::Zh).unwrap_err();
        assert!(zh.contains("只能问队友的 agent"), "{zh}");
        assert!(zh.contains("[senator::pm]"), "{zh}");
        // A name is flat: who is a person is the Senate's people's to say.
        // With none known, only the forms are checked.
        assert!(check_ask_recipients(&[addr("senator::pm")], &People::default(), en).is_ok());
        assert!(check_ask_recipients(&[Address::Speaker], &People::default(), en).is_err());
    }

    #[test]
    fn candidates_are_the_agents_own_live_bound_sessions() {
        let mut unbound = session("u", "/u", 0);
        unbound.inferred_committee = None;
        let mut stale = session("s", "/s", 0);
        stale.state = SessionState::Stale;
        let mut other = session("o", "/o", 0);
        other.joined_committee = Some(CommitteeId::new("other").unwrap());
        let mut codex = session("cx", "/cx", 0);
        codex.runtime = Runtime::Codex;
        codex.agent = path("cline");
        let ok = session("ok", "/ok", 0);
        let all = [unbound, stale, other, codex, ok];
        let ids = |agent: &str| -> Vec<String> {
            candidates(&all, &path(agent), &committee())
                .iter()
                .map(|s| s.session_id.clone())
                .collect()
        };
        assert_eq!(ids("copilot"), ["ok"]);
        assert_eq!(ids("cline"), ["cx"]);
        assert!(ids("reviewer").is_empty());
    }

    #[test]
    fn single_candidate_wins_regardless_of_path() {
        let a = session("a", "/a", 100);
        let pick = choose(&event(Some("x.rs")), &[&a], |_, _| false).unwrap();
        assert_eq!(pick.session_id, "a");
        assert!(choose(&event(None), &[], |_, _| true).is_none());
    }

    #[test]
    fn path_beats_recency_and_recency_breaks_ties() {
        let old = session("old", "/repo-b", 100);
        let new = session("new", "/repo-a", 1);
        let cands = [&old, &new];
        let contains = |s: &Session, _: &str| s.cwd == Path::new("/repo-b");
        assert_eq!(
            choose(&event(Some("src/idem.rs")), &cands, contains)
                .unwrap()
                .session_id,
            "old"
        );
        assert_eq!(
            choose(&event(None), &cands, contains).unwrap().session_id,
            "new"
        );
        // A path nobody contains falls back to recency.
        assert_eq!(
            choose(&event(Some("nope.rs")), &cands, |_, _| false)
                .unwrap()
                .session_id,
            "new"
        );
    }

    #[test]
    fn cwd_contains_resolves_against_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("gateway/src")).unwrap();
        std::fs::write(root.join("gateway/src/idem.rs"), "").unwrap();
        let mut s = session("s", root.join("gateway").to_str().unwrap(), 0);
        s.git_root = Some(root.to_path_buf());
        assert!(cwd_contains(&s, "gateway/src/idem.rs"));
        assert!(!cwd_contains(&s, "gateway/src/missing.rs"));
        s.cwd = root.join("other");
        assert!(!cwd_contains(&s, "gateway/src/idem.rs"));
        assert!(cwd_contains(&s, root.join("other/x").to_str().unwrap()));
    }
}
