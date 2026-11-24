use std::ffi::OsStr;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

use super::{Front, Shared, internal, send_ipc_line, wrap};
use crate::host;
use crate::ipc::{IpcError, MAX_SESSION_DROP_BYTES, Reply, Request, SESSION_DROP_TIMEOUT, codes};
use crate::paths::Paths;

pub(super) async fn serve(
    front: &Front,
    peer: Option<u32>,
    request: Request,
    mut read: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
) {
    let Request::SessionDrop {
        session_id,
        name,
        size,
    } = request
    else {
        return;
    };

    let outcome = async {
        front
            .refuse_agent_caller(peer, "Add attachments from the Senate app")
            .await?;
        validate(&name, size)?;
        let shared = front.primary();
        let directory = target(shared, &session_id)?;
        let _permit = front
            .session_drops
            .try_acquire()
            .map_err(|_| IpcError::bad_request("other attachments are uploading; try again"))?;
        if !send_ipc_line(&mut write, &Reply::ok(&())).await {
            return Err(IpcError::bad_request("the attachment client disconnected"));
        }
        let mut bytes = vec![0; size as usize];
        tokio::time::timeout(SESSION_DROP_TIMEOUT, read.read_exact(&mut bytes))
            .await
            .map_err(|_| IpcError::bad_request("the attachment upload timed out"))?
            .map_err(|_| {
                IpcError::bad_request("the attachment upload ended before all bytes arrived")
            })?;
        let shared = shared.clone();
        tokio::task::spawn_blocking(move || {
            if target(&shared, &session_id)? != directory {
                return Err(IpcError::bad_request(
                    "the session's terminal host changed during upload",
                ));
            }
            let drop = host::paste::store(&directory, OsStr::new(&name), &mut bytes.as_slice())
                .map_err(|error| {
                    IpcError::bad_request(format!("could not save the attachment: {error}"))
                })?;
            drop.join(name)
                .into_os_string()
                .into_string()
                .map_err(|_| IpcError::internal("the attachment path is not UTF-8"))
        })
        .await
        .map_err(IpcError::internal)?
    }
    .await;
    send_ipc_line(&mut write, &wrap(outcome)).await;
}

fn validate(name: &str, size: u32) -> Result<(), IpcError> {
    if size > MAX_SESSION_DROP_BYTES {
        return Err(IpcError::bad_request("attachments are limited to 20 MiB"));
    }
    if Path::new(name).file_name() != Some(OsStr::new(name))
        || name.len() > 255
        || name.contains('\\')
        || name.chars().any(char::is_control)
    {
        return Err(IpcError::bad_request(
            "use a file name without directories or control characters",
        ));
    }
    Ok(())
}

fn target(shared: &Shared, session_id: &str) -> Result<PathBuf, IpcError> {
    let session = shared
        .store
        .session(session_id)
        .map_err(internal)?
        .filter(|_| !shared.hosts.unpublished(session_id))
        .ok_or_else(|| IpcError::new(codes::NO_SESSION, "no registered session with that ID"))?;
    let host = shared
        .store
        .session_host(session_id)
        .map_err(internal)?
        .ok_or_else(|| IpcError::bad_request("this session is not hosted on this machine"))?;
    let paths = Paths::new(&shared.home);
    let socket = host::socket(&paths, &host);
    if !session.is_live()
        || !std::fs::symlink_metadata(&socket).is_ok_and(|stat| stat.file_type().is_socket())
    {
        return Err(IpcError::bad_request(
            "this session's terminal host is not running",
        ));
    }
    let drops = host::paste::drop_directory(&paths, &socket)
        .map_err(|_| IpcError::bad_request("this session's terminal directory is unavailable"))?;
    if !host::running(&paths, &host).map_err(IpcError::internal)? {
        return Err(IpcError::bad_request(
            "this session's terminal host is not running",
        ));
    }
    Ok(drops)
}
