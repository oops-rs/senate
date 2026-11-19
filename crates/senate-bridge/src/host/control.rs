//! Stop requests use a private socket so they can reach a host with a viewer.

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use senate_core::EventId;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

use super::{Viewer, accept, directory, observed_at};
use crate::ipc::{IpcError, codes};
use crate::paths::Paths;

const STOP: u8 = 0;
const FORCE: u8 = 1;
const ATTENDED: u8 = 0;
const ACCEPTED: u8 = 1;
const STOPPED: u8 = 1;
const FAILED: u8 = 2;
const END_UNCONFIRMED: u8 = 3;
const HANDSHAKE: Duration = Duration::from_secs(2);

fn socket(paths: &Paths, id: &EventId) -> PathBuf {
    directory(paths, id).join("ctl")
}

pub(super) fn listen(paths: &Paths, id: &EventId) -> io::Result<UnixListener> {
    let path = socket(paths, id);
    let listener = UnixListener::bind(&path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub(super) async fn next_stop(listener: &UnixListener, viewer: &Mutex<Viewer>) -> UnixStream {
    loop {
        let (mut stream, _) = accept(listener).await;
        let force = match tokio::time::timeout(HANDSHAKE, stream.read_u8()).await {
            Ok(Ok(STOP)) => false,
            Ok(Ok(FORCE)) => true,
            _ => continue,
        };
        let attended = Viewer::lock(viewer)
            .activity
            .unattended_since(observed_at())
            .is_none();
        if attended && !force {
            let _ = stream.write_u8(ATTENDED).await;
        } else if stream.write_u8(ACCEPTED).await.is_ok() {
            return stream;
        }
    }
}

/// Release the session binding guard after this acknowledgement, before the
/// host sends its normal HostedSessionEnd request back to the daemon.
pub(crate) async fn begin_stop(
    paths: &Paths,
    id: &EventId,
    force: bool,
) -> Result<UnixStream, IpcError> {
    tokio::time::timeout(HANDSHAKE, async {
        let mut stream = UnixStream::connect(socket(paths, id)).await.map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                IpcError::bad_request("the host's stop socket is unavailable. If it was started before this update, attach and exit the terminal")
            } else {
                IpcError::internal(error)
            }
        })?;
        stream.write_u8(if force { FORCE } else { STOP }).await.map_err(IpcError::internal)?;
        match stream.read_u8().await {
            Ok(ACCEPTED) => Ok(stream),
            Ok(ATTENDED) => Err(IpcError::bad_request(
                "a viewer is attending this terminal. Ask them to detach or have the operator run senate stop --force",
            )),
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
