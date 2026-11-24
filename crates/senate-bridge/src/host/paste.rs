//! Keep files dropped into the viewer available to the hosted process.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use rustix::fs::{Mode, OFlags};
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

fn copy(source: &Path, drops: &Path) -> io::Result<PathBuf> {
    let name = source.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    // Nonblocking open avoids waiting if a pasted path is a named pipe.
    let mut input = File::from(rustix::fs::open(
        source,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    if !input.metadata()?.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let private = || {
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    match private().create(drops) {
        Ok(()) => {}
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(drops)?.is_dir() => {}
        Err(error) => return Err(error),
    }
    let directory = drops.join(EventId::new().to_string());
    private().create(&directory)?;
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join(name))
        .and_then(|mut output| io::copy(&mut input, &mut output));
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&directory);
        return Err(error);
    }
    Ok(directory)
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
