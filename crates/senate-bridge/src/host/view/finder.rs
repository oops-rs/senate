//! Finding a session in the grid: `/` or Ctrl-P opens a list of every
//! session over the grid, narrowed as the person types.
//!
//! | key | does |
//! | --- | --- |
//! | text | narrows the list ([`fuzzy`]) |
//! | Ctrl-N, Ctrl-J, ↓ / Ctrl-P, Ctrl-K, ↑ | the next, the previous line |
//! | Enter | attach to the chosen session |
//! | Tab | focus its tile, on its page, and close the list |
//! | Ctrl-U | clear what was typed |
//! | Ctrl-W | delete the last word |
//! | Backspace | delete the last character |
//! | Esc, Ctrl-C | close the list |
//!
//! What is typed here is the finder's: it goes into the query, never to a
//! session (the viewer has no way to send anything to one).

use super::super::scroll::{self, Mouse};
use super::super::terminal::split_input;
use super::{Frame, Rect, erase, fit, paint_width};

// ----- matching --------------------------------------------------------------

/// How well a query matched a line, and which characters it matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Match {
    pub score: i64,
    /// Indices of the matched characters (chars, not bytes), ascending.
    pub positions: Vec<usize>,
}

// Scores as fzf's: a match, gaps, and bonuses for where a match falls.
const MATCHED: i64 = 16;
const GAP_START: i64 = -3;
const GAP_EXTENSION: i64 = -1;
/// After a space, or at the start of the line.
const AFTER_SPACE: i64 = 10;
/// After a delimiter: `:` `/` `-` `_` `·` `,` `.`.
const AFTER_DELIMITER: i64 = 9;
/// After any other non-alphanumeric, or at a lower-to-upper change.
const WORD_START: i64 = 8;
/// The least a match right after the previous one is given.
const ADJACENT: i64 = 4;
/// A term's first character counts its bonus this many times.
const FIRST_CHAR: i64 = 2;

/// fzf-style matching: every space-separated term of `query` appears in
/// `text` in order, not necessarily side by side. A term is matched
/// regardless of case unless it has an upper-case letter. The score
/// rewards matches at word starts and runs of adjacent matches, so `rv`
/// ranks `refund::reviewer` above `server`; `None` when a term does not
/// match. An empty query matches everything with score 0.
pub(super) fn fuzzy(query: &str, text: &str) -> Option<Match> {
    let text: Vec<char> = text.chars().collect();
    let mut score = 0;
    let mut positions = Vec::new();
    for term in query.split_whitespace() {
        let (term_score, term_positions) = best(term, &text)?;
        score += term_score;
        positions.extend(term_positions);
    }
    positions.sort_unstable();
    positions.dedup();
    Some(Match { score, positions })
}

/// The lines of `texts` that `query` matches, best first; equal scores
/// keep their order.
pub(super) fn rank<'a>(
    query: &str,
    texts: impl IntoIterator<Item = &'a str>,
) -> Vec<(usize, Match)> {
    let mut found: Vec<(usize, Match)> = texts
        .into_iter()
        .enumerate()
        .filter_map(|(at, text)| fuzzy(query, text).map(|m| (at, m)))
        .collect();
    found.sort_by_key(|(_, m)| std::cmp::Reverse(m.score));
    found
}

fn fold(c: char, exact: bool) -> char {
    if exact {
        c
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// The bonus for a match at `text[at]`, by what comes before it.
fn bonus(text: &[char], at: usize) -> i64 {
    let Some(&before) = at.checked_sub(1).and_then(|b| text.get(b)) else {
        return AFTER_SPACE;
    };
    let here = text[at];
    if before.is_whitespace() {
        AFTER_SPACE
    } else if matches!(before, ':' | '/' | '-' | '_' | '·' | ',' | '.') {
        AFTER_DELIMITER
    } else if !before.is_alphanumeric() || (before.is_lowercase() && here.is_uppercase()) {
        WORD_START
    } else {
        0
    }
}

/// The best placement of one term's characters in `text`, by the score.
fn best(term: &str, text: &[char]) -> Option<(i64, Vec<usize>)> {
    let exact = term.chars().any(char::is_uppercase);
    let term: Vec<char> = term.chars().map(|c| fold(c, exact)).collect();
    let folded: Vec<char> = text.iter().map(|&c| fold(c, exact)).collect();
    let (m, n) = (term.len(), text.len());
    if m == 0 || m > n {
        return (m == 0).then(|| (0, Vec::new()));
    }
    // cell[i][j]: the best (score, bonus of its run, where term[i - 1] is)
    // with term[i] at text[j].
    let mut cell = vec![vec![None::<(i64, i64, usize)>; n]; m];
    for (i, want) in term.iter().enumerate() {
        for j in i..n {
            if folded[j] != *want {
                continue;
            }
            let own = bonus(text, j);
            if i == 0 {
                cell[0][j] = Some((MATCHED + own * FIRST_CHAR, own, 0));
                continue;
            }
            let mut top: Option<(i64, i64, usize)> = None;
            for (k, before) in cell[i - 1].iter().enumerate().take(j).skip(i - 1) {
                let Some((before, run, _)) = *before else {
                    continue;
                };
                let (score, run) = if k + 1 == j {
                    // A run carries the bonus of where it started.
                    let b = own.max(run).max(ADJACENT);
                    (before + MATCHED + b, b)
                } else {
                    let gap = i64::try_from(j - k - 2).unwrap_or(i64::MAX / 4);
                    (
                        before + GAP_START + gap * GAP_EXTENSION + MATCHED + own,
                        own,
                    )
                };
                if top.is_none_or(|(s, _, _)| score > s) {
                    top = Some((score, run, k));
                }
            }
            cell[i][j] = top;
        }
    }
    // The best end; of equals, the leftmost.
    let (total, mut j) = (0..n)
        .filter_map(|j| cell[m - 1][j].map(|(s, _, _)| (s, j)))
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))?;
    let mut positions = vec![0; m];
    for i in (0..m).rev() {
        positions[i] = j;
        if i > 0
            && let Some((_, _, k)) = cell[i][j]
        {
            j = k;
        }
    }
    Some((total, positions))
}

// ----- the finder's keys ------------------------------------------------------

/// What one key does in the finder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Edit {
    Insert(char),
    Backspace,
    /// Ctrl-U.
    Clear,
    /// Ctrl-W.
    DeleteWord,
    Up,
    Down,
    /// Enter: attach.
    Accept,
    /// Tab: focus the tile.
    Show,
    /// Esc, Ctrl-C.
    Close,
    /// Ctrl-]: leave the viewer.
    Detach,
}

impl Edit {
    /// Whether the finder is done after this key: what follows it is the
    /// grid's.
    fn ends(self) -> bool {
        matches!(self, Self::Accept | Self::Show | Self::Close | Self::Detach)
    }
}

/// The finder: what was typed, and which line of the list is chosen.
#[derive(Debug, Default)]
pub(super) struct Finder {
    pub query: String,
    pub selected: usize,
    /// Inside a bracketed paste: Enter and the like are text, not keys.
    pasting: bool,
}

impl Finder {
    /// Opened with the line `selected` chosen.
    pub fn new(selected: usize) -> Self {
        Self {
            selected,
            ..Self::default()
        }
    }

    /// Apply an edit of the query; moving and leaving are the viewer's.
    pub fn edit(&mut self, edit: Edit) {
        match edit {
            Edit::Insert(c) => self.query.push(c),
            Edit::Backspace => {
                self.query.pop();
            }
            Edit::Clear => self.query.clear(),
            Edit::DeleteWord => {
                let kept = self.query.trim_end().len();
                self.query.truncate(kept);
                let word = self.query.rfind(char::is_whitespace).map_or(0, |at| at + 1);
                self.query.truncate(word);
            }
            Edit::Up | Edit::Down | Edit::Accept | Edit::Show | Edit::Close | Edit::Detach => {
                return;
            }
        }
        self.selected = 0;
    }

    /// The keys in `pending`, up to and including one that ends the finder
    /// ([`Edit::ends`]), which leaves the rest in `pending` for the grid.
    /// The start of a key still arriving is kept; anything else that is no
    /// key of the finder is dropped.
    pub fn keys(&mut self, pending: &mut Vec<u8>) -> Vec<Edit> {
        let (before, detach) = split_input(pending);
        // A lone Esc read on its own is the key, not the start of one.
        if !detach && pending.as_slice() == [0x1b] {
            pending.clear();
            return vec![Edit::Close];
        }
        let mut out = Vec::new();
        let mut at = 0;
        while at < before {
            let rest = &pending[at..before];
            match scroll::mouse(rest) {
                Mouse::Report { len, .. } => {
                    at += len;
                    continue;
                }
                Mouse::Incomplete if !detach => break,
                _ => {}
            }
            let (edit, len) = match rest[0] {
                0x1b => match escape(rest) {
                    Escape::Incomplete if detach => (None, rest.len()),
                    Escape::Incomplete => break,
                    Escape::Paste(on) => {
                        self.pasting = on;
                        (None, escape_len(rest))
                    }
                    Escape::Key(edit, len) => (edit.filter(|_| !self.pasting), len),
                },
                b if self.pasting && b < 0x20 => (None, 1),
                b'\r' => (Some(Edit::Accept), 1),
                b'\t' => (Some(Edit::Show), 1),
                0x0e | b'\n' => (Some(Edit::Down), 1),
                0x10 | 0x0b => (Some(Edit::Up), 1),
                0x15 => (Some(Edit::Clear), 1),
                0x17 => (Some(Edit::DeleteWord), 1),
                0x7f | 0x08 => (Some(Edit::Backspace), 1),
                0x03 | 0x07 => (Some(Edit::Close), 1),
                b if b < 0x20 => (None, 1),
                b => match utf8_len(b) {
                    None => (None, 1),
                    Some(len) if rest.len() < len => {
                        if detach {
                            (None, rest.len())
                        } else {
                            break;
                        }
                    }
                    Some(len) => (
                        std::str::from_utf8(&rest[..len])
                            .ok()
                            .and_then(|s| s.chars().next())
                            .filter(|c| !c.is_control())
                            .map(Edit::Insert),
                        len,
                    ),
                },
            };
            at += len;
            if let Some(edit) = edit {
                out.push(edit);
                if edit.ends() {
                    break;
                }
            }
        }
        if detach && !out.last().is_some_and(|e| e.ends()) {
            out.push(Edit::Detach);
            pending.clear();
        } else {
            pending.drain(..at);
        }
        out
    }
}

fn utf8_len(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

enum Escape {
    Incomplete,
    /// Bracketed paste starts (`true`) or ends.
    Paste(bool),
    Key(Option<Edit>, usize),
}

/// The length of the escape sequence `rest` starts with, once whole.
fn escape_len(rest: &[u8]) -> usize {
    match rest.get(1) {
        Some(b'[') => rest[2..]
            .iter()
            .position(|b| (0x40..=0x7e).contains(b))
            .map_or(rest.len(), |end| end + 3),
        Some(b'O') => 3.min(rest.len()),
        _ => 2.min(rest.len()),
    }
}

/// An escape sequence at the start of `rest`: arrows, a paste's brackets,
/// a key as the kitty keyboard protocol reports it (`CSI code;mods u`), or
/// anything else, which is skipped whole.
fn escape(rest: &[u8]) -> Escape {
    match rest {
        [0x1b] | [0x1b, b'O'] => Escape::Incomplete,
        [0x1b, b'O', key, ..] => Escape::Key(
            match key {
                b'A' => Some(Edit::Up),
                b'B' => Some(Edit::Down),
                _ => None,
            },
            3,
        ),
        [0x1b, b'[', tail @ ..] => {
            let Some(end) = tail.iter().position(|b| (0x40..=0x7e).contains(b)) else {
                return Escape::Incomplete;
            };
            let params = &tail[..end];
            let len = end + 3;
            match (tail[end], params) {
                (b'A', _) => Escape::Key(Some(Edit::Up), len),
                (b'B', _) => Escape::Key(Some(Edit::Down), len),
                (b'~', b"200") => Escape::Paste(true),
                (b'~', b"201") => Escape::Paste(false),
                (b'u', params) => Escape::Key(kitty(params), len),
                _ => Escape::Key(None, len),
            }
        }
        // Alt and a key: not the finder's.
        _ => Escape::Key(None, 2.min(rest.len())),
    }
}

/// A kitty keyboard report's parameters, `code[:shifted];mods[:event]`.
fn kitty(params: &[u8]) -> Option<Edit> {
    let params = std::str::from_utf8(params).ok()?;
    let mut fields = params.split(';');
    let mut codes = fields.next()?.split(':');
    let code: u32 = codes.next()?.parse().ok()?;
    let shifted: Option<u32> = codes.next().and_then(|c| c.parse().ok());
    let mut mods = fields.next().unwrap_or("1").split(':');
    let mods = mods.next()?.parse::<u32>().ok()?.saturating_sub(1);
    // A release is not a key press.
    if params.split(';').nth(1).and_then(|m| m.split(':').nth(1)) == Some("3") {
        return None;
    }
    let (shift, alt, ctrl) = (mods & 1 != 0, mods & 2 != 0, mods & 4 != 0);
    if ctrl {
        return match char::from_u32(code)? {
            'n' | 'j' => Some(Edit::Down),
            'p' | 'k' => Some(Edit::Up),
            'u' => Some(Edit::Clear),
            'w' => Some(Edit::DeleteWord),
            'c' | 'g' => Some(Edit::Close),
            _ => None,
        };
    }
    match code {
        13 => Some(Edit::Accept),
        9 => Some(Edit::Show),
        27 => Some(Edit::Close),
        127 | 8 => Some(Edit::Backspace),
        _ if alt => None,
        _ => {
            let c = char::from_u32(match (shift, shifted) {
                (true, Some(shifted)) => shifted,
                _ => code,
            })?;
            let c = if shift && shifted.is_none() {
                c.to_uppercase().next().unwrap_or(c)
            } else {
                c
            };
            (!c.is_control()).then_some(Edit::Insert(c))
        }
    }
}

// ----- the overlay -------------------------------------------------------------

/// A box of `width` × `height` in the middle of the terminal above its
/// status line; on a narrow terminal, its whole width.
pub(super) fn centred(rows: u16, cols: u16, height: u16, most_width: u16) -> Rect {
    let usable = rows.saturating_sub(1);
    let width = if cols < 24 {
        cols
    } else {
        cols.saturating_sub(4).min(most_width)
    };
    let height = height.min(usable);
    Rect {
        row: (usable - height) / 2,
        col: (cols - width) / 2,
        rows: height,
        cols: width,
    }
}

const PLAIN: &str = "\x1b[0m";
const PLAIN_MATCH: &str = "\x1b[0;1;4m";
const CHOSEN: &str = "\x1b[0;7m";
const CHOSEN_MATCH: &str = "\x1b[0;1;4;7m";
const HEADER: &str = "\x1b[0;1;7m";

/// `text` cut or padded to `width` columns, the characters at `positions`
/// in `marked`, the rest in `plain`.
fn fit_marked(text: &str, positions: &[usize], width: usize, plain: &str, marked: &str) -> String {
    let mut out = String::from(plain);
    let mut used = 0;
    let mut on = false;
    for (at, ch) in text.chars().enumerate() {
        if ch.is_control() {
            continue;
        }
        let w = paint_width(ch.encode_utf8(&mut [0; 4]));
        if used + w > width || (w == 0 && used == 0) {
            break;
        }
        let hit = positions.binary_search(&at).is_ok();
        if hit != on {
            out.push_str(if hit { marked } else { plain });
            on = hit;
        }
        out.push(ch);
        used += w;
    }
    if on {
        out.push_str(plain);
    }
    out.extend(std::iter::repeat_n(' ', width - used));
    out
}

/// The rows the finder takes for `lines` lines: its header, the query,
/// then the list.
pub(super) fn height(lines: usize) -> u16 {
    u16::try_from(lines.max(1))
        .unwrap_or(u16::MAX)
        .saturating_add(2)
}

/// Paint the finder over `area`: a header with how many lines match, the
/// query, then the matching lines with the chosen one shown inverted and
/// the matched characters bold and underlined. Returns where the cursor
/// stands, at the end of the query.
pub(super) fn paint(
    frame: &mut Frame,
    area: Rect,
    finder: &Finder,
    found: &[(usize, Match)],
    titles: &[&str],
) -> Option<(u16, u16)> {
    let width = usize::from(area.cols);
    let mut lines: Vec<String> = Vec::new();
    let count = format!("{}/{} ", found.len(), titles.len());
    let header = " Find a session";
    let room = width.saturating_sub(paint_width_str(&count));
    lines.push(format!(
        "{HEADER}{}{}",
        fit(header, room),
        fit(&count, width - room)
    ));
    // The query, its end kept in view.
    let prompt = "> ";
    let room = width.saturating_sub(prompt.len() + 1);
    let mut shown: Vec<char> = finder.query.chars().collect();
    while paint_width_str(&shown.iter().collect::<String>()) > room && !shown.is_empty() {
        shown.remove(0);
    }
    let shown: String = shown.into_iter().collect();
    let query_cols = prompt.len() + paint_width_str(&shown);
    lines.push(format!(
        "{PLAIN}{}",
        fit(&format!("{prompt}{shown}"), width)
    ));
    let list = usize::from(area.rows.saturating_sub(2));
    let selected = finder.selected.min(found.len().saturating_sub(1));
    let first = (selected + 1).saturating_sub(list);
    for line in 0..list {
        let text = match found.get(first + line) {
            Some((tile, matched)) => {
                let chosen = first + line == selected;
                let (plain, marked) = if chosen {
                    (CHOSEN, CHOSEN_MATCH)
                } else {
                    (PLAIN, PLAIN_MATCH)
                };
                let mark = if chosen { "▸ " } else { "  " };
                // The marker takes two columns: the matched positions
                // shift with it.
                let positions: Vec<usize> = matched.positions.iter().map(|p| p + 2).collect();
                fit_marked(
                    &format!("{mark}{}", titles[*tile]),
                    &positions,
                    width,
                    plain,
                    marked,
                )
            }
            None if line == 0 && found.is_empty() => {
                format!("\x1b[0;2m{}", fit("  no session matches", width))
            }
            None => format!("{PLAIN}{}", fit("", width)),
        };
        lines.push(text);
    }
    for (row, text) in (area.row..area.row + area.rows).zip(lines) {
        let out = frame.region(row, area.col);
        out.push_str(PLAIN);
        erase(out, area.cols);
        out.push_str(&text);
        out.push_str(PLAIN);
    }
    (area.rows >= 2 && query_cols < width).then(|| {
        (
            area.row + 1,
            area.col + u16::try_from(query_cols).unwrap_or(area.cols),
        )
    })
}

fn paint_width_str(text: &str) -> usize {
    text.chars()
        .map(|c| paint_width(c.encode_utf8(&mut [0; 4])))
        .sum()
}

/// What `?` shows.
pub(super) const HELP: &[(&str, &str)] = &[
    ("/, Ctrl-P", "find a session"),
    ("h j k l, arrows", "move; a count repeats it: 3j"),
    ("gg, G", "the first, the last; 5G: the fifth"),
    ("H L, [ ], PgUp PgDn", "the page before, after"),
    ("n, N", "next, previous match of the last find"),
    ("Enter, o", "attach; detaching comes back here"),
    ("?", "this help"),
    ("q, Ctrl-]", "quit"),
    ("", ""),
    ("in the finder", ""),
    ("Ctrl-N/P, Ctrl-J/K, arrows", "move"),
    ("Enter", "attach"),
    ("Tab", "show its tile"),
    ("Ctrl-U, Ctrl-W", "clear, delete a word"),
    ("Esc, Ctrl-C", "close"),
];

/// Paint the help over `area`.
pub(super) fn paint_help(frame: &mut Frame, area: Rect) {
    let width = usize::from(area.cols);
    let keys = HELP
        .iter()
        .map(|(key, _)| paint_width_str(key))
        .max()
        .unwrap_or(0)
        .min(width / 2);
    let mut lines = vec![format!("{HEADER}{}", fit(" Keys · any key closes", width))];
    for (key, does) in HELP {
        let key = fit(key, keys);
        lines.push(format!("{PLAIN}{}", fit(&format!(" {key}  {does}"), width)));
    }
    for (line, text) in lines.into_iter().take(usize::from(area.rows)).enumerate() {
        let out = frame.region(area.row + u16::try_from(line).unwrap_or(0), area.col);
        out.push_str(PLAIN);
        erase(out, area.cols);
        out.push_str(&text);
        out.push_str(PLAIN);
    }
}

/// The rows the help takes.
pub(super) fn help_height() -> u16 {
    u16::try_from(HELP.len() + 1).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(query: &str, texts: &[&str]) -> Vec<String> {
        rank(query, texts.iter().copied())
            .into_iter()
            .map(|(at, _)| texts[at].to_owned())
            .collect()
    }

    #[test]
    fn a_query_matches_in_order_and_ranks_word_starts_and_runs_first() {
        assert_eq!(fuzzy("", "anything").unwrap().score, 0);
        assert!(fuzzy("zx", "refund::reviewer").is_none());
        assert!(fuzzy("rr", "refund::reviewer").is_some(), "in order, apart");
        assert!(fuzzy("ver", "server").is_some());
        assert!(fuzzy("rev", "server").is_none(), "not out of order");
        assert_eq!(
            fuzzy("rev", "refund::reviewer").unwrap().positions,
            [8, 9, 10],
            "the word start, not r…e…v scattered"
        );
        // Word starts and runs win.
        assert_eq!(
            order("rv", &["server · claude", "refund::reviewer · codex"]),
            ["refund::reviewer · codex", "server · claude"]
        );
        assert_eq!(
            order("led", &["payments::filed", "payments::ledger"]),
            ["payments::ledger", "payments::filed"]
        );
        // Every term must match, anywhere.
        assert_eq!(
            order(
                "codex pay",
                &[
                    "payments::ledger · claude",
                    "payments::ledger · codex",
                    "senate::main · codex"
                ]
            ),
            ["payments::ledger · codex"]
        );
        // Case: ignored, unless the term has an upper-case letter.
        assert!(fuzzy("main", "senate::Main").is_some());
        assert!(fuzzy("Main", "senate::main").is_none());
        // Equal scores keep the grid's order.
        assert_eq!(order("", &["b", "a", "c"]), ["b", "a", "c"]);
    }

    #[test]
    fn wide_characters_match_and_paint_by_their_width() {
        let m = fuzzy("审阅", "评审::审阅者 · claude").unwrap();
        assert_eq!(m.positions, [4, 5]);
        let painted = fit_marked("审阅者 x", &[0], 5, PLAIN, PLAIN_MATCH);
        let shown = vt100_rows(&painted, 1, 10);
        assert_eq!(shown[0], "审阅 ", "a wide character cut in half is a blank");
    }

    fn vt100_rows(bytes: &str, rows: u16, cols: u16) -> Vec<String> {
        let size = |n| std::num::NonZeroU16::new(n).unwrap();
        let mut parser = vt100::Parser::new(size(rows), size(cols), 0);
        parser.process(bytes.as_bytes());
        parser.screen().rows(0, cols).collect()
    }

    fn typed(finder: &mut Finder, bytes: &[u8]) -> Vec<Edit> {
        let mut pending = bytes.to_vec();
        let edits = finder.keys(&mut pending);
        assert!(pending.is_empty(), "left {pending:?}");
        for edit in &edits {
            finder.edit(*edit);
        }
        edits
    }

    #[test]
    fn the_finders_keys_edit_the_query_move_and_close() {
        let mut f = Finder::default();
        typed(&mut f, "ab 审阅".as_bytes());
        assert_eq!(f.query, "ab 审阅");
        typed(&mut f, b"\x17");
        assert_eq!(f.query, "ab ", "Ctrl-W: the last word");
        typed(&mut f, b"\x17");
        assert_eq!(f.query, "");
        typed(&mut f, b"xyz\x7f");
        assert_eq!(f.query, "xy");
        typed(&mut f, b"\x15");
        assert_eq!(f.query, "", "Ctrl-U clears");
        assert_eq!(
            typed(&mut f, b"\x0e\x0a\x1b[B\x1bOB\x10\x0b\x1b[A"),
            [
                Edit::Down,
                Edit::Down,
                Edit::Down,
                Edit::Down,
                Edit::Up,
                Edit::Up,
                Edit::Up
            ]
        );
        // Each of these ends the finder; what follows is the grid's.
        for (bytes, edit) in [
            (&b"\rjj"[..], Edit::Accept),
            (b"\tjj", Edit::Show),
            (b"\x03jj", Edit::Close),
        ] {
            let mut pending = bytes.to_vec();
            assert_eq!(f.keys(&mut pending), [edit]);
            assert_eq!(pending, b"jj");
        }
        // A lone Esc closes; an Esc that starts an arrow waits for it.
        assert_eq!(f.keys(&mut b"\x1b".to_vec()), [Edit::Close]);
        let mut pending = b"a\x1b[".to_vec();
        assert_eq!(f.keys(&mut pending), [Edit::Insert('a')]);
        assert_eq!(pending, b"\x1b[");
        // Ctrl-] leaves at once, whatever came before it.
        assert_eq!(
            f.keys(&mut b"ab\x1d".to_vec()),
            [Edit::Insert('a'), Edit::Insert('b'), Edit::Detach]
        );
        // A character split across reads is kept until it is whole.
        let mut pending = "审".as_bytes()[..2].to_vec();
        assert!(f.keys(&mut pending).is_empty());
        pending.push("审".as_bytes()[2]);
        assert_eq!(f.keys(&mut pending), [Edit::Insert('审')]);
    }

    #[test]
    fn a_paste_is_text_and_kitty_reports_are_keys() {
        let mut f = Finder::default();
        let edits = typed(&mut f, b"\x1b[200~led\rger\x1b[201~");
        assert!(!edits.contains(&Edit::Accept), "Enter in a paste is no key");
        assert_eq!(f.query, "ledger");
        let mut f = Finder::default();
        assert_eq!(
            typed(
                &mut f,
                b"\x1b[110;5u\x1b[112;5u\x1b[97u\x1b[97;2u\x1b[97;1:3u"
            ),
            [Edit::Down, Edit::Up, Edit::Insert('a'), Edit::Insert('A')]
        );
        assert_eq!(f.keys(&mut b"\x1b[13u".to_vec()), [Edit::Accept]);
        assert_eq!(f.keys(&mut b"\x1b[27u".to_vec()), [Edit::Close]);
        // Other sequences are skipped whole.
        assert!(f.keys(&mut b"\x1b[1;5C\x1b[3~".to_vec()).is_empty());
    }
}
