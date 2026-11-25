//! Reading a host's screen as text, for terminal use.
//!
//! The daemon looks at a terminal the way `senate session watch` does: as
//! one more watcher on the host's `see` socket ([`super::watch`]), with
//! its own copy of the screen. A watcher can type nothing, so reading a
//! screen this way can never drive the session, and a host from before
//! terminal use serves it like any other watcher.

use std::time::Duration;

use senate_core::EventId;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::watch::{Frame, Mirror, SOCKET, SyncFrames, Watch};
use super::{connect_viewer, control, directory};
use crate::ipc::IpcError;
use crate::paths::Paths;

/// A screen as text.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Shown {
    pub rows: u16,
    pub cols: u16,
    /// The cursor's row and column, counted from 1.
    pub cursor: (u16, u16),
    pub cursor_hidden: bool,
    /// The program drew on the alternate screen, as full-screen programs do.
    pub alternate: bool,
    /// The rows that scrolled off the top, oldest first, as many as asked.
    pub scrollback: Vec<String>,
    /// Every row of the screen, top to bottom, without trailing blanks.
    pub lines: Vec<String>,
}

impl Shown {
    /// The screen's text, one row per line.
    pub(crate) fn text(&self) -> String {
        self.lines.join("\n")
    }
}

/// What [`Look::next`] saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Seen {
    /// The host said something: the program wrote, or the size changed.
    Changed,
    /// Nothing until the deadline.
    Quiet,
    /// The host is gone.
    Gone,
}

/// An open look at one host's screen.
pub(crate) struct Look {
    frames: mpsc::Receiver<Frame>,
    reader: tokio::task::JoinHandle<()>,
    mirror: Mirror,
    sync: SyncFrames,
}

impl Drop for Look {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Look {
    /// Start looking at host `id` as the daemon. The screen is complete
    /// when this returns.
    pub(crate) async fn open(paths: &Paths, id: &EventId) -> Result<Self, IpcError> {
        let see = directory(paths, id).join(SOCKET);
        let ticket = control::grant_viewer(paths, id, std::process::id(), false).await?;
        let unreadable =
            |error: anyhow::Error| IpcError::internal(format!("cannot read the screen: {error}"));
        let opened = async {
            let stream = connect_viewer(&see, &ticket).await?;
            let mut watch = Watch::remote(stream).await?;
            let mut mirror = Mirror::default();
            let mut sync = SyncFrames::default();
            // The host says its size, then sends the whole screen.
            loop {
                let frame = watch
                    .next()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("the terminal host left"))?;
                let whole = matches!(frame, Frame::Screen(_));
                sync.apply(&mut mirror, &frame);
                if whole {
                    return Ok((watch, mirror, sync));
                }
            }
        };
        let (mut watch, mirror, sync) = tokio::time::timeout(Duration::from_secs(3), opened)
            .await
            .map_err(|_| IpcError::internal("timed out reading the screen"))?
            .map_err(unreadable)?;
        // Reading a frame cannot be abandoned halfway, so a task reads
        // them whole and `next` only takes complete ones.
        let (send, frames) = mpsc::channel(64);
        let reader = tokio::spawn(async move {
            while let Ok(Some(frame)) = watch.next().await {
                if send.send(frame).await.is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            frames,
            reader,
            mirror,
            sync,
        })
    }

    /// Take in what the host says next, waiting until `deadline` at most.
    pub(crate) async fn next(&mut self, deadline: Instant) -> Seen {
        loop {
            let held = self.sync.deadline();
            tokio::select! {
                frame = self.frames.recv() => match frame {
                    Some(frame) => {
                        if self.sync.apply(&mut self.mirror, &frame) {
                            return Seen::Changed;
                        }
                    }
                    None => return Seen::Gone,
                },
                // A synchronized frame the program never closed is shown.
                () = tokio::time::sleep_until(held.unwrap_or(deadline)), if held.is_some() => {
                    if self.sync.finish(&mut self.mirror) {
                        return Seen::Changed;
                    }
                }
                () = tokio::time::sleep_until(deadline) => return Seen::Quiet,
            }
        }
    }

    /// Take in everything the host says until it has said nothing for
    /// `still`, or until `limit` has passed.
    pub(crate) async fn settle(&mut self, still: Duration, limit: Duration) -> Seen {
        let end = Instant::now() + limit;
        loop {
            match self.next((Instant::now() + still).min(end)).await {
                Seen::Changed if Instant::now() < end => {}
                seen => return seen,
            }
        }
    }

    /// The screen as it is now, with up to `scrollback` rows of what
    /// scrolled off above it.
    pub(crate) fn shown(&self, scrollback: usize) -> Shown {
        let screen = self.mirror.screen();
        let (rows, cols) = screen.size();
        let (rows, cols) = (rows.get(), cols.get());
        let lines = screen
            .rows(0, cols)
            .map(|row| row.trim_end().to_owned())
            .collect();
        let mut above = Vec::new();
        if scrollback > 0 && !screen.alternate_screen() {
            let mut history = screen.clone();
            let mut offset = history.scrollback_len().min(scrollback);
            while offset > 0 {
                history.set_scrollback(offset);
                let count = offset.min(usize::from(rows));
                above.extend(
                    history
                        .rows(0, cols)
                        .take(count)
                        .map(|row| row.trim_end().to_owned()),
                );
                offset -= count;
            }
        }
        let (row, col) = screen.cursor_position();
        Shown {
            rows,
            cols,
            cursor: (row + 1, col + 1),
            cursor_hidden: screen.hide_cursor(),
            alternate: screen.alternate_screen(),
            scrollback: above,
            lines,
        }
    }
}
