//! Stop, nudge and answer requests use a private socket so they can reach
//! a host with a viewer.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use senate_core::{EventId, NewEvent};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

use super::prompt::{self, NoChoice, PermissionChoice};
use super::{Tty, Viewer, accept, directory, observed_at};
use crate::ipc::{self, IpcError, codes};
use crate::paths::Paths;
use crate::state::Store;

const ATTENDED: u8 = 0;
const ACCEPTED: u8 = 1;
// The host checked attendance. The daemon may now interrupt the Codex turn.
const READY_FOR_INTERRUPT: u8 = 11;
const COMMIT_STOP: u8 = 12;
const INTERRUPT_LIMIT: Duration = Duration::from_secs(4);
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

/// The answer replies.
const ANSWERED: u8 = 7;
const NO_DIALOG: u8 = 8;
const NO_OPTION: u8 = 9;
const UNCHANGED: u8 = 10;
/// How long the TUI has to redraw after the key that answers its dialog.
const ANSWER_SETTLE: Duration = Duration::from_secs(3);

/// Type the key of `answer` into the Claude Code permission dialog on the
/// screen ([`prompt::permission_key`]); nothing when there is none.
#[derive(Serialize, Deserialize)]
struct AnswerRequest {
    answer: PermissionChoice,
}

/// Why a host typed no answer, or what came of the one it typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// The key was typed and the TUI redrew.
    Answered,
    /// The screen shows no permission dialog.
    NoDialog,
    /// The dialog has no option for this choice.
    NoOption,
    /// The key was typed, but the screen did not change.
    Unchanged,
}

#[derive(Serialize, Deserialize)]
struct StopRequest {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    prepare_interrupt: bool,
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
        if let Ok(AnswerRequest { answer }) = serde_json::from_str::<AnswerRequest>(&line) {
            answer_once(&mut stream, tty, answer).await;
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
        if request.prepare_interrupt {
            if stream.write_u8(READY_FOR_INTERRUPT).await.is_err() {
                continue;
            }
            // A failed interrupt or a disconnected daemon leaves the host running.
            if !matches!(
                tokio::time::timeout(INTERRUPT_LIMIT + HANDSHAKE, stream.read_u8()).await,
                Ok(Ok(COMMIT_STOP))
            ) {
                continue;
            }
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
    let wait = if prompt::looks_like_prompt(&tty.screen.rows()) {
        Some(quiet)
    } else if input_at > 0 && since < quiet_ns {
        Some(Duration::from_nanos(quiet_ns - since))
    } else {
        None
    };
    if let Some(wait) = wait {
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

/// Type the key that makes `choice` in the permission dialog on screen, as
/// a person at the terminal would, and wait for the TUI to redraw. Never
/// types when the screen shows no such dialog or option.
async fn answer_once(stream: &mut UnixStream, tty: &Tty, choice: PermissionChoice) {
    let before = tty.screen.rows();
    let key = match prompt::permission_key(&before, choice) {
        Ok(key) => key,
        Err(NoChoice::NotShown) => {
            let _ = stream.write_u8(NO_DIALOG).await;
            return;
        }
        Err(NoChoice::NoOption) => {
            let _ = stream.write_u8(NO_OPTION).await;
            return;
        }
    };
    let mut buf = [0; 4];
    let typed = tty
        .write
        .lock()
        .await
        .write_all(key.encode_utf8(&mut buf).as_bytes())
        .await;
    if let Err(error) = typed {
        tracing::warn!(%error, "typing a permission answer into the coding TUI");
        let _ = stream.write_u8(FAILED).await;
        return;
    }
    let deadline = tokio::time::Instant::now() + ANSWER_SETTLE;
    let reply = loop {
        if tty.screen.rows() != before {
            break ANSWERED;
        }
        if tokio::time::Instant::now() >= deadline {
            break UNCHANGED;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let _ = stream.write_u8(reply).await;
}

/// Ask host `id` to answer the Claude Code permission dialog on its screen.
pub(crate) async fn answer(
    paths: &Paths,
    id: &EventId,
    choice: PermissionChoice,
) -> Result<Answer, IpcError> {
    let mut payload =
        serde_json::to_vec(&AnswerRequest { answer: choice }).map_err(IpcError::internal)?;
    payload.push(b'\n');
    tokio::time::timeout(HANDSHAKE + ANSWER_SETTLE, async {
        let mut stream = UnixStream::connect(socket(paths, id))
            .await
            .map_err(IpcError::internal)?;
        stream
            .write_all(&payload)
            .await
            .map_err(IpcError::internal)?;
        match stream.read_u8().await {
            Ok(ANSWERED) => Ok(Answer::Answered),
            Ok(NO_DIALOG) => Ok(Answer::NoDialog),
            Ok(NO_OPTION) => Ok(Answer::NoOption),
            Ok(UNCHANGED) => Ok(Answer::Unchanged),
            Ok(_) => Err(IpcError::internal("the host could not type the answer")),
            Err(error) => Err(IpcError::internal(format!(
                "the host did not answer: {error}"
            ))),
        }
    })
    .await
    .map_err(|_| IpcError::internal("timed out waiting for the terminal host to answer"))?
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
    codex: Option<(&crate::delivery::codex::Transport, &str)>,
) -> Result<UnixStream, IpcError> {
    let mut payload = serde_json::to_vec(&StopRequest {
        prepare_interrupt: codex.is_some(),
        force,
        unconfirmed_audit,
    })
    .map_err(IpcError::internal)?;
    payload.push(b'\n');
    let (mut stream, first) = tokio::time::timeout(HANDSHAKE, async {
        let mut stream = UnixStream::connect(socket(paths, id)).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                IpcError::bad_request("the host's stop socket is unavailable. If it was started before this update, attach and exit the terminal")
            } else {
                IpcError::internal(error)
            }
        })?;
        stream.write_all(&payload).await.map_err(IpcError::internal)?;
        let first = stream.read_u8().await.map_err(IpcError::internal)?;
        Ok::<_, IpcError>((stream, first))
    }).await.map_err(|_| IpcError::internal("timed out waiting for the terminal host to accept stop"))??;
    if let Some((transport, thread)) = codex
        && matches!(first, READY_FOR_INTERRUPT | ACCEPTED)
    {
        // Hosts from before the handshake already committed on ACCEPTED. Still
        // interrupt their shared thread before reporting a successful stop.
        let interrupt = async {
            let mut server = crate::delivery::codex::AppServer::connect(transport).await?;
            server.interrupt_for_stop(thread).await
        };
        tokio::time::timeout(INTERRUPT_LIMIT, interrupt)
            .await
            .map_err(|_| {
                IpcError::internal(format!(
                    "timed out waiting for Codex thread {thread} to stop"
                ))
            })?
            .map_err(|e| {
                IpcError::internal(format!(
                    "could not confirm Codex thread {thread} stopped: {e}"
                ))
            })?;
    }
    let reply = if first == READY_FOR_INTERRUPT && codex.is_some() {
        stream
            .write_u8(COMMIT_STOP)
            .await
            .map_err(IpcError::internal)?;
        tokio::time::timeout(HANDSHAKE, stream.read_u8())
            .await
            .map_err(|_| IpcError::internal("the terminal host did not commit stop in time"))?
    } else {
        Ok(first)
    };
    match reply {
        Ok(ACCEPTED) => Ok(stream),
        Ok(ATTENDED) => Err(IpcError::bad_request(
            "a viewer is attending this terminal. Ask them to detach or have the operator run senate session stop --force",
        )),
        Ok(FAILED) => Err(IpcError::internal(
            "the terminal host could not record the stop request",
        )),
        Ok(_) => Err(IpcError::internal("the host sent an invalid stop reply")),
        Err(error) => Err(IpcError::internal(format!(
            "the host did not acknowledge the stop: {error}"
        ))),
    }
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
