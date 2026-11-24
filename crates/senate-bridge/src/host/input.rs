//! Distinguish terminal protocol reports from input that can attend to a
//! prompt. The relay forwards both unchanged. Only the distinction leaves
//! this module, and a report can span multiple frames.

use std::collections::VecDeque;

// A report arrives as terminal traffic, usually in one write. Waiting
// briefly across frames avoids swallowing a person's standalone Escape
// or the first keys after a malformed terminal control string.
const REPORT_WAIT_NS: u64 = 200_000_000;
const MAX_REPORT_BYTES: usize = 4096;

#[derive(Default)]
pub(super) struct Input {
    pending: Option<Pending>,
    viewer_scheme: Option<super::emulation::Scheme>,
    in_paste: bool,
}

struct Pending {
    sequence: Sequence,
    first_at: u64,
    last_at: u64,
    length: usize,
}

impl Pending {
    fn escape(at: u64) -> Self {
        Self {
            sequence: Sequence::Escape,
            first_at: at,
            last_at: at,
            length: 1,
        }
    }
}

enum Sequence {
    Escape,
    Csi(Vec<u8>),
    ControlStart(u8),
    ControlString { escape: bool },
    X10Mouse(u8),
}

impl Input {
    pub(super) fn take_viewer_scheme(&mut self) -> Option<super::emulation::Scheme> {
        self.viewer_scheme.take()
    }

    pub(super) fn deadline(&self) -> Option<u64> {
        self.pending
            .as_ref()
            .map(|pending| pending.first_at.saturating_add(REPORT_WAIT_NS))
    }

    pub(super) fn expire(&mut self, now: u64) -> Option<u64> {
        if self.deadline().is_some_and(|until| now >= until) {
            return self.finish();
        }
        None
    }

    /// Incomplete protocol traffic cannot establish that the bytes came
    /// from the emulator. Keep the actual key time, including at EOF.
    pub(super) fn finish(&mut self) -> Option<u64> {
        self.pending.take().map(|pending| pending.last_at)
    }

    pub(super) fn observe(
        &mut self,
        bytes: &[u8],
        at: u64,
        cursor: &mut CursorQueries,
    ) -> Option<u64> {
        let mut human = self.expire(at);
        for &byte in bytes {
            let Some(mut pending) = self.pending.take() else {
                if byte == 0x1b {
                    self.pending = Some(Pending::escape(at));
                } else {
                    human = human.max(Some(at));
                }
                continue;
            };
            if byte == 0x1b
                && matches!(
                    pending.sequence,
                    Sequence::Escape | Sequence::Csi(_) | Sequence::ControlStart(_)
                )
            {
                human = human.max(Some(pending.last_at));
                self.pending = Some(Pending::escape(at));
                continue;
            }

            pending.length += 1;
            pending.last_at = at;
            if pending.length > MAX_REPORT_BYTES {
                human = human.max(Some(at));
                continue;
            }
            let complete = match &mut pending.sequence {
                Sequence::Escape => match byte {
                    b'[' => {
                        pending.sequence = Sequence::Csi(Vec::new());
                        None
                    }
                    b']' | b'P' | b'_' => {
                        pending.sequence = Sequence::ControlStart(byte);
                        None
                    }
                    b'^' | b'X' => {
                        pending.sequence = Sequence::ControlString { escape: false };
                        None
                    }
                    b'\\' => Some(false), // String terminator.
                    _ => Some(true),
                },
                Sequence::Csi(params) => match byte {
                    b'M' if params.is_empty() => {
                        pending.sequence = Sequence::X10Mouse(3);
                        None
                    }
                    0x40..=0x7e => {
                        use super::emulation::Scheme;
                        match (params.as_slice(), byte) {
                            (b"200", b'~') => self.in_paste = true,
                            (b"201", b'~') => self.in_paste = false,
                            (b"?997;1", b'n') if !self.in_paste => {
                                self.viewer_scheme = Some(Scheme::Dark)
                            }
                            (b"?997;2", b'n') if !self.in_paste => {
                                self.viewer_scheme = Some(Scheme::Light)
                            }
                            _ => {}
                        }
                        Some(!report(params, byte, cursor, at))
                    }
                    0x20..=0x3f if params.len() < 64 => {
                        params.push(byte);
                        None
                    }
                    _ => Some(true),
                },
                Sequence::ControlStart(kind) => {
                    let report = match *kind {
                        b']' => byte.is_ascii_digit() || matches!(byte, b'l' | b'L'),
                        b'P' => matches!(byte, b'!' | b'+' | b'=' | b'>' | b'0'..=b'9'),
                        b'_' => byte == b'G',
                        _ => false,
                    };
                    if report {
                        pending.sequence = Sequence::ControlString { escape: false };
                        None
                    } else {
                        Some(true)
                    }
                }
                Sequence::ControlString { escape } => {
                    if byte == 7 || (*escape && byte == b'\\') {
                        Some(false)
                    } else {
                        *escape = byte == 0x1b;
                        None
                    }
                }
                Sequence::X10Mouse(left) => {
                    *left -= 1;
                    (*left == 0).then_some(false)
                }
            };
            match complete {
                Some(true) => human = human.max(Some(at)),
                Some(false) => {}
                None => self.pending = Some(pending),
            }
        }
        human
    }
}

/// Ordinary cursor position replies share an encoding with modified F3.
/// Track queries forwarded to this viewer before using that encoding as
/// evidence of a report. Old queries cannot hide later key presses.
#[derive(Default)]
pub(super) struct CursorQueries {
    prefix: usize,
    sent: VecDeque<u64>,
}

impl CursorQueries {
    pub(super) fn output(&mut self, bytes: &[u8], at: u64) {
        let query = b"\x1b[6n";
        for &byte in bytes {
            if byte == query[self.prefix] {
                self.prefix += 1;
                if self.prefix == query.len() {
                    if self.sent.len() == 16 {
                        self.sent.pop_front();
                    }
                    self.sent.push_back(at);
                    self.prefix = 0;
                }
            } else {
                self.prefix = usize::from(byte == query[0]);
            }
        }
    }

    fn consume(&mut self, at: u64) -> bool {
        self.sent
            .retain(|sent| at.saturating_sub(*sent) < 2_000_000_000);
        self.sent.pop_front().is_some()
    }
}

fn report(params: &[u8], final_byte: u8, cursor: &mut CursorQueries, at: u64) -> bool {
    let numeric =
        |value: &[u8]| !value.is_empty() && value.iter().all(|c| c.is_ascii_digit() || *c == b';');
    let private = |prefix: u8| params.strip_prefix(&[prefix]).is_some_and(numeric);
    match final_byte {
        b'R' if numeric(params) && params.contains(&b';') => {
            let modified_f3 = params
                .strip_prefix(b"1;")
                .and_then(|modifier| std::str::from_utf8(modifier).ok()?.parse::<u8>().ok())
                .is_some_and(|modifier| (2..=16).contains(&modifier));
            cursor.consume(at) || !modified_f3
        }
        b'R' => private(b'?') && params.contains(&b';'),
        b'c' => private(b'?') || private(b'>') || private(b'='),
        b'u' => private(b'?'), // Kitty keyboard mode, unlike a Kitty key.
        b'y' => params
            .strip_suffix(b"$")
            .is_some_and(|value| numeric(value) || value.strip_prefix(b"?").is_some_and(numeric)),
        b'n' => numeric(params) || private(b'?'), // DSR and colour scheme.
        b'I' | b'O' => params.is_empty(),         // Focus notifications.
        b'M' | b'm' => {
            private(b'<')
                || (numeric(params) && params.contains(&b';'))
                || (final_byte == b'm' && private(b'>'))
        }
        b't' => numeric(params), // Window or pixel size.
        b'x' => {
            (params.starts_with(b"2;") || params.starts_with(b"3;"))
                && numeric(params)
                && params.split(|byte| *byte == b';').count() == 7
        }
        _ => false,
    }
}

/// Whether a viewer has left an unsubmitted line in the TUI's prompt: any
/// key since the last Enter, Ctrl-U or Ctrl-C. Escape sequences (arrows,
/// focus and mouse reports, a paste's brackets) say nothing either way;
/// the text of a paste does. Backspace might empty the line or not, so it
/// leaves the flag as it was.
#[derive(Default)]
pub(super) struct Draft {
    pending: bool,
    escape: Escape,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Escape {
    #[default]
    None,
    /// After ESC.
    Start,
    /// A CSI (`ESC [`) or SS3 (`ESC O`) sequence, up to its final byte.
    Sequence,
    /// SS3 takes one byte.
    Single,
    /// A control string (OSC, DCS, APC, PM), up to BEL or ST.
    String { escape: bool },
}

impl Draft {
    pub(super) fn pending(&self) -> bool {
        self.pending
    }

    pub(super) fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.escape = match self.escape {
                Escape::Start => match byte {
                    b'[' => Escape::Sequence,
                    b'O' => Escape::Single,
                    b']' | b'P' | b'_' | b'^' => Escape::String { escape: false },
                    0x1b => Escape::Start,
                    _ => Escape::None,
                },
                Escape::Sequence if (0x40..=0x7e).contains(&byte) => Escape::None,
                Escape::Sequence => Escape::Sequence,
                Escape::Single => Escape::None,
                Escape::String { escape } => match byte {
                    0x07 => Escape::None,
                    b'\\' if escape => Escape::None,
                    _ => Escape::String {
                        escape: byte == 0x1b,
                    },
                },
                Escape::None => match byte {
                    0x1b => Escape::Start,
                    b'\r' | b'\n' | 0x15 | 0x03 => {
                        self.pending = false;
                        Escape::None
                    }
                    0x7f | 0x08 => Escape::None,
                    0x09 | 0x20.. => {
                        self.pending = true;
                        Escape::None
                    }
                    _ => Escape::None,
                },
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_scheme_report_updates_the_viewer_without_counting_as_typing() {
        use super::super::emulation::Scheme;
        let mut input = Input::default();
        let mut cursor = CursorQueries::default();
        assert_eq!(input.observe(b"\x1b[?997;", 1, &mut cursor), None);
        assert_eq!(input.observe(b"2n", 2, &mut cursor), None);
        assert_eq!(input.take_viewer_scheme(), Some(Scheme::Light));
        input.observe(b"\x1b[200~\x1b[?997;1n\x1b[201~", 3, &mut cursor);
        assert_eq!(input.take_viewer_scheme(), None);
    }

    #[test]
    fn emulator_reports_are_not_input_even_when_split_across_frames() {
        let reports: &[&[u8]] = &[
            b"\x1b[25;80R",
            b"\x1b[?25;80R",
            b"\x1b[?62;22c",
            b"\x1b[>1;4000;0c",
            b"\x1b[?31u",
            b"\x1b[?2026;2$y",
            b"\x1bP>|Ghostty 1.4.0\x1b\\",
            b"\x1b]10;rgb:ffff/ffff/ffff\x07",
            b"\x1b]11;rgb:0000/0000/0000\x1b\\",
            b"\x1b]ltest title\x1b\\",
            b"\x1b[2;1;1;112;112;1;0x",
            b"\x1b[I",
            b"\x1b[O",
            b"\x1b[<32;21;4M",
            b"\x1b[<0;21;4m",
            b"\x1b[M !\"",
            b"\x1b[?997;1n",
            b"\x1b[6;20;10t",
            b"\x1b_Gi=1;OK\x1b\\",
        ];
        let mut input = Input::default();
        let mut cursor = CursorQueries::default();
        for report in reports {
            for byte in *report {
                assert_eq!(
                    input.observe(std::slice::from_ref(byte), 1, &mut cursor),
                    None,
                    "{report:?}"
                );
            }
            assert_eq!(input.finish(), None);
        }
        assert_eq!(input.observe(b"\x1b[?31u\x1b[B", 2, &mut cursor), Some(2));
        assert_eq!(input.observe(b"\x1b[97;2u", 3, &mut cursor), Some(3));
        assert_eq!(input.observe(b"\x1b]x", 4, &mut cursor), Some(4));
        assert_eq!(input.observe(b"yes\r", 5, &mut cursor), Some(5));
    }

    #[test]
    fn unfinished_reports_count_as_human_at_the_byte_time() {
        let mut input = Input::default();
        let mut cursor = CursorQueries::default();
        assert_eq!(input.observe(b"\x1b", 10, &mut cursor), None);
        assert_eq!(input.observe(b"Xreply", 20, &mut cursor), None);
        assert_eq!(input.expire(9 + REPORT_WAIT_NS), None);
        assert_eq!(input.expire(10 + REPORT_WAIT_NS), Some(20));
        assert_eq!(
            input.observe(b"later text", 30 + REPORT_WAIT_NS, &mut cursor),
            Some(30 + REPORT_WAIT_NS)
        );

        for prefix in [b"\x1b^".as_slice(), b"\x1bP>", b"\x1b]10;", b"\x1b_G"] {
            assert_eq!(
                input.observe(prefix, REPORT_WAIT_NS + 40, &mut cursor),
                None
            );
            let text = vec![b'x'; MAX_REPORT_BYTES];
            assert_eq!(
                input.observe(&text, REPORT_WAIT_NS + 50, &mut cursor),
                Some(REPORT_WAIT_NS + 50)
            );
            assert_eq!(input.finish(), None);
        }
        assert_eq!(
            input.observe(b"\x1b]11;unterminated", REPORT_WAIT_NS + 60, &mut cursor),
            None
        );
        assert_eq!(input.finish(), Some(REPORT_WAIT_NS + 60));
        assert_eq!(
            input.observe(b"\x1b", REPORT_WAIT_NS + 70, &mut cursor),
            None
        );
        assert_eq!(input.finish(), Some(REPORT_WAIT_NS + 70));
    }

    #[test]
    fn modified_f3_only_counts_as_a_report_when_a_query_is_outstanding() {
        let mut input = Input::default();
        let mut cursor = CursorQueries::default();
        assert_eq!(input.observe(b"\x1b[1;2R", 3, &mut cursor), Some(3));
        cursor.output(b"\x1b[", 4);
        cursor.output(b"6n", 5);
        assert_eq!(input.observe(b"\x1b[1;2R", 6, &mut cursor), None);
        assert_eq!(input.observe(b"\x1b[1;2R", 7, &mut cursor), Some(7));
    }

    #[test]
    fn a_draft_is_what_was_typed_since_the_last_enter() {
        let mut draft = Draft::default();
        draft.observe(b"hello");
        assert!(draft.pending());
        draft.observe(b"\r");
        assert!(!draft.pending());
        // Arrows, focus reports and a paste's brackets are not text; split
        // across frames they still are not.
        draft.observe(b"\x1b[A\x1b[I\x1b");
        draft.observe(b"[O\x1bOA\x1b]11;?\x07");
        assert!(!draft.pending());
        draft.observe(b"\x1b[200~pasted\x1b[201~");
        assert!(draft.pending());
        // Backspace may or may not empty the line: still a draft.
        draft.observe(b"\x7f\x7f\x7f\x7f\x7f\x7f");
        assert!(draft.pending());
        draft.observe(b"\x15");
        assert!(!draft.pending());
        draft.observe("é".as_bytes());
        assert!(draft.pending());
        draft.observe(b"\x03");
        assert!(!draft.pending());
    }
}
