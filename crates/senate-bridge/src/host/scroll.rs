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

pub(super) fn move_by(screen: &mut vt100::Screen, rows: isize) -> bool {
    let before = screen.scrollback();
    screen.set_scrollback(before.saturating_add_signed(rows));
    screen.scrollback() != before
}
