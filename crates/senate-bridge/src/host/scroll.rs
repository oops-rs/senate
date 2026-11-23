//! Local navigation of a hosted terminal's main screen.

pub(super) const MOUSE_ON: &[u8] = b"\x1b[?1000;1006h";
pub(super) const MOUSE_OFF: &[u8] = b"\x1b[?1000;1006l";
pub(super) const STEP: usize = 3;

/// A complete input event, or a prefix to hold until the next read.
pub(super) enum Mouse {
    Other,
    Incomplete,
    Report { len: usize, scroll: isize },
}

pub(super) fn mouse(bytes: &[u8]) -> Mouse {
    const START: &[u8] = b"\x1b[<";
    if bytes.len() < START.len() {
        return if START.starts_with(bytes) {
            Mouse::Incomplete
        } else {
            Mouse::Other
        };
    }
    if !bytes.starts_with(START) {
        return Mouse::Other;
    }
    let Some(end) = bytes[START.len()..]
        .iter()
        .position(|b| !b.is_ascii_digit() && *b != b';')
        .map(|end| end + START.len())
    else {
        return if bytes.len() < 32 {
            Mouse::Incomplete
        } else {
            Mouse::Other
        };
    };
    if end >= 32 || !matches!(bytes[end], b'M' | b'm') {
        return Mouse::Other;
    }
    let params = std::str::from_utf8(&bytes[START.len()..end]).unwrap_or_default();
    let mut params = params.split(';');
    let Some(button) = params.next().and_then(|p| p.parse::<u8>().ok()) else {
        return Mouse::Other;
    };
    if params.clone().count() != 2 || !params.all(|p| p.parse::<u32>().is_ok()) {
        return Mouse::Other;
    }
    // Ignore Shift, Alt and Ctrl. Do not interpret motion or horizontal
    // wheel reports as vertical scrolls.
    let scroll = match (bytes[end], button & !28) {
        (b'M', 64) => STEP as isize,
        (b'M', 65) => -(STEP as isize),
        _ => 0,
    };
    Mouse::Report {
        len: end + 1,
        scroll,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Event {
    Keys(Vec<u8>),
    Scroll(isize),
}

#[derive(Default)]
pub(super) struct Input {
    pending: Vec<u8>,
    pasting: bool,
}

impl Input {
    pub(super) fn waiting(&self) -> bool {
        !self.pasting && !self.pending.is_empty() && !self.pending.starts_with(b"\x1b[<")
    }

    pub(super) fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// Keep real keys and bracketed paste bytes intact. A mouse report can
    /// be split across reads, even when attached through an older client.
    pub(super) fn feed(&mut self, bytes: &[u8], local: bool) -> Vec<Event> {
        self.pending.extend_from_slice(bytes);
        if !local {
            self.pasting = false;
            let keys = self.flush();
            return if keys.is_empty() {
                Vec::new()
            } else {
                vec![Event::Keys(keys)]
            };
        }
        let mut keys = Vec::new();
        let mut events = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            let rest = &self.pending[at..];
            let delimiter: &[u8] = if self.pasting {
                b"\x1b[201~"
            } else {
                b"\x1b[200~"
            };
            if rest.starts_with(delimiter) {
                self.pasting = !self.pasting;
                keys.extend_from_slice(delimiter);
                at += delimiter.len();
                continue;
            }
            if delimiter.starts_with(rest) {
                break;
            }
            if !self.pasting {
                match mouse(rest) {
                    Mouse::Other => {}
                    Mouse::Incomplete => break,
                    Mouse::Report { len, scroll } => {
                        if scroll != 0 {
                            if !keys.is_empty() {
                                events.push(Event::Keys(std::mem::take(&mut keys)));
                            }
                            events.push(Event::Scroll(scroll));
                        }
                        at += len;
                        continue;
                    }
                }
            }
            keys.push(rest[0]);
            at += 1;
        }
        self.pending.drain(..at);
        if !keys.is_empty() {
            events.push(Event::Keys(keys));
        }
        events
    }
}

pub(super) fn move_by(screen: &mut vt100::Screen, rows: isize) -> bool {
    let before = screen.scrollback();
    screen.set_scrollback(before.saturating_add_signed(rows));
    screen.scrollback() != before
}

#[derive(Clone, Copy, Default)]
pub(super) struct ModeChange {
    pub(super) mouse: bool,
    pub(super) exits_alternate: bool,
}

/// Cut at complete DEC mode controls so a viewer override never interrupts
/// the next escape sequence or lets later text land on the shell's screen.
pub(super) fn mode_chunks(mut bytes: &[u8]) -> impl Iterator<Item = (&[u8], ModeChange)> {
    std::iter::from_fn(move || {
        if bytes.is_empty() {
            return None;
        }
        let boundary = bytes.windows(3).enumerate().find_map(|(at, prefix)| {
            if prefix != b"\x1b[?" {
                return None;
            }
            let tail = &bytes[at + 3..];
            let end = tail
                .iter()
                .take_while(|b| b.is_ascii_digit() || **b == b';')
                .count();
            let set = match tail.get(end)? {
                b'h' => true,
                b'l' => false,
                _ => return None,
            };
            let mut change = ModeChange::default();
            let mut alternate = false;
            for mode in tail[..end].split(|b| *b == b';') {
                match mode {
                    b"47" | b"1049" => alternate = true,
                    b"9" | b"1000" | b"1001" | b"1002" | b"1003" | b"1005" | b"1006" | b"1015"
                    | b"1016" => change.mouse = true,
                    _ => {}
                }
            }
            change.exits_alternate = alternate && !set;
            (alternate || change.mouse).then_some((at + 3 + end + 1, change))
        });
        let (end, change) = boundary.unwrap_or((bytes.len(), ModeChange::default()));
        let (chunk, rest) = bytes.split_at(end);
        bytes = rest;
        Some((chunk, change))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wheel_is_local_only_on_the_main_screen_and_outside_a_paste() {
        let wheel = b"\x1b[<64;9;2M";
        let mut input = Input::default();
        assert_eq!(input.feed(wheel, true), [Event::Scroll(3)]);
        assert_eq!(input.feed(wheel, false), [Event::Keys(wheel.to_vec())]);
        let paste = [b"\x1b[200~".as_slice(), wheel, b"\x1b[201~"].concat();
        assert_eq!(input.feed(&paste, true), [Event::Keys(paste)]);
        assert!(!input.waiting());
        assert_eq!(
            input.feed(&[b"\x1b[A".as_slice(), wheel].concat(), true),
            [Event::Keys(b"\x1b[A".to_vec()), Event::Scroll(3)]
        );
    }
}
