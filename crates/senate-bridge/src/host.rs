//! A detached terminal owner. The daemon records the session's identity, but
//! only this process holds the master PTY and its child.

pub(crate) mod control;
mod input;
mod terminal;

use rustix::process::{Pid, Signal, kill_process_group, test_kill_process_group};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use pty_process::{OwnedWritePty, Size};
use rustix::fs::{FlockOperation, flock};
use senate_core::EventId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::broadcast;

use crate::host_approval::observed_at;
use crate::ipc::{self, HostedRuntime, Request};
use crate::paths::{HOME_ENV, Paths, SOCKET_PATH_LIMIT};

pub use terminal::attach;

pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const IO_CHUNK: usize = 8192;
const ATTACHED: u8 = 1;
const BUSY: u8 = 0;
const RESIZE: u8 = 1;
const KEYS: u8 = 2;

pub(crate) fn directory(paths: &Paths, id: &EventId) -> PathBuf {
    hosts(paths).join(id.to_string())
}

fn hosts(paths: &Paths) -> PathBuf {
    paths.home().join("hosts")
}

pub(crate) fn socket(paths: &Paths, id: &EventId) -> PathBuf {
    directory(paths, id).join("tty")
}

/// Host-owned facts that outlive a bridge restart. Raw terminal bytes
/// never enter this record.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ViewerActivity {
    pub attached: bool,
    pub input_at: u64,
    pub detached_at: u64,
}

// An attached terminal only stands in for a person who has used it in
// the last three minutes. Leaving an attachment open cannot hide a prompt.
const VIEWER_IDLE_NS: u64 = 3 * 60 * 1_000_000_000;

impl ViewerActivity {
    /// When this viewer stopped counting as attended, if it has.
    pub(crate) fn unattended_since(&self, now: u64) -> Option<u64> {
        if self.attached && self.input_at > self.detached_at {
            let idle_at = self.input_at.saturating_add(VIEWER_IDLE_NS);
            return (now >= idle_at).then_some(idle_at);
        }
        Some(self.detached_at)
    }
}

pub(crate) fn viewer_activity(paths: &Paths, id: &EventId) -> Option<ViewerActivity> {
    let data = std::fs::read(directory(paths, id).join("viewer.json")).ok()?;
    serde_json::from_slice(&data).ok()
}

pub(crate) fn approval_candidate(paths: &Paths, id: &EventId) -> PathBuf {
    directory(paths, id).join("approval-candidate")
}

struct Viewer {
    path: PathBuf,
    activity: ViewerActivity,
    dirty: bool,
}

impl Viewer {
    fn lock(viewer: &Mutex<Self>) -> MutexGuard<'_, Self> {
        viewer.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn record(&mut self) {
        self.dirty = false;
        let saved = (|| -> anyhow::Result<()> {
            let staged = self.path.with_extension("tmp");
            std::fs::write(&staged, serde_json::to_vec(&self.activity)?)?;
            std::fs::rename(staged, &self.path)?;
            Ok(())
        })();
        if let Err(error) = saved {
            let _ = std::fs::remove_file(&self.path);
            tracing::warn!(%error, path = %self.path.display(), "recording activity of the terminal viewer");
        }
    }

    fn input(&mut self, at: u64) {
        if at > self.activity.input_at {
            self.activity.input_at = at;
            self.dirty = true;
        }
    }
}

fn open_lock(dir: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join("owner"))
}

/// The operating system releases this lock even if the host crashes.
pub(crate) fn running(paths: &Paths, id: &EventId) -> io::Result<bool> {
    let file = match File::open(directory(paths, id).join("owner")) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match flock(&file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(e) => Err(e.into()),
    }
}

/// A host can outlive a daemon that stops during a launch.
/// Its lock still occupies a slot while it waits for registration.
pub(crate) fn running_ids(paths: &Paths) -> io::Result<Vec<EventId>> {
    let entries = match std::fs::read_dir(hosts(paths)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error),
    };
    let mut live = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.parse().ok())
            && running(paths, &id)?
        {
            live.push(id);
        }
    }
    Ok(live)
}

/// Remove abandoned, old launch directories. A live lock or a recent
/// directory can belong to a launch at any point in its startup.
pub(crate) fn prune_abandoned(paths: &Paths, age: Duration) -> io::Result<()> {
    let entries = match std::fs::read_dir(hosts(paths)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let known_id = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.parse::<EventId>().is_ok());
        if !known_id || !entry.file_type()?.is_dir() {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|elapsed| elapsed >= age);
        if !old {
            continue;
        }
        let path = entry.path();
        let _lock = match File::open(path.join("owner")) {
            Ok(file) => match flock(&file, FlockOperation::NonBlockingLockExclusive) {
                Ok(()) => Some(file),
                Err(rustix::io::Errno::WOULDBLOCK) => continue,
                Err(error) => return Err(error.into()),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        match std::fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// The daemon selects this native identity before the TUI is started.
pub struct SessionCommand<'a> {
    pub session_id: &'a str,
    pub program: &'a Path,
    pub resume: bool,
    pub codex_remote: Option<&'a str>,
}

pub(crate) const READY: u8 = 1;
pub(crate) const PUBLISHED: u8 = 1;
pub const HOSTED_CLAUDE_ENV: &str = "SENATE_HOSTED_CLAUDE";

pub(crate) fn preflight(paths: &Paths, id: &EventId, program: &Path) -> anyhow::Result<PathBuf> {
    let dir = directory(paths, id);
    let socket = socket(paths, id);
    ensure!(
        crate::testenv::confined(&socket),
        "terminal host socket is outside the test directory"
    );
    ensure!(
        socket.as_os_str().len() <= SOCKET_PATH_LIMIT,
        "host socket path is too long: {}; use a shorter {HOME_ENV}",
        socket.display()
    );
    let parent = dir.parent().context("missing host directory parent")?;
    match std::fs::DirBuilder::new().mode(0o700).create(parent) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(parent)?;
            ensure!(
                metadata.is_dir() && metadata.permissions().mode() & 0o777 == 0o700,
                "{} must be a private directory (0700)",
                parent.display()
            );
        }
        Err(e) => return Err(e.into()),
    }

    let executable = |path: &Path| {
        path.is_file() && rustix::fs::access(path, rustix::fs::Access::EXEC_OK).is_ok()
    };
    let found =
        if program.components().count() == 1 && program.file_name() == Some(program.as_os_str()) {
            std::env::var_os("PATH")
                .iter()
                .flat_map(std::env::split_paths)
                .map(|path| path.join(program))
                .find(|path| executable(path))
        } else {
            executable(program).then(|| program.to_owned())
        };
    // Make the path absolute while retaining symlinks, since executable
    // shims can dispatch on the name under which they were invoked.
    std::path::absolute(
        found.with_context(|| format!("coding TUI executable not found: {}", program.display()))?,
    )
    .context("resolve the coding TUI executable")
}

pub(crate) fn start(
    host_binary: &Path,
    paths: &Paths,
    id: &EventId,
    runtime: HostedRuntime,
    cwd: &Path,
    session: &SessionCommand<'_>,
) -> anyhow::Result<(Child, File)> {
    let dir = directory(paths, id);
    let mut command = Command::new(host_binary);
    command
        .arg("session-host")
        .arg(id.to_string())
        .arg(crate::session::Runtime::from(runtime).as_str())
        .arg(cwd)
        .arg(session.session_id)
        .arg("--program")
        .arg(session.program)
        .env(HOME_ENV, paths.home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    if session.resume {
        command.arg("--resume");
    }
    if let Some(remote) = session.codex_remote {
        command.arg("--codex-remote").arg(remote);
    }

    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let mut spawn = || -> anyhow::Result<(Child, File)> {
        File::create(dir.join("unpublished"))?;
        let path = dir.join("errors");
        let log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        let errors = File::open(path)?;
        let child = command
            .stderr(log)
            .spawn()
            .context("start Senate terminal host")?;
        Ok((child, errors))
    };
    match spawn() {
        Ok(child) => Ok(child),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

struct RemoveDirectory(PathBuf);

impl Drop for RemoveDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub async fn run(
    paths: &Paths,
    id: &EventId,
    runtime: HostedRuntime,
    cwd: &Path,
    session: &SessionCommand<'_>,
) -> anyhow::Result<()> {
    let mut publication = tokio::net::unix::pipe::Receiver::from_owned_fd(
        rustix::io::fcntl_dupfd_cloexec(std::io::stdin(), 3)?,
    )
    .context("open the daemon publication pipe")?;
    let dir = directory(paths, id);
    let file = open_lock(&dir).context("own the host directory")?;
    // A liveness reader may have opened the file just before we locked it.
    flock(&file, FlockOperation::LockExclusive)?;
    let cleanup = RemoveDirectory(dir.clone());
    rustix::process::setsid().context("detach the Senate terminal host")?;
    let mut terminate = signal(SignalKind::terminate())?;
    let socket = socket(paths, id);
    let listener = UnixListener::bind(&socket).context("listen for terminal attachments")?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    // The control socket has the same path length as the checked tty socket.
    let controls = control::listen(paths, id).context("listen for terminal stop requests")?;
    let viewer = Mutex::new(Viewer {
        path: dir.join("viewer.json"),
        activity: ViewerActivity::default(),
        dirty: false,
    });
    Viewer::lock(&viewer).record();
    let (pty, pts) = pty_process::open()?;
    pty.resize(Size::new(24, 80))?;
    let mut child = runtime_command(runtime, cwd, session, id, &dir)?
        .spawn(pts)
        .context("start coding TUI on its PTY")?;

    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw)
        .context("the coding TUI has no process ID")?;
    let mut startup_pid = file.try_clone()?;
    writeln!(startup_pid, "{pid}")?;
    drop(startup_pid);
    let (mut pty_read, mut pty_write) = pty.into_split();
    // A slow or disconnected viewer must never stop the PTY being drained.
    let (output, _) = broadcast::channel(64);
    let mut startup_output = output.subscribe();
    let feed = output.clone();
    let pump = tokio::spawn(async move {
        let mut buf = [0; IO_CHUNK];
        while let Ok(n @ 1..) = pty_read.read(&mut buf).await {
            let _ = feed.send(buf[..n].to_vec());
        }
    });

    // Catch early argument/bootstrap exits without storing terminal data on disk.
    tokio::select! {
        biased;
        status = child.wait() => {
            let mut detail = tokio::time::timeout(Duration::from_millis(50), startup_output.recv())
                .await.ok().and_then(Result::ok).unwrap_or_default();
            detail.truncate(4096);
            while let Ok(bytes) = startup_output.try_recv() {
                let open = 4096_usize.saturating_sub(detail.len());
                detail.extend_from_slice(&bytes[..bytes.len().min(open)]);
            }
            let detail: String = String::from_utf8_lossy(&detail).chars()
                .filter(|ch| !ch.is_control() || *ch == '\n').collect();
            bail!("the coding TUI exited during startup ({}): {}", status?, detail.trim());
        }
        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        _ = terminate.recv() => return stop_group(&mut child, pid).await,
    }
    drop(startup_output);
    if let Some(status) = child.try_wait().context("check coding TUI startup")? {
        bail!("the coding TUI exited during startup ({status})");
    }

    let ready = {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&[READY]).and_then(|()| stdout.flush())
    };
    if let Err(error) = ready {
        stop_group(&mut child, pid).await?;
        return Err(error).context("report host readiness");
    }
    let published = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(20), publication.read_u8()) =>
            matches!(result, Ok(Ok(PUBLISHED))),
        status = child.wait() => bail!("the coding TUI exited before publication ({})", status?),
        _ = terminate.recv() => false,
    };
    if !published {
        stop_group(&mut child, pid).await?;
        bail!("the daemon did not acknowledge publication");
    }
    // This is a host-local record for a restarted daemon. Failure to remove
    // it is diagnostic; the daemon's publication byte is the authority.
    if let Err(error) = std::fs::remove_file(dir.join("unpublished")) {
        tracing::warn!(%error, "clearing the host's startup marker");
    }

    let mut stop = None;
    let result = tokio::select! {
        biased;
        status = child.wait() => status.context("wait for the coding TUI").and_then(|status| {
            ensure!(status.success(), "the coding TUI exited with {status}");
            Ok(())
        }),
        _ = terminate.recv() => stop_group(&mut child, pid).await,
        served = serve(&listener, &mut pty_write, &output, &viewer) => served,
        peer = control::next_stop(&controls, &viewer) => {
            stop = Some(peer);
            stop_group(&mut child, pid).await
        }
    };
    pump.abort();
    // The runtime's normal SessionEnd hook usually gets there first. A
    // crash or stop without that hook must also retire this session.
    let end = Request::HostedSessionEnd {
        session_id: session.session_id.to_owned(),
        host_id: id.clone(),
    };
    let mut recorded = false;
    for _ in 0..if stop.is_some() { 2 } else { 1 } {
        match ipc::request::<serde_json::Value>(
            &paths.socket(),
            &end,
            Duration::from_millis(300),
            Duration::from_secs(2),
        )
        .await
        {
            Ok(_) => {
                recorded = true;
                break;
            }
            Err(error) => {
                tracing::debug!(%error, %id, "notifying the daemon that a terminal host ended")
            }
        }
    }
    if let Some(peer) = stop {
        // Returning from stop means the owner lock and capacity are free.
        drop(cleanup);
        drop(file);
        control::acknowledge(peer, result.is_ok(), recorded).await;
    }
    result
}

fn runtime_command(
    runtime: HostedRuntime,
    cwd: &Path,
    session: &SessionCommand<'_>,
    id: &EventId,
    dir: &Path,
) -> anyhow::Result<pty_process::Command> {
    let command = match runtime {
        HostedRuntime::Codex => pty_process::Command::new(session.program)
            .args([
                "resume",
                "--remote",
                session
                    .codex_remote
                    .context("missing Codex app-server address")?,
            ])
            .arg("--cd")
            .arg(cwd)
            .arg("--")
            .arg(session.session_id),
        HostedRuntime::Claude => {
            // Claude merges this overlay with its other settings. Only a
            // Senate-hosted Claude session runs these observer hooks.
            let settings = dir.join("approval-hooks.json");
            let exe = std::env::current_exe()?;
            let hook = |matcher: &str| {
                serde_json::json!([{
                    "matcher": matcher, "hooks": [{
                        "type": "command", "timeout": 4,
                    "command": format!("{} >/dev/null 2>&1 || :", crate::install::hook_command(&exe.to_string_lossy(), "hosted-approval"))
                    }]
                }])
            };
            let overlay = serde_json::json!({"hooks": {
                "PermissionRequest": hook("*"),
                "Notification": hook("permission_prompt"),
                "PostToolUse": hook("*"),
                "PostToolUseFailure": hook("*"),
            }});
            std::fs::write(&settings, serde_json::to_vec(&overlay)?)?;
            let flag = if session.resume {
                "--resume"
            } else {
                "--session-id"
            };
            pty_process::Command::new(session.program)
                .arg(format!("{flag}={}", session.session_id))
                .arg("--settings")
                .arg(settings)
        }
    };
    let command = if runtime == HostedRuntime::Claude {
        command.env(HOSTED_CLAUDE_ENV, id.to_string())
    } else {
        command.env_remove(HOSTED_CLAUDE_ENV)
    };
    let command = command
        .current_dir(cwd)
        .env("TERM", "xterm-256color")
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
        .env_remove("CLAUDE_CODE_MESSAGING_TOKEN")
        .env_remove("CLAUDE_PID")
        .env_remove("CODEX_THREAD_ID")
        .env_remove(crate::agent::AGENT_ENV)
        .env_remove(crate::delivery::agy::PROOF_ENV)
        .env_remove(crate::companion::ENV_COMPANION)
        .env_remove(crate::companion::ENV_SESSION)
        .kill_on_drop(true);
    Ok(command)
}

async fn stop_group(child: &mut Child, pid: Pid) -> anyhow::Result<()> {
    fn send(pid: Pid, signal: Signal) -> rustix::io::Result<()> {
        match kill_process_group(pid, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error),
        }
    }
    // pty-process creates the TUI's own session and process group.
    // Keep the leader unreaped until both signals have gone to that group.
    let signals = async {
        send(pid, Signal::TERM)?;
        tokio::time::sleep(Duration::from_millis(350)).await;
        send(pid, Signal::KILL)
    }
    .await;
    if let Err(error) = signals {
        // macOS can return EPERM for a group containing only the unreaped
        // leader. Reap it and confirm the group is gone before accepting it.
        if error != rustix::io::Errno::PERM
            || child.try_wait()?.is_none()
            || test_kill_process_group(pid) != Err(rustix::io::Errno::SRCH)
        {
            return Err(error).context("signal the coding TUI group");
        }
    }
    child.wait().await.context("stop the coding TUI group")?;
    Ok(())
}

pub(crate) fn mark_shared_codex_thread(paths: &Paths, id: &EventId) -> io::Result<()> {
    File::create(directory(paths, id).join("shared-codex")).map(drop)
}

pub(crate) fn clear_shared_codex_thread(paths: &Paths, id: &EventId) -> io::Result<()> {
    match std::fs::remove_file(directory(paths, id).join("shared-codex")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

pub(crate) fn shares_codex_thread(paths: &Paths, id: &EventId) -> bool {
    directory(paths, id).join("shared-codex").exists()
}

pub(crate) fn awaiting_publication(paths: &Paths, id: &EventId) -> bool {
    directory(paths, id).join("unpublished").exists()
}

pub(crate) fn unverified_switch(paths: &Paths, id: &EventId) -> Option<String> {
    std::fs::read_to_string(directory(paths, id).join("unverified-switch")).ok()
}

pub(crate) fn unbound_context(paths: &Paths, id: &EventId) -> Option<String> {
    std::fs::read_to_string(directory(paths, id).join("unbound-context")).ok()
}

#[derive(Default)]
pub(crate) struct HostStatus {
    pub(crate) hosted: BTreeMap<String, Option<String>>,
    pub(crate) unbound: BTreeMap<String, String>,
    pub(crate) viewer_unavailable: BTreeSet<String>,
}

pub(crate) fn status(
    paths: &Paths,
    store: &crate::state::Store,
    sessions: &[crate::session::Session],
) -> Result<HostStatus, crate::state::StoreError> {
    let mut status = HostStatus::default();
    for session in sessions {
        if let Some(id) = store.session_host(&session.session_id)?
            && running(paths, &id)?
        {
            status.hosted.insert(
                session.session_id.clone(),
                unverified_switch(paths, &id).filter(|new| new != &session.session_id),
            );
            if let Some(native) = unbound_context(paths, &id) {
                status.unbound.insert(session.session_id.clone(), native);
            }
            if !awaiting_publication(paths, &id) && viewer_activity(paths, &id).is_none() {
                status.viewer_unavailable.insert(session.session_id.clone());
            }
        }
    }
    Ok(status)
}

pub(crate) fn note_unverified_switch(paths: &Paths, id: &EventId, session: &str) -> io::Result<()> {
    std::fs::write(directory(paths, id).join("unverified-switch"), session)
}

pub(crate) fn note_unbound_context(paths: &Paths, id: &EventId, session: &str) -> io::Result<()> {
    std::fs::write(directory(paths, id).join("unbound-context"), session)
}

pub(crate) fn clear_context_reports(paths: &Paths, id: &EventId) {
    let dir = directory(paths, id);
    for file in ["unverified-switch", "unbound-context"] {
        if let Err(error) = std::fs::remove_file(dir.join(file))
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(%error, %id, file, "clearing a hosted Claude context report");
        }
    }
}

pub(crate) fn child_pid(paths: &Paths, id: &EventId) -> Option<u32> {
    std::fs::read_to_string(directory(paths, id).join("owner"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

async fn accept(listener: &UnixListener) -> (UnixStream, tokio::net::unix::SocketAddr) {
    loop {
        match listener.accept().await {
            Ok(connection) => return connection,
            Err(error) => {
                tracing::warn!(%error, "accepting a terminal host connection");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve(
    listener: &UnixListener,
    pty: &mut OwnedWritePty,
    output: &broadcast::Sender<Vec<u8>>,
    viewer: &Mutex<Viewer>,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = accept(listener).await;
        {
            let mut viewer = Viewer::lock(viewer);
            viewer.activity.attached = true;
            viewer.record();
        }
        {
            let relay = relay(stream, pty, output.subscribe(), viewer);
            tokio::pin!(relay);
            loop {
                tokio::select! {
                    biased;
                    result = &mut relay => {
                        if let Err(error) = result {
                            tracing::debug!(%error, "terminal attachment ended");
                        }
                        break;
                    }
                    (other, _) = accept(listener) => {
                        let _ = other.try_write(&[BUSY]);
                    }
                }
            }
        }
        let mut viewer = Viewer::lock(viewer);
        viewer.activity.attached = false;
        viewer.activity.detached_at = observed_at();
        viewer.record();
    }
}

async fn relay(
    mut stream: UnixStream,
    pty: &mut OwnedWritePty,
    mut output: broadcast::Receiver<Vec<u8>>,
    viewer: &Mutex<Viewer>,
) -> anyhow::Result<()> {
    stream.write_u8(ATTACHED).await?;
    let (mut input, mut screen) = stream.into_split();
    let mut classify = input::Input::default();
    let cursor = std::sync::Mutex::new(input::CursorQueries::default());
    let from_terminal = async {
        let mut first_resize = true;
        let mut keys = [0; IO_CHUNK];
        let mut writes = tokio::time::interval(Duration::from_millis(100));
        writes.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let deadline = classify.deadline();
            let dirty = Viewer::lock(viewer).dirty;
            let mut tag = [0];
            let len = tokio::select! {
                biased;
                _ = writes.tick(), if dirty => {
                    Viewer::lock(viewer).record();
                    continue;
                }
                _ = tokio::time::sleep(Duration::from_nanos(deadline.unwrap_or_default().saturating_sub(observed_at()))), if deadline.is_some() => {
                    if let Some(at) = classify.expire(observed_at()) {
                        Viewer::lock(viewer).input(at);
                    }
                    continue;
                }
                len = input.read(&mut tag) => len?,
            };
            if len == 0 {
                return Ok(());
            }
            match tag[0] {
                RESIZE => {
                    let rows = input.read_u16().await?;
                    let cols = input.read_u16().await?;
                    ensure!(rows > 0 && cols > 0, "invalid attachment dimensions");
                    if first_resize {
                        // A same-size ioctl does not wake the TUI. Change
                        // the size briefly so returning to it is a resize.
                        pty.resize(Size::new(rows.saturating_add(1), cols))?;
                        tokio::time::sleep(Duration::from_millis(75)).await;
                        first_resize = false;
                    }
                    pty.resize(Size::new(rows, cols))?;
                }
                KEYS => {
                    let n = usize::from(input.read_u16().await?);
                    ensure!(n <= keys.len(), "oversized terminal input");
                    input.read_exact(&mut keys[..n]).await?;
                    if n != 0 {
                        // Timestamp before forwarding: these keys can cause
                        // the next prompt, whose episode they must not clear.
                        let at = observed_at();
                        {
                            let mut queries =
                                cursor.lock().unwrap_or_else(|error| error.into_inner());
                            if let Some(human_at) = classify.observe(&keys[..n], at, &mut queries) {
                                Viewer::lock(viewer).input(human_at);
                            }
                        }
                        pty.write_all(&keys[..n]).await?;
                    }
                }
                _ => bail!("unknown terminal input frame"),
            }
        }
    };
    let to_terminal = async {
        loop {
            // If a viewer falls behind, an incomplete escape stream cannot
            // safely be replayed. Drop the connection and let it reattach.
            let bytes = output.recv().await?;
            cursor
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .output(&bytes, observed_at());
            screen.write_all(&bytes).await?;
        }
    };
    let result = tokio::select! {
        biased;
        result = from_terminal => result,
        result = to_terminal => result,
    };
    if let Some(at) = classify.finish() {
        Viewer::lock(viewer).input(at);
    }
    // The enclosing attachment records its detach and the final input in
    // one snapshot. While attached, input snapshots are coalesced.
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_viewer_can_answer_a_tui_query_without_reporting_human_input() {
        let (pty, _pts) = pty_process::open().unwrap();
        let (_read, mut pty) = pty.into_split();
        let (output, _) = broadcast::channel(1);
        let dir = tempfile::tempdir().unwrap();
        let activity = Mutex::new(Viewer {
            path: dir.path().join("viewer.json"),
            activity: ViewerActivity::default(),
            dirty: false,
        });
        let (host, mut viewer) = UnixStream::pair().unwrap();
        let view_only = async {
            assert_eq!(viewer.read_u8().await.unwrap(), ATTACHED);
            output.send(b"\x1b[6n".to_vec()).unwrap();
            let mut query = [0; 4];
            viewer.read_exact(&mut query).await.unwrap();
            assert_eq!(&query, b"\x1b[6n");
            viewer
                // The emulator's cursor position report, split into frames.
                .write_all(b"\x01\0\x18\0P\x02\0\x02\x1b[\x02\0\x042;34")
                .await
                .unwrap();
            viewer.write_all(&[KEYS, 0, 1, b'R']).await.unwrap();
            viewer.shutdown().await.unwrap();
        };
        let _ = tokio::join!(
            relay(host, &mut pty, output.subscribe(), &activity),
            view_only
        );
        assert_eq!(Viewer::lock(&activity).activity.input_at, 0);

        let (host, mut viewer) = UnixStream::pair().unwrap();
        let with_input = async {
            assert_eq!(viewer.read_u8().await.unwrap(), ATTACHED);
            viewer.write_all(&[KEYS, 0, 1, b'x']).await.unwrap();
            viewer.shutdown().await.unwrap();
        };
        let _ = tokio::join!(
            relay(host, &mut pty, output.subscribe(), &activity),
            with_input
        );
        assert_ne!(Viewer::lock(&activity).activity.input_at, 0);
    }

    #[test]
    fn an_attached_viewer_counts_as_attended_only_during_recent_human_activity() {
        let mut viewer = ViewerActivity {
            attached: true,
            detached_at: 10,
            input_at: 9,
        };
        assert_eq!(
            viewer.unattended_since(11),
            Some(10),
            "input from a previous attachment"
        );
        viewer.input_at = 12;
        assert_eq!(viewer.unattended_since(12 + VIEWER_IDLE_NS - 1), None);
        assert_eq!(
            viewer.unattended_since(12 + VIEWER_IDLE_NS),
            Some(12 + VIEWER_IDLE_NS)
        );
        assert_eq!(
            viewer.unattended_since(12 + VIEWER_IDLE_NS + 1),
            Some(12 + VIEWER_IDLE_NS)
        );
    }

    #[test]
    fn an_unregistered_owner_lock_counts_until_the_host_exits() {
        let home = tempfile::tempdir().unwrap();
        let paths = Paths::new(home.path());
        assert!(running_ids(&paths).unwrap().is_empty());

        let id = EventId::new();
        let dir = directory(&paths, &id);
        std::fs::create_dir_all(&dir).unwrap();
        let owner = open_lock(&dir).unwrap();
        flock(&owner, FlockOperation::NonBlockingLockExclusive).unwrap();

        let dead = directory(&paths, &EventId::new());
        std::fs::create_dir_all(&dead).unwrap();
        drop(open_lock(&dead).unwrap());

        assert_eq!(running_ids(&paths).unwrap(), std::slice::from_ref(&id));
        prune_abandoned(&paths, Duration::ZERO).unwrap();
        assert!(dir.exists());
        assert!(!dead.exists());
        // Other tests can be forking while we run. Unlock explicitly so
        // a descriptor inherited briefly before exec cannot delay release.
        flock(&owner, FlockOperation::Unlock).unwrap();
        flock(&owner, FlockOperation::LockShared).unwrap();
        assert!(
            !running(&paths, &id).unwrap(),
            "concurrent readers are not a live owner"
        );
        flock(&owner, FlockOperation::Unlock).unwrap();
        assert!(running_ids(&paths).unwrap().is_empty());
    }
}
