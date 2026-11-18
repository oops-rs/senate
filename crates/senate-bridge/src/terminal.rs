//! Layout and styling for text written directly to a terminal.

use std::borrow::Cow;
use std::io::{IsTerminal, stdout};

use textwrap::Options;

#[derive(Clone, Copy)]
pub struct Terminal {
    columns: usize,
    color: bool,
}

impl Terminal {
    pub fn stdout() -> Option<Self> {
        let stdout = stdout();
        if !stdout.is_terminal() {
            return None;
        }
        let columns =
            terminal_size::terminal_size_of(&stdout).map_or(80, |(width, _)| usize::from(width.0));
        let color = !std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            && !std::env::var_os("TERM").is_some_and(|v| v == "dumb");
        Some(Self::new(columns, color))
    }

    pub(crate) fn new(columns: usize, color: bool) -> Self {
        Self {
            columns: columns.max(2),
            color,
        }
    }

    pub fn columns(self) -> usize {
        self.columns
    }

    pub(crate) fn emphasis(self, text: &str) -> Cow<'_, str> {
        if self.color {
            format!("\x1b[1;36m{text}\x1b[0m").into()
        } else {
            text.into()
        }
    }

    pub fn line(self, out: &mut String, text: &str, indent: usize, emphasis: bool) {
        let text = printable(text);
        let max_indent = self.columns.saturating_sub(2);
        let initial = " ".repeat(indent.min(max_indent));
        let continued = " ".repeat(indent.saturating_add(2).min(max_indent));
        let options = Options::new(self.columns)
            .initial_indent(&initial)
            .subsequent_indent(&continued);
        for line in textwrap::wrap(&text, options) {
            if emphasis {
                out.push_str(&self.emphasis(&line));
            } else {
                out.push_str(&line);
            }
            out.push('\n');
        }
    }
}

pub(crate) fn printable(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return text.into();
    }
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .into()
}
