//! A fake Codex app-server: WebSocket on a temp unix socket, JSON-RPC in
//! text frames, the handful of methods the bridge calls, with the replies
//! and error messages codex-cli 0.167 gives. Never a real Codex.
//!
//! Approvals, as the app-server schema describes them: `thread/resume`
//! subscribes a connection to a loaded thread; [`FakeCodex::request_approval`]
//! sends a server request to every subscriber; the first client response
//! resolves it, and every subscriber then gets `serverRequest/resolved`
//! ([`FakeCodex::answer_in_tui`] is the TUI answering first).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Default)]
pub struct FakeThread {
    /// `false`: `thread/read` answers `notLoaded`.
    pub loaded: bool,
    /// The turn in progress, if the thread is busy.
    pub active: Option<String>,
}

/// A subscribed connection: its number and its outgoing frames.
type Subscriber = (usize, mpsc::UnboundedSender<Vec<u8>>);

#[derive(Debug, Default)]
pub struct CodexState {
    pub threads: HashMap<String, FakeThread>,
    /// Every request, `(method, params)`, in arrival order.
    pub calls: Vec<(String, Value)>,
    /// The next this many `turn/steer`s find the running turn replaced by a
    /// new one (the user's turn ended and another began after the bridge
    /// looked).
    pub rotate_steers: u32,
    next_turn: u32,
    pub connections: usize,
    /// Per thread: the connections subscribed with `thread/resume`.
    subscribers: HashMap<String, Vec<Subscriber>>,
    /// Server requests not answered yet: id → thread.
    pending: HashMap<i64, String>,
    next_request: i64,
    /// Every client response to a server request: `(id, result)`.
    pub responses: Vec<(Value, Value)>,
}

impl CodexState {
    fn new_turn(&mut self) -> String {
        self.next_turn += 1;
        format!("turn-{}", self.next_turn)
    }

    fn broadcast(&mut self, thread: &str, msg: &Value) {
        let frame = frame(1, msg.to_string().as_bytes());
        if let Some(subs) = self.subscribers.get_mut(thread) {
            subs.retain(|(_, tx)| tx.send(frame.clone()).is_ok());
        }
    }

    /// The request is answered: tell every subscriber.
    fn resolve(&mut self, id: i64) -> bool {
        let Some(thread) = self.pending.remove(&id) else {
            return false;
        };
        let msg = json!({"method": "serverRequest/resolved",
            "params": {"threadId": thread, "requestId": id}});
        self.broadcast(&thread, &msg);
        true
    }
}

pub struct FakeCodex {
    pub path: PathBuf,
    pub state: Arc<Mutex<CodexState>>,
    task: JoinHandle<()>,
}

impl Drop for FakeCodex {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeCodex {
    pub fn bind(path: PathBuf) -> Self {
        let listener = UnixListener::bind(&path).unwrap();
        let state = Arc::new(Mutex::new(CodexState::default()));
        let st = state.clone();
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let conn = {
                    let mut st = st.lock().unwrap();
                    st.connections += 1;
                    st.connections
                };
                tokio::spawn(serve(stream, st.clone(), conn));
            }
        });
        Self { path, state, task }
    }

    pub fn thread(&self, id: &str, thread: FakeThread) {
        self.state
            .lock()
            .unwrap()
            .threads
            .insert(id.to_string(), thread);
    }

    pub fn idle(&self, id: &str) {
        self.thread(
            id,
            FakeThread {
                loaded: true,
                active: None,
            },
        );
    }

    pub fn busy(&self, id: &str, turn: &str) {
        self.thread(
            id,
            FakeThread {
                loaded: true,
                active: Some(turn.to_string()),
            },
        );
    }

    pub fn active_turn(&self, id: &str) -> Option<String> {
        self.state.lock().unwrap().threads.get(id)?.active.clone()
    }

    pub fn calls(&self) -> Vec<(String, Value)> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Calls that put text into a thread: `(method, params)`.
    pub fn pushes(&self) -> Vec<(String, Value)> {
        self.calls()
            .into_iter()
            .filter(|(m, _)| m == "turn/start" || m == "turn/steer")
            .collect()
    }

    pub fn methods(&self) -> Vec<String> {
        self.calls().into_iter().map(|(m, _)| m).collect()
    }

    /// Connections subscribed to `thread`.
    pub fn subscribers(&self, thread: &str) -> usize {
        let mut st = self.state.lock().unwrap();
        st.subscribers
            .get_mut(thread)
            .map(|subs| {
                subs.retain(|(_, tx)| !tx.is_closed());
                subs.len()
            })
            .unwrap_or(0)
    }

    /// The app-server asks `thread`'s clients for an approval; its id.
    pub fn request_approval(&self, thread: &str, method: &str, params: Value) -> i64 {
        let mut st = self.state.lock().unwrap();
        st.next_request += 1;
        let id = st.next_request;
        st.pending.insert(id, thread.to_string());
        let msg = json!({"id": id, "method": method, "params": params});
        st.broadcast(thread, &msg);
        id
    }

    pub fn replay_approval(&self, thread: &str, id: i64, method: &str, params: Value) {
        let mut st = self.state.lock().unwrap();
        assert_eq!(st.pending.get(&id).map(String::as_str), Some(thread));
        st.broadcast(
            thread,
            &json!({"id": id, "method": method, "params": params}),
        );
    }

    pub fn disconnect_subscribers(&self, thread: &str) {
        let mut st = self.state.lock().unwrap();
        for (_, tx) in st.subscribers.remove(thread).into_iter().flatten() {
            let _ = tx.send(frame(8, b""));
        }
    }

    pub fn resolve_while_disconnected(&self, thread: &str, id: i64) {
        let mut st = self.state.lock().unwrap();
        for (_, tx) in st.subscribers.remove(thread).into_iter().flatten() {
            let _ = tx.send(frame(8, b""));
        }
        assert!(st.resolve(id));
    }

    /// The TUI answers request `id` first.
    pub fn answer_in_tui(&self, id: i64) -> bool {
        self.state.lock().unwrap().resolve(id)
    }

    pub fn responses(&self) -> Vec<(Value, Value)> {
        self.state.lock().unwrap().responses.clone()
    }
}

/// The text of a `turn/start` / `turn/steer`.
pub fn input_text(params: &Value) -> &str {
    params["input"][0]["text"].as_str().unwrap_or_default()
}

/// A `codex` stand-in for the proxy transport: `codex app-server proxy`
/// tunnels stdio to `socket` with `nc -U`, as the real proxy tunnels it to
/// the managed daemon. `None` when this machine has no `nc`.
pub fn fake_codex_bin(dir: &Path, socket: Option<&Path>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let nc = ["/usr/bin/nc", "/bin/nc"]
        .into_iter()
        .find(|p| Path::new(p).exists())?;
    let body = match socket {
        Some(s) => format!("exec {nc} -U '{}'", s.display()),
        // No daemon running: the real proxy fails at once.
        None => "echo 'no app-server daemon is running' >&2; exit 1".to_string(),
    };
    let bin = dir.join(if socket.is_some() {
        "codex"
    } else {
        "codex-down"
    });
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n[ \"$1 $2\" = 'app-server proxy' ] || {{ echo \"unexpected: $*\" >&2; exit 2; }}\n{body}\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    Some(bin)
}

fn frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0x80 | opcode];
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len < 65536 {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

async fn read_frame<R: AsyncRead + Unpin>(
    stream: &mut R,
    buf: &mut Vec<u8>,
) -> Option<(u8, Vec<u8>)> {
    async fn need<R: AsyncRead + Unpin>(stream: &mut R, buf: &mut Vec<u8>, n: usize) -> Option<()> {
        while buf.len() < n {
            let mut chunk = [0u8; 8192];
            let got = stream.read(&mut chunk).await.ok()?;
            if got == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..got]);
        }
        Some(())
    }
    need(stream, buf, 2).await?;
    let opcode = buf[0] & 0x0f;
    assert!(buf[1] & 0x80 != 0, "client frames must be masked");
    let (len, header) = match buf[1] & 0x7f {
        126 => {
            need(stream, buf, 4).await?;
            (usize::from(u16::from_be_bytes([buf[2], buf[3]])), 4)
        }
        127 => {
            need(stream, buf, 10).await?;
            let mut b = [0u8; 8];
            b.copy_from_slice(&buf[2..10]);
            (u64::from_be_bytes(b) as usize, 10)
        }
        n => (usize::from(n), 2),
    };
    need(stream, buf, header + 4 + len).await?;
    let mask = [
        buf[header],
        buf[header + 1],
        buf[header + 2],
        buf[header + 3],
    ];
    let payload: Vec<u8> = buf[header + 4..header + 4 + len]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ mask[i % 4])
        .collect();
    buf.drain(..header + 4 + len);
    Some((opcode, payload))
}

fn send(tx: &mpsc::UnboundedSender<Vec<u8>>, v: &Value) {
    let _ = tx.send(frame(1, v.to_string().as_bytes()));
}

async fn serve(mut stream: UnixStream, state: Arc<Mutex<CodexState>>, conn: usize) {
    let mut buf = Vec::new();
    // The upgrade.
    loop {
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&buf).to_lowercase();
    assert!(head.starts_with("get "), "{head}");
    assert!(head.contains("upgrade: websocket"), "{head}");
    assert!(head.contains("sec-websocket-key:"), "{head}");
    buf.clear();
    let _ = stream
        .write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nconnection: Upgrade\r\nupgrade: websocket\r\n\
              sec-websocket-accept: x\r\n\r\n",
        )
        .await;
    let (mut read, mut write) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if write.write_all(&bytes).await.is_err() {
                return;
            }
        }
    });
    let mut pinged = false;
    while let Some((opcode, payload)) = read_frame(&mut read, &mut buf).await {
        match opcode {
            0x1 => {}
            0x8 => break,
            _ => continue,
        }
        let msg: Value = serde_json::from_slice(&payload).unwrap();
        let Some(id) = msg.get("id").cloned() else {
            continue; // a notification (`initialized`)
        };
        if msg.get("method").is_none() {
            // A response to one of our server requests: the first wins.
            let mut st = state.lock().unwrap();
            let result = msg.get("result").cloned().unwrap_or(Value::Null);
            st.responses.push((id.clone(), result));
            if let Some(n) = id.as_i64() {
                st.resolve(n);
            }
            continue;
        }
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg["params"].clone();
        if !pinged {
            // Codex sends notifications whenever it likes; and a ping.
            pinged = true;
            let _ = tx.send(frame(0x9, b"hi"));
            send(
                &tx,
                &json!({"method": "remoteControl/status/changed", "params": {"status": "disabled"}}),
            );
        }
        let reply = {
            let mut st = state.lock().unwrap();
            st.calls.push((method.clone(), params.clone()));
            let reply = answer(&mut st, &method, &params);
            if method == "thread/resume" && reply.is_ok() {
                let thread = params["threadId"].as_str().unwrap_or_default().to_string();
                st.subscribers
                    .entry(thread)
                    .or_default()
                    .push((conn, tx.clone()));
            }
            reply
        };
        let out = match reply {
            Ok(result) => json!({"id": id, "result": result}),
            Err(message) => json!({"id": id, "error": {"code": -32600, "message": message}}),
        };
        send(&tx, &out);
    }
    {
        let mut st = state.lock().unwrap();
        for subs in st.subscribers.values_mut() {
            subs.retain(|(c, _)| *c != conn);
        }
    }
    drop(tx);
    let _ = writer.await;
}

fn answer(st: &mut CodexState, method: &str, params: &Value) -> Result<Value, String> {
    let thread_id = params["threadId"].as_str().unwrap_or_default().to_string();
    let thread = st.threads.get(&thread_id).cloned();
    match method {
        "initialize" => Ok(json!({
            "userAgent": "fake/0.167.0",
            "codexHome": "/nonexistent",
            "platformFamily": "unix",
            "platformOs": "macos"
        })),
        "thread/start" => {
            let id = format!("hosted-thread-{}", st.connections);
            st.threads.insert(
                id.clone(),
                FakeThread {
                    loaded: true,
                    active: None,
                },
            );
            Ok(json!({"thread": {"id": id}}))
        }
        "thread/inject_items" if thread.is_some() => Ok(json!({})),
        "thread/name/set" if thread.is_some() => Ok(json!({})),
        "thread/read" => {
            let t = thread.ok_or(format!("thread not loaded: {thread_id}"))?;
            let status = match (&t.active, t.loaded) {
                (_, false) => json!({"type": "notLoaded"}),
                (Some(_), true) => json!({"type": "active", "activeFlags": []}),
                (None, true) => json!({"type": "idle"}),
            };
            Ok(json!({"thread": {"id": thread_id, "status": status, "turns": []}}))
        }
        "thread/resume" => {
            let t = match thread {
                Some(t) if t.loaded => t,
                _ => return Err(format!("thread not found: {thread_id}")),
            };
            let status = match (
                &t.active,
                st.pending.values().any(|thread| thread == &thread_id),
            ) {
                (_, true) => json!({"type": "active", "activeFlags": ["waitingOnApproval"]}),
                (Some(_), false) => json!({"type": "active", "activeFlags": []}),
                (None, false) => json!({"type": "idle"}),
            };
            Ok(json!({"thread": {"id": thread_id, "status": status, "turns": []}}))
        }
        "thread/turns/list" => {
            let t = thread.ok_or(format!("thread not loaded: {thread_id}"))?;
            let turn = match &t.active {
                Some(id) => json!({"id": id, "status": "inProgress", "items": []}),
                None => json!({"id": "turn-old", "status": "completed", "items": []}),
            };
            Ok(json!({"data": [turn], "nextCursor": null}))
        }
        "turn/start" => {
            let t = match thread {
                Some(t) if t.loaded => t,
                _ => return Err(format!("thread not found: {thread_id}")),
            };
            // Codex adds the input to a running turn.
            let turn = match t.active {
                Some(turn) => turn,
                None => {
                    let turn = st.new_turn();
                    st.threads.get_mut(&thread_id).unwrap().active = Some(turn.clone());
                    turn
                }
            };
            Ok(json!({"turn": {"id": turn, "status": "inProgress", "items": []}}))
        }
        "turn/steer" => {
            if thread.as_ref().is_none_or(|t| !t.loaded) {
                return Err(format!("thread not found: {thread_id}"));
            }
            if st.rotate_steers > 0 {
                st.rotate_steers -= 1;
                let turn = st.new_turn();
                st.threads.get_mut(&thread_id).unwrap().active = Some(turn);
            }
            let active = st.threads[&thread_id]
                .active
                .clone()
                .ok_or("no active turn to steer".to_string())?;
            let expected = params["expectedTurnId"].as_str().unwrap_or_default();
            if expected != active {
                return Err(format!(
                    "expected active turn id `{expected}` but found `{active}`"
                ));
            }
            Ok(json!({"turnId": active}))
        }
        other => Err(format!("unknown method {other}")),
    }
}
