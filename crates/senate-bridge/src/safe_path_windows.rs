//! Worktree data on Windows, with the guarantees of the Unix version
//! (`safe_path.rs`): no link (symbolic link or junction) is followed at
//! any level, a file read or replaced is a singly linked regular file, and
//! nothing supplied by a caller is truncated.
//!
//! Windows has no `openat`. Instead a [`Directory`] holds an open handle on
//! every directory from the root down to itself, each opened without
//! following a reparse point and without `FILE_SHARE_DELETE`: while it is
//! held none of them can be renamed, removed or replaced by a link, so its
//! path keeps naming the directory that was checked.
#![allow(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle,
};

pub(crate) struct Directory {
    path: PathBuf,
    /// Root first; the last is this directory.
    _held: Vec<File>,
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, what.to_owned())
}

fn information(file: &File) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    // SAFETY: plain data, written by the call on success.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a live handle owned by `file`.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(info)
}

/// Open `path` itself, never what a reparse point at it names, keeping it
/// from being renamed or removed while the handle lives.
fn hold(path: &Path, write: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(write)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

fn directory(path: &Path) -> io::Result<File> {
    let file = hold(path, false)?;
    let attributes = information(&file)?.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 || attributes & FILE_ATTRIBUTE_DIRECTORY == 0
    {
        return Err(invalid("expected a directory, not a link"));
    }
    Ok(file)
}

fn regular(file: &File) -> io::Result<()> {
    let info = information(file)?;
    let attributes = info.dwFileAttributes;
    if attributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) == 0
        && info.nNumberOfLinks == 1
    {
        Ok(())
    } else {
        Err(invalid("expected a singly linked regular file"))
    }
}

impl Directory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let mut parts = path.components();
        let (Some(Component::Prefix(prefix)), Some(Component::RootDir)) =
            (parts.next(), parts.next())
        else {
            return Err(invalid("expected an absolute directory"));
        };
        let mut current = PathBuf::from(prefix.as_os_str());
        current.push("\\");
        let mut held = vec![directory(&current)?];
        for part in parts {
            let Component::Normal(name) = part else {
                return Err(invalid("invalid directory component"));
            };
            current.push(name);
            held.push(directory(&current)?);
        }
        Ok(Self {
            path: current,
            _held: held,
        })
    }

    /// The directory's path, which names it for as long as `self` lives.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn has_git_entry(&self) -> io::Result<bool> {
        let file = match hold(&self.path.join(".git"), false) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let attributes = information(&file)?.dwFileAttributes;
        if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(invalid("expected a directory or a file, not a link"));
        }
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            return Ok(true);
        }
        regular(&file).map(|()| true)
    }

    pub(crate) fn child(&self, name: &str, create: bool) -> io::Result<Self> {
        let name = leaf(name)?;
        let path = self.path.join(name);
        if create {
            match std::fs::create_dir(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        let child = directory(&path)?;
        let mut held = Vec::with_capacity(self._held.len() + 1);
        for file in &self._held {
            held.push(file.try_clone()?);
        }
        held.push(child);
        Ok(Self { path, _held: held })
    }

    pub(crate) fn read(&self, name: &str, limit: usize) -> io::Result<Vec<u8>> {
        let file = hold(&self.path.join(leaf(name)?), false)?;
        regular(&file)?;
        let mut bytes = Vec::new();
        file.take(limit as u64).read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    pub(crate) fn replace(&self, name: &str, bytes: &[u8], limit: usize) -> io::Result<()> {
        let name = leaf(name)?;
        if bytes.len() > limit {
            return Err(invalid("file exceeds size limit"));
        }
        let target = self.path.join(name);
        match hold(&target, false) {
            Ok(file) => regular(&file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let temp = self
            .path
            .join(format!(".senate-tmp-{}", senate_core::EventId::new()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&temp)?;
        let result = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .and_then(|()| {
                drop(file);
                std::fs::rename(&temp, &target)
            });
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
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
        Err(invalid("expected a single file name"))
    }
}

/// Whether a file's metadata says it is a link (a reparse point).
#[allow(dead_code)]
pub(crate) fn is_link(meta: &std::fs::Metadata) -> bool {
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_and_replaces_plain_files_only() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let dir = Directory::open(root.path())?;
        assert!(!dir.has_git_entry()?);
        let notes = dir.child(".senate", true)?;
        notes.replace("PROGRESS.md", b"one", 16)?;
        assert_eq!(notes.read("PROGRESS.md", 16)?, b"one");
        notes.replace("PROGRESS.md", b"two", 16)?;
        assert_eq!(notes.read("PROGRESS.md", 2)?, b"tw");
        assert!(notes.replace("PROGRESS.md", b"too long", 4).is_err());
        assert!(notes.read("../x", 4).is_err());
        assert!(Directory::open(Path::new("relative")).is_err());
        Ok(())
    }

    #[test]
    fn a_held_directory_cannot_be_renamed_away() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("worktree");
        std::fs::create_dir(&path)?;
        let dir = Directory::open(&path)?;
        assert!(std::fs::rename(&path, root.path().join("moved")).is_err());
        drop(dir);
        std::fs::rename(&path, root.path().join("moved"))?;
        Ok(())
    }

    #[test]
    fn hard_links_are_refused() -> io::Result<()> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("a"), "x")?;
        std::fs::hard_link(root.path().join("a"), root.path().join("b"))?;
        let dir = Directory::open(root.path())?;
        assert!(dir.read("a", 4).is_err());
        assert!(dir.replace("b", b"y", 4).is_err());
        Ok(())
    }
}
