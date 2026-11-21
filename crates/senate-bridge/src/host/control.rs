//! Stop and nudge requests use a private socket so they can reach a host
//! with a viewer.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use senate_core::{EventId, NewEvent};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::{Tty, Viewer, accept, directory, observed_at};
use crate::ipc::{self, IpcError, codes};
use crate::paths::Paths;
use crate::state::Store;

const ATTENDED: u8 = 0;
const ACCEPTED: u8 = 1;
const STOPPED: u8 = 1;
const FAILED: u8 = 2;
const END_UNCONFIRMED: u8 = 3;
const HANDSHAKE: Duration = Duration::from_secs(2);
/// The nudge replies.
const NUDGED: u8 = 4;
const TYPED_RECENTLY: u8 = 5;
const DRAFT_PENDING: u8 = 6;

/// The one line a host types into an idle hosted agy when something waits
/// for it: Senate's own fixed text, never anything a peer wrote. agy's
/// PreInvocation hook then hands over what waits, as at any prompt.
pub(crate) const AGY_NUDGE: &str = "[senate] new messages";
/// Between the nudge's text and its Enter: sent together, a TUI can take
/// the pair as a paste and the Enter as a newline in it.
const NUDGE_ENTER_DELAY: Duration = Duration::from_millis(150);

#[derive(Serialize, Deserialize)]
struct StopRequest {
    force: bool,
    unconfirmed_audit: NewEvent,
}

/// Type [`AGY_NUDGE`] unless a viewer typed within `quiet_ms`.
#[derive(Serialize, Deserialize)]
struct NudgeRequest {
    nudge: NudgeParams,
}

#[derive(Serialize, Deserialize)]
struct NudgeParams {
    quiet_ms: u64,
}

/// What a nudge request came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nudge {
    /// The line was typed.
    Typed,
    /// Someone typed in the terminal lately; ask again after this long.
    Wait(Duration),
    /// A viewer left a line typed but not submitted in the prompt: the
    /// nudge would be appended to it and submit it.
    Draft,
}

fn socket(paths: &Paths, id: &EventId) -> PathBuf {
    directory(paths, id).join("ctl")
}

pub(super) fn listen(paths: &Paths, id: &EventId) -> io::Result<UnixListener> {
    let path = socket(paths, id);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serves nudges until a stop request is accepted, and returns its stream.
pub(super) async fn next_stop(
    listener: &UnixListener,
    viewer: &Mutex<Viewer>,
    tty: &Tty,
    paths: &Paths,
    host: &EventId,
) -> UnixStream {
    loop {
        let (mut stream, _) = accept(listener).await;
        let Ok(Ok(Some(line))) =
            tokio::time::timeout(HANDSHAKE, ipc::read_line(&mut BufReader::new(&mut stream))).await
        else {
            continue;
        };
        if let Ok(NudgeRequest { nudge }) = serde_json::from_str::<NudgeRequest>(&line) {
            nudge_once(
                &mut stream,
                viewer,
                tty,
                Duration::from_millis(nudge.quiet_ms),
            )
            .await;
            continue;
        }
        let Ok(request) = serde_json::from_str::<StopRequest>(&line) else {
            continue;
        };
        let attended = Viewer::lock(viewer)
            .activity
            .unattended_since(observed_at())
            .is_none();
        if attended && !request.force {
            let _ = stream.write_u8(ATTENDED).await;
            continue;
        }
        if let Err(error) =
            Store::accept_host_stop(&paths.state_db(), host, &request.unconfirmed_audit)
        {
            tracing::warn!(%error, %host, "recording an accepted terminal stop");
            let _ = stream.write_u8(FAILED).await;
            continue;
        }
        // The durable row commits the stop. A disconnected daemon cannot
        // cancel it or lose its audit after this point.
        let _ = stream.write_u8(ACCEPTED).await;
        return stream;
    }
}

/// Type the nudge into the TUI, unless a viewer typed within `quiet` or
/// left a line unsubmitted, which it never touches. A
/// watcher records no input, so watching never holds a nudge back.
async fn nudge_once(stream: &mut UnixStream, viewer: &Mutex<Viewer>, tty: &Tty, quiet: Duration) {
    let now = observed_at();
    let (input_at, draft) = {
        let viewer = Viewer::lock(viewer);
        (viewer.activity.input_at, viewer.draft.pending())
    };
    if draft {
        let _ = stream.write_u8(DRAFT_PENDING).await;
        return;
    }
    let quiet_ns = u64::try_from(quiet.as_nanos()).unwrap_or(u64::MAX);
    let since = now.saturating_sub(input_at);
    if input_at > 0 && since < quiet_ns {
        let wait = Duration::from_nanos(quiet_ns - since);
        let mut reply = vec![TYPED_RECENTLY];
        reply.extend_from_slice(
            &u64::try_from(wait.as_millis())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        let _ = stream.write_all(&reply).await;
        return;
    }
    let typed = async {
        tty.write
            .lock()
            .await
            .write_all(AGY_NUDGE.as_bytes())
            .await?;
        tokio::time::sleep(NUDGE_ENTER_DELAY).await;
        tty.write.lock().await.write_all(b"\r").await
    }
    .await;
    match typed {
        Ok(()) => {
            let _ = stream.write_u8(NUDGED).await;
        }
        Err(error) => {
            tracing::warn!(%error, "typing the idle nudge into the coding TUI");
            let _ = stream.write_u8(FAILED).await;
        }
    }
}

/// Ask host `id` to type the idle nudge into its TUI, unless a viewer typed
/// there within `quiet`.
pub(crate) async fn nudge(paths: &Paths, id: &EventId, quiet: Duration) -> Result<Nudge, IpcError> {
    let mut payload = serde_json::to_vec(&NudgeRequest {
        nudge: NudgeParams {
            quiet_ms: u64::try_from(quiet.as_millis()).unwrap_or(u64::MAX),
        },
    })
    .map_err(IpcError::internal)?;
    payload.push(b'\n');
    tokio::time::timeout(HANDSHAKE, async {
        let mut stream = UnixStream::connect(socket(paths, id))
            .await
            .map_err(IpcError::internal)?;
        stream
            .write_all(&payload)
            .await
            .map_err(IpcError::internal)?;
        match stream.read_u8().await {
            Ok(NUDGED) => Ok(Nudge::Typed),
            Ok(DRAFT_PENDING) => Ok(Nudge::Draft),
            Ok(TYPED_RECENTLY) => {
                let ms = stream.read_u64().await.map_err(IpcError::internal)?;
                Ok(Nudge::Wait(Duration::from_millis(ms)))
            }
            Ok(_) => Err(IpcError::internal("the host could not type the nudge")),
            Err(error) => Err(IpcError::internal(format!(
                "the host did not answer the nudge: {error}"
            ))),
        }
    })
    .await
    .map_err(|_| IpcError::internal("timed out waiting for the terminal host to nudge"))?
}

/// Release the session binding guard after this acknowledgement, before the
/// host sends its normal HostedSessionEnd request back to the daemon.
pub(crate) async fn begin_stop(
    paths: &Paths,
    id: &EventId,
    force: bool,
    unconfirmed_audit: NewEvent,
) -> Result<UnixStream, IpcError> {
    let mut payload = serde_json::to_vec(&StopRequest {
        force,
        unconfirmed_audit,
    })
    .map_err(IpcError::internal)?;
    payload.push(b'\n');
    tokio::time::timeout(HANDSHAKE, async {
        let mut stream = UnixStream::connect(socket(paths, id)).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                IpcError::bad_request("the host's stop socket is unavailable. If it was started before this update, attach and exit the terminal")
            } else {
                IpcError::internal(error)
            }
        })?;
        stream.write_all(&payload).await.map_err(IpcError::internal)?;
        match stream.read_u8().await {
            Ok(ACCEPTED) => Ok(stream),
            Ok(ATTENDED) => Err(IpcError::bad_request(
                "a viewer is attending this terminal. Ask them to detach or have the operator run senate session stop --force",
            )),
            Ok(FAILED) => Err(IpcError::internal("the terminal host could not record the stop request")),
            Ok(_) => Err(IpcError::internal("the host sent an invalid stop reply")),
            Err(error) => Err(IpcError::internal(format!("the host did not acknowledge the stop: {error}"))),
        }
    }).await.map_err(|_| IpcError::internal("timed out waiting for the terminal host to accept stop"))?
}

pub(super) async fn acknowledge(mut stream: UnixStream, stopped: bool, recorded: bool) {
    let reply = match (stopped, recorded) {
        (false, _) => FAILED,
        (true, false) => END_UNCONFIRMED,
        (true, true) => STOPPED,
    };
    let _ = stream.write_u8(reply).await;
}

/// Returns whether the usual host-end request was acknowledged by the bridge.
pub(crate) async fn wait_stopped(mut stream: UnixStream) -> Result<bool, IpcError> {
    match tokio::time::timeout(Duration::from_secs(8), stream.read_u8()).await {
        Ok(Ok(STOPPED)) => Ok(true),
        Ok(Ok(END_UNCONFIRMED)) => Ok(false),
        Ok(Ok(FAILED)) => Err(IpcError::internal(
            "the terminal host could not stop its process group",
        )),
        _ => Err(IpcError::new(
            codes::INTERNAL,
            "the terminal host did not confirm shutdown",
        )),
    }
}
