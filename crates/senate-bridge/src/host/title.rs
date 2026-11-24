//! The attached terminal's title: the session's name for the whole
//! attachment, so a panel is found at a glance, and a short flash with the
//! bell when someone looks for it ([`FLASH_ON`], [`FLASH_OFF`]).
//!
//! What the agent writes to set its own title (`OSC 0`, `OSC 1`, `OSC 2`)
//! is taken out of what goes to the viewer ([`TitleFilter`]), so the
//! session's name stays. Nothing here is written into the agent's screen
//! or input: these bytes go only to the viewer's own terminal.

/// Save the viewer's title, then set it to `label` (at attach).
pub(crate) fn set(label: &str) -> Vec<u8> {
    let label: String = label.chars().filter(|c| !c.is_control()).collect();
    format!("\x1b[22;0t\x1b]2;{label}\x07").into_bytes()
}

/// Restore the title the viewer had before it attached (at detach).
pub(crate) const RESTORE: &[u8] = b"\x1b[23;0t";

/// Ring the bell and save the title, then show that this is the terminal
/// someone is looking for.
pub(crate) const FLASH_ON: &[u8] =
    b"\x07\x1b[22;0t\x1b]2;\xf0\x9f\x94\x94 Senate: this terminal\x07";

/// Back to the saved title, after [`FLASH_ON`].
pub(crate) const FLASH_OFF: &[u8] = b"\x1b[23;0t";

/// How long [`FLASH_ON`] shows before [`FLASH_OFF`].
pub(crate) const FLASH_FOR: std::time::Duration = std::time::Duration::from_secs(4);

/// Takes the title sequences (`ESC ] 0|1|2 ; … BEL` or `… ESC \`) out of
/// a byte stream that may split them anywhere; everything else passes.
#[derive(Debug, Default)]
pub(crate) struct TitleFilter {
    state: State,
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Text,
    /// Saw `ESC`.
    Escape,
    /// Saw `ESC ]` and these bytes of its number.
    Osc(Vec<u8>),
    /// Inside a title sequence: dropped until its end.
    Title,
    /// Inside a title sequence, saw `ESC` (maybe the start of `ESC \`).
    TitleEscape,
}

impl TitleFilter {
    /// `bytes` without the title sequences, carrying an incomplete one to
    /// the next call.
    pub(crate) fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        for &b in bytes {
            self.state = match std::mem::take(&mut self.state) {
                State::Text if b == 0x1b => State::Escape,
                State::Text => {
                    out.push(b);
                    State::Text
                }
                State::Escape if b == b']' => State::Osc(Vec::new()),
                State::Escape => {
                    out.extend_from_slice(&[0x1b, b]);
                    if b == 0x1b {
                        out.pop();
                        State::Escape
                    } else {
                        State::Text
                    }
                }
                State::Osc(mut number) => {
                    if b.is_ascii_digit() && number.len() < 4 {
                        number.push(b);
                        State::Osc(number)
                    } else if b == b';' && matches!(number.as_slice(), b"0" | b"1" | b"2") {
                        State::Title
                    } else {
                        // Another OSC: let it through as it was.
                        out.extend_from_slice(b"\x1b]");
                        out.extend_from_slice(&number);
                        out.push(b);
                        State::Text
                    }
                }
                State::Title if b == 0x07 => State::Text,
                State::Title if b == 0x1b => State::TitleEscape,
                State::Title => State::Title,
                State::TitleEscape if b == b'\\' => State::Text,
                State::TitleEscape => State::Title,
            };
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_sequences_are_taken_out_wherever_they_are_split() {
        let stream: &[u8] =
            b"a\x1b]0;\xe2\x9c\xb3 Claude\x07b\x1b]2;x\x1b\\c\x1b]11;?\x07d\x1b[1md\x1b]1;icon\x07e";
        let want: &[u8] = b"ab c\x1b]11;?\x07d\x1b[1mde";
        let want = want
            .iter()
            .filter(|b| **b != b' ')
            .copied()
            .collect::<Vec<_>>();
        assert_eq!(TitleFilter::default().filter(stream), want);
        // Byte by byte: the same.
        let mut filter = TitleFilter::default();
        let split: Vec<u8> = stream.iter().flat_map(|b| filter.filter(&[*b])).collect();
        assert_eq!(split, want);
    }

    #[test]
    fn the_title_is_the_sessions_and_restored_after() {
        assert_eq!(set("senate::main"), b"\x1b[22;0t\x1b]2;senate::main\x07");
        assert_eq!(
            set("a\x07b\x1bc"),
            b"\x1b[22;0t\x1b]2;abc\x07",
            "no control characters"
        );
        assert!(FLASH_ON.starts_with(b"\x07\x1b[22;0t"));
        assert_eq!(FLASH_OFF, RESTORE);
    }
}
