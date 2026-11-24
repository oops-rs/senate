//! Optional cgroup v2 protection for administrator-provisioned Linux hosts.
#![allow(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, ensure};
use senate_core::EventId;
use sha2::{Digest, Sha256};
use tokio::process::Command;

const HELPER: &str = "/usr/local/libexec/senate-host-guard";
const ROOT: &str = "/sys/fs/cgroup/senate-hosted";
const MAX_HELPER: u64 = 2 * 1024 * 1024;

fn root_controls(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.uid() == 0 && meta.mode() & 0o022 == 0)
}

pub(crate) fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        if rustix::process::geteuid().as_raw() == 0
            || !Path::new("/sys/fs/cgroup/cgroup.controllers").is_file()
            || !root_controls(Path::new("/sys/fs/cgroup/cgroup.procs"))
            || (Path::new(ROOT).exists() && !root_controls(&Path::new(ROOT).join("cgroup.procs")))
            || senate_remote_admin::check_install_location().is_err()
            || !std::fs::symlink_metadata(HELPER).is_ok_and(|meta| {
                meta.is_file() && meta.uid() == 0 && meta.mode() & 0o7777 == 0o4755
            })
        {
            return false;
        }
        std::os::unix::net::UnixStream::pair()
            .and_then(|(stream, _)| peer_pidfd(&stream))
            .is_ok()
    })
}

pub(crate) fn command(program: &Path, host: &EventId) -> Command {
    if available() {
        let mut command = Command::new(HELPER);
        command.arg(host.to_string()).arg(program);
        command
    } else {
        Command::new(program)
    }
}

fn peer_pidfd(stream: &impl AsRawFd) -> io::Result<OwnedFd> {
    let mut fd: libc::c_int = -1;
    let mut len = std::mem::size_of_val(&fd) as libc::socklen_t;
    // getsockopt writes one PID file descriptor into the integer buffer.
    let status = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERPIDFD,
            (&mut fd as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if status != 0 {
        return Err(io::Error::last_os_error());
    }
    if fd < 0 {
        return Err(io::Error::other("invalid socket peer process handle"));
    }
    // On success SO_PEERPIDFD returns a new owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

pub(crate) struct Peer {
    pid: u32,
    handle: OwnedFd,
}

impl Peer {
    pub(crate) fn capture(stream: &impl AsRawFd, pid: u32) -> io::Result<Self> {
        Ok(Self {
            pid,
            handle: peer_pidfd(stream)?,
        })
    }

    fn live(&self) -> bool {
        let mut event = libc::pollfd {
            fd: self.handle.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // A PID file descriptor becomes readable when its process exits.
        unsafe { libc::poll(&mut event, 1, 0) == 0 }
    }

    pub(crate) fn classify(&self) -> Option<(u32, bool)> {
        if !self.live() {
            return None;
        }
        let memberships = std::fs::read_to_string(format!("/proc/{}/cgroup", self.pid)).ok()?;
        let scope = memberships
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?;
        if !self.live() {
            return None;
        }
        let local = format!("/senate-hosted/{}", rustix::process::geteuid().as_raw());
        Some((self.pid, Path::new(scope).starts_with(local)))
    }
}

pub(crate) fn install(source: Option<&Path>, expected: &str) -> anyhow::Result<()> {
    ensure!(
        rustix::process::geteuid().as_raw() == 0,
        "Run sudo senate setup guard with the trusted helper checksum"
    );
    ensure!(
        Path::new("/sys/fs/cgroup/cgroup.controllers").is_file(),
        "This machine has no cgroup v2 mount"
    );
    std::os::unix::net::UnixStream::pair()
        .and_then(|(stream, _)| peer_pidfd(&stream))
        .context("The kernel does not provide stable Unix peer PID handles")?;
    let default = std::env::current_exe()?.with_file_name("senate-host-guard");
    let source = source.unwrap_or(&default);
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)
        .context("open the unprivileged helper build artifact")?;
    ensure!(
        file.metadata()?.is_file(),
        "The helper must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_HELPER + 1).read_to_end(&mut bytes)?;
    ensure!(
        !bytes.is_empty() && bytes.len() as u64 <= MAX_HELPER,
        "Invalid helper size"
    );
    ensure!(
        hex::encode(Sha256::digest(&bytes)) == expected.to_ascii_lowercase(),
        "Helper checksum differs"
    );
    senate_remote_admin::check_install_location()?;
    std::fs::create_dir_all("/usr/local/libexec")?;
    senate_remote_admin::check_install_location()?;
    std::fs::create_dir_all(ROOT)?;
    let scope = std::fs::symlink_metadata(ROOT)?;
    ensure!(
        scope.is_dir() && scope.uid() == 0 && scope.mode() & 0o022 == 0,
        "The cgroup root is not trusted"
    );

    let target = Path::new(HELPER);
    let parent = target.parent().context("missing helper parent")?;
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(&bytes)?;
    staged
        .as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o4755))?;
    staged.as_file().sync_all()?;
    staged.persist(target).map_err(|error| error.error)?;
    File::open(parent)?.sync_all()?;
    println!("Installed the root-owned Senate cgroup launcher at {HELPER}");
    Ok(())
}
