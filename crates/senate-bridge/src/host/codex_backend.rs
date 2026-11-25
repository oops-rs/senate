use std::net::{Ipv4Addr, SocketAddrV4};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use senate_core::EventId;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::process::{ChildStdin, Command};

use crate::Paths;
use crate::delivery::codex::Transport;
use crate::kernel_process::{self, BoundProcess};

const READY: Duration = Duration::from_secs(8);
const HOST_LEASE: Duration = Duration::from_secs(90);

pub(crate) struct Unpublished {
    // Closing this pipe rolls back a backend that has not acquired a host.
    _publication: ChildStdin,
    pub lease: PathBuf,
    pub transport: Transport,
}

pub(crate) fn socket(paths: &Paths, id: &EventId) -> PathBuf {
    directory(paths, id).join("rpc")
}

fn directory(paths: &Paths, id: &EventId) -> PathBuf {
    paths.home().join("cx").join(id.to_string())
}

impl Unpublished {
    pub(crate) async fn start(
        paths: &Paths,
        id: &EventId,
        bridge: &Path,
        program: &Path,
        cwd: &Path,
    ) -> Result<Self> {
        let rpc = socket(paths, id);
        let lease = directory(paths, id).join("host");
        ensure!(
            crate::testenv::confined(&rpc),
            "Codex backend socket is outside the test home"
        );
        ensure!(
            lease.as_os_str().len() <= crate::paths::SOCKET_PATH_LIMIT,
            "hosted Codex socket path is too long"
        );
        #[cfg(target_os = "linux")]
        let mut launcher = crate::linux_scope::command(bridge, id);
        #[cfg(not(target_os = "linux"))]
        let mut launcher = Command::new(bridge);
        let mut child = launcher
            .arg("session-codex-backend")
            .arg(id.to_string())
            .arg(program)
            .arg(cwd)
            .env(crate::paths::HOME_ENV, paths.home())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start the dedicated Codex backend supervisor")?;
        let publication = child
            .stdin
            .take()
            .context("open Codex backend startup lease")?;
        tokio::time::timeout(READY, async {
            loop {
                if let Some(status) = child.try_wait()? {
                    anyhow::bail!("dedicated Codex backend stopped before launch ({status})");
                }
                if UnixStream::connect(&rpc).await.is_ok() && lease.exists() {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("dedicated Codex app-server did not open its socket")??;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(Self {
            _publication: publication,
            lease,
            transport: Transport::Unix(rpc),
        })
    }
}

pub(crate) fn owner(paths: &Paths, id: &EventId) -> Option<crate::kernel_process::BoundProcess> {
    serde_json::from_slice(&std::fs::read(directory(paths, id).join("owner")).ok()?).ok()
}

struct BackendDirectory(PathBuf);

impl Drop for BackendDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) async fn supervise(
    paths: &Paths,
    id: &EventId,
    program: &Path,
    cwd: &Path,
) -> Result<()> {
    let daemon = bind_process(std::os::unix::process::parent_id())?;
    let executable = std::env::current_exe()?.canonicalize()?;
    let dir = directory(paths, id);
    ensure!(
        crate::testenv::confined(&dir),
        "Codex backend directory is outside the test home"
    );
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(
            dir.parent()
                .context("missing Codex backend directory parent")?,
        )?;
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let _cleanup = BackendDirectory(dir.clone());
    let lease = private_listener(&dir.join("host"))?;
    let mut secret = [0_u8; 32];
    getrandom::fill(&mut secret).map_err(|error| anyhow::anyhow!("{error}"))?;
    let token = hex::encode(secret);
    let digest = hex::encode(Sha256::digest(token.as_bytes()));
    let mut backend = Command::new(program)
        .args([
            "app-server",
            "--listen",
            "ws://127.0.0.1:0",
            "--ws-auth",
            "capability-token",
            "--ws-token-sha256",
            &digest,
        ])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start authenticated dedicated Codex app-server")?;
    let owner = bind_process(backend.id().context("dedicated Codex backend pid")?)?;
    let mut report = BufReader::new(backend.stderr.take().context("Codex startup report pipe")?);
    let upstream = tokio::time::timeout(READY, startup_address(&mut report))
        .await
        .context("Codex did not report its authenticated listener")??;
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut report, &mut tokio::io::sink()).await;
    });
    std::fs::write(dir.join("owner"), serde_json::to_vec(&owner)?)?;
    let rpc = private_listener(&socket(paths, id))?;
    let mut publication = tokio::net::unix::pipe::Receiver::from_owned_fd(
        rustix::io::fcntl_dupfd_cloexec(std::io::stdin(), 3)?,
    )?;
    let (host_tx, host_rx) = tokio::sync::watch::channel(None);
    let (tui_tx, tui_rx) = tokio::sync::watch::channel(None);
    let broker = serve_rpc(
        paths, &rpc, &daemon, &owner, upstream, &token, host_rx, tui_rx,
    );
    tokio::pin!(broker);
    let mut byte = [0];
    let (mut attached, host) = tokio::select! {
        biased;
        result = tokio::time::timeout(HOST_LEASE, accept_host(&lease, &daemon, &executable)) => {
            result.context("terminal host did not acquire its Codex backend")??
        }
        _ = publication.read(&mut byte) => return Ok(()),
        result = &mut broker => return result,
        status = backend.wait() => anyhow::bail!("dedicated Codex app-server exited ({})", status?),
    };
    host_tx.send_replace(Some(host.clone()));
    let tui = tokio::select! {
        result = tokio::time::timeout(READY, attached.read_u32()) => result.context("terminal host did not identify its TUI")??,
        result = &mut broker => return result,
        status = backend.wait() => anyhow::bail!("dedicated Codex app-server exited ({})", status?),
    };
    ensure!(
        is_child(tui, &host),
        "Codex TUI is not a child of the terminal host"
    );
    tui_tx.send_replace(Some(bind_process(tui)?));
    tokio::select! {
        _ = attached.read(&mut byte) => { backend.kill().await?; }
        result = &mut broker => result?,
        status = backend.wait() => { status?; }
    }
    Ok(())
}

fn private_listener(path: &Path) -> Result<UnixListener> {
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

fn bind_process(pid: u32) -> std::io::Result<BoundProcess> {
    BoundProcess::capture(kernel_process::inspect(pid)?.identity)
}

fn is_child(pid: u32, parent: &BoundProcess) -> bool {
    parent.live()
        && kernel_process::inspect(pid)
            .is_ok_and(|process| process.parent_pid == parent.pid && parent.contains(pid, 2))
}

async fn accept_host(
    listener: &UnixListener,
    daemon: &BoundProcess,
    executable: &Path,
) -> Result<(UnixStream, BoundProcess)> {
    loop {
        let (stream, _) = listener.accept().await?;
        if let Some(pid) = super::Admission::peer(&stream)
            && is_child(pid, daemon)
            && kernel_process::executable(pid).is_ok_and(|path| path == executable)
        {
            return Ok((stream, bind_process(pid)?));
        }
    }
}

async fn startup_address(
    report: &mut BufReader<impl tokio::io::AsyncRead + Unpin>,
) -> Result<SocketAddrV4> {
    let mut report = report.take(32 * 1024);
    let mut line = String::new();
    for _ in 0..32 {
        if report.read_line(&mut line).await? == 0 {
            break;
        }
        if let Some(port) = line.trim().strip_prefix("listening on: ws://127.0.0.1:") {
            let port = port.parse::<u16>()?;
            ensure!(port != 0, "Codex reported an invalid port");
            return Ok(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        }
        line.clear();
    }
    anyhow::bail!("Codex did not report an authenticated loopback listener")
}

type OwnerWatch = tokio::sync::watch::Receiver<Option<BoundProcess>>;

#[allow(clippy::too_many_arguments)]
async fn serve_rpc(
    paths: &Paths,
    listener: &UnixListener,
    daemon: &BoundProcess,
    backend: &BoundProcess,
    upstream: SocketAddrV4,
    token: &str,
    host: OwnerWatch,
    tui: OwnerWatch,
) -> Result<()> {
    let mut clients = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept(), if clients.len() < 64 => {
                let (stream, _) = accepted?;
                let Some(pid) = super::Admission::peer(&stream) else { continue };
                let local_daemon = if daemon.live() {
                    pid == daemon.pid
                } else {
                    #[cfg(target_os = "macos")]
                    let trusted = super::replacement_daemon(paths, pid).await;
                    #[cfg(not(target_os = "macos"))]
                    let trusted = { let _ = paths; false };
                    trusted
                };
                let terminal = host.borrow().as_ref().is_some_and(|host| is_child(pid, host));
                if !local_daemon && !terminal { continue }
                let Ok(peer) = bind_process(pid) else { continue };
                let backend = backend.clone();
                let token = token.to_owned();
                let mut tui = tui.clone();
                clients.spawn(async move {
                    if !local_daemon {
                        let admitted = tokio::time::timeout(READY, tui.wait_for(Option::is_some))
                            .await.is_ok_and(|result| result.is_ok_and(|known| known.as_ref() == Some(&peer)));
                        if !admitted { return }
                    }
                    if peer.live() && backend.live() {
                        let _ = relay(stream, upstream, &token, &backend).await;
                    }
                });
            }
            _ = clients.join_next(), if !clients.is_empty() => {}
        }
    }
}

async fn relay(
    mut local: UnixStream,
    address: SocketAddrV4,
    token: &str,
    backend: &BoundProcess,
) -> Result<()> {
    let mut head = Vec::new();
    let end = tokio::time::timeout(READY, async {
        let mut buf = [0; 1024];
        loop {
            if let Some(end) = head.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                return Ok(end + 2);
            }
            ensure!(head.len() < 16 * 1024, "Codex upgrade header is too long");
            let len = local.read(&mut buf).await?;
            ensure!(len != 0, "Codex client closed before the upgrade");
            head.extend_from_slice(&buf[..len]);
        }
    })
    .await??;
    let headers = std::str::from_utf8(&head[..end])?;
    ensure!(
        headers.starts_with("GET ")
            && !headers.lines().any(|line| {
                line.split_once(':')
                    .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
            }),
        "unexpected Codex upgrade"
    );
    ensure!(backend.live(), "Codex backend exited");
    let mut remote = TcpStream::connect(address).await?;
    ensure!(backend.live(), "Codex backend exited");
    remote.write_all(&head[..end]).await?;
    remote
        .write_all(format!("Authorization: Bearer {token}\r\n").as_bytes())
        .await?;
    remote.write_all(&head[end..]).await?;
    tokio::io::copy_bidirectional(&mut local, &mut remote).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_requires_an_explicit_loopback_address() {
        let mut good =
            BufReader::new(&b"Codex app-server\n  listening on: ws://127.0.0.1:45700\n"[..]);
        assert_eq!(startup_address(&mut good).await.unwrap().port(), 45_700);
        let mut public = BufReader::new(&b"listening on: ws://0.0.0.0:45700\n"[..]);
        assert!(startup_address(&mut public).await.is_err());
    }
}
