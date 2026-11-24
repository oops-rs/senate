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
//! In the grid, each tile asks its session's host to fit the session to
//! the tile ([`super::watch`]'s fit), whenever the tile's size changes, so
//! the TUI repaints to fill it. The host does so only while nobody is
//! attached: an attached session keeps its viewer's size, and its tile
//! shows a part of the screen, its left edge and the rows that end at the
//! last line with anything on it (or the cursor's line), since a TUI keeps
//! its prompt and its newest output at the bottom. `senate attach
//! --read-only` asks for no fit: it only looks at the one session.
//!
//! # The grid
//!
//! `senate session watch` lays the sessions out in as even a grid as the
//! terminal has room for, each tile at least [`MIN_TILE_COLS`] columns
//! and [`MIN_TILE_ROWS`] rows; when they do not all fit, in pages. Each
//! tile has a header naming its session; a paused or ended one is dimmed.
//! The list of sessions is asked for again every couple of seconds, so
//! sessions come and go as they start and stop.
//!
//! | key | does |
//! | --- | --- |
//! | `/`, Ctrl-P | find a session by name ([`finder`]) |
//! | arrows, `h` `j` `k` `l` | move to another tile; a count first repeats it (`3j`) |
//! | `gg`, `G` | the first tile, the last; with a count, that tile (`5G`) |
//! | `H` `L`, `[` `]`, PgUp / PgDn | the page before, the page after |
//! | `n`, `N` | the next, the previous tile the last find matches |
//! | Enter, `o` | attach to the tile's session, as `senate attach`; detaching comes back here |
//! | `?` | the keys |
//! | `q`, Ctrl-] | leave |
//!
//! Every key is the viewer's: nothing typed in the grid or the finder
//! reaches a session.
//!
//! `senate attach --read-only` is the one tile, full screen, under one
//! status line. The wheel scrolls main-screen history in the focused tile;
//! Ctrl-] leaves a read-only attach.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use unicode_width::UnicodeWidthStr as _;

use super::scroll::{self, Mouse};
use super::terminal::{RESET_DISPLAY, RawTerminal, split_input};
use super::watch::{Mirror, SyncFrames, Watch};
use crate::paths::Paths;

mod finder;
use finder::{Edit, Finder};

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
    /// What the header says of it: name, runtime, model, state.
    pub title: String,
    /// The host's attach socket (`tty`).
    pub tty: PathBuf,
    /// Shown dimmed: paused, or otherwise not running now.
    pub dim: bool,
    /// A running host currently has an interactive viewer.
    pub attached: bool,
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
    /// The size this tile asks its session to be fitted to, rows then
    /// columns; `None` for a tile that asks for no fit (a read-only attach).
    fit: Option<tokio::sync::watch::Sender<Option<(u16, u16)>>>,
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
         restart it with `senate session stop {alias}` then `senate session restore {alias}`"
    )
}

/// What a paused session's tile says.
pub fn paused_note(alias: &str) -> String {
    format!("paused; `senate session resume {alias}` brings it back")
}

impl Tile {
    /// Watch `session`; a tile that `fits` asks for its session to be
    /// fitted to it ([`Tile::fit_to`]).
    fn open(
        session: Session,
        changed: Arc<Notify>,
        fits: bool,
        remote: Option<crate::remote::Client>,
    ) -> Self {
        let fits = fits && remote.is_none();
        let session_id = session.key.clone();
        let shown_ = Arc::new(Mutex::new(Shown::default()));
        let (fit, mut wanted) = tokio::sync::watch::channel(None);
        let watching = tokio::spawn({
            let state = shown_.clone();
            let socket = session.watch_socket();
            let alias = session.alias.clone();
            let dim = session.dim;
            async move {
                if dim {
                    // Paused: no terminal to watch until it is resumed.
                    shown(&state).note = Some(if remote.is_some() {
                        "not running on the target machine".into()
                    } else {
                        paused_note(&alias)
                    });
                    changed.notify_one();
                    return;
                }
                let link = match &remote {
                    Some(client) => match client.stream(session_id, false).await {
                        Ok(stream) => Watch::remote(stream).await,
                        Err(error) => Err(error),
                    },
                    None => Watch::open(&socket).await,
                };
                let note = match link {
                    Ok(mut watch) => {
                        shown(&state).mirror = Some(Mirror::default());
                        // Kept as long as the watch: dropping it ends it.
                        let mut fitter = watch.fitter();
                        let fitting = async {
                            loop {
                                if wanted.changed().await.is_err() {
                                    std::future::pending::<()>().await;
                                }
                                let size = *wanted.borrow_and_update();
                                if let (Some(fitter), Some((rows, cols))) = (fitter.as_mut(), size)
                                    && fitter.fit(rows, cols).await.is_err()
                                {
                                    // The host is gone; the reading says so.
                                    std::future::pending::<()>().await;
                                }
                            }
                        };
                        let reading = async {
                            let mut sync = SyncFrames::default();
                            loop {
                                let next = watch.next();
                                tokio::pin!(next);
                                let result = if let Some(deadline) = sync.deadline() {
                                    tokio::select! {
                                        result = &mut next => result,
                                        _ = tokio::time::sleep_until(deadline) => {
                                            if let Some(mirror) = shown(&state).mirror.as_mut()
                                                && sync.finish(mirror)
                                            {
                                                changed.notify_one();
                                            }
                                            // Keep the in-flight frame read: cancelling
                                            // it could discard a partially read packet.
                                            next.await
                                        }
                                    }
                                } else {
                                    next.await
                                };
                                match result {
                                    Ok(Some(frame)) => {
                                        if let Some(mirror) = shown(&state).mirror.as_mut()
                                            && sync.apply(mirror, &frame)
                                        {
                                            changed.notify_one();
                                        }
                                    }
                                    Ok(None) => break "the session ended".to_owned(),
                                    Err(error) => break format!("the watch broke: {error:#}"),
                                }
                            }
                        };
                        tokio::select! {
                            note = reading => note,
                            () = fitting => unreachable!("fitting never ends"),
                        }
                    }
                    Err(_)
                        if remote.is_none()
                            && !socket.exists()
                            && socket.with_file_name("tty").exists() =>
                    {
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
            fit: fits.then_some(fit),
            watching,
        }
    }

    /// Ask for the session to be fitted to `rows` × `cols`, when this tile
    /// fits and that is not what it asked for last.
    fn fit_to(&self, rows: u16, cols: u16) {
        if let Some(fit) = &self.fit
            && rows > 0
            && cols > 0
        {
            fit.send_if_modified(|asked| {
                let changed = *asked != Some((rows, cols));
                *asked = Some((rows, cols));
                changed
            });
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

/// Budget for terminals that render ambiguous characters as wide, and for
/// prompt glyphs some terminals render with a double-width symbol font.
fn paint_width(text: &str) -> usize {
    match text {
        "›" | "⏺" => 2,
        _ => text.width().max(text.width_cjk()),
    }
}

/// `text` cut or padded to at most `width` columns on either terminal.
fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        if ch.is_control() {
            continue;
        }
        let w = paint_width(ch.encode_utf8(&mut [0; 4]));
        if used + w > width || (w == 0 && used == 0) {
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

/// Erase cells without advancing the cursor or wrapping at the right edge.
fn erase(out: &mut String, cols: u16) {
    if cols != 0 {
        let _ = write!(out, "\x1b[{cols}X");
    }
}

fn color(out: &mut String, color: vt100::Color, foreground: bool) {
    let base = if foreground { 38 } else { 48 };
    match color {
        // Each cell style starts with SGR 0, which restores the viewer defaults.
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
    if screen.scrollback() != 0 {
        return 0;
    }
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

/// A frame is made of positioned, non-overlapping row regions. Compare
/// the rendered bytes of each region, so wide glyph clipping and styling
/// are identical in full and incremental paints.
#[derive(Default)]
struct Frame {
    size: (u16, u16),
    regions: Vec<((u16, u16), String)>,
    cursor: Option<(u16, u16)>,
    /// The regions from this one on are painted over the others (the
    /// finder, the help): repainted whenever one before them is.
    overlay: Option<usize>,
}

impl Frame {
    fn region(&mut self, row: u16, col: u16) -> &mut String {
        let index = self.regions.len();
        self.regions.push(((row, col), String::new()));
        &mut self.regions[index].1
    }

    fn changes(&self, previous: Option<&Self>, recover: bool) -> String {
        let comparable = previous.filter(|prev| {
            self.size == prev.size
                && self
                    .regions
                    .iter()
                    .map(|(at, _)| at)
                    .eq(prev.regions.iter().map(|(at, _)| at))
        });
        let mut out = String::new();
        if recover || (previous.is_some() && comparable.is_none()) {
            out.push_str("\x1b[2J");
        }
        if self.cursor.is_none() && previous.is_none_or(|prev| prev.cursor.is_some()) {
            out.push_str("\x1b[?25l");
        }
        let mut repainted = false;
        for (index, &((row, col), ref bytes)) in self.regions.iter().enumerate() {
            let over = repainted && self.overlay.is_some_and(|first| index >= first);
            if !over && comparable.is_some_and(|prev| prev.regions[index].1 == *bytes) {
                continue;
            }
            repainted = true;
            at(&mut out, row, col);
            out.push_str(bytes);
        }
        if let Some((row, col)) = self.cursor
            && (!out.is_empty() || previous.is_none_or(|prev| prev.cursor != self.cursor))
        {
            at(&mut out, row, col);
            if previous.is_none_or(|prev| prev.cursor.is_none()) {
                out.push_str("\x1b[?25h");
            }
        }
        out
    }
}

/// Paint `rows` × `cols` of `screen` from its row `top`, left edge, at
/// (`row`, `col`). Returns where the session's cursor is in the terminal,
/// when it is shown and inside what was painted.
fn paint_screen(
    frame: &mut Frame,
    screen: &vt100::Screen,
    area: Rect,
    top: u16,
    dim: bool,
) -> Option<(u16, u16)> {
    let reset = if dim { "\x1b[0;2m" } else { "\x1b[0m" };
    for line in 0..area.rows {
        let out = frame.region(area.row + line, area.col);
        out.push_str(reset);
        erase(out, area.cols);
        let mut current = None;
        let mut col = 0;
        let mut remaining = usize::from(area.cols);
        while col < area.cols && remaining != 0 {
            let cell = screen.cell(top + line, col);
            let wide = cell.is_some_and(vt100::Cell::is_wide);
            let step = if wide { 2 } else { 1 };
            let width = cell.filter(|cell| cell.has_contents()).map_or(1, |cell| {
                paint_width(cell.contents()).max(usize::from(step))
            });
            match cell {
                // A wide character cut by the tile's edge, or the half of
                // one that starts left of it: a blank.
                Some(cell)
                    if cell.has_contents()
                        && !cell.is_wide_continuation()
                        && width <= remaining =>
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
                    col += step;
                    remaining -= width;
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
                    remaining -= 1;
                }
                _ => {
                    if current.is_some() {
                        out.push_str(reset);
                        current = None;
                    }
                    out.push(' ');
                    col += 1;
                    remaining -= 1;
                }
            }
        }
        out.push_str("\x1b[0m");
    }
    let (cursor_row, cursor_col) = screen.cursor_position();
    (screen.scrollback() == 0
        && !screen.hide_cursor()
        && cursor_row >= top
        && cursor_row - top < area.rows
        && cursor_col < area.cols)
        .then(|| (area.row + cursor_row - top, area.col + cursor_col))
}

/// Paint lines of plain text into `area`, the rest of it blank.
fn paint_note(frame: &mut Frame, area: Rect, note: &str) {
    let lines = wrapped(note, usize::from(area.cols));
    for line in 0..area.rows {
        let out = frame.region(area.row + line, area.col);
        let text = lines.get(usize::from(line)).map_or("", String::as_str);
        out.push_str("\x1b[0;2m");
        erase(out, area.cols);
        out.push_str(&fit(text, usize::from(area.cols)));
        out.push_str("\x1b[0m");
    }
}

/// Paint one tile: its header, then its session's screen or why there is
/// none.
fn paint_tile(frame: &mut Frame, tile: &Tile, area: Rect, focused: bool) -> Option<(u16, u16)> {
    let state = shown(&tile.shown);
    let dim = tile.session.dim || state.note.is_some();
    let out = frame.region(area.row, area.col);
    let style = match (focused, dim) {
        (true, _) => "\x1b[0;1;7m",
        (false, true) => "\x1b[0;2;4m",
        (false, false) => "\x1b[0;4m",
    };
    let mark = if focused { "▶ " } else { "  " };
    let title = format!("{mark}{}", tile.session.title);
    out.push_str(style);
    erase(out, area.cols);
    out.push_str(&fit(&title, usize::from(area.cols)));
    out.push_str("\x1b[0m");
    let body = Rect {
        row: area.row + 1,
        rows: area.rows.saturating_sub(1),
        // A blank column keeps this tile's text apart from the next one's.
        cols: area.cols.saturating_sub(1).max(1),
        ..area
    };
    if body.cols < area.cols {
        for line in 0..body.rows {
            let gutter = frame.region(body.row + line, area.col + body.cols);
            gutter.push_str("\x1b[0m");
            erase(gutter, 1);
        }
    }
    tile.fit_to(body.rows, body.cols);
    match (&state.mirror, &state.note) {
        (Some(mirror), None) => {
            let screen = mirror.screen();
            paint_screen(frame, screen, body, top_row(screen, body.rows), dim)
        }
        (_, note) => {
            paint_note(frame, body, note.as_deref().unwrap_or("connecting…"));
            None
        }
    }
}

// ----- keys ------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Key {
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Scroll(isize),
    Typing,
    Open,
    /// `/`, Ctrl-P: open the finder.
    Find,
    /// `?`.
    Help,
    /// `g` (twice: the first tile).
    G,
    /// `G`: the last tile.
    Last,
    /// `n`, `N`: the next, the previous match of the last find.
    Next,
    Prev,
    /// A digit of a count.
    Digit(u8),
    /// `q`: leaves the grid; a key like any other in a read-only attach.
    Quit,
    /// Ctrl-]: leaves either.
    Detach,
}

/// The keys in `pending`, leaving behind only the start of one that may
/// still be arriving. Anything that is no key of the viewer is dropped.
pub(super) fn keys(pending: &mut Vec<u8>) -> Vec<Key> {
    decode_keys(pending, None)
}

/// The keys in `pending`, up to and including [`Key::Find`]: what follows
/// it is the finder's, so it is left in `pending`.
fn decode_keys(pending: &mut Vec<u8>, unknown: Option<Key>) -> Vec<Key> {
    let mut out = Vec::new();
    // The detach key ends everything, wherever it stands.
    let (before, detach) = split_input(pending);
    let mut at = 0;
    while at < before {
        let rest = &pending[at..before];
        match scroll::mouse(rest) {
            Mouse::Report { len, scroll } => {
                if scroll != 0 {
                    out.push(Key::Scroll(scroll));
                }
                at += len;
                continue;
            }
            Mouse::Incomplete if !detach => break,
            _ => {}
        }
        const KITTY_ENTER: &[&[u8]] = &[b"\x1b[13u", b"\x1b[13;1u", b"\x1b[13;1:1u"];
        if let Some(key) = KITTY_ENTER.iter().find(|key| rest.starts_with(key)) {
            out.push(Key::Open);
            at += key.len();
            continue;
        }
        if !detach && KITTY_ENTER.iter().any(|key| key.starts_with(rest)) {
            break;
        }
        let (key, len) = match rest {
            // Ctrl-C, in raw mode a plain byte.
            [b'q' | 0x03, ..] => (Some(Key::Quit), 1),
            [b'\r' | b'\n' | b'o', ..] => (Some(Key::Open), 1),
            [b'h', ..] => (Some(Key::Left), 1),
            [b'l', ..] => (Some(Key::Right), 1),
            [b'k', ..] => (Some(Key::Up), 1),
            [b'j', ..] => (Some(Key::Down), 1),
            [b'H' | b'[', ..] => (Some(Key::PageUp), 1),
            [b'L' | b']', ..] => (Some(Key::PageDown), 1),
            [b'/' | 0x10, ..] => (Some(Key::Find), 1),
            [b'?', ..] => (Some(Key::Help), 1),
            [b'g', ..] => (Some(Key::G), 1),
            [b'G', ..] => (Some(Key::Last), 1),
            [b'n', ..] => (Some(Key::Next), 1),
            [b'N', ..] => (Some(Key::Prev), 1),
            [d @ b'0'..=b'9', ..] => (Some(Key::Digit(d - b'0')), 1),
            [0x1b, b'[', b'1', b'1', b'2', b';', b'5', b'u', ..] => (Some(Key::Find), 8),
            [0x1b, b'[' | b'O', b'A', ..] => (Some(Key::Up), 3),
            [0x1b, b'[' | b'O', b'B', ..] => (Some(Key::Down), 3),
            [0x1b, b'[' | b'O', b'C', ..] => (Some(Key::Right), 3),
            [0x1b, b'[' | b'O', b'D', ..] => (Some(Key::Left), 3),
            [0x1b, b'[', b'5', b'~', ..] => (Some(Key::PageUp), 4),
            [0x1b, b'[', b'6', b'~', ..] => (Some(Key::PageDown), 4),
            // The start of one of those, still arriving.
            [0x1b] | [0x1b, b'[' | b'O'] | [0x1b, b'[', b'5' | b'6'] if !detach => break,
            // A paste is no key, whatever it holds: skipped to its end.
            [0x1b, b'[', b'2', b'0', b'0', b'~', ..] => {
                match rest.windows(6).position(|w| w == b"\x1b[201~") {
                    Some(end) => (unknown, end + 6),
                    None if !detach => break,
                    None => (unknown, rest.len()),
                }
            }
            // Any other sequence is skipped whole, so none of its bytes
            // reads as a key of its own.
            [0x1b, b'[', tail @ ..] => match tail.iter().position(|b| (0x40..=0x7e).contains(b)) {
                Some(end) => (unknown, end + 3),
                None if !detach => break,
                None => (unknown, rest.len()),
            },
            [0x1b, b'O', _, ..] => (unknown, 3),
            [0x1b, _, ..] => (unknown, 2),
            _ => (unknown, 1),
        };
        out.extend(key);
        at += len;
        if key == Some(Key::Find) {
            break;
        }
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `senate attach --read-only`: show one session.
    One,
    /// `senate session watch`.
    Grid,
}

struct Viewer {
    remote: Option<crate::remote::Client>,
    mode: Mode,
    tiles: Vec<Tile>,
    focus: usize,
    changed: Arc<Notify>,
    /// The finder, while it is open.
    finder: Option<Finder>,
    /// The help, while it is shown.
    help: bool,
    /// A count typed before a key (`3j`).
    count: Option<usize>,
    /// The first `g` of `gg`.
    g: bool,
    /// What the finder last found by: `n` and `N` find by it again.
    last_find: String,
}

/// The most a count is taken to be.
const MOST_COUNT: usize = 9999;

impl Viewer {
    fn new(mode: Mode, remote: Option<crate::remote::Client>) -> Self {
        Self {
            remote,
            mode,
            tiles: Vec::new(),
            focus: 0,
            changed: Arc::new(Notify::new()),
            finder: None,
            help: false,
            count: None,
            g: false,
            last_find: String::new(),
        }
    }

    fn titles(&self) -> Vec<&str> {
        self.tiles
            .iter()
            .map(|t| t.session.title.as_str())
            .collect()
    }

    /// The tiles the finder's query matches, best first.
    fn found(&self) -> Vec<(usize, finder::Match)> {
        let query = self.finder.as_ref().map_or("", |f| f.query.as_str());
        finder::rank(query, self.titles())
    }

    /// The next tile after the focus (or before it, `back`), round the end,
    /// that the last find matches.
    fn find_again(&self, back: bool) -> Option<usize> {
        let n = self.tiles.len();
        if self.last_find.trim().is_empty() || n == 0 {
            return None;
        }
        (1..=n)
            .map(|step| {
                if back {
                    (self.focus + n * step - step) % n
                } else {
                    (self.focus + step) % n
                }
            })
            .find(|&at| finder::fuzzy(&self.last_find, &self.tiles[at].session.title).is_some())
    }

    /// The keys in `pending`, on a terminal `rows` × `cols`: whether the
    /// screen changes, and whether the viewer is left. Every key is the
    /// viewer's own; none is sent anywhere.
    fn input(&mut self, pending: &mut Vec<u8>, rows: u16, cols: u16) -> (bool, Option<Left>) {
        let (across, down) = grid(self.tiles.len(), rows, cols);
        let (across, per_page) = (usize::from(across), usize::from(across) * usize::from(down));
        let mut dirty = false;
        loop {
            if self.mode == Mode::Grid
                && let Some(finder) = self.finder.as_mut()
            {
                let edits = finder.keys(pending);
                if edits.is_empty() {
                    return (dirty, None);
                }
                for edit in edits {
                    dirty = true;
                    if let Some(left) = self.edit(edit) {
                        return (dirty, Some(left));
                    }
                }
                if self.finder.is_some() {
                    return (dirty, None);
                }
                // Closed: the rest is the grid's.
                continue;
            }
            let keys = decode_keys(pending, Some(Key::Typing));
            if keys.is_empty() {
                return (dirty, None);
            }
            for key in keys {
                if let Key::Scroll(rows) = key {
                    dirty |= self.scroll(Some(rows));
                    continue;
                }
                dirty |= self.scroll(None);
                if let Some(left) = self.key(key, across, per_page, &mut dirty) {
                    return (dirty, Some(left));
                }
            }
            if self.finder.is_none() {
                return (dirty, None);
            }
        }
    }

    /// One key in the grid (or a read-only attach).
    fn key(&mut self, key: Key, across: usize, per_page: usize, dirty: &mut bool) -> Option<Left> {
        if key == Key::Detach {
            return Some(Left::Quit);
        }
        // In a read-only attach every other key is typing, and typing goes
        // nowhere.
        if self.mode == Mode::One {
            return None;
        }
        if std::mem::take(&mut self.help) {
            // Any key closes the help, and does nothing else.
            *dirty = true;
            return None;
        }
        let count = match key {
            Key::Digit(d) if d != 0 || self.count.is_some() => {
                let count = self.count.unwrap_or(0) * 10 + usize::from(d);
                self.count = Some(count.min(MOST_COUNT));
                *dirty = true;
                return None;
            }
            Key::G if !self.g => {
                self.g = true;
                return None;
            }
            _ => self.count.take(),
        };
        let gg = std::mem::take(&mut self.g) && key == Key::G;
        *dirty = true;
        let last = self.tiles.len().saturating_sub(1);
        match key {
            Key::Quit => return Some(Left::Quit),
            Key::Open => {
                return self
                    .tiles
                    .get(self.focus)
                    .map(|tile| Left::Attach(tile.session.clone()));
            }
            Key::Find => self.finder = Some(Finder::new(self.focus)),
            Key::Help => self.help = true,
            Key::G if gg => self.focus = count.map_or(0, |n| n.saturating_sub(1)).min(last),
            Key::Last => self.focus = count.map_or(last, |n| n.saturating_sub(1)).min(last),
            Key::Next | Key::Prev => {
                for _ in 0..count.unwrap_or(1) {
                    if let Some(at) = self.find_again(key == Key::Prev) {
                        self.focus = at;
                    }
                }
            }
            Key::Left | Key::Right | Key::Up | Key::Down | Key::PageUp | Key::PageDown => {
                for _ in 0..count.unwrap_or(1) {
                    self.step(key, across, per_page);
                }
            }
            Key::G | Key::Digit(_) | Key::Typing | Key::Scroll(_) | Key::Detach => {}
        }
        None
    }

    /// One key in the finder.
    fn edit(&mut self, edit: Edit) -> Option<Left> {
        let found = self.found();
        let finder = self.finder.as_mut()?;
        match edit {
            Edit::Up => finder.selected = finder.selected.min(found.len()).saturating_sub(1),
            Edit::Down => {
                finder.selected = (finder.selected + 1).min(found.len().saturating_sub(1));
            }
            Edit::Detach => return Some(Left::Quit),
            Edit::Close => self.finder = None,
            Edit::Accept | Edit::Show => {
                let chosen = found.get(finder.selected.min(found.len().saturating_sub(1)));
                // Nothing matches: the finder stays open.
                let (tile, _) = chosen?;
                self.focus = *tile;
                self.last_find = std::mem::take(&mut finder.query);
                self.finder = None;
                if edit == Edit::Accept {
                    return Some(Left::Attach(self.tiles[self.focus].session.clone()));
                }
            }
            edit => finder.edit(edit),
        }
        None
    }

    /// Keep tiles whose source is still the same, in the order listed.
    /// The focus stays on its session when it can.
    fn list(&mut self, sessions: Vec<Session>) {
        let focused = self.tiles.get(self.focus).map(|t| t.session.key.clone());
        let mut known: BTreeMap<String, Tile> = std::mem::take(&mut self.tiles)
            .into_iter()
            .map(|tile| (tile.session.key.clone(), tile))
            .collect();
        for session in sessions {
            let tile = match known.remove(&session.key) {
                Some(mut tile)
                    if tile.session.tty == session.tty
                        && tile.session.dim == session.dim
                        && (self.remote.is_none()
                            || !tile.watching.is_finished()
                            || session.dim) =>
                {
                    tile.session = session;
                    tile
                }
                _ => Tile::open(
                    session,
                    self.changed.clone(),
                    self.mode == Mode::Grid,
                    self.remote.clone(),
                ),
            };
            self.tiles.push(tile);
        }
        self.focus = focused
            .and_then(|key| self.tiles.iter().position(|t| t.session.key == key))
            .unwrap_or(self.focus)
            .min(self.tiles.len().saturating_sub(1));
    }

    fn compose(&self, rows: u16, cols: u16) -> Frame {
        let mut frame = Frame {
            size: (rows, cols),
            ..Frame::default()
        };
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
                    return frame;
                };
                let state = shown(&tile.shown);
                match (&state.mirror, &state.note) {
                    (Some(mirror), None) => {
                        let screen = mirror.screen();
                        cursor = paint_screen(
                            &mut frame,
                            screen,
                            area,
                            top_row(screen, area.rows),
                            false,
                        );
                    }
                    (_, note) => {
                        paint_note(&mut frame, area, note.as_deref().unwrap_or("connecting…"));
                    }
                }
                format!(" read-only · {} · Ctrl-] leaves", tile.session.title)
            }
            Mode::Grid if self.tiles.is_empty() => {
                paint_note(
                    &mut frame,
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
                            paint_tile(
                                &mut frame,
                                tile,
                                area,
                                page * per_page + slot == self.focus,
                            );
                        }
                        None => paint_note(&mut frame, area, ""),
                    }
                }
                let paging = if pages > 1 {
                    format!(" · page {}/{pages}", page + 1)
                } else {
                    String::new()
                };
                let typed = match (self.count, self.g) {
                    (Some(n), g) => format!(" · {n}{}", if g { "g" } else { "" }),
                    (None, true) => " · g".to_owned(),
                    (None, false) => String::new(),
                };
                format!(
                    " {} sessions{paging}{typed} · / find · hjkl move · Enter attach · ? help · q quit",
                    self.tiles.len()
                )
            }
        };
        let status = if self.mode == Mode::Grid && self.help {
            frame.overlay = Some(frame.regions.len());
            finder::paint_help(
                &mut frame,
                finder::centred(rows, cols, finder::help_height(), 64),
            );
            " ? help · any key closes".to_owned()
        } else if self.mode == Mode::Grid
            && let Some(open) = &self.finder
        {
            frame.overlay = Some(frame.regions.len());
            let area = finder::centred(rows, cols, finder::height(self.tiles.len()), 84);
            let titles = self.titles();
            cursor = finder::paint(&mut frame, area, open, &self.found(), &titles);
            " find · Enter attach · Tab show · Ctrl-N/P move · Ctrl-U clear · Esc close".to_owned()
        } else {
            status
        };
        // The status line is painted over too.
        let out = frame.region(status_row, 0);
        out.push_str("\x1b[0;7m");
        erase(out, cols);
        out.push_str(&fit(&status, usize::from(cols)));
        out.push_str("\x1b[0m");
        frame.cursor = cursor;
        frame
    }

    #[cfg(test)]
    fn paint(&self, rows: u16, cols: u16) -> String {
        self.compose(rows, cols).changes(None, false)
    }

    fn focused_shown(&self) -> Option<MutexGuard<'_, Shown>> {
        self.tiles.get(self.focus).map(|tile| shown(&tile.shown))
    }

    fn captures_wheel(&self) -> bool {
        self.focused_shown().is_some_and(|state| {
            state.note.is_none()
                && state
                    .mirror
                    .as_ref()
                    .is_some_and(|mirror| !mirror.screen().alternate_screen())
        })
    }

    fn scroll(&self, rows: Option<isize>) -> bool {
        self.focused_shown().is_some_and(|mut state| {
            state.mirror.as_mut().is_some_and(|mirror| match rows {
                Some(rows) => mirror.scroll(rows),
                None => mirror.live(),
            })
        })
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
            _ => self.focus,
        }
        .min(last);
    }
}

/// What ended one stay in the viewer.
enum Left {
    Quit,
    AllAttached,
    /// Attach to this session, then come back.
    Attach(Session),
}

async fn stay(
    viewer: &mut Viewer,
    mut relist: Option<&mut GridListing<'_>>,
) -> anyhow::Result<Left> {
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
    let mut captures_wheel = false;
    let mut previous = None;
    let mut recover = false;
    let left = loop {
        if dirty {
            let (rows, cols) = terminal.window()?;
            let frame = viewer.compose(rows, cols);
            let mut changes = frame.changes(previous.as_ref(), recover);
            let capture = viewer.captures_wheel();
            if recover || capture != captures_wheel {
                let mode = if capture {
                    scroll::MOUSE_ON
                } else {
                    scroll::MOUSE_OFF
                };
                changes.push_str(std::str::from_utf8(mode).unwrap_or_default());
            }
            if !changes.is_empty() {
                // Terminals that support synchronized output present the
                // whole patch at once. Other terminals ignore the brackets.
                // Cancel an old partial CSI and release its synchronized
                // paint before starting the recovery frame.
                let prefix = if recover { "\x18\x1b[?2026l" } else { "" };
                let patch = format!("{prefix}\x1b[?2026h{changes}\x1b[?2026l");
                match tokio::time::timeout(STUCK, terminal.draw(patch.as_bytes())).await {
                    Ok(drawn) => drawn?,
                    Err(_) => {
                        stuck = true;
                        recover = true;
                        previous = None;
                    }
                }
            }
            if !stuck {
                previous = Some(frame);
                captures_wheel = capture;
                recover = false;
                dirty = false;
                if !changes.is_empty() {
                    tokio::time::sleep(FRAME).await;
                }
            }
        }
        tokio::select! {
            () = viewer.changed.notified(), if !dirty => dirty = true,
            () = tokio::time::sleep(FRAME), if std::mem::take(&mut stuck) => {}
            _ = winch.recv() => dirty = true,
            _ = listing.tick(), if relist.is_some() => {
                if let Some(relist) = relist.as_mut()
                    && let Ok(sessions) = (relist.fetch)().await
                {
                    let Some(sessions) = relist.visible(sessions) else { break Left::AllAttached };
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
                let (changed, leave) = viewer.input(&mut pending, rows, cols);
                dirty |= changed;
                if let Some(leave) = leave {
                    break leave;
                }
            }
            _ = hup.recv() => break Left::Quit,
            _ = term.recv() => break Left::Quit,
            _ = interrupt.recv() => break Left::Quit,
        }
    };
    let cleanup = [b"\x18\x1b[?2026l".as_slice(), RESET_DISPLAY].concat();
    let _ = tokio::time::timeout(Duration::from_millis(250), terminal.draw(&cleanup)).await;
    Ok(left)
}

type Listing = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<Vec<Session>>> + Send>>;
type Relist<'a> = Box<dyn FnMut() -> Listing + Send + 'a>;

struct GridListing<'a> {
    fetch: Relist<'a>,
    all: bool,
    just_detached: Option<String>,
}

impl GridListing<'_> {
    /// None means there were sessions, but interactive viewers hid all of them.
    fn visible(&mut self, mut sessions: Vec<Session>) -> Option<Vec<Session>> {
        let just_detached = self.just_detached.take();
        if self.all || sessions.is_empty() {
            return Some(sessions);
        }
        // Returning from Enter can race the host publishing its detach.
        // Protect that first refresh only, so later external attaches are observed.
        sessions.retain(|s| !s.attached || Some(&s.key) == just_detached.as_ref());
        (!sessions.is_empty()).then_some(sessions)
    }
}

/// `senate attach --read-only`: show `session`'s terminal until Ctrl-],
/// the person closes the terminal, or the session ends.
pub async fn watch_one(session: Session) -> anyhow::Result<()> {
    anyhow::ensure!(
        session.watch_socket().exists() || !session.tty.exists(),
        "{}",
        cannot_watch(&session.alias)
    );
    let mut viewer = Viewer::new(Mode::One, None);
    viewer.list(vec![session]);
    stay(&mut viewer, None).await.map(drop)
}

/// `senate session watch`: visible hosted sessions, asked again every
/// couple of seconds, until `q` or Ctrl-]. Enter attaches to the focused
/// one and comes back when that detaches.
pub async fn watch_all(
    paths: &Paths,
    all: bool,
    mut sessions: impl FnMut() -> Listing + Send,
) -> anyhow::Result<()> {
    watch_grid(paths, all, &mut sessions, None).await
}

pub(crate) async fn watch_remote(
    paths: &Paths,
    all: bool,
    mut sessions: impl FnMut() -> Listing + Send,
    client: crate::remote::Client,
) -> anyhow::Result<()> {
    watch_grid(paths, all, &mut sessions, Some(client)).await
}

pub(crate) async fn watch_remote_one(
    session: Session,
    client: crate::remote::Client,
) -> anyhow::Result<()> {
    let mut viewer = Viewer::new(Mode::One, Some(client));
    viewer.list(vec![session]);
    stay(&mut viewer, None).await.map(drop)
}

async fn watch_grid(
    paths: &Paths,
    all: bool,
    mut sessions: impl FnMut() -> Listing + Send,
    remote: Option<crate::remote::Client>,
) -> anyhow::Result<()> {
    let first = sessions().await?;
    let mut grid = GridListing {
        fetch: Box::new(sessions),
        all,
        just_detached: None,
    };
    const HINT: &str = "All hosted terminals have an interactive viewer. Use senate session watch --all to include them.";
    let Some(first) = grid.visible(first) else {
        println!("{HINT}");
        return Ok(());
    };
    let mut viewer = Viewer::new(Mode::Grid, remote);
    viewer.list(first);
    loop {
        match stay(&mut viewer, Some(&mut grid)).await? {
            Left::Quit => return Ok(()),
            Left::AllAttached => {
                println!("{HINT}");
                return Ok(());
            }
            Left::Attach(session) => {
                let result = match &viewer.remote {
                    Some(client) => match client.stream(session.key.clone(), true).await {
                        Ok(stream) => super::attach_stream(stream, &session.title, None).await,
                        Err(error) => Err(error),
                    },
                    None => super::attach(&session.tty, paths, &session.title).await,
                };
                if let Err(error) = result {
                    eprintln!("{error:#}");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                } else {
                    grid.just_detached = Some(session.key);
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
    fn grid_visibility_tracks_interactive_viewers_and_the_return_from_enter() {
        let mut listing = GridListing {
            fetch: Box::new(|| unreachable!()),
            all: false,
            just_detached: None,
        };
        let north = viewer_session("north");
        let mut south = viewer_session("south");
        south.attached = true;
        assert_eq!(
            listing.visible(vec![north.clone(), south.clone()]),
            Some(vec![north])
        );
        assert_eq!(listing.visible(vec![south.clone()]), None);
        assert_eq!(listing.visible(vec![]), Some(vec![]));
        listing.just_detached = Some(south.key.clone());
        assert_eq!(
            listing.visible(vec![south.clone()]),
            Some(vec![south.clone()])
        );
        assert_eq!(listing.visible(vec![south.clone()]), None);
        listing.all = true;
        assert_eq!(listing.visible(vec![south.clone()]), Some(vec![south]));
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
        let mut out = Frame::default();
        let area = Rect {
            row: 2,
            col: 5,
            rows: 4,
            cols: 12,
        };
        let cursor = paint_screen(&mut out, session.screen(), area, 3, false);
        let terminal = shows(10, 40, &out.changes(None, false));
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
        let mut out = Frame::default();
        let area = Rect {
            row: 0,
            col: 0,
            rows: 2,
            cols: 4,
        };
        paint_screen(&mut out, session.screen(), area, 0, false);
        let terminal = shows(4, 20, &out.changes(None, false));
        let cell = terminal.screen().cell(0, 0).unwrap();
        assert_eq!(cell.contents(), "b");
        assert!(cell.bold());
        assert_eq!(cell.fgcolor(), vt100::Color::Idx(4));
        assert_eq!(terminal.screen().rows(0, 20).next().unwrap(), "bann");
        assert!(
            !terminal.screen().cell(1, 0).unwrap().bold(),
            "line 1 is plain"
        );
        let plain = terminal.screen().cell(1, 0).unwrap();
        assert_eq!(plain.fgcolor(), vt100::Color::Default);
        assert_eq!(plain.bgcolor(), vt100::Color::Default);
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
                attached: false,
            },
            shown: Arc::new(Mutex::new(Shown {
                mirror: Some(mirror),
                note: None,
            })),
            fit: Some(tokio::sync::watch::channel(None).0),
            watching: tokio::spawn(async {}),
        }
    }

    /// What a tile last asked its session to be fitted to.
    fn asked(tile: &Tile) -> Option<(u16, u16)> {
        *tile.fit.as_ref().expect("a fitting tile").borrow()
    }

    #[tokio::test]
    async fn the_grid_asks_each_tile_shown_to_be_fitted_and_again_on_a_resize() {
        let viewer = Viewer {
            tiles: vec![tile("north", "n", false), tile("south", "s", false)],
            focus: 0,
            ..Viewer::new(Mode::Grid, None)
        };
        viewer.paint(30, 120);
        // Two tiles side by side under the status line; each body is its
        // tile less the header row and the blank column.
        assert_eq!(asked(&viewer.tiles[0]), Some((28, 59)));
        assert_eq!(asked(&viewer.tiles[1]), Some((28, 59)));
        viewer.paint(40, 100);
        assert_eq!(asked(&viewer.tiles[0]), Some((38, 49)));
        assert_eq!(asked(&viewer.tiles[1]), Some((38, 49)));

        // A read-only attach asks for no fit at all.
        let mut one = tile("north", "n", false);
        one.fit = None;
        let viewer = Viewer {
            tiles: vec![one],
            focus: 0,
            ..Viewer::new(Mode::One, None)
        };
        viewer.paint(30, 120);
        assert!(viewer.tiles[0].fit.is_none());
    }

    #[tokio::test]
    async fn the_grid_pages_names_each_tile_and_marks_the_focused_one() {
        let mut viewer = Viewer {
            tiles: ["a", "b", "c", "d", "e"]
                .into_iter()
                .map(|alias| tile(alias, &format!("screen of {alias}"), alias == "c"))
                .collect(),
            focus: 1,
            ..Viewer::new(Mode::Grid, None)
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
            attached: false,
        }
    }

    #[tokio::test]
    async fn a_tile_reopens_its_source_when_paused_or_resumed() {
        let mut viewer = Viewer {
            tiles: Vec::new(),
            focus: 0,
            ..Viewer::new(Mode::Grid, None)
        };
        let mut session = viewer_session("north");
        viewer.list(vec![session.clone()]);
        let running = viewer.tiles[0].shown.clone();

        session.dim = true;
        viewer.list(vec![session.clone()]);
        let paused = viewer.tiles[0].shown.clone();
        assert!(!Arc::ptr_eq(&running, &paused));
        viewer.list(vec![session.clone()]);
        assert!(Arc::ptr_eq(&viewer.tiles[0].shown, &paused));

        session.dim = false;
        session.tty = PathBuf::from("/nowhere/resumed/tty");
        viewer.list(vec![session]);
        assert!(!Arc::ptr_eq(&viewer.tiles[0].shown, &paused));
    }

    #[test]
    fn a_host_from_before_watching_is_named_with_the_commands_to_restart_it() {
        assert_eq!(
            cannot_watch("north"),
            "this session's terminal host was started before sessions could be watched; \
             restart it with `senate session stop north` then `senate session restore north`"
        );
    }

    #[test]
    fn a_wide_character_cut_by_the_edge_is_a_blank() {
        let session = screen(2, 10, "ab你好".as_bytes());
        let mut out = Frame::default();
        // Three columns: `a`, `b`, and half of 你.
        let area = Rect {
            row: 0,
            col: 0,
            rows: 1,
            cols: 3,
        };
        paint_screen(&mut out, session.screen(), area, 0, false);
        let terminal = shows(2, 10, &out.changes(None, false));
        assert_eq!(terminal.screen().rows(0, 10).next().unwrap(), "ab ");
    }

    #[tokio::test]
    async fn symbol_heavy_tiles_clip_and_repaint_their_gutters() {
        let left = tile("left", "", false);
        let source = "ab你\r\nabc›\r\nabc•\r\nabc⏺\r\nabc─";
        shown(&left.shown)
            .mirror
            .as_mut()
            .unwrap()
            .apply(&super::super::watch::Frame::Output(
                source.as_bytes().to_vec(),
            ));
        let mut out = Frame::default();
        paint_tile(
            &mut out,
            &left,
            Rect {
                row: 0,
                col: 2,
                rows: 6,
                cols: 5,
            },
            false,
        );
        let before = "NNNNNNNNN\r\n".repeat(6);
        let terminal = shows(7, 9, &(before + &out.changes(None, false)));
        let lines: Vec<_> = terminal.screen().rows(0, 9).collect();
        assert_eq!(lines[1], "NNab你 NN");
        for line in &lines[2..6] {
            assert_eq!(line, "NNabc  NN");
        }
        assert!(lines[0].starts_with("NN") && lines[0].ends_with("NN"));
        assert_eq!(paint_width("你"), 2);
        for symbol in ["›", "•", "⏺", "─"] {
            assert_eq!(paint_width(symbol), 2);
        }
    }

    #[tokio::test]
    async fn changing_one_grid_row_repaints_only_that_region() {
        let left = tile("north", "before", false);
        let right = tile("south", "unchanged", false);
        let viewer = Viewer {
            tiles: vec![left, right],
            focus: 0,
            ..Viewer::new(Mode::Grid, None)
        };
        let (rows, cols) = (24, 100);
        let first = viewer.compose(rows, cols);
        assert!(
            viewer
                .compose(rows, cols)
                .changes(Some(&first), false)
                .is_empty()
        );
        shown(&viewer.tiles[0].shown)
            .mirror
            .as_mut()
            .unwrap()
            .apply(&super::super::watch::Frame::Output(b"\rnew text".to_vec()));
        let next = viewer.compose(rows, cols);
        let patch = next.changes(Some(&first), false);
        assert!(patch.contains("new text"));
        assert!(!patch.contains("south") && !patch.contains("north"));
        assert!(!patch.contains("unchanged"));
        assert_eq!(
            patch.matches('X').count(),
            1,
            "only one region erases its cells"
        );
        let mut incremental = shows(rows, cols, &first.changes(None, false));
        incremental.process(patch.as_bytes());
        let full = shows(rows, cols, &next.changes(None, false));
        for row in 0..rows {
            for col in 0..cols {
                assert_eq!(
                    incremental.screen().cell(row, col),
                    full.screen().cell(row, col)
                );
            }
        }
        assert_eq!(
            incremental.screen().hide_cursor(),
            full.screen().hide_cursor()
        );
    }

    #[test]
    fn kitty_enter_opens_the_grid_tile_even_when_split() {
        let mut pending = b"\x1b[13;1:".to_vec();
        assert!(keys(&mut pending).is_empty());
        pending.extend_from_slice(b"1u");
        assert_eq!(keys(&mut pending), [Key::Open]);
        assert!(keys(&mut b"\x1b[13;2u".to_vec()).is_empty());
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
        assert_eq!(fit("demo · senate", 8), "demo · ");
        assert_eq!(fit("审阅 · x", 5), "审阅 ");
        assert_eq!(fit("审阅", 3), "审 ", "half a wide character is a blank");
        assert_eq!(fit("ab", 4), "ab  ");
    }

    /// A grid of tiles named `aliases` (`committee::alias`, the first of
    /// each committee its chair), as `senate session watch` titles them.
    fn named_grid(aliases: &[&str]) -> Viewer {
        let mut seen = std::collections::BTreeSet::new();
        Viewer {
            tiles: aliases
                .iter()
                .map(|alias| {
                    let committee = alias.split("::").next().unwrap_or_default();
                    let chair = if seen.insert(committee) {
                        "[chair] "
                    } else {
                        ""
                    };
                    let mut tile = tile(alias, alias, false);
                    tile.session.title = format!("{chair}{alias} · claude · opus · running");
                    tile
                })
                .collect(),
            ..Viewer::new(Mode::Grid, None)
        }
    }

    /// Type `bytes` into `viewer` on a 21 × 80 terminal: two tiles across,
    /// two down.
    fn type_in(viewer: &mut Viewer, bytes: &[u8]) -> Option<Left> {
        let mut pending = bytes.to_vec();
        let (_, left) = viewer.input(&mut pending, 21, 80);
        left
    }

    fn focused(viewer: &Viewer) -> &str {
        &viewer.tiles[viewer.focus].session.alias
    }

    fn attached(left: Option<Left>) -> Option<String> {
        match left {
            Some(Left::Attach(session)) => Some(session.alias),
            _ => None,
        }
    }

    #[tokio::test]
    async fn vim_keys_move_with_counts_pages_and_ends() {
        let names = [
            "s::a", "s::b", "s::c", "s::d", "s::e", "s::f", "s::g", "s::h", "s::i",
        ];
        let mut v = named_grid(&names);
        type_in(&mut v, b"l");
        assert_eq!(focused(&v), "s::b");
        type_in(&mut v, b"3l");
        assert_eq!(focused(&v), "s::e", "a count repeats");
        type_in(&mut v, b"gg");
        assert_eq!(focused(&v), "s::a");
        type_in(&mut v, b"G");
        assert_eq!(focused(&v), "s::i");
        type_in(&mut v, b"5G");
        assert_eq!(focused(&v), "s::e", "the fifth");
        type_in(&mut v, b"2gg");
        assert_eq!(focused(&v), "s::b");
        // Pages of four: L, ], PgDn on; H, [, PgUp back.
        type_in(&mut v, b"L");
        assert_eq!(focused(&v), "s::f");
        type_in(&mut v, b"]");
        assert_eq!(focused(&v), "s::i", "no further than the last");
        type_in(&mut v, b"[H");
        assert_eq!(focused(&v), "s::a");
        type_in(&mut v, b"2j");
        assert_eq!(focused(&v), "s::e", "down two rows of two");
        // A count shows while it is typed; another key forgets it.
        type_in(&mut v, b"1");
        assert!(
            shows(21, 80, &v.paint(21, 80))
                .screen()
                .contents()
                .contains("· 1 ·")
        );
        type_in(&mut v, b"x");
        assert_eq!(v.count, None);
        // `o` and Enter attach; `q` quits.
        assert_eq!(attached(type_in(&mut v, b"o")).as_deref(), Some("s::e"));
        assert_eq!(attached(type_in(&mut v, b"\r")).as_deref(), Some("s::e"));
        assert!(matches!(type_in(&mut v, b"q"), Some(Left::Quit)));
        // The footer says what to press.
        let footer = shows(21, 100, &v.paint(21, 100))
            .screen()
            .rows(0, 100)
            .nth(20)
            .unwrap();
        assert!(
            footer.contains("/ find · hjkl move · Enter attach · ? help · q quit"),
            "{footer:?}"
        );
    }

    #[tokio::test]
    async fn the_finder_narrows_attaches_shows_and_finds_again() {
        let names = [
            "payments::lead",
            "payments::ledger",
            "senate::main",
            "senate::reviewer",
            "refund::builder",
        ];
        let mut v = named_grid(&names);
        // `/` and what follows in the same read: the finder's.
        assert!(type_in(&mut v, b"/rev").is_none());
        assert_eq!(v.found().len(), 1);
        // Tab: focus its tile, on its page, and close.
        type_in(&mut v, b"\t");
        assert!(v.finder.is_none());
        assert_eq!(focused(&v), "senate::reviewer");
        // Ctrl-P opens it too; Ctrl-N/J and arrows move; Enter attaches.
        type_in(&mut v, b"\x10pay led");
        let found: Vec<&str> = v
            .found()
            .iter()
            .map(|(tile, _)| v.tiles[*tile].session.alias.as_str())
            .collect();
        assert_eq!(found, ["payments::ledger", "payments::lead"], "best first");
        type_in(&mut v, b"\x0e");
        assert_eq!(
            attached(type_in(&mut v, b"\r")).as_deref(),
            Some("payments::lead")
        );
        assert!(v.finder.is_none());
        // `n`, `N`: the next, the previous tile the last find matches.
        v.focus = 0;
        type_in(&mut v, b"n");
        assert_eq!(focused(&v), "payments::ledger");
        type_in(&mut v, b"n");
        assert_eq!(focused(&v), "payments::lead", "round the end");
        type_in(&mut v, b"N");
        assert_eq!(focused(&v), "payments::ledger");
        // Esc and Ctrl-C close without moving; nothing matched stays open.
        type_in(&mut v, b"/zzz\r");
        assert!(v.finder.is_some(), "Enter on nothing");
        type_in(&mut v, b"\x1b");
        assert!(v.finder.is_none());
        type_in(&mut v, b"/main\x03");
        assert!(v.finder.is_none());
        assert_eq!(focused(&v), "payments::ledger");
        // Ctrl-] leaves from the finder too.
        assert!(matches!(type_in(&mut v, b"/x\x1d"), Some(Left::Quit)));
    }

    #[tokio::test]
    async fn the_finder_is_a_centred_list_and_fits_any_terminal() {
        let mut v = named_grid(&[
            "payments::lead",
            "payments::ledger",
            "评审::审阅者",
            "senate::main",
        ]);
        type_in(&mut v, b"/led");
        let (rows, cols) = (24, 100);
        let terminal = shows(rows, cols, &v.paint(rows, cols));
        let lines: Vec<String> = terminal.screen().rows(0, cols).collect();
        let header = lines
            .iter()
            .position(|l| l.contains("Find a session"))
            .unwrap();
        assert!(lines[header].contains("2/4"), "{:?}", lines[header]);
        assert!(lines[header + 1].contains("> led"));
        assert!(
            lines[header + 2].contains("▸ payments::ledger · claude"),
            "best first"
        );
        assert!(
            lines[header + 3].contains("[chair] payments::lead"),
            "the chair marked"
        );
        // Centred: as far from the left edge as from the right.
        let left = lines[header].find("Find").unwrap() - 1;
        assert_eq!(left, (100 - 84) / 2);
        // The chosen line is inverted, a matched letter bold.
        let chosen = header + 2;
        let at = lines[chosen].find("ledger").unwrap();
        let col = u16::try_from(lines[chosen][..at].chars().count()).unwrap();
        let row = u16::try_from(chosen).unwrap();
        assert!(terminal.screen().cell(row, col).unwrap().inverse());
        assert!(terminal.screen().cell(row, col).unwrap().bold(), "l of led");
        assert!(
            !terminal.screen().cell(row, col + 3).unwrap().bold(),
            "g is not"
        );
        // The cursor stands after the query.
        assert_eq!(
            terminal.screen().cursor_position(),
            (row - 1, u16::try_from(left + 2 + 3).unwrap())
        );
        // Wide characters in the query and the list.
        type_in(&mut v, "\x15审阅".as_bytes());
        let terminal = shows(rows, cols, &v.paint(rows, cols));
        assert!(
            terminal
                .screen()
                .contents()
                .contains("▸ [chair] 评审::审阅者")
        );
        // A narrow, short terminal: the whole width, what rows there are.
        for (rows, cols) in [(6, 20), (3, 10), (2, 30)] {
            let terminal = shows(rows, cols, &v.paint(rows, cols));
            let text = terminal.screen().contents();
            assert!(text.contains("Find") || rows < 4, "{rows}x{cols}: {text:?}");
        }
        // Typing changes only the finder's rows and the status line.
        let before = v.compose(24, 100);
        type_in(&mut v, b"\x15m");
        let after = v.compose(24, 100);
        let patch = after.changes(Some(&before), false);
        assert!(!patch.contains("\x1b[2J"), "no full repaint per key");
        let mut incremental = shows(24, 100, &before.changes(None, false));
        incremental.process(patch.as_bytes());
        assert_eq!(
            incremental.screen().contents(),
            shows(24, 100, &after.changes(None, false))
                .screen()
                .contents()
        );
    }

    #[tokio::test]
    async fn the_help_lists_the_keys_and_any_key_closes_it() {
        let mut v = named_grid(&["s::a", "s::b"]);
        type_in(&mut v, b"?");
        let text = shows(24, 100, &v.paint(24, 100)).screen().contents();
        for line in ["find a session", "gg, G", "n, N", "Ctrl-U, Ctrl-W"] {
            assert!(text.contains(line), "{line}: {text}");
        }
        type_in(&mut v, b"q");
        assert!(!v.help, "q closed the help, and did not quit");
        assert!(
            !shows(24, 100, &v.paint(24, 100))
                .screen()
                .contents()
                .contains("find a session")
        );
    }

    /// Whatever is typed in the grid and in the finder, all a session's
    /// host ever receives from the viewer is a request to fit its tile.
    #[tokio::test]
    async fn nothing_typed_in_the_grid_or_the_finder_reaches_a_session() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let listener =
            tokio::net::UnixListener::bind(dir.path().join(super::super::watch::SOCKET)).unwrap();
        let host = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Greet, say the size, and show a prompt.
            let mut hello = vec![1, 1, 0, 24, 0, 80, 2];
            hello.extend_from_slice(&5u16.to_be_bytes());
            hello.extend_from_slice(b"ready");
            stream.write_all(&hello).await.unwrap();
            let mut received = Vec::new();
            stream.read_to_end(&mut received).await.unwrap();
            received
        });
        let mut session = viewer_session("north");
        session.tty = dir.path().join("tty");
        let mut v = Viewer::new(Mode::Grid, None);
        v.list(vec![session, viewer_session("south")]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !shown(&v.tiles[0].shown)
                .mirror
                .as_ref()
                .is_some_and(|m| m.screen().contents().contains("ready"))
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let settle = || tokio::time::sleep(Duration::from_millis(50));
        v.paint(21, 80);
        settle().await;
        for typed in [
            &b"rm -rf ~\r"[..],
            b"echo hi\x15\x17\x7f",
            b"/rm -rf ~\x1b[200~paste\r\x1b[201~\x15north\t",
            b"\x1b[97u\x1b[13;5u\x1b[Z\x1bOP",
            "审阅 é \x00\x04\x1a".as_bytes(),
            b"3jggG?x/no\x03jkhl[]HLnNo",
        ] {
            let mut pending = typed.to_vec();
            let _ = v.input(&mut pending, 21, 80);
            v.paint(21, 80);
            settle().await;
        }
        v.paint(30, 100);
        settle().await;
        drop(v);
        let received = tokio::time::timeout(Duration::from_secs(5), host)
            .await
            .unwrap()
            .unwrap();
        assert!(!received.is_empty(), "the tile asked to be fitted");
        assert_eq!(received.len() % 5, 0, "{received:?}");
        for frame in received.chunks(5) {
            assert_eq!(frame[0], 1, "only fit requests: {received:?}");
        }
    }
}
