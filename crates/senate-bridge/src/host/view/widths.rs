//! How wide the viewer's own terminal draws the glyphs terminals disagree on.
//!
//! A tile paints its session's screen cell by cell and must not run past
//! its right edge into the next tile. Most glyphs are as wide everywhere,
//! but East Asian "ambiguous" ones (`─`, `●`, `·`, `…`) are one column in
//! most terminals and two in those set to draw them wide, and a few
//! symbols (`⏺`, `›`) come out two columns wide in some terminals.
//! Assuming the widest for all of them never overruns a tile, but halves
//! a row made of them: a TUI's full-width `─` rule shows at half width.
//!
//! So the view asks its terminal once, when it opens: it prints each
//! doubtful glyph at the top-left corner and asks where the cursor went
//! (`CSI 6 n`). A terminal that does not answer in time is assumed to draw
//! them all at their widest, as before.

use std::time::Duration;

use unicode_width::UnicodeWidthStr as _;

use super::super::terminal::RawTerminal;

/// `─` stands for every East Asian ambiguous glyph.
const PROBED: [&str; 3] = ["─", "⏺", "›"];
/// How long a terminal has to say where its cursor went.
const PATIENCE: Duration = Duration::from_millis(300);

/// Columns the terminal moves its cursor for each of [`PROBED`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Widths([usize; 3]);

impl Widths {
    /// Not measured: the widest any terminal draws them.
    pub(super) const WIDEST: Self = Self([2, 2, 2]);

    #[cfg(test)]
    pub(super) fn measured(columns: [usize; 3]) -> Self {
        Self(columns)
    }

    /// The columns `text` (one cell's contents) takes on this terminal.
    pub(super) fn of(self, text: &str) -> usize {
        let [ambiguous, record, guillemet] = self.0;
        match text {
            "⏺" => record,
            "›" => guillemet,
            _ if ambiguous > 1 => text.width().max(text.width_cjk()),
            _ => text.width(),
        }
    }

    /// Ask `terminal`, which shows the view's own alternate screen, how wide
    /// it draws [`PROBED`]. Also returns what the person typed meanwhile.
    pub(super) async fn measure(terminal: &RawTerminal) -> (Self, Vec<u8>) {
        let mut ask = Vec::new();
        for glyph in PROBED {
            ask.extend_from_slice(b"\x1b[H");
            ask.extend_from_slice(glyph.as_bytes());
            ask.extend_from_slice(b"\x1b[6n");
        }
        ask.extend_from_slice(b"\x1b[H\x1b[2K");
        let mut answers = Vec::new();
        if terminal.draw(&ask).await.is_ok() {
            let read = async {
                let mut chunk = [0u8; 256];
                while let Ok(n @ 1..) = terminal.read(&mut chunk).await {
                    answers.extend_from_slice(&chunk[..n]);
                    if read_widths(&answers).0.is_some() {
                        break;
                    }
                }
            };
            let _ = tokio::time::timeout(PATIENCE, read).await;
        }
        let (widths, typed) = read_widths(&answers);
        (widths.unwrap_or(Self::WIDEST), typed)
    }
}

/// The widths in a terminal's answers to [`Widths::measure`], once all have
/// come, and every other byte, which the person typed.
fn read_widths(bytes: &[u8]) -> (Option<Widths>, Vec<u8>) {
    let mut columns = Vec::new();
    let mut typed = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        match cursor_report(&bytes[at..]) {
            Some((len, column)) if columns.len() < PROBED.len() => {
                columns.push(column);
                at += len;
            }
            _ => {
                typed.push(bytes[at]);
                at += 1;
            }
        }
    }
    let widths = <[u16; 3]>::try_from(columns).ok().map(|columns| {
        // Printed from the first column, a glyph leaves the cursor one past
        // its width. Anything but one or two columns is not believed.
        Widths(columns.map(|column| match column {
            2 => 1,
            _ => 2,
        }))
    });
    (widths, typed)
}

/// `CSI row ; column R` at the start of `bytes`: its length and column.
fn cursor_report(bytes: &[u8]) -> Option<(usize, u16)> {
    let rest = bytes.strip_prefix(b"\x1b[")?;
    let end = rest
        .iter()
        .position(|&b| !(b.is_ascii_digit() || b == b';'))?;
    if rest[end] != b'R' {
        return None;
    }
    let params = std::str::from_utf8(&rest[..end]).ok()?;
    let (row, column) = params.split_once(';')?;
    row.parse::<u16>().ok()?;
    Some((2 + end + 1, column.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_that_draws_box_lines_narrow_is_believed() {
        let (widths, typed) = read_widths(b"\x1b[1;2R\x1b[1;2R\x1b[1;2R");
        assert_eq!(widths, Some(Widths([1, 1, 1])));
        assert!(typed.is_empty());
        let widths = widths.unwrap();
        for glyph in ["─", "●", "·", "…", "⏺", "›", "a"] {
            assert_eq!(widths.of(glyph), 1, "{glyph}");
        }
        assert_eq!(widths.of("你"), 2);
    }

    #[test]
    fn each_glyph_takes_what_its_terminal_said_and_keys_typed_meanwhile_are_kept() {
        let (widths, typed) = read_widths(b"q\x1b[1;3R\x1b[1;3Rj\x1b[1;2R");
        let widths = widths.unwrap();
        assert_eq!(widths, Widths([2, 2, 1]));
        assert_eq!(typed, b"qj");
        assert_eq!((widths.of("─"), widths.of("●")), (2, 2));
        assert_eq!((widths.of("⏺"), widths.of("›")), (2, 1));
        assert_eq!(widths.of("a"), 1);
    }

    #[test]
    fn without_every_answer_nothing_is_measured() {
        let (widths, typed) = read_widths(b"\x1b[1;2R\x1b[1;2");
        assert_eq!(widths, None);
        assert_eq!(typed, b"\x1b[1;2");
        // An odd column is taken as the widest, never narrower.
        let (widths, _) = read_widths(b"\x1b[1;2R\x1b[1;5R\x1b[1;1R");
        assert_eq!(widths, Some(Widths([1, 2, 2])));
        for glyph in ["─", "●", "⏺", "›"] {
            assert_eq!(Widths::WIDEST.of(glyph), 2, "{glyph}");
        }
    }
}
