//! Authenticated platform IPC. Only one adapter can serve the configured
//! platform. Its reader keeps accepting results while Senate handles an
//! inbound message, since handling it can itself call the remote surface.

mod framing;
mod socket;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use anyhow::{Result, anyhow, bail, ensure};
use async_trait::async_trait;
use senate_core::{
    AdoptedGroup, AnnouncementSection, Card, ChatSurface, CreatedGroup, NewGroup, PostTarget,
    Posted, SurfaceError,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncWrite, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_util::sync::CancellationToken;

use super::InboundHandler;
use super::bind::RejectedInput;
use crate::Inbound;

const VERSION: u32 = 1;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_PENDING: usize = 32;
const MAX_CONNECTIONS: usize = 8;

type Finish = Box<dyn FnOnce(Result<Value, SurfaceError>) -> Result<()> + Send>;

struct Call {
    op: &'static str,
    args: Value,
    finish: Finish,
}

struct Peer {
    calls: mpsc::Sender<Call>,
}

pub(super) struct RemoteSurface {
    platform: String,
    // ChatGateway caches Unsupported for the lifetime of this surface.
    capabilities: OnceLock<HashSet<String>>,
    peer: Mutex<Option<Arc<Peer>>>,
    /// Where whether an adapter is attached is written
    /// ([`super::GatewayStatus`]); unset, nowhere.
    status: OnceLock<PathBuf>,
}

impl RemoteSurface {
    pub(super) fn new(platform: String) -> Arc<Self> {
        Arc::new(Self {
            platform,
            capabilities: OnceLock::new(),
            peer: Mutex::new(None),
            status: OnceLock::new(),
        })
    }

    /// Write whether an adapter is attached to `path` from now on.
    pub(super) fn record_to(&self, path: PathBuf) {
        let _ = self.status.set(path);
        self.record(false);
    }

    fn record(&self, connected: bool) {
        if let Some(path) = self.status.get() {
            super::GatewayStatus::now(&self.platform, connected).write(path);
        }
    }

    fn peer(&self) -> MutexGuard<'_, Option<Arc<Peer>>> {
        self.peer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn call<T: DeserializeOwned + Send + 'static>(
        &self,
        op: &'static str,
        args: Value,
    ) -> Result<T, SurfaceError> {
        let peer = self
            .peer()
            .clone()
            .ok_or_else(|| unavailable("gateway offline"))?;
        if !self
            .capabilities
            .get()
            .is_some_and(|caps| caps.contains(op))
        {
            return Err(SurfaceError::Unsupported(op.into()));
        }
        let (tx, rx) = oneshot::channel();
        let finish: Finish = Box::new(move |result| {
            let parsed = match result {
                Ok(value) => Ok(serde_json::from_value(value)?),
                Err(error) => Err(error),
            };
            let _ = tx.send(parsed);
            Ok(())
        });
        peer.calls
            .try_send(Call { op, args, finish })
            .map_err(|_| unavailable("gateway unavailable or busy"))?;
        rx.await.map_err(|_| unavailable("gateway disconnected"))?
    }
}

fn unavailable(reason: &str) -> SurfaceError {
    SurfaceError::Unavailable(reason.into())
}

#[derive(Deserialize)]
struct PostedReply {
    message_ref: String,
    thread_ref: Option<String>,
    duplicate: bool,
}

impl From<PostedReply> for Posted {
    fn from(reply: PostedReply) -> Self {
        Self {
            message_ref: reply.message_ref,
            thread_ref: reply.thread_ref,
            duplicate: reply.duplicate,
        }
    }
}

fn wire_target(target: PostTarget) -> Value {
    match target {
        PostTarget::NewRoot { group_id } => json!({"kind":"new_root","group_id":group_id}),
        PostTarget::Reply {
            message_ref,
            in_thread,
        } => json!({"kind":"reply","message_ref":message_ref,"in_thread":in_thread}),
    }
}

#[derive(Deserialize)]
struct AdoptedReply {
    group_id: String,
    already_granted: bool,
}

#[async_trait]
impl ChatSurface for RemoteSurface {
    fn platform(&self) -> &str {
        &self.platform
    }

    async fn post(
        &self,
        target: PostTarget,
        markdown: &str,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        let reply: PostedReply = self
            .call(
                "post",
                json!({"target":wire_target(target),"markdown":markdown,"operation_id":operation_id}),
            )
            .await?;
        Ok(reply.into())
    }

    async fn post_mentioning(
        &self,
        target: PostTarget,
        markdown: &str,
        user: &str,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        let reply: PostedReply = self
            .call(
                "post_mentioning",
                json!({"target":wire_target(target),"markdown":markdown,"user":user,"operation_id":operation_id}),
            )
            .await?;
        Ok(reply.into())
    }

    async fn post_card(
        &self,
        target: PostTarget,
        card: &Card,
        operation_id: &str,
    ) -> Result<Posted, SurfaceError> {
        let reply: PostedReply = self
            .call(
                "post_card",
                json!({"target":wire_target(target),"card":card,"operation_id":operation_id}),
            )
            .await?;
        Ok(reply.into())
    }

    async fn update_card(
        &self,
        message_ref: &str,
        card: &Card,
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        self.call(
            "update_card",
            json!({"message_ref":message_ref,"card":card,"operation_id":operation_id}),
        )
        .await
    }

    async fn pin(&self, message_ref: &str, operation_id: &str) -> Result<(), SurfaceError> {
        self.call(
            "pin",
            json!({"message_ref":message_ref,"operation_id":operation_id}),
        )
        .await
    }

    async fn create_group(
        &self,
        group: &NewGroup,
        operation_id: &str,
    ) -> Result<CreatedGroup, SurfaceError> {
        self.call(
            "create_group",
            json!({"group":group,"operation_id":operation_id}),
        )
        .await
    }

    async fn invite(
        &self,
        group_id: &str,
        users: &[String],
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        self.call(
            "invite",
            json!({"group_id":group_id,"users":users,"operation_id":operation_id}),
        )
        .await
    }

    async fn rename_group(
        &self,
        group_id: &str,
        name: &str,
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        self.call(
            "rename_group",
            json!({"group_id":group_id,"name":name,"operation_id":operation_id}),
        )
        .await
    }

    async fn set_announcement(
        &self,
        group_id: &str,
        section: &AnnouncementSection,
        operation_id: &str,
    ) -> Result<(), SurfaceError> {
        self.call(
            "set_announcement",
            json!({"group_id":group_id,"section":section,"operation_id":operation_id}),
        )
        .await
    }

    async fn adopt_group(&self, message_ref: &str) -> Result<AdoptedGroup, SurfaceError> {
        let reply: AdoptedReply = self
            .call("adopt_group", json!({"message_ref":message_ref}))
            .await?;
        Ok(AdoptedGroup {
            group_id: reply.group_id,
            already_granted: reply.already_granted,
        })
    }

    async fn release_group(&self, group_id: &str) -> Result<bool, SurfaceError> {
        self.call("release_group", json!({"group_id":group_id}))
            .await
    }
}

/// Removing a disconnected adapter closes its outstanding requests. The
/// next connection receives fresh correlation IDs and its own request queue.
struct Attachment(Arc<RemoteSurface>);

impl Drop for Attachment {
    fn drop(&mut self) {
        *self.0.peer() = None;
        self.0.record(false);
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum AdapterFrame {
    Hello {
        platform: String,
        token: String,
        version: u32,
        capabilities: HashSet<String>,
    },
    Inbound {
        id: String,
        message: Inbound,
    },
    Result(Outcome),
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
enum Outcome {
    Success { id: String, value: Value },
    Failure { id: String, error: WireError },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireError {
    kind: ErrorKind,
    detail: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ErrorKind {
    Rejected,
    Unsupported,
    Unavailable,
}

impl From<WireError> for SurfaceError {
    fn from(error: WireError) -> Self {
        match error.kind {
            ErrorKind::Rejected => Self::Rejected(error.detail),
            ErrorKind::Unsupported => Self::Unsupported(error.detail),
            ErrorKind::Unavailable => Self::Unavailable(error.detail),
        }
    }
}

pub(super) async fn serve(
    path: &Path,
    token: &str,
    surface: Arc<RemoteSurface>,
    handler: Arc<InboundHandler>,
    shutdown: CancellationToken,
) -> Result<()> {
    ensure!(!token.is_empty(), "gateway token is empty");
    let socket = socket::BoundSocket::bind(path).await?;
    let token = Arc::<str>::from(token);
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            joined = connections.join_next(), if !connections.is_empty() => {
                if !matches!(joined, Some(Ok(Ok(())))) {
                    // Neither peer-controlled invalid frames nor handshake
                    // data belong in the log.
                    tracing::debug!("chat gateway connection ended");
                }
            }
            stream = socket.listener.accept() => {
                let (stream, _) = stream?;
                if connections.len() < MAX_CONNECTIONS {
                    let handler = handler.clone();
                    let surface = surface.clone();
                    let token = token.clone();
                    connections.spawn(async move {
                        session(stream, &token, surface, handler.resume(), |message| {
                            let handler = handler.clone();
                            async move { handler.deliver(message).await }
                        }).await
                    });
                }
            }
        }
    }
    connections.shutdown().await;
    Ok(())
}

async fn send(writer: &mut (impl AsyncWrite + Unpin), frame: Value) -> Result<()> {
    timeout(IO_TIMEOUT, framing::write(writer, frame)).await??;
    Ok(())
}

async fn session<F, Fut>(
    stream: UnixStream,
    token: &str,
    surface: Arc<RemoteSurface>,
    resume: impl Future<Output = Result<()>> + Send,
    deliver: F,
) -> Result<()>
where
    F: Fn(Inbound) -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let (read, mut writer) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut partial = Vec::new();
    let hello = timeout(HANDSHAKE_TIMEOUT, framing::read(&mut reader, &mut partial)).await??;
    let AdapterFrame::Hello {
        platform,
        token: presented,
        version,
        capabilities,
    } = hello
    else {
        bail!("expected a gateway hello");
    };
    ensure!(
        version == VERSION
            && platform == surface.platform
            && bool::from(presented.as_bytes().ct_eq(token.as_bytes())),
        "gateway handshake refused"
    );
    ensure!(capabilities.contains("post"), "gateway must advertise post");
    let (tx, mut calls) = mpsc::channel(MAX_PENDING);
    let attached = {
        let mut peer = surface.peer();
        ensure!(peer.is_none(), "another gateway adapter is connected");
        if surface.capabilities.get_or_init(|| capabilities.clone()) == &capabilities {
            *peer = Some(Arc::new(Peer { calls: tx }));
            true
        } else {
            false
        }
    };
    if !attached {
        let detail = "gateway capabilities changed; restart senated to change adapter capabilities";
        send(
            &mut writer,
            json!({"type":"error","error":{"kind":"rejected","detail":detail}}),
        )
        .await?;
        bail!(detail);
    }
    surface.record(true);
    let _attachment = Attachment(surface);
    send(&mut writer, json!({"type":"ready","version":VERSION})).await?;
    let mut pending = HashMap::<String, (Instant, Finish)>::new();
    let (inbound, mut incoming) = mpsc::channel(1);
    let (acked, mut acks) = mpsc::channel(1);
    // Recovery can itself call the remote surface. Poll the reader and this
    // future concurrently; one freshly arrived input may wait behind it.
    let deliveries = async move {
        resume.await?;
        while let Some((id, message)) = incoming.recv().await {
            let ack = match deliver(message).await {
                Ok(()) => json!({"type":"ack", "id":id}),
                Err(error) => {
                    let rejection = error.downcast::<RejectedInput>()?;
                    json!({"type":"ack", "id":id, "error":{"kind":"rejected", "detail":rejection.0}})
                }
            };
            acked.send(ack).await?;
        }
        Ok(())
    };
    tokio::pin!(deliveries);
    let mut awaiting_ack = false;
    let mut serial = 0u64;
    loop {
        let deadline = pending.values().map(|(deadline, _)| *deadline).min();
        tokio::select! {
            frame = framing::read(&mut reader, &mut partial) => match frame? {
                AdapterFrame::Inbound { id, message } => {
                    ensure!(!id.is_empty() && !awaiting_ack, "expected one inbound at a time");
                    inbound.try_send((id, message)).map_err(|_| anyhow!("gateway inbound unavailable"))?;
                    awaiting_ack = true;
                }
                AdapterFrame::Result(outcome) => {
                    let (id, reply) = match outcome {
                        Outcome::Success { id, value } => (id, Ok(value)),
                        Outcome::Failure { id, error } => (id, Err(error.into())),
                    };
                    let (_, finish) = pending.remove(&id).ok_or_else(|| anyhow!("unexpected gateway correlation ID"))?;
                    finish(reply)?;
                }
                AdapterFrame::Hello { .. } => bail!("duplicate hello"),
            },
            result = &mut deliveries => return result,
            Some(ack) = acks.recv() => {
                send(&mut writer, ack).await?;
                awaiting_ack = false;
            }
            Some(call) = calls.recv(), if pending.len() < MAX_PENDING => {
                serial += 1;
                let id = format!("s{serial}");
                send(&mut writer, json!({"type":"call","id":id,"op":call.op,"args":call.args})).await?;
                pending.insert(id, (Instant::now() + IO_TIMEOUT, call.finish));
            }
            _ = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                bail!("gateway result timed out");
            }
        }
    }
}
