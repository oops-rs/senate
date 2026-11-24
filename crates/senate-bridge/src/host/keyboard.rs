//! The TUI's keyboard state is virtual. Replay effective flags to the
//! attached terminal while keeping TUI pushes and pops off its own stack.

const STACK_LIMIT: usize = 32;

pub(super) const RESET_MODIFY_OTHER_KEYS: &[u8] = b"\x1b[>4;0m";
pub(super) const SHIFT_ENTER: &[u8] = b"\x1b[13;2u";
pub(super) const XTERM_SHIFT_ENTER: &[u8] = b"\x1b[27;2;13~";

#[derive(Default)]
struct Screen {
    flags: u16,
    stack: Vec<u16>,
}

#[derive(Default)]
pub(super) struct Keyboard {
    main: Screen,
    alternate: Screen,
    in_alternate: bool,
    modify_other_keys: u16,
}

impl Keyboard {
    fn active(&mut self) -> &mut Screen {
        if self.in_alternate {
            &mut self.alternate
        } else {
            &mut self.main
        }
    }

    fn flags(&self) -> u16 {
        if self.in_alternate {
            self.alternate.flags
        } else {
            self.main.flags
        }
    }

    pub(super) fn answer(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(format!("\x1b[?{}u", self.flags()).as_bytes());
    }

    pub(super) fn select_screen(&mut self, alternate: bool, out: &mut Vec<u8>) {
        if self.in_alternate != alternate {
            self.in_alternate = alternate;
            set_flags(out, self.flags());
        }
    }

    pub(super) fn in_alternate(&self) -> bool {
        self.in_alternate
    }

    pub(super) fn change(&mut self, prefix: u8, params: &[u8], out: &mut Vec<u8>) -> bool {
        let Some((first, second)) = parameters(params) else {
            return false;
        };
        let previous = self.flags();
        let screen = self.active();
        match (prefix, second) {
            (b'>', None) => {
                if screen.stack.len() == STACK_LIMIT {
                    screen.stack.remove(0);
                }
                screen.stack.push(screen.flags);
                screen.flags = first.unwrap_or(0);
            }
            (b'<', None) => {
                let count = usize::from(first.unwrap_or(1));
                if count > screen.stack.len() {
                    screen.stack.clear();
                    screen.flags = 0;
                } else if count != 0 {
                    let new_len = screen.stack.len() - count;
                    screen.flags = screen.stack[new_len];
                    screen.stack.truncate(new_len);
                }
            }
            (b'=', mode) => match mode.unwrap_or(1) {
                1 => screen.flags = first.unwrap_or(0),
                2 => screen.flags |= first.unwrap_or(0),
                3 => screen.flags &= !first.unwrap_or(0),
                _ => return false,
            },
            _ => return false,
        }
        // A bare pop is part of several runtimes' startup reset. It must
        // not pop the viewer's stack or emit a spurious mode change.
        if prefix != b'<' || previous != self.flags() {
            set_flags(out, self.flags());
        }
        true
    }

    pub(super) fn modify_other_keys(&mut self, params: &[u8]) {
        if let Some((Some(4), mode)) = parameters(params)
            && mode.unwrap_or(0) <= 2
        {
            self.modify_other_keys = mode.unwrap_or(0);
        }
    }

    pub(super) fn preamble(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        set_flags(&mut bytes, self.flags());
        bytes.extend_from_slice(format!("\x1b[>4;{}m", self.modify_other_keys).as_bytes());
        bytes
    }
}

fn parameters(params: &[u8]) -> Option<(Option<u16>, Option<u16>)> {
    let mut split = std::str::from_utf8(params).ok()?.split(';');
    let parse = |field: Option<&str>| -> Option<Option<u16>> {
        match field {
            Some(text) if !text.is_empty() => Some(Some(text.parse().ok()?)),
            _ => Some(None),
        }
    };
    let first = parse(split.next())?;
    let second = parse(split.next())?;
    split.next().is_none().then_some((first, second))
}

pub(super) fn set_flags(out: &mut Vec<u8>, flags: u16) {
    out.extend_from_slice(format!("\x1b[={flags}u").as_bytes());
}
