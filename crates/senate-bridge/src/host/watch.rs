//! Watching a hosted terminal without touching it.
//!
//! An attached viewer ([`super::attach`]) drives the session: it types, it
//! sets the terminal's size, and there is one of it. A watcher only looks.
//! Any number of them connect to the host's second socket (`see`, beside
//! `tty`), alongside the viewer or with nobody attached, and nothing a
//! watcher does reaches the session: the host never reads from one (what a
//! watcher sends is dropped), never resizes for one, and never records one
//! as activity of the viewer, so a watcher can never count as someone
//! having typed.
//!
//! # The screen
//!
//! The session's TUI paints its screen once and then changes parts of it,
//! so the bytes it writes mean nothing to someone who was not there from
//! the start. The host therefore keeps the screen itself: every byte the
//! TUI writes goes through a terminal emulator ([`vt100`]) held at the
//! PTY's size. That is all it keeps: the visible screen, no scrollback, so
//! one host holds one grid of cells (rows × columns, a few dozen bytes a
//! cell: about half a megabyte for a 50 × 200 terminal) however long the
//! session runs.
//!
//! A watcher is first told the size and sent the screen as it is, then
//! every change in order: the TUI's output, and each new size. The two are
//! taken under one lock, so nothing falls between the screen it was sent
//! and the first change after it. A watcher that falls behind is not sent
//! what it missed; it is sent the screen again, as it is now.
//!
//! # The stream
//!
//! The host greets with [`WATCHING`], then sends frames, each a tag byte
//! and its payload, integers big-endian:
//!
//! | tag | payload | meaning |
//! | --- | --- | --- |
//! | [`SIZE`] | rows `u16`, columns `u16` | the session's terminal is this size now |
//! | [`SCREEN`] | length `u32`, bytes | forget what was shown: these escape codes paint the whole screen |
//! | [`OUTPUT`] | length `u16`, bytes | what the TUI wrote next |
//!
//! The watcher holds its own emulator at the size it was told
//! ([`Mirror`]), and shows as much of that screen as it has room for.

use std::num::NonZeroU16;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::JoinSet;

use super::IO_CHUNK;

/// The host's greeting on the watch socket.
const WATCHING: u8 = 1;
const SIZE: u8 = 1;
const OUTPUT: u8 = 2;
const SCREEN: u8 = 3;
/// The most a host is believed about its screen: a watcher reads the
/// length from the socket before the bytes.
const MAX_SCREEN: usize = 8 << 20;
/// Changes a watcher may fall behind by before it is sent the screen
/// again instead.
const BACKLOG: usize = 256;

/// The file name of a host's watch socket, in its directory. As long as
/// `tty`, whose path is the one checked against the socket path limit
/// before a host starts.
pub(crate) const SOCKET: &str = "see";

fn dimension(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

#[derive(Clone)]
enum Change {
    Size(u16, u16),
    Output(Arc<[u8]>),
}

/// The session's screen as the host keeps it, and those watching it.
pub(super) struct Screen {
    parser: Mutex<vt100::Parser>,
    changes: broadcast::Sender<Change>,
}

impl Screen {
    pub(super) fn new(rows: u16, cols: u16) -> Self {
        Self {
            // No scrollback: the visible screen is all a watcher is shown.
            parser: Mutex::new(vt100::Parser::new(dimension(rows), dimension(cols), 0)),
            changes: broadcast::channel(BACKLOG).0,
        }
    }

    fn parser(&self) -> MutexGuard<'_, vt100::Parser> {
        self.parser
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// The TUI wrote `bytes`.
    pub(super) fn output(&self, bytes: &[u8]) {
        let mut parser = self.parser();
        parser.process(bytes);
        // Nobody watching is not an error.
        let _ = self.changes.send(Change::Output(bytes.into()));
    }

    /// The PTY is `rows` × `cols` now.
    pub(super) fn resize(&self, rows: u16, cols: u16) {
        let mut parser = self.parser();
        parser.set_size(dimension(rows), dimension(cols));
        let _ = self.changes.send(Change::Size(rows, cols));
    }

    /// The size and the screen as they are, and every change after them.
    fn join(&self) -> (u16, u16, Vec<u8>, broadcast::Receiver<Change>) {
        let parser = self.parser();
        let (rows, cols) = parser.screen().size();
        let state = parser.screen().state_formatted().into_bytes();
        (rows.get(), cols.get(), state, self.changes.subscribe())
    }
}

/// Serve everyone who watches, for as long as the host runs.
pub(super) async fn serve(listener: &UnixListener, screen: &Arc<Screen>) -> anyhow::Result<()> {
    let mut watchers = JoinSet::new();
    loop {
        tokio::select! {
            (stream, _) = super::accept(listener) => {
                let screen = screen.clone();
                watchers.spawn(async move {
                    if let Err(error) = watch(stream, &screen).await {
                        tracing::debug!(%error, "a terminal watcher left");
                    }
                });
            }
            Some(_) = watchers.join_next() => {}
        }
    }
}

async fn send_screen(
    to: &mut (impl AsyncWrite + Unpin),
    screen: &Screen,
) -> anyhow::Result<broadcast::Receiver<Change>> {
    let (rows, cols, state, changes) = screen.join();
    to.write_u8(SIZE).await?;
    to.write_u16(rows).await?;
    to.write_u16(cols).await?;
    to.write_u8(SCREEN).await?;
    to.write_u32(u32::try_from(state.len())?).await?;
    to.write_all(&state).await?;
    Ok(changes)
}

async fn watch(stream: UnixStream, screen: &Screen) -> anyhow::Result<()> {
    let (mut from, mut to) = stream.into_split();
    to.write_u8(WATCHING).await?;
    let mut changes = send_screen(&mut to, screen).await?;
    let mut said = [0u8; 64];
    loop {
        tokio::select! {
            // A watcher has nothing to say: what it sends is dropped, and
            // reading only tells when it is gone.
            n = from.read(&mut said) => {
                if n? == 0 {
                    return Ok(());
                }
            }
            change = changes.recv() => match change {
                Ok(Change::Size(rows, cols)) => {
                    to.write_u8(SIZE).await?;
                    to.write_u16(rows).await?;
                    to.write_u16(cols).await?;
                }
                Ok(Change::Output(bytes)) => {
                    for chunk in bytes.chunks(IO_CHUNK) {
                        to.write_u8(OUTPUT).await?;
                        to.write_u16(u16::try_from(chunk.len())?).await?;
                        to.write_all(chunk).await?;
                    }
                }
                // Too far behind to be sent what it missed: the screen as
                // it is now, and on from there.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    changes = send_screen(&mut to, screen).await?;
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

/// One thing a host says to a watcher.
pub(crate) enum Frame {
    Size(u16, u16),
    Screen(Vec<u8>),
    Output(Vec<u8>),
}

/// A watcher's copy of a session's screen, at the session's own size.
pub(crate) struct Mirror {
    parser: vt100::Parser,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            parser: vt100::Parser::new(dimension(24), dimension(80), 0),
        }
    }
}

impl Mirror {
    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// The session's size, rows then columns.
    #[cfg(test)]
    pub(crate) fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser.screen().size();
        (rows.get(), cols.get())
    }

    /// Take in what the host said.
    pub(crate) fn apply(&mut self, frame: &Frame) {
        match frame {
            Frame::Size(rows, cols) => self.parser.set_size(dimension(*rows), dimension(*cols)),
            Frame::Screen(state) => {
                // Forget what was shown: the screen starts from this.
                let (rows, cols) = self.parser.screen().size();
                self.parser = vt100::Parser::new(rows, cols, 0);
                self.parser.process(state);
            }
            Frame::Output(bytes) => self.parser.process(bytes),
        }
    }
}

/// An open watch of one hosted terminal.
pub(crate) struct Watch {
    from: tokio::net::unix::OwnedReadHalf,
    // Closing this half would tell the host the watcher is gone.
    _to: tokio::net::unix::OwnedWriteHalf,
}

impl Watch {
    /// Start watching the host whose watch socket is `socket`.
    pub(crate) async fn open(socket: &Path) -> anyhow::Result<Self> {
        ensure!(
            crate::testenv::confined(socket),
            "watch socket is outside the test directory"
        );
        let mut stream = UnixStream::connect(socket)
            .await
            .context("connect to the hosted terminal's watch socket")?;
        match tokio::time::timeout(Duration::from_secs(3), stream.read_u8()).await?? {
            WATCHING => {}
            _ => bail!("unrecognized terminal host reply"),
        }
        let (from, to) = stream.into_split();
        Ok(Self { from, _to: to })
    }

    /// The next thing the host says; `None` when the host is gone (the
    /// session ended).
    pub(crate) async fn next(&mut self) -> anyhow::Result<Option<Frame>> {
        read_frame(&mut self.from).await
    }
}

async fn read_frame(from: &mut (impl AsyncRead + Unpin)) -> anyhow::Result<Option<Frame>> {
    let tag = match from.read_u8().await {
        Ok(tag) => tag,
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    Ok(Some(match tag {
        SIZE => Frame::Size(from.read_u16().await?, from.read_u16().await?),
        SCREEN => {
            let n = usize::try_from(from.read_u32().await?)?;
            ensure!(n <= MAX_SCREEN, "oversized terminal screen");
            let mut state = vec![0; n];
            from.read_exact(&mut state).await?;
            Frame::Screen(state)
        }
        OUTPUT => {
            let n = usize::from(from.read_u16().await?);
            ensure!(n <= IO_CHUNK, "oversized terminal output");
            let mut bytes = vec![0; n];
            from.read_exact(&mut bytes).await?;
            Frame::Output(bytes)
        }
        _ => bail!("unknown terminal watch frame"),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read frames until the mirror shows `text`.
    async fn until(from: &mut UnixStream, mirror: &mut Mirror, text: &str) {
        let wait = async {
            while !mirror.screen().contents().contains(text) {
                let frame = read_frame(from).await.unwrap().expect("the host left");
                mirror.apply(&frame);
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("never showed {text:?}"));
    }

    #[tokio::test]
    async fn a_watcher_is_sent_the_screen_as_it_is_then_every_change() {
        let screen = Arc::new(Screen::new(5, 20));
        // Painted before anyone watched, in colour, the cursor left mid-line.
        screen.output(b"\x1b[2J\x1b[H\x1b[31mready\x1b[m\r\n> ");
        let (host, mut watcher) = UnixStream::pair().unwrap();
        let serving = tokio::spawn({
            let screen = screen.clone();
            async move { watch(host, &screen).await }
        });
        assert_eq!(watcher.read_u8().await.unwrap(), WATCHING);
        let mut mirror = Mirror::default();
        until(&mut watcher, &mut mirror, "ready").await;
        assert_eq!(mirror.size(), (5, 20));
        assert_eq!(mirror.screen().contents(), "ready\n> ");
        assert_eq!(mirror.screen().cursor_position(), (1, 2));
        let cell = mirror.screen().cell(0, 0).unwrap();
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(1), "colours come too");

        // What the TUI writes next, and a new size.
        screen.output(b"hello");
        until(&mut watcher, &mut mirror, "> hello").await;
        screen.resize(8, 40);
        screen.output(b"\r\nwider");
        until(&mut watcher, &mut mirror, "wider").await;
        assert_eq!(mirror.size(), (8, 40));
        assert_eq!(mirror.screen().contents(), "ready\n> hello\nwider");

        // What a watcher sends goes nowhere, and leaving ends its service.
        watcher.write_all(b"rm -rf /\r").await.unwrap();
        screen.output(b"!");
        until(&mut watcher, &mut mirror, "wider!").await;
        drop(watcher);
        serving.await.unwrap().unwrap();
        assert_eq!(
            screen.parser().screen().contents(),
            "ready\n> hello\nwider!"
        );
    }

    #[tokio::test]
    async fn a_watcher_that_fell_behind_is_sent_the_screen_again() {
        let screen = Arc::new(Screen::new(4, 20));
        let (host, mut watcher) = UnixStream::pair().unwrap();
        let serving = tokio::spawn({
            let screen = screen.clone();
            async move { watch(host, &screen).await }
        });
        assert_eq!(watcher.read_u8().await.unwrap(), WATCHING);
        let mut mirror = Mirror::default();
        screen.output(b"start");
        until(&mut watcher, &mut mirror, "start").await;
        // More changes than are kept for a watcher, faster than it is
        // served: each repaints the one line.
        for n in 0..BACKLOG * 4 {
            screen.output(format!("\r\x1b[Kline {n}").as_bytes());
        }
        let last = format!("line {}", BACKLOG * 4 - 1);
        until(&mut watcher, &mut mirror, &last).await;
        assert_eq!(mirror.screen().contents(), last);
        drop(watcher);
        serving.await.unwrap().unwrap();
    }
}
