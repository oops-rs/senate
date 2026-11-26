//! The terminal emulator a host keeps a session's screen in, and each
//! watcher's copy of it ([`super::watch`]).
//!
//! It is [`vt100`], plus two control functions vt100 does not implement
//! that the hosted TUIs draw with. Without them the emulator's screen
//! drifts from the one the TUI believes it drew. The TUI then redraws only
//! the rows it believes changed, so the drift stays on screen: rows
//! repeated next to each other until the TUI next repaints everything.
//!
//! - DECAWM (`CSI ? 7 l`, `CSI ? 7 h`). agy turns autowrap off around every
//!   line it draws. A line that reaches past the right margin then
//!   overwrites the last column, as on a real terminal, instead of
//!   wrapping onto a row the TUI never drew.
//! - CBT and CHT (`CSI Z`, `CSI I`): the cursor goes back or forward to a
//!   tab stop, every 8 columns as vt100 keeps them. Ignored, text lands to
//!   the right of where the TUI put it.
//!
//! Only the bytes given to vt100 change: what the TUI wrote goes to an
//! attached terminal and to watchers as it was.

use std::num::NonZeroU16;

use unicode_width::UnicodeWidthChar as _;

const ESC: u8 = 0x1b;
/// The longest control sequence held whole while it arrives.
const MAX_SEQUENCE: usize = 64;
const TAB: u16 = 8;
const VS16: char = '\u{fe0f}';

/// Where the scanner is in the TUI's byte stream.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// After ESC.
    Escape,
    /// After ESC and an intermediate byte.
    EscapeIntermediate,
    /// Inside `CSI`, held in [`Vt::sequence`] until its final byte.
    Csi,
    /// Inside a `CSI` too long to be one of ours: passed on as it comes.
    LongCsi,
    /// Inside OSC, DCS, SOS, PM or APC: passed on until its terminator.
    String,
}

pub(crate) struct Vt {
    parser: vt100::Parser,
    autowrap: bool,
    state: State,
    /// The CSI being read, from its ESC; or the start of a UTF-8 character.
    sequence: Vec<u8>,
    /// What goes to vt100 next.
    out: Vec<u8>,
    /// The cursor's column once `out` has gone to vt100, while text is
    /// written with autowrap off; `None` when it has to be asked.
    column: Option<u16>,
}

impl Vt {
    pub(crate) fn new(rows: NonZeroU16, cols: NonZeroU16, history: usize) -> Self {
        Self::from_screen(
            vt100::Parser::new(rows, cols, history).screen().clone(),
            true,
        )
    }

    /// A terminal showing `screen`, autowrapping or not.
    pub(crate) fn from_screen(screen: vt100::Screen, autowrap: bool) -> Self {
        let (rows, cols) = screen.size();
        let mut parser = vt100::Parser::new(rows, cols, 0);
        *parser.screen_mut() = screen;
        Self {
            parser,
            autowrap,
            state: State::Ground,
            sequence: Vec::new(),
            out: Vec::new(),
            column: None,
        }
    }

    pub(crate) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub(crate) fn screen_mut(&mut self) -> &mut vt100::Screen {
        self.parser.screen_mut()
    }

    /// Whether text that reaches the right margin wraps (DECAWM).
    pub(crate) fn autowrap(&self) -> bool {
        self.autowrap
    }

    /// What tells another terminal the modes kept here and not by vt100.
    pub(crate) fn modes_formatted(&self) -> &'static [u8] {
        if self.autowrap { b"" } else { b"\x1b[?7l" }
    }

    pub(crate) fn set_size(&mut self, rows: NonZeroU16, cols: NonZeroU16) {
        self.parser.screen_mut().set_size(rows, cols);
        self.column = None;
    }

    /// Take in what the TUI wrote. A control sequence or character split
    /// between two writes is held until it is whole.
    pub(crate) fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.byte(byte);
        }
        self.flush();
    }

    fn flush(&mut self) {
        if !self.out.is_empty() {
            self.parser.process(&self.out);
            self.out.clear();
        }
    }

    /// The cursor's column, with everything before it taken in.
    fn cursor_column(&mut self) -> u16 {
        if let Some(column) = self.column {
            return column;
        }
        self.flush();
        let column = self.parser.screen().cursor_position().1;
        self.column = Some(column);
        column
    }

    fn cols(&self) -> u16 {
        self.parser.screen().size().1.get()
    }

    fn byte(&mut self, byte: u8) {
        match self.state {
            State::Ground => self.ground(byte),
            State::Escape => {
                if byte == b'[' {
                    self.sequence.push(byte);
                    self.state = State::Csi;
                    return;
                }
                let escape = std::mem::take(&mut self.sequence);
                self.out.extend_from_slice(&escape);
                if byte == ESC {
                    self.sequence.push(ESC);
                    return;
                }
                self.out.push(byte);
                self.state = match byte {
                    b']' | b'P' | b'X' | b'^' | b'_' => State::String,
                    0x20..=0x2f => State::EscapeIntermediate,
                    _ => {
                        if byte == b'c' {
                            // RIS: the terminal as it starts.
                            self.autowrap = true;
                        }
                        State::Ground
                    }
                };
            }
            State::EscapeIntermediate => {
                if byte == ESC {
                    self.sequence.push(ESC);
                    self.state = State::Escape;
                    return;
                }
                self.out.push(byte);
                if !(0x20..=0x2f).contains(&byte) {
                    self.state = State::Ground;
                }
            }
            State::Csi => self.csi(byte),
            State::LongCsi => match byte {
                0x18 | 0x1a | ESC => {
                    self.state = State::Ground;
                    self.ground(byte);
                }
                _ => {
                    self.out.push(byte);
                    if (0x40..=0x7e).contains(&byte) {
                        self.state = State::Ground;
                    }
                }
            },
            State::String => match byte {
                // ESC starts its terminator, or what cuts it short.
                ESC => {
                    self.sequence.push(ESC);
                    self.state = State::Escape;
                }
                // BEL ends an OSC; CAN and SUB cancel any of them.
                0x07 | 0x18 | 0x1a => {
                    self.out.push(byte);
                    self.state = State::Ground;
                }
                _ => self.out.push(byte),
            },
        }
    }

    fn ground(&mut self, byte: u8) {
        if !self.sequence.is_empty() {
            // The rest of a UTF-8 character.
            if byte & 0xc0 == 0x80 {
                self.sequence.push(byte);
                if self.sequence.len() == utf8_len(self.sequence[0]) {
                    let bytes = std::mem::take(&mut self.sequence);
                    self.text(&bytes);
                }
                return;
            }
            // Not UTF-8 after all: vt100 makes of it what it does.
            let bytes = std::mem::take(&mut self.sequence);
            self.out.extend_from_slice(&bytes);
            self.column = None;
        }
        match byte {
            ESC => {
                self.sequence.push(ESC);
                self.column = None;
                self.state = State::Escape;
            }
            0x00..=0x1f | 0x7f => {
                self.out.push(byte);
                self.column = None;
            }
            0x20..=0x7e => self.text(&[byte]),
            0xc2..=0xf4 => self.sequence.push(byte),
            _ => {
                self.out.push(byte);
                self.column = None;
            }
        }
    }

    /// One whole character of text.
    fn text(&mut self, bytes: &[u8]) {
        if self.autowrap {
            self.out.extend_from_slice(bytes);
            return;
        }
        let Some(ch) = std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.chars().next())
        else {
            self.out.extend_from_slice(bytes);
            self.column = None;
            return;
        };
        let cols = self.cols();
        // As vt100 measures it: a C1 control it does not draw is left to it.
        let Some(width) = ch.width().or((u32::from(ch) >= 256).then_some(1)) else {
            self.out.extend_from_slice(bytes);
            self.column = None;
            return;
        };
        let width = u16::try_from(width).unwrap_or(2).min(2);
        let column = self.cursor_column();
        if width == 0 {
            // A presentation selector on the last column's character would
            // widen it, and vt100 would move it to the next row.
            if ch == VS16 && column >= cols {
                return;
            }
            self.out.extend_from_slice(bytes);
            self.column = None;
            return;
        }
        if column + width > cols {
            // Autowrap off: the character takes the last column instead.
            self.flush();
            let to = cols.saturating_sub(width) + 1;
            self.parser.process(format!("\x1b[{to}G").as_bytes());
            self.column = Some(cols);
        } else {
            self.column = Some(column + width);
        }
        self.out.extend_from_slice(bytes);
    }

    fn csi(&mut self, byte: u8) {
        self.sequence.push(byte);
        match byte {
            0x40..=0x7e => {
                let sequence = std::mem::take(&mut self.sequence);
                self.state = State::Ground;
                self.column = None;
                self.control(&sequence);
            }
            // CAN and SUB cancel it; ESC starts another.
            0x18 | 0x1a | ESC => {
                let mut sequence = std::mem::take(&mut self.sequence);
                sequence.pop();
                self.out.extend_from_slice(&sequence);
                self.state = State::Ground;
                self.column = None;
                self.ground(byte);
            }
            _ if self.sequence.len() > MAX_SEQUENCE => {
                // Nothing this long is one of ours: vt100 has it as it is.
                let sequence = std::mem::take(&mut self.sequence);
                self.out.extend_from_slice(&sequence);
                self.state = State::LongCsi;
                self.column = None;
            }
            _ => {}
        }
    }

    /// A whole `CSI` sequence, from its ESC to its final byte.
    fn control(&mut self, sequence: &[u8]) {
        let body = &sequence[2..sequence.len() - 1];
        let last = sequence[sequence.len() - 1];
        let private = body.first().filter(|b| (0x3c..=0x3f).contains(*b)).copied();
        let params = &body[usize::from(private.is_some())..];
        let intermediate = params.iter().position(|b| (0x20..=0x2f).contains(b));
        let (params, intermediates) = match intermediate {
            Some(at) => (&params[..at], &params[at..]),
            None => (params, &params[..0]),
        };
        let numbers = || {
            std::str::from_utf8(params)
                .unwrap_or_default()
                .split(';')
                .map(|n| n.parse::<u16>().ok())
        };
        match (private, intermediates, last) {
            (Some(b'?'), [], b'h' | b'l') if numbers().any(|n| n == Some(7)) => {
                self.autowrap = last == b'h';
            }
            // DECSTR: a soft reset turns autowrap back on.
            (None, [b'!'], b'p') => self.autowrap = true,
            (None, [], b'Z' | b'I') => {
                let cols = self.cols();
                let count = numbers().next().flatten().unwrap_or(1).clamp(1, cols);
                let mut column = self.cursor_column().min(cols - 1);
                for _ in 0..count {
                    column = if last == b'Z' {
                        column.saturating_sub(1) / TAB * TAB
                    } else {
                        ((column / TAB + 1) * TAB).min(cols - 1)
                    };
                }
                self.out
                    .extend_from_slice(format!("\x1b[{}G", column + 1).as_bytes());
                return;
            }
            _ => {}
        }
        self.out.extend_from_slice(sequence);
    }
}

/// The length of the UTF-8 character that starts with `lead`.
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(n: u16) -> NonZeroU16 {
        NonZeroU16::new(n).unwrap()
    }

    fn terminal(rows: u16, cols: u16) -> Vt {
        Vt::new(size(rows), size(cols), 0)
    }

    fn rows(vt: &Vt) -> Vec<String> {
        let cols = vt.screen().size().1.get();
        vt.screen().rows(0, cols).collect()
    }

    #[test]
    fn text_past_the_margin_stays_on_its_row_while_autowrap_is_off() {
        let mut vt = terminal(3, 10);
        vt.process(b"\x1b[?7labcdefghijkl\r\nnext");
        assert_eq!(rows(&vt), ["abcdefghil", "next", ""]);
        // Wide characters too, and the cursor stays on the row.
        vt.process(b"\x1b[1;1H\x1b[K\xe4\xbd\xa0\xe5\xa5\xbd\xe4\xbd\xa0\xe5\xa5\xbd\xe5\x95\x8a\xe5\x91\x80");
        assert_eq!(vt.screen().cursor_position().0, 0);
        assert_eq!(rows(&vt)[0], "你好你好呀");
        // And back on, it wraps again.
        vt.process(b"\x1b[?7h\x1b[2;1Habcdefghijkl");
        assert_eq!(rows(&vt), ["你好你好呀", "abcdefghij", "kl"]);
    }

    #[test]
    fn modes_and_characters_split_between_writes_are_still_seen() {
        let mut vt = terminal(2, 6);
        for part in [b"\x1b[?".as_slice(), b"7", b"l\xe4\xbd", b"\xa0abcdef"] {
            vt.process(part);
        }
        assert!(!vt.autowrap());
        assert_eq!(rows(&vt), ["你abcf", ""]);
        vt.process(b"\x1b[!p");
        assert!(vt.autowrap());
        vt.process(b"\x1b[?7;25l");
        assert!(!vt.autowrap());
        vt.process(b"\x1bc");
        assert!(vt.autowrap());
    }

    #[test]
    fn a_presentation_selector_on_the_last_column_does_not_wrap() {
        let mut vt = terminal(2, 4);
        vt.process("\x1b[?7labc✔\u{fe0f}".as_bytes());
        assert_eq!(vt.screen().cursor_position().0, 0);
        assert_eq!(rows(&vt), ["abc✔", ""]);
    }

    #[test]
    fn back_and_forward_tabs_move_to_tab_stops() {
        let mut vt = terminal(2, 30);
        vt.process(b"abcdefghijkl\x1b[Zx");
        assert_eq!(rows(&vt)[0], "abcdefghxjkl");
        vt.process(b"\r\x1b[2Iy\x1b[3Zz");
        assert_eq!(rows(&vt)[0], "zbcdefghxjkl    y");
        // Never past the last column, nor before the first.
        vt.process(b"\x1b[9Iw\x1b[9Zv");
        assert_eq!(vt.screen().cursor_position(), (0, 1));
        assert_eq!(rows(&vt)[0], "vbcdefghxjkl    y            w");
    }

    #[test]
    fn everything_else_reaches_vt100_unchanged() {
        let long_sgr = format!("\x1b[{}m", ["38;2;1;2;3"; 8].join(";"));
        assert!(long_sgr.len() > MAX_SEQUENCE);
        let stream = format!(
            "\x1b[31mred\x1b[m \x1b]0;title\x07\x1b]8;;file:///x\x1b\\link\x1b]8;;\x1b\\\
             \x1b[2;3H\x1b[1;2r\x1b[?1049hfull\x1b[?1049l\x1b[?2026h\x1b[5D\x1b[2K{long_sgr}end"
        );
        for autowrap in ["", "\x1b[?7l"] {
            let stream = format!("{autowrap}{stream}");
            let mut plain = vt100::Parser::new(size(4), size(20), 0);
            plain.process(stream.as_bytes());
            let mut vt = terminal(4, 20);
            for chunk in stream.as_bytes().chunks(3) {
                vt.process(chunk);
            }
            assert_eq!(
                vt.screen().contents_formatted(),
                plain.screen().contents_formatted()
            );
            assert_eq!(
                vt.screen().cursor_position(),
                plain.screen().cursor_position()
            );
        }
    }
}
