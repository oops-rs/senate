//! The approvals relay: Codex app-server approval requests,
//! offered to the bridge's IPC clients (the desktop app) as well as to the
//! TUI. The first answer wins.
//!
//! For each live Codex session the daemon keeps one persistent app-server
//! connection (the pushes of [`crate::delivery::codex`] stay one connection
//! each). After `initialize` it sends `thread/resume {threadId,
//! excludeTurns: true}`, which for a running thread rejoins it rather than
//! loading anything, so the connection receives the thread's events,
//! including the app-server's *server requests* for approval:
//!
//! | method | params | `result` (see [`decision_value`]) |
//! | --- | --- | --- |
//! | `item/commandExecution/requestApproval` | `threadId`, `turnId`, `itemId`, `command?`, `cwd?`, `reason?` | `{decision: accept \| acceptForSession \| decline \| cancel}` |
//! | `item/fileChange/requestApproval` | `threadId`, `turnId`, `itemId`, `reason?`, `grantRoot?` | same |
//! | `applyPatchApproval` (legacy) | `conversationId`, `callId`, `fileChanges`, `reason?` | `{decision: approved \| approved_for_session \| {denied: {rejection}} \| abort}` |
//! | `execCommandApproval` (legacy) | `conversationId`, `callId`, `command`, `cwd`, `reason?` | same |
//!
//! Each becomes a pending [`Approval`] with id `<thread>:<request id>`.
//! [`Relay::answer`] sends the JSON-RPC response on that connection. When
//! any client answers, the app-server notifies every client with
//! `serverRequest/resolved {threadId, requestId}`; the relay then drops the
//! approval, so an answer from the TUI shows up as
//! [`ResolvedBy::Elsewhere`] and a late answer from the app is refused as
//! already resolved. A dropped connection drops its approvals
//! ([`ResolvedBy::Gone`]) and is retried with backoff while the session is
//! live. Other server requests (user input, elicitations, permission
//! profiles) are left to the TUI.
//!
//! Verified against the app-server's published JSON schema and a fake; not
//! yet against a live codex-cli (whether `thread/resume` from a second
//! client is what subscribes it, and whether requests already pending when
//! it connects are re-sent).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use crate::delivery::codex::ws::{Incoming, WsWriter};
use crate::delivery::codex::{AppServer, CodexError, Transport};
use crate::ipc::WatchEvent;

/// Server-request methods relayed.
pub const APPROVAL_METHODS: [&str; 4] = [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "applyPatchApproval",
    "execCommandApproval",
];
/// The notification that a server request was answered (by anyone).
pub const RESOLVED: &str = "serverRequest/resolved";
/// Bound on connecting and subscribing.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    /// Run a command (`item/commandExecution/requestApproval`).
    Command,
    /// Change files (`item/fileChange/requestApproval`).
    FileChange,
    /// Legacy `applyPatchApproval`.
    Patch,
    /// Legacy `execCommandApproval`.
    Exec,
}

impl ApprovalKind {
    fn of(method: &str) -> Option<Self> {
        match method {
            "item/commandExecution/requestApproval" => Some(Self::Command),
            "item/fileChange/requestApproval" => Some(Self::FileChange),
            "applyPatchApproval" => Some(Self::Patch),
            "execCommandApproval" => Some(Self::Exec),
            _ => None,
        }
    }

    fn legacy(self) -> bool {
        matches!(self, Self::Patch | Self::Exec)
    }
}

/// A Codex approval request waiting for an answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    /// `<thread>:<JSON-RPC request id>`.
    pub id: String,
    /// The Codex session (thread) asking.
    pub session_id: String,
    pub kind: ApprovalKind,
    pub method: String,
    /// One line for a list: `run: cargo test`, `apply patch: 2 files`.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Files a patch touches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// The request's params as Codex sent them.
    pub params: Value,
    pub received_at: DateTime<Utc>,
}

/// An answer to an [`Approval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    /// Allow, and stop asking for the same thing this session.
    AllowForSession,
    /// Refuse; the turn goes on.
    Deny,
    /// Refuse and interrupt the turn.
    Cancel,
}

/// Who answered an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    /// An IPC client of this bridge (`approve`).
    Bridge,
    /// The TUI, or another app-server client.
    Elsewhere,
    /// The connection or the session went away first.
    Gone,
}

/// The JSON-RPC `result` that answers `method` with `decision`.
pub fn decision_value(kind: ApprovalKind, decision: Decision) -> Value {
    let decision = if kind.legacy() {
        match decision {
            Decision::Allow => json!("approved"),
            Decision::AllowForSession => json!("approved_for_session"),
            Decision::Deny => {
                json!({"denied": {"rejection": "denied from the Senate desktop app"}})
            }
            Decision::Cancel => json!("abort"),
        }
    } else {
        match decision {
            Decision::Allow => json!("accept"),
            Decision::AllowForSession => json!("acceptForSession"),
            Decision::Deny => json!("decline"),
            Decision::Cancel => json!("cancel"),
        }
    };
    json!({ "decision": decision })
}

/// `<thread>:<request id>` (a string id as is, a number in decimal).
pub fn approval_id(thread: &str, rpc_id: &Value) -> String {
    match rpc_id {
        Value::String(s) => format!("{thread}:{s}"),
        other => format!("{thread}:{other}"),
    }
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            (!words.is_empty()).then(|| words.join(" "))
        }
        _ => None,
    }
}

/// The approval a server request asks for, if it is one.
pub fn parse_request(thread: &str, msg: &Value) -> Option<(Value, Approval)> {
    let method = msg.get("method")?.as_str()?;
    let kind = ApprovalKind::of(method)?;
    let rpc_id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let command = params.get("command").and_then(text);
    let cwd = params.get("cwd").and_then(text);
    let reason = params.get("reason").and_then(text);
    let files: Vec<String> = params
        .get("fileChanges")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let summary = match kind {
        ApprovalKind::Command | ApprovalKind::Exec => {
            format!("run: {}", command.as_deref().unwrap_or("a command"))
        }
        ApprovalKind::FileChange => match params.get("grantRoot").and_then(text) {
            Some(root) => format!("change files (write access under {root})"),
            None => "change files".to_string(),
        },
        ApprovalKind::Patch => match files.len() {
            0 => "apply a patch".to_string(),
            1 => format!("apply a patch to {}", files[0]),
            n => format!("apply a patch to {n} files"),
        },
    };
    Some((
        rpc_id.clone(),
        Approval {
            id: approval_id(thread, &rpc_id),
            session_id: thread.to_string(),
            kind,
            method: method.to_string(),
            summary,
            command,
            cwd,
            reason,
            files,
            params,
            received_at: Utc::now(),
        },
    ))
}

struct Pending {
    approval: Approval,
    rpc_id: Value,
    writer: Arc<tokio::sync::Mutex<WsWriter>>,
}

/// The pending approvals of every live Codex session, and the connections
/// that hear them.
pub struct Relay {
    transport: Transport,
    backoff: (Duration, Duration),
    events: broadcast::Sender<WatchEvent>,
    pending: Mutex<BTreeMap<String, Pending>>,
    subscribers: Mutex<HashMap<String, JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Relay {
    /// `backoff`: first and longest wait between reconnects.
    pub fn new(
        transport: Transport,
        backoff: (Duration, Duration),
        events: broadcast::Sender<WatchEvent>,
    ) -> Self {
        Self {
            transport,
            backoff,
            events,
            pending: Mutex::new(BTreeMap::new()),
            subscribers: Mutex::new(HashMap::new()),
        }
    }

    fn publish(&self, event: WatchEvent) {
        // No watcher is not an error.
        let _ = self.events.send(event);
    }

    /// Keep one subscriber per session in `sessions`, and only those.
    pub fn reconcile(self: &Arc<Self>, sessions: &[String]) {
        let mut subscribers = lock(&self.subscribers);
        let gone: Vec<String> = subscribers
            .keys()
            .filter(|s| !sessions.contains(s))
            .cloned()
            .collect();
        for session in gone {
            if let Some(task) = subscribers.remove(&session) {
                task.abort();
            }
            self.drop_session(&session);
        }
        for session in sessions {
            subscribers
                .entry(session.clone())
                .or_insert_with(|| tokio::spawn(self.clone().subscribe(session.clone())));
        }
    }

    /// Stop every subscriber (daemon shutdown).
    pub fn stop_all(&self) {
        for (_, task) in lock(&self.subscribers).drain() {
            task.abort();
        }
    }

    /// Pending approvals, oldest first.
    pub fn list(&self) -> Vec<Approval> {
        let mut out: Vec<Approval> = lock(&self.pending)
            .values()
            .map(|p| p.approval.clone())
            .collect();
        out.sort_by_key(|a| a.received_at);
        out
    }

    /// Answer approval `id`. Fails when it is not pending (answered
    /// elsewhere first, or its session went away).
    pub async fn answer(&self, id: &str, decision: Decision) -> Result<Approval, String> {
        let Some(pending) = lock(&self.pending).remove(id) else {
            return Err(format!(
                "no pending approval {id}: it was answered elsewhere first, or its session is gone"
            ));
        };
        let response = json!({
            "jsonrpc": "2.0",
            "id": pending.rpc_id,
            "result": decision_value(pending.approval.kind, decision),
        });
        let sent = pending
            .writer
            .lock()
            .await
            .send_text(&response.to_string())
            .await;
        match sent {
            Ok(()) => {
                self.publish(WatchEvent::ApprovalResolved {
                    id: id.to_string(),
                    session_id: pending.approval.session_id.clone(),
                    by: ResolvedBy::Bridge,
                    decision: Some(decision),
                });
                Ok(pending.approval)
            }
            Err(e) => {
                self.publish(WatchEvent::ApprovalResolved {
                    id: id.to_string(),
                    session_id: pending.approval.session_id.clone(),
                    by: ResolvedBy::Gone,
                    decision: None,
                });
                Err(format!("the Codex app-server connection failed: {e}"))
            }
        }
    }

    fn drop_session(&self, session: &str) {
        let dropped: Vec<Approval> = {
            let mut pending = lock(&self.pending);
            let ids: Vec<String> = pending
                .iter()
                .filter(|(_, p)| p.approval.session_id == session)
                .map(|(id, _)| id.clone())
                .collect();
            ids.iter()
                .filter_map(|id| pending.remove(id))
                .map(|p| p.approval)
                .collect()
        };
        for approval in dropped {
            self.publish(WatchEvent::ApprovalResolved {
                id: approval.id,
                session_id: approval.session_id,
                by: ResolvedBy::Gone,
                decision: None,
            });
        }
    }

    async fn subscribe(self: Arc<Self>, session: String) {
        let (base, max) = self.backoff;
        let mut delay = base;
        loop {
            match self.listen(&session).await {
                Ok(()) => {
                    tracing::debug!(session, "the Codex approvals connection closed");
                    delay = base;
                }
                Err(e) => tracing::debug!(session, "no Codex approvals connection: {e}"),
            }
            self.drop_session(&session);
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(max);
        }
    }

    /// One connection: subscribe to the thread, then relay until it ends.
    async fn listen(&self, thread: &str) -> Result<(), CodexError> {
        let connect = async {
            let server = AppServer::connect(&self.transport).await?;
            server.subscribe(thread).await
        };
        let mut sub = tokio::time::timeout(SUBSCRIBE_TIMEOUT, connect)
            .await
            .map_err(|_| CodexError::Timeout)??;
        tracing::debug!(thread, "listening for Codex approvals");
        let writer = Arc::new(tokio::sync::Mutex::new(sub.writer));
        loop {
            match sub.reader.next().await? {
                Incoming::Text(text) => self.on_message(thread, &text, &writer),
                Incoming::Ping(payload) => writer.lock().await.pong(&payload).await?,
                Incoming::Close(payload) => {
                    let _ = writer.lock().await.close(&payload).await;
                    return Ok(());
                }
            }
        }
    }

    fn on_message(&self, thread: &str, text: &str, writer: &Arc<tokio::sync::Mutex<WsWriter>>) {
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let method = msg.get("method").and_then(Value::as_str);
        if method == Some(RESOLVED) {
            let params = msg.get("params").cloned().unwrap_or(Value::Null);
            let of = params
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or(thread);
            let Some(request) = params.get("requestId") else {
                return;
            };
            let id = approval_id(of, request);
            if let Some(p) = lock(&self.pending).remove(&id) {
                tracing::info!(id, "a Codex approval was answered elsewhere");
                self.publish(WatchEvent::ApprovalResolved {
                    id,
                    session_id: p.approval.session_id,
                    by: ResolvedBy::Elsewhere,
                    decision: None,
                });
            }
            return;
        }
        if let Some((rpc_id, approval)) = parse_request(thread, &msg) {
            tracing::info!(id = %approval.id, summary = %approval.summary, "a Codex approval is waiting");
            lock(&self.pending).insert(
                approval.id.clone(),
                Pending {
                    approval: approval.clone(),
                    rpc_id,
                    writer: writer.clone(),
                },
            );
            self.publish(WatchEvent::ApprovalRequested(Box::new(approval)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_parse_and_decisions_match_the_schema() {
        let msg = json!({"id": 7, "method": "item/commandExecution/requestApproval", "params": {
            "threadId": "t1", "turnId": "u", "itemId": "i", "startedAtMs": 1,
            "command": "cargo test", "cwd": "/w", "reason": null}});
        let (id, a) = parse_request("t1", &msg).unwrap();
        assert_eq!(id, json!(7));
        assert_eq!(a.id, "t1:7");
        assert_eq!(a.summary, "run: cargo test");
        assert_eq!(a.reason, None);
        assert_eq!(
            decision_value(a.kind, Decision::Deny),
            json!({"decision": "decline"})
        );

        let msg = json!({"id": "r-1", "method": "applyPatchApproval", "params": {
            "conversationId": "t1", "callId": "c",
            "fileChanges": {"src/a.rs": {}, "src/b.rs": {}}}});
        let (_, a) = parse_request("t1", &msg).unwrap();
        assert_eq!(a.id, "t1:r-1");
        assert_eq!(a.summary, "apply a patch to 2 files");
        assert_eq!(
            decision_value(a.kind, Decision::Deny)["decision"]["denied"]["rejection"],
            "denied from the Senate desktop app"
        );
        assert_eq!(
            decision_value(a.kind, Decision::AllowForSession),
            json!({"decision": "approved_for_session"})
        );
        let msg = json!({"id": 1, "method": "execCommandApproval", "params": {
            "conversationId": "t1", "callId": "c", "command": ["git", "push"], "cwd": "/w",
            "parsedCmd": []}});
        assert_eq!(
            parse_request("t1", &msg).unwrap().1.command.as_deref(),
            Some("git push")
        );

        let other = json!({"id": 2, "method": "item/tool/requestUserInput", "params": {}});
        assert!(parse_request("t1", &other).is_none());
        let notification = json!({"method": "turn/started", "params": {}});
        assert!(parse_request("t1", &notification).is_none());
    }
}
