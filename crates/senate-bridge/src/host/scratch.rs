//! A scratch terminal: a program on a PTY of its own, for terminal use.
//!
//! An agent (or a person) opens one from its own shell with `senate
//! terminal open -- <command>` and then drives it through the daemon:
//! reads its screen, types, presses keys, waits for it. The host is the
//! same process as a hosted session's ([`super::run`]) with the session
//! taken out: the same directory under `hosts/`, the same `tty`, `see` and
//! `ctl` sockets, the same emulator and screen. What it does not have is
//! everything that makes a hosted session a senator: no runtime, no hooks,
//! no prompt watcher, no session record.
//!
//! # Whose authority
//!
//! The process that runs `senate terminal open` starts the host itself
//! ([`start`]), so the program inherits that shell command's environment,
//! working directory, sandbox and process scope. The daemon never starts
//! a program for anyone: it only reserves the id ([`reserve`]), records who
//! opened it ([`Record`]) and checks that record on every later request.
//! The host detaches into its own session (`setsid`), so it outlives the
//! command that started it and is not ended with that command's process
//! group; a sandbox the command ran in is inherited and stays in force.
//!
//! # Input
//!
//! Only the daemon may send input, on `ctl` ([`Control`]), and only a
//! scratch host serves that request: a hosted session's host has no such
//! request at all ([`super::control`]). While a person is attached on
//! `tty` the keyboard is theirs, and the daemon's input is refused
//! ([`Sent::Held`]).
//!
//! # After the program exits
//!
//! The host stays, with the last screen, until it is closed: what an
//! installer or a failing test printed last is usually what the reader
//! wants. How the program ended is in the `exit` file.

use crate::fsmode::{DirBuilderExt, OpenOptionsExt};
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::local::{Listener as UnixListener, Stream as UnixStream};
use anyhow::{Context, ensure};
#[cfg(unix)]
use pty_process::Size;
#[cfg(unix)]
use rustix::fs::{FlockOperation, flock};
#[cfg(unix)]
use rustix::process::Pid;
use senate_core::EventId;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _, BufReader};
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal};

use super::{
    Admission, DaemonIdentity, HOST_PID, HOSTED_AGY_ENV, HOSTED_CLAUDE_ENV, IO_CHUNK, READY,
    RemoveDirectory, START_TIMEOUT, ViewTicket, Viewer, ViewerActivity, accept, control, directory,
    emulation, hosts, input, keys, open_lock, running, socket, vanished, watch, write_protocol,
};
#[cfg(unix)]
use super::{Tty, serve, stop_group};
use crate::ipc::{self, IpcError};
use crate::paths::{HOME_ENV, Paths, SOCKET_PATH_LIMIT};

/// The file in a host's directory that makes it a scratch terminal.
const RECORD: &str = "scratch.json";
/// How the program ended, once it has: `code 0`, `signal 9`.
const EXIT: &str = "exit";
/// The largest side a scratch terminal may have.
pub const MAX_ROWS: u16 = 200;
pub const MAX_COLS: u16 = 500;
/// The most text one `type` sends.
pub const MAX_TEXT: usize = 16 * 1024;

const DONE: u8 = 1;
const HELD: u8 = 2;
const EXITED: u8 = 3;
const REFUSED: u8 = 4;
const HANDSHAKE: Duration = Duration::from_secs(2);
/// How long the program has to take what was typed.
const WRITE_LIMIT: Duration = Duration::from_secs(3);

/// Who opened a scratch terminal and what runs in it, as the daemon
/// recorded it when it reserved the id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Record {
    /// The session that opened it; `None` when a person did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub command: Vec<String>,
    pub cwd: PathBuf,
    pub opened_at: chrono::DateTime<chrono::Utc>,
}

/// Where a scratch terminal is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    /// Reserved; its host has not taken the directory yet.
    Starting,
    Running,
    /// The program ended this way; the host keeps its last screen.
    Exited(String),
    /// The host is gone and left its directory behind.
    Ended,
}

/// Reserve `id` for a scratch terminal: its directory, with `record`. The
/// daemon's doing; the opener then starts the host in it ([`start`]).
#[cfg(windows)]
pub(crate) fn reserve(_paths: &Paths, _id: &EventId, _record: &Record) -> anyhow::Result<()> {
    anyhow::bail!("scratch terminals are not supported on Windows yet")
}

#[cfg(unix)]
pub(crate) fn reserve(paths: &Paths, id: &EventId, record: &Record) -> anyhow::Result<()> {
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
    let parent = hosts(paths);
    match std::fs::DirBuilder::new().mode(0o700).create(&parent) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(&parent)?;
            ensure!(
                metadata.is_dir() && metadata.permissions().mode() & 0o777 == 0o700,
                "{} must be a private directory (0700)",
                parent.display()
            );
        }
        Err(error) => return Err(error.into()),
    }
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let written = (|| -> anyhow::Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&dir)?;
        serde_json::to_writer(&mut file, record)?;
        file.persist(dir.join(RECORD))?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    written
}

/// The record of scratch terminal `id`; `None` for any other host.
pub(crate) fn record(paths: &Paths, id: &EventId) -> Option<Record> {
    serde_json::from_slice(&std::fs::read(directory(paths, id).join(RECORD)).ok()?).ok()
}

/// Whether host `id` is a scratch terminal.
pub(crate) fn is_scratch(paths: &Paths, id: &EventId) -> bool {
    directory(paths, id).join(RECORD).exists()
}

/// Every scratch terminal under `paths`' home, oldest first.
pub(crate) fn all(paths: &Paths) -> io::Result<Vec<(EventId, Record)>> {
    let entries = match std::fs::read_dir(hosts(paths)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error),
    };
    let mut found = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if let Some(id) = name.to_str().and_then(|name| name.parse::<EventId>().ok())
            && let Some(record) = record(paths, &id)
        {
            found.push((id, record));
        }
    }
    found.sort_by(|a, b| a.1.opened_at.cmp(&b.1.opened_at).then(a.0.cmp(&b.0)));
    Ok(found)
}

/// How the program in `id` ended, if it has.
pub(crate) fn exit_status(paths: &Paths, id: &EventId) -> Option<String> {
    std::fs::read_to_string(directory(paths, id).join(EXIT))
        .ok()
        .map(|status| status.trim().to_owned())
}

pub(crate) fn state(paths: &Paths, id: &EventId) -> io::Result<State> {
    if running(paths, id)? {
        return Ok(match exit_status(paths, id) {
            Some(status) => State::Exited(status),
            None => State::Running,
        });
    }
    Ok(if directory(paths, id).join("owner").exists() {
        State::Ended
    } else {
        State::Starting
    })
}

/// Remove what is left of scratch terminal `id` once its host is gone:
/// one that was killed, or one that never started.
pub(crate) fn remove(paths: &Paths, id: &EventId) -> io::Result<()> {
    if !is_scratch(paths, id) || running(paths, id)? {
        return Ok(());
    }
    match std::fs::remove_dir_all(directory(paths, id)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// What runs in a scratch terminal.
pub struct Program<'a> {
    /// The program and its arguments.
    pub argv: &'a [OsString],
    pub cwd: &'a Path,
    pub rows: u16,
    pub cols: u16,
}

/// Start the host of the reserved scratch terminal `id` as a child of this
/// process, and wait until its program runs. This process is the opener:
/// what it may do is what the program may do.
#[cfg(windows)]
pub async fn start(_paths: &Paths, _id: &EventId, _program: &Program<'_>) -> anyhow::Result<()> {
    anyhow::bail!("scratch terminals are not supported on Windows yet")
}

#[cfg(unix)]
pub async fn start(paths: &Paths, id: &EventId, program: &Program<'_>) -> anyhow::Result<()> {
    let dir = directory(paths, id);
    let errors = dir.join("errors");
    let log = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&errors)
        .context("write in the terminal's directory")?;
    // Opened now: a host that fails takes its directory with it.
    let mut said = std::fs::File::open(&errors)?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("terminal-host")
        .arg(id.to_string())
        .arg("--rows")
        .arg(program.rows.to_string())
        .arg("--cols")
        .arg(program.cols.to_string())
        .arg("--cwd")
        .arg(program.cwd)
        .arg("--")
        .args(program.argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .context("start the terminal host")?;
    let mut stdout = child
        .stdout
        .take()
        .context("the terminal host has no output")?;
    if matches!(
        tokio::time::timeout(START_TIMEOUT, stdout.read_u8()).await,
        Ok(Ok(READY))
    ) {
        return Ok(());
    }
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
    let mut detail = String::new();
    let _ = std::io::Read::read_to_string(&mut said, &mut detail);
    let detail = detail.trim().trim_start_matches("senate: ");
    if detail.is_empty() {
        anyhow::bail!("the terminal did not start");
    }
    anyhow::bail!("the terminal did not start: {detail}")
}

#[cfg(unix)]
fn describe(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("code {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "unknown".to_owned(),
    }
}

/// Run the host of scratch terminal `id`: `senate terminal-host`, started
/// by [`start`].
#[cfg(windows)]
pub async fn run(_paths: &Paths, _id: &EventId, _program: &Program<'_>) -> anyhow::Result<()> {
    anyhow::bail!("scratch terminals are not supported on Windows yet")
}

#[cfg(unix)]
pub async fn run(paths: &Paths, id: &EventId, program: &Program<'_>) -> anyhow::Result<()> {
    ensure!(
        record(paths, id).is_some(),
        "the Senate daemon reserved no scratch terminal {id}"
    );
    let (first, rest) = program
        .argv
        .split_first()
        .context("a scratch terminal needs a program to run")?;
    // The daemon is whoever listens on the Senate socket now: this host's
    // parent is the opener, not the daemon.
    let bridge = UnixStream::connect(paths.socket())
        .await
        .context("reach the Senate daemon")?;
    let daemon = Admission::peer(&bridge).context("the Senate daemon runs as another user")?;
    drop(bridge);
    let daemon =
        DaemonIdentity::capture(daemon).context("bind the terminal host to the Senate daemon")?;
    let admission = Arc::new(Admission {
        paths: paths.clone(),
        admitted: Mutex::new(daemon.process.clone()),
        daemon,
        tickets: Mutex::default(),
    });
    if let Err(error) = crate::fd::cap_host_limit() {
        tracing::warn!(%error, "could not lower the terminal host file descriptor limit");
    }
    let dir = directory(paths, id);
    let file = open_lock(&dir).context("own the terminal's directory")?;
    flock(&file, FlockOperation::LockExclusive)?;
    let _cleanup = RemoveDirectory(dir.clone());
    // Out of the opener's session and process group: the terminal outlives
    // the command that opened it.
    rustix::process::setsid().context("detach the Senate terminal host")?;
    std::fs::write(dir.join(HOST_PID), std::process::id().to_string())
        .context("record the host's process ID")?;
    let process = crate::kernel_process::inspect(std::process::id())?;
    let process = crate::kernel_process::BoundProcess::capture(process.identity)?;
    serde_json::to_writer(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join("host-process"))?,
        &process,
    )?;
    write_protocol(paths, id, &admission.daemon.process)?;
    let mut terminate = signal(SignalKind::terminate())?;
    let socket = socket(paths, id);
    let listener = UnixListener::bind(&socket).context("listen for terminal attachments")?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let controls = control::listen(paths, id).context("listen for terminal control requests")?;
    let viewer = Mutex::new(Viewer {
        path: dir.join("viewer.json"),
        activity: ViewerActivity::default(),
        dirty: false,
        draft: input::Draft::default(),
    });
    Viewer::lock(&viewer).record();
    let (pty, pts) = pty_process::open()?;
    pty.resize(Size::new(program.rows, program.cols))?;
    let scheme = emulation::system_scheme().await;
    // The opener's environment as it is, less the two names that would say
    // this program is a hosted session's own TUI.
    let mut child = pty_process::Command::new(first)
        .args(rest)
        .current_dir(program.cwd)
        .env("TERM", "xterm-256color")
        .env("COLORTERM", "truecolor")
        .env_remove(HOSTED_CLAUDE_ENV)
        .env_remove(HOSTED_AGY_ENV)
        .kill_on_drop(true)
        .spawn(pts)
        .with_context(|| format!("start {}", Path::new(first).display()))?;
    let pid = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(Pid::from_raw)
        .context("the program has no process ID")?;
    let mut startup_pid = file.try_clone()?;
    writeln!(startup_pid, "{pid}")?;
    drop(startup_pid);
    let (mut pty_read, pty_write) = pty.into_split();
    let tty = Arc::new(Tty::new(pty_write, scheme, program.rows, program.cols));
    let watch_socket = dir.join(watch::SOCKET);
    let watchers = UnixListener::bind(&watch_socket).context("listen for terminal watchers")?;
    std::fs::set_permissions(&watch_socket, std::fs::Permissions::from_mode(0o600))?;
    // The screen keeps what the program writes, and answers its terminal
    // queries, whether or not anyone looks.
    let mut pump = tokio::spawn({
        let tty = tty.clone();
        async move {
            let mut buf = [0; IO_CHUNK];
            while let Ok(n @ 1..) = pty_read.read(&mut buf).await {
                tty.output(&buf[..n]).await;
            }
        }
    });
    {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&[READY])
            .and_then(|()| stdout.flush())
            .context("report host readiness")?;
    }

    let exited = AtomicBool::new(false);
    let attached = serve(&listener, &tty, &viewer, &admission);
    let watched = watch::serve(&watchers, &tty.screen, &admission);
    let controlled = serve_control(&controls, &tty, &admission, id, &exited);
    tokio::pin!(attached, watched, controlled);
    let result = loop {
        tokio::select! {
            biased;
            status = child.wait(), if !exited.load(Ordering::Acquire) => {
                // What the program wrote last is still on its way through
                // the PTY; a child of it that holds the PTY open is not
                // waited for.
                let _ = tokio::time::timeout(Duration::from_millis(300), &mut pump).await;
                let status = status.map_or_else(|error| error.to_string(), describe);
                let staged = dir.join("exit.tmp");
                std::fs::write(&staged, &status)
                    .and_then(|()| std::fs::rename(&staged, dir.join(EXIT)))
                    .context("record how the program ended")?;
                exited.store(true, Ordering::Release);
                // Nothing more will be written: tell whoever waits on the
                // screen to look at the exit.
                let (rows, cols) = tty.screen.size();
                tty.screen.resize(rows, cols);
            }
            _ = terminate.recv() => break stop(&mut child, pid, &exited).await,
            () = vanished(&dir) => break stop(&mut child, pid, &exited).await,
            served = &mut attached => break served,
            served = &mut watched => break served,
            () = &mut controlled => break Ok(()),
        }
    };
    pump.abort();
    result
}

/// End the program's process group, unless the program already ended:
/// what it left behind is hung up on when the host drops the PTY.
#[cfg(unix)]
async fn stop(
    child: &mut tokio::process::Child,
    pid: Pid,
    exited: &AtomicBool,
) -> anyhow::Result<()> {
    if exited.load(Ordering::Acquire) {
        return Ok(());
    }
    stop_group(child, pid).await
}

/// What the daemon asks of a scratch host on `ctl`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Control {
    /// Type this text; a line end is Enter.
    Text(String),
    /// Press these keys ([`keys::parse`]).
    Keys(String),
    Resize {
        rows: u16,
        cols: u16,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    scratch: Control,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Grant {
    viewer: ViewTicket,
}

/// What became of a [`Control`] request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sent {
    Done,
    /// A person is attached and has the keyboard: nothing was sent.
    Held,
    /// The program has exited: nothing was sent.
    Exited,
}

/// Serve the daemon's requests for as long as the host runs: tickets for
/// viewers and watchers, as any host, and [`Control`].
#[cfg(unix)]
async fn serve_control(
    listener: &UnixListener,
    tty: &Tty,
    admission: &Admission,
    host: &EventId,
    exited: &AtomicBool,
) {
    loop {
        let mut stream = accept(listener).await;
        if !admission.is_daemon(&stream, host).await {
            tracing::warn!(%host, "refused a terminal control request from an unverified daemon peer");
            continue;
        }
        let Ok(Ok(Some(line))) =
            tokio::time::timeout(HANDSHAKE, ipc::read_line(&mut BufReader::new(&mut stream))).await
        else {
            continue;
        };
        if let Ok(Grant { viewer }) = serde_json::from_str::<Grant>(&line) {
            let _ = stream.write_u8(u8::from(admission.grant(viewer))).await;
            continue;
        }
        if line == r#"{"protocol_probe":true}"# {
            let _ = stream.write_u8(1).await;
            continue;
        }
        let Ok(Request { scratch }) = serde_json::from_str::<Request>(&line) else {
            continue;
        };
        let reply = apply(tty, exited, scratch).await;
        let _ = stream.write_u8(reply).await;
    }
}

#[cfg(unix)]
async fn apply(tty: &Tty, exited: &AtomicBool, control: Control) -> u8 {
    // The keyboard and the size are the attached person's while they hold
    // the terminal.
    if tty.attached.load(Ordering::Acquire) {
        return HELD;
    }
    let bytes = match control {
        Control::Resize { rows, cols } => {
            let fits = (1..=MAX_ROWS).contains(&rows) && (1..=MAX_COLS).contains(&cols);
            return match fits {
                true if tty.resize(rows, cols).await.is_ok() => DONE,
                _ => REFUSED,
            };
        }
        Control::Text(text) if text.len() <= MAX_TEXT => keys::text_bytes(&text),
        Control::Text(_) => return REFUSED,
        Control::Keys(names) => {
            let Ok(parsed) = keys::parse(&names) else {
                return REFUSED;
            };
            let application_cursor = tty.screen.application_cursor();
            let mut bytes = Vec::new();
            for key in parsed {
                key.encode(application_cursor, &mut bytes);
            }
            bytes
        }
    };
    if exited.load(Ordering::Acquire) {
        return EXITED;
    }
    let typed = async { tty.write.lock().await.write_all(&bytes).await };
    match tokio::time::timeout(WRITE_LIMIT, typed).await {
        Ok(Ok(())) => DONE,
        // The PTY takes nothing once the program is gone.
        _ if exited.load(Ordering::Acquire) => EXITED,
        _ => REFUSED,
    }
}

/// Ask scratch host `id` for `control`. The daemon's call: the host
/// answers nobody else.
pub(crate) async fn send(paths: &Paths, id: &EventId, control: Control) -> Result<Sent, IpcError> {
    let mut payload =
        serde_json::to_vec(&Request { scratch: control }).map_err(IpcError::internal)?;
    payload.push(b'\n');
    let reply = tokio::time::timeout(HANDSHAKE + WRITE_LIMIT, async {
        let mut stream = UnixStream::connect(directory(paths, id).join("ctl")).await?;
        stream.write_all(&payload).await?;
        stream.read_u8().await
    })
    .await
    .map_err(|_| IpcError::internal("timed out waiting for the terminal host"))?
    .map_err(|error| IpcError::internal(format!("the terminal host did not answer: {error}")))?;
    match reply {
        DONE => Ok(Sent::Done),
        HELD => Ok(Sent::Held),
        EXITED => Ok(Sent::Exited),
        _ => Err(IpcError::bad_request(
            "the terminal did not take that: the program is not reading its input, or the request is out of range",
        )),
    }
}
