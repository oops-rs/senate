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

/// Whether a viewer has left an unsubmitted line in the TUI's prompt.
///
/// Tracks plain sequential typing by character count so backspacing all typed
/// characters back to empty recovers automatically, while partial backspaces,
/// cursor movement, pastes, and multi-line edits stay conservatively protected
/// until an explicit submit or clear (legacy `\r`/`\n`/`^U`/`^C`, Kitty CSI u,
/// or `modifyOtherKeys`). Terminal protocol traffic — including X10 mouse
/// reports (`CSI M <3 bytes>`), SGR mouse reports, focus events, and Kitty key
/// releases — never marks a draft pending.
#[derive(Default)]
pub(super) struct Draft {
    state: DraftState,
    escape: Escape,
    in_paste: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum DraftState {
    #[default]
    Empty,
    /// Exact number of plain characters typed at the end of a single line.
    Exact(usize),
    /// Non-empty, but cursor movement, paste, or complex editing makes exact
    /// backspace counting unsafe; cleared only by submit or line-clear.
    Unknown,
}

#[derive(Default, Clone, PartialEq, Eq)]
enum Escape {
    #[default]
    None,
    /// After ESC.
    Start,
    /// A CSI (`ESC [`) sequence's parameter/intermediate bytes, up to its final byte.
    Sequence(Vec<u8>),
    /// SS3 (`ESC O`) takes one byte.
    Single,
    /// X10 mouse report (`CSI M`) takes three raw payload bytes.
    X10Mouse(u8),
    /// A control string (OSC, DCS, APC, PM), up to BEL or ST.
    String { escape: bool },
}

impl Draft {
    pub(super) fn pending(&self) -> bool {
        !matches!(self.state, DraftState::Empty)
    }

    fn clear(&mut self) {
        self.state = DraftState::Empty;
    }

    fn push_char(&mut self) {
        self.state = if self.in_paste {
            DraftState::Unknown
        } else {
            match self.state {
                DraftState::Empty => DraftState::Exact(1),
                DraftState::Exact(n) => DraftState::Exact(n.saturating_add(1)),
                DraftState::Unknown => DraftState::Unknown,
            }
        };
    }

    fn backspace(&mut self) {
        if self.in_paste {
            self.state = DraftState::Unknown;
            return;
        }
        self.state = match self.state {
            DraftState::Empty => DraftState::Empty,
            DraftState::Exact(0 | 1) => DraftState::Empty,
            DraftState::Exact(n) => DraftState::Exact(n - 1),
            DraftState::Unknown => DraftState::Unknown,
        };
    }

    fn degrade_if_pending(&mut self) {
        if matches!(self.state, DraftState::Exact(_)) {
            self.state = DraftState::Unknown;
        }
    }

    fn observe_csi(&mut self, params: &[u8], final_byte: u8) -> Escape {
        // X10 mouse report: CSI M followed by 3 raw bytes (>= 0x20).
        if params.is_empty() && final_byte == b'M' {
            return Escape::X10Mouse(3);
        }
        // Bracketed paste boundaries.
        if final_byte == b'~' && params == b"200" {
            self.in_paste = true;
            return Escape::None;
        }
        if final_byte == b'~' && params == b"201" {
            self.in_paste = false;
            return Escape::None;
        }
        // Kitty keyboard protocol: `CSI <key>[:...] [; <mods>[:<event>]] [; ...] u`.
        // `CSI ? <flags> u` is a capability report, never a key press.
        if final_byte == b'u' && !params.starts_with(b"?") {
            self.observe_kitty_key(params);
            return Escape::None;
        }
        // xterm modifyOtherKeys: `CSI 27 ; <mods> ; <codepoint> ~`.
        if final_byte == b'~' && params.starts_with(b"27;") {
            self.observe_modify_other_key(params);
            return Escape::None;
        }
        // Cursor navigation or forward-delete while a draft is active makes
        // naive backspace counting unsafe: keep it conservatively protected.
        if matches!(final_byte, b'A' | b'B' | b'C' | b'D' | b'H' | b'F')
            || (final_byte == b'~' && matches!(params, b"1" | b"3" | b"4" | b"7" | b"8"))
        {
            self.degrade_if_pending();
        }
        Escape::None
    }

    fn observe_kitty_key(&mut self, params: &[u8]) {
        let Ok(text) = std::str::from_utf8(params) else {
            return;
        };
        let mut parts = text.split(';');
        let Some(key_part) = parts.next() else {
            return;
        };
        let Some(codepoint) = key_part
            .split(':')
            .next()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            return;
        };
        let (mods, event_type) = match parts.next() {
            Some(mod_part) if !mod_part.is_empty() => {
                let mut sub = mod_part.split(':');
                let mods = sub.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
                let event = sub.next().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
                (mods, event)
            }
            _ => (1, 1),
        };
        // Key-release events (`:3`) neither start nor clear a draft.
        if event_type == 3 {
            return;
        }
        self.apply_encoded_key(codepoint, mods);
    }

    fn observe_modify_other_key(&mut self, params: &[u8]) {
        let Ok(text) = std::str::from_utf8(params) else {
            return;
        };
        let mut parts = text.split(';');
        if parts.next() != Some("27") {
            return;
        }
        let Some(mods) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            return;
        };
        let Some(codepoint) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            return;
        };
        self.apply_encoded_key(codepoint, mods);
    }

    fn apply_encoded_key(&mut self, codepoint: u32, mods: u32) {
        // Mask out CapsLock (64) and NumLock (128); keep Shift(1), Alt(2),
        // Ctrl(4), Super(8), Hyper(16), Meta(32).
        let active_mods = mods.saturating_sub(1) & 0b0011_1111;
        let is_enter = codepoint == 13 || codepoint == 57414;
        if is_enter {
            if self.in_paste {
                self.state = DraftState::Unknown;
            } else if active_mods == 0 {
                self.clear();
            } else {
                // Shift+Enter / Alt+Enter inserts a newline in a multi-line draft.
                self.degrade_if_pending();
            }
            return;
        }
        // Ctrl-U or Ctrl-C clears the input line.
        if active_mods == 4 && matches!(codepoint, 99 | 67 | 117 | 85) && !self.in_paste {
            self.clear();
            return;
        }
        // Unmodified Backspace.
        if matches!(codepoint, 127 | 8) {
            if active_mods == 0 {
                self.backspace();
            } else {
                self.degrade_if_pending();
            }
            return;
        }
        // Tab completion or indentation.
        if codepoint == 9 && active_mods == 0 {
            self.state = DraftState::Unknown;
            return;
        }
        // Printable Unicode key (unmodified or Shift-only), excluding Kitty's
        // Private Use Area functional key codes (57344..=63743).
        let is_printable =
            codepoint >= 32 && codepoint != 127 && !(57344..=63743).contains(&codepoint);
        if is_printable && (active_mods & !1) == 0 {
            self.push_char();
        } else if active_mods != 0 {
            self.degrade_if_pending();
        }
    }

    pub(super) fn observe(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            let escape = std::mem::take(&mut self.escape);
            self.escape = match escape {
                Escape::Start => match byte {
                    b'[' => Escape::Sequence(Vec::new()),
                    b'O' => Escape::Single,
                    b']' | b'P' | b'_' | b'^' => Escape::String { escape: false },
                    0x1b => Escape::Start,
                    _ => Escape::None,
                },
                Escape::Sequence(params) if (0x40..=0x7e).contains(&byte) => {
                    self.observe_csi(&params, byte)
                }
                Escape::Sequence(mut params) => {
                    if params.len() < 64 {
                        params.push(byte);
                    }
                    Escape::Sequence(params)
                }
                Escape::Single => {
                    if matches!(byte, b'A' | b'B' | b'C' | b'D' | b'H' | b'F') {
                        self.degrade_if_pending();
                    }
                    Escape::None
                }
                Escape::X10Mouse(left) => {
                    if left <= 1 {
                        Escape::None
                    } else {
                        Escape::X10Mouse(left - 1)
                    }
                }
                Escape::String { escape } => match byte {
                    0x07 => Escape::None,
                    b'\\' if escape => Escape::None,
                    _ => Escape::String {
                        escape: byte == 0x1b,
                    },
                },
                Escape::None => match byte {
                    0x1b => Escape::Start,
                    b'\r' | b'\n' => {
                        if self.in_paste {
                            self.state = DraftState::Unknown;
                        } else {
                            self.clear();
                        }
                        Escape::None
                    }
                    0x15 | 0x03 => {
                        if !self.in_paste {
                            self.clear();
                        }
                        Escape::None
                    }
                    0x7f | 0x08 => {
                        self.backspace();
                        Escape::None
                    }
                    0x09 => {
                        self.state = DraftState::Unknown;
                        Escape::None
                    }
                    // Line-editing / cursor-movement control keys (Ctrl-A/B/E/F/K/W).
                    0x01 | 0x02 | 0x05 | 0x06 | 0x0b | 0x17 => {
                        self.degrade_if_pending();
                        Escape::None
                    }
                    0x20..=0x7e | 0xc0..=0xff => {
                        self.push_char();
                        Escape::None
                    }
                    // UTF-8 continuation bytes: already counted on the leading byte.
                    0x80..=0xbf => {
                        if self.in_paste {
                            self.state = DraftState::Unknown;
                        } else if matches!(self.state, DraftState::Empty) {
                            self.state = DraftState::Exact(1);
                        }
                        Escape::None
                    }
                    _ => Escape::None,
                },
            };
        }
    }
}

/// Whether `keys` is a single menu/dialog selection key (1–9, y/n, Space) that
/// answers an on-screen prompt rather than starting a composer draft.
pub(super) fn is_modal_choice_key(keys: &[u8]) -> bool {
    matches!(keys, [b'1'..=b'9' | b'y' | b'Y' | b'n' | b'N' | b' '])
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

    #[test]
    fn x10_mouse_reports_do_not_set_a_false_draft() {
        let mut draft = Draft::default();
        // X10 mouse press + release (including bytes >= 0x20, split across frames).
        draft.observe(b"\x1b[M !\"");
        assert!(!draft.pending(), "X10 mouse click must not set draft");
        draft.observe(b"\x1b[M");
        draft.observe(b"#");
        draft.observe(b"ab");
        assert!(
            !draft.pending(),
            "split X10 mouse report must not set draft"
        );
        // SGR mouse reports also stay clear.
        draft.observe(b"\x1b[<0;21;4M\x1b[<0;21;4m");
        assert!(!draft.pending());
    }

    #[test]
    fn kitty_and_modify_other_keys_enter_clear_and_protect_drafts() {
        let mut draft = Draft::default();
        // Plain text submitted via Kitty Enter variants clears the draft.
        for enter in [
            b"\x1b[13u".as_slice(),
            b"\x1b[13;1u",
            b"\x1b[13;1:1u",
            b"\x1b[57414u",
            b"\x1b[27;1;13~",
        ] {
            draft.observe(b"hello");
            assert!(draft.pending());
            draft.observe(enter);
            assert!(!draft.pending(), "enter {enter:?} must clear draft");
        }
        // Shift+Enter / Alt+Enter keep a multi-line draft protected, and a key
        // release event (:3) must not clear a real draft.
        draft.observe(b"line1");
        draft.observe(b"\x1b[13;2u");
        assert!(draft.pending(), "Shift+Enter must preserve draft");
        draft.observe(b"\x1b[13;1:3u");
        assert!(draft.pending(), "Enter key-release must not clear draft");
        // Kitty Ctrl-U and Ctrl-C clear it.
        draft.observe(b"\x1b[117;5u");
        assert!(!draft.pending(), "Kitty Ctrl-U must clear draft");
        draft.observe(b"again");
        draft.observe(b"\x1b[99;5u");
        assert!(!draft.pending(), "Kitty Ctrl-C must clear draft");
        draft.observe(b"again");
        draft.observe(b"\x1b[27;5;117~");
        assert!(!draft.pending(), "modifyOtherKeys Ctrl-U must clear draft");

        // Kitty capability report and key release never set a false draft;
        // Kitty printable key press protects a real draft.
        draft.observe(b"\x1b[?31u\x1b[97;1:3u");
        assert!(!draft.pending());
        draft.observe(b"\x1b[97u");
        assert!(draft.pending(), "Kitty printable press must protect draft");
        draft.observe(b"\x1b[127u");
        assert!(
            !draft.pending(),
            "Kitty Backspace back to empty clears draft"
        );
    }

    #[test]
    fn backspacing_plain_typing_to_empty_recovers_while_partial_or_moved_draft_stays_protected() {
        let mut draft = Draft::default();
        draft.observe(b"abc");
        assert!(draft.pending());
        draft.observe(b"\x7f");
        assert!(draft.pending(), "partial backspace leaves 'ab' protected");
        draft.observe(b"\x7f\x7f");
        assert!(!draft.pending(), "backspacing all typed chars clears draft");

        // Multi-byte UTF-8 characters count per character, not per byte.
        draft.observe("你好".as_bytes());
        assert!(draft.pending());
        draft.observe(b"\x7f");
        assert!(draft.pending(), "one character left");
        draft.observe(b"\x7f");
        assert!(!draft.pending(), "both characters deleted");

        // Cursor movement degrades exact counting so deleting at col 0 cannot
        // falsely clear text still to the right of the cursor.
        draft.observe(b"abc");
        draft.observe(b"\x1b[D");
        draft.observe(b"\x7f\x7f\x7f");
        assert!(draft.pending(), "cursor-moved draft stays protected");
        draft.observe(b"\x15");
        assert!(!draft.pending());

        // Multi-line bracketed paste ending in a newline stays protected until
        // submitted outside the paste.
        draft.observe(b"\x1b[200~line1\nline2\n\x1b[201~");
        assert!(draft.pending(), "pasted newline must not clear draft");
        draft.observe(b"\r");
        assert!(!draft.pending());
    }
}
