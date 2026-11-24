use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use senate_core::EventId;
use tokio::io::AsyncReadExt;
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{ChildStdin, Command};

use crate::Paths;
use crate::delivery::codex::Transport;

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
        let mut child = Command::new(bridge)
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
    use std::os::unix::fs::DirBuilderExt;

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
    let lease = UnixListener::bind(dir.join("host"))?;
    let rpc = socket(paths, id);
    let mut backend = Command::new(program)
        .args(["app-server", "--listen"])
        .arg(Transport::Unix(rpc.clone()).remote())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("start protected Codex app-server")?;
    let mut publication = tokio::net::unix::pipe::Receiver::from_owned_fd(
        rustix::io::fcntl_dupfd_cloexec(std::io::stdin(), 3)?,
    )?;
    let mut byte = [0];
    let (mut attached, _) = tokio::select! {
        biased;
        result = tokio::time::timeout(HOST_LEASE, lease.accept()) => {
            result.context("terminal host did not acquire its Codex backend")??
        }
        _ = publication.read(&mut byte) => return Ok(()),
        status = backend.wait() => anyhow::bail!("dedicated Codex app-server exited ({})", status?),
    };
    tokio::select! {
        _ = attached.read(&mut byte) => { backend.kill().await?; }
        status = backend.wait() => { status?; }
    }
    Ok(())
}
