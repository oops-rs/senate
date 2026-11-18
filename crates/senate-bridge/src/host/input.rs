//! Distinguish terminal protocol reports from input that can attend to a
//! prompt. The relay forwards both unchanged. Only the distinction leaves
//! this module, and a report can span multiple frames.

#[derive(Default)]
pub(super) enum Input {
    #[default]
    Ground,
    Escape,
    Csi(Vec<u8>),
    ControlString {
        escape: bool,
    },
    X10Mouse(u8),
}

impl Input {
    pub(super) fn observe(&mut self, bytes: &[u8]) -> bool {
        let mut human = false;
        for &byte in bytes {
            match self {
                Self::Ground if byte == 0x1b => *self = Self::Escape,
                Self::Ground => human = true,
                Self::Escape => match byte {
                    b'[' => *self = Self::Csi(Vec::new()),
                    b']' | b'P' | b'_' | b'^' | b'X' => {
                        *self = Self::ControlString { escape: false };
                    }
                    b'\\' => *self = Self::Ground, // String terminator.
                    0x1b => human = true,          // Two independent Escape presses.
                    _ => {
                        *self = Self::Ground;
                        human = true;
                    }
                },
                Self::Csi(params) => match byte {
                    b'M' if params.is_empty() => *self = Self::X10Mouse(3),
                    0x40..=0x7e => {
                        human |= !report(params, byte);
                        *self = Self::Ground;
                    }
                    0x20..=0x3f if params.len() < 64 => params.push(byte),
                    0x20..=0x3f => {} // A bounded prefix suffices for classification.
                    0x1b => *self = Self::Escape,
                    _ => {
                        *self = Self::Ground;
                        human = true;
                    }
                },
                Self::ControlString { escape } => {
                    if byte == 7 || (*escape && byte == b'\\') {
                        *self = Self::Ground;
                    } else {
                        *escape = byte == 0x1b;
                    }
                }
                Self::X10Mouse(left) => {
                    *left -= 1;
                    if *left == 0 {
                        *self = Self::Ground;
                    }
                }
            }
        }
        human
    }

    /// The attach client has already waited for a possible split detach
    /// sequence. An escape left at the end of its input stream was a key.
    pub(super) fn end_of_input(&self) -> bool {
        matches!(self, Self::Escape)
    }
}

fn report(params: &[u8], final_byte: u8) -> bool {
    let numeric =
        |value: &[u8]| !value.is_empty() && value.iter().all(|c| c.is_ascii_digit() || *c == b';');
    let private = |prefix: u8| params.strip_prefix(&[prefix]).is_some_and(numeric);
    match final_byte {
        // Cursor position, including the extended DEC form. The ordinary
        // response shares its encoding with some modified F3 sequences.
        b'R' => (numeric(params) || private(b'?')) && params.contains(&b';'),
        b'c' => private(b'?') || private(b'>') || private(b'='),
        b'u' => private(b'?'), // Kitty keyboard mode, unlike a Kitty key.
        b'y' => params
            .strip_suffix(b"$")
            .is_some_and(|value| numeric(value) || value.strip_prefix(b"?").is_some_and(numeric)),
        b'n' => numeric(params) || private(b'?'), // DSR and colour scheme.
        b'I' | b'O' => params.is_empty(),         // Focus notifications.
        b'M' | b'm' => private(b'<') || (numeric(params) && params.contains(&b';')),
        b't' => numeric(params), // Window or pixel size.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for report in reports {
            for byte in *report {
                assert!(!input.observe(std::slice::from_ref(byte)), "{report:?}");
            }
            assert!(!input.end_of_input());
        }
        assert!(input.observe(b"\x1b[?31u\x1b[B"));
        assert!(input.observe(b"\x1b[97;2u"));
        assert!(input.observe(b"yes\r"));
        assert!(!input.observe(b"\x1b"));
        assert!(input.end_of_input());
    }
}
