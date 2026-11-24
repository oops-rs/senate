//! A fake developer-machine daemon on a unix socket, speaking the bridge's
//! own IPC types: enough of `watch`, `inbox`, `answer`, `take`, `task`,
//! `docket`, `approvals` and `approve` for the app. Used by the tests and by
//! `examples/fake_daemon.rs`.
//!
//! It serves the world the app has to handle: the person `alice`, the one
//! Senate the daemon serves (`alice-desk`), where her `claude` and `codex`
//! sit on this machine, `claude` presides, and the agents `reviewer` and
//! `tester` sit without a stream here. Nothing here touches a real Senate
//! home.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use senate_bridge::agent::AgentSource;
use senate_bridge::approvals::{Approval, ApprovalKind, ResolvedBy};
use senate_bridge::ipc::{
    ApprovalsReply, ApproveReply, CommitteeStatus, DeskDecision, DeskItemDetail, DeskOutcome,
    DocketReply, HostState, HostedModel, HostedPrompt, InboxReply, IpcError, Posted, Reply,
    Request, SenateDocket, SessionRow, SessionsReply, StatusReply, StreamState, ThreadReply,
    TimelineDirection, TimelineItem, TimelineKind, TimelineReply, TimelineUpdate, WatchEvent,
};
use senate_bridge::session::{Runtime, Session, SessionState};
use senate_bridge::state::QueueCounts;
use senate_core::{
    Address, Awaiting, CheckpointFacts, CommitteeId, DocketItem, DocketPage, Event, EventId, Kind,
    Locale, Name, People, SeatFacts, SenatorName, Seq, SpeakerSeat, StatusState, ThreadSummary,
    TriageVerdict,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

pub fn committee() -> CommitteeId {
    CommitteeId::new("refund-v2").unwrap()
}

pub fn path(s: &str) -> SenatorName {
    s.parse().unwrap()
}

pub fn alice() -> Name {
    Name::new("alice").unwrap()
}

fn stream(committee: &str, agent: &str, sessions: usize) -> CommitteeStatus {
    CommitteeStatus {
        agent: path(agent),
        committee: CommitteeId::new(committee).unwrap(),
        delivered: Seq(120),
        reported: Seq(120),
        stream: Some(StreamState::Connected { since: Utc::now() }),
        configured: true,
        sessions,
        // `claude` reviews on refund-v2.
        duty: (agent == "claude").then(|| "reviews refunds".to_string()),
    }
}

/// The Senate the daemon serves.
pub const SENATE: &str = "alice-desk";

/// The daemon's status: one Senate, `alice` its only person, her `claude`
/// holding the Speaker's seat.
pub fn status(sessions: Vec<Session>) -> StatusReply {
    StatusReply {
        descriptors: None,
        paused: Default::default(),
        pid: 4242,
        version: "0.1.0".into(),
        person: alice(),
        senate: Some(SENATE.parse().unwrap()),
        people: People::new([alice()]),
        locale: Locale::En,
        server_url: "http://127.0.0.1:7001".into(),
        committees: vec![
            stream("refund-v2", "claude", 1),
            stream("refund-v2", "codex", sessions.len()),
        ],
        sessions: {
            let mut all = vec![claude_session("cl-1", "claude")];
            all.extend(sessions);
            all
        },
        hosted: Default::default(),
        hosted_unbound: Default::default(),
        hosted_viewer_unavailable: Default::default(),
        dropped_hooks: None,
        queue: QueueCounts::default(),
        speaker: Some(SpeakerSeat::Senator(path("claude"))),
        roles: None,
    }
}

/// The Senate's roles: `claude` chairs refund-v2, and one child Senate,
/// `carol-desk`, declares its `refund-dev` seat there; this Senate is
/// registered with the one above.
pub fn roles() -> senate_bridge::ipc::RolesReply {
    use senate_bridge::ipc::{Registration, RolesReply, UplinkStatus};
    use senate_core::{ChildSeat, ChildSenate, PresenceState, api::ChairInfo};
    RolesReply {
        senate: Some(SENATE.parse().unwrap()),
        office: None,
        present: Vec::new(),
        chairs: vec![ChairInfo {
            committee: committee(),
            chair: Some(path("claude")),
            source: None,
        }],
        children: vec![ChildSenate {
            seats: vec![ChildSeat {
                committee: committee(),
                senator: path("claude-c222"),
                alias: Some(path("refund-dev")),
                duty: Some("reviews refunds".into()),
                presence: PresenceState::Online,
                paused: false,
            }],
            last_seen: Some(Utc::now()),
            ..ChildSenate::new(Name::new("carol-desk").unwrap(), path("claude-d333"))
        }],
        uplink: Some(UplinkStatus {
            url: "https://senate.example".into(),
            parent: Some("/acme".parse().unwrap()),
            name: Name::new("alice-desk").unwrap(),
            speaker: "/acme::senator::claude-e444".parse().unwrap(),
            registration: Registration::Registered,
            at: Utc::now(),
        }),
    }
}

/// `status` with nobody presiding: no agent holds the seat.
pub fn status_alone(sessions: Vec<Session>) -> StatusReply {
    let mut s = status(sessions);
    s.speaker = None;
    s
}

/// A thread addressed to `agent`, parked on the person.
pub fn suspended(root: u64, title: &str, agent: &str) -> ThreadSummary {
    ThreadSummary {
        root: Seq(root),
        kind: Kind::Question,
        from: Address::Speaker,
        on_behalf_of: Some(Address::senator(&path("copilot"))),
        to: vec![Address::senator(&path(agent))],
        agents: vec![path(agent)],
        title: title.into(),
        state: StatusState::Suspended,
        since: Seq(root),
        since_ts: Utc::now(),
        updated: Seq(root),
        steps: Vec::new(),
        links: Vec::new(),
        owner: None,
        triage: Some(TriageVerdict::NeedsHuman),
        escalated_to: None,
        takeover: None,
        reason: Some("a product call".into()),
        answered: false,
        answered_by: None,
        reopened: false,
        escalation: None,
        directed: Vec::new(),
    }
}

pub fn approval(id: &str) -> Approval {
    Approval {
        id: id.into(),
        session_id: "019a-thread".into(),
        kind: ApprovalKind::Command,
        method: "item/commandExecution/requestApproval".into(),
        summary: "run: rm -rf target".into(),
        command: Some("rm -rf target".into()),
        cwd: Some("/w".into()),
        reason: None,
        files: Vec::new(),
        params: serde_json::json!({}),
        received_at: Utc::now(),
    }
}

fn session(id: &str, runtime: Runtime, agent: &str) -> Session {
    Session {
        session_id: id.into(),
        runtime,
        agent: path(agent),
        agent_source: AgentSource::Harness,
        cwd: "/Users/alice/dev/refund".into(),
        git_root: Some("/Users/alice/dev/refund".into()),
        branch: Some("senate/refund-v2/alice".into()),
        inferred_committee: Some(committee()),
        committee_source: None,
        joined_committee: None,
        socket: None,
        token: None,
        claude_pid: None,
        transcript_path: None,
        state: SessionState::Live,
        started_at: Utc::now(),
        last_seen: Utc::now(),
        socket_ok: None,
    }
}

pub fn claude_session(id: &str, agent: &str) -> Session {
    session(id, Runtime::Claude, agent)
}

/// A live Codex thread of `codex` on [`committee`].
pub fn codex_session(thread: &str) -> Session {
    session(thread, Runtime::Codex, "codex")
}

/// One item of each kind awaiting the Speaker `claude`.
pub fn docket() -> DocketReply {
    let event = |seq: u64, from: Address, to: Vec<Address>, body: &str| Event {
        seq: Seq(seq),
        committee: committee(),
        id: EventId::new(),
        ts: Utc::now(),
        kind: Kind::Question,
        from,
        to,
        thread: None,
        on_behalf_of: None,
        source: None,
        body: body.into(),
        refs: None,
        mirror: None,
        meta: serde_json::Map::new(),
    };
    let reviewer = path("reviewer");
    let items = vec![
        DocketItem {
            event: event(
                40,
                Address::senior(&alice()),
                vec![Address::Speaker],
                "谁来看一下 [senator::reviewer] 的补丁？",
            ),
            awaiting: Awaiting::Pick {
                owner: None,
                candidates: vec![reviewer.clone(), path("tester")],
            },
            intended: None,
        },
        DocketItem {
            event: event(
                44,
                Address::senator(&path("tester")),
                vec![Address::senator(&reviewer)],
                "Is the lint config shared?",
            ),
            awaiting: Awaiting::Away {
                agents: vec![reviewer.clone()],
            },
            intended: None,
        },
    ];
    DocketReply {
        speaker: None,
        senates: vec![SenateDocket {
            senate: Some(Name::new(SENATE).unwrap()),
            pages: vec![DocketPage {
                committee: committee(),
                speaker: Some(SpeakerSeat::Senator(path("claude"))),
                office: None,
                items,
                covered: Default::default(),
                seats: vec![SeatFacts {
                    paused: false,
                    agent: reviewer,
                    connected: false,
                    presence: None,
                    last_seen: None,
                    checkpoint: Some(CheckpointFacts {
                        seq: Seq(31),
                        ts: Utc::now(),
                        summary: "lint config moved to the repo root".into(),
                        branch: "senate/refund-v2/alice".into(),
                    }),
                    open_threads: 1,
                    duty: None,
                }],
                head: Seq(44),
            }],
            checkpoints: Vec::new(),
        }],
    }
}

/// What the fake daemon holds and saw.
pub struct World {
    pub status: StatusReply,
    /// Threads with more events than their canned two: root → count.
    pub long: std::collections::BTreeMap<u64, usize>,
    pub sessions: SessionsReply,
    pub threads: Vec<ThreadSummary>,
    pub approvals: Vec<Approval>,
    pub docket: DocketReply,
    desk_receipts: std::collections::BTreeMap<EventId, DeskOutcome>,
    desk_sources: std::collections::BTreeMap<(CommitteeId, Seq), Event>,
    pub requests: Vec<Request>,
    /// The committee of each thread not on [`committee`], by root.
    pub committee_of: std::collections::BTreeMap<u64, CommitteeId>,
    /// The Senate's aliases; `None` is `claude` as `lead` on refund-v2.
    pub aliases: Option<senate_core::alias::KnownAliases>,
    /// The `roster` reply: empty unless a world sets one.
    pub roster: Vec<senate_bridge::ipc::RosterCommittee>,
    /// Each session's timeline, by session id ([`crate::timelines`]).
    pub timelines: std::collections::BTreeMap<String, Vec<TimelineItem>>,
    /// What a watched timeline does next, by session id: one step each
    /// [`World::grow_every`].
    pub growth: std::collections::BTreeMap<String, std::collections::VecDeque<Growth>>,
    /// How often a watched timeline grows; `None`: never (the tests).
    pub grow_every: Option<std::time::Duration>,
    /// A session that grows on its own clock instead: a step every `min`
    /// to `max` milliseconds (a reply streamed a few characters at a time).
    pub stream_ms: std::collections::BTreeMap<String, (u64, u64)>,
    /// Ended and stopped sessions, as `session_history` lists them.
    pub history: Vec<senate_bridge::ipc::SessionHistoryRow>,
    /// The machine's default secretary, as `default_secretary` reads it.
    pub secretary: senate_bridge::ipc::DefaultSecretaryState,
    /// The archived committees, as `archived_committees` reads them.
    pub archived: Vec<senate_core::CommitteeId>,
    /// Roots of threads that are settled: readable, in no inbox.
    pub settled: Vec<u64>,
}

/// One step of a watched timeline: an item appended, or one replaced in
/// place (a tool call that completes), and whether the runtime still works.
#[derive(Debug, Clone)]
pub struct Growth {
    pub item: TimelineItem,
    pub working: bool,
}

impl World {
    /// The committee thread `root` is on.
    pub fn committee_of(&self, root: Seq) -> CommitteeId {
        self.committee_of
            .get(&root.0)
            .cloned()
            .unwrap_or_else(committee)
    }
}

impl World {
    pub fn new() -> Self {
        Self {
            status: status(vec![codex_session("019a-thread")]),
            threads: vec![
                suspended(118, "退款要不要走审批？", "claude"),
                suspended(131, "which env?", "codex"),
            ],
            approvals: vec![approval("019a-thread:7")],
            // Hosted terminals are opt-in (`hosted_sessions()`), so the
            // counts the other tests check stay theirs.
            sessions: SessionsReply::default(),
            long: Default::default(),
            docket: docket(),
            desk_receipts: Default::default(),
            desk_sources: Default::default(),
            requests: Vec::new(),
            committee_of: Default::default(),
            aliases: None,
            roster: Vec::new(),
            timelines: Default::default(),
            growth: Default::default(),
            grow_every: None,
            stream_ms: Default::default(),
            secretary: senate_bridge::ipc::DefaultSecretaryState::NotRecorded,
            history: Vec::new(),
            archived: Vec::new(),
            settled: Vec::new(),
        }
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

pub struct FakeDaemon {
    socket: PathBuf,
    world: Arc<Mutex<World>>,
    events: broadcast::Sender<WatchEvent>,
    tasks: Arc<Mutex<Vec<AbortHandle>>>,
}

impl FakeDaemon {
    pub fn start(socket: &Path, world: Arc<Mutex<World>>) -> Self {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket).unwrap();
        let (events, _) = broadcast::channel(64);
        let tasks: Arc<Mutex<Vec<AbortHandle>>> = Arc::default();
        let fake = Self {
            socket: socket.to_path_buf(),
            world,
            events,
            tasks: Arc::clone(&tasks),
        };
        let world = Arc::clone(&fake.world);
        let events = fake.events.clone();
        let inner = Arc::clone(&tasks);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else {
                    return;
                };
                let task = tokio::spawn(serve(conn, Arc::clone(&world), events.clone()));
                inner.lock().unwrap().push(task.abort_handle());
            }
        });
        tasks.lock().unwrap().push(accept.abort_handle());
        fake
    }

    /// Kill the daemon: every connection drops, the socket goes.
    pub fn stop(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        let _ = std::fs::remove_file(&self.socket);
    }

    pub fn emit(&self, event: WatchEvent) {
        let _ = self.events.send(event);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.world.lock().unwrap().requests.clone()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn serve(conn: UnixStream, world: Arc<Mutex<World>>, events: broadcast::Sender<WatchEvent>) {
    let (r, mut w) = conn.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let request: Request = serde_json::from_str(&line).unwrap();
        if matches!(request, Request::Watch { .. }) {
            let mut rx = events.subscribe();
            let mut out = vec![
                serde_json::to_string(&Reply::ok(&serde_json::json!({"watching": true}))).unwrap(),
            ];
            out.push(
                serde_json::to_string(&WatchEvent::Status(Box::new(
                    world.lock().unwrap().status.clone(),
                )))
                .unwrap(),
            );
            for a in world.lock().unwrap().approvals.clone() {
                out.push(
                    serde_json::to_string(&WatchEvent::ApprovalRequested(Box::new(a))).unwrap(),
                );
            }
            for l in out {
                w.write_all(format!("{l}\n").as_bytes()).await.unwrap();
            }
            while let Ok(event) = rx.recv().await {
                let l = serde_json::to_string(&event).unwrap();
                if w.write_all(format!("{l}\n").as_bytes()).await.is_err() {
                    return;
                }
            }
            return;
        }
        if let Request::SessionDrop {
            session_id,
            name,
            size,
        } = &request
        {
            world.lock().unwrap().requests.push(request.clone());
            let refusal = if *size > senate_bridge::ipc::MAX_SESSION_DROP_BYTES {
                Some("attachments are limited to 20 MiB")
            } else if session_id == "unknown" {
                Some("no registered session with that ID")
            } else {
                None
            };
            if let Some(message) = refusal {
                let reply = Reply::err(IpcError::bad_request(message));
                w.write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes())
                    .await
                    .unwrap();
                return;
            }
            w.write_all(b"{\"ok\":true}\n").await.unwrap();
            let mut bytes = vec![0; *size as usize];
            lines.get_mut().read_exact(&mut bytes).await.unwrap();
            let path = format!("/tmp/senate-fake/drops/fixture/{name}");
            w.write_all(
                format!("{}\n", serde_json::to_string(&Reply::ok(&path)).unwrap()).as_bytes(),
            )
            .await
            .unwrap();
            return;
        }
        if let Request::TimelineWatch { session_id, after } = &request {
            world.lock().unwrap().requests.push(request.clone());
            watch_timeline(&world, session_id, *after, &mut w).await;
            return;
        }
        let reply = handle(&world, &events, request);
        let l = serde_json::to_string(&reply).unwrap();
        w.write_all(format!("{l}\n").as_bytes()).await.unwrap();
    }
}

fn posted(thread: Seq) -> Posted {
    Posted {
        committee: committee(),
        seq: Seq(200),
        to: Vec::new(),
        thread: Some(thread),
        duplicate: false,
        unseen_escalation: None,
    }
}

fn handle(world: &Mutex<World>, events: &broadcast::Sender<WatchEvent>, request: Request) -> Reply {
    let mut w = world.lock().unwrap();
    w.requests.push(request.clone());
    match request {
        Request::Inbox {
            committee: Some(c),
            agent: Some(name),
            session,
            ..
        } => {
            assert_eq!(session, Default::default(), "the app has no session");
            let agent = SenatorName::new(name).unwrap();
            // As the daemon answers: what on this committee is addressed to
            // this agent, and what was put to the person.
            let threads = w
                .threads
                .iter()
                .filter(|t| w.committee_of(t.root) == c)
                .filter(|t| {
                    t.is_for(&agent)
                        || t.escalated_to.as_ref() == Some(&alice())
                        || t.waits_on_person(&alice(), Seq(0)).is_some()
                })
                .cloned()
                .collect();
            Reply::ok(&InboxReply {
                committee: c,
                agents: vec![agent],
                person: alice(),
                threads,
                settled: 0,
                seen: Vec::new(),
            })
        }
        Request::Answer {
            thread,
            as_person,
            committee: Some(_),
            ..
        } => {
            assert!(as_person, "the app replies as the person");
            // As the server folds it: an answer moves the thread to
            // `active`, and `inbox` keeps listing it until it is archived.
            for t in w.threads.iter_mut().filter(|t| t.root == thread) {
                t.state = StatusState::Active;
                t.answered = true;
                t.since = Seq(200);
                t.since_ts = Utc::now();
                t.updated = Seq(200);
            }
            let _ = events.send(WatchEvent::Inbox {
                agent: path("claude"),
                committee: committee(),
                seq: Seq(200),
                kind: Kind::Answer,
            });
            Reply::ok(&posted(thread))
        }
        Request::Take { thread, .. } => {
            for t in w.threads.iter_mut().filter(|t| t.root == thread) {
                t.state = StatusState::Suspended;
                t.takeover = Some(alice());
            }
            Reply::ok(&posted(thread))
        }
        Request::Task {
            thread,
            status,
            as_person,
            ..
        } => {
            assert!(as_person);
            if status.state == StatusState::Archived {
                w.threads.retain(|t| t.root != thread);
            }
            Reply::ok(&posted(thread))
        }
        Request::DeskItem { committee, seq } => {
            let item = w
                .docket
                .senates
                .iter()
                .flat_map(|s| &s.pages)
                .filter(|page| page.committee == committee)
                .flat_map(|page| &page.items)
                .find(|entry| entry.event.seq == seq);
            match item {
                Some(entry) => Reply::ok(&DeskItemDetail {
                    item: entry.event.clone(), events: vec![entry.event.clone()],
                    origin: None,
                    destination: entry.intended.as_ref().map(|_| "payments::senator::ledger".into()),
                    blocked: match seq.0 {
                        60 | 61 => None,
                        62 => Some("Only the Speaker or the owner of the agent serving as Speaker can decide this request.".into()),
                        _ => Some("This card can only decide requests for another committee in this Senate.".into()),
                    },
                }),
                None => Reply::err(IpcError::bad_request("desk item not found")),
            }
        }
        Request::DeskDecide {
            committee,
            seq,
            item_id: _,
            id,
            decision,
        } => {
            if let Some(outcome) = w.desk_receipts.get(&id) {
                return Reply::ok(outcome);
            }
            if seq == Seq(61) && matches!(decision, DeskDecision::Forward) {
                return Reply::err(IpcError::bad_request(
                    "The destination is temporarily unavailable.",
                ));
            }
            if seq != Seq(60) && seq != Seq(61) {
                return Reply::err(IpcError::new(
                    "not_presiding",
                    "Only the Speaker may decide this request.",
                ));
            }
            let receipt = match decision {
                DeskDecision::Forward => DeskOutcome::Delivered {
                    destination: "payments::senator::ledger".into(),
                    committee: CommitteeId::new("payments").unwrap(),
                    seq: Seq(75),
                },
                DeskDecision::Refuse { reason } if !reason.trim().is_empty() => {
                    DeskOutcome::Refused {
                        destination: "refund-v2::senator::tester".into(),
                        committee: committee.clone(),
                        seq: Seq(76),
                    }
                }
                DeskDecision::Refuse { .. } => {
                    return Reply::err(IpcError::bad_request("A refusal reason is required."));
                }
            };
            for page in w.docket.senates.iter_mut().flat_map(|s| &mut s.pages) {
                if page.committee == committee {
                    page.items.retain(|entry| entry.event.seq != seq);
                }
            }
            w.desk_receipts.insert(id, receipt.clone());
            Reply::ok(&receipt)
        }
        Request::Docket { session, committee } => {
            assert_eq!(
                session,
                Default::default(),
                "a person's read has no session"
            );
            assert_eq!(committee, None);
            Reply::ok(&w.docket)
        }
        Request::Thread {
            thread,
            committee: Some(c),
            ..
        } if let Some(event) = w.desk_sources.get(&(c.clone(), thread)) => {
            Reply::ok(&ThreadReply {
                committee: c,
                root: thread,
                events: vec![event.clone()],
            })
        }
        Request::Thread {
            thread,
            committee: Some(c),
            ..
        } => match w
            .threads
            .iter()
            .find(|t| t.root == thread && w.committee_of(t.root) == c)
        {
            Some(t) => Reply::ok(&ThreadReply {
                committee: c,
                root: t.root,
                events: match w.long.get(&t.root.0) {
                    Some(&n) => long_thread(t, n),
                    None => thread_events(t),
                },
            }),
            // A settled thread, no longer in any inbox: a note and its reply.
            None if w.settled.contains(&thread.0) => {
                let mut t = suspended(
                    thread.0,
                    "Reviewed the threshold change; fine by me.",
                    "claude",
                );
                t.kind = Kind::Note;
                t.from = Address::senator(&path("reviewer"));
                t.on_behalf_of = None;
                t.reason = None;
                let mut events = thread_events(&t);
                for e in &mut events {
                    e.committee = c.clone();
                }
                Reply::ok(&ThreadReply {
                    committee: c,
                    root: thread,
                    events,
                })
            }
            None => Reply::err(IpcError::bad_request(format!("no thread {thread}"))),
        },
        // `claude` goes by `lead` on refund-v2.
        Request::Aliases if w.aliases.is_some() => Reply::ok(&w.aliases),
        Request::Roster => Reply::ok(&w.roster),
        Request::Aliases => {
            let mut table = senate_core::alias::CommitteeAliases::new();
            table
                .set(&committee(), path("lead"), path("claude"))
                .unwrap();
            Reply::ok(&senate_core::alias::KnownAliases::Scoped(table))
        }
        Request::Sessions => Reply::ok(&w.sessions),
        Request::Timeline {
            session_id,
            direction,
            cursor,
            limit,
        } => timeline_page(&w, &session_id, direction, cursor, limit),
        Request::Say {
            committee: Some(c),
            to,
            body,
            ..
        } => {
            // Delivered to the session it names: its timeline shows it.
            let ids: Vec<String> = w
                .sessions
                .rows
                .iter()
                .map(|r| (r.session_id.clone(), r.primary.clone()))
                .chain(
                    w.sessions
                        .local_rows
                        .iter()
                        .map(|r| (r.session_id.clone(), r.primary.clone())),
                )
                .filter(|(_, p)| to.iter().any(|a| a.as_senator() == Some(p)))
                .map(|(id, _)| id)
                .collect();
            for id in ids {
                let items = w.timelines.entry(id).or_default();
                let seq = items.last().map_or(1, |i| i.seq + 1);
                items.push(item(
                    seq,
                    0,
                    TimelineKind::SenateMessage {
                        committee: Some(c.to_string()),
                        seq: Some(300),
                        thread: Some(300),
                        from: Some("senator::alice".into()),
                        kind: Some("note".into()),
                        text: body.clone(),
                    },
                ));
            }
            Reply::ok(&Posted {
                committee: c,
                seq: Seq(300),
                to,
                thread: None,
                duplicate: false,
                unseen_escalation: None,
            })
        }
        Request::Pause { senator, .. } => {
            let label = senator.to_string();
            match w.sessions.rows.iter_mut().find(|r| r.label() == label) {
                Some(row) => {
                    row.state = HostState::Stopped;
                    row.paused = true;
                    row.prompt = None;
                    row.working = false;
                    let _ = events.send(WatchEvent::Sessions);
                    Reply::ok(&serde_json::json!({}))
                }
                None => Reply::err(IpcError::bad_request(format!("no session {label}"))),
            }
        }
        Request::Resume { senator } => {
            let label = senator.to_string();
            match w.sessions.rows.iter_mut().find(|r| r.label() == label) {
                Some(row) => {
                    row.state = HostState::Running;
                    row.paused = false;
                    let _ = events.send(WatchEvent::Sessions);
                    Reply::ok(&serde_json::json!({}))
                }
                None => Reply::err(IpcError::bad_request(format!("no session {label}"))),
            }
        }
        Request::Stop { senator, force, .. } => {
            assert!(!force, "the app never forces a stop");
            let label = senator.to_string();
            match w.sessions.rows.iter_mut().find(|r| r.label() == label) {
                Some(row) => {
                    row.state = HostState::Stopped;
                    row.prompt = None;
                    let _ = events.send(WatchEvent::Sessions);
                    Reply::ok(&serde_json::json!({}))
                }
                None => Reply::err(IpcError::bad_request(format!("no session {label}"))),
            }
        }
        Request::ApproveSession { senator, decision } => {
            let label = senator.to_string();
            match w.sessions.rows.iter_mut().find(|r| r.label() == label) {
                Some(row) if row.prompt.as_ref().is_some_and(|p| !p.screen) => {
                    let tool = row.prompt.take().and_then(|p| p.tool);
                    let _ = events.send(WatchEvent::Sessions);
                    Reply::ok(&senate_bridge::ipc::ApproveSessionReply {
                        senator: row.primary.clone(),
                        committee: row.committee.clone().unwrap_or_else(committee),
                        runtime: row.runtime,
                        tool,
                        decision,
                        audit_pending: false,
                    })
                }
                Some(_) => Reply::err(IpcError::bad_request(format!(
                    "{label} has no permission prompt waiting."
                ))),
                None => Reply::err(IpcError::bad_request(format!("no session {label}"))),
            }
        }
        Request::Stats { from, to, tz } => Reply::ok(&sample_stats(from, to, tz)),
        Request::SessionHistory {
            committee,
            before,
            limit,
        } => {
            let mut rows: Vec<_> = w
                .history
                .iter()
                .filter(|r| {
                    committee
                        .as_ref()
                        .is_none_or(|c| r.committee.as_ref() == Some(c))
                })
                .cloned()
                .collect();
            rows.sort_by_key(|row| std::cmp::Reverse(row.last_activity));
            let from: usize = before.and_then(|b| b.parse().ok()).unwrap_or(0);
            let limit = limit.unwrap_or(30).max(1);
            let page: Vec<_> = rows.iter().skip(from).take(limit).cloned().collect();
            let next = (from + page.len() < rows.len()).then(|| (from + page.len()).to_string());
            Reply::ok(&senate_bridge::ipc::SessionHistoryReply {
                rows: page,
                next_before: next,
            })
        }
        Request::SessionSearch {
            query,
            committee,
            limit,
            cursor,
        } => {
            let hits = search_sessions(&w, &query, committee.as_ref());
            let from: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
            let limit = limit.unwrap_or(30).max(1);
            let page: Vec<_> = hits.iter().skip(from).take(limit).cloned().collect();
            let next = (from + page.len() < hits.len()).then(|| (from + page.len()).to_string());
            Reply::ok(&senate_bridge::ipc::SessionSearchReply {
                hits: page,
                next_cursor: next,
                indexing: false,
                progress: None,
            })
        }
        Request::SessionForget { session_id } => {
            match w.history.iter().position(|r| r.session_id == session_id) {
                Some(at) => {
                    w.history.remove(at);
                    w.timelines.remove(&session_id);
                    Reply::ok(&serde_json::Value::Null)
                }
                None => Reply::err(IpcError::bad_request(format!(
                    "{session_id} is not an ended or stopped session"
                ))),
            }
        }
        Request::DefaultSecretary => Reply::ok(&secretary_reply(&w, None)),
        Request::ArchivedCommittees => Reply::ok(&w.archived),
        Request::ArchiveCommittee {
            committee,
            archived,
            ..
        } => {
            let was = w.archived.contains(&committee);
            if archived {
                if !was {
                    w.archived.push(committee.clone());
                }
            } else {
                w.archived.retain(|c| c != &committee);
            }
            Reply::ok(&senate_core::api::CommitteeArchiveInfo {
                committee,
                archived,
                seq: (archived != was).then_some(senate_core::Seq(1)),
            })
        }
        Request::EnsureSecretary => {
            use senate_bridge::ipc::{DefaultSecretaryState as St, EnsureAction};
            let action = match w.secretary {
                St::Stopped => {
                    return Reply::err(IpcError::bad_request(
                        "The secretary was stopped on purpose; `senate session restore office::secretary` brings it back."
                            .to_string(),
                    ));
                }
                St::Elsewhere => {
                    return Reply::err(IpcError::bad_request(
                        "office::secretary is someone else's seat.".to_string(),
                    ));
                }
                St::NotRecorded => EnsureAction::Started,
                St::Paused => EnsureAction::Resumed,
                St::Ended => EnsureAction::Restored,
                St::Running => EnsureAction::AlreadyRunning,
            };
            seat_secretary(&mut w, St::Running);
            let _ = events.send(WatchEvent::Sessions);
            Reply::ok(&secretary_reply(&w, Some(action)))
        }
        Request::Restore { senator, .. } => {
            let label = senator.to_string();
            if label == "office::secretary" {
                seat_secretary(&mut w, senate_bridge::ipc::DefaultSecretaryState::Running);
            }
            // A session from History: it runs again, and leaves History.
            if let Some(at) = w.history.iter().position(|r| history_label(r) == label) {
                let r = w.history.remove(at);
                revive(&mut w, &r);
                let _ = events.send(WatchEvent::Sessions);
                return Reply::ok(&serde_json::json!({}));
            }
            match w.sessions.rows.iter_mut().find(|r| r.label() == label) {
                Some(row) => {
                    row.state = HostState::Running;
                    let _ = events.send(WatchEvent::Sessions);
                    Reply::ok(&serde_json::json!({}))
                }
                None => Reply::err(IpcError::bad_request(format!("no session {label}"))),
            }
        }
        Request::Approvals => Reply::ok(&ApprovalsReply {
            approvals: w.approvals.clone(),
        }),
        Request::Approve { id, decision } => match w.approvals.iter().position(|a| a.id == id) {
            Some(i) => {
                let approval = w.approvals.remove(i);
                let _ = events.send(WatchEvent::ApprovalResolved {
                    id: id.clone(),
                    session_id: approval.session_id.clone(),
                    by: ResolvedBy::Bridge,
                    decision: Some(decision),
                });
                Reply::ok(&ApproveReply { approval, decision })
            }
            None => Reply::err(IpcError::bad_request(format!(
                "approval {id} was already answered"
            ))),
        },
        other => Reply::err(IpcError::bad_request(format!("fake: {other:?}"))),
    }
}

/// Two hosted terminals: `claude` (alias `lead`) running with a permission
/// prompt waiting, and a stopped Codex `reviewer`.
pub fn hosted_sessions() -> SessionsReply {
    let row = |name: &str, alias: Option<&str>, runtime: Runtime, state: HostState| SessionRow {
        paused: false,
        chair: false,
        shown_name: None,
        host_id: EventId::new(),
        session_id: format!("s-{name}"),
        primary: path(name),
        alias: alias.map(path),
        committee: Some(committee()),
        runtime,
        model: HostedModel::default(),
        dir: "/Users/alice/dev/refund".into(),
        state,
        started_at: (state == HostState::Running).then(|| Utc::now() - chrono::Duration::hours(2)),
        attached: false,
        typed_secs_ago: None,
        tty: "/tmp/none".into(),
        prompt: None,
        working: false,
        last_activity: Utc::now() - chrono::Duration::minutes(2),
        title: None,
        branch: None,
    };
    let mut lead = row("claude", Some("lead"), Runtime::Claude, HostState::Running);
    lead.prompt = Some(HostedPrompt {
        tool: Some("cargo publish".into()),
        since: Utc::now() - chrono::Duration::minutes(3),
        screen: false,
    });
    lead.model = HostedModel {
        model: Some("opus".into()),
        effort: Some("high".into()),
        ..HostedModel::default()
    };
    SessionsReply {
        rows: vec![
            lead,
            row("reviewer", None, Runtime::Codex, HostState::Stopped),
        ],
        orphans: Vec::new(),
        local_rows: Vec::new(),
    }
}

/// `n` more threads waiting on alice (half to claude, half to codex), and
/// thread 990 with `events` events.
pub fn scale(w: &mut World, n: u64, events: usize) {
    let titles = [
        "Should the refund flow ask for approval",
        "[senator::claude] wants a review of the retry budget",
        "Which environment runs the migration?",
        "Rotate the staging keys before Friday",
    ];
    for i in 0..n {
        let agent = if i % 2 == 0 { "claude" } else { "codex" };
        let title = format!("{} #{i}", titles[i as usize % titles.len()]);
        w.threads.push(suspended(1000 + i, &title, agent));
    }
    w.threads
        .push(suspended(990, "A very long discussion", "claude"));
    w.long.insert(990, events);
}

/// What a canned thread says: its question, and the agent's note on it.
pub fn thread_events(t: &ThreadSummary) -> Vec<Event> {
    let event = |seq: u64, kind: Kind, from: Address, body: String| Event {
        seq: Seq(seq),
        committee: committee(),
        id: EventId::new(),
        ts: t.since_ts,
        kind,
        from,
        to: t.to.clone(),
        thread: (seq != t.root.0).then_some(t.root),
        on_behalf_of: None,
        source: None,
        body,
        refs: None,
        mirror: None,
        meta: Default::default(),
    };
    let agent = t.agents.first().cloned().unwrap_or_else(|| path("claude"));
    let mut events = vec![Event {
        on_behalf_of: t.on_behalf_of.clone(),
        ..event(t.root.0, t.kind.clone(), t.from.clone(), t.title.clone())
    }];
    if let Some(reason) = &t.reason {
        events.push(event(
            t.root.0 + 1,
            Kind::Note,
            Address::senator(&agent),
            format!("Waiting on a person: {reason}."),
        ));
    }
    // Thread 1565's shape: the agent's statuses, one a repeat (left out).
    let status = |seq: u64, title: Option<&str>| {
        let mut payload = senate_core::StatusPayload::new(senate_core::StatusState::Processing);
        payload.title = title.map(str::to_string);
        let mut e = event(seq, Kind::Status, Address::senator(&agent), String::new());
        e.meta
            .insert(senate_core::StatusPayload::META_KEY.into(), payload.value());
        e
    };
    events.push(status(t.root.0 + 2, Some("Wait for startup seat reports")));
    events.push(status(t.root.0 + 3, None));
    events.push(status(
        t.root.0 + 4,
        Some("Cover the uplink reconnect cycle"),
    ));
    events
}

/// A thread of `n` events, the agent and alice taking turns.
pub fn long_thread(t: &ThreadSummary, n: usize) -> Vec<Event> {
    let mut events = thread_events(t);
    events.truncate(1);
    for i in 1..n as u64 {
        let (from, kind) = if i % 2 == 1 {
            (Address::senator(&path("claude")), Kind::Note)
        } else {
            (Address::senior(&alice()), Kind::Answer)
        };
        events.push(Event {
            seq: Seq(t.root.0 + i),
            thread: Some(t.root),
            kind,
            from,
            body: format!(
                "Step {i}: [senator::claude] checked the retry budget against the \
                 staging numbers; the p99 stays under the limit."
            ),
            id: EventId::new(),
            ..events[0].clone()
        });
    }
    events
}

/// A socket path in a fresh temp dir (short enough to bind), and a world.
pub fn setup() -> (tempfile::TempDir, PathBuf, Arc<Mutex<World>>) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("bridge.sock");
    (dir, socket, Arc::new(Mutex::new(World::new())))
}

/// A Senate with several committees, the way a busy one looks: chairs on
/// three of four committees (one of them away), alice's agents seated under
/// aliases beside agents that are long gone, a request and an escalation
/// waiting on her, tasks her agents are still working on, and an item put
/// to the Speaker on another committee. `examples/fake_daemon.rs` serves it;
/// the tests keep the plain world.
pub fn desk_fixture(w: &mut World) {
    let page = &mut w.docket.senates[0].pages[0];
    let template = page.items[0].event.clone();
    for (seq, body) in [
        (
            60,
            "Please confirm the revised refund threshold with the payments owner. The threshold and handoff are recorded in this original thread.",
        ),
        (61, "Please send the audit summary to the payments owner."),
        (
            62,
            "Please pass this request to payments. The signed-in account may inspect it here.",
        ),
    ] {
        let mut event = template.clone();
        event.id = EventId::new();
        event.seq = Seq(seq);
        event.kind = Kind::Request;
        event.from = Address::senator(&path("tester"));
        event.to = vec![Address::Speaker];
        event.body = body.into();
        let intended = "payments::chair".parse().unwrap();
        w.desk_sources
            .insert((page.committee.clone(), Seq(seq)), event.clone());
        page.items.push(DocketItem {
            event,
            awaiting: Awaiting::Addressed,
            intended: Some(intended),
        });
    }
    page.head = Seq(62);
}

pub fn busy_senate(w: &mut World) {
    desk_fixture(w);
    use senate_bridge::ipc::{RosterCommittee, RosterMember};
    use senate_core::{Directed, Escalation};

    let c = |name: &str| CommitteeId::new(name).unwrap();
    let ledger = "codex-01a206b8-8ed9-7a73-934c-f967b877774a";
    let auditor = "claude-3f1d9a20-5b7e-4c11-9e0a-2d6c8b41f7aa";
    let scribe = "claude-bff528a7-9e17-4c3a-8d21-7f0e5b2a9c11";
    let editor = "codex-01a1f00d-77c2-7e10-b0a4-5c3e19d2a6b8";
    let oncall = "agy-36358e84-ac54-409e-beb4-9d2c71f0e3a5";
    let gone = [
        "claude-0a1b2c3d-4e5f-4a6b-8c7d-9e0f1a2b3c4d",
        "codex-01a1e8ff-f5a4-7ab1-ad38-fca61f2e8847",
        "claude-77aa0c19-3b2d-4e8f-a1c6-0d5e9f2b4c3e",
    ];

    // Aliases: each committee names its seats.
    let mut table = senate_core::alias::CommitteeAliases::new();
    for (committee, alias, primary) in [
        ("refund-v2", "lead", "claude"),
        ("payments", "ledger", ledger),
        ("payments", "auditor", auditor),
        ("docs", "scribe", scribe),
        ("docs", "editor", editor),
        ("infra", "oncall", oncall),
    ] {
        table
            .set(&c(committee), path(alias), path(primary))
            .unwrap();
    }
    w.aliases = Some(senate_core::alias::KnownAliases::Scoped(table));
    if let Some(roles) = w.status.roles.as_mut() {
        for (committee, chair) in [("payments", ledger), ("docs", scribe)] {
            roles.chairs.push(senate_core::api::ChairInfo {
                committee: c(committee),
                chair: Some(path(chair)),
                source: None,
            });
        }
        roles.chairs.push(senate_core::api::ChairInfo {
            committee: c("infra"),
            chair: None,
            source: None,
        });
    }

    // alice's streams: live where her agents are at work, stopped where
    // they left.
    let stopped = |committee: &str, agent: &str| {
        let mut s = stream(committee, agent, 0);
        s.stream = Some(StreamState::Stopped);
        s.duty = None;
        s
    };
    w.status.committees.extend([
        stream("payments", ledger, 1),
        stream("infra", oncall, 1),
        stopped("docs", scribe),
        stopped("refund-v2", gone[2]),
    ]);
    // Sessions: the live ones, and old ones that ended days ago.
    let on = |id: &str, runtime: Runtime, agent: &str, committee: &str| {
        let mut x = session(id, runtime, agent);
        x.inferred_committee = Some(c(committee));
        x
    };
    let mut sessions = vec![
        on("pay-1", Runtime::Codex, ledger, "payments"),
        on("ops-1", Runtime::Claude, oncall, "infra"),
    ];
    for (i, agent) in gone.iter().enumerate() {
        let runtime = if agent.starts_with("codex") {
            Runtime::Codex
        } else {
            Runtime::Claude
        };
        let mut x = on(&format!("old-{i}"), runtime, agent, "refund-v2");
        x.state = SessionState::Ended;
        x.last_seen = Utc::now() - chrono::Duration::days(3 + i as i64);
        sessions.push(x);
    }
    let mut x = on("docs-1", Runtime::Claude, scribe, "docs");
    x.state = SessionState::Stale;
    x.last_seen = Utc::now() - chrono::Duration::hours(20);
    sessions.push(x);
    w.status.sessions.extend(sessions);

    // Threads.
    let directed = |seq: u64, from: &str, kind: Kind, escalated: bool, line: &str| Directed {
        person: alice(),
        seq: Seq(seq),
        ts: Utc::now() - chrono::Duration::minutes(40),
        from: Address::senator(&path(from)),
        on_behalf_of: None,
        kind,
        escalated,
        line: line.into(),
    };
    // A request from payments' chair to alice herself.
    let mut request = suspended(
        210,
        "Approve the ledger migration window (Sat 02:00 UTC)?",
        ledger,
    );
    request.kind = Kind::Request;
    request.from = Address::senator(&path(ledger));
    request.on_behalf_of = None;
    request.to = vec![Address::senior(&alice())];
    request.agents = Vec::new();
    request.state = StatusState::Active;
    request.triage = None;
    request.reason = None;
    request.since_ts = Utc::now() - chrono::Duration::minutes(40);
    request.directed = vec![directed(
        210,
        ledger,
        Kind::Request,
        false,
        "The migration needs a 30-minute write freeze; Saturday 02:00 UTC is the quietest window.",
    )];
    // An escalation from docs' chair, about a decision only she can make.
    let mut escalation = suspended(
        57,
        "Publish the API reference under the new domain?",
        scribe,
    );
    escalation.from = Address::senator(&path(editor));
    escalation.on_behalf_of = None;
    escalation.reason = Some("needs a product call".into());
    escalation.since_ts = Utc::now() - chrono::Duration::hours(2);
    escalation.escalation = Some(Escalation {
        to: alice(),
        by: Address::senator(&path(scribe)),
        seq: Seq(59),
        ts: Utc::now() - chrono::Duration::hours(2),
        reason: Some("Moving the docs changes every published link; that is your call.".into()),
    });
    escalation.escalated_to = Some(alice());
    escalation.directed = vec![directed(
        59,
        scribe,
        Kind::Note,
        true,
        "Moving the docs changes every published link; that is your call.",
    )];
    // What her agents are still working on: waits on them, not on her.
    let task = |root: u64, title: &str, from: &str, to: &str, minutes: i64| {
        let mut t = suspended(root, title, to);
        t.kind = Kind::Request;
        t.from = Address::senator(&path(from));
        t.on_behalf_of = None;
        t.state = StatusState::Processing;
        t.triage = None;
        t.reason = None;
        t.since_ts = Utc::now() - chrono::Duration::minutes(minutes);
        t
    };
    w.threads.extend([
        request,
        escalation,
        task(215, "Reconcile the October payouts", auditor, ledger, 25),
        task(12, "Rotate the staging certificates", "claude", oncall, 90),
    ]);
    for (root, committee) in [
        (210, "payments"),
        (57, "docs"),
        (215, "payments"),
        (12, "infra"),
    ] {
        w.committee_of.insert(root, c(committee));
    }

    // Put to the Speaker on payments: a question addressed to the desk.
    let mut asked = w.docket.senates[0].pages[0].items[0].clone();
    asked.event.seq = Seq(220);
    asked.event.committee = c("payments");
    asked.event.from = Address::senator(&path(auditor));
    asked.event.body = "Who signs off on refunds above 10,000 EUR?".into();
    asked.awaiting = Awaiting::Addressed;
    let mut page = w.docket.senates[0].pages[0].clone();
    page.committee = c("payments");
    page.items = vec![asked];
    page.seats = Vec::new();
    w.docket.senates[0].pages.push(page);

    // The roster as the daemon reads it, members alphabetical by id.
    let member = |id: &str, name: &str, active: bool| RosterMember {
        senator: path(id),
        display_name: name.into(),
        active,
    };
    let committee = |name: &str,
                     chair: Option<(&str, &str, bool)>,
                     pending: usize,
                     members: Vec<RosterMember>| {
        RosterCommittee {
            committee: c(name),
            chair: chair.map(|(id, _, _)| path(id)),
            chair_display_name: chair.map(|(_, n, _)| n.to_string()),
            chair_live: chair.is_some_and(|(_, _, live)| live),
            members,
            pending,
        }
    };
    w.roster = vec![
        committee(
            "docs",
            Some((scribe, "scribe", false)),
            1,
            vec![
                member(scribe, "scribe", false),
                member(editor, "editor", false),
            ],
        ),
        committee("infra", None, 1, vec![member(oncall, "oncall", true)]),
        committee(
            "payments",
            Some((ledger, "ledger", true)),
            2,
            vec![
                member(auditor, "auditor", true),
                member(ledger, "ledger", true),
            ],
        ),
        committee(
            "refund-v2",
            Some(("claude", "lead", true)),
            2,
            vec![
                member("claude", "lead", true),
                member(gone[2], "claude-77aa0c19", false),
                member("codex", "codex", true),
                member("reviewer", "reviewer", false),
                member("tester", "tester", false),
            ],
        ),
    ];
}

// ---- Sessions and their timelines ------------------------------------------

/// A timeline item `minutes_ago` minutes old.
pub fn item(seq: u64, minutes_ago: i64, kind: TimelineKind) -> TimelineItem {
    TimelineItem {
        seq,
        ts: Some(Utc::now() - chrono::Duration::minutes(minutes_ago)),
        truncated: false,
        kind,
    }
}

fn timeline_page(
    w: &World,
    session_id: &str,
    direction: TimelineDirection,
    cursor: Option<u64>,
    limit: Option<usize>,
) -> Reply {
    let runtime = w
        .sessions
        .rows
        .iter()
        .find(|r| r.session_id == session_id)
        .map(|r| r.runtime)
        .or_else(|| {
            w.sessions
                .local_rows
                .iter()
                .find(|r| r.session_id == session_id)
                .map(|r| r.runtime)
        })
        // An ended session's conversation reads as a live one's does.
        .or_else(|| {
            w.history
                .iter()
                .find(|r| r.session_id == session_id)
                .map(|r| r.runtime)
        });
    let Some(runtime) = runtime else {
        return Reply::err(IpcError::bad_request(format!("no session {session_id}")));
    };
    let all = w.timelines.get(session_id);
    let limit = limit.unwrap_or(60).min(300);
    let items: Vec<TimelineItem> = all.map_or_else(Vec::new, |all| {
        let picked: Vec<&TimelineItem> = match direction {
            TimelineDirection::Tail => all.iter().collect(),
            TimelineDirection::Before => all
                .iter()
                .filter(|i| cursor.is_none_or(|c| i.seq < c))
                .collect(),
            TimelineDirection::After => all
                .iter()
                .filter(|i| cursor.is_none_or(|c| i.seq > c))
                .collect(),
        };
        let skip = match direction {
            TimelineDirection::After => 0,
            _ => picked.len().saturating_sub(limit),
        };
        picked.into_iter().skip(skip).take(limit).cloned().collect()
    });
    let first = items.first().map(|i| i.seq);
    let last = items.last().map(|i| i.seq);
    let all_first = all.and_then(|a| a.first()).map(|i| i.seq);
    let all_last = all.and_then(|a| a.last()).map(|i| i.seq);
    Reply::ok(&TimelineReply {
        session_id: session_id.to_string(),
        runtime,
        available: all.is_some(),
        unavailable_reason: None,
        has_older: first.is_some_and(|f| all_first.is_some_and(|a| a < f)),
        has_newer: last.is_some_and(|l| all_last.is_some_and(|a| a > l)),
        first_seq: first,
        last_seq: last,
        items,
        working: working(w, session_id),
    })
}

fn working(w: &World, session_id: &str) -> bool {
    w.sessions
        .rows
        .iter()
        .find(|r| r.session_id == session_id)
        .map(|r| r.working)
        .or_else(|| {
            w.sessions
                .local_rows
                .iter()
                .find(|r| r.session_id == session_id)
                .map(|r| r.working)
        })
        .unwrap_or(false)
}

/// `timeline_watch`: what came after `after`, then one step of the
/// session's growth every [`World::grow_every`], as the daemon streams it.
async fn watch_timeline(
    world: &Mutex<World>,
    session_id: &str,
    after: u64,
    out: &mut tokio::net::unix::OwnedWriteHalf,
) {
    let first = {
        let w = world.lock().unwrap();
        w.timelines.get(session_id).map(|items| TimelineUpdate {
            items: items.iter().filter(|i| i.seq > after).cloned().collect(),
            working: working(&w, session_id),
            reset: false,
        })
    };
    let Some(first) = first else {
        let l = serde_json::to_string(&Reply::err(IpcError::bad_request(format!(
            "no timeline for {session_id}"
        ))))
        .unwrap();
        let _ = out.write_all(format!("{l}\n").as_bytes()).await;
        return;
    };
    let l = serde_json::to_string(&first).unwrap();
    if out.write_all(format!("{l}\n").as_bytes()).await.is_err() {
        return;
    }
    let (every, stream) = {
        let w = world.lock().unwrap();
        (w.grow_every, w.stream_ms.get(session_id).copied())
    };
    let every = stream
        .map(|(min, _)| std::time::Duration::from_millis(min))
        .or(every);
    let mut jitter: u64 = 0x9e37_79b9_7f4a_7c15;
    let Some(every) = every else {
        // Hold the stream open, as the daemon does, until the app lets go.
        std::future::pending::<()>().await;
        return;
    };
    loop {
        let wait = match stream {
            Some((min, max)) if max > min => {
                jitter ^= jitter << 13;
                jitter ^= jitter >> 7;
                jitter ^= jitter << 17;
                std::time::Duration::from_millis(min + jitter % (max - min + 1))
            }
            _ => every,
        };
        tokio::time::sleep(wait).await;
        let update = {
            let mut w = world.lock().unwrap();
            let Some(step) = w.growth.get_mut(session_id).and_then(|g| g.pop_front()) else {
                drop(w);
                continue;
            };
            let items = w.timelines.entry(session_id.to_string()).or_default();
            match items.iter_mut().find(|i| i.seq == step.item.seq) {
                Some(old) => *old = step.item.clone(),
                None => items.push(step.item.clone()),
            }
            for r in w
                .sessions
                .rows
                .iter_mut()
                .filter(|r| r.session_id == session_id)
            {
                r.working = step.working;
                r.last_activity = Utc::now();
            }
            for r in w
                .sessions
                .local_rows
                .iter_mut()
                .filter(|r| r.session_id == session_id)
            {
                r.working = step.working;
                r.last_activity = Utc::now();
            }
            drop(w);
            TimelineUpdate {
                items: vec![step.item],
                working: step.working,
                reset: false,
            }
        };
        let l = serde_json::to_string(&update).unwrap();
        if out.write_all(format!("{l}\n").as_bytes()).await.is_err() {
            return;
        }
    }
}

/// The sessions on this machine in [`busy_senate`]'s Senate, one of each
/// state, on every committee and on none, and their timelines. Call after
/// `busy_senate`.
pub fn session_tree(w: &mut World) {
    use senate_bridge::ipc::LocalSessionRow;

    let ledger = "codex-01a206b8-8ed9-7a73-934c-f967b877774a";
    let auditor = "claude-3f1d9a20-5b7e-4c11-9e0a-2d6c8b41f7aa";
    let scribe = "claude-bff528a7-9e17-4c3a-8d21-7f0e5b2a9c11";
    let oncall = "agy-36358e84-ac54-409e-beb4-9d2c71f0e3a5";
    let loner = "claude-5e1f2a3b-4c5d-4e6f-8a9b-0c1d2e3f4a5b";
    let c = |name: &str| CommitteeId::new(name).unwrap();
    let ago = |minutes: i64| Utc::now() - chrono::Duration::minutes(minutes);

    let mut rows = hosted_sessions().rows;
    // refund-v2: `lead` chairs, a prompt waits in it.
    rows[0].chair = true;
    rows[0].shown_name = Some("lead".into());
    rows[0].title = Some("Refund approval flow".into());
    rows[0].branch = Some("feat/refund-approval".into());
    rows[0].last_activity = ago(3);
    rows[1].title = Some("Review the retry budget".into());
    rows[1].last_activity = ago(60 * 26);
    let hosted =
        |id: &str, primary: &str, alias: &str, committee: &str, runtime: Runtime, chair: bool| {
            SessionRow {
                host_id: EventId::new(),
                session_id: id.into(),
                primary: path(primary),
                alias: Some(path(alias)),
                committee: Some(c(committee)),
                chair,
                shown_name: Some(alias.into()),
                runtime,
                model: HostedModel::default(),
                dir: format!("/Users/alice/dev/{committee}").into(),
                state: HostState::Running,
                started_at: Some(ago(180)),
                attached: false,
                typed_secs_ago: None,
                tty: "/tmp/none".into(),
                prompt: None,
                paused: false,
                working: false,
                last_activity: ago(1),
                title: None,
                branch: Some("main".into()),
            }
        };
    // payments: its chair `ledger` at work.
    let mut l = hosted("pay-1", ledger, "ledger", "payments", Runtime::Codex, true);
    l.working = true;
    l.title = Some("Ledger migration plan".into());
    l.model = HostedModel {
        model: Some("gpt-5.5-codex".into()),
        effort: Some("high".into()),
        ..HostedModel::default()
    };
    // docs: its chair `scribe`, paused.
    let mut d = hosted("docs-1", scribe, "scribe", "docs", Runtime::Claude, true);
    d.state = HostState::Stopped;
    d.started_at = None;
    d.paused = true;
    d.title = Some("API reference move".into());
    d.last_activity = ago(60 * 20);
    // infra: `deploy`, paused with nothing unread (every state shows).
    let mut p = hosted(
        "ops-2",
        "claude-9a8b7c6d-1e2f-4a3b-8c4d-5e6f7a8b9c0d",
        "deploy",
        "infra",
        Runtime::Claude,
        false,
    );
    p.state = HostState::Stopped;
    p.started_at = None;
    p.paused = true;
    p.title = Some("Roll the staging certificates".into());
    p.last_activity = ago(60 * 5);
    rows.extend([l, d, p]);

    let local = |id: &str,
                 primary: &str,
                 alias: Option<&str>,
                 committee: Option<&str>,
                 runtime: Runtime,
                 minutes: i64| LocalSessionRow {
        session_id: id.into(),
        primary: path(primary),
        alias: alias.map(path),
        committee: committee.map(c),
        chair: false,
        shown_name: alias.map(Into::into),
        runtime,
        dir: format!("/Users/alice/dev/{}", committee.unwrap_or("scratch")).into(),
        branch: Some("main".into()),
        working: false,
        last_activity: ago(minutes),
        title: None,
    };
    let mut codex = local(
        "019a-thread",
        "codex",
        Some("codex"),
        Some("refund-v2"),
        Runtime::Codex,
        0,
    );
    codex.working = true;
    codex.title = Some("Which env runs the migration".into());
    let mut audit = local(
        "pay-2",
        auditor,
        Some("auditor"),
        Some("payments"),
        Runtime::Claude,
        25,
    );
    audit.title = Some("Reconcile the October payouts".into());
    let ops = local(
        "ops-1",
        oncall,
        Some("oncall"),
        Some("infra"),
        Runtime::Agy,
        8,
    );
    let mut scratch = local("scratch-1", loner, None, None, Runtime::Claude, 130);
    scratch.title = Some("Try the new parser".into());
    w.sessions = SessionsReply {
        rows,
        orphans: Vec::new(),
        local_rows: vec![codex, audit, ops, scratch],
    };
    timelines(w);
    w.settled.push(140);
}

/// Canned timelines: `lead`'s is long (older pages to scroll back to) and
/// ends at the tool call its prompt asks about; `ledger` and `codex` are at
/// work. Watched, `lead`'s call completes, a Senate message arrives and it
/// answers; `ledger` finishes its command.
fn timelines(w: &mut World) {
    use senate_bridge::ipc::{TimelineTodo, TimelineToolDetail as D, TimelineToolStatus as S};

    let user = |text: &str| TimelineKind::UserMessage { text: text.into() };
    let said = |text: &str| TimelineKind::AssistantMessage { text: text.into() };
    let think = |text: &str| TimelineKind::Reasoning { text: text.into() };
    let tool = |id: &str, name: &str, status: S, detail: D| TimelineKind::ToolCall {
        call_id: id.into(),
        name: name.into(),
        status,
        output: None,
        detail,
    };
    let shell = |command: &str, output: Option<&str>, exit: Option<i64>| D::Shell {
        command: command.into(),
        cwd: Some("/Users/alice/dev/refund".into()),
        output: output.map(Into::into),
        exit_code: exit,
    };
    let todo = |items: &[(&str, &str)]| TimelineKind::Todo {
        items: items
            .iter()
            .map(|(text, status)| TimelineTodo {
                text: (*text).into(),
                status: (*status).into(),
            })
            .collect(),
    };

    // lead: forty older exchanges, then the conversation the page opens on.
    let mut lead = Vec::new();
    let mut seq = 0;
    let mut push = |items: &mut Vec<TimelineItem>, minutes: i64, kind: TimelineKind| {
        seq += 1;
        items.push(item(seq, minutes, kind));
        seq
    };
    for i in 0..40 {
        let m = 400 - i * 8;
        push(
            &mut lead,
            m,
            user(&format!(
                "Earlier step {}: check the refund fixtures.",
                i + 1
            )),
        );
        push(
            &mut lead,
            m - 1,
            tool(
                &format!("old-{i}"),
                "Bash",
                S::Completed,
                shell(
                    "cargo test -p refund --lib",
                    Some("test result: ok. 42 passed; 0 failed"),
                    Some(0),
                ),
            ),
        );
        push(
            &mut lead,
            m - 2,
            said(&format!("Fixtures for step {} pass.", i + 1)),
        );
    }
    // A build with a long log (64 KiB) and a large diff: the page stays
    // responsive with both open.
    let log: String = (0..1200)
        .map(|i| {
            format!(
                "   Compiling crate-{i:04} v0.1.0 (/Users/alice/dev/refund/vendor/crate-{i:04})\n"
            )
        })
        .collect();
    push(
        &mut lead,
        33,
        tool(
            "build-1",
            "Bash",
            S::Completed,
            shell("cargo build -vv", Some(&log), Some(0)),
        ),
    );
    let diff: String = std::iter::once("@@ -1,600 +1,600 @@\n".to_string())
        .chain((0..600).map(|i| {
            format!(
                "-    let fee_{i} = Money::eur({i});\n+    let fee_{i} = Money::eur({i}) * RATE;\n"
            )
        }))
        .collect();
    push(
        &mut lead,
        32,
        tool(
            "fmt-1",
            "Edit",
            S::Completed,
            D::Edit {
                path: "crates/refund/src/fees.rs".into(),
                unified_diff: Some(diff),
                old: None,
                new: None,
            },
        ),
    );
    // Two MCP calls: read as "senate.status", "senate.answer".
    push(
        &mut lead,
        31,
        tool(
            "mcp-1",
            "mcp__senate__status",
            S::Completed,
            D::Other {
                summary: String::new(),
            },
        ),
    );
    push(
        &mut lead,
        31,
        tool(
            "mcp-2",
            "mcp__senate__answer",
            S::Completed,
            D::Other {
                summary: "thread 2419".into(),
            },
        ),
    );
    push(
        &mut lead,
        30,
        user("Add an approval step to refunds above 1,000 EUR, then publish the crate."),
    );
    push(
        &mut lead,
        29,
        think(
            "The approval rule lives in `policy.rs`; the flow in `flow.rs` calls it before paying out. I should read both, add the threshold, test, then publish.",
        ),
    );
    push(
        &mut lead,
        29,
        todo(&[
            ("Read the refund policy and flow", "completed"),
            ("Add the 1,000 EUR approval threshold", "completed"),
            ("Run the refund tests", "completed"),
            ("Publish refund 0.4.0", "in_progress"),
        ]),
    );
    push(
        &mut lead,
        28,
        tool(
            "read-1",
            "Read",
            S::Completed,
            D::Read {
                path: "crates/refund/src/policy.rs".into(),
            },
        ),
    );
    // Reasoning with no text is dropped; this one goes inside the search,
    // which has no output of its own.
    push(&mut lead, 27, think("  "));
    push(
        &mut lead,
        27,
        think("Where is the threshold decided? Search before editing."),
    );
    push(
        &mut lead,
        27,
        tool(
            "grep-1",
            "Grep",
            S::Completed,
            D::Search {
                query: "fn needs_approval".into(),
            },
        ),
    );
    push(&mut lead, 25, tool("edit-1", "Edit", S::Completed, D::Edit {
        path: "crates/refund/src/policy.rs".into(),
        unified_diff: Some(
            "@@ -12,7 +12,9 @@ pub fn needs_approval(refund: &Refund) -> bool {\n-    refund.amount > Money::eur(5_000)\n+    // Above 1,000 EUR a person approves.\n+    refund.amount > Money::eur(1_000)\n+        || refund.customer.is_flagged()\n }\n".into(),
        ),
        old: None,
        new: None,
    }));
    push(
        &mut lead,
        22,
        think("The edit compiles; run the whole crate's tests, not just the policy module."),
    );
    push(
        &mut lead,
        22,
        tool(
            "test-1",
            "Bash",
            S::Completed,
            shell(
                "cargo test -p refund",
                Some(
                    "running 47 tests\n...............................................\ntest result: ok. 47 passed; 0 failed; 0 ignored",
                ),
                Some(0),
            ),
        ),
    );
    push(
        &mut lead,
        20,
        TimelineKind::SenateMessage {
            committee: Some("refund-v2".into()),
            seq: Some(131),
            thread: Some(131),
            from: Some("senator::codex".into()),
            kind: Some("question".into()),
            text: "which env runs the migration — staging or prod-eu?".into(),
        },
    );
    // Reasoning before a reply, with no call between: behind "Thinking".
    push(
        &mut lead,
        19,
        think("Tests pass. codex asked about the environment; answer that after publishing."),
    );
    push(
        &mut lead,
        19,
        said(
            "The approval step is in: refunds above **1,000 EUR**, or for a flagged customer, now wait for a person.\n\n- `policy.rs`: the threshold and the flagged check\n- 47 tests pass\n\nNext I publish `refund 0.4.0`.",
        ),
    );
    let publish = push(
        &mut lead,
        3,
        tool(
            "pub-1",
            "Bash",
            S::Running,
            shell("cargo publish -p refund", None, None),
        ),
    );
    w.timelines.insert("s-claude".into(), lead);
    w.growth.insert(
        "s-claude".into(),
        [
            Growth {
                item: item(publish, 0, tool("pub-1", "Bash", S::Completed, shell(
                    "cargo publish -p refund",
                    Some("    Packaging refund v0.4.0\n    Uploading refund v0.4.0\n    Published refund v0.4.0 at registry `crates-io`"),
                    Some(0),
                ))),
                working: true,
            },
            Growth {
                item: item(publish + 1, 0, TimelineKind::SenateMessage {
                    committee: Some("refund-v2".into()),
                    seq: Some(140),
                    thread: Some(140),
                    from: Some("senator::reviewer".into()),
                    kind: Some("note".into()),
                    text: "Reviewed the threshold change; fine by me.".into(),
                }),
                working: true,
            },
            Growth {
                item: item(publish + 2, 0, said("Published **refund 0.4.0**. The reviewer agrees with the threshold.")),
                working: false,
            },
        ]
        .into(),
    );

    // ledger: mid-command.
    w.timelines.insert("pay-1".into(), vec![
        item(1, 14, user("Plan the ledger migration: what freezes, for how long, and when.")),
        item(2, 13, tool("r", "Read", S::Completed, D::Read { path: "migrations/0042_split_ledger.sql".into() })),
        item(3, 10, said("The split needs a 30-minute write freeze. I asked alice for the Saturday 02:00 UTC window.")),
        item(4, 1, tool("dry", "shell", S::Running, D::Shell {
            command: "ledgerctl migrate --dry-run 0042".into(),
            cwd: Some("/Users/alice/dev/payments".into()),
            output: None,
            exit_code: None,
        })),
    ]);
    w.growth.insert(
        "pay-1".into(),
        [Growth {
            item: item(
                4,
                0,
                tool(
                    "dry",
                    "shell",
                    S::Completed,
                    D::Shell {
                        command: "ledgerctl migrate --dry-run 0042".into(),
                        cwd: Some("/Users/alice/dev/payments".into()),
                        output: Some("dry run: 1,204,331 rows to move, est. 22m 10s".into()),
                        exit_code: Some(0),
                    },
                ),
            ),
            working: false,
        }]
        .into(),
    );

    w.timelines.insert(
        "019a-thread".into(),
        vec![
            item(1, 6, user("Find out which environment runs the migration.")),
            item(
                2,
                5,
                tool(
                    "s",
                    "shell",
                    S::Failed,
                    D::Shell {
                        command: "kubectl --context prod-eu get jobs -n payments".into(),
                        cwd: None,
                        output: Some(
                            "error: You must be logged in to the server (Unauthorized)".into(),
                        ),
                        exit_code: Some(1),
                    },
                ),
            ),
            item(
                3,
                0,
                think("No access to prod-eu from here; asking the committee."),
            ),
        ],
    );
    w.timelines.insert(
        "pay-2".into(),
        vec![
            item(
                1,
                40,
                user("Reconcile the October payouts against the bank statement."),
            ),
            item(
                2,
                25,
                said("Reconciled: 3 payouts differ, all by FX rounding under 0.01 EUR."),
            ),
        ],
    );
    // `scribe` (paused) and the scratch session keep theirs; `ops-1` runs
    // agy, which has none.
    w.timelines.insert(
        "docs-1".into(),
        vec![
            item(
                1,
                60 * 21,
                user("Move the API reference to docs.example.com."),
            ),
            item(
                2,
                60 * 20,
                said("Paused until alice decides on the new domain."),
            ),
        ],
    );
    w.timelines.insert(
        "scratch-1".into(),
        vec![
            item(1, 131, user("Try the new parser on the fixtures.")),
            item(2, 130, said("All 212 fixtures parse.")),
            item(
                3,
                129,
                user("Show me every Markdown element you can write."),
            ),
            // One reply with every element the page draws, English and
            // Chinese: the renderer's specimen.
            item(4, 128, said(include_str!("specimen.md"))),
            item(5, 127, user("And the thread-2148 sample.")),
            item(
                6,
                126,
                said(include_str!(
                    "../../../senate-markdown/tests/fixtures/thread-2148.md"
                )),
            ),
        ],
    );
}

/// A long, busy session shaped like a real chair's: `n` items of what a
/// coding agent does (the person's request, reasoning, shell calls with
/// small and 64 KiB outputs, edits, Markdown-heavy replies, to-do lists),
/// and growth that keeps it busy while watched: a running call whose
/// output grows in place, then completes, then the next. `FAKE_BIG=N` in
/// the fake daemon; what `SENATE_APP_PERF=session` measures.
pub fn marathon(w: &mut World, n: usize) {
    use senate_bridge::ipc::{
        LocalSessionRow, TimelineTodo, TimelineToolDetail as D, TimelineToolStatus as S,
    };
    let id = "marathon-1";
    w.sessions.local_rows.push(LocalSessionRow {
        session_id: id.into(),
        primary: path("claude-6d1e0f2a-3b4c-4d5e-8f6a-7b8c9d0e1f2a"),
        alias: Some(path("marathon")),
        committee: Some(committee()),
        chair: false,
        shown_name: Some("marathon".into()),
        runtime: Runtime::Claude,
        dir: "/Users/alice/dev/senate".into(),
        branch: Some("feat/app-sessions".into()),
        working: true,
        last_activity: Utc::now(),
        title: Some("A long chair session".into()),
    });
    let big: String = (0..900)
        .map(|i| format!("   Compiling crate-{i:04} v0.1.0 (/Users/alice/dev/senate/vendor/crate-{i:04}) [{}]\n", "=".repeat(i % 7)))
        .collect();
    // Code in several languages, and the whole specimen now and then.
    let code = [
        (
            "rust",
            "fn coalesce(&self) {\n    if self.due.swap(true, Ordering::AcqRel) { return; }\n}",
        ),
        (
            "swift",
            "func coalesce() async {\n    guard !due else { return }\n    due = true // one re-read\n}",
        ),
        (
            "python",
            "def coalesce(self):\n    if self.due:  # already queued\n        return\n    self.due = True",
        ),
        (
            "typescript",
            "const coalesce = (s: Store): void => { if (s.due) return; s.due = true; };",
        ),
        (
            "bash",
            "cargo test -p senate-app-core && rg -n 'coalesce' crates/ | wc -l",
        ),
        (
            "json",
            "{ \"due\": true, \"reads\": 1, \"note\": \"coalesced\" }",
        ),
    ];
    let reply = |i: usize| {
        if i % 400 == 5 {
            return include_str!("specimen.md").to_string();
        }
        let (lang, body) = code[(i / 8) % code.len()];
        format!(
            "## Step {i}\n\nThe change to `link.rs` is in: the **sessions** re-read is coalesced, and the watch keeps its *place*. What I checked:\n\n1. `cargo test -p senate-app-core` passes ({} tests)\n2. the fake daemon's tour runs\n   - light and dark\n   - English and 中文\n3. clippy is clean\n\n```{lang}\n{body}\n```\n\n> Next: measure the scroll, then the watch.\n\nSee [the README](https://example.com/readme) for the numbers.",
            40 + i % 9
        )
    };
    let shell = |cmd: String, out: Option<String>, status: S| TimelineKind::ToolCall {
        call_id: format!("c-{cmd}"),
        name: "Bash".into(),
        status,
        output: None,
        detail: D::Shell {
            command: cmd,
            cwd: Some("/Users/alice/dev/senate".into()),
            output: out,
            exit_code: Some(0),
        },
    };
    let mut items = Vec::with_capacity(n);
    let mut seq = 0u64;
    let mut push = |items: &mut Vec<TimelineItem>, kind: TimelineKind| {
        seq += 1;
        items.push(item(seq, 0, kind));
    };
    let mut i = 0;
    while items.len() < n {
        match i % 8 {
            0 => push(
                &mut items,
                TimelineKind::UserMessage {
                    text: format!("Now step {i}: tighten the watch and measure it."),
                },
            ),
            1 => push(
                &mut items,
                TimelineKind::Reasoning {
                    text: format!(
                        "Step {i}: read link.rs first, then the store; the cost is in the re-read."
                    ),
                },
            ),
            2 => push(
                &mut items,
                shell(
                    format!("rg -n coalesce crates/{i}"),
                    Some("crates/senate-app-core/src/link.rs:412: fn sessions_soon".into()),
                    S::Completed,
                ),
            ),
            3 => push(
                &mut items,
                shell(
                    format!("cargo build -vv #{i}"),
                    Some(if i % 24 == 3 {
                        big.clone()
                    } else {
                        "Finished `dev` profile in 4.2s".into()
                    }),
                    S::Completed,
                ),
            ),
            4 => push(
                &mut items,
                TimelineKind::ToolCall {
                    call_id: format!("e-{i}"),
                    name: "Edit".into(),
                    status: S::Completed,
                    output: None,
                    detail: D::Edit {
                        path: "crates/senate-app-core/src/link.rs".into(),
                        unified_diff: Some(format!(
                            "@@ -1,3 +1,4 @@\n fn a() {{}}\n-fn b{i}() {{}}\n+fn b{i}() {{ todo!() }}\n+fn c{i}() {{}}\n"
                        )),
                        old: None,
                        new: None,
                    },
                },
            ),
            5 => push(
                &mut items,
                TimelineKind::AssistantMessage { text: reply(i) },
            ),
            6 if i % 16 == 6 => push(
                &mut items,
                TimelineKind::Todo {
                    items: vec![
                        TimelineTodo {
                            text: "Measure".into(),
                            status: "completed".into(),
                        },
                        TimelineTodo {
                            text: "Fix".into(),
                            status: "in_progress".into(),
                        },
                        TimelineTodo {
                            text: "Measure again".into(),
                            status: "pending".into(),
                        },
                    ],
                },
            ),
            _ => push(
                &mut items,
                TimelineKind::AssistantMessage {
                    text: format!("Step {i} done."),
                },
            ),
        }
        i += 1;
    }
    // Growth: a call that runs, its output growing in place, then done;
    // then a reply; again.
    let mut growth = std::collections::VecDeque::new();
    let mut next = seq;
    for round in 0..400 {
        next += 1;
        let call = next;
        let mut out = String::new();
        for step in 0..4 {
            out.push_str(&format!("test step {round}.{step} ... ok\n"));
            growth.push_back(Growth {
                item: item(
                    call,
                    0,
                    shell(
                        format!("cargo test #{round}"),
                        Some(out.clone()),
                        S::Running,
                    ),
                ),
                working: true,
            });
        }
        growth.push_back(Growth {
            item: item(
                call,
                0,
                shell(format!("cargo test #{round}"), Some(out), S::Completed),
            ),
            working: true,
        });
        next += 1;
        growth.push_back(Growth {
            item: item(
                next,
                0,
                TimelineKind::AssistantMessage { text: reply(round) },
            ),
            working: true,
        });
    }
    w.timelines.insert(id.into(), items);
    w.growth.insert(id.into(), growth);
}

/// `FAKE_WORKING=N` in the fake daemon: N more sessions, all working, on no
/// committee, cycling Claude Code, Codex, Antigravity and ACP: the
/// sidebar's working marks, to look at and to measure.
pub fn working_sessions(w: &mut World, n: usize) {
    use senate_bridge::ipc::LocalSessionRow;
    let runtimes = [Runtime::Claude, Runtime::Codex, Runtime::Agy, Runtime::Acp];
    for i in 0..n {
        let runtime = runtimes[i % runtimes.len()];
        let name = format!("{}-{}", runtime.as_str(), i / runtimes.len() + 1);
        w.sessions.local_rows.push(LocalSessionRow {
            session_id: format!("working-{i}"),
            primary: path(&format!(
                "{}-0000{i:04}-0000-4000-8000-000000000000",
                runtime.as_str()
            )),
            alias: Some(path(&name)),
            committee: None,
            chair: false,
            shown_name: Some(name),
            runtime,
            dir: "/Users/alice/dev/scratch".into(),
            branch: Some("main".into()),
            working: true,
            last_activity: Utc::now(),
            title: None,
        });
    }
}

/// `FAKE_STREAM=1` in the fake daemon: a session, `stream`, whose replies
/// arrive as a token stream (a few characters every 20 to 40 ms, the one
/// message growing in place): prose, a long Rust and a Swift block, a
/// table, a list; then another request, and the reply again. Real sessions
/// deliver whole messages; this shows the renderer's streaming path.
pub fn streaming(w: &mut World) {
    use senate_bridge::ipc::LocalSessionRow;
    let id = "stream-1";
    w.sessions.local_rows.push(LocalSessionRow {
        session_id: id.into(),
        primary: path("claude-5f7a9c1e-2b3d-4e5f-8a6b-7c8d9e0f1a2b"),
        alias: Some(path("stream")),
        committee: None,
        chair: false,
        shown_name: Some("stream".into()),
        runtime: Runtime::Claude,
        dir: "/Users/alice/dev/senate".into(),
        branch: Some("main".into()),
        working: true,
        last_activity: Utc::now(),
        title: Some("A streamed reply".into()),
    });
    let user = |text: &str| TimelineKind::UserMessage { text: text.into() };
    let said = |text: &str| TimelineKind::AssistantMessage { text: text.into() };
    let ask = "Show me how the coalescing works, with the Rust side and the Swift side.";
    w.timelines.insert(id.into(), vec![item(1, 1, user(ask))]);
    let reply = include_str!("stream.md");
    let mut growth = std::collections::VecDeque::new();
    let mut seq = 1;
    for round in 0..20 {
        if round > 0 {
            seq += 1;
            growth.push_back(Growth {
                item: item(seq, 0, user("Once more, please.")),
                working: true,
            });
        }
        seq += 1;
        // Chunks of 1 to 6 characters, on character boundaries.
        let mut at = 0;
        let mut n: usize = 7 + round;
        while at < reply.len() {
            n = n.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let mut end = (at + 1 + (n >> 16) % 6).min(reply.len());
            while !reply.is_char_boundary(end) {
                end += 1;
            }
            at = end;
            growth.push_back(Growth {
                item: item(seq, 0, said(&reply[..at])),
                working: at < reply.len(),
            });
        }
    }
    w.growth.insert(id.into(), growth);
    w.stream_ms.insert(id.into(), (20, 40));
}

/// The default secretary's senator in the fake.
pub const SECRETARY: &str = "claude-5ec2e7a1-0b1c-4d2e-8f3a-4b5c6d7e8f90";

fn secretary_reply(
    w: &World,
    action: Option<senate_bridge::ipc::EnsureAction>,
) -> senate_bridge::ipc::DefaultSecretary {
    use senate_bridge::ipc::DefaultSecretaryState as St;
    senate_bridge::ipc::DefaultSecretary {
        committee: CommitteeId::new("office").unwrap(),
        alias: "secretary".parse().unwrap(),
        senator: (w.secretary != St::NotRecorded).then(|| SECRETARY.parse().unwrap()),
        state: w.secretary,
        action,
    }
}

/// `FAKE_SECRETARY=<state>` in the fake daemon (`running`, `paused`,
/// `ended`, `stopped`, `elsewhere`; none recorded by default), and what an
/// ensure leaves: its session, on `office`, as `secretary`, on this machine
/// unless its seat is elsewhere.
pub fn seat_secretary(w: &mut World, state: senate_bridge::ipc::DefaultSecretaryState) {
    use senate_bridge::ipc::{DefaultSecretaryState as St, LocalSessionRow};
    w.secretary = state;
    let id = "secretary-1";
    w.sessions.rows.retain(|r| r.session_id != id);
    w.sessions.local_rows.retain(|r| r.session_id != id);
    if matches!(state, St::NotRecorded | St::Elsewhere) {
        return;
    }
    let host = match state {
        St::Running => HostState::Running,
        _ => HostState::Stopped,
    };
    w.sessions.rows.push(SessionRow {
        paused: state == St::Paused,
        chair: false,
        shown_name: Some("secretary".into()),
        host_id: EventId::new(),
        session_id: id.into(),
        primary: path(SECRETARY),
        alias: Some(path("secretary")),
        committee: Some(CommitteeId::new("office").unwrap()),
        runtime: Runtime::Claude,
        model: HostedModel::default(),
        dir: "/Users/alice/.senate/secretary".into(),
        state: host,
        started_at: (host == HostState::Running).then(Utc::now),
        attached: false,
        typed_secs_ago: None,
        tty: "/tmp/none".into(),
        prompt: None,
        working: false,
        last_activity: Utc::now(),
        title: Some("Your secretary".into()),
        branch: None,
    });
    w.sessions.local_rows.push(LocalSessionRow {
        session_id: id.into(),
        primary: path(SECRETARY),
        alias: Some(path("secretary")),
        committee: Some(CommitteeId::new("office").unwrap()),
        chair: false,
        shown_name: Some("secretary".into()),
        runtime: Runtime::Claude,
        dir: "/Users/alice/.senate/secretary".into(),
        branch: None,
        working: false,
        last_activity: Utc::now(),
        title: Some("Your secretary".into()),
    });
    w.timelines.entry(id.into()).or_insert_with(|| {
        vec![item(
            1,
            0,
            TimelineKind::AssistantMessage {
                text: "I'm your secretary. Tell me what you're working on: I open a committee per project, give each chair a brief, and follow up through the chairs.".into(),
            },
        )]
    });
}

fn history_label(r: &senate_bridge::ipc::SessionHistoryRow) -> String {
    match (&r.committee, &r.alias) {
        (Some(c), Some(a)) => format!("{c}::{a}"),
        _ => r.primary.to_string(),
    }
}

/// A History session restored: hosted and running again, in the sidebar.
fn revive(w: &mut World, r: &senate_bridge::ipc::SessionHistoryRow) {
    use senate_bridge::ipc::LocalSessionRow;
    w.sessions.rows.push(SessionRow {
        paused: false,
        chair: false,
        shown_name: r.shown_name.clone(),
        host_id: EventId::new(),
        session_id: r.session_id.clone(),
        primary: r.primary.clone(),
        alias: r.alias.clone(),
        committee: r.committee.clone(),
        runtime: r.runtime,
        model: r.model.clone(),
        dir: r.dir.clone(),
        state: HostState::Running,
        started_at: Some(Utc::now()),
        attached: false,
        typed_secs_ago: None,
        tty: "/tmp/none".into(),
        prompt: None,
        working: false,
        last_activity: Utc::now(),
        title: r.title.clone(),
        branch: r.branch.clone(),
    });
    w.sessions.local_rows.push(LocalSessionRow {
        session_id: r.session_id.clone(),
        primary: r.primary.clone(),
        alias: r.alias.clone(),
        committee: r.committee.clone(),
        chair: false,
        shown_name: r.shown_name.clone(),
        runtime: r.runtime,
        dir: r.dir.clone(),
        branch: r.branch.clone(),
        working: false,
        last_activity: Utc::now(),
        title: r.title.clone(),
    });
}

/// The searchable text of a timeline item, and what kind it is.
fn item_text(kind: &TimelineKind) -> Option<(senate_bridge::ipc::SessionSearchKind, String)> {
    use senate_bridge::ipc::{SessionSearchKind as K, TimelineToolDetail as D};
    match kind {
        TimelineKind::UserMessage { text } => Some((K::UserMessage, text.clone())),
        TimelineKind::AssistantMessage { text } => Some((K::AssistantMessage, text.clone())),
        TimelineKind::ToolCall { detail, name, .. } => Some((
            K::ToolCall,
            match detail {
                D::Shell {
                    command, output, ..
                } => format!("{command}\n{}", output.clone().unwrap_or_default()),
                other => format!("{name} {other:?}"),
            },
        )),
        _ => None,
    }
}

/// Case-insensitive matches in every timeline the fake has, live and past,
/// newest first.
fn search_sessions(
    w: &World,
    query: &str,
    committee: Option<&CommitteeId>,
) -> Vec<senate_bridge::ipc::SessionSearchHit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let committee_of = |id: &str| -> Option<CommitteeId> {
        w.history
            .iter()
            .find(|r| r.session_id == id)
            .and_then(|r| r.committee.clone())
            .or_else(|| {
                w.sessions
                    .local_rows
                    .iter()
                    .find(|r| r.session_id == id)
                    .and_then(|r| r.committee.clone())
            })
            .or_else(|| {
                w.sessions
                    .rows
                    .iter()
                    .find(|r| r.session_id == id)
                    .and_then(|r| r.committee.clone())
            })
    };
    let mut hits = Vec::new();
    for (id, items) in &w.timelines {
        if committee.is_some() && committee_of(id).as_ref() != committee {
            continue;
        }
        let live = !w.history.iter().any(|r| &r.session_id == id);
        for it in items {
            let Some((kind, text)) = item_text(&it.kind) else {
                continue;
            };
            let lower = text.to_lowercase();
            let Some(at) = lower.find(&needle) else {
                continue;
            };
            // About 50 characters either side, on character boundaries.
            let mut start = at.saturating_sub(50);
            while !text.is_char_boundary(start) {
                start -= 1;
            }
            let mut end = (at + needle.len() + 50).min(text.len());
            while !text.is_char_boundary(end) {
                end += 1;
            }
            let mut snippet = text[start..end].replace('\n', " ");
            if start > 0 {
                snippet.insert(0, '…');
            }
            if end < text.len() {
                snippet.push('…');
            }
            hits.push(senate_bridge::ipc::SessionSearchHit {
                session_id: id.clone(),
                seq: it.seq,
                kind,
                snippet,
                ts: it.ts,
                live,
            });
        }
    }
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.ts));
    hits
}

/// `FAKE_HISTORY=N` in the fake daemon (36 by default): ended and stopped
/// sessions across committees, runtimes and days, each readable one with
/// a short conversation the search can find; every ninth is unreadable
/// (its transcript gone), and only Claude Code and Codex ones restore.
pub fn history(w: &mut World, n: usize) {
    use senate_bridge::ipc::{SessionHistoryRow, TimelineToolDetail as D, TimelineToolStatus as S};
    let committees = ["refund-v2", "payments", "infra", "docs", ""];
    let runtimes = [Runtime::Claude, Runtime::Codex, Runtime::Agy, Runtime::Acp];
    let subjects = [
        (
            "Refund threshold review",
            "the refund threshold",
            "cargo test -p refund",
        ),
        (
            "Ledger migration plan",
            "the ledger migration",
            "ledgerctl migrate --dry-run 0042",
        ),
        (
            "Flaky watch test",
            "the flaky watch test",
            "cargo test -p senate-bridge watch -- --nocapture",
        ),
        (
            "Release notes for 0.11",
            "the release notes",
            "git log --oneline v0.10.0..HEAD",
        ),
        (
            "Docs site navigation",
            "the docs navigation",
            "npm run build",
        ),
        (
            "Approval card layout",
            "the approval card",
            "xcodebuild -scheme Senate build",
        ),
        (
            "Rate limiter for the API",
            "the rate limiter",
            "cargo bench -p gateway",
        ),
        (
            "On-call runbook",
            "the on-call runbook",
            "rg -n 'page' runbooks/",
        ),
        ("退款审批流程", "退款审批", "cargo test -p refund approval"),
    ];
    let names = [
        "scout",
        "mender",
        "auditor-2",
        "scribe-old",
        "builder",
        "porter",
        "tester",
        "planner",
    ];
    for i in 0..n {
        let committee = committees[i % committees.len()];
        let runtime = runtimes[i % runtimes.len()];
        let (title, topic, command) = subjects[i % subjects.len()];
        let name = format!("{}-{}", names[i % names.len()], i / names.len() + 1);
        let id = format!("past-{i:03}");
        let started = Utc::now() - chrono::Duration::hours(6 + i as i64 * 9);
        let ran = chrono::Duration::minutes(25 + (i as i64 * 37) % 180);
        let readable = i % 9 != 8;
        let primary = path(&format!(
            "{}-{:08x}-0000-4000-8000-{:012x}",
            runtime.as_str(),
            i,
            i
        ));
        w.history.push(SessionHistoryRow {
            session_id: id.clone(),
            primary,
            alias: Some(path(&name)),
            shown_name: Some(name.clone()),
            committee: (!committee.is_empty()).then(|| CommitteeId::new(committee).unwrap()),
            runtime,
            model: HostedModel {
                model: (runtime == Runtime::Claude).then(|| "opus".into()),
                effort: (runtime == Runtime::Claude && i % 2 == 0).then(|| "high".into()),
                ..HostedModel::default()
            },
            dir: format!(
                "/Users/alice/dev/{}",
                if committee.is_empty() {
                    "scratch"
                } else {
                    committee
                }
            )
            .into(),
            branch: Some(format!(
                "feat/{}",
                title.to_lowercase().split(' ').next().unwrap_or("work")
            )),
            title: Some(title.into()),
            started_at: started,
            last_activity: started + ran,
            readable,
            restorable: readable && matches!(runtime, Runtime::Claude | Runtime::Codex),
            item_count: readable.then_some(6),
        });
        if !readable {
            continue;
        }
        let at = |minutes: i64| started + chrono::Duration::minutes(minutes);
        let mut items = vec![
            item(
                1,
                0,
                TimelineKind::UserMessage {
                    text: format!("Look into {topic} and tell me what you find."),
                },
            ),
            item(
                2,
                0,
                TimelineKind::Reasoning {
                    text: format!("Start with {topic}: read the code, then run the check."),
                },
            ),
            item(
                3,
                0,
                TimelineKind::ToolCall {
                    call_id: format!("c-{i}"),
                    name: "Bash".into(),
                    status: S::Completed,
                    output: None,
                    detail: D::Shell {
                        command: command.into(),
                        cwd: None,
                        output: Some(format!("ok: {topic} checked, 0 failures")),
                        exit_code: Some(0),
                    },
                },
            ),
            item(
                4,
                0,
                TimelineKind::AssistantMessage {
                    text: format!(
                        "## {title}\n\nI looked at **{topic}**. The check passes; one thing to decide:\n\n- keep the current behaviour\n- or tighten it behind a flag\n\n```bash\n{command}\n```"
                    ),
                },
            ),
            item(
                5,
                0,
                TimelineKind::UserMessage {
                    text: "Tighten it behind a flag, then stop here.".into(),
                },
            ),
            item(
                6,
                0,
                TimelineKind::AssistantMessage {
                    text: format!("Done: {topic} is behind `--strict`, off by default. Stopping."),
                },
            ),
        ];
        for (k, it) in items.iter_mut().enumerate() {
            it.ts = Some(at(k as i64 * 4));
        }
        w.timelines.insert(id, items);
    }
}

fn sample_stats(
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
    tz: String,
) -> senate_bridge::stats::StatsReply {
    use senate_bridge::stats::{
        Counts, Progress, Rank, SessionCounts, StatsDay, StatsReply, Tokens,
    };
    let breakdown = SessionCounts {
        sessions_started: 1,
        sessions_active: 2,
        active_seconds: 2340,
    };
    let counts = Counts {
        sessions_started: 1,
        sessions_active: 2,
        active_seconds: 2340,
        prompts: 5,
        agent_turns: 6,
        tool_calls: 12,
        commands: 4,
        edits: 5,
        mcp_calls: 2,
        sent: [("note".into(), 3), ("answer".into(), 1)].into(),
        received: [("question".into(), 2)].into(),
        threads_opened: 2,
        threads_closed: 1,
        threads_answered: 1,
        approvals_decided: 1,
        tokens: [(
            "claude".into(),
            Tokens {
                input: 16900,
                output: 3300,
            },
        )]
        .into(),
        ..Counts::default()
    };
    let mut hours = [0; 24];
    hours[10] = 8;
    hours[14] = 15;
    let day = StatsDay {
        date: to,
        counts: counts.clone(),
        hours,
        by_runtime: [
            (
                "claude".into(),
                SessionCounts {
                    sessions_started: 1,
                    sessions_active: 1,
                    active_seconds: 1600,
                },
            ),
            (
                "codex".into(),
                SessionCounts {
                    sessions_started: 0,
                    sessions_active: 1,
                    active_seconds: 740,
                },
            ),
        ]
        .into(),
        by_committee: [(committee().to_string(), breakdown)].into(),
        repositories: [("refund-service".into(), 2340)].into(),
    };
    StatsReply {
        from,
        to,
        tz,
        days: vec![day],
        totals: counts,
        hours,
        current_streak: 1,
        longest_streak: 1,
        top_committees: vec![Rank {
            name: committee().to_string(),
            active_seconds: 2340,
        }],
        top_repositories: vec![Rank {
            name: "refund-service".into(),
            active_seconds: 2340,
        }],
        indexing: false,
        progress: Progress {
            days_cached: 1,
            days_total: 1,
            sessions: None,
            logs_current: true,
        },
        notes: vec!["Fixture uses recorded activity.".into()],
    }
}
