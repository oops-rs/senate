//! File descriptor budgets shared by the daemon and its service definitions.

use rustix::process::{Resource, getrlimit, setrlimit};

pub(crate) const SERVICE_LIMIT: u64 = 10_240;
const HOST_LIMIT: u64 = 4_096;

#[cfg(target_os = "macos")]
fn kernel_limit() -> Option<u64> {
    let mut value: libc::c_int = 0;
    let mut size = size_of_val(&value);
    // SAFETY: the kernel receives an initialized size and a writable integer.
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.maxfilesperproc".as_ptr(),
            (&raw mut value).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && value > 0).then_some(value as u64)
}

#[cfg(not(target_os = "macos"))]
fn kernel_limit() -> Option<u64> {
    None
}

/// Run before opening the daemon's listeners. Keep an administrator's hard
/// limit. Darwin wants a finite request rather than RLIM_INFINITY.
pub fn raise_daemon_limit() {
    let mut limits = getrlimit(Resource::Nofile);
    let before = limits.current;
    let target = SERVICE_LIMIT
        .min(limits.maximum.unwrap_or(u64::MAX))
        .min(kernel_limit().unwrap_or(u64::MAX));
    if before.is_some_and(|current| current < target) {
        limits.current = Some(target);
        if let Err(error) = setrlimit(Resource::Nofile, limits) {
            tracing::warn!(%error, target, "could not raise the daemon file descriptor limit");
        }
    }
    let after = effective_limit();
    tracing::info!(?before, ?after, hard_limit = ?limits.maximum, "daemon file descriptor limit");
}

/// A terminal host and its runtime need a smaller budget than a daemon
/// serving every session. Preserve any lower limit inherited from launch.
pub(crate) fn cap_host_limit() -> std::io::Result<()> {
    let mut limits = getrlimit(Resource::Nofile);
    if limits.current.is_none_or(|current| current > HOST_LIMIT) {
        limits.current = Some(HOST_LIMIT);
        setrlimit(Resource::Nofile, limits)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Snapshot {
    pub soft_limit: Option<u64>,
    pub open: Option<u64>,
}

fn effective_limit() -> Option<u64> {
    let current = getrlimit(Resource::Nofile).current;
    match (current, kernel_limit()) {
        (Some(current), Some(kernel)) => Some(current.min(kernel)),
        (current, kernel) => current.or(kernel),
    }
}

pub(crate) fn snapshot() -> Snapshot {
    Snapshot {
        soft_limit: effective_limit(),
        open: open_count(),
    }
}

pub(crate) fn exhausted(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

#[cfg(target_os = "macos")]
fn open_count() -> Option<u64> {
    let pid = std::process::id().try_into().ok()?;
    // No descriptor is opened by PROC_PIDLISTFDS, including at the limit.
    // SAFETY: a null buffer asks for the required number of bytes.
    let bytes =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let entry = size_of::<libc::proc_fdinfo>();
    let mut entries = (usize::try_from(bytes).ok()? / entry).max(32) + 32;
    loop {
        let capacity = (entries * entry).try_into().ok()?;
        let mut buffer = Vec::<libc::proc_fdinfo>::with_capacity(entries);
        // SAFETY: the buffer can receive capacity bytes and is never read
        // as Rust values. The result is the number of bytes written.
        let used = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                buffer.as_mut_ptr().cast(),
                capacity,
            )
        };
        if used <= 0 {
            return None;
        }
        if used < capacity {
            return Some(u64::try_from(used).ok()? / entry as u64);
        }
        entries *= 2;
    }
}

#[cfg(target_os = "linux")]
fn open_count() -> Option<u64> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    // The enumeration includes its own directory descriptor.
    Some((entries.count() as u64).saturating_sub(1))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_count() -> Option<u64> {
    None
}
