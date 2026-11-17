use std::fs::{self, File, Metadata};
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use tokio::net::{UnixListener, UnixStream};
use tokio::time::timeout;

pub(super) struct BoundSocket {
    pub(super) listener: UnixListener,
    path: PathBuf,
    identity: Metadata,
}

impl BoundSocket {
    pub(super) async fn bind(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)?;
        let directory = File::open(parent)?;
        ensure!(
            directory.metadata()?.mode() & 0o022 == 0,
            "chat gateway socket directory must not be writable by other accounts"
        );
        // Serialize competing binders through stale-socket recovery. This
        // lock is released as soon as bind returns, including for other
        // endpoints under the same private directory.
        directory
            .try_lock()
            .context("locking gateway socket directory")?;
        match fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_socket() => {
                match timeout(Duration::from_secs(1), UnixStream::connect(path)).await {
                    Ok(Err(e)) if e.kind() == ErrorKind::ConnectionRefused => {
                        fs::remove_file(path)?
                    }
                    Ok(Err(e)) if e.kind() == ErrorKind::NotFound => {}
                    _ => bail!("chat gateway socket is already in use"),
                }
            }
            Ok(_) => bail!("chat gateway socket path already exists"),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(path).context("binding chat gateway socket")?;
        let bound = Self {
            listener,
            path: path.to_path_buf(),
            identity: fs::symlink_metadata(path)?,
        };
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(bound)
    }
}

impl Drop for BoundSocket {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| {
            m.file_type().is_socket()
                && m.dev() == self.identity.dev()
                && m.ino() == self.identity.ino()
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
