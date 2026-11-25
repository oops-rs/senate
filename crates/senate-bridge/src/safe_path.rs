//! Descriptor-relative access to worktree data. Open every component before
//! inspecting it, and never truncate a filesystem object supplied by a caller.

use std::{
    fs::File,
    io::{self, Read, Write},
    os::fd::OwnedFd,
    path::{Component, Path},
};

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};

pub(crate) struct Directory(OwnedFd);

const DIRECTORY: OFlags = OFlags::DIRECTORY
    .union(OFlags::RDONLY)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW);
const READ_FILE: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

impl Directory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        // macOS exposes these fixed system aliases for temporary directories.
        // Resolve the aliases lexically so we still never open a symlink.
        #[cfg(target_os = "macos")]
        let mapped = ["/tmp", "/var"].iter().find_map(|prefix| {
            path.strip_prefix(prefix).ok().map(|rest| {
                Path::new("/private")
                    .join(prefix.trim_start_matches('/'))
                    .join(rest)
            })
        });
        #[cfg(target_os = "macos")]
        let path = mapped.as_deref().unwrap_or(path);
        let mut parts = path.components();
        if parts.next() != Some(Component::RootDir) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected an absolute directory",
            ));
        }
        let mut current = Self(fs::open("/", DIRECTORY, Mode::empty())?);
        for part in parts {
            let Component::Normal(name) = part else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid directory component",
                ));
            };
            current = Self(fs::openat(&current.0, name, DIRECTORY, Mode::empty())?);
        }
        Ok(current)
    }

    pub(crate) fn fd(&self) -> &OwnedFd {
        &self.0
    }

    pub(crate) fn has_git_entry(&self) -> io::Result<bool> {
        match fs::statat(&self.0, ".git", AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Directory => Ok(true),
            Ok(stat) => regular(&stat).map(|()| true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn child(&self, name: &str, create: bool) -> io::Result<Self> {
        let name = leaf(name)?;
        if create {
            match fs::mkdirat(&self.0, name, Mode::from_bits_truncate(0o700)) {
                Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self(fs::openat(&self.0, name, DIRECTORY, Mode::empty())?))
    }

    pub(crate) fn read(&self, name: &str, limit: usize) -> io::Result<Vec<u8>> {
        let fd = fs::openat(&self.0, leaf(name)?, READ_FILE, Mode::empty())?;
        regular(&fs::fstat(&fd)?)?;
        let mut bytes = Vec::new();
        File::from(fd).take(limit as u64).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn replace(&self, name: &str, bytes: &[u8], limit: usize) -> io::Result<()> {
        let name = leaf(name)?;
        if bytes.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file exceeds size limit",
            ));
        }
        match fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => regular(&stat)?,
            Err(rustix::io::Errno::NOENT) => {}
            Err(error) => return Err(error.into()),
        }
        let temp = format!(".senate-tmp-{}", senate_core::EventId::new());
        let fd = fs::openat(
            &self.0,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )?;
        let mut file = File::from(fd);
        let result = file
            .write_all(bytes)
            .and_then(|()| fs::renameat(&self.0, temp.as_str(), &self.0, name).map_err(Into::into));
        if result.is_err() {
            let _ = fs::unlinkat(&self.0, temp.as_str(), AtFlags::empty());
        }
        result
    }
}

fn leaf(name: &str) -> io::Result<&str> {
    let mut parts = Path::new(name).components();
    if matches!(parts.next(), Some(Component::Normal(part)) if part == name)
        && parts.next().is_none()
    {
        Ok(name)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a single file name",
        ))
    }
}

fn regular(stat: &fs::Stat) -> io::Result<()> {
    if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile && stat.st_nlink == 1 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected a singly linked regular file",
        ))
    }
}
