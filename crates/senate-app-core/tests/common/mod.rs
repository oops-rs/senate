//! A fake developer-machine daemon on a unix socket, speaking the bridge's
//! own IPC types: enough of `watch`, `inbox`, `answer`, `take`, `task`,
//! `docket`, `approvals` and `approve` for the app. Used by the tests and by
//! `examples/fake_daemon.rs`.
//!
//! It serves the world the app has to handle: the person `alice`, a primary
//! Senate where `alice/claude` and `alice/codex` sit, and a Senate one level
//! down that `alice/claude` presides over, where `alice/claude/reviewer`
//! sits. Nothing here touches a real Senate home.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use senate_bridge::agent::AgentSource;
use senate_bridge::approvals::{Approval, ApprovalKind, ResolvedBy};
use senate_bridge::ipc::{
    ApprovalsReply, ApproveReply, CommitteeStatus, DocketReply, InboxReply, IpcError, Posted,
    Reply, Request, SenateDocket, StatusReply, StreamState, WatchEvent,
};
use senate_bridge::session::{Runtime, Session, SessionState};
use senate_bridge::state::QueueCounts;
use senate_core::{
    Address, Awaiting, CheckpointFacts, CommitteeId, DocketItem, DocketPage, Event, EventId, Kind,
    Locale, Name, SeatFacts, SenatorPath, Seq, SpeakerSeat, StatusState, ThreadSummary,
    TriageVerdict,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

pub fn committee() -> CommitteeId {
    CommitteeId::new("refund-v2").unwrap()
}

pub fn path(s: &str) -> SenatorPath {
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
    }
}

/// The daemon's status: the primary Senate, and the one `alice/claude`
/// presides over under it.
pub fn status(sessions: Vec<Session>) -> StatusReply {
    let below = StatusReply {
        pid: 4242,
        version: "0.1.0".into(),
        person: alice(),
        presider: Some(path("alice/claude")),
        locale: Locale::En,
        server_url: "http://127.0.0.1:7001".into(),
        committees: vec![stream("refund-v2", "alice/claude/reviewer", 1)],
        sessions: vec![claude_session("rev-1", "alice/claude/reviewer")],
        queue: QueueCounts::default(),
        speaker: Some(SpeakerSeat::Senator(path("alice/claude"))),
        others: Vec::new(),
    };
    StatusReply {
        pid: 4242,
        version: "0.1.0".into(),
        person: alice(),
        presider: None,
        locale: Locale::En,
        server_url: "http://senate.test".into(),
        committees: vec![
            stream("refund-v2", "alice/claude", 1),
            stream("refund-v2", "alice/codex", sessions.len()),
        ],
        sessions: {
            let mut all = vec![claude_session("cl-1", "alice/claude")];
            all.extend(sessions);
            all
        },
        queue: QueueCounts::default(),
        speaker: Some(SpeakerSeat::BuiltIn),
        others: vec![below],
    }
}

/// `status` without the nested Senate: one Senate, nobody presiding.
pub fn status_alone(sessions: Vec<Session>) -> StatusReply {
    let mut s = status(sessions);
    s.others.clear();
    s.speaker = None;
    s
}

/// A thread addressed to `agent`, parked on the person.
pub fn suspended(root: u64, title: &str, agent: &str) -> ThreadSummary {
    ThreadSummary {
        root: Seq(root),
        kind: Kind::Question,
        from: Address::Speaker,
        on_behalf_of: Some(Address::senator(&path("bob/claude"))),
        to: vec![Address::senator(&path(agent))],
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
        branch: Some("feature/refund-v2/alice".into()),
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

/// A live Codex thread of `alice/codex` on [`committee`].
pub fn codex_session(thread: &str) -> Session {
    session(thread, Runtime::Codex, "alice/codex")
}

/// One item of each kind awaiting the Speaker `alice/claude` in the Senate
/// below.
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
    let reviewer = path("alice/claude/reviewer");
    let items = vec![
        DocketItem {
            event: event(
                40,
                Address::senior(&alice()),
                vec![Address::Speaker],
                "谁来看一下 [senator::alice/claude/reviewer] 的补丁？",
            ),
            awaiting: Awaiting::Pick {
                owner: alice(),
                candidates: vec![reviewer.clone(), path("alice/claude/tester")],
            },
        },
        DocketItem {
            event: event(
                44,
                Address::senator(&path("alice/claude/tester")),
                vec![Address::senator(&reviewer)],
                "Is the lint config shared?",
            ),
            awaiting: Awaiting::Away {
                agents: vec![reviewer.clone()],
            },
        },
    ];
    DocketReply {
        speaker: None,
        senates: vec![SenateDocket {
            senate: Some(path("alice/claude")),
            pages: vec![DocketPage {
                committee: committee(),
                speaker: Some(SpeakerSeat::Senator(path("alice/claude"))),
                items,
                seats: vec![SeatFacts {
                    agent: reviewer,
                    connected: false,
                    presence: None,
                    last_seen: None,
                    checkpoint: Some(CheckpointFacts {
                        seq: Seq(31),
                        ts: Utc::now(),
                        summary: "lint config moved to the repo root".into(),
                        branch: "feature/refund-v2/alice".into(),
                    }),
                    open_threads: 1,
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
                suspended(118, "退款要不要走审批？", "alice/claude"),
                suspended(131, "which env?", "alice/codex"),
            ],
            approvals: vec![approval("019a-thread:7")],
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
            let agent = path(&format!("alice/{name}"));
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
                agent: path("alice/claude"),
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

/// A socket path in a fresh temp dir (short enough to bind), and a world.
pub fn setup() -> (tempfile::TempDir, PathBuf, Arc<Mutex<World>>) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("bridge.sock");
    (dir, socket, Arc::new(Mutex::new(World::new())))
}
