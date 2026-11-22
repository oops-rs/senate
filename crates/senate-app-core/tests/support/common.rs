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
    ApprovalsReply, ApproveReply, CommitteeStatus, DocketReply, HostState, HostedModel,
    HostedPrompt, InboxReply, IpcError, Posted, Reply, Request, SenateDocket, SessionRow,
    SessionsReply, StatusReply, StreamState, ThreadReply, WatchEvent,
};
use senate_bridge::session::{Runtime, Session, SessionState};
use senate_bridge::state::QueueCounts;
use senate_core::{
    Address, Awaiting, CheckpointFacts, CommitteeId, DocketItem, DocketPage, Event, EventId, Kind,
    Locale, Name, People, SeatFacts, SenatorName, Seq, SpeakerSeat, StatusState, ThreadSummary,
    TriageVerdict,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    pub requests: Vec<Request>,
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
            requests: Vec::new(),
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
            // As the daemon answers: what is addressed to this agent, and
            // what was put to the person.
            let threads = w
                .threads
                .iter()
                .filter(|t| t.is_for(&agent) || t.escalated_to.as_ref() == Some(&alice()))
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
        } => match w.threads.iter().find(|t| t.root == thread) {
            Some(t) => Reply::ok(&ThreadReply {
                committee: c,
                root: t.root,
                events: match w.long.get(&t.root.0) {
                    Some(&n) => long_thread(t, n),
                    None => thread_events(t),
                },
            }),
            None => Reply::err(IpcError::bad_request(format!("no thread {thread}"))),
        },
        // `claude` goes by `lead` on refund-v2.
        Request::Aliases => {
            let mut table = senate_core::alias::CommitteeAliases::new();
            table
                .set(&committee(), path("lead"), path("claude"))
                .unwrap();
            Reply::ok(&senate_core::alias::KnownAliases::Scoped(table))
        }
        Request::Sessions => Reply::ok(&w.sessions),
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
        Request::Restore { senator, .. } => {
            let label = senator.to_string();
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
    };
    SessionsReply {
        rows: vec![
            lead,
            row("reviewer", None, Runtime::Codex, HostState::Stopped),
        ],
        orphans: Vec::new(),
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
