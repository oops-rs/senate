//! The Codex app-server adapter: push a batch into a Codex
//! thread as a second client of the app-server the developer's TUI
//! (`codex --remote`) is attached to.
//!
//! # Protocol, as observed against codex-cli 0.167
//!
//! The app-server's control socket (`codex app-server --listen unix://…`,
//! and the managed daemon of `codex app-server daemon start`) speaks
//! **WebSocket**, not newline-delimited JSON: an HTTP/1.1 upgrade, then one
//! JSON-RPC 2.0 message per text frame (the `jsonrpc` field is optional).
//! `codex app-server proxy` only tunnels bytes between stdio and that socket,
//! so the WebSocket framing is spoken through it too. [`ws`] is the minimal
//! client this needs.
//!
//! Per connection: `initialize` {clientInfo}, then the `initialized`
//! notification. Per push:
//!
//! 1. `thread/read` {threadId} → `thread.status.type`: `idle`, `active`
//!    (with `activeFlags`), `notLoaded` or `systemError`. A thread the
//!    app-server never loaded fails with `thread not loaded: <id>`.
//! 2. `idle` → `turn/start` {threadId, input: [{type: "text", text}]}.
//! 3. `active` → `thread/turns/list` {threadId, limit: 1, itemsView:
//!    "notLoaded"} (newest first) gives the running turn's id, then
//!    `turn/steer` {threadId, expectedTurnId, input}. A wrong id fails with
//!    `expected active turn id `x` but found `y``; a turn that ended
//!    meanwhile with `no active turn to steer`.
//! 4. A start or steer that fails is retried once from step 1.
//!
//! A thread stays loaded when the client that started it disconnects, and a
//! fresh connection may start or steer it without subscribing to it. (A
//! `turn/start` on a thread that is already active adds the input to the
//! running turn, so the race between step 1 and step 2 is harmless.)
//!
//! # Outcomes
//!
//! - Started or steered → `Injected`.
//! - The thread is not loaded (its TUI is gone) → `Unavailable` with
//!   `unreachable`: the session is marked stale and its items wait until
//!   the thread is resumed (it runs SessionStart again), then are pushed.
//! - No app-server, a protocol error, `systemError`, a timeout →
//!   `Unavailable`, not unreachable: the ladder's next rung holds the batch
//!   for the session and the daemon retries with backoff; each hook of the
//!   session retries it again. A Codex hook never hands items over as
//!   context: Codex leaves that context unanswered. A timeout
//!   after the request was written may mean the turn did start; the batch is
//!   then delivered twice rather than lost.
//!
//! The session-start brief needs no `thread/inject_items`: Codex runs the
//! SessionStart hook with the same `additionalContext` output as Claude Code.

pub mod ws;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Child;

use super::{Batch, DeliveryAdapter, DeliveryOutcome};
use crate::session::{Runtime, Session};
use ws::WebSocket;

/// `clientInfo.name` the bridge introduces itself with.
pub const CLIENT_NAME: &str = "senate";

/// `[codex]` in `senate.toml`.
///
/// ```toml
/// [codex]
/// # Default: spawn `codex app-server proxy` (the managed daemon).
/// listen = "unix:///path/to/app-server.sock"
/// # The codex CLI used for the proxy (default: `codex` on PATH).
/// bin = "/opt/homebrew/bin/codex"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CodexConfig {
    #[serde(default)]
    pub listen: Option<String>,
    #[serde(default)]
    pub bin: Option<PathBuf>,
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    codex: CodexConfig,
}

impl CodexConfig {
    /// The `[codex]` table of the bridge config at `path`; the defaults when
    /// the file or the table is missing.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|e| format!("parsing [codex] in {}: {e}", path.display()))?;
        file.codex.transport()?;
        Ok(file.codex)
    }

    pub fn transport(&self) -> Result<Transport, String> {
        match self.listen.as_deref().map(str::trim) {
            None | Some("") => Ok(Transport::Proxy {
                program: self.bin.clone().unwrap_or_else(|| PathBuf::from("codex")),
            }),
            Some(url) => match url.strip_prefix("unix://") {
                Some(path) if path.starts_with('/') => Ok(Transport::Unix(PathBuf::from(path))),
                _ => Err(format!(
                    "codex.listen must be unix:///absolute/path, got {url:?}"
                )),
            },
        }
    }
}

/// How the bridge reaches the app-server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// `<program> app-server proxy`: stdio tunnelled to the running managed
    /// daemon's control socket, wherever Codex keeps it.
    Proxy { program: PathBuf },
    /// The control socket itself.
    Unix(PathBuf),
}

impl Transport {
    pub(crate) fn remote(&self) -> String {
        match self {
            Self::Unix(path) => format!("unix://{}", path.display()),
            Self::Proxy { .. } => "unix://".into(),
        }
    }
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Proxy { program } => write!(f, "{} app-server proxy", program.display()),
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CodexError {
    #[error("cannot reach the Codex app-server ({transport}): {reason}")]
    Connect { transport: String, reason: String },
    #[error("Codex app-server connection: {0}")]
    Io(#[from] std::io::Error),
    #[error("Codex app-server connection: {0}")]
    Protocol(String),
    #[error("Codex app-server answered {method} with error {code}: {message}")]
    Rpc {
        method: String,
        code: i64,
        message: String,
    },
    #[error("Codex thread {0} is not loaded in the app-server (its TUI is gone)")]
    NotLoaded(String),
    #[error("Codex thread {0} is in systemError")]
    SystemError(String),
    #[error("Codex app-server did not answer in time")]
    Timeout,
}

impl CodexError {
    fn from_rpc(method: &str, thread: &str, error: &Value) -> Self {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if message.starts_with("thread not loaded") || message.starts_with("thread not found") {
            return Self::NotLoaded(thread.to_string());
        }
        Self::Rpc {
            method: method.to_string(),
            code,
            message,
        }
    }
}

/// `thread.status.type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadState {
    Idle,
    Active { flags: Vec<String> },
    NotLoaded,
    SystemError,
    Other(String),
}

impl ThreadState {
    fn parse(status: &Value) -> Self {
        match status.get("type").and_then(Value::as_str).unwrap_or("") {
            "idle" => Self::Idle,
            "active" => Self::Active {
                flags: status
                    .get("activeFlags")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            "notLoaded" => Self::NotLoaded,
            "systemError" => Self::SystemError,
            other => Self::Other(other.to_string()),
        }
    }
}

/// What a push did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pushed {
    /// The thread was idle: a new turn.
    Started { turn: String },
    /// The thread was busy: the text joined its running turn.
    Steered { turn: String },
}

/// One JSON-RPC connection to the app-server.
pub struct AppServer {
    ws: WebSocket,
    next_id: u64,
    /// The proxy process, killed when the connection is dropped.
    _child: Option<Child>,
}

impl AppServer {
    /// Connect, upgrade, `initialize`.
    pub async fn connect(transport: &Transport) -> Result<Self, CodexError> {
        let connect_error = |reason: String| CodexError::Connect {
            transport: transport.to_string(),
            reason,
        };
        let (ws, child) = match transport {
            Transport::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(|e| connect_error(e.to_string()))?;
                let ws = WebSocket::handshake(Box::new(stream))
                    .await
                    .map_err(|e| connect_error(e.to_string()))?;
                (ws, None)
            }
            Transport::Proxy { program } => {
                let mut child = tokio::process::Command::new(program)
                    .args(["app-server", "proxy"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|e| connect_error(e.to_string()))?;
                let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
                    return Err(connect_error("the proxy has no stdio".into()));
                };
                // With no daemon running, the proxy exits and the upgrade
                // reads end of file.
                let ws = WebSocket::handshake(Box::new(tokio::io::join(stdout, stdin)))
                    .await
                    .map_err(|e| connect_error(e.to_string()))?;
                (ws, Some(child))
            }
        };
        let mut server = Self {
            ws,
            next_id: 1,
            _child: child,
        };
        server
            .request(
                "initialize",
                "",
                json!({"clientInfo": {"name": CLIENT_NAME, "version": env!("CARGO_PKG_VERSION")}}),
            )
            .await?;
        server.notify("initialized", json!({})).await?;
        Ok(server)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), CodexError> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.ws.send_text(&msg.to_string()).await?;
        Ok(())
    }

    /// One request; notifications and server requests that arrive first
    /// are skipped. `thread` names the thread errors are about.
    async fn request(
        &mut self,
        method: &str,
        thread: &str,
        params: Value,
    ) -> Result<Value, CodexError> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.ws.send_text(&msg.to_string()).await?;
        loop {
            let text = self.ws.recv_text().await?;
            let Ok(reply) = serde_json::from_str::<Value>(&text) else {
                return Err(CodexError::Protocol(format!(
                    "not JSON: {}",
                    text.chars().take(200).collect::<String>()
                )));
            };
            if reply.get("method").is_some() || reply.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(error) = reply.get("error") {
                return Err(CodexError::from_rpc(method, thread, error));
            }
            return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Create a resumable thread without sending a model turn.
    pub(crate) async fn start_hosted_thread(&mut self, cwd: &Path) -> Result<String, CodexError> {
        let result = self
            .request("thread/start", "", json!({"cwd": cwd}))
            .await?;
        let id = result
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| CodexError::Protocol("thread/start returned no thread ID".into()))?;
        // A newly created thread needs history for the TUI's resume bootstrap.
        if let Err(error) = self
            .request(
                "thread/inject_items",
                id,
                json!({
                    "threadId": id,
                    "items": [{
                        "type": "message",
                        "role": "developer",
                        "content": [{"type": "input_text", "text": "Senate started this session."}]
                    }]
                }),
            )
            .await
        {
            if let Err(archive) = self.archive_thread(id).await {
                tracing::warn!(%archive, thread = %id, "could not archive a failed hosted thread");
            }
            return Err(error);
        }
        Ok(id.into())
    }

    pub(crate) async fn archive_thread(&mut self, id: &str) -> Result<(), CodexError> {
        self.request("thread/archive", id, json!({"threadId": id}))
            .await?;
        Ok(())
    }

    pub async fn thread_state(&mut self, thread: &str) -> Result<ThreadState, CodexError> {
        let r = self
            .request("thread/read", thread, json!({"threadId": thread}))
            .await?;
        let status = r
            .pointer("/thread/status")
            .ok_or_else(|| CodexError::Protocol("thread/read: no thread.status".into()))?;
        Ok(ThreadState::parse(status))
    }

    /// The running turn's id, newest turn first.
    pub async fn active_turn(&mut self, thread: &str) -> Result<Option<String>, CodexError> {
        let r = self
            .request(
                "thread/turns/list",
                thread,
                json!({"threadId": thread, "limit": 1, "itemsView": "notLoaded"}),
            )
            .await?;
        Ok(r.get("data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|t| t.get("status").and_then(Value::as_str) == Some("inProgress"))
            .and_then(|t| t.get("id")?.as_str())
            .map(str::to_string))
    }

    pub async fn turn_start(&mut self, thread: &str, text: &str) -> Result<String, CodexError> {
        let r = self
            .request(
                "turn/start",
                thread,
                json!({"threadId": thread, "input": [{"type": "text", "text": text}]}),
            )
            .await?;
        Ok(r.pointer("/turn/id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string())
    }

    pub async fn turn_steer(
        &mut self,
        thread: &str,
        turn: &str,
        text: &str,
    ) -> Result<String, CodexError> {
        let r = self
            .request(
                "turn/steer",
                thread,
                json!({
                    "threadId": thread,
                    "expectedTurnId": turn,
                    "input": [{"type": "text", "text": text}],
                }),
            )
            .await?;
        Ok(r.get("turnId")
            .and_then(Value::as_str)
            .unwrap_or(turn)
            .to_string())
    }

    /// Put `text` into the thread: a new turn when idle, the running turn
    /// when busy. A start or steer the app-server refuses (the turn changed
    /// under us) is retried once from a fresh read.
    pub async fn push(&mut self, thread: &str, text: &str) -> Result<Pushed, CodexError> {
        let mut retried = false;
        loop {
            let attempt = match self.thread_state(thread).await? {
                ThreadState::Idle => self
                    .turn_start(thread, text)
                    .await
                    .map(|turn| Pushed::Started { turn }),
                ThreadState::Active { .. } => match self.active_turn(thread).await? {
                    Some(turn) => self
                        .turn_steer(thread, &turn, text)
                        .await
                        .map(|turn| Pushed::Steered { turn }),
                    // Between turns: read again.
                    None => Err(CodexError::Protocol(format!(
                        "thread {thread} is active but has no turn in progress"
                    ))),
                },
                ThreadState::NotLoaded => return Err(CodexError::NotLoaded(thread.to_string())),
                ThreadState::SystemError => {
                    return Err(CodexError::SystemError(thread.to_string()));
                }
                ThreadState::Other(s) => {
                    return Err(CodexError::Protocol(format!(
                        "thread {thread} has an unknown status {s:?}"
                    )));
                }
            };
            match attempt {
                Err(e @ (CodexError::Rpc { .. } | CodexError::Protocol(_))) if !retried => {
                    tracing::debug!(thread, "Codex push refused ({e}); reading the thread again");
                    retried = true;
                }
                other => return other,
            }
        }
    }
}

/// A connection subscribed to one thread ([`AppServer::subscribe`]), split
/// so one task reads while answers are written.
pub struct Subscription {
    pub reader: ws::WsReader,
    pub writer: ws::WsWriter,
    _child: Option<Child>,
}

impl AppServer {
    /// `thread/resume {threadId, excludeTurns: true}`: rejoin the running
    /// thread, so this connection hears its events and server requests
    /// ([`crate::approvals`]).
    pub async fn subscribe(mut self, thread: &str) -> Result<Subscription, CodexError> {
        self.request(
            "thread/resume",
            thread,
            json!({"threadId": thread, "excludeTurns": true}),
        )
        .await?;
        let (reader, writer) = self.ws.into_split();
        Ok(Subscription {
            reader,
            writer,
            _child: self._child,
        })
    }
}

/// Connect, push, disconnect, all within `timeout`.
pub async fn push_once(
    transport: &Transport,
    thread: &str,
    text: &str,
    timeout: Duration,
) -> Result<Pushed, CodexError> {
    let run = async {
        let mut server = AppServer::connect(transport).await?;
        server.push(thread, text).await
    };
    tokio::time::timeout(timeout, run)
        .await
        .unwrap_or(Err(CodexError::Timeout))
}

/// The ladder rung for Codex sessions.
#[derive(Debug, Clone)]
pub struct CodexAppServerAdapter {
    pub transport: Transport,
    /// Bound on one whole push (connect, read, start or steer).
    pub timeout: Duration,
}

#[async_trait]
impl DeliveryAdapter for CodexAppServerAdapter {
    fn name(&self) -> &'static str {
        "codex-app-server"
    }

    fn can_push(&self, session: &Session) -> bool {
        session.runtime == Runtime::Codex
    }

    async fn deliver(&self, session: &Session, batch: &Batch) -> DeliveryOutcome {
        if session.runtime != Runtime::Codex {
            return DeliveryOutcome::unavailable("not a Codex session");
        }
        match push_once(
            &self.transport,
            &session.session_id,
            &batch.text,
            self.timeout,
        )
        .await
        {
            Ok(pushed) => {
                tracing::debug!(session = %session.session_id, ?pushed, "pushed into Codex");
                DeliveryOutcome::Injected
            }
            Err(e @ CodexError::NotLoaded(_)) => DeliveryOutcome::Unavailable {
                reason: e.to_string(),
                unreachable: true,
            },
            Err(e) => DeliveryOutcome::unavailable(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_transport() {
        let c: ConfigFile = toml::from_str("server_url = \"x\"\n").unwrap();
        assert_eq!(
            c.codex.transport().unwrap(),
            Transport::Proxy {
                program: "codex".into()
            }
        );
        let c: ConfigFile =
            toml::from_str("[codex]\nlisten = \"unix:///tmp/cx.sock\"\nbin = \"/opt/codex\"\n")
                .unwrap();
        assert_eq!(
            c.codex.transport().unwrap(),
            Transport::Unix("/tmp/cx.sock".into())
        );
        let bad = CodexConfig {
            listen: Some("ws://127.0.0.1:1".into()),
            bin: None,
        };
        assert!(bad.transport().is_err());
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            CodexConfig::load(&dir.path().join("none.toml")).unwrap(),
            CodexConfig::default()
        );
    }

    #[test]
    fn thread_states() {
        assert_eq!(
            ThreadState::parse(&json!({"type": "idle"})),
            ThreadState::Idle
        );
        assert_eq!(
            ThreadState::parse(&json!({"type": "active", "activeFlags": ["waitingOnApproval"]})),
            ThreadState::Active {
                flags: vec!["waitingOnApproval".into()]
            }
        );
        assert_eq!(
            ThreadState::parse(&json!({"type": "notLoaded"})),
            ThreadState::NotLoaded
        );
        assert!(matches!(
            CodexError::from_rpc("thread/read", "t", &json!({"code": -32600, "message": "thread not loaded: t"})),
            CodexError::NotLoaded(t) if t == "t"
        ));
    }
}
