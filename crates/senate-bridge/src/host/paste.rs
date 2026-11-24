//! Keep files dropped into the viewer available to the hosted process.

use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{self as fd, AtFlags, Dir, FileType, FlockOperation, Mode, OFlags};
use senate_core::EventId;

const START: &[u8] = b"\x1b[200~";
const END: &[u8] = b"\x1b[201~";
// Large text pastes can stream through without being held in memory.
const MAX_PATH_PASTE: usize = 64 * 1024;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum State {
    #[default]
    Plain,
    Paste,
    Stream,
}

#[derive(Default)]
pub(super) struct Pastes {
    pending: Vec<u8>,
    state: State,
}

impl Pastes {
    pub(super) fn waiting_for_key(&self) -> bool {
        self.state == State::Plain
            && !self.pending.is_empty()
            && !(self.pending.len() > 2
                && super::keyboard::MODIFIED_ENTER
                    .iter()
                    .any(|key| key.starts_with(&self.pending)))
    }

    pub(super) fn flush_key(&mut self) -> Vec<u8> {
        if self.state == State::Plain {
            std::mem::take(&mut self.pending)
        } else {
            Vec::new()
        }
    }

    /// Forward the input in order. Only a key outside a paste may detach.
    pub(super) async fn feed(&mut self, bytes: &[u8], drops: Option<&Path>) -> (Vec<u8>, bool) {
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        loop {
            let marker = if self.state == State::Plain {
                START
            } else {
                END
            };
            let found = self.pending.windows(marker.len()).position(|w| w == marker);
            let mut send = self.pending.len();
            if self.state == State::Plain {
                let (keys, detach) =
                    super::terminal::split_input(&self.pending[..found.unwrap_or(send)]);
                if detach {
                    plain_keys(&mut output, &self.pending[..keys]);
                    return (output, true);
                }
                send = keys;
            }
            if let Some(at) = found {
                let mut part: Vec<_> = self.pending.drain(..at + marker.len()).collect();
                if self.state == State::Paste
                    && at <= MAX_PATH_PASTE
                    && at != 0
                    && let Some(drops) = drops
                {
                    part.truncate(at);
                    let drops = drops.to_owned();
                    let original = part.clone();
                    part = tokio::task::spawn_blocking(move || rewrite(&part, &drops))
                        .await
                        .unwrap_or(original);
                    part.extend_from_slice(END);
                }
                if self.state == State::Plain {
                    plain_keys(&mut output, &part[..at]);
                    output.extend_from_slice(START);
                } else {
                    output.extend_from_slice(&part);
                }
                self.state = match (self.state, drops) {
                    (State::Plain, Some(_)) => State::Paste,
                    (State::Plain, None) => State::Stream,
                    _ => State::Plain,
                };
            } else {
                if self.state == State::Paste {
                    if self.pending.len() < MAX_PATH_PASTE + END.len() {
                        break;
                    }
                    self.state = State::Stream;
                }
                // Keep just enough to recognize a delimiter split across reads.
                let tail = (1..marker.len().min(self.pending.len() + 1))
                    .rev()
                    .find(|&n| marker.starts_with(&self.pending[self.pending.len() - n..]))
                    .unwrap_or(0);
                let send = send.min(self.pending.len() - tail);
                if self.state == State::Plain {
                    plain_keys(&mut output, &self.pending[..send]);
                    self.pending.drain(..send);
                } else {
                    output.extend(self.pending.drain(..send));
                }
                break;
            }
        }
        (output, false)
    }
}

/// Preserve everything except the alternate xterm encoding of shifted
/// Enter. Bracketed paste bodies never go through this translation.
fn plain_keys(output: &mut Vec<u8>, mut bytes: &[u8]) {
    use super::keyboard::{SHIFT_ENTER, XTERM_SHIFT_ENTER};
    while let Some(at) = bytes
        .windows(XTERM_SHIFT_ENTER.len())
        .position(|window| window == XTERM_SHIFT_ENTER)
    {
        output.extend_from_slice(&bytes[..at]);
        output.extend_from_slice(SHIFT_ENTER);
        bytes = &bytes[at + XTERM_SHIFT_ENTER.len()..];
    }
    output.extend_from_slice(bytes);
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Quote {
    #[default]
    Bare,
    Single,
    Double,
}

impl Quote {
    fn delimiter(self) -> &'static [u8] {
        match self {
            Self::Bare => b"",
            Self::Single => b"'",
            Self::Double => b"\"",
        }
    }

    fn encode(self, bytes: &[u8], out: &mut Vec<u8>) {
        for &byte in bytes {
            match self {
                Self::Single if byte == b'\'' => out.extend_from_slice(b"'\\''"),
                Self::Double if b"\"\\`$".contains(&byte) => {
                    out.extend_from_slice(&[b'\\', byte]);
                }
                Self::Bare
                    if byte.is_ascii()
                        && !byte.is_ascii_alphanumeric()
                        && !b"/._-".contains(&byte) =>
                {
                    out.extend_from_slice(&[b'\\', byte]);
                }
                _ => out.push(byte),
            }
        }
    }
}

#[derive(Default)]
struct Word {
    decoded: Vec<u8>,
    first: Option<(usize, Quote)>,
    after_slash: Option<(usize, Quote)>,
}

fn read_word(bytes: &[u8], at: &mut usize) -> Option<Word> {
    let mut word = Word::default();
    let mut quote = Quote::Bare;
    while let Some(&byte) = bytes.get(*at) {
        let start = *at;
        if quote == Quote::Bare && byte.is_ascii_whitespace() {
            break;
        }
        *at += 1;
        match (quote, byte) {
            (Quote::Bare, b'\'') => quote = Quote::Single,
            (Quote::Bare, b'"') => quote = Quote::Double,
            (Quote::Single, b'\'') | (Quote::Double, b'"') => quote = Quote::Bare,
            _ => {
                let mut literal = byte;
                if byte == b'\\' && quote != Quote::Single {
                    let escaped = *bytes.get(*at)?;
                    if quote == Quote::Bare || b"\"\\`$\n".contains(&escaped) {
                        *at += 1;
                        if escaped == b'\n' {
                            continue;
                        }
                        literal = escaped;
                    }
                }
                word.first.get_or_insert((start, quote));
                if literal == b'/' {
                    word.after_slash = Some((*at, quote));
                }
                word.decoded.push(literal);
            }
        }
    }
    (quote == Quote::Bare).then_some(word)
}

fn rewrite(bytes: &[u8], drops: &Path) -> Vec<u8> {
    let mut at = 0;
    let mut copied = 0;
    let mut output = Vec::new();
    while at < bytes.len() {
        if bytes[at].is_ascii_whitespace() {
            at += 1;
            continue;
        }
        if !b"/.~'\"\\".contains(&bytes[at]) {
            at += bytes[at..]
                .iter()
                .take_while(|byte| !byte.is_ascii_whitespace())
                .count();
            continue;
        }
        let Some(word) = read_word(bytes, &mut at) else {
            break;
        };
        let (Some((start, prefix)), Some((end, suffix))) = (word.first, word.after_slash) else {
            continue;
        };
        // These are file references even in prose. Avoid interpreting
        // arbitrary words or expanding shell expressions.
        let source = match word.decoded.as_slice() {
            path if path.starts_with(b"/")
                || path.starts_with(b"./")
                || path.starts_with(b"../") =>
            {
                PathBuf::from(OsStr::from_bytes(path))
            }
            path if path.starts_with(b"~/") && prefix == Quote::Bare => match dirs::home_dir() {
                Some(home) => home.join(OsStr::from_bytes(&path[2..])),
                None => continue,
            },
            _ => continue,
        };
        let Ok(directory) = copy(&source, drops) else {
            continue;
        };
        output.extend_from_slice(&bytes[copied..start]);
        prefix.encode(directory.as_os_str().as_bytes(), &mut output);
        output.push(b'/');
        if prefix != suffix {
            output.extend_from_slice(prefix.delimiter());
            output.extend_from_slice(suffix.delimiter());
        }
        // Leave the filename, quotes, escaping and whitespace byte-for-byte.
        copied = end;
    }
    output.extend_from_slice(&bytes[copied..]);
    output
}

/// The same host directory for terminal pastes and local composer uploads.
pub(crate) fn drop_directory(paths: &crate::Paths, socket: &Path) -> io::Result<PathBuf> {
    let parent = socket.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let host_name = parent.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let hosts = super::hosts(paths);
    if !fs::symlink_metadata(&hosts)?.is_dir() || !fs::symlink_metadata(parent)?.is_dir() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let host = parent.canonicalize()?;
    if host != hosts.canonicalize()?.join(host_name) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(host.join("drops"))
}

fn copy(source: &Path, drops: &Path) -> io::Result<PathBuf> {
    let name = source.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    // Nonblocking open avoids waiting if a pasted path is a named pipe.
    let mut input = File::from(fd::open(
        source,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if !input.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    store(drops, name, &mut input)
}

// Both writers share the same budget and the same cross-process lock.
// The count limit also bounds the directory walk for zero-byte drops.
const DROP_BYTES: u64 = 100 * 1024 * 1024;
const DROP_FILES: usize = 128;
const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const PRIVATE_DIR: Mode = Mode::RUSR.union(Mode::WUSR).union(Mode::XUSR);
const PRIVATE_FILE: Mode = Mode::RUSR.union(Mode::WUSR);

struct AttachmentLock(OwnedFd);

impl AttachmentLock {
    fn acquire(root: &impl AsFd) -> io::Result<Self> {
        let lock = fd::openat(
            root,
            ".lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            PRIVATE_FILE,
        )?;
        if !FileType::from_raw_mode(fd::fstat(&lock)?.st_mode).is_file() {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        fd::flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                io::Error::other("another attachment is being saved; try again")
            } else {
                e.into()
            }
        })?;
        Ok(Self(lock))
    }
}

impl Drop for AttachmentLock {
    fn drop(&mut self) {
        // A concurrent process spawn can briefly inherit the file description
        // before CLOEXEC closes it. Closing our fd alone would keep it locked.
        let _ = rustix::io::retry_on_intr(|| fd::flock(&self.0, FlockOperation::Unlock));
    }
}

fn quota_error() -> io::Error {
    io::Error::other("this session's attachment storage is full (100 MiB or 128 files)")
}

fn remaining(directory: &impl AsFd) -> io::Result<u64> {
    let mut size = 0u64;
    let mut count = 0;
    for entry in Dir::read_from(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b".." | b".lock") {
            continue;
        }
        count += 1;
        if count >= DROP_FILES {
            return Err(quota_error());
        }
        let drop = fd::openat(directory, name, DIRECTORY, Mode::empty())?;
        let mut seen_file = false;
        for file in Dir::read_from(&drop)? {
            let file = file?;
            let name = file.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            if std::mem::replace(&mut seen_file, true) {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            let stat = fd::statat(&drop, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if !FileType::from_raw_mode(stat.st_mode).is_file() {
                return Err(io::ErrorKind::InvalidInput.into());
            }
            size = size.saturating_add(stat.st_size as u64);
            if size >= DROP_BYTES {
                return Err(quota_error());
            }
        }
    }
    Ok(DROP_BYTES - size)
}

/// Store one file in a fresh 0700 directory under a host's drops directory.
/// Every destination component under the host is opened without following
/// symlinks. The host's existing directory removal also removes its drops.
pub(crate) fn store(drops: &Path, name: &OsStr, input: &mut impl Read) -> io::Result<PathBuf> {
    if Path::new(name).file_name() != Some(name) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let parent = drops.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let basename = drops.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let hosts = fd::open(
        parent.parent().ok_or(io::ErrorKind::InvalidInput)?,
        DIRECTORY,
        Mode::empty(),
    )?;
    let host = fd::openat(
        &hosts,
        parent.file_name().ok_or(io::ErrorKind::InvalidInput)?,
        DIRECTORY,
        Mode::empty(),
    )?;
    match fd::mkdirat(&host, basename, PRIVATE_DIR) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(error.into()),
    }
    let root = fd::openat(&host, basename, DIRECTORY, Mode::empty())?;
    let _lock = AttachmentLock::acquire(&root)?;
    let remaining = remaining(&root)?;
    let id = EventId::new().to_string();
    fd::mkdirat(&root, id.as_str(), PRIVATE_DIR)?;
    let result = (|| {
        let directory = fd::openat(&root, id.as_str(), DIRECTORY, Mode::empty())?;
        let file = fd::openat(
            &directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            PRIVATE_FILE,
        )?;
        let mut output = File::from(file);
        let written = io::copy(&mut input.take(remaining + 1), &mut output).and_then(|size| {
            if size > remaining {
                Err(quota_error())
            } else {
                Ok(())
            }
        });
        if written.is_err() {
            let _ = fd::unlinkat(&directory, name, AtFlags::empty());
        }
        written
    })();
    if let Err(error) = result {
        let _ = fd::unlinkat(&root, id.as_str(), AtFlags::REMOVEDIR);
        return Err(error);
    }
    Ok(drops.join(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn modified_enter_survives_split_reads_and_only_keys_are_translated() {
        use super::super::keyboard::{SHIFT_ENTER, XTERM_SHIFT_ENTER};
        for input in [SHIFT_ENTER, b"\x1b[13;2:1u", XTERM_SHIFT_ENTER] {
            let expected = if input == XTERM_SHIFT_ENTER {
                SHIFT_ENTER
            } else {
                input
            };
            for split in 1..input.len() {
                let mut paste = Pastes::default();
                assert_eq!(paste.feed(&input[..split], None).await, (Vec::new(), false));
                if split > 2 {
                    assert!(!paste.waiting_for_key());
                }
                assert_eq!(
                    paste.feed(&input[split..], None).await,
                    (expected.to_vec(), false)
                );
            }
        }
        let framed = [START, XTERM_SHIFT_ENTER, END].concat();
        assert_eq!(Pastes::default().feed(&framed, None).await, (framed, false));
    }

    #[tokio::test]
    async fn paste_frames_can_split_without_changing_text_or_keys() {
        let root = tempfile::tempdir().unwrap();
        let drops = root.path().join("drops");
        let mut pastes = Pastes::default();
        let (output, detach) = pastes.feed(b"hi\x1b[20", Some(&drops)).await;
        assert_eq!(output, b"hi");
        assert!(!detach);
        let (output, detach) = pastes.feed(b"0~plain\x1d text\x1b[201", Some(&drops)).await;
        assert_eq!(output, START);
        assert!(!detach);
        let (output, detach) = pastes.feed(b"~bye\x1d", Some(&drops)).await;
        assert_eq!(output, b"plain\x1d text\x1b[201~bye");
        assert!(detach);
        assert!(!drops.exists());
    }

    #[test]
    fn the_shared_store_rejects_linked_drop_directories_and_full_storage() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let drops = root.path().join("drops");
        let name = OsStr::new("image.png");
        std::os::unix::fs::symlink(outside.path(), &drops).unwrap();
        assert!(store(&drops, name, &mut &b"data"[..]).is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        fs::remove_file(&drops).unwrap();

        let staged = store(&drops, name, &mut &b"data"[..]).unwrap().join(name);
        File::options()
            .write(true)
            .open(staged)
            .unwrap()
            .set_len(DROP_BYTES)
            .unwrap();
        let refused = store(&drops, name, &mut &b"more"[..]).unwrap_err();
        assert!(
            refused.to_string().contains("storage is full"),
            "{refused:?}"
        );
    }

    #[test]
    fn the_attachment_lock_unlocks_even_if_a_spawn_inherited_its_description() {
        let directory = tempfile::tempdir().unwrap();
        let root = File::open(directory.path()).unwrap();
        let lock = AttachmentLock::acquire(&root).unwrap();
        // A dup models the second reference a fork holds before exec.
        let inherited = rustix::io::dup(&lock.0).unwrap();
        assert!(AttachmentLock::acquire(&root).is_err());
        drop(lock);
        assert!(AttachmentLock::acquire(&root).is_ok());
        drop(inherited);
    }

    #[test]
    fn concurrent_writers_cannot_exceed_the_byte_or_file_budget() {
        use std::sync::mpsc::{Receiver, Sender, channel};
        use std::time::Duration;

        struct Gate {
            signals: Option<(Sender<()>, Receiver<()>)>,
            bytes: &'static [u8],
        }

        impl Read for Gate {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                if let Some((started, resume)) = self.signals.take() {
                    started.send(()).unwrap();
                    resume.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                self.bytes.read(buffer)
            }
        }

        let filename = OsStr::new("drop.txt");
        for byte_limit in [true, false] {
            let fixture = tempfile::tempdir().unwrap();
            let drops = fixture.path().join("drops");
            let initial = store(&drops, filename, &mut &b""[..]).unwrap();
            if byte_limit {
                // A sparse file leaves room for exactly one four-byte drop.
                File::options()
                    .write(true)
                    .open(initial.join(filename))
                    .unwrap()
                    .set_len(DROP_BYTES - 4)
                    .unwrap();
            } else {
                // Leave one free slot. Empty attachments still count.
                for i in 1..(DROP_FILES - 1) {
                    let path = drops.join(format!("fixture-{i}"));
                    fs::create_dir(&path).unwrap();
                    File::create(path.join(filename)).unwrap();
                }
            }

            std::thread::scope(|scope| {
                let (started, wait) = channel();
                let (resume, paused) = channel();
                let writer = scope.spawn(|| {
                    store(
                        &drops,
                        filename,
                        &mut Gate {
                            signals: Some((started, paused)),
                            bytes: b"data",
                        },
                    )
                });

                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                let competing = store(&drops, filename, &mut &b"more"[..]).unwrap_err();
                assert!(
                    competing.to_string().contains("another attachment"),
                    "{competing:?}"
                );
                resume.send(()).unwrap();
                let saved = writer.join().unwrap().unwrap();
                assert_eq!(fs::read(saved.join(filename)).unwrap(), b"data");
                let refused = store(&drops, filename, &mut &b"more"[..]).unwrap_err();
                assert!(
                    refused.to_string().contains("storage is full"),
                    "{refused:?}"
                );
            });
        }
    }

    #[test]
    fn filenames_and_shell_quoting_survive_copying_multiple_paths() {
        let root = tempfile::tempdir().unwrap();
        let drops = root.path().join("drops with spaces");
        let first = root.path().join("first image.png");
        let second = root.path().join("second image.png");
        fs::write(&first, b"first bytes").unwrap();
        fs::write(&second, b"second bytes").unwrap();
        let paste = format!(
            "don't edit  '{}'	{}/second\\ image.png  /no-such-senate-test/file",
            first.display(),
            root.path().display()
        );
        let output = rewrite(paste.as_bytes(), &drops);
        let output = std::str::from_utf8(&output).unwrap();
        let words = output.strip_prefix("don't edit  ").unwrap();
        let (first, rest) = words.split_once('\t').unwrap();
        let (second, rest) = rest.split_once("  ").unwrap();
        assert_eq!(rest, "/no-such-senate-test/file");
        assert!(first.ends_with("/first image.png'"));
        assert!(second.ends_with("/second\\ image.png"));
        let first = first
            .strip_prefix('\'')
            .unwrap()
            .strip_suffix('\'')
            .unwrap();
        assert_eq!(fs::read(first).unwrap(), b"first bytes");
        assert_eq!(
            fs::read(second.replace("\\ ", " ")).unwrap(),
            b"second bytes"
        );
    }
}
