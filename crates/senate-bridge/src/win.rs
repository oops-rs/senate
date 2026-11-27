//! Windows primitives for the developer machine: the person's SID, an
//! owner-only security descriptor for the home directory and the daemon's
//! named pipe, the peer of a named pipe, and the kernel's facts about a
//! process (birth, parent, image).
//!
//! Each call checks its result and fails with the OS error, never with a
//! guess: callers treat an error as "no proof".
#![allow(unsafe_code)]

use std::ffi::OsString;
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::RawHandle;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, EqualSid, GetSecurityDescriptorDacl, GetTokenInformation,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeServerProcessId};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};

/// A handle closed on drop.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the handle was returned open by the OS and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn check(ok: i32) -> io::Result<()> {
    if ok == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// A security identifier, copied out of a token.
#[derive(Clone)]
pub(crate) struct Sid(Vec<u8>);

impl Sid {
    fn ptr(&self) -> *mut core::ffi::c_void {
        self.0.as_ptr().cast_mut().cast()
    }

    /// `S-1-5-21-…`.
    pub(crate) fn string(&self) -> io::Result<String> {
        let mut text: *mut u16 = std::ptr::null_mut();
        // SAFETY: the SID bytes came from GetTokenInformation; the string
        // is allocated by the OS and freed below.
        check(unsafe { ConvertSidToStringSidW(self.ptr(), &mut text) })?;
        let mut len = 0;
        // SAFETY: the OS wrote a NUL-terminated string.
        while unsafe { *text.add(len) } != 0 {
            len += 1;
        }
        // SAFETY: `len` u16s precede the NUL.
        let value = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
        // SAFETY: allocated by ConvertSidToStringSidW.
        unsafe { LocalFree(text.cast()) };
        Ok(value)
    }
}

impl PartialEq for Sid {
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: both are valid SIDs copied from tokens.
        unsafe { EqualSid(self.ptr(), other.ptr()) != 0 }
    }
}

impl std::fmt::Debug for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.string().unwrap_or_else(|_| "<sid>".into()))
    }
}

fn token_user(process: HANDLE) -> io::Result<Sid> {
    token_sid(process, TokenUser)
}

/// The owner a process gives what it creates: its user, or for an
/// elevated administrator the Administrators group.
pub(crate) fn current_default_owner() -> io::Result<Sid> {
    use windows_sys::Win32::Security::TokenOwner;
    // SAFETY: the pseudo-handle of the current process needs no closing.
    token_sid(unsafe { GetCurrentProcess() }, TokenOwner)
}

/// A SID-valued token field, TokenUser or TokenOwner: both structures
/// start with a pointer to the SID, which lies in the same buffer.
fn token_sid(
    process: HANDLE,
    class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> io::Result<Sid> {
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: `process` is a live process handle with query access.
    check(unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) })?;
    let token = Owned(token);
    let mut needed = 0u32;
    // SAFETY: a size query with no buffer.
    let ok = unsafe { GetTokenInformation(token.0, class, std::ptr::null_mut(), 0, &mut needed) };
    if ok == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(error);
        }
    }
    // u64 storage keeps the structure aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: the buffer holds `needed` bytes.
    check(unsafe {
        GetTokenInformation(
            token.0,
            class,
            buffer.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    })?;
    // SAFETY: the OS wrote a TOKEN_USER (whose first field, a
    // SID_AND_ATTRIBUTES, starts with the SID pointer) or a TOKEN_OWNER
    // (the SID pointer) at the start of the buffer.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }.cast::<u8>();
    // A SID is 8 bytes plus 4 per sub-authority (byte 1 counts them).
    // SAFETY: `sid` points at a valid SID inside `buffer`.
    let len = 8 + 4 * usize::from(unsafe { *sid.add(1) });
    // SAFETY: `len` bytes of the SID are inside `buffer`.
    Ok(Sid(unsafe { std::slice::from_raw_parts(sid, len) }.to_vec()))
}

/// The user this process runs as.
pub(crate) fn current_user() -> io::Result<Sid> {
    // SAFETY: the pseudo-handle of the current process needs no closing.
    token_user(unsafe { GetCurrentProcess() })
}

fn open(pid: u32) -> io::Result<Owned> {
    // SAFETY: OpenProcess has no memory arguments.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(Owned(handle))
    }
}

/// The user process `pid` runs as.
pub(crate) fn process_user(pid: u32) -> io::Result<Sid> {
    token_user(open(pid)?.0)
}

/// Whether the OS refused to describe a process because another user (or
/// the system) owns it.
pub(crate) fn foreign(error: &io::Error) -> bool {
    error.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
}

/// A self-relative security descriptor whose DACL grants all access to
/// one user and nobody else, protected from inheritance.
pub(crate) struct OwnerOnly {
    descriptor: PSECURITY_DESCRIPTOR,
}

// SAFETY: the descriptor is immutable after creation.
unsafe impl Send for OwnerOnly {}
// SAFETY: the descriptor is immutable after creation.
unsafe impl Sync for OwnerOnly {}

impl OwnerOnly {
    /// For `user`; containers pass it on to what is created inside them.
    pub(crate) fn new(user: &Sid) -> io::Result<Self> {
        Self::from_sddl(&format!("D:P(A;OICI;GA;;;{})", user.string()?))
    }

    /// From an SDDL string. Tests use it to make a descriptor for someone
    /// else.
    pub(crate) fn from_sddl(sddl: &str) -> io::Result<Self> {
        let sddl = wide(sddl);
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: a NUL-terminated SDDL string; the OS allocates the
        // descriptor, freed on drop.
        check(unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        })?;
        Ok(Self { descriptor })
    }

    /// For the current user.
    pub(crate) fn current() -> io::Result<Self> {
        Self::new(&current_user()?)
    }

    /// Attributes for creating an object with this descriptor. They point
    /// into `self`, which must outlive the call that uses them.
    pub(crate) fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: self.descriptor,
            bInheritHandle: 0,
        }
    }

    fn dacl(&self) -> io::Result<*mut ACL> {
        let (mut present, mut defaulted) = (0, 0);
        let mut dacl: *mut ACL = std::ptr::null_mut();
        // SAFETY: a valid descriptor; the DACL points into it.
        check(unsafe {
            GetSecurityDescriptorDacl(self.descriptor, &mut present, &mut dacl, &mut defaulted)
        })?;
        if present == 0 || dacl.is_null() {
            return Err(io::Error::other("owner-only descriptor has no DACL"));
        }
        Ok(dacl)
    }

    /// Replace `path`'s DACL with this one, inherited by nothing above it.
    pub(crate) fn apply(&self, path: &Path) -> io::Result<()> {
        let dacl = self.dacl()?;
        let path = wide_path(path);
        // SAFETY: a NUL-terminated path and a DACL inside `self`.
        let status = unsafe {
            SetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(status as i32))
        }
    }
}

impl Drop for OwnerOnly {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW.
        unsafe { LocalFree(self.descriptor) };
    }
}

/// The process at the other end of a connected named pipe, as the kernel
/// reports it: the client, seen from the server.
pub(crate) fn pipe_client(handle: RawHandle) -> io::Result<u32> {
    let mut pid = 0;
    // SAFETY: a live pipe handle owned by the caller.
    check(unsafe { GetNamedPipeClientProcessId(handle, &mut pid) })?;
    Ok(pid)
}

/// The server, seen from a client.
pub(crate) fn pipe_server(handle: RawHandle) -> io::Result<u32> {
    let mut pid = 0;
    // SAFETY: a live pipe handle owned by the caller.
    check(unsafe { GetNamedPipeServerProcessId(handle, &mut pid) })?;
    Ok(pid)
}

/// A process's creation time (FILETIME, 100 ns since 1601): together with
/// its pid, the process instance.
pub(crate) fn birth(pid: u32) -> io::Result<u64> {
    let process = open(pid)?;
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: a live handle and four writable FILETIMEs.
    check(unsafe {
        GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user)
    })?;
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// One process in a snapshot of all of them.
pub(crate) struct Entry {
    pub pid: u32,
    pub parent: u32,
    /// The image file name, `codex.exe`.
    pub name: String,
}

/// Every process the kernel lists, at one instant.
pub(crate) fn processes() -> io::Result<Vec<Entry>> {
    // SAFETY: no memory arguments.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = Owned(snapshot);
    // SAFETY: PROCESSENTRY32W is plain data; dwSize is set before use.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut out = Vec::new();
    // SAFETY: a live snapshot and an entry with dwSize set.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
    while more {
        let len = entry
            .szExeFile
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExeFile.len());
        out.push(Entry {
            pid: entry.th32ProcessID,
            parent: entry.th32ParentProcessID,
            name: String::from_utf16_lossy(&entry.szExeFile[..len]),
        });
        // SAFETY: as above.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
    }
    Ok(out)
}

/// The full path of a process's image.
pub(crate) fn executable(pid: u32) -> io::Result<PathBuf> {
    let process = open(pid)?;
    let mut path = vec![0u16; 32_768];
    let mut len = path.len() as u32;
    // SAFETY: a live handle and a buffer of `len` u16s.
    check(unsafe {
        QueryFullProcessImageNameW(process.0, PROCESS_NAME_WIN32, path.as_mut_ptr(), &mut len)
    })?;
    path.truncate(len as usize);
    Ok(OsString::from_wide(&path).into())
}

/// Whether a process with this pid is running (it may belong to another
/// user: then the OS refuses to open it, which says it exists).
pub(crate) fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    if pid == 0 {
        return false;
    }
    match open(pid) {
        Ok(process) => {
            let mut code = 0u32;
            // SAFETY: a live handle and a writable u32.
            unsafe { GetExitCodeProcess(process.0, &mut code) != 0 && code == STILL_ACTIVE as u32 }
        }
        Err(error) => foreign(&error),
    }
}

/// The parent this process was started by, as the kernel recorded it.
pub(crate) fn parent_id() -> u32 {
    let me = std::process::id();
    processes()
        .ok()
        .and_then(|list| list.into_iter().find(|entry| entry.pid == me))
        .map_or(0, |entry| entry.parent)
}

/// The owner of a file or directory.
pub(crate) fn file_owner(path: &Path) -> io::Result<Sid> {
    use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
    use windows_sys::Win32::Security::{GetLengthSid, OWNER_SECURITY_INFORMATION};
    let path = wide_path(path);
    let mut owner: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path; the OS allocates the descriptor that
    // `owner` points into, freed below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    // SAFETY: `owner` is a valid SID inside `descriptor`.
    let len = unsafe { GetLengthSid(owner) } as usize;
    // SAFETY: `len` bytes of the SID are readable.
    let sid = Sid(unsafe { std::slice::from_raw_parts(owner.cast::<u8>(), len) }.to_vec());
    // SAFETY: allocated by GetNamedSecurityInfoW.
    unsafe { LocalFree(descriptor) };
    Ok(sid)
}

/// Whether `path`'s DACL lets nobody but this user in: every ACE that
/// allows access names this user's SID. A missing (null) DACL admits
/// everyone.
pub(crate) fn admits_only_me(path: &Path) -> io::Result<bool> {
    use windows_sys::Win32::Security::Authorization::GetNamedSecurityInfoW;
    use windows_sys::Win32::Security::{ACCESS_ALLOWED_ACE, ACE_HEADER, GetAce};
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    let me = current_user()?;
    let wide = wide_path(path);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated path; the OS allocates the descriptor that
    // `dacl` points into, freed below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let mut only_me = !dacl.is_null();
    if only_me {
        // SAFETY: a valid ACL inside `descriptor`.
        let count = unsafe { (*dacl).AceCount };
        for index in 0..u32::from(count) {
            let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
            // SAFETY: `index` is below the ACL's count.
            if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
                only_me = false;
                break;
            }
            // SAFETY: every ACE starts with a header.
            let header = unsafe { &*ace.cast::<ACE_HEADER>() };
            if header.AceType != ACCESS_ALLOWED_ACE_TYPE {
                // A deny ACE takes access away; any other allowing kind
                // (object, callback) is not expected here: refuse it.
                if header.AceType == 1 {
                    continue;
                }
                only_me = false;
                break;
            }
            // SAFETY: an ACCESS_ALLOWED_ACE, whose SID starts at SidStart.
            let sid =
                unsafe { (&raw const (*ace.cast::<ACCESS_ALLOWED_ACE>()).SidStart).cast::<u8>() };
            // SAFETY: byte 1 of a SID counts its sub-authorities.
            let len = 8 + 4 * usize::from(unsafe { *sid.add(1) });
            // SAFETY: the SID lies within the ACE.
            let sid = Sid(unsafe { std::slice::from_raw_parts(sid, len) }.to_vec());
            if sid != me {
                only_me = false;
                break;
            }
        }
    }
    // SAFETY: allocated by GetNamedSecurityInfoW.
    unsafe { LocalFree(descriptor) };
    Ok(only_me)
}

#[repr(C)]
struct TimeOfDay {
    boot_time: i64,
    current_time: i64,
    time_zone_bias: i64,
    time_zone_id: u32,
    reserved: u32,
    boot_time_bias: u64,
    sleep_time_bias: u64,
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtQuerySystemInformation(
        class: u32,
        information: *mut core::ffi::c_void,
        length: u32,
        returned: *mut u32,
    ) -> i32;
}

/// This boot, as the kernel's boot time (100 ns since 1601). It is fixed
/// for the life of the boot, unlike a boot time derived from uptime.
pub(crate) fn boot_id() -> io::Result<String> {
    const SYSTEM_TIME_OF_DAY_INFORMATION: u32 = 3;
    // SAFETY: plain data, fully written by the call on success.
    let mut info: TimeOfDay = unsafe { std::mem::zeroed() };
    let mut returned = 0;
    // SAFETY: the buffer is the documented layout for this class.
    let status = unsafe {
        NtQuerySystemInformation(
            SYSTEM_TIME_OF_DAY_INFORMATION,
            (&raw mut info).cast(),
            std::mem::size_of::<TimeOfDay>() as u32,
            &mut returned,
        )
    };
    if status != 0 || info.boot_time == 0 {
        return Err(io::Error::other(format!(
            "the boot time is unavailable (NTSTATUS {status:#x})"
        )));
    }
    Ok(format!("windows-boot-{}", info.boot_time))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_described_by_the_kernel() -> io::Result<()> {
        let me = std::process::id();
        let user = current_user()?;
        assert_eq!(process_user(me)?, user);
        assert!(user.string()?.starts_with("S-1-"));
        let list = processes()?;
        let entry = list.iter().find(|entry| entry.pid == me).expect("listed");
        assert!(entry.name.to_ascii_lowercase().ends_with(".exe"));
        assert_eq!(birth(me)?, birth(me)?);
        assert!(birth(entry.parent)? < birth(me)?);
        assert_eq!(executable(me)?, std::env::current_exe()?);
        assert_eq!(boot_id()?, boot_id()?);
        Ok(())
    }

    #[test]
    fn the_system_account_is_not_the_person() -> io::Result<()> {
        // A process another account runs is either unreadable or not ours.
        let system = processes()?
            .into_iter()
            .find(|entry| entry.name.eq_ignore_ascii_case("lsass.exe"))
            .expect("lsass runs");
        match process_user(system.pid) {
            Ok(sid) => assert_ne!(sid, current_user()?),
            Err(error) => assert!(foreign(&error), "{error}"),
        }
        Ok(())
    }

    #[test]
    fn an_owner_only_directory_admits_only_its_owner() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let home = dir.path().join("home");
        std::fs::create_dir(&home)?;
        assert!(
            !admits_only_me(&home)?,
            "a temp dir inherits the profile's ACL"
        );
        OwnerOnly::current()?.apply(&home)?;
        assert!(admits_only_me(&home)?);
        std::fs::write(home.join("token"), "x")?;
        assert!(admits_only_me(&home.join("token"))?, "inherited");
        assert_eq!(std::fs::read_to_string(home.join("token"))?, "x");
        Ok(())
    }
}
