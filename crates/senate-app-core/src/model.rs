//! What the app knows and how it changes: plain data, no window, no socket.
//!
//! [`Model`] is folded from the daemon's `watch` stream ([`Model::apply`])
//! and from the replies to the reads it triggers ([`Refresh`]). The app
//! renders it through [`crate::state::AppState`].
//!
//! # What the daemon lets a desktop client reach
//!
//! `senated --machine` serves several Senates behind one socket. A request
//! that names no session (the app has none) goes to the **primary** Senate,
//! the one where the person's own agents sit directly. So the model keeps,
//! per (agent, committee) of that Senate, the inbox the daemon reports, and
//! the other Senates the machine serves are known by their status (streams,
//! sessions) and by the docket. Codex approvals and the `watch` stream are
//! the machine's, whatever Senate a session sits in.
//!
//! [`Action`] is what the window asks for; [`Action::request`] maps it to the
//! bridge's own IPC [`Request`], so the wire format stays the bridge's.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use senate_bridge::approvals::{Approval, Decision};
use senate_bridge::ipc::{
    ApprovalsReply, ClientError, DocketReply, InboxReply, Request, SessionSelector, StatusReply,
    WatchEvent,
};
use senate_bridge::session::{Runtime, Session, SessionState};
use senate_core::{
    CommitteeId, Directed, Locale, SenatorName, Seq, SpeakerSeat, StatusPayload, StatusState,
    ThreadSummary,
};

use crate::text;

/// A read the backend should make after a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refresh {
    /// `inbox` of one agent on one committee of the primary Senate.
    Inbox {
        agent: SenatorName,
        committee: CommitteeId,
    },
    /// Every inbox of the primary Senate.
    AllInboxes,
    /// `approvals`, replacing the list.
    Approvals,
    /// `docket`: what awaits the Speaker of every Senate one of the
    /// person's agents presides over. Does nothing when none does.
    Docket,
}

/// Something the user should be told about outside the window.
#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    ApprovalArrived(Approval),
    /// Something came back to the person: an answer, a note, an escalation
    /// addressed to them. The app posts the notification itself, and the
    /// daemon stays quiet while the app is connected.
    ForPerson {
        committee: CommitteeId,
        /// The thread's root seq.
        thread: Seq,
        /// `Senate · refund-v2`.
        title: String,
        /// `[senator::claude] answered: the first line`.
        body: String,
    },
}

/// What [`Model::apply`] asks of the backend.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Effects {
    pub refresh: Vec<Refresh>,
    pub notices: Vec<Notice>,
}

/// Why the daemon cannot be reached. Kept as data and written in the app's
/// language when shown, so changing the language needs no reconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offline {
    /// Nothing listens on the socket.
    NotRunning(PathBuf),
    /// It did not answer in time.
    Timeout,
    /// An open stream ended.
    Closed,
    /// Anything else, as the client reported it.
    Other(String),
}

impl From<&ClientError> for Offline {
    fn from(e: &ClientError) -> Self {
        match e {
            ClientError::Connect { path, .. } => Self::NotRunning(path.clone()),
            ClientError::Timeout => Self::Timeout,
            other => Self::Other(message(other)),
        }
    }
}

impl Offline {
    pub fn text(&self, locale: Locale) -> String {
        match (self, locale) {
            (Self::NotRunning(p), Locale::Zh) => format!("无法连接 {}", p.display()),
            (Self::NotRunning(p), Locale::En) => format!("cannot connect to {}", p.display()),
            (Self::Timeout, l) => l
                .pick("守护进程没有及时响应", "the daemon did not answer in time")
                .to_string(),
            (Self::Closed, l) => l
                .pick("守护进程关闭了连接", "the daemon closed the connection")
                .to_string(),
            (Self::Other(m), l) => format!("{}{m}", l.pick("连接失败：", "connection failed: ")),
        }
    }
}

/// What a failed read was reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOf {
    Inbox {
        agent: SenatorName,
        committee: CommitteeId,
    },
    Approvals,
    Docket,
}

/// The last failed action or read, shown in the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// A reply with nothing in it; never sent.
    EmptyReply,
    /// The daemon refused or could not be reached, in its own words.
    Daemon(String),
    Read {
        what: ReadOf,
        message: String,
    },
}

impl Failure {
    pub fn text(&self, locale: Locale) -> String {
        match self {
            Self::EmptyReply => text::empty_reply(locale),
            Self::Daemon(m) => m.clone(),
            Self::Read { what, message } => {
                let what = match what {
                    ReadOf::Inbox { agent, committee } => text::inbox_of(
                        locale,
                        &senate_core::Address::senator(agent).in_text(),
                        committee.as_str(),
                    ),
                    ReadOf::Approvals => text::approvals_word(locale).to_string(),
                    ReadOf::Docket => text::docket_word(locale).to_string(),
                };
                text::read_failed(locale, &what, message)
            }
        }
    }
}

/// The app's picture of the daemon.
#[derive(Debug, Clone, Default)]
pub struct Model {
    /// The language texts are written in.
    pub locale: Locale,
    /// A `watch` stream is open.
    pub online: bool,
    /// Why the daemon is unreachable, when it is.
    pub offline: Option<Offline>,
    /// The status of the Senate the daemon serves.
    pub status: Option<StatusReply>,
    /// Pending approvals by id.
    pub approvals: BTreeMap<String, Approval>,
    /// The last `inbox` reply per (agent, committee) of the primary Senate.
    pub inboxes: BTreeMap<(SenatorName, CommitteeId), InboxReply>,
    /// What awaits the Speaker, while one of the person's agents presides.
    pub docket: Option<DocketReply>,
    pub last_error: Option<Failure>,
    /// Approvals already notified about, so a reconnect does not repeat them.
    notified: BTreeSet<String>,
}

/// A thread waiting on the person, across the agents whose inbox lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    pub committee: CommitteeId,
    pub thread: ThreadSummary,
    /// The person's agents the thread was addressed to.
    pub for_agents: Vec<SenatorName>,
    /// The thread was escalated to the person: it waits on them whichever
    /// agent it was for.
    pub escalated: bool,
    /// The latest event addressed to the person that they have not seen
    /// ([`ThreadSummary::waits_on_person`], with the daemon's read state):
    /// the thread is unread for them. A thread can wait on the person for
    /// this alone, though its last word is an answer.
    pub unread: Option<Directed>,
}

impl Model {
    pub fn new(locale: Locale) -> Self {
        Self {
            locale,
            ..Self::default()
        }
    }

    /// The `watch` stream opened. Its first lines restate everything, so
    /// what the last connection left is dropped.
    pub fn connected(&mut self) {
        self.online = true;
        self.offline = None;
        self.last_error = None;
        self.approvals.clear();
    }

    /// The daemon cannot be reached (or the stream broke).
    pub fn disconnected(&mut self, reason: Offline) {
        self.online = false;
        self.offline = Some(reason);
        self.approvals.clear();
    }

    /// Fold one `watch` line.
    pub fn apply(&mut self, event: WatchEvent) -> Effects {
        let mut fx = Effects::default();
        match event {
            WatchEvent::Status(status) => {
                crate::ffi::note_people(&status.people);
                self.status = Some(*status);
                let targets: BTreeSet<_> = self.targets().into_iter().collect();
                self.inboxes.retain(|k, _| targets.contains(k));
                if !self.presiding() {
                    self.docket = None;
                }
                // The first status, and every sweep (30 s), re-reads every
                // inbox and the docket: it is cheap, and it catches what the
                // stream does not announce (a committee joined, a thread that
                // aged, an item put to the Speaker).
                fx.refresh.push(Refresh::AllInboxes);
                fx.refresh.push(Refresh::Docket);
            }
            WatchEvent::ApprovalRequested(approval) => {
                let approval = *approval;
                if self.notified.insert(approval.id.clone()) {
                    fx.notices.push(Notice::ApprovalArrived(approval.clone()));
                }
                self.approvals.insert(approval.id.clone(), approval);
            }
            WatchEvent::ApprovalResolved { id, .. } => {
                self.approvals.remove(&id);
            }
            WatchEvent::Inbox {
                agent, committee, ..
            } => {
                if self.targets().contains(&(agent.clone(), committee.clone())) {
                    fx.refresh.push(Refresh::Inbox { agent, committee });
                }
                // The event may have been put to a Speaker, in any Senate.
                fx.refresh.push(Refresh::Docket);
            }
            WatchEvent::ForPerson {
                news,
                escalated,
                text,
            } => {
                let by = senate_bridge::person::byline(&news.from, news.on_behalf_of.as_ref());
                let verb = senate_bridge::person::did(&news.kind, escalated, self.locale);
                let line = text.lines().next().unwrap_or_default().trim().to_string();
                let sep = self.locale.pick("：", ": ");
                fx.notices.push(Notice::ForPerson {
                    committee: news.committee.clone(),
                    thread: news.thread.unwrap_or(news.seq),
                    title: format!("Senate · {}", news.committee),
                    body: if line.is_empty() {
                        format!("{by} {verb}")
                    } else {
                        format!("{by} {verb}{sep}{line}")
                    },
                });
                fx.refresh.push(Refresh::AllInboxes);
            }
            WatchEvent::Lagged { .. } => {
                fx.refresh.push(Refresh::Approvals);
                fx.refresh.push(Refresh::AllInboxes);
                fx.refresh.push(Refresh::Docket);
            }
        }
        fx
    }

    pub fn set_inbox(&mut self, reply: InboxReply) {
        for agent in reply.agents.clone() {
            self.inboxes
                .insert((agent, reply.committee.clone()), reply.clone());
        }
    }

    pub fn set_approvals(&mut self, reply: ApprovalsReply) {
        self.approvals = reply
            .approvals
            .into_iter()
            .map(|a| (a.id.clone(), a))
            .collect();
    }

    /// A read succeeded: forget its failure, if that is what the error bar
    /// still shows.
    pub fn clear_read(&mut self, what: &ReadOf) {
        if matches!(&self.last_error, Some(Failure::Read { what: w, .. }) if w == what) {
            self.last_error = None;
        }
    }

    pub fn set_docket(&mut self, reply: Option<DocketReply>) {
        self.docket = reply;
    }

    /// An approval this app answered: drop it now rather than wait for the
    /// stream's `approval_resolved`.
    pub fn approval_answered(&mut self, id: &str) {
        self.approvals.remove(id);
    }

    /// Every Senate the machine serves: one.
    pub fn senates(&self) -> impl Iterator<Item = &StatusReply> {
        self.status.iter()
    }

    /// The (agent, committee) streams of the primary Senate: the inboxes the
    /// app can read.
    pub fn targets(&self) -> Vec<(SenatorName, CommitteeId)> {
        self.status
            .as_ref()
            .map(|s| {
                s.committees
                    .iter()
                    .map(|c| (c.agent.clone(), c.committee.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Committees of the primary Senate the daemon holds a stream for.
    pub fn committees(&self) -> Vec<CommitteeId> {
        let set: BTreeSet<_> = self.targets().into_iter().map(|(_, c)| c).collect();
        set.into_iter().collect()
    }

    /// Whether one of the person's agents holds the Speaker's seat of a
    /// Senate this machine serves.
    pub fn presiding(&self) -> bool {
        self.senates().any(|s| presider_of(s).is_some())
    }

    /// The threads waiting on the person, merged across the agents' inboxes
    /// and across committees, longest in their state first.
    ///
    /// The daemon's `inbox` lists every thread addressed to the agent that is
    /// not archived, including those already answered (`active`); those wait
    /// on no one, so they are not listed and not counted. A reply moves its
    /// thread to `active`, so it leaves the list and the counts together. A
    /// thread escalated to the person is in every agent's reply; it is listed
    /// once.
    pub fn waiting(&self) -> Vec<Waiting> {
        let person = self.status.as_ref().map(|s| &s.person);
        let mut merged: BTreeMap<(CommitteeId, Seq), Waiting> = BTreeMap::new();
        for ((agent, committee), reply) in &self.inboxes {
            for t in &reply.threads {
                // The agents' rule, or the person's own: something said to
                // them that they have not seen.
                let wait = reply.waits_on_person(t);
                if !is_waiting(t) && wait.is_none() {
                    continue;
                }
                let unread = wait.and_then(|w| w.unread).cloned();
                let entry = merged
                    .entry((committee.clone(), t.root))
                    .or_insert_with(|| Waiting {
                        committee: committee.clone(),
                        thread: t.clone(),
                        for_agents: Vec::new(),
                        escalated: person.is_some_and(|p| is_escalated_to(t, p)),
                        unread,
                    });
                if t.is_for(agent) && !entry.for_agents.contains(agent) {
                    entry.for_agents.push(agent.clone());
                }
            }
        }
        let mut all: Vec<Waiting> = merged.into_values().collect();
        all.sort_by_key(|w| w.thread.since_ts);
        all
    }

    /// How many of [`Model::waiting`] are unread for the person.
    pub fn unread_threads(&self) -> usize {
        self.waiting().iter().filter(|w| w.unread.is_some()).count()
    }

    /// How many threads [`Model::waiting`] lists.
    pub fn pending_threads(&self) -> usize {
        self.waiting().len()
    }

    /// Items awaiting the Speaker, across the Senates the person presides
    /// over. They wait on the presiding agent, not on the person.
    pub fn docket_items(&self) -> usize {
        self.docket
            .iter()
            .flat_map(|d| &d.senates)
            .flat_map(|s| &s.pages)
            .map(|p| p.items.len())
            .sum()
    }

    /// Live and stale Codex sessions, in every Senate.
    pub fn codex_sessions(&self) -> Vec<&Session> {
        self.senates()
            .flat_map(|s| &s.sessions)
            .filter(|x| x.runtime == Runtime::Codex && x.state != SessionState::Ended)
            .collect()
    }
}

/// The agent holding the Speaker's seat of the Senate `status` describes,
/// when it is an agent (the person's own: a machine's agents are its
/// person's).
pub fn presider_of(status: &StatusReply) -> Option<&SenatorName> {
    match &status.speaker {
        Some(SpeakerSeat::Senator(p)) if !status.people.is_person(p) => Some(p),
        _ => None,
    }
}

/// What the window asks the backend to do on the daemon.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Inbox {
        agent: SenatorName,
        committee: CommitteeId,
    },
    /// Reply in a thread as the person (`senator::<person>`).
    Answer {
        committee: CommitteeId,
        thread: Seq,
        body: String,
    },
    /// Take the thread over (`/senate take`).
    Take {
        committee: CommitteeId,
        thread: Seq,
    },
    /// Archive the thread, as the person.
    Close {
        committee: CommitteeId,
        thread: Seq,
    },
    /// The person has seen the thread up to `seen`: read state on this
    /// machine, nothing is appended to the log.
    MarkRead {
        committee: CommitteeId,
        thread: Seq,
        seen: Seq,
    },
    Approvals,
    Docket,
    Approve {
        id: String,
        decision: Decision,
    },
}

impl Action {
    /// The bridge IPC request for this action. The app has no session, so
    /// every committee-scoped op names its committee and an empty selector.
    pub fn request(&self) -> Result<Request, Failure> {
        let session = SessionSelector::default();
        Ok(match self.clone() {
            Action::Inbox { agent, committee } => Request::Inbox {
                session,
                committee: Some(committee),
                agent: Some(agent.name().clone()),
                all: false,
            },
            Action::Answer {
                committee,
                thread,
                body,
            } => {
                let body = body.trim().to_string();
                if body.is_empty() {
                    return Err(Failure::EmptyReply);
                }
                Request::Answer {
                    session,
                    thread,
                    body,
                    id: None,
                    committee: Some(committee),
                    agent: None,
                    as_person: true,
                }
            }
            Action::Take { committee, thread } => Request::Take {
                session,
                thread,
                id: None,
                committee: Some(committee),
            },
            Action::Close { committee, thread } => Request::Task {
                session,
                thread,
                status: StatusPayload::new(StatusState::Archived),
                id: None,
                committee: Some(committee),
                agent: None,
                as_person: true,
            },
            Action::MarkRead {
                committee,
                thread,
                seen,
            } => Request::Seen {
                committee,
                marks: vec![senate_bridge::ipc::ThreadSeen { thread, seen }],
            },
            Action::Approvals => Request::Approvals,
            Action::Docket => Request::Docket {
                session,
                committee: None,
            },
            Action::Approve { id, decision } => Request::Approve { id, decision },
        })
    }

    /// The committee whose inboxes this action changes.
    pub fn committee(&self) -> Option<&CommitteeId> {
        match self {
            Action::Inbox { committee, .. }
            | Action::Answer { committee, .. }
            | Action::Take { committee, .. }
            | Action::Close { committee, .. }
            | Action::MarkRead { committee, .. } => Some(committee),
            Action::Approvals | Action::Docket | Action::Approve { .. } => None,
        }
    }
}

/// The person the thread is escalated to now: the escalation in force, else
/// (a server too old to send it) the one a suspended thread remembers. Gone
/// once the escalation ended: a past one is not shown.
pub(crate) fn escalated_to_now(t: &ThreadSummary) -> Option<&senate_core::Name> {
    match &t.escalation {
        Some(e) => Some(&e.to),
        None if t.state == StatusState::Suspended => t.escalated_to.as_ref(),
        None => None,
    }
}

/// Whether the thread is (still) escalated to `person`.
fn is_escalated_to(t: &ThreadSummary, person: &senate_core::Name) -> bool {
    escalated_to_now(t) == Some(person)
}

/// Whether a thread from `inbox` still waits on an agent or is parked on a
/// person: the agents' rule ([`ThreadSummary::waits`]). A thread whose last
/// word is an answer is settled for them, and one closed is neither. What
/// the person has not seen is the person's rule
/// ([`InboxReply::waits_on_person`]), which [`Model::waiting`] adds.
pub fn is_waiting(thread: &ThreadSummary) -> bool {
    thread.waits().is_some()
}

/// An error as the user should read it: the daemon's message alone when it
/// refused.
pub fn message(e: &ClientError) -> String {
    match e {
        ClientError::Remote(e) => e.message.clone(),
        other => other.to_string(),
    }
}

/// The `--remote` address for `codex`: `[codex] listen` from `senate.toml`
/// when set, else the managed app-server daemon (`unix://`), as `senate
/// install` tells the developer to run it.
pub fn codex_remote(listen: Option<&str>) -> String {
    match listen.map(str::trim) {
        Some(l) if !l.is_empty() => l.to_string(),
        _ => "unix://".into(),
    }
}

/// The shell line Terminal.app runs for "open in terminal": `codex
/// --remote <addr>` in the thread's directory, resuming the thread when
/// one is given.
pub fn codex_shell_line(
    bin: &str,
    remote: &str,
    cwd: Option<&str>,
    thread: Option<&str>,
) -> String {
    let mut line = String::new();
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        line.push_str(&format!("cd {} && ", shell_quote(cwd)));
    }
    line.push_str(&shell_quote(bin));
    match thread {
        Some(id) => line.push_str(&format!(
            " resume --remote {} {}",
            shell_quote(remote),
            shell_quote(id)
        )),
        None => line.push_str(&format!(" --remote {}", shell_quote(remote))),
    }
    line
}

/// The AppleScript that opens a Terminal.app window running `shell`.
pub fn terminal_script(shell: &str) -> String {
    format!(
        "tell application \"Terminal\"\n  activate\n  do script \"{}\"\nend tell",
        applescript_escape(shell)
    )
}

/// POSIX single-quoting, leaving plain words alone.
pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use senate_bridge::approvals::ResolvedBy;
    use senate_bridge::ipc::{CommitteeStatus, StreamState};
    use senate_core::{Address, Kind, Name};

    fn committee(c: &str) -> CommitteeId {
        CommitteeId::new(c).unwrap()
    }

    fn path(p: &str) -> SenatorName {
        p.parse().unwrap()
    }

    fn status(streams: &[(&str, &str)]) -> StatusReply {
        StatusReply {
            pid: 1,
            version: "0.1.0".into(),
            person: Name::new("alice").unwrap(),
            senate: Some("alice-desk".parse().unwrap()),
            people: senate_core::People::new([
                Name::new("alice").unwrap(),
                Name::new("bob").unwrap(),
            ]),
            locale: Locale::En,
            server_url: "http://senate.test".into(),
            committees: streams
                .iter()
                .map(|(agent, c)| CommitteeStatus {
                    agent: path(agent),
                    committee: committee(c),
                    delivered: Seq(40),
                    reported: Seq(40),
                    stream: Some(StreamState::Connected { since: Utc::now() }),
                    configured: true,
                    sessions: 1,
                })
                .collect(),
            sessions: Vec::new(),
            queue: Default::default(),
            speaker: None,
        }
    }

    fn thread(root: u64, agent: &str) -> ThreadSummary {
        ThreadSummary {
            root: Seq(root),
            kind: Kind::Question,
            from: Address::Speaker,
            on_behalf_of: None,
            to: vec![Address::senator(&path(agent))],
            agents: vec![path(agent)],
            title: "why?".into(),
            state: StatusState::Suspended,
            since: Seq(root),
            since_ts: Utc::now(),
            updated: Seq(root),
            steps: Vec::new(),
            links: Vec::new(),
            owner: None,
            triage: None,
            escalated_to: None,
            takeover: None,
            reason: None,
            answered: false,
            answered_by: None,
            reopened: false,
            escalation: None,
            directed: Vec::new(),
        }
    }

    fn inbox(agent: &str, c: &str, roots: &[u64]) -> InboxReply {
        InboxReply {
            committee: committee(c),
            agents: vec![path(agent)],
            person: Name::new("alice").unwrap(),
            threads: roots.iter().map(|r| thread(*r, agent)).collect(),
            settled: 0,
            seen: Vec::new(),
        }
    }

    #[test]
    fn status_refreshes_every_inbox_and_the_docket_and_drops_left_streams() {
        let mut m = Model::default();
        m.connected();
        let fx = m.apply(WatchEvent::Status(Box::new(status(&[
            ("claude", "a"),
            ("codex", "a"),
            ("claude", "b"),
        ]))));
        assert_eq!(fx.refresh, vec![Refresh::AllInboxes, Refresh::Docket]);
        m.set_inbox(inbox("claude", "a", &[3, 5]));
        m.set_inbox(inbox("codex", "a", &[7]));
        m.set_inbox(inbox("claude", "b", &[9]));
        assert_eq!(m.pending_threads(), 4);
        assert_eq!(m.committees(), vec![committee("a"), committee("b")]);
        m.apply(WatchEvent::Status(Box::new(status(&[("claude", "a")]))));
        assert_eq!(m.pending_threads(), 2, "the other streams are gone");
    }

    #[test]
    fn answered_threads_leave_the_list_and_the_count_together() {
        let mut m = Model::default();
        m.connected();
        m.apply(WatchEvent::Status(Box::new(status(&[("claude", "a")]))));
        let mut reply = inbox("claude", "a", &[3, 5, 8]);
        reply.threads[2].state = StatusState::Processing;
        m.set_inbox(reply.clone());
        assert_eq!(m.pending_threads(), 3);
        reply.threads[0].state = StatusState::Active;
        reply.threads[0].answered = true;
        m.set_inbox(reply);
        let roots: Vec<u64> = m.waiting().iter().map(|w| w.thread.root.0).collect();
        assert_eq!(roots, vec![5, 8]);
        assert_eq!(m.pending_threads(), 2);
    }

    #[test]
    fn a_settled_thread_waits_on_no_one_until_something_is_said_after_the_answer() {
        let mut m = Model::default();
        m.connected();
        m.apply(WatchEvent::Status(Box::new(status(&[("claude", "a")]))));
        let mut reply = inbox("claude", "a", &[3, 5]);
        for t in &mut reply.threads {
            t.state = StatusState::Active;
            t.answered = true;
        }
        m.set_inbox(reply.clone());
        // The daemon leaves settled threads out, and so does the model if
        // one is handed to it anyway.
        assert_eq!(m.pending_threads(), 0);
        reply.threads[1].reopened = true;
        m.set_inbox(reply);
        let roots: Vec<u64> = m.waiting().iter().map(|w| w.thread.root.0).collect();
        assert_eq!(roots, vec![5]);
    }

    #[test]
    fn the_same_root_on_two_committees_is_two_threads() {
        let mut m = Model::default();
        m.connected();
        m.apply(WatchEvent::Status(Box::new(status(&[
            ("claude", "a"),
            ("claude", "b"),
        ]))));
        m.set_inbox(inbox("claude", "a", &[3]));
        m.set_inbox(inbox("claude", "b", &[3]));
        assert_eq!(m.pending_threads(), 2);
    }

    #[test]
    fn inbox_event_refreshes_its_stream_only_when_the_app_can_read_it() {
        let mut m = Model::default();
        m.apply(WatchEvent::Status(Box::new(status(&[("claude", "a")]))));
        let event = |agent: &str| WatchEvent::Inbox {
            agent: path(agent),
            committee: committee("a"),
            seq: Seq(9),
            kind: Kind::Question,
        };
        let fx = m.apply(event("claude"));
        assert_eq!(
            fx.refresh,
            vec![
                Refresh::Inbox {
                    agent: path("claude"),
                    committee: committee("a")
                },
                Refresh::Docket
            ]
        );
        let fx = m.apply(event("reviewer"));
        assert_eq!(fx.refresh, vec![Refresh::Docket]);
    }

    fn directed(person: &str, seq: u64, kind: Kind, escalated: bool, line: &str) -> Directed {
        Directed {
            person: Name::new(person).unwrap(),
            seq: Seq(seq),
            ts: Utc::now(),
            from: Address::senator(&path("claude")),
            on_behalf_of: None,
            kind,
            escalated,
            line: line.into(),
        }
    }

    /// An answered thread for alice's agent, with `d` addressed to alice.
    fn answered_to_alice(root: u64, d: Directed) -> ThreadSummary {
        let mut t = thread(root, "claude");
        t.state = StatusState::Active;
        t.answered = true;
        t.updated = Seq(root + 5);
        t.directed = vec![d];
        t
    }

    #[test]
    fn an_answer_the_person_has_not_seen_waits_on_them_until_they_have() {
        let mut m = Model::default();
        m.connected();
        m.apply(WatchEvent::Status(Box::new(status(&[("claude", "a")]))));
        let mut reply = inbox("claude", "a", &[]);
        reply.threads = vec![
            answered_to_alice(3, directed("alice", 4, Kind::Answer, false, "The result")),
            answered_to_alice(6, directed("alice", 8, Kind::Note, false, "FYI")),
            // Settled for the agents, nothing addressed to the person.
            {
                let mut t = answered_to_alice(9, directed("bob", 10, Kind::Answer, false, "x"));
                t.directed.clear();
                t
            },
        ];
        m.set_inbox(reply.clone());
        let w = m.waiting();
        assert_eq!(
            w.iter().map(|w| w.thread.root.0).collect::<Vec<_>>(),
            vec![3, 6],
            "settled for the agents, unread for the person"
        );
        assert!(w.iter().all(|w| w.unread.is_some() && !w.escalated));
        assert_eq!(m.unread_threads(), 2);
        assert_eq!(m.pending_threads(), 2);
        // The person reads thread 3: only the later one is left.
        reply.seen = vec![senate_bridge::ipc::ThreadSeen {
            thread: Seq(3),
            seen: Seq(4),
        }];
        m.set_inbox(reply.clone());
        let w = m.waiting();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].thread.root, Seq(6));
        assert_eq!(w[0].unread.as_ref().unwrap().line, "FYI");
        // A new event after what they saw waits again.
        reply.threads[0].directed = vec![directed("alice", 7, Kind::Answer, false, "More")];
        m.set_inbox(reply);
        assert_eq!(m.unread_threads(), 2);
    }

    #[test]
    fn an_escalation_waits_after_it_is_seen_but_is_not_news() {
        let mut m = Model::default();
        m.connected();
        m.apply(WatchEvent::Status(Box::new(status(&[("claude", "a")]))));
        let mut t = answered_to_alice(3, directed("alice", 5, Kind::Status, true, "nine things"));
        t.state = StatusState::Suspended;
        t.escalated_to = Some(Name::new("alice").unwrap());
        t.escalation = Some(senate_core::Escalation {
            to: Name::new("alice").unwrap(),
            by: Address::senator(&path("claude")),
            seq: Seq(5),
            ts: Utc::now(),
            reason: Some("nine things".into()),
        });
        let mut reply = inbox("claude", "a", &[]);
        reply.threads = vec![t];
        m.set_inbox(reply.clone());
        let w = m.waiting();
        assert!(w[0].escalated && w[0].unread.as_ref().unwrap().escalated);
        reply.seen = vec![senate_bridge::ipc::ThreadSeen {
            thread: Seq(3),
            seen: Seq(9),
        }];
        m.set_inbox(reply);
        let w = m.waiting();
        assert_eq!(w.len(), 1, "it lasts until the person acts");
        assert!(w[0].escalated && w[0].unread.is_none());
        assert_eq!(m.unread_threads(), 0);
    }

    #[test]
    fn a_thread_shows_who_it_is_escalated_to_only_while_the_escalation_is_in_force() {
        let shown = |t: &ThreadSummary| {
            let w = Waiting {
                committee: committee("a"),
                thread: t.clone(),
                for_agents: Vec::new(),
                escalated: false,
                unread: None,
            };
            crate::state::inbox_thread(&w, Locale::En).escalated_to
        };
        let alice = Name::new("alice").unwrap();
        // Before: nothing was escalated.
        let mut t = answered_to_alice(3, directed("alice", 4, Kind::Answer, false, "x"));
        assert_eq!(shown(&t), None);
        // During: the escalation in force names the person.
        t.state = StatusState::Suspended;
        t.escalated_to = Some(alice.clone());
        t.escalation = Some(senate_core::Escalation {
            to: alice,
            by: Address::senator(&path("claude")),
            seq: Seq(5),
            ts: Utc::now(),
            reason: None,
        });
        assert_eq!(shown(&t).as_deref(), Some("[senator::alice]"));
        // After: the person answered and the agent is working again. The
        // thread remembers the latest escalation, but it is not shown.
        t.escalation = None;
        t.state = StatusState::Processing;
        assert!(t.escalated_to.is_some());
        assert_eq!(shown(&t), None);
    }

    #[test]
    fn an_answer_that_crossed_an_escalation_leaves_it_new_in_the_app() {
        let mut m = Model::default();
        m.connected();
        m.apply(WatchEvent::Status(Box::new(status(&[("claude", "a")]))));
        let alice = Name::new("alice").unwrap();
        // The escalation at 5 is still in force after the person's answer at
        // 6, which carried `seen` 4; the daemon marked them seen up to 4.
        let mut t = answered_to_alice(3, directed("alice", 5, Kind::Status, true, "nine things"));
        t.state = StatusState::Suspended;
        t.escalated_to = Some(alice.clone());
        t.escalation = Some(senate_core::Escalation {
            to: alice,
            by: Address::senator(&path("claude")),
            seq: Seq(5),
            ts: Utc::now(),
            reason: Some("nine things".into()),
        });
        t.updated = Seq(6);
        let mut reply = inbox("claude", "a", &[]);
        reply.threads = vec![t];
        reply.seen = vec![senate_bridge::ipc::ThreadSeen {
            thread: Seq(3),
            seen: Seq(4),
        }];
        m.set_inbox(reply);
        let w = m.waiting();
        assert_eq!(w.len(), 1);
        assert!(w[0].escalated, "it still waits on the person");
        let news = w[0].unread.as_ref().unwrap();
        assert!(news.escalated && news.seq == Seq(5), "and is still news");
        assert_eq!(m.unread_threads(), 1);
    }

    #[test]
    fn marking_a_thread_read_is_a_seen_request() {
        let r = Action::MarkRead {
            committee: committee("a"),
            thread: Seq(3),
            seen: Seq(9),
        }
        .request()
        .unwrap();
        assert!(matches!(
            r,
            Request::Seen { committee: c, marks } if c == committee("a")
                && marks == vec![senate_bridge::ipc::ThreadSeen { thread: Seq(3), seen: Seq(9) }]
        ));
    }

    #[test]
    fn something_for_the_person_is_a_notice_in_the_apps_language_and_a_refresh() {
        for (locale, body) in [
            (Locale::En, "[senator::claude] answered: Idempotency-Key"),
            (Locale::Zh, "[senator::claude] 回答了：Idempotency-Key"),
        ] {
            let mut m = Model::new(locale);
            let event = senate_core::NewEvent::new(
                committee("a"),
                Kind::Answer,
                Address::senator(&path("claude")),
                "Idempotency-Key\nmore",
            )
            .to(Address::senior(&Name::new("alice").unwrap()))
            .in_thread(Seq(3))
            .into_event(Seq(9), Utc::now());
            let fx = m.apply(WatchEvent::ForPerson {
                news: Box::new(event),
                escalated: false,
                text: "Idempotency-Key\nmore".into(),
            });
            assert_eq!(
                fx.notices,
                vec![Notice::ForPerson {
                    committee: committee("a"),
                    thread: Seq(3),
                    title: "Senate · a".into(),
                    body: body.into(),
                }]
            );
            assert!(fx.refresh.contains(&Refresh::AllInboxes));
        }
    }

    #[test]
    fn approvals_come_and_go_and_notify_once() {
        let approval = |id: &str| Approval {
            id: id.into(),
            session_id: "thr".into(),
            kind: senate_bridge::approvals::ApprovalKind::Command,
            method: "m".into(),
            summary: "run: cargo test".into(),
            command: None,
            cwd: None,
            reason: None,
            files: Vec::new(),
            params: serde_json::json!({}),
            received_at: Utc::now(),
        };
        let mut m = Model::default();
        m.connected();
        let fx = m.apply(WatchEvent::ApprovalRequested(Box::new(approval("t:1"))));
        assert!(matches!(&fx.notices[..], [Notice::ApprovalArrived(a)] if a.id == "t:1"));
        // A reconnect restates it; no second notification.
        m.disconnected(Offline::Closed);
        assert!(m.approvals.is_empty());
        m.connected();
        let fx = m.apply(WatchEvent::ApprovalRequested(Box::new(approval("t:1"))));
        assert!(fx.notices.is_empty());
        m.apply(WatchEvent::ApprovalResolved {
            id: "t:1".into(),
            session_id: "thr".into(),
            by: ResolvedBy::Elsewhere,
            decision: None,
        });
        assert!(m.approvals.is_empty());
        let fx = m.apply(WatchEvent::Lagged { missed: 4 });
        assert_eq!(
            fx.refresh,
            vec![Refresh::Approvals, Refresh::AllInboxes, Refresh::Docket]
        );
    }

    #[test]
    fn presiding_is_an_agent_holding_the_seat() {
        let presiding = |s: &StatusReply| {
            Model {
                status: Some(s.clone()),
                ..Model::default()
            }
            .presiding()
        };
        let mut s = status(&[]);
        assert!(!presiding(&s));
        // The built-in Speaker, or a person holding the seat: no agent presides.
        s.speaker = Some(SpeakerSeat::BuiltIn);
        assert!(!presiding(&s));
        s.speaker = Some(SpeakerSeat::Senator(path("bob")));
        assert!(!presiding(&s), "bob is one of the Senate's people");
        s.speaker = Some(SpeakerSeat::Senator(path("alice")));
        assert!(!presiding(&s));
        // An agent: whoever is not among the people the daemon reported.
        s.speaker = Some(SpeakerSeat::Senator(path("claude")));
        assert!(presiding(&s));
        assert_eq!(presider_of(&s), Some(&path("claude")));
        // The daemon serves one Senate: there is no Senate below to preside over.
        assert_eq!(
            Model {
                status: Some(s.clone()),
                ..Model::default()
            }
            .senates()
            .count(),
            1
        );
    }

    #[test]
    fn actions_map_to_committee_scoped_requests_as_the_person() {
        let a = Action::Answer {
            committee: committee("a"),
            thread: Seq(12),
            body: "  yes  ".into(),
        };
        let json = serde_json::to_value(a.request().unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"op":"answer","session":{},"thread":12,"body":"yes","committee":"a","as_person":true})
        );
        let close = Action::Close {
            committee: committee("a"),
            thread: Seq(12),
        };
        let json = serde_json::to_value(close.request().unwrap()).unwrap();
        assert_eq!(json["op"], "task");
        assert_eq!(json["status"]["state"], "archived");
        assert_eq!(json["as_person"], true);
        let take = Action::Take {
            committee: committee("a"),
            thread: Seq(12),
        };
        assert!(matches!(
            take.request().unwrap(),
            Request::Take {
                committee: Some(_),
                thread: Seq(12),
                ..
            }
        ));
        let inbox = Action::Inbox {
            agent: path("codex"),
            committee: committee("a"),
        };
        let json = serde_json::to_value(inbox.request().unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"op":"inbox","session":{},"committee":"a","agent":"codex"})
        );
        let docket = serde_json::to_value(Action::Docket.request().unwrap()).unwrap();
        assert_eq!(docket, serde_json::json!({"op":"docket","session":{}}));
        let empty = Action::Answer {
            committee: committee("a"),
            thread: Seq(1),
            body: " ".into(),
        };
        assert_eq!(empty.request(), Err(Failure::EmptyReply));
    }

    #[test]
    fn offline_reasons_are_written_in_the_language_asked() {
        let gone = Offline::NotRunning("/h/bridge.sock".into());
        assert_eq!(gone.text(Locale::En), "cannot connect to /h/bridge.sock");
        assert_eq!(gone.text(Locale::Zh), "无法连接 /h/bridge.sock");
        assert_eq!(
            Offline::Other("boom".into()).text(Locale::Zh),
            "连接失败：boom"
        );
        let f = Failure::Read {
            what: ReadOf::Inbox {
                agent: path("claude"),
                committee: committee("a"),
            },
            message: "no".into(),
        };
        assert_eq!(f.text(Locale::En), "inbox of [senator::claude] on a: no");
        assert_eq!(f.text(Locale::Zh), "[senator::claude] 在 a 的收件箱：no");
    }

    #[test]
    fn codex_terminal_line() {
        assert_eq!(codex_remote(None), "unix://");
        assert_eq!(codex_remote(Some(" ")), "unix://");
        assert_eq!(
            codex_remote(Some("unix:///tmp/cx.sock")),
            "unix:///tmp/cx.sock"
        );
        assert_eq!(
            codex_shell_line("codex", "unix://", Some("/Users/w/my repo"), Some("019a-b")),
            "cd '/Users/w/my repo' && codex resume --remote unix:// 019a-b"
        );
        assert_eq!(
            codex_shell_line("codex", "unix://", None, None),
            "codex --remote unix://"
        );
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        let script = terminal_script(r#"cd '/a "b"' && codex"#);
        assert!(
            script.contains(r#"do script "cd '/a \"b\"' && codex""#),
            "{script}"
        );
    }
}
