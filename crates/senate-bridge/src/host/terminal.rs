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

use super::{ATTACHED, BUSY, IO_CHUNK, KEYS, RESIZE};

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
    async fn open() -> anyhow::Result<Self> {
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
        terminal.draw(b"\x1b[?1049h\x1b[2J\x1b[H").await?;
        Ok(terminal)
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

/// Attach until Ctrl-], the person closes the terminal, or the host exits.
pub async fn attach(path: &Path) -> anyhow::Result<()> {
    ensure!(
        crate::testenv::confined(path),
        "terminal socket is outside the test directory"
    );
    let mut stream = UnixStream::connect(path)
        .await
        .context("connect to the hosted terminal")?;
    match tokio::time::timeout(Duration::from_secs(3), stream.read_u8()).await?? {
        ATTACHED => {}
        BUSY => bail!("the senator already has an attached terminal"),
        _ => bail!("unrecognized terminal host reply"),
    }

    let mut winch = signal(SignalKind::window_change())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let terminal = RawTerminal::open().await?;
    let (mut screen, mut keys) = stream.into_split();

    let to_host = async {
        terminal.size(&mut keys).await?;
        let mut input = [0u8; IO_CHUNK];
        let mut pending = Vec::with_capacity(IO_CHUNK + 10);
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
    fn detach_recognizes_legacy_and_kitty_keys_without_consuming_escape() {
        assert_eq!(split_input(b"hello\x1d"), (5, true));
        assert_eq!(split_input(b"hello\x1b[93;5u"), (5, true));
        assert_eq!(split_input(b"\x1b[93;5:1u"), (0, true));
        assert_eq!(split_input(b"hello\x1b[93;"), (5, false));
        assert_eq!(split_input(b"\x1b[D"), (3, false));
    }
}
