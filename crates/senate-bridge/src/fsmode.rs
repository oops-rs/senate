//! File modes where they exist. On Unix these are std's own extension
//! traits, so call sites read `.mode(0o600)` as before. On Windows there are
//! no modes: privacy comes from the DACL of Senate's home, which grants the
//! person alone and is inherited by everything created inside it
//! ([`crate::paths::Paths::ensure_home`]), and from the profile's own
//! ACL for the agents' configuration files. Setting a mode is then a no-op;
//! reading one is not offered, so no check can pass on an invented mode.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub(crate) use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

#[cfg(windows)]
pub(crate) trait OpenOptionsExt {
    fn mode(&mut self, mode: u32) -> &mut Self;
}

#[cfg(windows)]
impl OpenOptionsExt for std::fs::OpenOptions {
    fn mode(&mut self, _mode: u32) -> &mut Self {
        self
    }
}

#[cfg(windows)]
pub(crate) trait DirBuilderExt {
    fn mode(&mut self, mode: u32) -> &mut Self;
}

#[cfg(windows)]
impl DirBuilderExt for std::fs::DirBuilder {
    fn mode(&mut self, _mode: u32) -> &mut Self {
        self
    }
}

/// `chmod`; nothing on Windows.
pub(crate) fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    return std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode));
    #[cfg(windows)]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Whether `meta` belongs to a file the OS will run: an execute bit on
/// Unix, an executable extension on Windows (`.exe`, `.cmd`, `.bat`,
/// `.com`).
pub(crate) fn runnable(path: &Path, meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        let _ = path;
        meta.is_file() && std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0
    }
    #[cfg(windows)]
    {
        meta.is_file()
            && path.extension().is_some_and(|ext| {
                ["exe", "cmd", "bat", "com"]
                    .iter()
                    .any(|known| ext.eq_ignore_ascii_case(known))
            })
    }
}

/// Whether only the owner may read `meta`'s file. Windows answers no: a
/// DACL is not checked here, so a check that needs this fails closed.
/// [`private`] checks one.
pub(crate) fn owner_only(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    return std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o077 == 0;
    #[cfg(windows)]
    {
        let _ = meta;
        false
    }
}

/// Whether nobody but its owner may open `path`: no group or other bits
/// on Unix; on Windows a DACL that admits this user's SID alone.
pub(crate) fn private(path: &Path, meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        let _ = path;
        owner_only(meta)
    }
    #[cfg(windows)]
    {
        let _ = meta;
        crate::win::admits_only_me(path).unwrap_or(false)
    }
}

#[cfg(unix)]
pub(crate) use std::os::unix::fs::{FileExt, MetadataExt};

/// Positional reads. `seek_read` also moves the file's cursor, which no
/// caller here relies on: they read only through `read_at`.
#[cfg(windows)]
pub(crate) trait FileExt {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize>;
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

#[cfg(windows)]
impl FileExt for std::fs::File {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        std::os::windows::fs::FileExt::seek_read(self, buf, offset)
    }

    fn read_exact_at(&self, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
        while !buf.is_empty() {
            match self.read_at(buf, offset) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    buf = &mut buf[n..];
                    offset += n as u64;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

/// A file's identity, for noticing that a path now names another file.
/// Stable Rust exposes no file index on Windows, so the identity there is
/// the creation time: a file created in another's place has another one
/// (unless NTFS tunnelling hands the old one on, within 15 seconds of a
/// delete under the same name; the size and anchor checks still apply).
#[cfg(windows)]
pub(crate) trait MetadataExt {
    fn dev(&self) -> u64;
    fn ino(&self) -> u64;
}

#[cfg(windows)]
impl MetadataExt for std::fs::Metadata {
    fn dev(&self) -> u64 {
        0
    }

    fn ino(&self) -> u64 {
        std::os::windows::fs::MetadataExt::creation_time(self)
    }
}

/// Open without following a link at the last component, and without
/// waiting on a FIFO (Unix). On Windows a reparse point is opened as
/// itself, and so is never a regular file to the caller's check.
pub(crate) fn no_follow(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    return std::os::unix::fs::OpenOptionsExt::custom_flags(
        options,
        libc::O_NONBLOCK | libc::O_NOFOLLOW,
    );
    #[cfg(windows)]
    return std::os::windows::fs::OpenOptionsExt::custom_flags(
        options,
        windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
    );
}
