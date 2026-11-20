//! The part of a terminal the host has to be while nobody is attached.
//!
//! A hosted TUI talks to its terminal: it turns reporting modes on and asks
//! for the terminal's colours. With a viewer attached the viewer's terminal
//! hears that and answers. Without one, nobody does, so two things go wrong
//! that this module puts right:
//!
//! - A mode the TUI turned on before a viewer attached was never seen by
//!   that viewer's terminal. [`Emulation::attach_preamble`] replays the
//!   modes that are on, so bracketed paste, focus and theme reports work
//!   after attaching as they do in a terminal the TUI started in.
//! - A colour question asked while unattended gets no answer, and a TUI
//!   that asks only at startup (Codex) never learns the theme.
//!   [`Emulation::output`] answers from [`Colors`], what the person's
//!   terminal last said about itself.
//!
//! Nothing here interprets or stores what is on the screen.

use std::collections::BTreeSet;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// Reporting modes whose state the viewer's terminal has to share with the
/// TUI. The attach client turns the same modes off when it detaches.
const REPLAYED: [u16; 8] = [1000, 1002, 1003, 1004, 1006, 1016, 2004, 2031];
/// The terminal reports light/dark changes as `CSI ? 997 ; 1|2 n`.
const THEME_NOTIFY: u16 = 2031;
/// Longest unfinished sequence kept between two chunks of output.
const CARRY: usize = 32;

/// Whether the person's terminal is showing a light or a dark theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Scheme {
    Dark,
    Light,
}

impl Scheme {
    fn report(self) -> &'static [u8] {
        match self {
            Self::Dark => b"\x1b[?997;1n",
            Self::Light => b"\x1b[?997;2n",
        }
    }
}

/// What the person's terminal last said about its colours: the answers to
/// `OSC 10 ; ?`, `OSC 11 ; ?` and `CSI ? 996 n`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Colors {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<Scheme>,
}

impl Colors {
    fn file(paths: &Paths) -> PathBuf {
        paths.home().join("terminal.json")
    }

    /// The colours on record, or none. A record that cannot be read is
    /// treated as absent: the TUI then gets no answer, as before.
    pub(crate) fn load(paths: &Paths) -> Self {
        std::fs::read(Self::file(paths))
            .ok()
            .and_then(|data| serde_json::from_slice::<Self>(&data).ok())
            .map(Self::checked)
            .unwrap_or_default()
    }

    /// Record them for hosts that start later. Only inside an existing home.
    pub(crate) fn save(&self, paths: &Paths) -> io::Result<()> {
        if self == &Self::default() || !paths.home().is_dir() {
            return Ok(());
        }
        let file = Self::file(paths);
        let staged = file.with_extension("tmp");
        std::fs::write(&staged, serde_json::to_vec(self)?)?;
        std::fs::rename(staged, file)
    }

    /// Drop anything that is not a plain colour specification. These strings
    /// are written into a TUI's input, so only `rgb:` hex forms pass.
    pub(crate) fn checked(mut self) -> Self {
        let plain = |spec: &String| {
            spec.len() <= 32
                && spec
                    .strip_prefix("rgb:")
                    .or_else(|| spec.strip_prefix("rgba:"))
                    .is_some_and(|rest| {
                        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit() || b == b'/')
                    })
        };
        self.foreground = self.foreground.filter(plain);
        self.background = self.background.filter(plain);
        self
    }
}

#[derive(Default)]
pub(crate) struct Emulation {
    modes: BTreeSet<u16>,
    colors: Colors,
    /// The scheme the TUI was last told, so it is told only of a change.
    told: Option<Scheme>,
    carry: Vec<u8>,
}

impl Emulation {
    pub(crate) fn new(colors: Colors) -> Self {
        Self {
            colors,
            ..Self::default()
        }
    }

    /// Follow what the TUI wrote. Returns what its terminal should answer
    /// when no viewer's terminal is there to do it; with a viewer attached
    /// the answer is that terminal's to give and this returns nothing.
    pub(crate) fn output(&mut self, bytes: &[u8], attached: bool) -> Vec<u8> {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let mut answers = Vec::new();
        let mut at = 0;
        while let Some(start) = data[at..].iter().position(|b| *b == 0x1b).map(|i| at + i) {
            match self.sequence(&data[start..], attached, &mut answers) {
                Some(len) => at = start + len,
                None => {
                    // Unfinished: wait for the rest in the next chunk.
                    if data.len() - start <= CARRY {
                        self.carry = data[start..].to_vec();
                    }
                    return answers;
                }
            }
        }
        answers
    }

    /// Length of the sequence at the start of `data` (an ESC), having acted
    /// on it if it is one this module follows. `None` if it is cut short.
    fn sequence(&mut self, data: &[u8], attached: bool, answers: &mut Vec<u8>) -> Option<usize> {
        match *data.get(1)? {
            b'[' => {
                if *data.get(2)? != b'?' {
                    return Some(2);
                }
                let params = data[3..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit() || **b == b';')
                    .count();
                let end = 3 + params;
                let last = *data.get(end)?;
                let numbers = || {
                    std::str::from_utf8(&data[3..end])
                        .unwrap_or_default()
                        .split(';')
                        .filter_map(|n| n.parse::<u16>().ok())
                };
                match last {
                    b'h' => self
                        .modes
                        .extend(numbers().filter(|mode| REPLAYED.contains(mode))),
                    b'l' => numbers().for_each(|mode| {
                        self.modes.remove(&mode);
                    }),
                    b'n' if !attached && numbers().eq([996]) => {
                        if let Some(scheme) = self.colors.scheme {
                            answers.extend_from_slice(scheme.report());
                            self.told = Some(scheme);
                        }
                    }
                    _ => {}
                }
                Some(end + 1)
            }
            b']' => {
                // Only `OSC 10 ; ?` and `OSC 11 ; ?` matter, so look no
                // further than their length for the terminator.
                let body = &data[2..data.len().min(2 + 8)];
                let (len, terminator): (usize, &[u8]) =
                    if let Some(i) = body.iter().position(|b| *b == 0x07) {
                        (i, b"\x07")
                    } else if let Some(i) = body.windows(2).position(|w| w == b"\x1b\\") {
                        (i, b"\x1b\\")
                    } else if body.len() < 8 {
                        return None;
                    } else {
                        return Some(2);
                    };
                let color = match &body[..len] {
                    b"10;?" => self.colors.foreground.as_deref().map(|spec| ("10", spec)),
                    b"11;?" => self.colors.background.as_deref().map(|spec| ("11", spec)),
                    _ => None,
                };
                if let (false, Some((code, spec))) = (attached, color) {
                    answers.extend_from_slice(format!("\x1b]{code};{spec}").as_bytes());
                    answers.extend_from_slice(terminator);
                }
                Some(2 + len + terminator.len())
            }
            _ => Some(1),
        }
    }

    /// What a newly attached viewer's terminal has to be told so that it is
    /// in the state the TUI believes its terminal is in.
    pub(crate) fn attach_preamble(&self) -> Vec<u8> {
        if self.modes.is_empty() {
            return Vec::new();
        }
        let modes: Vec<String> = self.modes.iter().map(u16::to_string).collect();
        format!("\x1b[?{}h", modes.join(";")).into_bytes()
    }

    /// The attached viewer's terminal has said what it looks like. Returns
    /// what to tell the TUI: a theme report, if it asked for those and the
    /// theme is not the one it was last told.
    pub(crate) fn viewer(&mut self, colors: Colors) -> &'static [u8] {
        let colors = colors.checked();
        if colors.foreground.is_some() {
            self.colors.foreground = colors.foreground;
        }
        if colors.background.is_some() {
            self.colors.background = colors.background;
        }
        if colors.scheme.is_some() {
            self.colors.scheme = colors.scheme;
        }
        match self.colors.scheme {
            Some(scheme) if self.modes.contains(&THEME_NOTIFY) && self.told != Some(scheme) => {
                self.told = Some(scheme);
                scheme.report()
            }
            _ => b"",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latte() -> Colors {
        Colors {
            foreground: Some("rgb:4c4c/4f4f/6969".into()),
            background: Some("rgb:efef/f1f1/f5f5".into()),
            scheme: Some(Scheme::Light),
        }
    }

    #[test]
    fn modes_turned_on_while_unattended_are_replayed_to_a_viewer() {
        let mut tui = Emulation::default();
        // Split inside a sequence, as a PTY read can.
        assert!(tui.output(b"\x1b[?2004h\x1b[?10", false).is_empty());
        assert!(
            tui.output(b"04h\x1b[?25h\x1b[?2031;1000h", false)
                .is_empty()
        );
        assert_eq!(tui.attach_preamble(), b"\x1b[?1000;1004;2004;2031h");
        // A mode turned off again is not replayed; the cursor never is.
        tui.output(b"\x1b[?1000l", true);
        assert_eq!(tui.attach_preamble(), b"\x1b[?1004;2004;2031h");
        assert!(Emulation::default().attach_preamble().is_empty());
    }

    #[test]
    fn colour_questions_are_answered_only_while_nobody_is_attached() {
        let mut tui = Emulation::new(latte());
        let asked = b"\x1b]10;?\x1b\\\x1b]11;?\x07\x1b[?996n";
        assert_eq!(
            tui.output(asked, false),
            b"\x1b]10;rgb:4c4c/4f4f/6969\x1b\\\x1b]11;rgb:efef/f1f1/f5f5\x07\x1b[?997;2n"
        );
        // The viewer's own terminal answers these.
        assert!(tui.output(asked, true).is_empty());
        // Nothing is on record: stay silent, as a host always did.
        assert!(Emulation::default().output(asked, false).is_empty());
        // Other OSC traffic, long or short, is left alone.
        assert!(
            tui.output(b"\x1b]0;a long window title\x07", false)
                .is_empty()
        );
        assert!(
            tui.output(b"\x1b]11;rgb:0000/0000/0000\x07", false)
                .is_empty()
        );
    }

    #[test]
    fn a_question_split_across_reads_is_still_answered() {
        let mut tui = Emulation::new(latte());
        assert!(tui.output(b"text\x1b]11", false).is_empty());
        assert_eq!(
            tui.output(b";?\x1b\\more", false),
            b"\x1b]11;rgb:efef/f1f1/f5f5\x1b\\"
        );
    }

    #[test]
    fn a_tui_that_asked_for_theme_reports_is_told_of_a_change_once() {
        let mut tui = Emulation::default();
        // No theme reports requested: the viewer's colours are only kept.
        assert!(tui.viewer(latte()).is_empty());
        tui.output(b"\x1b[?2031h", false);
        assert_eq!(tui.viewer(latte()), b"\x1b[?997;2n");
        assert!(tui.viewer(latte()).is_empty());
        let mocha = Colors {
            scheme: Some(Scheme::Dark),
            ..Colors::default()
        };
        assert_eq!(tui.viewer(mocha), b"\x1b[?997;1n");
        // A viewer that could not say keeps what is known.
        assert!(tui.viewer(Colors::default()).is_empty());
        assert_eq!(
            tui.output(b"\x1b]11;?\x07", false),
            b"\x1b]11;rgb:efef/f1f1/f5f5\x07"
        );
    }

    #[test]
    fn only_plain_colour_specifications_reach_a_tui() {
        let colors = Colors {
            foreground: Some("rgb:ffff/ffff/ffff".into()),
            background: Some("rgb:00/00/00\x1b[2J".into()),
            scheme: None,
        }
        .checked();
        assert_eq!(colors.foreground.as_deref(), Some("rgb:ffff/ffff/ffff"));
        assert_eq!(colors.background, None);
    }
}
