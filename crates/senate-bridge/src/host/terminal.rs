//! The person attaches through their controlling terminal. Reads and writes
//! are cancellable so detaching never strands a blocking stdin reader.

use std::fs::File;
use std::io::{self, IsTerminal as _, Read as _, Write as _};
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt, Interest, unix::AsyncFd};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};

use super::emulation::{Colors, Scheme};
use super::{ATTACHED, ATTACHED_PLAIN, BUSY, COLORS, IO_CHUNK, KEYS, RESIZE};
use crate::paths::Paths;

// Ctrl-] in the legacy and Kitty keyboard encodings. Kitty can also
// include the event type when the terminal reports key presses separately.
const DETACH: &[&[u8]] = &[b"\x1d", b"\x1b[93;5u", b"\x1b[93;5:1u"];
const RESET_DISPLAY: &[u8] =
    b"\x1b[0m\x1b[?2004l\x1b[?1004l\x1b[?2031l\x1b[?1000;1001;1002;1003;1005;1006;1015;1016l\x1b[?25h\x1b[?1049l";

fn split_input(bytes: &[u8]) -> (usize, bool) {
    if let Some(detach) = DETACH
        .iter()
        .filter_map(|key| bytes.windows(key.len()).position(|w| w == *key))
        .min()
    {
        return (detach, true);
    }
    // A control sequence can straddle two reads. Keep only the suffix
    // that might complete the detach key on the next read.
    let max_prefix = DETACH
        .iter()
        .map(|key| key.len() - 1)
        .max()
        .unwrap_or(0)
        .min(bytes.len());
    let suffix = (1..=max_prefix)
        .rev()
        .find(|&len| {
            DETACH
                .iter()
                .any(|key| key.starts_with(&bytes[bytes.len() - len..]))
        })
        .unwrap_or(0);
    (bytes.len() - suffix, false)
}

// Foreground, background and light-or-dark, then primary device attributes.
// Every terminal answers the last one, so its answer ends the wait whether
// or not the terminal knew the others.
const ASK_COLORS: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n\x1b[c";

/// The length of the control sequence at the start of `bytes` (an ESC) if
/// it is a terminal's report, and what it reports when that is a colour.
fn report(bytes: &[u8], colors: &mut Colors) -> Option<usize> {
    match *bytes.get(1)? {
        b']' => {
            // A colour answer is short. Without a nearby end this is a key
            // the person pressed (Alt+]), not the start of an answer.
            let (len, end) = bytes[2..]
                .iter()
                .enumerate()
                .take(48)
                .find_map(|(i, b)| match b {
                    0x07 => Some((i, i + 1)),
                    0x1b if bytes.get(2 + i + 1) == Some(&b'\\') => Some((i, i + 2)),
                    0x1b => Some((0, 0)),
                    _ => None,
                })
                .filter(|(_, end)| *end != 0)?;
            let body = std::str::from_utf8(&bytes[2..2 + len]).ok();
            match body.and_then(|body| body.split_once(';')) {
                Some(("10", spec)) => colors.foreground = Some(spec.to_owned()),
                Some(("11", spec)) => colors.background = Some(spec.to_owned()),
                _ => {}
            }
            Some(2 + end)
        }
        b'[' if bytes.get(2) == Some(&b'?') => {
            let params = bytes[3..]
                .iter()
                .take_while(|b| b.is_ascii_digit() || **b == b';')
                .count();
            match (&bytes[3..3 + params], *bytes.get(3 + params)?) {
                (b"997;1", b'n') => colors.scheme = Some(Scheme::Dark),
                (b"997;2", b'n') => colors.scheme = Some(Scheme::Light),
                (_, b'c' | b'n') => {}
                _ => return None,
            }
            Some(3 + params + 1)
        }
        _ => None,
    }
}

/// Split what arrived after [`ASK_COLORS`] into the terminal's answers and
/// anything the person typed meanwhile. The flag says whether the device
/// attributes, the last answer, are in.
fn read_colors(bytes: &[u8]) -> (Colors, Vec<u8>, bool) {
    let mut colors = Colors::default();
    let mut typed = Vec::new();
    let mut complete = false;
    let mut at = 0;
    while at < bytes.len() {
        let known = (bytes[at] == 0x1b)
            .then(|| report(&bytes[at..], &mut colors))
            .flatten();
        match known {
            Some(len) => {
                complete |= bytes[at + len - 1] == b'c';
                at += len;
            }
            None => {
                typed.push(bytes[at]);
                at += 1;
            }
        }
    }
    (colors.checked(), typed, complete)
}

async fn send_keys(out: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> anyhow::Result<()> {
    for chunk in bytes.chunks(IO_CHUNK) {
        out.write_u8(KEYS).await?;
        out.write_u16(u16::try_from(chunk.len())?).await?;
        out.write_all(chunk).await?;
    }
    Ok(())
}

struct RawTerminal {
    input: AsyncFd<NonblockingFile>,
    output: AsyncFd<NonblockingFile>,
    original: Termios,
}

struct NonblockingFile {
    file: File,
    original: OFlags,
}

impl NonblockingFile {
    fn new(file: File) -> io::Result<Self> {
        let original = fcntl_getfl(&file)?;
        Ok(Self { file, original })
    }

    fn register(self, interest: Interest) -> io::Result<AsyncFd<Self>> {
        fcntl_setfl(&self.file, self.original | OFlags::NONBLOCK)?;
        // File owns a stable descriptor and this module never replaces it.
        // AsyncFd's generic replacement requires an unsafe call.
        #[allow(deprecated)]
        AsyncFd::with_interest(self, interest)
    }
}

impl AsRawFd for NonblockingFile {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl Drop for NonblockingFile {
    fn drop(&mut self) {
        let _ = fcntl_setfl(&self.file, self.original);
    }
}

impl RawTerminal {
    async fn open(alternate_screen: bool) -> anyhow::Result<Self> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        ensure!(
            stdin.is_terminal() && stdout.is_terminal(),
            "attach needs a terminal on stdin and stdout"
        );
        // Read both sets of flags before changing either: stdin and stdout
        // can refer to the same open file description.
        let input = NonblockingFile::new(File::from(rustix::io::dup(stdin)?))?;
        let output = NonblockingFile::new(File::from(rustix::io::dup(stdout)?))?;
        let input = input.register(Interest::READABLE)?;
        let output = output.register(Interest::WRITABLE)?;
        let original = tcgetattr(&input.get_ref().file)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(&input.get_ref().file, OptionalActions::Now, &raw)?;

        let terminal = Self {
            input,
            output,
            original,
        };
        if alternate_screen {
            terminal.draw(b"\x1b[?1049h\x1b[2J\x1b[H").await?;
        }
        Ok(terminal)
    }

    /// What this terminal says its colours are, and what the person typed
    /// while it was being asked.
    async fn colors(&self) -> (Colors, Vec<u8>) {
        let mut answers = Vec::new();
        if self.draw(ASK_COLORS).await.is_ok() {
            let mut chunk = [0u8; 256];
            let read = async {
                while let Ok(n @ 1..) = self
                    .input
                    .async_io(Interest::READABLE, |fd| (&fd.file).read(&mut chunk))
                    .await
                {
                    answers.extend_from_slice(&chunk[..n]);
                    if read_colors(&answers).2 {
                        break;
                    }
                }
            };
            // A terminal that answers nothing at all must not hold us up.
            let _ = tokio::time::timeout(Duration::from_millis(300), read).await;
        }
        let (colors, typed, _) = read_colors(&answers);
        (colors, typed)
    }

    async fn draw(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let written = self
                .output
                .async_io(Interest::WRITABLE, |fd| (&fd.file).write(bytes))
                .await?;
            if written == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[written..];
        }
        Ok(())
    }

    async fn size(&self, out: &mut (impl AsyncWrite + Unpin)) -> anyhow::Result<()> {
        let size = tcgetwinsize(&self.input.get_ref().file)?;
        ensure!(
            size.ws_row > 0 && size.ws_col > 0,
            "cannot read terminal size"
        );
        out.write_u8(RESIZE).await?;
        out.write_u16(size.ws_row).await?;
        out.write_u16(size.ws_col).await?;
        Ok(())
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = tcsetattr(
            &self.input.get_ref().file,
            OptionalActions::Now,
            &self.original,
        );
    }
}

/// Record what the calling terminal looks like, for a host that starts with
/// nobody attached. Does nothing when not run from a terminal.
pub async fn remember_terminal(paths: &Paths) {
    let stdin = io::stdin();
    // Leave the terminal alone unless this is a person's foreground command
    // on a terminal that can answer: changing its modes from the background
    // would stop the job.
    let foreground =
        rustix::termios::tcgetpgrp(&stdin).is_ok_and(|group| group == rustix::process::getpgrp());
    if !foreground || std::env::var_os("TERM").is_some_and(|term| term == "dumb") {
        return;
    }
    let Ok(terminal) = RawTerminal::open(false).await else {
        return;
    };
    // Anything already typed or pasted belongs to the shell. Reading the
    // answers would take it too, so do without them this once.
    if rustix::io::ioctl_fionread(&terminal.input.get_ref().file).unwrap_or(1) != 0 {
        return;
    }
    let (colors, _) = terminal.colors().await;
    drop(terminal);
    if let Err(error) = colors.save(paths) {
        tracing::debug!(%error, "recording the terminal's colours");
    }
}

/// Attach until Ctrl-], the person closes the terminal, or the host exits.
pub async fn attach(path: &Path, paths: &Paths) -> anyhow::Result<()> {
    ensure!(
        crate::testenv::confined(path),
        "terminal socket is outside the test directory"
    );
    let mut stream = UnixStream::connect(path)
        .await
        .context("connect to the hosted terminal")?;
    let takes_colors =
        match tokio::time::timeout(Duration::from_secs(3), stream.read_u8()).await?? {
            ATTACHED => true,
            ATTACHED_PLAIN => false,
            BUSY => bail!("the senator already has an attached terminal"),
            _ => bail!("unrecognized terminal host reply"),
        };

    let mut winch = signal(SignalKind::window_change())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let terminal = RawTerminal::open(true).await?;
    let (colors, typed) = terminal.colors().await;
    if let Err(error) = colors.save(paths) {
        tracing::debug!(%error, "recording the terminal's colours");
    }
    let (mut screen, mut keys) = stream.into_split();

    let to_host = async {
        terminal.size(&mut keys).await?;
        if takes_colors {
            let colors = serde_json::to_vec(&colors)?;
            keys.write_u8(COLORS).await?;
            keys.write_u16(u16::try_from(colors.len())?).await?;
            keys.write_all(&colors).await?;
        }
        let mut input = [0u8; IO_CHUNK];
        let mut pending = typed;
        loop {
            let n = tokio::select! {
                n = terminal.input.async_io(Interest::READABLE, |fd| (&fd.file).read(&mut input)) => n?,
                _ = tokio::time::sleep(Duration::from_millis(35)), if !pending.is_empty() => {
                    send_keys(&mut keys, &pending).await?;
                    pending.clear();
                    continue;
                }
                _ = winch.recv() => {
                    terminal.size(&mut keys).await?;
                    continue;
                }
            };
            if n == 0 {
                return Ok(());
            }
            pending.extend_from_slice(&input[..n]);
            let (send, detach) = split_input(&pending);
            send_keys(&mut keys, &pending[..send]).await?;
            if detach {
                return Ok(());
            }
            pending.drain(..send);
        }
    };
    let to_screen = async {
        let mut bytes = [0; IO_CHUNK];
        loop {
            let n = screen.read(&mut bytes).await?;
            if n == 0 {
                return Ok(());
            }
            terminal.draw(&bytes[..n]).await?;
        }
    };
    let result = tokio::select! {
        result = to_host => result,
        result = to_screen => result,
        _ = hup.recv() => Ok(()),
        _ = term.recv() => Ok(()),
        _ = interrupt.recv() => Ok(()),
    };
    // draw handles partial writes and waits for writable readiness. Bound
    // cleanup in case the attached terminal has disappeared.
    let _ = tokio::time::timeout(Duration::from_millis(250), terminal.draw(RESET_DISPLAY)).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminals_answers_are_told_apart_from_what_was_typed() {
        // Ghostty, light theme, with a key pressed while it was being asked.
        let (colors, typed, complete) = read_colors(
            b"\x1b]10;rgb:4c4c/4f4f/6969\x1b\\q\x1b]11;rgb:efef/f1f1/f5f5\x07\x1b[?997;2n\x1b[A\x1b[?62;22c",
        );
        assert_eq!(colors.foreground.as_deref(), Some("rgb:4c4c/4f4f/6969"));
        assert_eq!(colors.background.as_deref(), Some("rgb:efef/f1f1/f5f5"));
        assert_eq!(colors.scheme, Some(Scheme::Light));
        assert_eq!(typed, b"q\x1b[A");
        assert!(complete);

        // A terminal that knows none of the colour questions still ends the wait.
        let (colors, typed, complete) = read_colors(b"\x1b[?1;2c");
        assert_eq!(colors, Colors::default());
        assert!(typed.is_empty() && complete);

        // Alt+] is a key, even with an answer right behind it.
        let (colors, typed, _) = read_colors(b"\x1b]\x1b]10;rgb:4c4c/4f4f/6969\x1b\\\x1b[?62c");
        assert_eq!(colors.foreground.as_deref(), Some("rgb:4c4c/4f4f/6969"));
        assert_eq!(typed, b"\x1b]");

        // Cut short: nothing is claimed and the wait goes on.
        let (colors, _, complete) = read_colors(b"\x1b]11;rgb:1e1e/1e1e/2e2e");
        assert_eq!(colors, Colors::default());
        assert!(!complete);
    }

    #[test]
    fn detach_recognizes_legacy_and_kitty_keys_without_consuming_escape() {
        assert_eq!(split_input(b"hello\x1d"), (5, true));
        assert_eq!(split_input(b"hello\x1b[93;5u"), (5, true));
        assert_eq!(split_input(b"\x1b[93;5:1u"), (0, true));
        assert_eq!(split_input(b"hello\x1b[93;"), (5, false));
        assert_eq!(split_input(b"\x1b[D"), (3, false));
    }
}
