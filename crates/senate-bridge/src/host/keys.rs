//! Named keys for terminal use: what `senate terminal key` and the
//! `terminal` tool accept, and the bytes a terminal sends for each.
//!
//! Keys are written as a space-separated list: `down down enter`,
//! `ctrl-c`, `esc`, `shift-tab`, `alt-x`, `f5`, or any single character.
//! Modifiers go in front, joined by `-` or `+`. The encodings are the ones
//! xterm sends without any negotiated keyboard protocol, so a combination
//! such a terminal cannot tell apart from the plain key (`ctrl-enter`,
//! `shift-enter`) is refused by name instead of being sent as the plain key.

/// One key with its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Key {
    base: Base,
    shift: bool,
    alt: bool,
    ctrl: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Base {
    Char(char),
    Enter,
    Tab,
    Esc,
    Backspace,
    /// A cursor key, by the final byte of its sequence.
    Cursor(u8),
    /// A key sent as `CSI <n> ~`.
    Tilde(u8),
    /// F1 to F4, by the final byte of `SS3 <byte>`.
    LowFunction(u8),
}

/// The names `key` takes, for an error that has to list them.
pub(crate) const NAMES: &str = "enter, tab, esc, space, backspace, delete, insert, up, down, left, \
     right, home, end, pageup, pagedown, f1 to f12, or one character; with ctrl-, alt- or shift- \
     in front";

fn named(name: &str) -> Option<Base> {
    Some(match name {
        "enter" | "return" => Base::Enter,
        "tab" => Base::Tab,
        "esc" | "escape" => Base::Esc,
        "space" => Base::Char(' '),
        "backspace" => Base::Backspace,
        "up" => Base::Cursor(b'A'),
        "down" => Base::Cursor(b'B'),
        "right" => Base::Cursor(b'C'),
        "left" => Base::Cursor(b'D'),
        "home" => Base::Cursor(b'H'),
        "end" => Base::Cursor(b'F'),
        "insert" | "ins" => Base::Tilde(2),
        "delete" | "del" => Base::Tilde(3),
        "pageup" | "pgup" => Base::Tilde(5),
        "pagedown" | "pgdn" | "pgdown" => Base::Tilde(6),
        "f1" => Base::LowFunction(b'P'),
        "f2" => Base::LowFunction(b'Q'),
        "f3" => Base::LowFunction(b'R'),
        "f4" => Base::LowFunction(b'S'),
        "f5" => Base::Tilde(15),
        "f6" => Base::Tilde(17),
        "f7" => Base::Tilde(18),
        "f8" => Base::Tilde(19),
        "f9" => Base::Tilde(20),
        "f10" => Base::Tilde(21),
        "f11" => Base::Tilde(23),
        "f12" => Base::Tilde(24),
        _ => return None,
    })
}

/// The byte `ctrl` makes of `c`, for the characters that have one.
fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        '@'..='_' => Some(c as u8 & 0x1f),
        ' ' => Some(0),
        '?' => Some(0x7f),
        _ => None,
    }
}

fn parse_one(token: &str) -> Result<Key, String> {
    let mut key = Key {
        base: Base::Enter,
        shift: false,
        alt: false,
        ctrl: false,
    };
    let mut rest = token;
    loop {
        let lower = rest.to_ascii_lowercase();
        let modifier = [
            ("ctrl", 0),
            ("control", 0),
            ("alt", 1),
            ("meta", 1),
            ("shift", 2),
        ]
        .into_iter()
        .find(|(name, _)| {
            lower
                .strip_prefix(name)
                .is_some_and(|after| after.len() > 1 && after.starts_with(['-', '+']))
        });
        let Some((name, which)) = modifier else { break };
        match which {
            0 => key.ctrl = true,
            1 => key.alt = true,
            _ => key.shift = true,
        }
        rest = &rest[name.len() + 1..];
    }
    let mut chars = rest.chars();
    key.base = match (
        named(&rest.to_ascii_lowercase()),
        chars.next(),
        chars.next(),
    ) {
        (Some(base), ..) => base,
        (None, Some(c), None) => Base::Char(c),
        _ => return Err(format!("unknown key {token:?}; a key is {NAMES}")),
    };
    let unsendable = |what: &str| {
        Err(format!(
            "{token:?} cannot be sent: a terminal sends the same bytes for it as for {what}"
        ))
    };
    match key.base {
        Base::Char(c) => {
            if key.ctrl && control_byte(c.to_ascii_lowercase()).is_none() {
                return Err(format!(
                    "{token:?} cannot be sent: ctrl has no code for {c:?}"
                ));
            }
            if key.shift && !c.is_alphabetic() {
                return Err(format!(
                    "{token:?} cannot be sent: write the shifted character itself"
                ));
            }
        }
        Base::Enter if key.ctrl || key.shift => return unsendable("enter"),
        Base::Esc if key.ctrl || key.shift => return unsendable("esc"),
        Base::Tab if key.ctrl => return unsendable("tab"),
        Base::Backspace if key.shift => return unsendable("backspace"),
        _ => {}
    }
    Ok(key)
}

/// The keys in `keys`, in order.
pub(crate) fn parse(keys: &str) -> Result<Vec<Key>, String> {
    let parsed = keys
        .split_whitespace()
        .map(parse_one)
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        return Err(format!("no key given; a key is {NAMES}"));
    }
    Ok(parsed)
}

impl Key {
    /// xterm's modifier parameter: one more than the sum of the modifiers.
    fn modifier(self) -> u8 {
        1 + u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl)
    }

    /// Append what a terminal sends for this key. `application_cursor` is
    /// whether the program asked for application cursor keys (DECCKM).
    pub(crate) fn encode(self, application_cursor: bool, out: &mut Vec<u8>) {
        let modifier = self.modifier();
        match self.base {
            Base::Char(c) => {
                if self.alt {
                    out.push(0x1b);
                }
                if self.ctrl {
                    out.extend(control_byte(c.to_ascii_lowercase()));
                } else {
                    let c = if self.shift {
                        c.to_uppercase().next().unwrap_or(c)
                    } else {
                        c
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
            }
            Base::Enter | Base::Esc | Base::Backspace => {
                if self.alt {
                    out.push(0x1b);
                }
                out.push(match self.base {
                    Base::Enter => b'\r',
                    Base::Esc => 0x1b,
                    _ if self.ctrl => 0x08,
                    _ => 0x7f,
                });
            }
            Base::Tab => {
                if self.alt {
                    out.push(0x1b);
                }
                out.extend_from_slice(if self.shift { b"\x1b[Z" } else { b"\t" });
            }
            Base::Cursor(last) if modifier == 1 => {
                out.extend_from_slice(if application_cursor {
                    b"\x1bO"
                } else {
                    b"\x1b["
                });
                out.push(last);
            }
            Base::LowFunction(last) if modifier == 1 => {
                out.extend_from_slice(b"\x1bO");
                out.push(last);
            }
            Base::Cursor(last) | Base::LowFunction(last) => {
                out.extend_from_slice(format!("\x1b[1;{modifier}").as_bytes());
                out.push(last);
            }
            Base::Tilde(code) if modifier == 1 => {
                out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
            }
            Base::Tilde(code) => {
                out.extend_from_slice(format!("\x1b[{code};{modifier}~").as_bytes());
            }
        }
    }
}

/// The bytes typing `text` sends: each line end is Enter.
pub(crate) fn text_bytes(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent(keys: &str, application_cursor: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for key in parse(keys).unwrap() {
            key.encode(application_cursor, &mut out);
        }
        out
    }

    #[test]
    fn named_keys_and_chords_send_what_xterm_sends() {
        assert_eq!(sent("enter", false), b"\r");
        assert_eq!(sent("down down Enter", false), b"\x1b[B\x1b[B\r");
        assert_eq!(sent("up", true), b"\x1bOA", "application cursor keys");
        assert_eq!(sent("ctrl-c", false), [3]);
        assert_eq!(sent("Ctrl+D ctrl-[ ctrl-space", false), [4, 0x1b, 0]);
        assert_eq!(sent("esc tab shift-tab", false), b"\x1b\t\x1b[Z");
        assert_eq!(sent("alt-x alt-enter", false), b"\x1bx\x1b\r");
        assert_eq!(sent("backspace delete", false), b"\x7f\x1b[3~");
        assert_eq!(sent("f1 f5 f12", false), b"\x1bOP\x1b[15~\x1b[24~");
        assert_eq!(
            sent("pageup pgdn home end", false),
            b"\x1b[5~\x1b[6~\x1b[H\x1b[F"
        );
        assert_eq!(sent("ctrl-right shift-f5", true), b"\x1b[1;5C\x1b[15;2~");
        assert_eq!(sent("q : - + shift-a é", false), "q:-+Aé".as_bytes());
        assert_eq!(sent("space", false), b" ");
    }

    #[test]
    fn a_key_that_cannot_be_told_from_another_is_refused_by_name() {
        for keys in [
            "ctrl-enter",
            "shift-enter",
            "ctrl-tab",
            "shift-esc",
            "ctrl-1",
        ] {
            let error = parse(keys).unwrap_err();
            assert!(error.contains(keys), "{error}");
        }
        assert!(parse("entr").unwrap_err().contains("unknown key"));
        assert!(parse("   ").unwrap_err().contains("no key given"));
    }

    #[test]
    fn typed_line_ends_are_enter() {
        assert_eq!(text_bytes("a\nb\r\nc"), b"a\rb\rc");
    }
}
