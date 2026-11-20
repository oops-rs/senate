//! Looking at hosted terminals: one, read-only, or all of them in a grid.
//!
//! Both are the same viewer. Each terminal shown is one watch of its host
//! ([`super::watch`]): the viewer holds that session's screen at the
//! session's own size and paints as much of it as the tile has room for.
//! Nothing here can reach a session: a watch carries nothing to its host,
//! and the keys read here only move the viewer.
//!
//! # What a tile shows
//!
//! A session's terminal is as large as whoever attached to it last made
//! it, and a tile is usually smaller. No session is resized for being
//! watched, so a tile shows a part of the screen: its left edge, and the
//! rows that end at the last line with anything on it (or the cursor's
//! line), since a TUI keeps its prompt and its newest output at the
//! bottom.
//!
//! # The grid
//!
//! `senate watch-all` lays the sessions out in as even a grid as the
//! terminal has room for, each tile at least [`MIN_TILE_COLS`] columns
//! and [`MIN_TILE_ROWS`] rows; when they do not all fit, in pages. Each
//! tile has a header naming its session; a paused or ended one is dimmed.
//! The list of sessions is asked for again every couple of seconds, so
//! sessions come and go as they start and stop.
//!
//! | key | does |
//! | --- | --- |
//! | arrows, `h` `j` `k` `l` | move to another tile |
//! | PgUp / PgDn | the page before, the page after |
//! | Enter | attach to the tile's session, as `senate attach`; detaching comes back here |
//! | `q`, Ctrl-] | leave |
//!
//! `senate attach --read-only` is the one tile, full screen, under one
//! status line. Every key but Ctrl-] is dropped.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use textwrap::core::display_width;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::terminal::{RESET_DISPLAY, RawTerminal, split_input};
use super::watch::{Mirror, Watch};
use crate::paths::Paths;

/// The least a tile is given, its header row included.
pub const MIN_TILE_COLS: u16 = 40;
pub const MIN_TILE_ROWS: u16 = 10;
/// How often the grid asks which sessions there are.
const RELIST: Duration = Duration::from_secs(2);
/// A note may run over several rows of its tile.
fn wrapped(note: &str, width: usize) -> Vec<String> {
    textwrap::wrap(note, width.max(1))
        .into_iter()
        .map(std::borrow::Cow::into_owned)
        .collect()
}

/// The least time between two paints: a busy session repaints often.
const FRAME: Duration = Duration::from_millis(40);
/// How long one paint may take before the keys are looked at again.
const STUCK: Duration = Duration::from_millis(250);

/// One hosted session, as the viewer needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// What tells this session from every other.
    pub key: String,
    /// The name a command takes for it: its alias, else its senator name.
    pub alias: String,
    /// What the header says of it: alias, committee, runtime, state.
    pub title: String,
    /// The host's attach socket (`tty`).
    pub tty: PathBuf,
    /// Shown dimmed: paused, or otherwise not running now.
    pub dim: bool,
}

impl Session {
    /// The host's watch socket, beside its attach socket.
    fn watch_socket(&self) -> PathBuf {
        self.tty.with_file_name(super::watch::SOCKET)
    }
}

/// What a tile has of its session.
#[derive(Default)]
struct Shown {
    mirror: Option<Mirror>,
    /// Why there is nothing to show, or nothing more.
    note: Option<String>,
}

struct Tile {
    session: Session,
    shown: Arc<Mutex<Shown>>,
    watching: JoinHandle<()>,
}

impl Drop for Tile {
    fn drop(&mut self) {
        self.watching.abort();
    }
}

fn shown(shown: &Mutex<Shown>) -> MutexGuard<'_, Shown> {
    shown.lock().unwrap_or_else(|error| error.into_inner())
}

/// What to say when a host has no watch socket: it was started by a build
/// from before sessions could be watched.
pub fn cannot_watch(alias: &str) -> String {
    format!(
        "this session's terminal host was started before sessions could be watched; \
         restart it with `senate stop {alias}` then `senate restore {alias}`"
    )
}

impl Tile {
    fn open(session: Session, changed: Arc<Notify>) -> Self {
        let shown_ = Arc::new(Mutex::new(Shown::default()));
        let watching = tokio::spawn({
            let state = shown_.clone();
            let socket = session.watch_socket();
            let alias = session.alias.clone();
            async move {
                let note = match Watch::open(&socket).await {
                    Ok(mut watch) => {
                        shown(&state).mirror = Some(Mirror::default());
                        loop {
                            match watch.next().await {
                                Ok(Some(frame)) => {
                                    if let Some(mirror) = shown(&state).mirror.as_mut() {
                                        mirror.apply(&frame);
                                    }
                                    changed.notify_one();
                                }
                                Ok(None) => break "the session ended".to_owned(),
                                Err(error) => break format!("the watch broke: {error:#}"),
                            }
                        }
                    }
                    Err(_) if !socket.exists() && socket.with_file_name("tty").exists() => {
                        cannot_watch(&alias)
                    }
                    Err(error) => format!("{error:#}"),
                };
                shown(&state).note = Some(note);
                changed.notify_one();
            }
        });
        Self {
            session,
            shown: shown_,
            watching,
        }
    }
}

// ----- layout ----------------------------------------------------------------

/// A grid for `count` sessions on a terminal `rows` × `cols`, one row kept
/// for the status line: how many tiles across and down a page.
fn grid(count: usize, rows: u16, cols: u16) -> (u16, u16) {
    let most_across = (cols / MIN_TILE_COLS).max(1);
    let most_down = (rows.saturating_sub(1) / MIN_TILE_ROWS).max(1);
    let count = u16::try_from(count.max(1)).unwrap_or(u16::MAX);
    // As square as it gets: across first, since terminals are wide.
    let mut across = 1;
    while across < most_across && across * across < count {
        across += 1;
    }
    let down = count.div_ceil(across).clamp(1, most_down);
    (across, down)
}

/// Where tile `slot` of a page is: its top-left corner (0-based) and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    row: u16,
    col: u16,
    rows: u16,
    cols: u16,
}

fn rect(slot: u16, across: u16, down: u16, rows: u16, cols: u16) -> Rect {
    let (tile_rows, tile_cols) = (rows.saturating_sub(1) / down, cols / across);
    let (r, c) = (slot / across, slot % across);
    Rect {
        row: r * tile_rows,
        col: c * tile_cols,
        // The last column and row take what the division left over.
        rows: if r + 1 == down {
            rows.saturating_sub(1) - r * tile_rows
        } else {
            tile_rows
        },
        cols: if c + 1 == across {
            cols - c * tile_cols
        } else {
            tile_cols
        },
    }
}

// ----- painting --------------------------------------------------------------

/// `text` cut or padded to exactly `width` columns.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = display_width(ch.encode_utf8(&mut [0; 4]));
        if used + w > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.extend(std::iter::repeat_n(' ', width - used));
    out
}

fn at(out: &mut String, row: u16, col: u16) {
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
}

fn color(out: &mut String, color: vt100::Color, foreground: bool) {
    let base = if foreground { 38 } else { 48 };
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(n) => {
            let _ = write!(out, ";{base};5;{n}");
        }
        vt100::Color::Rgb(r, g, b) => {
            let _ = write!(out, ";{base};2;{r};{g};{b}");
        }
    }
}

/// The first screen row a body of `rows` rows shows: the rows that end at
/// the last line with anything on it, or the cursor's.
fn top_row(screen: &vt100::Screen, rows: u16) -> u16 {
    let (height, width) = screen.size();
    let last_written = screen
        .rows(0, width.get())
        .enumerate()
        .filter(|(_, text)| !text.trim().is_empty())
        .map(|(row, _)| u16::try_from(row).unwrap_or(u16::MAX))
        .last()
        .unwrap_or(0);
    let last = last_written.max(screen.cursor_position().0);
    (last + 1)
        .saturating_sub(rows)
        .min(height.get().saturating_sub(rows))
}

/// Paint `rows` × `cols` of `screen` from its row `top`, left edge, at
/// (`row`, `col`). Returns where the session's cursor is in the terminal,
/// when it is shown and inside what was painted.
fn paint_screen(
    out: &mut String,
    screen: &vt100::Screen,
    area: Rect,
    top: u16,
    dim: bool,
) -> Option<(u16, u16)> {
    let reset = if dim { "\x1b[0;2m" } else { "\x1b[0m" };
    for line in 0..area.rows {
        at(out, area.row + line, area.col);
        out.push_str(reset);
        let mut current = None;
        let mut col = 0;
        while col < area.cols {
            let cell = screen.cell(top + line, col);
            let wide = cell.is_some_and(vt100::Cell::is_wide);
            match cell {
                // A wide character cut by the tile's edge, or the half of
                // one that starts left of it: a blank.
                Some(cell)
                    if cell.has_contents()
                        && !cell.is_wide_continuation()
                        && !(wide && col + 1 >= area.cols) =>
                {
                    let attrs = (
                        cell.fgcolor(),
                        cell.bgcolor(),
                        cell.bold(),
                        cell.dim() || dim,
                        cell.italic(),
                        cell.underline(),
                        cell.inverse(),
                    );
                    if current != Some(attrs) {
                        out.push_str("\x1b[0");
                        for (on, code) in [
                            (attrs.2, ";1"),
                            (attrs.3, ";2"),
                            (attrs.4, ";3"),
                            (attrs.5, ";4"),
                            (attrs.6, ";7"),
                        ] {
                            if on {
                                out.push_str(code);
                            }
                        }
                        color(out, attrs.0, true);
                        color(out, attrs.1, false);
                        out.push('m');
                        current = Some(attrs);
                    }
                    out.push_str(cell.contents());
                    col += if wide { 2 } else { 1 };
                }
                Some(cell) if !cell.has_contents() && !cell.is_wide_continuation() => {
                    // An empty cell still has a background.
                    let attrs = (
                        vt100::Color::Default,
                        cell.bgcolor(),
                        false,
                        dim,
                        false,
                        false,
                        cell.inverse(),
                    );
                    if current != Some(attrs) {
                        out.push_str("\x1b[0");
                        if dim {
                            out.push_str(";2");
                        }
                        if attrs.6 {
                            out.push_str(";7");
                        }
                        color(out, attrs.1, false);
                        out.push('m');
                        current = Some(attrs);
                    }
                    out.push(' ');
                    col += 1;
                }
                _ => {
                    if current.is_some() {
                        out.push_str(reset);
                        current = None;
                    }
                    out.push(' ');
                    col += 1;
                }
            }
        }
        out.push_str("\x1b[0m");
    }
    let (cursor_row, cursor_col) = screen.cursor_position();
    (!screen.hide_cursor()
        && cursor_row >= top
        && cursor_row - top < area.rows
        && cursor_col < area.cols)
        .then(|| (area.row + cursor_row - top, area.col + cursor_col))
}

/// Paint lines of plain text into `area`, the rest of it blank.
fn paint_note(out: &mut String, area: Rect, note: &str) {
    let lines = wrapped(note, usize::from(area.cols));
    for line in 0..area.rows {
        at(out, area.row + line, area.col);
        let text = lines.get(usize::from(line)).map_or("", String::as_str);
        let _ = write!(out, "\x1b[0;2m{}\x1b[0m", fit(text, usize::from(area.cols)));
    }
}

/// Paint one tile: its header, then its session's screen or why there is
/// none.
fn paint_tile(out: &mut String, tile: &Tile, area: Rect, focused: bool) -> Option<(u16, u16)> {
    let state = shown(&tile.shown);
    let dim = tile.session.dim || state.note.is_some();
    at(out, area.row, area.col);
    let style = match (focused, dim) {
        (true, _) => "\x1b[0;1;7m",
        (false, true) => "\x1b[0;2;4m",
        (false, false) => "\x1b[0;4m",
    };
    let mark = if focused { "▶ " } else { "  " };
    let title = format!("{mark}{}", tile.session.title);
    let _ = write!(out, "{style}{}\x1b[0m", fit(&title, usize::from(area.cols)));
    let body = Rect {
        row: area.row + 1,
        rows: area.rows.saturating_sub(1),
        // A blank column keeps this tile's text apart from the next one's.
        cols: area.cols.saturating_sub(1).max(1),
        ..area
    };
    match (&state.mirror, &state.note) {
        (Some(mirror), None) => {
            let screen = mirror.screen();
            paint_screen(out, screen, body, top_row(screen, body.rows), dim)
        }
        (_, note) => {
            paint_note(out, body, note.as_deref().unwrap_or("connecting…"));
            None
        }
    }
}

// ----- keys ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Open,
    /// `q`: leaves the grid; a key like any other in a read-only attach.
    Quit,
    /// Ctrl-]: leaves either.
    Detach,
}

/// The keys in `pending`, leaving behind only the start of one that may
/// still be arriving. Anything that is no key of the viewer is dropped.
fn keys(pending: &mut Vec<u8>) -> Vec<Key> {
    let mut out = Vec::new();
    // The detach key ends everything, wherever it stands.
    let (before, detach) = split_input(pending);
    let mut at = 0;
    while at < before {
        let rest = &pending[at..before];
        let (key, len) = match rest {
            [b'q', ..] => (Some(Key::Quit), 1),
            [b'\r' | b'\n', ..] => (Some(Key::Open), 1),
            [b'h', ..] => (Some(Key::Left), 1),
            [b'l', ..] => (Some(Key::Right), 1),
            [b'k', ..] => (Some(Key::Up), 1),
            [b'j', ..] => (Some(Key::Down), 1),
            [0x1b, b'[' | b'O', b'A', ..] => (Some(Key::Up), 3),
            [0x1b, b'[' | b'O', b'B', ..] => (Some(Key::Down), 3),
            [0x1b, b'[' | b'O', b'C', ..] => (Some(Key::Right), 3),
            [0x1b, b'[' | b'O', b'D', ..] => (Some(Key::Left), 3),
            [0x1b, b'[', b'5', b'~', ..] => (Some(Key::PageUp), 4),
            [0x1b, b'[', b'6', b'~', ..] => (Some(Key::PageDown), 4),
            // The start of one of those, still arriving.
            [0x1b] | [0x1b, b'[' | b'O'] | [0x1b, b'[', b'5' | b'6'] if !detach => break,
            _ => (None, 1),
        };
        out.extend(key);
        at += len;
    }
    if detach {
        out.push(Key::Detach);
        pending.clear();
    } else {
        pending.drain(..at);
    }
    out
}

// ----- the viewer ------------------------------------------------------------

enum Mode {
    /// `senate attach --read-only`: the one session, nothing but detach.
    One,
    /// `senate watch-all`.
    Grid,
}

struct Viewer {
    mode: Mode,
    tiles: Vec<Tile>,
    focus: usize,
    changed: Arc<Notify>,
}

impl Viewer {
    /// Keep the tiles of the sessions still there, open the new ones, in
    /// the order listed. The focus stays on its session when it can.
    fn list(&mut self, sessions: Vec<Session>) {
        let focused = self.tiles.get(self.focus).map(|t| t.session.key.clone());
        let mut known: BTreeMap<String, Tile> = std::mem::take(&mut self.tiles)
            .into_iter()
            .map(|tile| (tile.session.key.clone(), tile))
            .collect();
        for session in sessions {
            let tile = match known.remove(&session.key) {
                Some(mut tile) => {
                    tile.session = session;
                    tile
                }
                None => Tile::open(session, self.changed.clone()),
            };
            self.tiles.push(tile);
        }
        self.focus = focused
            .and_then(|key| self.tiles.iter().position(|t| t.session.key == key))
            .unwrap_or(self.focus)
            .min(self.tiles.len().saturating_sub(1));
    }

    fn paint(&self, rows: u16, cols: u16) -> String {
        let mut out = String::from("\x1b[?25l");
        let status_row = rows.saturating_sub(1);
        let mut cursor = None;
        let status = match self.mode {
            Mode::One => {
                let area = Rect {
                    row: 0,
                    col: 0,
                    rows: status_row,
                    cols,
                };
                let Some(tile) = self.tiles.first() else {
                    return out;
                };
                let state = shown(&tile.shown);
                match (&state.mirror, &state.note) {
                    (Some(mirror), None) => {
                        let screen = mirror.screen();
                        cursor =
                            paint_screen(&mut out, screen, area, top_row(screen, area.rows), false);
                    }
                    (_, note) => {
                        paint_note(&mut out, area, note.as_deref().unwrap_or("connecting…"));
                    }
                }
                format!(" read-only · {} · Ctrl-] leaves", tile.session.title)
            }
            Mode::Grid if self.tiles.is_empty() => {
                paint_note(
                    &mut out,
                    Rect {
                        row: 0,
                        col: 0,
                        rows: status_row,
                        cols,
                    },
                    "no hosted sessions on this machine; `senate spawn` starts one",
                );
                " q leaves".to_owned()
            }
            Mode::Grid => {
                let (across, down) = grid(self.tiles.len(), rows, cols);
                let per_page = usize::from(across) * usize::from(down);
                let pages = self.tiles.len().div_ceil(per_page);
                let page = self.focus / per_page;
                for slot in 0..per_page {
                    let area = rect(
                        u16::try_from(slot).unwrap_or(u16::MAX),
                        across,
                        down,
                        rows,
                        cols,
                    );
                    match self.tiles.get(page * per_page + slot) {
                        Some(tile) => {
                            paint_tile(&mut out, tile, area, page * per_page + slot == self.focus);
                        }
                        None => paint_note(&mut out, area, ""),
                    }
                }
                let paging = if pages > 1 {
                    format!(" · page {}/{pages} (PgUp/PgDn)", page + 1)
                } else {
                    String::new()
                };
                format!(
                    " {} sessions{paging} · arrows move · Enter attaches · q leaves",
                    self.tiles.len()
                )
            }
        };
        at(&mut out, status_row, 0);
        let _ = write!(out, "\x1b[0;7m{}\x1b[0m", fit(&status, usize::from(cols)));
        if let Some((row, col)) = cursor {
            at(&mut out, row, col);
            out.push_str("\x1b[?25h");
        }
        out
    }

    /// Move the focus by `key`, on a grid `across` wide showing `per_page`.
    fn step(&mut self, key: Key, across: usize, per_page: usize) {
        let last = self.tiles.len().saturating_sub(1);
        self.focus = match key {
            Key::Left => self.focus.saturating_sub(1),
            Key::Right => self.focus + 1,
            Key::Up => self.focus.saturating_sub(across),
            Key::Down => self.focus + across,
            Key::PageUp => self.focus.saturating_sub(per_page),
            Key::PageDown => self.focus + per_page,
            Key::Open | Key::Quit | Key::Detach => self.focus,
        }
        .min(last);
    }
}

/// What ended one stay in the viewer.
enum Left {
    Quit,
    /// Attach to this session, then come back.
    Attach(Session),
}

async fn stay(viewer: &mut Viewer, relist: &mut Option<Relist<'_>>) -> anyhow::Result<Left> {
    let terminal = RawTerminal::open(true).await?;
    let mut winch = signal(SignalKind::window_change())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut listing = tokio::time::interval(RELIST);
    let mut pending = Vec::new();
    let mut input = [0u8; 256];
    let mut dirty = true;
    let mut stuck = false;
    let left = loop {
        if dirty {
            let (rows, cols) = terminal.window()?;
            let frame = viewer.paint(rows, cols);
            // A terminal that is not taking output must not keep the keys
            // from being read: leaving has to work. What was cut short is
            // painted whole the next time round.
            match tokio::time::timeout(STUCK, terminal.draw(frame.as_bytes())).await {
                Ok(drawn) => {
                    drawn?;
                    dirty = false;
                    // A busy session changes many times a second.
                    tokio::time::sleep(FRAME).await;
                }
                Err(_) => stuck = true,
            }
        }
        tokio::select! {
            () = viewer.changed.notified(), if !dirty => dirty = true,
            () = tokio::time::sleep(FRAME), if std::mem::take(&mut stuck) => {}
            _ = winch.recv() => {
                terminal.draw(b"\x1b[2J").await?;
                dirty = true;
            }
            _ = listing.tick(), if relist.is_some() => {
                if let Some(relist) = relist.as_mut()
                    && let Ok(sessions) = relist().await
                {
                    viewer.list(sessions);
                    dirty = true;
                }
            }
            n = terminal.read(&mut input) => {
                let n = n?;
                if n == 0 {
                    break Left::Quit;
                }
                pending.extend_from_slice(&input[..n]);
                let (rows, cols) = terminal.window()?;
                let (across, down) = grid(viewer.tiles.len(), rows, cols);
                let mut leave = None;
                for key in keys(&mut pending) {
                    match (&viewer.mode, key) {
                        (_, Key::Detach) | (Mode::Grid, Key::Quit) => leave = Some(Left::Quit),
                        // In a read-only attach every other key is typing,
                        // and typing goes nowhere.
                        (Mode::One, _) => {}
                        (Mode::Grid, Key::Open) => {
                            if let Some(tile) = viewer.tiles.get(viewer.focus) {
                                leave = Some(Left::Attach(tile.session.clone()));
                            }
                        }
                        (Mode::Grid, key) => {
                            viewer.step(key, usize::from(across), usize::from(across) * usize::from(down));
                            dirty = true;
                        }
                    }
                }
                if let Some(leave) = leave {
                    break leave;
                }
            }
            _ = hup.recv() => break Left::Quit,
            _ = term.recv() => break Left::Quit,
            _ = interrupt.recv() => break Left::Quit,
        }
    };
    let _ = tokio::time::timeout(Duration::from_millis(250), terminal.draw(RESET_DISPLAY)).await;
    Ok(left)
}

type Listing = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<Vec<Session>>> + Send>>;
type Relist<'a> = Box<dyn FnMut() -> Listing + Send + 'a>;

/// `senate attach --read-only`: show `session`'s terminal until Ctrl-],
/// the person closes the terminal, or the session ends.
pub async fn watch_one(session: Session) -> anyhow::Result<()> {
    anyhow::ensure!(
        session.watch_socket().exists() || !session.tty.exists(),
        "{}",
        cannot_watch(&session.alias)
    );
    let mut viewer = Viewer {
        mode: Mode::One,
        tiles: Vec::new(),
        focus: 0,
        changed: Arc::new(Notify::new()),
    };
    viewer.list(vec![session]);
    stay(&mut viewer, &mut None).await.map(drop)
}

/// `senate watch-all`: every session `sessions` lists, asked again every
/// couple of seconds, until `q` or Ctrl-]. Enter attaches to the focused
/// one and comes back when that detaches.
pub async fn watch_all(
    paths: &Paths,
    sessions: impl FnMut() -> Listing + Send,
) -> anyhow::Result<()> {
    let mut relist: Option<Relist<'_>> = Some(Box::new(sessions));
    let mut viewer = Viewer {
        mode: Mode::Grid,
        tiles: Vec::new(),
        focus: 0,
        changed: Arc::new(Notify::new()),
    };
    if let Some(relist) = relist.as_mut() {
        viewer.list(relist().await?);
    }
    loop {
        match stay(&mut viewer, &mut relist).await? {
            Left::Quit => return Ok(()),
            Left::Attach(session) => {
                if let Err(error) = super::attach(&session.tty, paths).await {
                    eprintln!("{error:#}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let size = |n| std::num::NonZeroU16::new(n).unwrap();
        let mut parser = vt100::Parser::new(size(rows), size(cols), 0);
        parser.process(bytes);
        parser
    }

    /// What a terminal `rows` × `cols` shows after `painted`.
    fn shows(rows: u16, cols: u16, painted: &str) -> vt100::Parser {
        screen(rows, cols, painted.as_bytes())
    }

    #[test]
    fn the_grid_is_as_even_as_the_terminal_has_room_for() {
        // (sessions, rows, cols) -> (across, down)
        for (count, rows, cols, expected) in [
            (1, 50, 200, (1, 1)),
            (2, 50, 200, (2, 1)),
            (4, 50, 200, (2, 2)),
            (5, 50, 200, (3, 2)),
            (9, 50, 200, (3, 3)),
            // No room for more than two across or two down: pages.
            (9, 24, 80, (2, 2)),
            (3, 12, 60, (1, 1)),
            (0, 24, 80, (1, 1)),
        ] {
            assert_eq!(
                grid(count, rows, cols),
                expected,
                "{count} on {rows}x{cols}"
            );
        }
        // Tiles cover the terminal above the status line, with no overlap.
        let (rows, cols) = (51, 201);
        let (across, down) = grid(5, rows, cols);
        let rects: Vec<Rect> = (0..across * down)
            .map(|slot| rect(slot, across, down, rows, cols))
            .collect();
        assert_eq!(
            rects[0],
            Rect {
                row: 0,
                col: 0,
                rows: 25,
                cols: 67
            }
        );
        let last = rects.last().unwrap();
        assert_eq!(
            (last.row + last.rows, last.col + last.cols),
            (rows - 1, cols)
        );
    }

    #[test]
    fn a_tile_shows_the_left_edge_and_the_rows_that_end_at_the_last_line() {
        // A 10 x 30 session: a banner, output, and a prompt on row 6.
        let mut text = Vec::new();
        text.extend_from_slice(b"\x1b[2J\x1b[H\x1b[1;34mbanner\x1b[m");
        for n in 1..=5 {
            text.extend_from_slice(format!("\r\nline {n} is longer than the tile").as_bytes());
        }
        text.extend_from_slice("\r\n> 你好".as_bytes());
        let session = screen(10, 30, &text);
        assert_eq!(top_row(session.screen(), 4), 3, "rows 3..=6");
        assert_eq!(top_row(session.screen(), 20), 0, "all of it fits");

        // Painted into a 4 x 12 area at (2, 5) of a 10 x 40 terminal.
        let mut out = String::new();
        let area = Rect {
            row: 2,
            col: 5,
            rows: 4,
            cols: 12,
        };
        let cursor = paint_screen(&mut out, session.screen(), area, 3, false);
        let terminal = shows(10, 40, &out);
        let rows: Vec<String> = terminal.screen().rows(0, 40).collect();
        assert_eq!(rows[1], "");
        assert_eq!(rows[2], "     line 3 is lo");
        assert_eq!(rows[4], "     line 5 is lo");
        assert_eq!(rows[5].trim_end(), "     > 你好");
        assert_eq!(
            rows[5].len(),
            "     > 你好      ".len(),
            "blank to the area's edge, no further"
        );
        assert_eq!(rows[6], "");
        assert_eq!(
            cursor,
            Some((5, 5 + 6)),
            "after the prompt's two wide characters"
        );

        // Colours and weight come through; nothing leaks past the area.
        let mut out = String::new();
        let area = Rect {
            row: 0,
            col: 0,
            rows: 2,
            cols: 4,
        };
        paint_screen(&mut out, session.screen(), area, 0, false);
        let terminal = shows(4, 20, &out);
        let cell = terminal.screen().cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "b");
        assert!(cell.bold());
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(4));
        assert_eq!(terminal.screen().rows(0, 20).next().unwrap(), "bann");
        assert!(
            !terminal.screen().cell(1, 0).unwrap().bold(),
            "line 1 is plain"
        );
    }

    /// A tile showing `text`, with no host behind it.
    fn tile(alias: &str, text: &str, dim: bool) -> Tile {
        let mut mirror = Mirror::default();
        mirror.apply(&crate::host::watch::Frame::Output(text.as_bytes().to_vec()));
        Tile {
            session: Session {
                key: alias.to_owned(),
                alias: alias.to_owned(),
                title: format!("{alias} · refund-v2 · claude · live"),
                tty: PathBuf::from(format!("/nowhere/{alias}/tty")),
                dim,
            },
            shown: Arc::new(Mutex::new(Shown {
                mirror: Some(mirror),
                note: None,
            })),
            watching: tokio::spawn(async {}),
        }
    }

    #[tokio::test]
    async fn the_grid_pages_names_each_tile_and_marks_the_focused_one() {
        let mut viewer = Viewer {
            mode: Mode::Grid,
            tiles: ["a", "b", "c", "d", "e"]
                .into_iter()
                .map(|alias| tile(alias, &format!("screen of {alias}"), alias == "c"))
                .collect(),
            focus: 1,
            changed: Arc::new(Notify::new()),
        };
        // 21 x 80: two tiles across, two down, so two pages of the five.
        let (rows, cols) = (21, 80);
        let terminal = shows(rows, cols, &viewer.paint(rows, cols));
        let lines: Vec<String> = terminal.screen().rows(0, cols).collect();
        assert!(
            lines[0].starts_with("  a · refund-v2 · claude · live"),
            "{:?}",
            lines[0]
        );
        assert_eq!(
            &lines[0][lines[0].find('▶').unwrap()..][..5],
            "▶ b",
            "{:?}",
            lines[0]
        );
        assert!(lines[1].starts_with("screen of a"), "{:?}", lines[1]);
        assert!(lines[1].contains("screen of b"), "{:?}", lines[1]);
        assert!(lines[10].starts_with("  c · refund-v2"), "{:?}", lines[10]);
        assert!(lines[11].starts_with("screen of c"));
        assert!(lines[11].contains("screen of d"));
        assert!(
            !lines.iter().any(|l| l.contains("screen of e")),
            "on the next page"
        );
        assert!(
            lines[20].contains("5 sessions · page 1/2"),
            "{:?}",
            lines[20]
        );
        // The focused header stands out; a dimmed session is dim all over.
        let focused = terminal.screen().cell(0, 42).unwrap();
        assert!(focused.inverse() && focused.bold());
        assert!(!terminal.screen().cell(0, 2).unwrap().inverse());
        assert!(terminal.screen().cell(10, 2).unwrap().dim());
        assert!(terminal.screen().cell(11, 0).unwrap().dim());
        assert!(!terminal.screen().cell(1, 0).unwrap().dim());

        // The next page: the fifth alone, the rest of the page blank.
        viewer.step(Key::PageDown, 2, 4);
        assert_eq!(viewer.focus, 4, "no further than the last");
        let terminal = shows(rows, cols, &viewer.paint(rows, cols));
        let lines: Vec<String> = terminal.screen().rows(0, cols).collect();
        assert!(lines[0].starts_with("▶ e · refund-v2"), "{:?}", lines[0]);
        assert!(lines[1].starts_with("screen of e"));
        assert!(!lines.iter().any(|l| l.contains("screen of a")));
        assert!(lines[20].contains("page 2/2"));

        // Moving: down a row, left, up; never off either end.
        viewer.focus = 0;
        viewer.step(Key::Down, 2, 4);
        assert_eq!(viewer.focus, 2);
        viewer.step(Key::Right, 2, 4);
        viewer.step(Key::Up, 2, 4);
        assert_eq!(viewer.focus, 1);
        viewer.step(Key::Up, 2, 4);
        viewer.step(Key::Left, 2, 4);
        viewer.step(Key::Left, 2, 4);
        assert_eq!(viewer.focus, 0);

        // The list changes: the focus stays on its session, sessions that
        // went are dropped, and the order is the list's.
        viewer.focus = 3;
        let listed = |alias: &str| viewer_session(alias);
        viewer.list(vec![listed("d"), listed("a")]);
        let order: Vec<&str> = viewer
            .tiles
            .iter()
            .map(|t| t.session.alias.as_str())
            .collect();
        assert_eq!(order, ["d", "a"]);
        assert_eq!(viewer.focus, 0, "still on d");
    }

    fn viewer_session(alias: &str) -> Session {
        Session {
            key: alias.to_owned(),
            alias: alias.to_owned(),
            title: alias.to_owned(),
            tty: PathBuf::from(format!("/nowhere/{alias}/tty")),
            dim: false,
        }
    }

    #[test]
    fn a_host_from_before_watching_is_named_with_the_commands_to_restart_it() {
        assert_eq!(
            cannot_watch("north"),
            "this session's terminal host was started before sessions could be watched; \
             restart it with `senate stop north` then `senate restore north`"
        );
    }

    #[test]
    fn a_wide_character_cut_by_the_edge_is_a_blank() {
        let session = screen(2, 10, "ab你好".as_bytes());
        let mut out = String::new();
        // Three columns: `a`, `b`, and half of 你.
        let area = Rect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 3,
        };
        paint_screen(&mut out, session.screen(), area, 0, false);
        let terminal = shows(2, 10, &out);
        assert_eq!(terminal.screen().rows(0, 10).next().unwrap(), "ab ");
    }

    #[test]
    fn keys_move_open_and_leave_and_everything_else_is_dropped() {
        let read = |bytes: &[u8]| {
            let mut pending = bytes.to_vec();
            let found = keys(&mut pending);
            (found, pending)
        };
        assert_eq!(
            read(b"h\x1b[Cj\x1bOA\x1b[5~\x1b[6~\r").0,
            [
                Key::Left,
                Key::Right,
                Key::Down,
                Key::Up,
                Key::PageUp,
                Key::PageDown,
                Key::Open
            ]
        );
        // Typing is not a key of the viewer, whatever it is.
        assert_eq!(read(b"rm -rf\x1b[200~x\x1b[201~").0, []);
        assert_eq!(read(b"q").0, [Key::Quit]);
        // Ctrl-], in either encoding, leaves at once.
        assert_eq!(read(b"x\x1d\r").0, [Key::Detach]);
        assert_eq!(read(b"\x1b[93;5u").0, [Key::Detach]);
        // An arrow split across two reads is kept until it is whole.
        let (found, pending) = read(b"l\x1b[");
        assert_eq!(
            (found, pending.as_slice()),
            (vec![Key::Right], &b"\x1b["[..])
        );
        let mut pending = pending;
        pending.push(b'B');
        assert_eq!(keys(&mut pending), [Key::Down]);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_header_is_cut_to_its_tile_by_what_it_takes_on_screen() {
        assert_eq!(fit("demo · senate", 8), "demo · s");
        assert_eq!(fit("审阅 · x", 5), "审阅 ");
        assert_eq!(fit("审阅", 3), "审 ", "half a wide character is a blank");
        assert_eq!(fit("ab", 4), "ab  ");
    }
}
