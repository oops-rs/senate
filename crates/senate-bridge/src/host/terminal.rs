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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, Interest, unix::AsyncFd};
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal};

use super::emulation::{Colors, Scheme};
use super::keyboard::{self, MODIFIED_ENTER};
use super::paste::Pastes;
use super::{ATTACHED, ATTACHED_NATIVE, ATTACHED_PLAIN, BUSY, COLORS, IO_CHUNK, KEYS, RESIZE};
use crate::paths::Paths;

// Ctrl-] in the legacy and Kitty keyboard encodings. Kitty can also
// include the event type when the terminal reports key presses separately.
const DETACH: &[&[u8]] = &[b"\x1d", b"\x1b[93;5u", b"\x1b[93;5:1u"];
pub(super) const RESET_DISPLAY: &[u8] =
    b"\x1b[0m\x1b[?2004l\x1b[?1004l\x1b[?2031l\x1b[?1000;1001;1002;1003;1005;1006;1015;1016l\x1b[?25h\x1b[?1049l";

// Leave the shell at the bottom of the primary screen. Exit the TUI's
// alternate screen first, then reset cursor, margins and input modes there.
// End a synchronized update even when a dropped socket cut it in half.
const RESET_NATIVE_DISPLAY: &[u8] = b"\x1b[?2026l\x1b[?1049;1047;47l\x1b[?6;1l\x1b[r\x1b>\x1b[?7h\x1b[0 q\x1b[0m\x1b[?2004;1004;2031;9;1000;1001;1002;1003;1005;1006;1015;1016l\x1b[?25h\x1b[9999;1H\r\n";

fn attach_separator(label: &str, width: u16) -> String {
    let time = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z");
    let suffix = format!(" · {time} ──");
    let width = usize::from(width);
    let label_width = width.saturating_sub(3 + textwrap::core::display_width(&suffix));
    let label = super::picker::clip(label, label_width);
    let separator = super::picker::clip(&format!("── {label}{suffix}"), width);
    format!("\r\n\x1b[2m{separator}\x1b[0m\r\n")
}

pub(super) fn split_input(bytes: &[u8]) -> (usize, bool) {
    if let Some(detach) = DETACH
        .iter()
        .filter_map(|key| bytes.windows(key.len()).position(|w| w == *key))
        .min()
    {
        return (detach, true);
    }
    // Keep the prefix of a detach or modified Enter if it spans reads.
    let sequences = || DETACH.iter().chain(MODIFIED_ENTER);
    let max_prefix = sequences()
        .map(|key| key.len() - 1)
        .max()
        .unwrap_or(0)
        .min(bytes.len());
    let suffix = (1..=max_prefix)
        .rev()
        .find(|&len| {
            sequences().any(|key| key.len() > len && key.starts_with(&bytes[bytes.len() - len..]))
        })
        .unwrap_or(0);
    (bytes.len() - suffix, false)
}

// Foreground, background, light-or-dark and keyboard flags, then DA.
// Every terminal answers the last one, so its answer ends the wait whether
// or not the terminal knew the others.
const ASK_COLORS: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[?996n\x1b[?u\x1b[c";

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
                (b"997;1", b'n') => {
                    colors.scheme = Some(Scheme::Dark);
                    colors.scheme_reporting = true;
                }
                (b"997;2", b'n') => {
                    colors.scheme = Some(Scheme::Light);
                    colors.scheme_reporting = true;
                }
                (flags, b'u') if !flags.is_empty() && flags.iter().all(u8::is_ascii_digit) => {
                    colors.keyboard_flags = Some(std::str::from_utf8(flags).ok()?.parse().ok()?);
                }
                (attributes, b'c') if !attributes.is_empty() => colors.device_attributes = true,
                (_, b'n') => {}
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

pub(super) struct RawTerminal {
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
    pub(super) async fn open(alternate_screen: bool) -> anyhow::Result<Self> {
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

    /// Read what was typed into `buf`; 0 when the terminal is gone.
    pub(super) async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.input
            .async_io(Interest::READABLE, |fd| (&fd.file).read(buf))
            .await
    }

    /// The terminal's size, rows then columns.
    pub(super) fn window(&self) -> anyhow::Result<(u16, u16)> {
        let size = tcgetwinsize(&self.input.get_ref().file)?;
        ensure!(
            size.ws_row > 0 && size.ws_col > 0,
            "cannot read terminal size"
        );
        Ok((size.ws_row, size.ws_col))
    }

    pub(super) async fn draw(&self, mut bytes: &[u8]) -> io::Result<()> {
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

/// The host refused the attachment: another terminal holds it.
#[derive(Debug)]
pub struct Busy;

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the senator already has an attached terminal")
    }
}

impl std::error::Error for Busy {}

/// The path of the terminal on stdin (`/dev/ttys012`), when it is one.
fn own_tty() -> Option<String> {
    rustix::termios::ttyname(std::io::stdin(), Vec::new())
        .ok()
        .and_then(|name| name.into_string().ok())
}

/// Attach until Ctrl-], the person closes the terminal, or the host exits.
pub async fn attach(path: &Path, paths: &Paths, label: &str) -> anyhow::Result<()> {
    ensure!(
        crate::testenv::confined(path),
        "terminal socket is outside the test directory"
    );
    let drops = super::paste::drop_directory(paths, path).ok();
    let stream = tokio::time::timeout(ATTACH_STARTUP_TIMEOUT, UnixStream::connect(path))
        .await
        .with_context(|| format!("connecting to {label}'s terminal host timed out"))?
        .with_context(|| format!("connect to {label}'s terminal host"))?;
    drive(stream, label, drops, UnixStream::into_split).await
}

pub(crate) async fn attach_stream(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    label: &str,
    drops: Option<std::path::PathBuf>,
) -> anyhow::Result<()> {
    drive(stream, label, drops, tokio::io::split).await
}

const ATTACH_STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

async fn drive<S, R, W>(
    mut stream: S,
    label: &str,
    drops: Option<std::path::PathBuf>,
    split: impl FnOnce(S) -> (R, W),
) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (takes_colors, native) =
        match tokio::time::timeout(ATTACH_STARTUP_TIMEOUT, stream.read_u8())
            .await
            .with_context(|| format!("{label}'s terminal host did not answer in time"))?
            .with_context(|| format!("read the reply from {label}'s terminal host"))?
        {
            ATTACHED_NATIVE => (true, true),
            ATTACHED => (true, false),
            ATTACHED_PLAIN => (false, false),
            BUSY => return Err(Busy.into()),
            _ => bail!("unrecognized terminal host reply"),
        };

    let mut winch = signal(SignalKind::window_change())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let terminal = RawTerminal::open(!native).await?;
    if native {
        let (_, cols) = terminal.window()?;
        terminal
            .draw(attach_separator(label, cols).as_bytes())
            .await?;
    }
    let (mut colors, typed) = terminal.colors().await;
    colors.tty = own_tty();
    // The terminal's title names the session while it is attached.
    let _ = terminal.draw(&super::title::set(label)).await;
    let (mut screen, mut keys) = split(stream);

    let to_host = async {
        if native {
            terminal.size(&mut keys).await?;
        }
        if takes_colors {
            let colors = serde_json::to_vec(&colors)?;
            keys.write_u8(COLORS).await?;
            keys.write_u16(u16::try_from(colors.len())?).await?;
            keys.write_all(&colors).await?;
        }
        if !native {
            terminal.size(&mut keys).await?;
        }
        let mut input = [0u8; IO_CHUNK];
        let mut pastes = Pastes::default();
        let (bytes, detaching) = pastes.feed(&typed, drops.as_deref()).await;
        send_keys(&mut keys, &bytes).await?;
        if detaching {
            return Ok(());
        }
        loop {
            let n = tokio::select! {
                n = terminal.read(&mut input) => n?,
                _ = tokio::time::sleep(Duration::from_millis(35)), if pastes.waiting_for_key() => {
                    send_keys(&mut keys, &pastes.flush_key()).await?;
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
            let (bytes, detaching) = pastes.feed(&input[..n], drops.as_deref()).await;
            send_keys(&mut keys, &bytes).await?;
            if detaching {
                return Ok(());
            }
        }
    };
    let to_screen = async {
        let mut bytes = [0; IO_CHUNK];
        // A native host always sends a replay after the initial resize.
        // Legacy relays may have no screen to send until the TUI paints.
        if native {
            let n = tokio::time::timeout(ATTACH_STARTUP_TIMEOUT, screen.read(&mut bytes))
                .await
                .with_context(|| {
                    format!("{label}'s terminal host did not send its initial screen in time")
                })??;
            ensure!(
                n != 0,
                "{label}'s terminal host closed before sending its initial screen"
            );
            terminal.draw(&bytes[..n]).await?;
        }
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
    let mut reset = reset_display(native, colors.keyboard_flags);
    reset.extend_from_slice(super::title::RESTORE);
    let _ = tokio::time::timeout(Duration::from_millis(250), terminal.draw(&reset)).await;
    result
}

fn reset_display(native: bool, inherited_keyboard_flags: Option<u16>) -> Vec<u8> {
    let mut reset = if native {
        RESET_NATIVE_DISPLAY
    } else {
        RESET_DISPLAY
    }
    .to_vec();
    // Native attach probes the person's screen before the TUI paints.
    // The display reset returns to that screen before restoring its flags.
    if native && let Some(flags) = inherited_keyboard_flags {
        keyboard::set_flags(&mut reset, flags);
    }
    reset.extend_from_slice(keyboard::RESET_MODIFY_OTHER_KEYS);
    reset
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_probe_is_not_input_and_detach_restores_inherited_flags() {
        let (colors, typed, done) = read_colors(b"\x1b[?9u\x1b[13;2u\x1b[?62;22c");
        assert!(done);
        assert_eq!(colors.keyboard_flags, Some(9));
        assert_eq!(typed, b"\x1b[13;2u");
        assert!(reset_display(true, colors.keyboard_flags).ends_with(b"\x1b[=9u\x1b[>4;0m"));
        assert!(reset_display(false, Some(9)).ends_with(b"\x1b[>4;0m"));
    }

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
        assert!(complete && colors.device_attributes && colors.scheme_reporting);

        // A terminal that knows none of the colour questions still ends the wait.
        let (colors, typed, complete) = read_colors(b"\x1b[?1;2c");
        assert_eq!(
            colors,
            Colors {
                device_attributes: true,
                ..Colors::default()
            }
        );
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
