//! A detached terminal owner. The daemon records the session's identity, but
//! only this process holds the master PTY and its child.

mod terminal;

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
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

use crate::ipc::{self, HostedRuntime, Request};
use crate::paths::{HOME_ENV, Paths, SOCKET_PATH_LIMIT};

pub use terminal::attach;

pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(75);
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
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(false),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(e) => Err(e.into()),
    }
}

/// A host can outlive a daemon that stops before committing SessionStart.
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

/// The daemon selects this native identity before the TUI is started.
pub struct SessionCommand<'a> {
    pub session_id: &'a str,
    pub resume: bool,
    pub codex_remote: Option<&'a str>,
}

pub(crate) const READY: u8 = 1;

pub(crate) fn start(
    paths: &Paths,
    id: &EventId,
    runtime: HostedRuntime,
    cwd: &Path,
    session: &SessionCommand<'_>,
) -> anyhow::Result<(Child, File)> {
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

    let exe = std::env::current_exe()?.with_file_name("senate");
    let mut command = Command::new(exe);
    command
        .arg("session-host")
        .arg(id.to_string())
        .arg(crate::session::Runtime::from(runtime).as_str())
        .arg(cwd)
        .arg(session.session_id)
        .env(HOME_ENV, paths.home())
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    if session.resume {
        command.arg("--resume");
    }
    if let Some(remote) = session.codex_remote {
        command.arg("--codex-remote").arg(remote);
    }

    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let mut spawn = || -> anyhow::Result<(Child, File)> {
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
    let dir = directory(paths, id);
    let file = open_lock(&dir).context("own the host directory")?;
    flock(&file, FlockOperation::NonBlockingLockExclusive)?;
    let _cleanup = RemoveDirectory(dir.clone());
    rustix::process::setsid().context("detach the Senate terminal host")?;
    let mut terminate = signal(SignalKind::terminate())?;
    let socket = socket(paths, id);
    let listener = UnixListener::bind(&socket).context("listen for terminal attachments")?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
    let (pty, pts) = pty_process::open()?;
    pty.resize(Size::new(24, 80))?;
    let mut child = runtime_command(paths, runtime, cwd, session)?
        .spawn(pts)
        .context("start coding TUI on its PTY")?;

    let (mut pty_read, mut pty_write) = pty.into_split();
    // A slow or disconnected viewer must never stop the PTY being drained.
    let (output, _) = broadcast::channel(64);
    let feed = output.clone();
    let pump = tokio::spawn(async move {
        let mut buf = [0; IO_CHUNK];
        while let Ok(n @ 1..) = pty_read.read(&mut buf).await {
            let _ = feed.send(buf[..n].to_vec());
        }
    });

    // This pipe belongs to the launching daemon. When it has gone away,
    // allow the already recorded session to keep running.
    use std::io::Write as _;
    let mut ready = std::io::stdout().lock();
    let _ = ready.write_all(&[READY]).and_then(|()| ready.flush());
    drop(ready);

    let result = tokio::select! {
        status = child.wait() => status.context("wait for the coding TUI").and_then(|status| {
            ensure!(status.success(), "the coding TUI exited with {status}");
            Ok(())
        }),
        served = serve(&listener, &mut pty_write, &output) => served,
        _ = terminate.recv() => child.kill().await.context("stop the coding TUI"),
    };
    pump.abort();
    // The runtime's normal SessionEnd hook usually gets there first. A
    // crash or stop without that hook must also retire this session.
    let _: Result<serde_json::Value, _> = ipc::request(
        &paths.socket(),
        &Request::SessionEnd {
            session_id: session.session_id.to_owned(),
            reason: Some("terminal host ended".into()),
        },
        Duration::from_millis(300),
        Duration::from_secs(2),
    )
    .await;
    result
}

fn runtime_command(
    paths: &Paths,
    runtime: HostedRuntime,
    cwd: &Path,
    session: &SessionCommand<'_>,
) -> anyhow::Result<pty_process::Command> {
    let command = match runtime {
        HostedRuntime::Codex => {
            let config = crate::delivery::codex::CodexConfig::load(&paths.config())
                .map_err(anyhow::Error::msg)?;
            pty_process::Command::new(config.bin.unwrap_or_else(|| PathBuf::from("codex")))
                .args([
                    "resume",
                    "--remote",
                    session
                        .codex_remote
                        .context("missing Codex app-server address")?,
                ])
                .arg("--cd")
                .arg(cwd)
                .arg(session.session_id)
        }
        HostedRuntime::Claude => pty_process::Command::new("claude").args([
            if session.resume {
                "--resume"
            } else {
                "--session-id"
            },
            session.session_id,
        ]),
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
        .env_remove(crate::companion::ENV_COMPANION)
        .env_remove(crate::companion::ENV_SESSION)
        .kill_on_drop(true);
    Ok(command)
}

async fn serve(
    listener: &UnixListener,
    pty: &mut OwnedWritePty,
    output: &broadcast::Sender<Vec<u8>>,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let relay = relay(stream, pty, output.subscribe());
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
                connection = listener.accept() => {
                    let (other, _) = connection?;
                    let _ = other.try_write(&[BUSY]);
                }
            }
        }
    }
}

async fn relay(
    mut stream: UnixStream,
    pty: &mut OwnedWritePty,
    mut output: broadcast::Receiver<Vec<u8>>,
) -> anyhow::Result<()> {
    stream.write_u8(ATTACHED).await?;
    let (mut input, mut screen) = stream.into_split();
    let from_terminal = async {
        let mut first_resize = true;
        let mut keys = [0; IO_CHUNK];
        loop {
            match input.read_u8().await? {
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
                    pty.write_all(&keys[..n]).await?;
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
            screen.write_all(&bytes).await?;
        }
    };
    tokio::select! {
        result = from_terminal => result,
        result = to_terminal => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Other tests can be forking while we run. Unlock explicitly so
        // a descriptor inherited briefly before exec cannot delay release.
        flock(&owner, FlockOperation::Unlock).unwrap();
        assert!(running_ids(&paths).unwrap().is_empty());
    }
}
