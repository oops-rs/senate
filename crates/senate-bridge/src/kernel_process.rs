//! Process instance data supplied by the kernel.

use std::io;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProcessIdentity {
    pub pid: u32,
    pub birth: u64,
}

/// A persisted process instance. PID and start identity are scoped to one boot.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct BoundProcess {
    boot: String,
    pub(crate) pid: u32,
    birth: u64,
}

impl BoundProcess {
    pub(crate) fn capture(process: ProcessIdentity) -> io::Result<Self> {
        #[cfg(target_os = "macos")]
        let boot = crate::mac_scope::boot_id()?;
        #[cfg(target_os = "linux")]
        let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
            .trim()
            .to_owned();
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let boot = String::new();
        if boot.is_empty() {
            return Err(io::Error::other("kernel boot identity is unavailable"));
        }
        Ok(Self {
            boot,
            pid: process.pid,
            birth: process.birth,
        })
    }

    pub(crate) fn live(&self) -> bool {
        inspect(self.pid)
            .ok()
            .and_then(|process| Self::capture(process.identity).ok())
            .is_some_and(|current| current == *self)
    }

    pub(crate) fn contains(&self, caller: u32, limit: usize) -> bool {
        self.live()
            && (caller == self.pid
                || ancestors(caller, limit).is_ok_and(|ancestors| {
                    ancestors.contains(&ProcessIdentity {
                        pid: self.pid,
                        birth: self.birth,
                    })
                }))
    }
}

#[derive(Debug)]
pub(crate) struct Process {
    pub identity: ProcessIdentity,
    pub parent_pid: u32,
    pub parent_birth: Option<u64>,
    pub name: String,
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) fn inspect(pid: u32) -> io::Result<Process> {
    // PROC_PIDT_BSDINFOWITHUNIQID puts the BSD and process generation data in
    // one kernel snapshot. In particular p_puniqueid survives reparenting.
    const PROC_PIDT_BSDINFOWITHUNIQID: libc::c_int = 18;

    #[repr(C)]
    struct ProcessInfo {
        bsd: libc::proc_bsdinfo,
        executable_uuid: [u8; 16],
        unique_id: u64,
        original_parent_unique_id: u64,
        pid_version: i32,
        original_parent_pid_version: i32,
        reserved: [u64; 2],
    }

    // ProcessInfo matches the integer-only layout for this flavor.
    let info = unsafe { mac_info::<ProcessInfo>(pid, PROC_PIDT_BSDINFOWITHUNIQID) }?;
    let name = if info.bsd.pbi_name[0] == 0 {
        mac_path_bytes(info.bsd.pbi_comm)
    } else {
        mac_path_bytes(info.bsd.pbi_name)
    };
    Ok(Process {
        identity: ProcessIdentity {
            pid,
            birth: info.unique_id,
        },
        parent_pid: info.bsd.pbi_ppid,
        parent_birth: Some(info.original_parent_unique_id),
        name: String::from_utf8_lossy(&name).into_owned(),
    })
}

#[cfg(target_os = "linux")]
pub(crate) fn inspect(pid: u32) -> io::Result<Process> {
    let stat = std::fs::read(format!("/proc/{pid}/stat"))?;
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid process stat");
    let start = stat
        .iter()
        .position(|byte| *byte == b'(')
        .ok_or_else(invalid)?;
    let end = stat
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(invalid)?;
    let name = stat.get(start + 1..end).ok_or_else(invalid)?;
    let fields = std::str::from_utf8(&stat[end + 1..]).map_err(|_| invalid())?;
    let mut fields = fields.split_ascii_whitespace();
    // After the final ')' the first field is state (#3). Parent PID is
    // field #4 and the process start time is field #22.
    let parent_pid = fields.nth(1).and_then(|field| field.parse::<u32>().ok());
    let birth = fields.nth(17).and_then(|field| field.parse::<u64>().ok());
    let (Some(parent_pid), Some(birth)) = (parent_pid, birth) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid process stat",
        ));
    };
    Ok(Process {
        identity: ProcessIdentity { pid, birth },
        parent_pid,
        parent_birth: None,
        name: String::from_utf8_lossy(name).into_owned(),
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn inspect(_pid: u32) -> io::Result<Process> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process generations are unavailable",
    ))
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
unsafe fn mac_info<T>(pid: u32, flavor: libc::c_int) -> io::Result<T> {
    let pid = libc::pid_t::try_from(pid)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid process ID"))?;
    let mut info = std::mem::MaybeUninit::<T>::uninit();
    let size = std::mem::size_of::<T>() as libc::c_int;
    // The caller supplies the kernel layout for this flavor. The buffer is
    // aligned and writable for the entire size passed to libproc.
    let written = unsafe { libc::proc_pidinfo(pid, flavor, 0, info.as_mut_ptr().cast(), size) };
    if written != size {
        return Err(if written == 0 {
            io::Error::last_os_error()
        } else {
            io::Error::new(io::ErrorKind::InvalidData, "unexpected process info layout")
        });
    }
    // The caller supplies an integer-only layout and all bytes were written.
    Ok(unsafe { info.assume_init() })
}

#[cfg(target_os = "macos")]
fn mac_path_bytes(chars: impl IntoIterator<Item = libc::c_char>) -> Vec<u8> {
    chars
        .into_iter()
        .take_while(|&ch| ch != 0)
        .map(|ch| ch as u8)
        .collect()
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) fn cwd(pid: u32) -> io::Result<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    // proc_vnodepathinfo is the integer-only layout for this kernel flavor.
    let info = unsafe { mac_info::<libc::proc_vnodepathinfo>(pid, libc::PROC_PIDVNODEPATHINFO) }?;
    let bytes = mac_path_bytes(info.pvi_cdir.vip_path.into_iter().flatten());
    Ok(std::ffi::OsString::from_vec(bytes).into())
}

#[cfg(target_os = "linux")]
pub(crate) fn cwd(pid: u32) -> io::Result<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn cwd(_pid: u32) -> io::Result<PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "process working directories are unavailable",
    ))
}

/// Return the verified prefix of a live process's parent chain. A reparented
/// process has no link to its former parent through this lookup.
pub(crate) fn ancestors(peer: u32, limit: usize) -> io::Result<Vec<ProcessIdentity>> {
    let mut child = inspect(peer)?;
    let origin = child.identity;
    let mut ancestors = Vec::new();
    while child.parent_pid > 1 && ancestors.len() < limit {
        let parent = inspect(child.parent_pid)?;
        if child
            .parent_birth
            .is_some_and(|birth| birth != parent.identity.birth)
        {
            break;
        }
        let checked_child = inspect(child.identity.pid)?;
        if checked_child.identity != child.identity
            || checked_child.parent_pid != parent.identity.pid
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "process ancestry changed",
            ));
        }
        if parent.identity == origin || ancestors.contains(&parent.identity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "cyclic process ancestry",
            ));
        }
        ancestors.push(parent.identity);
        child = parent;
    }
    if inspect(peer)?.identity != origin {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "peer process changed",
        ));
    }
    Ok(ancestors)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn live_process_identity_matches_its_parent() -> io::Result<()> {
        let pid = std::process::id();
        let process = inspect(pid)?;
        let parent = std::os::unix::process::parent_id();
        assert_eq!(process.identity.pid, pid);
        assert_eq!(process.parent_pid, parent);
        assert_eq!(process.identity, inspect(pid)?.identity);
        assert_eq!(
            cwd(pid)?.canonicalize()?,
            std::env::current_dir()?.canonicalize()?
        );
        if parent > 1 {
            assert_eq!(ancestors(pid, 1)?, [inspect(parent)?.identity]);
        }
        Ok(())
    }
}
