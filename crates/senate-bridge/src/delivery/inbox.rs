//! The Claude Code session inbox socket.
//!
//! **Undocumented.** This framing was observed in Claude Code 2.1.310's own
//! debug hint and nowhere else; everything that depends on it is in this
//! module, and `senate debug push` exercises it by hand.
//!
//! A running session exports `CLAUDE_CODE_MESSAGING_SOCKET` (a Unix socket
//! path) and `CLAUDE_CODE_MESSAGING_TOKEN`. To put a user-role message into
//! the session:
//!
//! 1. connect;
//! 2. write `{"type":"auth","token":"<token>"}` + `\n`;
//! 3. write `{"type":"user","message":{"role":"user","content":"<text>"}}` + `\n`;
//! 4. close.
//!
//! One JSON object per line. Claude closes a connection that has not sent a
//! complete line within 30 s, so the connection is opened only once the text
//! is ready. The inbox refuses bursts and queues at most 50 messages, which is
//! why the daemon batches and spaces its pushes. After writing, `push` reads
//! for [`PushOptions::linger`] (at most [`MAX_REPLY`] bytes) and returns
//! whatever Claude sent back. Silence is the only reply observed so far; the
//! format of any other is unknown, so the delivery adapter treats a
//! non-empty reply as "not delivered" and logs it verbatim.
//!
//! Without `"crossSessionInbound": "accept"` in Claude's settings, a message
//! from a process that is not a child of the session may be held for the
//! user's approval and dropped after 5 minutes (see `senate setup install`).

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;

/// Most bytes of a reply kept.
pub const MAX_REPLY: usize = 64 * 1024;

/// A JSON string literal. Serializing a `&str` cannot fail; the fallback only
/// keeps this total.
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// `{"type":"auth","token":…}\n`, keys in the observed order.
pub fn auth_frame(token: &str) -> String {
    format!("{{\"type\":\"auth\",\"token\":{}}}\n", json_str(token))
}

/// `{"type":"user","message":{"role":"user","content":…}}\n`, keys in the
/// observed order. Newlines in `text` are escaped, so this is one line.
pub fn user_frame(text: &str) -> String {
    format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n",
        json_str(text)
    )
}

#[derive(Debug, Clone, Copy)]
pub struct PushOptions {
    /// Bound on connecting and on writing, each.
    pub timeout: Duration,
    /// How long to wait for anything Claude sends back before closing.
    pub linger: Duration,
}

impl Default for PushOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(3),
            linger: Duration::from_millis(200),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InboxError {
    #[error("inbox socket {path} refused the connection: {source}")]
    Connect { path: PathBuf, source: io::Error },
    #[error("inbox socket {0}: connect timed out")]
    ConnectTimeout(PathBuf),
    #[error("inbox socket {path}: write failed: {source}")]
    Write { path: PathBuf, source: io::Error },
    #[error("inbox socket {0}: write timed out")]
    WriteTimeout(PathBuf),
    /// This process was started confined to a test's directory
    /// ([`crate::testenv`]) and the socket is outside it.
    #[error("inbox socket {0} is outside the directory this process is confined to")]
    Outside(PathBuf),
}

impl InboxError {
    /// The session is not listening: treat it as gone.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            Self::Connect { .. } | Self::ConnectTimeout(_) | Self::Outside(_)
        )
    }
}

/// Write one user message into a session inbox. `token` `None` skips the
/// auth line (for manual experiments only). Returns whatever the socket sent
/// back within `linger`, if anything.
pub async fn push(
    socket: &Path,
    token: Option<&str>,
    text: &str,
    options: &PushOptions,
) -> Result<Option<String>, InboxError> {
    if !crate::testenv::confined(socket) {
        return Err(InboxError::Outside(socket.to_path_buf()));
    }
    // Build both frames before connecting (see the module docs on the 30 s rule).
    let mut payload = String::new();
    if let Some(token) = token {
        payload.push_str(&auth_frame(token));
    }
    payload.push_str(&user_frame(text));

    let mut stream = connect(socket, options.timeout).await?;
    let write = async {
        stream.write_all(payload.as_bytes()).await?;
        stream.flush().await?;
        stream.shutdown().await
    };
    match tokio::time::timeout(options.timeout, write).await {
        Ok(Ok(())) => {}
        Ok(Err(source)) => {
            return Err(InboxError::Write {
                path: socket.to_path_buf(),
                source,
            });
        }
        Err(_) => return Err(InboxError::WriteTimeout(socket.to_path_buf())),
    }
    // Read until Claude closes, the linger runs out, or the cap. A read error
    // after a complete write says nothing about the message, so it ends the
    // read like silence does.
    let deadline = tokio::time::Instant::now() + options.linger;
    // Straight into `reply`: an array here would be carried in this future
    // and in every one that awaits it.
    let mut reply = Vec::new();
    while reply.len() < MAX_REPLY {
        reply.reserve(4096);
        match tokio::time::timeout_at(deadline, stream.read_buf(&mut reply)).await {
            Ok(Ok(n)) if n > 0 => {}
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    reply.truncate(MAX_REPLY);
    let reply = String::from_utf8_lossy(&reply).trim().to_string();
    Ok((!reply.is_empty()).then_some(reply))
}

#[cfg(unix)]
async fn connect(socket: &Path, timeout: Duration) -> Result<UnixStream, InboxError> {
    match tokio::time::timeout(timeout, UnixStream::connect(socket)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(source)) => Err(InboxError::Connect {
            path: socket.to_path_buf(),
            source,
        }),
        Err(_) => Err(InboxError::ConnectTimeout(socket.to_path_buf())),
    }
}

/// Claude Code's inbox is a Unix socket; Claude is not supported on
/// Windows yet.
#[cfg(windows)]
async fn connect(socket: &Path, _timeout: Duration) -> Result<crate::local::Stream, InboxError> {
    Err(InboxError::Connect {
        path: socket.to_path_buf(),
        source: io::Error::new(io::ErrorKind::Unsupported, "Claude Code's inbox on Windows"),
    })
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use serde_json::json;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixListener;

    use super::*;

    #[test]
    fn frames_are_one_json_object_per_line() {
        assert_eq!(auth_frame("t0k"), "{\"type\":\"auth\",\"token\":\"t0k\"}\n");
        assert_eq!(
            user_frame("hi \"x\""),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi \\\"x\\\"\"}}\n"
        );
        let user: serde_json::Value = serde_json::from_str(user_frame("a\nb").trim_end()).unwrap();
        assert_eq!(
            user,
            json!({"type":"user","message":{"role":"user","content":"a\nb"}})
        );
        assert_eq!(user_frame("a\nb").matches('\n').count(), 1);
    }

    #[tokio::test]
    async fn push_writes_auth_then_user_then_closes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut lines = tokio::io::BufReader::new(stream).lines();
            let mut got = Vec::new();
            while let Some(line) = lines.next_line().await.unwrap() {
                got.push(line);
            }
            got
        });
        let reply = push(&path, Some("tok"), "hello", &PushOptions::default())
            .await
            .unwrap();
        assert_eq!(reply, None);
        let lines = server.await.unwrap();
        assert_eq!(lines.len(), 2);
        let auth: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(auth, json!({"type":"auth","token":"tok"}));
        let user: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(user["message"]["content"], "hello");
    }

    #[tokio::test]
    async fn a_reply_is_returned_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.sock");
        let listener = UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut sink = Vec::new();
            stream.read_to_end(&mut sink).await.unwrap();
            stream
                .write_all(b"{\"type\":\"error\",\"message\":\"busy\"}\n")
                .await
                .unwrap();
        });
        let reply = push(&path, Some("tok"), "hello", &PushOptions::default())
            .await
            .unwrap();
        assert_eq!(
            reply.as_deref(),
            Some("{\"type\":\"error\",\"message\":\"busy\"}")
        );
    }

    #[tokio::test]
    async fn missing_socket_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let err = push(
            &dir.path().join("none.sock"),
            Some("t"),
            "x",
            &PushOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(err.is_unreachable());
    }
}
