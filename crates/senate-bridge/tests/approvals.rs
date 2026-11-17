//! The approvals relay and `watch`, against the fake Codex app-server.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;

use common::*;
use senate_bridge::approvals::{ApprovalKind, Decision, ResolvedBy};
use senate_bridge::hook::HookKind;
use senate_bridge::ipc::{self, ApprovalsReply, ApproveReply, Request, WatchEvent};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// A `watch` connection.
struct Watcher {
    lines: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    _write: tokio::net::unix::OwnedWriteHalf,
}

impl Watcher {
    async fn open(socket: &Path) -> Self {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(b"{\"op\":\"watch\"}\n").await.unwrap();
        let mut lines = BufReader::new(read).lines();
        let first: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(first, json!({"ok": true, "result": {"watching": true}}));
        Self {
            lines,
            _write: write,
        }
    }

    async fn next(&mut self) -> WatchEvent {
        let line = tokio::time::timeout(WAIT, self.lines.next_line())
            .await
            .expect("a watch event in time")
            .unwrap()
            .expect("the watch stream ended");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    }

    /// The next event `pick` accepts, skipping the rest (statuses, inbox).
    async fn wait<T>(&mut self, mut pick: impl FnMut(WatchEvent) -> Option<T>) -> T {
        loop {
            if let Some(t) = pick(self.next().await) {
                return t;
            }
        }
    }
}

async fn approvals(h: &Harness) -> ApprovalsReply {
    ipc::request(&h.socket(), &Request::Approvals, WAIT, WAIT)
        .await
        .unwrap()
}

async fn approve(h: &Harness, id: &str, decision: Decision) -> Result<ApproveReply, String> {
    ipc::request(
        &h.socket(),
        &Request::Approve {
            id: id.into(),
            decision,
        },
        WAIT,
        WAIT,
    )
    .await
    .map_err(|e| e.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn codex_approvals_reach_the_app_and_the_first_answer_wins() {
    let mut h = Harness::new().await;
    let codex = h.codex();
    codex.idle("thread-1");
    h.start().await;
    let wd = h.workdir("cx", Some("refund-v2"));
    h.hook_codex(HookKind::SessionStart, "thread-1", &wd).await;
    h.wait_stream("refund-v2").await;
    eventually("the relay subscribed", WAIT, || {
        (codex.subscribers("thread-1") == 1).then_some(())
    })
    .await;
    assert!(
        codex.methods().contains(&"thread/resume".to_string()),
        "{:?}",
        codex.methods()
    );

    let mut watch = Watcher::open(&h.socket()).await;
    let WatchEvent::Status(status) = watch.next().await else {
        panic!("a watch starts with the status");
    };
    assert_eq!(status.person.as_str(), "alice");

    // The app denies a command; Codex hears the decision.
    let first = codex.request_approval(
        "thread-1",
        "item/commandExecution/requestApproval",
        json!({"threadId": "thread-1", "turnId": "u1", "itemId": "i1", "startedAtMs": 1,
               "command": "rm -rf build", "cwd": "/w"}),
    );
    let approval = watch
        .wait(|e| match e {
            WatchEvent::ApprovalRequested(a) => Some(*a),
            _ => None,
        })
        .await;
    assert_eq!(approval.id, format!("thread-1:{first}"));
    assert_eq!(approval.kind, ApprovalKind::Command);
    assert_eq!(approval.summary, "run: rm -rf build");
    assert_eq!(
        approvals(&h).await.approvals,
        std::slice::from_ref(&approval)
    );
    let reply = approve(&h, &approval.id, Decision::Deny).await.unwrap();
    assert_eq!(reply.approval.id, approval.id);
    eventually("Codex got the answer", WAIT, || {
        (!codex.responses().is_empty()).then_some(())
    })
    .await;
    assert_eq!(
        codex.responses(),
        [(json!(first), json!({"decision": "decline"}))]
    );
    let (by, decision) = watch
        .wait(|e| match e {
            WatchEvent::ApprovalResolved { by, decision, .. } => Some((by, decision)),
            _ => None,
        })
        .await;
    assert_eq!((by, decision), (ResolvedBy::Bridge, Some(Decision::Deny)));
    assert!(approve(&h, &approval.id, Decision::Allow).await.is_err());

    // The TUI answers the next one first: the app sees it resolved, and
    // its own late answer is refused.
    let second = codex.request_approval(
        "thread-1",
        "item/fileChange/requestApproval",
        json!({"threadId": "thread-1", "turnId": "u1", "itemId": "i2", "startedAtMs": 2}),
    );
    let id = watch
        .wait(|e| match e {
            WatchEvent::ApprovalRequested(a) => Some(a.id),
            _ => None,
        })
        .await;
    assert_eq!(id, format!("thread-1:{second}"));
    assert!(codex.answer_in_tui(second));
    let by = watch
        .wait(|e| match e {
            WatchEvent::ApprovalResolved { id: rid, by, .. } if rid == id => Some(by),
            _ => None,
        })
        .await;
    assert_eq!(by, ResolvedBy::Elsewhere);
    assert!(approvals(&h).await.approvals.is_empty());
    let late = approve(&h, &id, Decision::Allow).await.unwrap_err();
    assert!(late.contains("answered elsewhere"), "{late}");
    assert_eq!(codex.responses().len(), 1, "nothing sent for it");

    // A legacy patch approval answers in the legacy vocabulary.
    let third = codex.request_approval(
        "thread-1",
        "applyPatchApproval",
        json!({"conversationId": "thread-1", "callId": "c", "fileChanges": {"a.rs": {}}}),
    );
    let id = watch
        .wait(|e| match e {
            WatchEvent::ApprovalRequested(a) => Some(a.id),
            _ => None,
        })
        .await;
    approve(&h, &id, Decision::AllowForSession).await.unwrap();
    eventually("the patch answer", WAIT, || {
        (codex.responses().len() == 2).then_some(())
    })
    .await;
    assert_eq!(
        codex.responses()[1],
        (json!(third), json!({"decision": "approved_for_session"}))
    );

    // A question for alice shows up as an inbox change.
    h.server
        .append(question(claude("bob"), claude("alice"), "在吗？"));
    let (committee, kind) = watch
        .wait(|e| match e {
            WatchEvent::Inbox {
                committee, kind, ..
            } => Some((committee, kind)),
            _ => None,
        })
        .await;
    assert_eq!(committee.as_str(), "refund-v2");
    assert_eq!(kind, senate_core::Kind::Question);

    // A watcher that joins late is told what is pending.
    let fourth = codex.request_approval(
        "thread-1",
        "execCommandApproval",
        json!({"conversationId": "thread-1", "callId": "c2", "command": ["git", "push"],
               "cwd": "/w", "parsedCmd": []}),
    );
    let id = watch
        .wait(|e| match e {
            WatchEvent::ApprovalRequested(a) => Some(a.id),
            _ => None,
        })
        .await;
    assert_eq!(id, format!("thread-1:{fourth}"));
    let mut late_watch = Watcher::open(&h.socket()).await;
    let summary = late_watch
        .wait(|e| match e {
            WatchEvent::ApprovalRequested(a) => Some(a.summary),
            _ => None,
        })
        .await;
    assert_eq!(summary, "run: git push");

    // The session ends: its approvals go with it.
    h.store()
        .set_session_state("thread-1", senate_bridge::SessionState::Ended)
        .unwrap();
    let by = late_watch
        .wait(|e| match e {
            WatchEvent::ApprovalResolved { by, .. } => Some(by),
            _ => None,
        })
        .await;
    assert_eq!(by, ResolvedBy::Gone);
    h.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_app_acts_on_a_committee_without_a_session() {
    let mut h = Harness::with_committees(&["refund-v2"]).await;
    h.start().await;
    // claude on record (its session ended): the configured committee
    // stays open for it, and the app acts with no session at all.
    let wd = h.workdir("w", Some("refund-v2"));
    h.hook(HookKind::SessionStart, "s0", &wd, None).await;
    h.hook(HookKind::SessionEnd, "s0", &wd, None).await;
    h.wait_stream("refund-v2").await;
    let q = h
        .server
        .append(question(claude("bob"), claude("alice"), "上限多少？"));
    let f = Some(committee());
    let none = ipc::SessionSelector::default();
    let inbox: ipc::InboxReply = ipc::request(
        &h.socket(),
        &Request::Inbox {
            session: none.clone(),
            committee: f.clone(),
            agent: None,
            all: false,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    assert_eq!(inbox.threads.len(), 1);
    assert_eq!(inbox.threads[0].root, q.seq);
    let posted: ipc::Posted = ipc::request(
        &h.socket(),
        &Request::Answer {
            session: none.clone(),
            thread: q.seq,
            body: "30 天".into(),
            id: None,
            committee: f.clone(),
            agent: None,
            as_person: true,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap();
    let answer = h.server.last_event("refund-v2");
    assert_eq!(answer.seq, posted.seq);
    assert_eq!(answer.from, person("alice"), "the person answered");
    assert_eq!(inbox.agents, [path("claude")]);

    // As an agent: the only one on the committee, or the one named.
    let as_agent = |agent: Option<&str>| Request::Post {
        session: ipc::SessionSelector::default(),
        body: "from the app".into(),
        proposal: false,
        committee: f.clone(),
        agent: agent.map(name),
        id: None,
    };
    let _: ipc::Posted = ipc::request(&h.socket(), &as_agent(None), WAIT, WAIT)
        .await
        .unwrap();
    assert_eq!(h.server.last_event("refund-v2").from, claude("alice"));
    let _: ipc::Posted = ipc::request(&h.socket(), &as_agent(Some("codex")), WAIT, WAIT)
        .await
        .unwrap();
    assert_eq!(h.server.last_event("refund-v2").from, codex("alice"));
    // Without a committee, a session is still required.
    let err = ipc::request::<ipc::InboxReply>(
        &h.socket(),
        &Request::Inbox {
            session: none,
            committee: None,
            agent: None,
            all: false,
        },
        WAIT,
        WAIT,
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not registered"), "{err}");
    h.stop().await;
}
