//! Terminal colour queries and reporting modes for hosted TUIs.
//!
//! The host answers colour queries itself, using the attached viewer's
//! palette where available and the system palette otherwise. It replays
//! reporting modes to a newly attached terminal.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Reporting modes whose state the viewer's terminal has to share with the
/// TUI. The attach client turns the same modes off when it detaches.
const REPORTING: [u16; 5] = [9, 1000, 1001, 1002, 1003];
const ENCODING: [u16; 4] = [1005, 1006, 1015, 1016];
const OTHER_MODES: [u16; 3] = [1004, 2004, 2031];
/// The terminal reports light/dark changes as `CSI ? 997 ; 1|2 n`.
const THEME_NOTIFY: u16 = 2031;
const SYNCHRONIZED_OUTPUT: u16 = 2026;
pub(super) const BEGIN_SYNC: &[u8] = b"\x1b[?2026h";
pub(super) const END_SYNC: &[u8] = b"\x1b[?2026l";
/// Longest unfinished sequence kept between two chunks of output.
const CARRY: usize = 32;
/// Primary device attributes of a VT220 with ANSI colour, as common
/// terminals report them. It claims nothing a viewer's terminal lacks.
const DEVICE_ATTRIBUTES: &[u8] = b"\x1b[?62;22c";

/// What to write back to the TUI for one chunk of its output.
#[derive(Default)]
struct Answers {
    colors: Vec<u8>,
    attributes: bool,
}

impl Answers {
    /// The device attributes go last whatever order they were asked in:
    /// they tell the TUI that no more answers are coming.
    fn finish(mut self) -> Vec<u8> {
        if self.attributes {
            self.colors.extend_from_slice(DEVICE_ATTRIBUTES);
        }
        self.colors
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_flags: Option<u16>,
}

impl Colors {
    fn system_palette(scheme: Scheme) -> Self {
        let (foreground, background) = match scheme {
            Scheme::Dark => ("rgb:eeee/eeee/eeee", "rgb:1e1e/1e1e/1e1e"),
            Scheme::Light => ("rgb:4c4c/4f4f/6969", "rgb:efef/f1f1/f5f5"),
        };
        Self {
            foreground: Some(foreground.into()),
            background: Some(background.into()),
            scheme: Some(scheme),
            keyboard_flags: None,
        }
    }

    /// Drop anything that is not a plain colour specification. These strings
    /// are written into a TUI's input, so only `rgb:` hex forms pass.
    pub(crate) fn checked(mut self) -> Self {
        self.foreground = self.foreground.filter(|spec| rgb(spec).is_some());
        self.background = self.background.filter(|spec| rgb(spec).is_some());
        if self.scheme.is_none()
            && let Some([red, green, blue]) = self.background.as_deref().and_then(rgb)
        {
            // Perceived brightness of the background when 996 is unsupported.
            let brightness = 0.299 * red + 0.587 * green + 0.114 * blue;
            self.scheme = Some(if brightness < 0.5 {
                Scheme::Dark
            } else {
                Scheme::Light
            });
        }
        self
    }
}

fn rgb(spec: &str) -> Option<[f64; 3]> {
    let (channels, alpha) = match spec.split_once(':')? {
        ("rgb", channels) => (channels, false),
        ("rgba", channels) => (channels, true),
        _ => return None,
    };
    let mut channels = channels.split('/');
    let mut read = || {
        let component = channels.next()?;
        if !(1..=4).contains(&component.len()) || !component.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        let value = u16::from_str_radix(component, 16).ok()?;
        Some(f64::from(value) / f64::from(u16::MAX >> (4 * (4 - component.len()))))
    };
    let rgb = [read()?, read()?, read()?];
    if alpha {
        read()?;
    }
    channels.next().is_none().then_some(rgb)
}

pub(crate) async fn system_scheme() -> Scheme {
    #[cfg(target_os = "macos")]
    if let Ok(Ok(output)) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tokio::process::Command::new("/usr/bin/defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .kill_on_drop(true)
            .output(),
    )
    .await
        && output.status.success()
        && output.stdout.trim_ascii() == b"Dark"
    {
        return Scheme::Dark;
    }
    Scheme::Light
}

pub(crate) struct Emulation {
    modes: BTreeSet<u16>,
    keyboard: super::keyboard::Keyboard,
    system: Colors,
    viewer: Colors,
    viewer_ready: bool,
    synchronized: bool,
    carry: Vec<u8>,
}

impl Emulation {
    pub(crate) fn new(scheme: Scheme) -> Self {
        Self {
            modes: BTreeSet::new(),
            keyboard: super::keyboard::Keyboard::default(),
            system: Colors::system_palette(scheme),
            viewer: Colors::default(),
            viewer_ready: false,
            synchronized: false,
            carry: Vec::new(),
        }
    }

    /// Return the response for the TUI and the bytes for the attached
    /// terminal. Suppress the queries the host has answered so the viewer
    /// cannot send a duplicate reply.
    pub(crate) fn route(&mut self, bytes: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let mut data = std::mem::take(&mut self.carry);
        data.extend_from_slice(bytes);
        let mut answers = Answers::default();
        let mut display = Vec::with_capacity(data.len());
        let mut after = Vec::new();
        let mut at = 0;
        while let Some(start) = data[at..].iter().position(|b| *b == 0x1b).map(|i| at + i) {
            display.extend_from_slice(&data[at..start]);
            match self.sequence(&data[start..], &mut answers, &mut after) {
                Some((len, handled)) => {
                    at = start + len;
                    if !handled {
                        display.extend_from_slice(&data[start..at]);
                    }
                    display.append(&mut after);
                }
                None => {
                    if data.len() - start <= CARRY {
                        self.carry.extend_from_slice(&data[start..]);
                    } else {
                        display.extend_from_slice(&data[start..]);
                    }
                    at = data.len();
                    break;
                }
            }
        }
        display.extend_from_slice(&data[at..]);
        (answers.finish(), display)
    }

    /// Length of the sequence and whether the host consumed the query.
    fn sequence(
        &mut self,
        data: &[u8],
        answers: &mut Answers,
        after: &mut Vec<u8>,
    ) -> Option<(usize, bool)> {
        match *data.get(1)? {
            b'[' => {
                let prefix = *data.get(2)?;
                if matches!(prefix, b'>' | b'<' | b'=') {
                    let end = 3 + data[3..]
                        .iter()
                        .take_while(|byte| byte.is_ascii_digit() || **byte == b';')
                        .count();
                    match *data.get(end)? {
                        b'u' if self.keyboard.change(prefix, &data[3..end], after) => {
                            return Some((end + 1, true));
                        }
                        b'm' if prefix == b'>' => {
                            self.keyboard.modify_other_keys(&data[3..end]);
                            return Some((end + 1, false));
                        }
                        _ => return Some((2, false)),
                    }
                }
                // `CSI c` or `CSI 0 c`: the request for device attributes.
                let request = match (*data.get(2)?, data.get(3)) {
                    (b'?', _) => None,
                    (b'c', _) => Some(3),
                    (b'0', None) => return None,
                    (b'0', Some(b'c')) => Some(4),
                    _ => return Some((2, false)),
                };
                if let Some(len) = request {
                    answers.attributes = true;
                    return Some((len, true));
                }
                let params = data[3..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit() || **b == b';')
                    .count();
                let end = 3 + params;
                let last = *data.get(end)?;
                if last == 0x1b {
                    // Not a sequence after all; the ESC starts the next one.
                    return Some((end, false));
                }
                let numbers = || {
                    std::str::from_utf8(&data[3..end])
                        .unwrap_or_default()
                        .split(';')
                        .filter_map(|n| n.parse::<u16>().ok())
                };
                match last {
                    b'u' if params == 0 => {
                        self.keyboard.answer(&mut answers.colors);
                        return Some((end + 1, true));
                    }
                    b'h' => numbers().for_each(|mode| {
                        if matches!(mode, 47 | 1047 | 1049) {
                            self.keyboard.select_screen(true, after);
                        }
                        if mode == SYNCHRONIZED_OUTPUT {
                            self.synchronized = true;
                        }
                        if let Some(group) = [&REPORTING[..], &ENCODING[..]]
                            .into_iter()
                            .find(|group| group.contains(&mode))
                        {
                            self.modes.retain(|previous| !group.contains(previous));
                            self.modes.insert(mode);
                        } else if OTHER_MODES.contains(&mode) {
                            self.modes.insert(mode);
                        }
                    }),
                    b'l' => numbers().for_each(|mode| {
                        if matches!(mode, 47 | 1047 | 1049) {
                            self.keyboard.select_screen(false, after);
                        }
                        if mode == SYNCHRONIZED_OUTPUT {
                            self.synchronized = false;
                        }
                        self.modes.remove(&mode);
                    }),
                    b'$' if numbers().eq([SYNCHRONIZED_OUTPUT]) && *data.get(end + 1)? == b'p' => {
                        // An attached terminal answers for itself. Before the
                        // subscriber exists, answer for the virtual terminal.
                        if !self.viewer_ready {
                            let reply = if self.synchronized {
                                b"\x1b[?2026;1$y"
                            } else {
                                b"\x1b[?2026;2$y"
                            };
                            answers.colors.extend_from_slice(reply);
                        }
                        return Some((end + 2, !self.viewer_ready));
                    }
                    b'n' if numbers().eq([996]) => {
                        if let Some(scheme) = self.scheme() {
                            answers.colors.extend_from_slice(scheme.report());
                        }
                        return Some((end + 1, true));
                    }
                    _ => {}
                }
                Some((end + 1, false))
            }
            b']' => {
                // Only `OSC 10 ; ?` and `OSC 11 ; ?` matter, so look no
                // further than their length for the terminator.
                let body = &data[2..data.len().min(2 + 8)];
                let bel = body.iter().position(|b| *b == 0x07);
                let st = body.windows(2).position(|w| w == b"\x1b\\");
                let (len, terminator): (usize, &[u8]) = match (bel, st) {
                    (Some(bel), Some(st)) if st < bel => (st, b"\x1b\\"),
                    (Some(bel), _) => (bel, b"\x07"),
                    (None, Some(st)) => (st, b"\x1b\\"),
                    (None, None) if body.len() < 8 => return None,
                    (None, None) => return Some((2, false)),
                };
                let color = match &body[..len] {
                    b"10;?" => self
                        .viewer
                        .foreground
                        .as_deref()
                        .or(self.system.foreground.as_deref())
                        .map(|spec| ("10", spec)),
                    b"11;?" => self
                        .viewer
                        .background
                        .as_deref()
                        .or(self.system.background.as_deref())
                        .map(|spec| ("11", spec)),
                    _ => None,
                };
                if let Some((code, spec)) = color {
                    answers
                        .colors
                        .extend_from_slice(format!("\x1b]{code};{spec}").as_bytes());
                    answers.colors.extend_from_slice(terminator);
                }
                Some((2 + len + terminator.len(), color.is_some()))
            }
            b'c' => {
                self.modes.clear();
                self.keyboard = super::keyboard::Keyboard::default();
                Some((2, false))
            }
            _ => Some((1, false)),
        }
    }

    /// What a newly attached viewer's terminal has to be told so that it is
    /// in the state the TUI believes its terminal is in.
    pub(crate) fn attach_preamble(&self) -> Vec<u8> {
        Self::mode_preamble(self.modes.iter().copied())
    }

    /// Called with the same lock used to subscribe to TUI output, so a
    /// forwarded terminal query always has a receiver.
    pub(super) fn attach(&mut self) -> Vec<u8> {
        self.viewer_ready = true;
        let mut preamble = self.attach_preamble();
        if self.synchronized {
            preamble.extend_from_slice(BEGIN_SYNC);
        }
        preamble
    }

    /// Keyboard flags apply after the snapshot selects its screen.
    pub(super) fn keyboard_preamble(&self) -> Vec<u8> {
        self.keyboard.preamble()
    }

    fn mode_preamble(modes: impl Iterator<Item = u16>) -> Vec<u8> {
        let modes: Vec<String> = modes.map(|mode| mode.to_string()).collect();
        if modes.is_empty() {
            Vec::new()
        } else {
            format!("\x1b[?{}h", modes.join(";")).into_bytes()
        }
    }

    fn scheme(&self) -> Option<Scheme> {
        self.viewer.scheme.or(self.system.scheme)
    }

    fn theme_report(&self) -> &'static [u8] {
        match self.scheme() {
            Some(scheme) if self.modes.contains(&THEME_NOTIFY) => scheme.report(),
            _ => b"",
        }
    }

    pub(crate) fn viewer(&mut self, colors: Colors) -> &'static [u8] {
        self.viewer = colors.checked();
        self.theme_report()
    }

    pub(crate) fn detach(&mut self) -> &'static [u8] {
        self.viewer_ready = false;
        let had_viewer_scheme = self.viewer.scheme.is_some();
        self.viewer = Colors::default();
        // A terminal may have sent its own theme reports directly during
        // the attachment, so announce the fallback even when it matches.
        if had_viewer_scheme {
            self.theme_report()
        } else {
            b""
        }
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn system(&mut self, scheme: Scheme) -> &'static [u8] {
        let previous = self.scheme();
        if self.system.scheme != Some(scheme) {
            self.system = Colors::system_palette(scheme);
        }
        if self.scheme() != previous {
            self.theme_report()
        } else {
            b""
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Emulation {
        fn output(&mut self, bytes: &[u8]) -> Vec<u8> {
            self.route(bytes).0
        }
    }

    #[test]
    fn keyboard_negotiation_and_replay_keep_independent_screen_stacks() {
        let mut tui = Emulation::new(Scheme::Light);
        assert_eq!(tui.route(b"\x1b[?u\x1b[c").0, b"\x1b[?0u\x1b[?62;22c");
        assert!(tui.route(b"\x1b[>").1.is_empty());
        assert_eq!(tui.route(b"5u\x1b[>4;2m").1, b"\x1b[=5u\x1b[>4;2m");
        assert_eq!(tui.keyboard_preamble(), b"\x1b[=5u\x1b[>4;2m");
        assert_eq!(tui.route(b"\x1b[?1049h").1, b"\x1b[?1049h\x1b[=0u");
        assert_eq!(tui.route(b"\x1b[>7u").1, b"\x1b[=7u");
        assert_eq!(tui.route(b"\x1b[?u").0, b"\x1b[?7u");
        assert_eq!(tui.keyboard_preamble(), b"\x1b[=7u\x1b[>4;2m");
        assert_eq!(tui.route(b"\x1b[?1049l").1, b"\x1b[?1049l\x1b[=5u");
        assert_eq!(tui.route(b"\x1b[<u").1, b"\x1b[=0u");
        assert_eq!(tui.route(b"\x1b[?u").0, b"\x1b[?0u");
    }

    #[test]
    fn kitty_set_operations_and_counted_pop_do_not_reach_the_viewer_stack() {
        let mut tui = Emulation::new(Scheme::Light);
        assert_eq!(tui.route(b"\x1b[<u").1, b"");
        assert_eq!(
            tui.route(b"\x1b[>1u\x1b[=4;2u\x1b[=1;3u").1,
            b"\x1b[=1u\x1b[=5u\x1b[=4u"
        );
        assert_eq!(tui.route(b"\x1b[>7u\x1b[<2u").1, b"\x1b[=7u\x1b[=0u");
    }

    #[test]
    fn a_headless_tui_can_discover_synchronized_output() {
        let mut tui = Emulation::new(Scheme::Light);
        assert_eq!(tui.route(b"\x1b[?2026$"), (vec![], vec![]));
        let (answer, display) = tui.route(b"p");
        assert_eq!(answer, b"\x1b[?2026;2$y");
        assert!(display.is_empty());
        assert_eq!(tui.route(BEGIN_SYNC).1, BEGIN_SYNC);
        assert_eq!(tui.route(b"\x1b[?2026$p").0, b"\x1b[?2026;1$y");
        assert_eq!(tui.attach(), BEGIN_SYNC);
        let (answer, display) = tui.route(b"\x1b[?2026$p\x1b[?2026l");
        assert!(answer.is_empty());
        assert_eq!(display, b"\x1b[?2026$p\x1b[?2026l");
        tui.detach();
        assert_eq!(tui.route(b"\x1b[?2026$p").0, b"\x1b[?2026;2$y");
    }

    #[test]
    fn modes_turned_on_while_unattended_are_replayed_to_a_viewer() {
        let mut tui = Emulation::new(Scheme::Light);
        // Split inside a sequence, as a PTY read can.
        assert!(tui.output(b"\x1b[?2004h\x1b[?10").is_empty());
        assert!(tui.output(b"04h\x1b[?25h\x1b[?2031;1000h").is_empty());
        assert_eq!(tui.attach_preamble(), b"\x1b[?1000;1004;2004;2031h");
        // A mode turned off again is not replayed; the cursor never is.
        tui.output(b"\x1b[?1000l");
        assert_eq!(tui.attach_preamble(), b"\x1b[?1004;2004;2031h");
        assert!(Emulation::new(Scheme::Light).attach_preamble().is_empty());
    }

    #[test]
    fn changing_mouse_protocols_does_not_resurrect_old_modes_on_attach() {
        let mut tui = Emulation::new(Scheme::Light);
        tui.output(b"\x1b[?1000;1005h\x1b[?1002;1006h");
        assert_eq!(tui.attach_preamble(), b"\x1b[?1002;1006h");
        tui.output(b"\x1b[?1002l");
        assert_eq!(tui.attach_preamble(), b"\x1b[?1006h");
        tui.output(b"\x1bc");
        assert!(tui.attach_preamble().is_empty());
    }

    #[test]
    fn colour_questions_use_the_viewer_and_fall_back_to_the_system() {
        let mut tui = Emulation::new(Scheme::Light);
        let asked = b"before\x1b]10;?\x1b\\\x1b]11;?\x07\x1b[?996nafter";
        let light = b"\x1b]10;rgb:4c4c/4f4f/6969\x1b\\\x1b]11;rgb:efef/f1f1/f5f5\x07\x1b[?997;2n";
        let dark = b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\\x1b]11;rgb:0/0/0\x07\x1b[?997;1n";
        let (reply, display) = tui.route(asked);
        assert_eq!(reply, light);
        assert_eq!(display, b"beforeafter");
        tui.viewer(Colors {
            foreground: Some("rgb:ffff/ffff/ffff".into()),
            background: Some("rgb:0/0/0".into()),
            scheme: None,
            ..Colors::default()
        });
        assert_eq!(tui.output(asked), dark);

        tui.viewer(Colors {
            foreground: Some("rgb:f/f/f".into()),
            background: Some("rgb:e/e/e".into()),
            scheme: Some(Scheme::Light),
            ..Colors::default()
        });
        assert_eq!(
            tui.output(asked),
            b"\x1b]10;rgb:f/f/f\x1b\\\x1b]11;rgb:e/e/e\x07\x1b[?997;2n"
        );
        tui.viewer(Colors::default());
        assert_eq!(tui.output(asked), light);
        // The nearer terminator ends the question.
        assert_eq!(
            tui.output(b"\x1b]11;?\x1b\\\x07"),
            b"\x1b]11;rgb:efef/f1f1/f5f5\x1b\\"
        );
        // An ESC where a final byte should be starts the next sequence.
        tui.output(b"\x1b[?12\x1b[?2004h");
        assert_eq!(tui.attach_preamble(), b"\x1b[?2004h");
        // Other OSC traffic, long or short, is left alone.
        assert!(tui.output(b"\x1b]0;a long window title\x07").is_empty());
        assert!(tui.output(b"\x1b]11;rgb:0000/0000/0000\x07").is_empty());
    }

    #[test]
    fn the_device_attributes_answer_closes_what_was_asked() {
        // Claude Code asks for the background, then for device attributes,
        // and keeps the colour only if the second answer arrives.
        let mut tui = Emulation::new(Scheme::Light);
        assert_eq!(
            tui.output(b"\x1b]11;?\x07"),
            b"\x1b]11;rgb:efef/f1f1/f5f5\x07"
        );
        assert_eq!(tui.output(b"\x1b[c"), DEVICE_ATTRIBUTES);
        // Codex asks for everything at once, attributes first or last.
        assert_eq!(
            tui.output(b"\x1b[c\x1b[?u\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[6n"),
            b"\x1b[?0u\x1b]10;rgb:4c4c/4f4f/6969\x1b\\\x1b]11;rgb:efef/f1f1/f5f5\x1b\\\x1b[?62;22c"
        );
        // `CSI 0 c` is the same request, also when a read ends inside it.
        assert!(tui.output(b"\x1b[0").is_empty());
        assert_eq!(tui.output(b"c"), DEVICE_ATTRIBUTES);
        // A terminal with nothing on record is still a terminal.
        assert_eq!(
            Emulation::new(Scheme::Light).output(b"\x1b[c"),
            DEVICE_ATTRIBUTES
        );
        // The host closes the query even if the viewer cannot answer.
        assert_eq!(tui.output(b"\x1b[c\x1b[0c"), DEVICE_ATTRIBUTES);
        // Neither the secondary request nor ordinary output is one.
        assert!(tui.output(b"\x1b[>c\x1b[>0q\x1b[0m\x1b[0;1mc").is_empty());
    }

    #[test]
    fn a_question_split_across_reads_is_still_answered() {
        let mut tui = Emulation::new(Scheme::Light);
        assert!(tui.output(b"text\x1b]11").is_empty());
        assert_eq!(
            tui.output(b";?\x1b\\more"),
            b"\x1b]11;rgb:efef/f1f1/f5f5\x1b\\"
        );
    }

    #[test]
    fn subscribed_tui_is_notified_when_the_effective_scheme_changes() {
        let mut tui = Emulation::new(Scheme::Light);
        assert!(tui.system(Scheme::Dark).is_empty());
        tui.output(b"\x1b[?2031h");
        assert_eq!(tui.system(Scheme::Light), b"\x1b[?997;2n");
        assert_eq!(tui.system(Scheme::Dark), b"\x1b[?997;1n");
        assert!(tui.system(Scheme::Dark).is_empty());
        let light = Colors {
            scheme: Some(Scheme::Light),
            ..Colors::default()
        };
        assert_eq!(tui.viewer(light), b"\x1b[?997;2n");
        assert!(tui.system(Scheme::Light).is_empty());
        assert!(tui.system(Scheme::Dark).is_empty());
        assert_eq!(tui.detach(), b"\x1b[?997;1n");
        assert_eq!(
            tui.output(b"\x1b]11;?\x07"),
            b"\x1b]11;rgb:1e1e/1e1e/1e1e\x07"
        );
    }

    #[test]
    fn only_plain_colour_specifications_reach_a_tui() {
        let colors = Colors {
            foreground: Some("rgb:ffff/ffff/ffff".into()),
            background: Some("rgb:00/00/00\x1b[2J".into()),
            scheme: None,
            ..Colors::default()
        }
        .checked();
        assert_eq!(colors.foreground.as_deref(), Some("rgb:ffff/ffff/ffff"));
        assert_eq!(colors.background, None);
    }
}
