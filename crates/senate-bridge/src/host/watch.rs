//! Watching a hosted terminal without touching it.
//!
//! An attached viewer ([`super::attach`]) drives the session: it types, it
//! sets the terminal's size, and there is one of it. A watcher only looks.
//! Any number of them connect to the host's second socket (`see`, beside
//! `tty`), alongside the viewer or with nobody attached. Nothing a watcher
//! types reaches the session, and a watcher is never recorded as activity
//! of the viewer, so it can never count as someone having typed.
//!
//! # Fitting
//!
//! The one thing a watcher may ask is that the session be fitted to its
//! tile ([`FIT`]), so the TUI repaints at that size. The host keeps the
//! latest such request (the most recent wins; one is forgotten when the
//! watcher that sent it leaves) and resizes the PTY to it only while no
//! viewer is attached on `tty`. An attached viewer's size wins, as before;
//! when it detaches, the latest request standing applies again. Anything
//! else a watcher sends is dropped.
//!
//! # The screen
//!
//! The session's TUI paints its screen once and then changes parts of it,
//! so the bytes it writes mean nothing to someone who was not there from
//! the start. The host therefore keeps the screen itself: every byte the
//! TUI writes goes through a terminal emulator ([`vt100`]) held at the
//! PTY's size, retaining a bounded number of main-screen history rows.
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
//!
//! A watcher may send one frame back, the same way:
//!
//! | tag | payload | meaning |
//! | --- | --- | --- |
//! | [`FIT`] | rows `u16`, columns `u16` | fit the session to this, while nobody is attached |
//!
//! A host from before fitting drops it, as it dropped everything a watcher
//! sent.

use std::num::NonZeroU16;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast};
use tokio::task::JoinSet;

use super::IO_CHUNK;

/// The host's greeting on the watch socket.
const WATCHING: u8 = 1;
const SIZE: u8 = 1;
const OUTPUT: u8 = 2;
const SCREEN: u8 = 3;
/// The one frame a watcher sends: fit the session to rows × columns.
const FIT: u8 = 1;
/// The largest side a fit request is believed for.
const MAX_FIT: u16 = 1000;
/// The most a host is believed about its screen: a watcher reads the
/// length from the socket before the bytes.
const MAX_SCREEN: usize = 8 << 20;
/// Changes a watcher may fall behind by before it is sent the screen
/// again instead.
const BACKLOG: usize = 256;
const HISTORY: usize = 512;

/// The file name of a host's watch socket, in its directory. As long as
/// `tty`, whose path is the one checked against the socket path limit
/// before a host starts.
pub(crate) const SOCKET: &str = "see";

fn dimension(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

#[derive(Clone)]
pub(super) enum Change {
    Size(u16, u16),
    Output(Arc<[u8]>),
}

/// The latest fit a watcher asked for, and which watcher asked.
#[derive(Clone, Copy)]
struct Fit {
    watcher: u64,
    rows: u16,
    cols: u16,
}

/// The session's screen as the host keeps it, and those watching it.
pub(super) struct Screen {
    parser: Mutex<vt100::Parser>,
    changes: broadcast::Sender<Change>,
    fit: Mutex<Option<Fit>>,
    /// A new fit was asked for, or the viewer left: the latest applies.
    fit_due: Notify,
    watchers: AtomicU64,
    /// When the TUI last wrote anything ([`super::observed_at`]).
    last_output: AtomicU64,
}

impl Screen {
    pub(super) fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: Mutex::new(vt100::Parser::new(
                dimension(rows),
                dimension(cols),
                HISTORY,
            )),
            changes: broadcast::channel(BACKLOG).0,
            fit: Mutex::new(None),
            fit_due: Notify::new(),
            watchers: AtomicU64::new(0),
            last_output: AtomicU64::new(super::observed_at()),
        }
    }

    fn fit_slot(&self) -> MutexGuard<'_, Option<Fit>> {
        self.fit.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// The size the latest watcher still watching asked the session to
    /// be, rows then columns.
    pub(super) fn wanted_fit(&self) -> Option<(u16, u16)> {
        self.fit_slot().map(|fit| (fit.rows, fit.cols))
    }

    /// Until a fit may be due: a watcher asked for one, or [`Self::refit`].
    pub(super) async fn fit_due(&self) {
        self.fit_due.notified().await;
    }

    /// The latest fit applies again: the viewer that held the size left.
    pub(super) fn refit(&self) {
        self.fit_due.notify_one();
    }

    /// The PTY's size as the screen is kept at, rows then columns.
    pub(super) fn size(&self) -> (u16, u16) {
        let (rows, cols) = self.parser().screen().size();
        (rows.get(), cols.get())
    }

    /// `watcher` asks for the session to be `rows` × `cols`.
    fn ask_fit(&self, watcher: u64, rows: u16, cols: u16) {
        *self.fit_slot() = Some(Fit {
            watcher,
            rows,
            cols,
        });
        self.fit_due.notify_one();
    }

    /// `watcher` left: what it asked for no longer stands.
    fn forget_fit(&self, watcher: u64) {
        let mut slot = self.fit_slot();
        if slot.is_some_and(|fit| fit.watcher == watcher) {
            *slot = None;
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
        self.last_output
            .store(super::observed_at(), Ordering::Release);
        // Nobody watching is not an error.
        let _ = self.changes.send(Change::Output(bytes.into()));
    }

    /// The PTY is `rows` × `cols` now.
    pub(super) fn resize(&self, rows: u16, cols: u16) {
        let mut parser = self.parser();
        parser.set_size(dimension(rows), dimension(cols));
        let _ = self.changes.send(Change::Size(rows, cols));
    }

    /// When the TUI last wrote anything.
    pub(super) fn last_output(&self) -> u64 {
        self.last_output.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn set_last_output(&self, at: u64) {
        self.last_output.store(at, Ordering::Release);
    }

    /// The text of each row of the screen, top to bottom. Kept in memory
    /// only, for [`super::prompt`].
    pub(super) fn rows(&self) -> Vec<String> {
        let parser = self.parser();
        let (_, cols) = parser.screen().size();
        parser.screen().rows(0, cols.get()).collect()
    }

    /// The screen as it is, and every change after it.
    pub(super) fn join(&self) -> (Mirror, broadcast::Receiver<Change>) {
        let parser = self.parser();
        let mut mirror = Mirror::default();
        *mirror.parser.screen_mut() = parser.screen().clone();
        (mirror, self.changes.subscribe())
    }
}

/// Serve everyone who watches, for as long as the host runs.
pub(super) async fn serve(
    listener: &UnixListener,
    screen: &Arc<Screen>,
    admission: &Arc<super::Admission>,
) -> anyhow::Result<()> {
    let mut watchers = JoinSet::new();
    loop {
        tokio::select! {
            (mut stream, _) = super::accept(listener), if watchers.len() < 128 => {
                let screen = screen.clone();
                let admission = admission.clone();
                watchers.spawn(async move {
                    if !admission.viewer(&mut stream, false).await { return; }
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
    let (mirror, changes) = screen.join();
    let (rows, cols) = mirror.screen().size();
    let state = mirror.snapshot();
    to.write_u8(SIZE).await?;
    to.write_u16(rows.get()).await?;
    to.write_u16(cols.get()).await?;
    to.write_u8(SCREEN).await?;
    to.write_u32(u32::try_from(state.len())?).await?;
    to.write_all(&state).await?;
    Ok(changes)
}

async fn watch(stream: UnixStream, screen: &Screen) -> anyhow::Result<()> {
    let me = screen.watchers.fetch_add(1, Ordering::Relaxed);
    let watched = watching(stream, screen, me).await;
    screen.forget_fit(me);
    watched
}

/// The fit requests in what a watcher sent, leaving behind only the start
/// of one that may still be arriving. Anything else is dropped.
fn fits(said: &mut Vec<u8>) -> Vec<(u16, u16)> {
    let mut fits = Vec::new();
    let mut at = 0;
    while at < said.len() {
        if said[at] != FIT {
            at += 1;
            continue;
        }
        let Some(frame) = said.get(at + 1..at + 5) else {
            break;
        };
        let rows = u16::from_be_bytes([frame[0], frame[1]]);
        let cols = u16::from_be_bytes([frame[2], frame[3]]);
        if (1..=MAX_FIT).contains(&rows) && (1..=MAX_FIT).contains(&cols) {
            fits.push((rows, cols));
            at += 5;
        } else {
            at += 1;
        }
    }
    said.drain(..at);
    fits
}

async fn watching(stream: UnixStream, screen: &Screen, me: u64) -> anyhow::Result<()> {
    let (mut from, mut to) = stream.into_split();
    to.write_u8(WATCHING).await?;
    let mut changes = send_screen(&mut to, screen).await?;
    let mut said = [0u8; 64];
    let mut pending = Vec::new();
    loop {
        tokio::select! {
            // A watcher has nothing to say but a fit: the rest is dropped,
            // and reading also tells when it is gone.
            n = from.read(&mut said) => {
                let n = n?;
                if n == 0 {
                    return Ok(());
                }
                pending.extend_from_slice(&said[..n]);
                if let Some(&(rows, cols)) = fits(&mut pending).last() {
                    screen.ask_fit(me, rows, cols);
                }
                // Junk that never becomes a frame is not kept.
                if pending.len() > 64 {
                    pending.clear();
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

/// Withhold a TUI's synchronized frame from the visible mirror until
/// its end, including when the watch socket splits either bracket. A
/// missing end cannot freeze the display or grow an unbounded buffer.
#[derive(Default)]
pub(super) struct SyncFrames {
    partial: Vec<u8>,
    held: Vec<u8>,
    deadline: Option<tokio::time::Instant>,
}

impl SyncFrames {
    const MAX_BYTES: usize = 256 * 1024;
    const TIMEOUT: Duration = Duration::from_millis(150);

    pub(super) fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    pub(super) fn finish(&mut self, mirror: &mut Mirror) -> bool {
        self.deadline = None;
        if self.held.is_empty() {
            return false;
        }
        mirror.parser.process(&self.held);
        self.held.clear();
        true
    }

    pub(super) fn apply(&mut self, mirror: &mut Mirror, frame: &Frame) -> bool {
        match frame {
            Frame::Output(bytes) => self.output(mirror, bytes),
            Frame::Screen(_) => {
                // The new snapshot supersedes bytes from the old stream.
                *self = Self::default();
                mirror.apply(frame);
                true
            }
            Frame::Size(..) => {
                self.finish(mirror);
                mirror.apply(frame);
                true
            }
        }
    }

    fn output(&mut self, mirror: &mut Mirror, bytes: &[u8]) -> bool {
        use super::emulation::{BEGIN_SYNC, END_SYNC};
        let mut prefix = std::mem::take(&mut self.partial);
        let data = if prefix.is_empty() {
            bytes
        } else {
            prefix.extend_from_slice(bytes);
            &prefix
        };
        let mut changed = false;
        let mut start = 0;
        let mut seek = 0;
        while let Some(offset) = data[seek..].iter().position(|&byte| byte == 0x1b) {
            let at = seek + offset;
            let rest = &data[at..];
            if rest.starts_with(BEGIN_SYNC) {
                changed |= self.write(mirror, &data[start..at]);
                self.deadline
                    .get_or_insert_with(|| tokio::time::Instant::now() + Self::TIMEOUT);
                start = at + BEGIN_SYNC.len();
                seek = start;
            } else if rest.starts_with(END_SYNC) {
                changed |= self.write(mirror, &data[start..at]);
                changed |= self.finish(mirror);
                start = at + END_SYNC.len();
                seek = start;
            } else if BEGIN_SYNC.starts_with(rest) || END_SYNC.starts_with(rest) {
                self.partial.extend_from_slice(rest);
                return changed | self.write(mirror, &data[start..at]);
            } else {
                seek = at + 1;
            }
        }
        changed | self.write(mirror, &data[start..])
    }

    fn write(&mut self, mirror: &mut Mirror, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return false;
        }
        if self.deadline.is_some() && bytes.len() <= Self::MAX_BYTES - self.held.len() {
            self.held.extend_from_slice(bytes);
            return false;
        }
        self.finish(mirror);
        mirror.parser.process(bytes);
        true
    }
}

/// A watcher's copy of a session's screen, at the session's own size.
pub(crate) struct Mirror {
    parser: vt100::Parser,
}

impl Default for Mirror {
    fn default() -> Self {
        Self {
            parser: vt100::Parser::new(dimension(24), dimension(80), HISTORY),
        }
    }
}

impl Mirror {
    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub(super) fn live(&mut self) -> bool {
        self.scroll(-isize::MAX)
    }

    pub(super) fn scroll(&mut self, rows: isize) -> bool {
        let screen = self.parser.screen_mut();
        !screen.alternate_screen() && super::scroll::move_by(screen, rows)
    }

    pub(super) fn resize(&mut self, rows: u16, cols: u16) {
        self.parser.set_size(dimension(rows), dimension(cols));
    }

    pub(super) fn output(&mut self, bytes: &[u8]) {
        self.live();
        self.parser.process(bytes);
    }

    /// Paint a real terminal once. Lines go through the terminal's main
    /// screen so that its ordinary history holds them, even when the TUI
    /// currently shows its alternate screen.
    pub(super) fn attach_snapshot(&self) -> Vec<u8> {
        let screen = self.screen();
        let mut primary = Self::default();
        *primary.parser.screen_mut() = screen.clone();
        if screen.alternate_screen() {
            primary.parser.process(b"\x1b[?47l");
        }
        let live = primary.screen().contents_formatted();
        let history = primary.parser.screen_mut();
        let (rows, cols) = history.size();
        let mut output = b"\x1b[0m\x1b[9999;1H".to_vec();
        let mut offset = history.scrollback_len();
        while offset > 0 {
            history.set_scrollback(offset);
            let count = offset.min(usize::from(rows.get()));
            for row in history.rows(0, cols.get()).take(count) {
                output.extend_from_slice(row.as_bytes());
                output.extend_from_slice(b"\r\n");
            }
            offset -= count;
        }
        // The separator and last history row have already moved up one
        // line. Push them entirely into scrollback before painting live.
        for _ in 1..rows.get() {
            output.extend_from_slice(b"\r\n");
        }
        output.extend_from_slice(live.as_bytes());
        if screen.alternate_screen() {
            output.extend_from_slice(b"\x1b[?1049h");
            output.extend_from_slice(screen.contents_formatted().as_bytes());
        }
        // Reporting modes are replayed separately by Emulation. The keypad
        // and cursor modes come from the screen emulator.
        output.extend_from_slice(if screen.application_keypad() {
            b"\x1b="
        } else {
            b"\x1b>"
        });
        output.extend_from_slice(if screen.application_cursor() {
            b"\x1b[?1h"
        } else {
            b"\x1b[?1l"
        });
        let style = match screen.cursor_style() {
            vt100::CursorStyle::Default => 0,
            vt100::CursorStyle::BlinkBlock => 1,
            vt100::CursorStyle::SteadyBlock => 2,
            vt100::CursorStyle::BlinkUnderline => 3,
            vt100::CursorStyle::SteadyUnderline => 4,
            vt100::CursorStyle::BlinkVerticalBar => 5,
            vt100::CursorStyle::SteadyVerticalBar => 6,
        };
        output.extend_from_slice(format!("\x1b[{style} q").as_bytes());
        output
    }

    fn snapshot(&self) -> Vec<u8> {
        let screen = self.screen();
        let state = screen.state_formatted();
        if screen.alternate_screen() {
            let mut primary = Self::default();
            *primary.parser.screen_mut() = screen.clone();
            primary.parser.process(b"\x1b[?47l");
            let mut bytes = primary.snapshot();
            let alternate = [b"\x1b[?1049h", state.as_bytes()].concat();
            if bytes.len() + alternate.len() <= MAX_SCREEN {
                bytes.extend_from_slice(&alternate);
                return bytes;
            }
            return alternate;
        }
        if screen.scrollback_len() > 0 {
            // Replay history through the ordinary screen frame so older viewers
            // still get the same visible screen. The final snapshot restores
            // exact live contents, cursor and attributes.
            let mut capture = String::new();
            let _ = screen.write_contents_formatted_basic(
                &mut capture,
                vt100::capture::BasicFormattedCaptureRange::Full(&mut Default::default()),
            );
            let mut bytes = capture.replace('\n', "\r\n").into_bytes();
            if bytes.len() + state.len() <= MAX_SCREEN {
                bytes.extend_from_slice(state.as_bytes());
                return bytes;
            }
        }
        state.into_bytes()
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
            Frame::Size(rows, cols) => self.resize(*rows, *cols),
            Frame::Screen(state) => {
                // Forget what was shown: the screen starts from this.
                let (rows, cols) = self.parser.screen().size();
                self.parser = vt100::Parser::new(rows, cols, HISTORY);
                self.parser.process(state);
            }
            Frame::Output(bytes) => self.output(bytes),
        }
    }
}

/// An open watch of one hosted terminal.
pub(crate) struct Watch {
    from: Box<dyn AsyncRead + Send + Unpin>,
    /// Where fit requests go ([`Fitter`]); closing it would tell the host
    /// the watcher is gone.
    to: Option<tokio::net::unix::OwnedWriteHalf>,
}

/// What asks the host to fit the session to a tile.
pub(crate) struct Fitter {
    to: tokio::net::unix::OwnedWriteHalf,
}

impl Fitter {
    /// Ask for the session to be `rows` × `cols`.
    pub(crate) async fn fit(&mut self, rows: u16, cols: u16) -> std::io::Result<()> {
        let mut frame = [FIT, 0, 0, 0, 0];
        frame[1..3].copy_from_slice(&rows.to_be_bytes());
        frame[3..5].copy_from_slice(&cols.to_be_bytes());
        self.to.write_all(&frame).await
    }
}

impl Watch {
    /// Start watching the host whose watch socket is `socket`.
    pub(crate) async fn open(socket: &Path) -> anyhow::Result<Self> {
        ensure!(
            crate::testenv::confined(socket),
            "watch socket is outside the test directory"
        );
        let mut stream = open_watch_stream(socket)
            .await
            .context("connect to the hosted terminal's watch socket")?;
        match tokio::time::timeout(Duration::from_secs(3), stream.read_u8()).await?? {
            WATCHING => {}
            _ => bail!("unrecognized terminal host reply"),
        }
        let (from, to) = stream.into_split();
        Ok(Self {
            from: Box::new(from),
            to: Some(to),
        })
    }

    pub(crate) async fn remote(
        mut stream: impl AsyncRead + Send + Unpin + 'static,
    ) -> anyhow::Result<Self> {
        ensure!(
            tokio::time::timeout(Duration::from_secs(3), stream.read_u8()).await?? == WATCHING,
            "unrecognized terminal host reply"
        );
        Ok(Self {
            from: Box::new(stream),
            to: None,
        })
    }

    /// The watch's [`Fitter`], once. Keep it as long as the watch: dropping
    /// it ends the watch.
    pub(crate) fn fitter(&mut self) -> Option<Fitter> {
        self.to.take().map(|to| Fitter { to })
    }

    /// The next thing the host says; `None` when the host is gone (the
    /// session ended).
    pub(crate) async fn next(&mut self) -> anyhow::Result<Option<Frame>> {
        read_frame(&mut self.from).await
    }
}

async fn open_watch_stream(socket: &Path) -> anyhow::Result<UnixStream> {
    #[cfg(test)]
    if super::viewer_address(socket).is_err() {
        // The grid-input unit test uses a synthetic screen server without a bridge.
        return Ok(UnixStream::connect(socket).await?);
    }
    super::open_viewer(socket, false).await
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
    async fn a_watch_snapshot_contains_bounded_main_screen_history() {
        let screen = Screen::new(3, 20);
        screen.output(b"historic-0\r\nhistoric-1\r\nhistoric-2\r\nlive-0\r\nlive-1\r\nlive-2");
        screen.output(b"\x1b[?1049halternate");
        let (host, mut watcher) = UnixStream::pair().unwrap();
        let check = async {
            assert_eq!(watcher.read_u8().await.unwrap(), WATCHING);
            let mut mirror = Mirror::default();
            until(&mut watcher, &mut mirror, "alternate").await;
            assert!(mirror.screen().alternate_screen());
            assert!(!mirror.scroll(3));

            screen.output(b"\x1b[?1049l");
            until(&mut watcher, &mut mirror, "live-2").await;
            assert!(mirror.scroll(3));
            assert_eq!(
                mirror.screen().contents(),
                "historic-0\nhistoric-1\nhistoric-2"
            );

            screen.output(b"\r\nnew-output");
            until(&mut watcher, &mut mirror, "new-output").await;
            assert_eq!(mirror.screen().scrollback(), 0);
            watcher.shutdown().await.unwrap();
        };
        let (result, ()) = tokio::join!(watch(host, &screen), check);
        result.unwrap();
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

    fn fit_frame(rows: u16, cols: u16) -> Vec<u8> {
        let mut frame = vec![FIT];
        frame.extend_from_slice(&rows.to_be_bytes());
        frame.extend_from_slice(&cols.to_be_bytes());
        frame
    }

    #[test]
    fn watched_synchronized_frames_publish_only_on_completion_or_timeout() {
        let mut mirror = Mirror::default();
        mirror.parser.process(b"initial");
        let mut sync = SyncFrames::default();
        assert!(!sync.apply(&mut mirror, &Frame::Output(b"\x1b[?20".to_vec())));
        assert!(!sync.apply(&mut mirror, &Frame::Output(b"26h\rpartial".to_vec())));
        assert_eq!(
            mirror.screen().rows(0, 80).next().as_deref(),
            Some("initial")
        );
        assert!(!sync.apply(&mut mirror, &Frame::Output(b" frame\x1b[?2026".to_vec())));
        assert!(sync.apply(&mut mirror, &Frame::Output(b"l".to_vec())));
        assert_eq!(
            mirror.screen().rows(0, 80).next().as_deref(),
            Some("partial frame")
        );
        assert!(sync.deadline().is_none());

        assert!(!sync.apply(
            &mut mirror,
            &Frame::Output(b"\x1b[?2026h\n\rtimeout".to_vec())
        ));
        assert!(sync.deadline().is_some());
        assert!(sync.finish(&mut mirror));
        assert_eq!(
            mirror.screen().rows(0, 80).nth(1).as_deref(),
            Some("timeout")
        );
    }

    #[test]
    fn attach_snapshot_preserves_primary_history_while_starting_in_alternate() {
        let screen = Screen::new(3, 20);
        screen.output(b"history-1\r\nhistory-2\r\nprimary-1\r\nprimary-2\r\nprimary-3");
        screen.output(b"\x1b[?1049halternate");
        let (mirror, _) = screen.join();
        let bytes = mirror.attach_snapshot();
        assert_eq!(
            bytes.windows(9).filter(|row| *row == b"history-1").count(),
            1
        );
        let mut viewer = vt100::Parser::new(dimension(3), dimension(20), HISTORY);
        viewer.process(b"shell text\r\nseparator\r\n");
        viewer.process(&bytes);
        assert!(viewer.screen().alternate_screen());
        assert_eq!(viewer.screen().contents(), "alternate");
        viewer.process(b"\x1b[?1049l");
        assert_eq!(
            viewer.screen().contents(),
            "primary-1\nprimary-2\nprimary-3"
        );
        let screen = viewer.screen_mut();
        screen.set_scrollback(screen.scrollback_len());
        assert!(
            screen
                .contents()
                .contains("shell text\nseparator\nhistory-1")
        );
        screen.set_scrollback(1);
        assert!(screen.contents().starts_with("history-2\nprimary-1"));
    }

    #[test]
    fn attach_snapshot_overrides_both_cursor_and_keypad_modes() {
        for alternate in [false, true] {
            let screen = Screen::new(3, 20);
            if alternate {
                screen.output(b"\x1b[?1049h");
            }
            let mut viewer = vt100::Parser::new(dimension(3), dimension(20), 0);
            viewer.process(b"\x1b[?1h\x1b=");
            let (mirror, _) = screen.join();
            viewer.process(&mirror.attach_snapshot());
            assert_eq!(viewer.screen().alternate_screen(), alternate);
            assert!(!viewer.screen().application_cursor());
            assert!(!viewer.screen().application_keypad());

            screen.output(b"\x1b[?1h\x1b=");
            let (mirror, _) = screen.join();
            viewer.process(&mirror.attach_snapshot());
            assert!(viewer.screen().application_cursor());
            assert!(viewer.screen().application_keypad());
        }
    }

    #[test]
    fn fit_requests_are_read_from_what_a_watcher_sends_and_the_rest_dropped() {
        let mut said = b"rm -rf /\r".to_vec();
        said.extend(fit_frame(20, 60));
        said.extend(fit_frame(0, 60)); // no such size
        said.extend(fit_frame(24, 80));
        said.extend(&fit_frame(30, 100)[..3]); // still arriving
        assert_eq!(fits(&mut said), [(20, 60), (24, 80)]);
        assert_eq!(said, fit_frame(30, 100)[..3]);
        said.extend(&fit_frame(30, 100)[3..]);
        assert_eq!(fits(&mut said), [(30, 100)]);
        assert!(said.is_empty());
    }

    /// Wait until the screen wants `size`.
    async fn wants(screen: &Screen, size: Option<(u16, u16)>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while screen.wanted_fit() != size {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("never wanted {size:?}"));
    }

    #[tokio::test]
    async fn the_latest_fit_wins_and_leaves_with_its_watcher() {
        let screen = Arc::new(Screen::new(24, 80));
        let open = |screen: &Arc<Screen>| {
            let (host, watcher) = UnixStream::pair().unwrap();
            let screen = screen.clone();
            (
                tokio::spawn(async move { watch(host, &screen).await }),
                watcher,
            )
        };
        let (first, mut one) = open(&screen);
        let (second, mut two) = open(&screen);
        assert_eq!(one.read_u8().await.unwrap(), WATCHING);
        assert_eq!(two.read_u8().await.unwrap(), WATCHING);
        assert_eq!(screen.wanted_fit(), None, "watching alone asks nothing");

        one.write_all(&fit_frame(20, 60)).await.unwrap();
        wants(&screen, Some((20, 60))).await;
        two.write_all(&fit_frame(12, 50)).await.unwrap();
        wants(&screen, Some((12, 50))).await;
        // The first watcher leaving takes nothing with it: the latest was
        // the second's.
        drop(one);
        first.await.unwrap().unwrap();
        assert_eq!(screen.wanted_fit(), Some((12, 50)));
        // The second leaving takes its request with it.
        drop(two);
        second.await.unwrap().unwrap();
        assert_eq!(screen.wanted_fit(), None);
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
