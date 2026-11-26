//! Stop, wake and prompt requests use a private socket so they can reach
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
/// The wake replies.
const NUDGED: u8 = 4;
const TYPED_RECENTLY: u8 = 5;
const DRAFT_PENDING: u8 = 6;
const PROMPT_SHOWN: u8 = 16;
const TUI_BUSY: u8 = 17;

/// The one line a host types into an idle hosted session when something
/// waits for it unseen: Senate's own fixed text, never anything a peer
/// wrote. The runtime's own prompt hook then takes what waits, as at any
/// prompt a person types: agy's PreInvocation and Claude Code's
/// UserPromptSubmit hand it over, and Codex's starts the push again.
pub(crate) const WAKE_LINE: &str = "[senate] new messages";
/// Between the line's text and its Enter: sent together, a TUI can take
/// the pair as a paste and the Enter as a newline in it.
const WAKE_ENTER_DELAY: Duration = Duration::from_millis(150);

/// The find replies.
const FOUND: u8 = 13;
const NO_VIEWER: u8 = 14;
const DIALOG_SHOWN: u8 = 15;

#[derive(Deserialize)]
struct CheckPermissionRequest {
    #[serde(rename = "check_permission")]
    _check_permission: CheckPermissionParams,
}

#[derive(Deserialize)]
struct CheckPermissionParams {}

/// Make the attached terminal ring and flash its title, so a person finds
/// the panel that holds the session.
#[derive(Serialize, Deserialize)]
struct FindRequest {
    find: FindParams,
}

#[derive(Serialize, Deserialize)]
struct FindParams {}

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

/// Type [`WAKE_LINE`] if the terminal has been quiet for `quiet_ms` and
/// shows no prompt and no draft ([`wake_once`]).
#[derive(Serialize, Deserialize)]
struct WakeRequest {
    wake: WakeParams,
}

/// What a host from before [`WakeRequest`] takes: the same line, without
/// the check that its TUI is quiet. Asked only of an agy, which had it.
#[derive(Serialize, Deserialize)]
struct NudgeRequest {
    nudge: WakeParams,
}

#[derive(Serialize, Deserialize)]
struct WakeParams {
    quiet_ms: u64,
}

/// What a wake request came to. Only [`Wake::Typed`] typed anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wake {
    /// The line was typed.
    Typed,
    /// Someone typed in the terminal lately; ask again after this long.
    Typing(Duration),
    /// A viewer left a line typed but not submitted in the prompt: the
    /// line would be appended to it and submit it.
    Draft,
    /// The screen shows a prompt or a choice: a key typed there would
    /// answer it.
    Prompt,
    /// The TUI wrote to its terminal lately: a turn is running. Ask again
    /// after this long.
    Busy(Duration),
    /// The host is from before the wake request and its runtime never had
    /// the older one.
    Unsupported,
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
    admission: &super::Admission,
) -> UnixStream {
    loop {
        let (mut stream, _) = accept(listener).await;
        if !admission.is_daemon(&stream, host).await {
            tracing::warn!(%host, "refused a terminal control request from an unverified daemon peer");
            continue;
        }
        let Ok(Ok(Some(line))) =
            tokio::time::timeout(HANDSHAKE, ipc::read_line(&mut BufReader::new(&mut stream))).await
        else {
            continue;
        };
        if let Ok(GrantViewer { viewer }) = serde_json::from_str::<GrantViewer>(&line) {
            let _ = stream.write_u8(u8::from(admission.grant(viewer))).await;
            continue;
        }
        if line == r#"{"protocol_probe":true}"# {
            let _ = stream.write_u8(1).await;
            continue;
        }
        if let Ok(WakeRequest { wake }) = serde_json::from_str::<WakeRequest>(&line) {
            wake_once(
                &mut stream,
                viewer,
                tty,
                Duration::from_millis(wake.quiet_ms),
                observed_at,
            )
            .await;
            continue;
        }
        if let Ok(AnswerRequest { answer }) = serde_json::from_str::<AnswerRequest>(&line) {
            answer_once(&mut stream, tty, answer).await;
            continue;
        }
        if serde_json::from_str::<CheckPermissionRequest>(&line).is_ok() {
            let shown = prompt::permission_key(&tty.screen.rows(), PermissionChoice::Once).is_ok();
            let _ = stream
                .write_u8(if shown { DIALOG_SHOWN } else { NO_DIALOG })
                .await;
            continue;
        }
        if serde_json::from_str::<FindRequest>(&line).is_ok() {
            let attached = tty.attached.load(std::sync::atomic::Ordering::Acquire);
            if attached {
                tty.find.notify_one();
            }
            let _ = stream
                .write_u8(if attached { FOUND } else { NO_VIEWER })
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

pub(crate) async fn refresh_daemon_identity(paths: &Paths, id: &EventId) {
    let _ = tokio::time::timeout(Duration::from_millis(300), async {
        let mut stream = UnixStream::connect(socket(paths, id)).await?;
        stream.write_all(b"{\"protocol_probe\":true}\n").await?;
        stream.read_u8().await
    })
    .await;
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GrantViewer {
    viewer: super::ViewTicket,
}

pub(crate) async fn grant_viewer(
    paths: &Paths,
    host: &EventId,
    peer: u32,
    writable: bool,
) -> Result<String, IpcError> {
    let peer = crate::kernel_process::inspect(peer)
        .and_then(|process| crate::kernel_process::BoundProcess::capture(process.identity))
        .map_err(IpcError::internal)?;
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).map_err(IpcError::internal)?;
    let nonce = hex::encode(nonce);
    let mut frame = serde_json::to_vec(&GrantViewer {
        viewer: super::ViewTicket {
            peer,
            writable,
            nonce: nonce.clone(),
        },
    })
    .map_err(IpcError::internal)?;
    frame.push(b'\n');
    tokio::time::timeout(HANDSHAKE, async {
        let mut stream = UnixStream::connect(socket(paths, host)).await?;
        stream.write_all(&frame).await?;
        stream.read_u8().await
    })
    .await
    .map_err(IpcError::internal)?
    .map_err(IpcError::internal)
    .and_then(|accepted| {
        if accepted == 1 {
            Ok(nonce)
        } else {
            Err(IpcError::bad_request(
                "The terminal host declined the viewer",
            ))
        }
    })
}

/// Whether the screen shows something a typed Enter would answer: a
/// prompt or choice as [`prompt::looks_like_prompt`] sees it, or a choice
/// whose options are spread out by long descriptions.
fn prompt_on_screen(tty: &Tty) -> bool {
    let rows = tty.screen.rows();
    prompt::looks_like_prompt(&rows) || prompt::may_be_choice(&rows)
}

/// Type the wake line into the TUI, and only into one that sits idle at
/// its own prompt. It is never typed onto a line a viewer left unsubmitted,
/// into a prompt or a choice on the screen (the key would answer it),
/// within `quiet` of a viewer typing, or within `quiet` of the TUI writing
/// to its terminal (a turn is running: [`super::activity`]). A watcher
/// records no input, so watching never holds a wake back.
async fn wake_once(
    stream: &mut UnixStream,
    viewer: &Mutex<Viewer>,
    tty: &Tty,
    quiet: Duration,
    now: impl FnOnce() -> u64,
) {
    // Check the draft in the same order as viewer writes. Keep the lock
    // through Enter so no viewer input can become part of the line.
    let mut write = tty.write.lock().await;
    let now = now();
    let (input_at, draft) = {
        let viewer = Viewer::lock(viewer);
        (viewer.activity.input_at, viewer.draft.pending())
    };
    if draft {
        drop(write);
        let _ = stream.write_u8(DRAFT_PENDING).await;
        return;
    }
    if prompt_on_screen(tty) {
        drop(write);
        let _ = stream.write_u8(PROMPT_SHOWN).await;
        return;
    }
    let quiet_ns = u64::try_from(quiet.as_nanos()).unwrap_or(u64::MAX);
    let remaining = |since: u64| {
        let since = now.saturating_sub(since);
        (since < quiet_ns).then(|| Duration::from_nanos(quiet_ns - since))
    };
    let wait = match remaining(input_at).filter(|_| input_at > 0) {
        Some(wait) => Some((TYPED_RECENTLY, wait)),
        None => remaining(tty.screen.last_output()).map(|wait| (TUI_BUSY, wait)),
    };
    if let Some((why, wait)) = wait {
        drop(write);
        let mut reply = vec![why];
        reply.extend_from_slice(
            &u64::try_from(wait.as_millis())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        let _ = stream.write_all(&reply).await;
        return;
    }
    // A dialog may come up while the line waits for its Enter (a
    // background task asking permission): look again, and leave the
    // Enter out if one did, since it would pick the dialog's first option.
    let typed = async {
        write.write_all(WAKE_LINE.as_bytes()).await?;
        tokio::time::sleep(WAKE_ENTER_DELAY).await;
        if prompt_on_screen(tty) {
            return Ok(false);
        }
        write.write_all(b"\r").await.map(|()| true)
    }
    .await;
    drop(write);
    match typed {
        Ok(true) => {
            let _ = stream.write_u8(NUDGED).await;
        }
        Ok(false) => {
            let _ = stream.write_u8(PROMPT_SHOWN).await;
        }
        Err(error) => {
            tracing::warn!(%error, "typing the wake line into the coding TUI");
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

/// Read the current screen without typing. A host that cannot report its
/// screen, including an older host, gives no evidence either way.
pub(crate) async fn permission_shown(paths: &Paths, id: &EventId) -> Option<bool> {
    tokio::time::timeout(HANDSHAKE, async {
        let mut stream = UnixStream::connect(socket(paths, id)).await.ok()?;
        stream
            .write_all(b"{\"check_permission\":{}}\n")
            .await
            .ok()?;
        match stream.read_u8().await.ok()? {
            DIALOG_SHOWN => Some(true),
            NO_DIALOG => Some(false),
            _ => None,
        }
    })
    .await
    .ok()
    .flatten()
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

/// Ask host `id` to make its attached terminal ring and flash its title:
/// whether a terminal is attached to it.
pub(crate) async fn find(paths: &Paths, id: &EventId) -> Result<bool, IpcError> {
    let mut payload = serde_json::to_vec(&FindRequest {
        find: FindParams {},
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
            Ok(FOUND) => Ok(true),
            Ok(NO_VIEWER) => Ok(false),
            // A host from before `find` closes the connection.
            _ => Err(IpcError::bad_request(
                "this terminal host is older than `senate session find`; it cannot ring its terminal",
            )),
        }
    })
    .await
    .map_err(|_| IpcError::internal("timed out waiting for the terminal host"))?
}

/// Ask host `id` to type the wake line into its TUI if it sits idle
/// ([`wake_once`]): quiet for `quiet`, no draft, no prompt. A host from
/// before the request closes the connection without a word; `older` then
/// asks it the way it knows, which only an agy's host does.
pub(crate) async fn wake(
    paths: &Paths,
    id: &EventId,
    quiet: Duration,
    older: bool,
) -> Result<Wake, IpcError> {
    let params = || WakeParams {
        quiet_ms: u64::try_from(quiet.as_millis()).unwrap_or(u64::MAX),
    };
    let payload =
        serde_json::to_vec(&WakeRequest { wake: params() }).map_err(IpcError::internal)?;
    match ask_to_wake(paths, id, payload).await? {
        Wake::Unsupported if older => {
            let payload = serde_json::to_vec(&NudgeRequest { nudge: params() })
                .map_err(IpcError::internal)?;
            ask_to_wake(paths, id, payload).await
        }
        wake => Ok(wake),
    }
}

async fn ask_to_wake(paths: &Paths, id: &EventId, mut payload: Vec<u8>) -> Result<Wake, IpcError> {
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
            Ok(NUDGED) => Ok(Wake::Typed),
            Ok(DRAFT_PENDING) => Ok(Wake::Draft),
            Ok(PROMPT_SHOWN) => Ok(Wake::Prompt),
            Ok(why @ (TYPED_RECENTLY | TUI_BUSY)) => {
                let ms = stream.read_u64().await.map_err(IpcError::internal)?;
                let wait = Duration::from_millis(ms);
                Ok(if why == TUI_BUSY {
                    Wake::Busy(wait)
                } else {
                    Wake::Typing(wait)
                })
            }
            Ok(_) => Err(IpcError::internal("the host could not type the wake line")),
            // A request the host does not know: it reads the next one.
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(Wake::Unsupported),
            Err(error) => Err(IpcError::internal(format!(
                "the host did not answer the wake request: {error}"
            ))),
        }
    })
    .await
    .map_err(|_| IpcError::internal("timed out waiting for the terminal host to wake its TUI"))?
}

/// Release the session binding guard after this acknowledgement, before the
/// host sends its normal HostedSessionEnd request back to the daemon.
pub(crate) async fn begin_stop(
    paths: &Paths,
    id: &EventId,
    force: bool,
    unconfirmed_audit: NewEvent,
    codex: Option<(&crate::delivery::codex::Transport, &str)>,
    idle_only: bool,
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
            use crate::delivery::codex::ThreadState;
            let mut server = crate::delivery::codex::AppServer::connect(transport).await?;
            if idle_only {
                // An unforced restart must not interrupt a turn that began
                // after preflight. A dropped stream cancels a staged stop.
                return server
                    .thread_state(thread)
                    .await
                    .map(|state| matches!(state, ThreadState::Idle | ThreadState::NotLoaded));
            }
            server.interrupt_for_stop(thread).await.map(|()| true)
        };
        let ready = tokio::time::timeout(INTERRUPT_LIMIT, interrupt)
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
        if !ready {
            return Err(IpcError::bad_request(
                "Codex started a turn before restart could stop the host. Use --force to interrupt it.",
            ));
        }
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

#[cfg(test)]
mod tests {
    use std::io::Read as _;

    use super::*;
    use crate::host::{ViewerActivity, emulation::Scheme, forward_keys, input};

    /// What a wake request's reply says, as the daemon reads it.
    async fn reply(client: &mut UnixStream) -> anyhow::Result<(u8, Option<u64>)> {
        let code = client.read_u8().await?;
        let wait = match code {
            TYPED_RECENTLY | TUI_BUSY => Some(client.read_u64().await?),
            _ => None,
        };
        Ok((code, wait))
    }

    #[tokio::test(start_paused = true)]
    async fn a_wake_is_typed_only_into_a_quiet_tui_with_no_prompt_on_its_screen()
    -> anyhow::Result<()> {
        let (pty, pts) = pty_process::open()?;
        let mut slave = std::fs::File::from(rustix::io::dup(&pts)?);
        let flags = rustix::fs::fcntl_getfl(&slave)?;
        rustix::fs::fcntl_setfl(&slave, flags | rustix::fs::OFlags::NONBLOCK)?;
        let (_, write) = pty.into_split();
        let tty = Tty::new(write, Scheme::Light, 24, 80);
        let viewer = Mutex::new(Viewer {
            path: PathBuf::new(),
            activity: ViewerActivity::default(),
            dirty: false,
            draft: input::Draft::default(),
        });
        let (mut server, mut client) = UnixStream::pair()?;
        let quiet = Duration::from_secs(10);
        let now = 100_000_000_000;
        let typed = |slave: &mut std::fs::File| {
            let mut bytes = [0; 64];
            slave.read(&mut bytes).unwrap_or(0)
        };

        // The TUI wrote 4 s ago: a turn is running. Nothing is typed.
        tty.screen.set_last_output(now - 4_000_000_000);
        wake_once(&mut server, &viewer, &tty, quiet, || now).await;
        assert_eq!(reply(&mut client).await?, (TUI_BUSY, Some(6000)));
        assert_eq!(typed(&mut slave), 0, "typed into a busy TUI");

        // Quiet, but a choice is on the screen: a key would answer it.
        tty.screen.set_last_output(now - 60_000_000_000);
        tty.screen
            .output(b"Allow this command?\r\n\xe2\x9d\xaf 1. Yes\r\n  2. No\r\n");
        tty.screen.set_last_output(now - 60_000_000_000);
        wake_once(&mut server, &viewer, &tty, quiet, || now).await;
        assert_eq!(reply(&mut client).await?, (PROMPT_SHOWN, None));
        assert_eq!(typed(&mut slave), 0, "typed into a prompt");

        // The choice is gone and the TUI is quiet: the line goes in.
        tty.screen.output(b"\x1b[2J\x1b[H> ");
        tty.screen.set_last_output(now - 60_000_000_000);
        wake_once(&mut server, &viewer, &tty, quiet, || now).await;
        assert_eq!(reply(&mut client).await?, (NUDGED, None));
        assert!(typed(&mut slave) > 0);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_wake_is_not_typed_into_a_question_with_spread_out_options() -> anyhow::Result<()> {
        let (pty, pts) = pty_process::open()?;
        let mut slave = std::fs::File::from(rustix::io::dup(&pts)?);
        let flags = rustix::fs::fcntl_getfl(&slave)?;
        rustix::fs::fcntl_setfl(&slave, flags | rustix::fs::OFlags::NONBLOCK)?;
        let (_, write) = pty.into_split();
        let tty = Tty::new(write, Scheme::Light, 24, 80);
        let viewer = Mutex::new(Viewer {
            path: PathBuf::new(),
            activity: ViewerActivity::default(),
            dirty: false,
            draft: input::Draft::default(),
        });
        let (mut server, mut client) = UnixStream::pair()?;
        let now = 100_000_000_000;
        // Each option carries four rows of description, as a question an
        // agent asks may: the Enter would pick option 1.
        tty.screen.output(
            "Which approach?\r\n\u{276f} 1. Rewrite\r\n   a\r\n   b\r\n   c\r\n   d\r\n  2. Patch\r\n"
                .as_bytes(),
        );
        tty.screen.set_last_output(now - 60_000_000_000);
        wake_once(&mut server, &viewer, &tty, Duration::from_secs(10), || now).await;
        assert_eq!(reply(&mut client).await?, (PROMPT_SHOWN, None));
        let mut bytes = [0; 64];
        assert_eq!(
            slave.read(&mut bytes).unwrap_or(0),
            0,
            "typed into a question"
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_that_comes_up_before_enter_keeps_the_enter() -> anyhow::Result<()> {
        let (pty, pts) = pty_process::open()?;
        let mut slave = std::fs::File::from(rustix::io::dup(&pts)?);
        let mut raw = rustix::termios::tcgetattr(&slave)?;
        raw.make_raw();
        rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &raw)?;
        let flags = rustix::fs::fcntl_getfl(&slave)?;
        rustix::fs::fcntl_setfl(&slave, flags | rustix::fs::OFlags::NONBLOCK)?;
        let (_, write) = pty.into_split();
        let tty = Tty::new(write, Scheme::Light, 24, 80);
        let viewer = Mutex::new(Viewer {
            path: PathBuf::new(),
            activity: ViewerActivity::default(),
            dirty: false,
            draft: input::Draft::default(),
        });
        let (mut server, mut client) = UnixStream::pair()?;
        let mut nudge = std::pin::pin!(wake_once(
            &mut server,
            &viewer,
            &tty,
            Duration::ZERO,
            observed_at
        ));
        let mut prefix = [0; WAKE_LINE.len()];
        let mut read = 0;
        while read < prefix.len() {
            assert!(futures::poll!(nudge.as_mut()).is_pending());
            match slave.read(&mut prefix[read..]) {
                Ok(0) => anyhow::bail!("PTY closed before the wake line arrived"),
                Ok(n) => read += n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        assert_eq!(prefix, WAKE_LINE.as_bytes());
        // A permission dialog comes up while the line waits for its Enter.
        tty.screen
            .output(b"Allow this command?\r\n\xe2\x9d\xaf 1. Yes\r\n  2. No\r\n");
        nudge.await;
        assert_eq!(client.read_u8().await?, PROMPT_SHOWN);
        let mut tail = [0; 8];
        match slave.read(&mut tail) {
            Ok(n) => assert_eq!(n, 0, "typed {:?} after the dialog came up", &tail[..n]),
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
        }
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn wake_quiet_window_starts_at_viewer_input() -> anyhow::Result<()> {
        let (pty, _pts) = pty_process::open()?;
        let (_, write) = pty.into_split();
        let tty = Tty::new(write, Scheme::Light, 24, 80);
        tty.screen.set_last_output(0);
        let input_at = 1_000_000_000;
        let viewer = Mutex::new(Viewer {
            path: PathBuf::new(),
            activity: ViewerActivity {
                input_at,
                ..ViewerActivity::default()
            },
            dirty: false,
            draft: input::Draft::default(),
        });
        let (mut server, mut client) = UnixStream::pair()?;
        let quiet = Duration::from_secs(2);

        wake_once(&mut server, &viewer, &tty, quiet, || {
            input_at + 1_200_000_000
        })
        .await;
        assert_eq!(reply(&mut client).await?, (TYPED_RECENTLY, Some(800)));

        wake_once(&mut server, &viewer, &tty, quiet, || {
            input_at + 2_000_000_000
        })
        .await;
        assert_eq!(client.read_u8().await?, NUDGED);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn viewer_keys_cannot_join_the_wake_line_before_enter() -> anyhow::Result<()> {
        let (pty, pts) = pty_process::open()?;
        let mut slave = std::fs::File::from(rustix::io::dup(&pts)?);
        let mut raw = rustix::termios::tcgetattr(&slave)?;
        raw.make_raw();
        rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &raw)?;
        let flags = rustix::fs::fcntl_getfl(&slave)?;
        rustix::fs::fcntl_setfl(&slave, flags | rustix::fs::OFlags::NONBLOCK)?;
        let (_, write) = pty.into_split();
        let tty = Tty::new(write, Scheme::Light, 24, 80);
        let viewer = Mutex::new(Viewer {
            path: PathBuf::new(),
            activity: ViewerActivity::default(),
            dirty: false,
            draft: input::Draft::default(),
        });
        let (mut server, mut client) = UnixStream::pair()?;
        let mut nudge = std::pin::pin!(wake_once(
            &mut server,
            &viewer,
            &tty,
            Duration::ZERO,
            observed_at
        ));
        let cursor = Mutex::new(input::CursorQueries::default());
        let mut classify = input::Input::default();
        let keys = forward_keys(&tty, &viewer, &cursor, &mut classify, b"draft");

        // Poll until the line has been written and its Enter delay reached,
        // with the viewer input still ready to arrive before Enter.
        let mut prefix = [0; WAKE_LINE.len()];
        let mut read = 0;
        while read < prefix.len() {
            assert!(futures::poll!(nudge.as_mut()).is_pending());
            match slave.read(&mut prefix[read..]) {
                Ok(0) => anyhow::bail!("PTY closed before the wake line arrived"),
                Ok(n) => read += n,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(error.into()),
            }
        }
        assert_eq!(prefix, WAKE_LINE.as_bytes());
        let ((), keys) = tokio::join!(nudge, keys);
        keys?;
        assert_eq!(client.read_u8().await?, NUDGED);
        let mut tail = [0; 6];
        slave.read_exact(&mut tail)?;
        assert_eq!(&tail, b"\rdraft");
        assert!(Viewer::lock(&viewer).draft.pending());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn answering_a_prompt_choice_does_not_leave_a_false_draft() -> anyhow::Result<()> {
        let (pty, pts) = pty_process::open()?;
        let mut slave = std::fs::File::from(rustix::io::dup(&pts)?);
        let flags = rustix::fs::fcntl_getfl(&slave)?;
        rustix::fs::fcntl_setfl(&slave, flags | rustix::fs::OFlags::NONBLOCK)?;
        let (_, write) = pty.into_split();
        let tty = Tty::new(write, Scheme::Light, 24, 80);
        let viewer = Mutex::new(Viewer {
            path: PathBuf::new(),
            activity: ViewerActivity::default(),
            dirty: false,
            draft: input::Draft::default(),
        });
        let cursor = Mutex::new(input::CursorQueries::default());
        let mut classify = input::Input::default();
        let (mut server, mut client) = UnixStream::pair()?;
        let now = 100_000_000_000;

        // A choice menu is on screen; answering it with `1` must not set a draft.
        tty.screen
            .output(b"Allow this command?\r\n\xe2\x9d\xaf 1. Yes\r\n  2. No\r\n");
        forward_keys(&tty, &viewer, &cursor, &mut classify, b"1").await?;
        assert!(
            !Viewer::lock(&viewer).draft.pending(),
            "answering an on-screen choice must not leave a stale draft"
        );

        // Once the modal clears and the quiet window passes, the wake succeeds.
        tty.screen.output(b"\x1b[2J\x1b[H> ");
        tty.screen.set_last_output(now - 60_000_000_000);
        Viewer::lock(&viewer).activity.input_at = now - 60_000_000_000;
        wake_once(&mut server, &viewer, &tty, Duration::from_secs(10), || now).await;
        assert_eq!(reply(&mut client).await?, (NUDGED, None));
        let mut buf = [0; 64];
        assert!(slave.read(&mut buf).unwrap_or(0) > 0);
        Ok(())
    }
}
