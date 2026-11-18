//! Delivery into the stdio connection owned by `senate acp`. The bridge
//! submits a prompt to a private, per-session Unix socket. The owner uses
//! standard ACP for the connection to the agent.

use std::ffi::OsString;
use std::io::{BufRead, IsTerminal, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use senate_core::EventId;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

use super::{Batch, DeliveryAdapter, DeliveryOutcome};
use crate::ipc::{self, HookEvent, HookReply, HookReport, Request};
use crate::paths::{HOME_ENV, Paths};
use crate::session::{self, Runtime, Session};

const LOCAL_TIMEOUT: Duration = Duration::from_secs(1);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(3);
const SETUP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize)]
struct Push {
    session_id: String,
    expires_at_ms: i64,
    text: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Receipt {
    Accepted,
    Busy,
    WrongSession,
}

#[derive(Clone, Debug)]
pub struct AcpAdapter {
    pub timeout: Duration,
}

#[async_trait]
impl DeliveryAdapter for AcpAdapter {
    fn name(&self) -> &'static str {
        "acp"
    }

    fn can_push(&self, session: &Session) -> bool {
        session.runtime == Runtime::Acp && session.socket.is_some()
    }

    async fn deliver(&self, session: &Session, batch: &Batch) -> DeliveryOutcome {
        if session.runtime != Runtime::Acp {
            return DeliveryOutcome::unavailable("not an ACP session");
        }
        let Some(path) = &session.socket else {
            return DeliveryOutcome::unavailable("no ACP client socket");
        };
        if !crate::testenv::confined(path) {
            return DeliveryOutcome::unavailable(
                "ACP socket is outside the isolated test directory",
            );
        }
        let send = async {
            let stream = UnixStream::connect(path).await.map_err(|e| {
                let gone = matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                );
                DeliveryOutcome::Unavailable {
                    reason: e.to_string(),
                    unreachable: gone,
                }
            })?;
            let mut stream = BufReader::new(stream);
            write_json(
                stream.get_mut(),
                &Push {
                    session_id: session.session_id.clone(),
                    expires_at_ms: chrono::Utc::now().timestamp_millis().saturating_add(
                        i64::try_from(self.timeout.as_millis()).unwrap_or(i64::MAX),
                    ),
                    text: batch.text.clone(),
                },
            )
            .await
            .map_err(|e| DeliveryOutcome::unavailable(e.to_string()))?;
            let line = ipc::read_line(&mut stream)
                .await
                .map_err(|e| DeliveryOutcome::unavailable(e.to_string()))?
                .ok_or_else(|| {
                    DeliveryOutcome::unavailable("ACP client disconnected during delivery")
                })?;
            serde_json::from_str::<Receipt>(&line)
                .map_err(|_| DeliveryOutcome::unavailable("invalid ACP delivery receipt"))
        };
        match tokio::time::timeout(self.timeout, send).await {
            Ok(Ok(Receipt::Accepted)) => DeliveryOutcome::Injected,
            Ok(Ok(Receipt::Busy)) => {
                DeliveryOutcome::unavailable("ACP session already has an active turn")
            }
            Ok(Ok(Receipt::WrongSession)) => DeliveryOutcome::Unavailable {
                reason: "ACP socket belongs to a different session".into(),
                unreachable: true,
            },
            Ok(Err(outcome)) => outcome,
            Err(_) => DeliveryOutcome::unavailable("ACP client did not acknowledge delivery"),
        }
    }
}

async fn write_json(
    out: &mut (impl AsyncWrite + Unpin),
    value: &impl Serialize,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() < ipc::MAX_LINE,
        "ACP message exceeds the line limit"
    );
    bytes.push(b'\n');
    out.write_all(&bytes).await?;
    out.flush().await?;
    Ok(())
}

/// One ACP connection. A dedicated reader avoids losing a partial frame
/// when input from the person or Senate wins the host's select.
struct Client {
    _child: Child,
    stdin: ChildStdin,
    messages: mpsc::Receiver<anyhow::Result<Value>>,
    next_id: u64,
}

impl Client {
    fn spawn(command: &[OsString]) -> anyhow::Result<Self> {
        let (exe, args) = command
            .split_first()
            .context("supply an ACP command after --")?;
        let mut child = Command::new(exe)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("start ACP agent")?;
        let stdin = child.stdin.take().context("missing ACP input pipe")?;
        let mut reader = BufReader::new(child.stdout.take().context("missing ACP output pipe")?);
        let (send, messages) = mpsc::channel(32);
        tokio::spawn(async move {
            loop {
                let frame = match ipc::read_line(&mut reader).await {
                    Ok(Some(line)) => serde_json::from_str(&line).map_err(Into::into),
                    Ok(None) => break,
                    Err(e) => {
                        let _ = send.send(Err(e.into())).await;
                        break;
                    }
                };
                if send.send(frame).await.is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            _child: child,
            stdin,
            messages,
            next_id: 1,
        })
    }

    async fn send(&mut self, method: &str, params: Value) -> anyhow::Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        tokio::time::timeout(
            LOCAL_TIMEOUT,
            write_json(
                &mut self.stdin,
                &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
            ),
        )
        .await
        .context("ACP agent did not read its input")??;
        Ok(id)
    }

    async fn setup(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        tokio::time::timeout(SETUP_TIMEOUT, async {
            let id = self.send(method, params).await?;
            while let Some(frame) = self.messages.recv().await {
                let frame = frame?;
                if frame.get("method").is_none() && frame["id"].as_u64() == Some(id) {
                    anyhow::ensure!(
                        frame.get("error").is_none(),
                        "{method} failed: {}",
                        frame["error"]
                    );
                    return frame.get("result").cloned().context("missing ACP result");
                }
                self.handle(&frame).await?;
            }
            anyhow::bail!("ACP agent disconnected during {method}")
        })
        .await
        .context("ACP setup timed out")?
    }

    async fn handle(&mut self, frame: &Value) -> anyhow::Result<()> {
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        if let Some(id) = frame.get("id") {
            let response = if method == "session/request_permission" {
                json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome":"cancelled"}}})
            } else {
                json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "Client method unavailable"},
                })
            };
            write_json(&mut self.stdin, &response).await?;
        } else if method == "session/update"
            && frame["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
            && let Some(text) = frame["params"]["update"]["content"]["text"].as_str()
        {
            print!("{text}");
            std::io::stdout().flush()?;
        }
        Ok(())
    }

    async fn prompt(&mut self, id: &str, text: &str) -> anyhow::Result<u64> {
        self.send(
            "session/prompt",
            json!({
                "sessionId": id, "prompt": [{"type": "text", "text": text}],
            }),
        )
        .await
    }
}

/// An ACP tool request is an opportunity for the person at this terminal to
/// consent to one answer. A tool label alone never grants permission.
struct AnswerPermission {
    rpc_id: Value,
    option: String,
    command: String,
    thread: u64,
    body: String,
}

impl AnswerPermission {
    fn from_request(frame: &Value, session: &str) -> Option<Self> {
        let params = &frame["params"];
        if frame["method"] != "session/request_permission"
            || params["sessionId"] != session
            || params["toolCall"]["name"] != "mcp_senate_answer"
        {
            return None;
        }
        let input = &params["toolCall"]["rawInput"];
        let once = params["options"]
            .as_array()?
            .iter()
            .find(|o| o["kind"] == "allow_once")?;
        let nonce = EventId::new().to_string();
        Some(Self {
            rpc_id: frame.get("id")?.clone(),
            option: once["optionId"].as_str()?.to_owned(),
            command: format!("/allow {}", &nonce[nonce.len() - 8..]),
            thread: input["thread"].as_u64()?,
            body: input["body"].as_str()?.to_owned(),
        })
    }

    fn show(&self) {
        eprintln!("Senate · thread {} · answer {:?}", self.thread, self.body);
        eprintln!(
            "{}  (allow once / 仅允许本次)    /deny  (decline / 拒绝)",
            self.command
        );
    }

    fn decision(&self, line: &str) -> Option<Value> {
        let outcome = if line == self.command {
            json!({"outcome":"selected","optionId":self.option})
        } else if line == "/deny" {
            json!({"outcome":"cancelled"})
        } else {
            return None;
        };
        Some(json!({"jsonrpc":"2.0", "id":self.rpc_id, "result":{"outcome":outcome}}))
    }
}

struct Registration<'a> {
    paths: &'a Paths,
    session_id: &'a str,
    cwd: &'a Path,
    socket: &'a Path,
}

impl Registration<'_> {
    async fn refresh(&self, event: HookEvent) -> anyhow::Result<HookReply> {
        let (facts, _) = session::gather_synced(self.paths, self.cwd, LOCAL_TIMEOUT).await;
        Ok(ipc::request(
            &self.paths.socket(),
            &Request::SessionHook(HookReport {
                event,
                session_id: self.session_id.to_owned(),
                cwd: self.cwd.to_path_buf(),
                transcript_path: None,
                source: None,
                branch: facts.branch,
                git_root: facts.toplevel,
                committee_file: facts.committee_file,
                socket: Some(self.socket.to_path_buf()),
                token: None,
                // Same store field used for owner liveness. MCP resolution
                // never uses this pid for ACP.
                claude_pid: Some(std::process::id()),
                ancestor_pids: vec![],
                runtime: Runtime::Acp,
                agent_env: None,
                agent_file: facts.agent_file,
            }),
            LOCAL_TIMEOUT,
            REGISTER_TIMEOUT,
        )
        .await?)
    }

    async fn end(&self) {
        let _: Result<Value, _> = ipc::request(
            &self.paths.socket(),
            &Request::SessionEnd {
                session_id: self.session_id.to_owned(),
                reason: None,
            },
            LOCAL_TIMEOUT,
            REGISTER_TIMEOUT,
        )
        .await;
    }
}

struct SocketPath(PathBuf);
impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

async fn receive_push(stream: &mut BufReader<UnixStream>) -> anyhow::Result<Push> {
    let line = tokio::time::timeout(LOCAL_TIMEOUT, ipc::read_line(stream))
        .await??
        .context("ACP push disconnected")?;
    Ok(serde_json::from_str(&line)?)
}

fn with_context(context: Option<String>, text: &str) -> String {
    match context {
        Some(context) => format!("{context}\n\n{text}"),
        None => text.to_owned(),
    }
}

/// An opt-in terminal client for a local ACP subprocess. Lines on stdin
/// start normal turns, and Senate can start another while the session is
/// idle. The listener is owned by this process, never discovered by PID.
pub async fn run(paths: &Paths, cwd: &Path, command: &[OsString]) -> anyhow::Result<()> {
    paths.ensure_home()?;
    let nonce = EventId::new().to_string().to_ascii_lowercase();
    let session_id = Runtime::Acp.session_key(&nonce);
    let path = paths.home().join(format!("acp-{nonce}.sock"));
    let listener = UnixListener::bind(&path).context("bind the private ACP delivery socket")?;
    let socket = SocketPath(path);
    std::fs::set_permissions(&socket.0, std::fs::Permissions::from_mode(0o600))?;
    let mut client = Client::spawn(command)?;
    let init = client
        .setup(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "senate", "version": env!("CARGO_PKG_VERSION")},
                "clientCapabilities": {},
            }),
        )
        .await?;
    anyhow::ensure!(
        init["protocolVersion"].as_u64() == Some(1),
        "ACP agent did not negotiate protocol v1"
    );
    let exe = std::env::current_exe()?;
    let new = client
        .setup(
            "session/new",
            json!({
                "cwd": cwd,
                "mcpServers": [{
                    "name": "senate",
                    "command": exe,
                    "args": ["mcp", "--acp-session", &session_id],
                    "env": [{"name": HOME_ENV, "value": paths.home()}],
                }],
            }),
        )
        .await?;
    let remote_id = new["sessionId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("ACP agent did not return a session ID")?;
    let registration = Registration {
        paths,
        session_id: &session_id,
        cwd,
        socket: &socket.0,
    };
    let ready = registration.refresh(HookEvent::SessionStart).await?;
    let mut context = ready.context;
    eprintln!(
        "Senate ACP session [senator::{}] ({session_id})",
        ready.session.agent
    );

    let (send, mut input) = mpsc::channel(1);
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if send.blocking_send(line).is_err() {
                break;
            }
        }
    });

    let result: anyhow::Result<()> = async {
        let mut active = None;
        let mut permission: Option<AnswerPermission> = None;
        let sigint = tokio::signal::ctrl_c();
        tokio::pin!(sigint);
        loop {
            tokio::select! {
                _ = &mut sigint => return Ok(()),
                line = input.recv(), if active.is_none() || permission.is_some() => {
                    let Some(line) = line else { return Ok(()) };
                    if line == "/quit" { return Ok(()) }
                    if let Some(pending) = &permission {
                        if let Some(response) = pending.decision(&line) {
                            write_json(&mut client.stdin, &response).await?;
                            permission = None;
                        } else {
                            pending.show();
                        }
                        continue;
                    }
                    if !line.trim().is_empty() {
                        if let Ok(reply) = registration.refresh(HookEvent::UserPromptSubmit).await
                            && let Some(new) = reply.context
                        {
                            context = Some(with_context(context.take(), &new));
                        }
                        active = Some(client.prompt(remote_id, &with_context(context.take(), &line)).await?);
                    }
                }
                incoming = listener.accept() => {
                    let (stream, _) = incoming?;
                    let mut stream = BufReader::new(stream);
                    let Ok(push) = receive_push(&mut stream).await else { continue };
                    // A disconnected caller's expired push must not wake the
                    // agent after the daemon already scheduled a retry.
                    if push.expires_at_ms <= chrono::Utc::now().timestamp_millis()
                        || matches!(stream.get_ref().try_read(&mut [0]), Ok(0))
                    {
                        continue;
                    }
                    let receipt = if push.session_id != session_id {
                        Receipt::WrongSession
                    } else if active.is_some() {
                        Receipt::Busy
                    } else {
                        // Acceptance is the handoff to the owned connection.
                        // ACP replies to session/prompt after the whole turn.
                        active = Some(client.prompt(remote_id, &with_context(context.take(), &push.text)).await?);
                        Receipt::Accepted
                    };
                    let _ = tokio::time::timeout(LOCAL_TIMEOUT, write_json(stream.get_mut(), &receipt)).await;
                }
                frame = client.messages.recv() => {
                    let frame = frame.context("ACP agent disconnected")??;
                    if active.is_some() && frame.get("method").is_none() && frame["id"].as_u64() == active {
                        if let Some(error) = frame.get("error") {
                            eprintln!("ACP turn failed: {error}");
                        }
                        println!();
                        active = None;
                        permission = None;
                        if let Ok(reply) = registration.refresh(HookEvent::UserPromptSubmit).await {
                            context = reply.context;
                        }
                    } else {
                        if active.is_some() && permission.is_none() && std::io::stdin().is_terminal()
                            && let Some(pending) = AnswerPermission::from_request(&frame, remote_id) {
                            pending.show();
                            permission = Some(pending);
                        } else {
                            client.handle(&frame).await?;
                        }
                    }
                }
            }
        }
    }.await;
    registration.end().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_is_for_one_visible_answer_and_requires_its_exact_operator_response() {
        let mut frame = json!({
            "id":3, "method":"session/request_permission",
            "params":{"sessionId":"owned","toolCall":{"name":"mcp_senate_answer","rawInput":{"thread":7,"body":"ack"}},
                "options":[{"kind":"allow_once","optionId":"once"}]}
        });
        let pending = AnswerPermission::from_request(&frame, "owned").unwrap();
        assert!(pending.decision("/allow").is_none());
        assert!(pending.decision("yes").is_none());
        assert_eq!(
            pending.decision("/deny").unwrap()["result"]["outcome"]["outcome"],
            "cancelled"
        );
        assert_eq!(
            pending.decision(&pending.command).unwrap()["result"]["outcome"]["optionId"],
            "once"
        );
        assert!(AnswerPermission::from_request(&frame, "different").is_none());
        frame["params"]["toolCall"]["rawInput"] = Value::Null;
        assert!(AnswerPermission::from_request(&frame, "owned").is_none());
    }
}
