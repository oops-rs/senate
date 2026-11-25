//! Layout and styling for text written directly to a terminal.

use std::borrow::Cow;
use std::io::{IsTerminal, stdout};

use textwrap::Options;
use textwrap::core::display_width;

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
        let columns = terminal_size::terminal_size_of(&stdout)
            .map(|(width, _)| width.0)
            .filter(|&width| width > 0)
            .or_else(|| {
                std::env::var("COLUMNS")
                    .ok()?
                    .parse::<u16>()
                    .ok()
                    .filter(|&w| w > 0)
            })
            .map_or(80, usize::from);
        let color = !std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            && !std::env::var_os("TERM").is_some_and(|v| v == "dumb");
        Some(Self::new(columns, color))
    }

    pub fn new(columns: usize, color: bool) -> Self {
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
        self.line_hanging(out, text, indent, indent.saturating_add(2), emphasis);
    }

    /// As [`Self::line`], with the wrapped rest starting at column `hang`.
    pub fn line_hanging(
        self,
        out: &mut String,
        text: &str,
        indent: usize,
        hang: usize,
        emphasis: bool,
    ) {
        let text = printable(text);
        let max_indent = self.columns.saturating_sub(2);
        let initial = " ".repeat(indent.min(max_indent));
        let continued = " ".repeat(hang.min(max_indent));
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

/// Emphasis at a terminal, the plain text elsewhere.
fn label(terminal: Option<Terminal>, text: &str) -> Cow<'_, str> {
    match terminal {
        Some(terminal) => terminal.emphasis(text),
        None => text.into(),
    }
}

/// Label and value lines, the values in one column.
pub fn fields(terminal: Option<Terminal>, rows: &[(&str, String)]) -> String {
    let width = rows
        .iter()
        .map(|(name, _)| display_width(name))
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (name, value) in rows {
        let pad = " ".repeat(width - display_width(name));
        out.push_str(&label(terminal, name));
        out.push_str(&pad);
        out.push_str("  ");
        out.push_str(&printable(value));
        out.push('\n');
    }
    out
}

/// A table whose columns are as wide as what they hold.
pub fn table(terminal: Option<Terminal>, headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| display_width(h)).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(display_width(cell));
        }
    }
    let line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut line = String::new();
        for (cell, width) in cells.zip(&widths) {
            line.push_str(cell);
            line.push_str(&" ".repeat(width - display_width(cell) + 2));
        }
        line.trim_end().to_owned()
    };
    let mut out = String::new();
    out.push_str(&label(terminal, &line(&mut headers.iter().copied())));
    out.push('\n');
    for row in rows {
        out.push_str(&line(&mut row.iter().map(String::as_str)));
        out.push('\n');
    }
    out
}

/// The same text with each senator written short, for a person to read:
/// `[senator::claude-6d8b2e5d-4a60-486b-b3d8-21f2d82e4382]` becomes
/// `claude-6d8b2e5d…4382`, and `[senator::alice]` becomes `alice`. A line
/// that quotes what someone wrote (indented, or behind a `│`) is left as it
/// is, and so is a name qualified with a committee or Senate.
pub fn brief_names(text: &str) -> String {
    const OPEN: &str = "[senator::";
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if line.starts_with([' ', '\t', '│']) {
            out.push_str(line);
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find(OPEN) {
            let after = &rest[start + OPEN.len()..];
            let name = after.find(']').map(|end| &after[..end]);
            match name {
                Some(name)
                    if !name.is_empty()
                        && name.chars().all(|c| {
                            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '<' | '>')
                        }) =>
                {
                    out.push_str(&rest[..start]);
                    out.push_str(&brief_name(name));
                    rest = &after[name.len() + 1..];
                }
                _ => {
                    out.push_str(&rest[..start + OPEN.len()]);
                    rest = after;
                }
            }
        }
        out.push_str(rest);
    }
    out
}

/// A name that ends in a session's UUID keeps the start and the end of it.
pub(crate) fn brief_name(name: &str) -> Cow<'_, str> {
    const UUID: usize = 36;
    let Some(split) = name.len().checked_sub(UUID) else {
        return name.into();
    };
    let (prefix, id) = name.split_at(split);
    let is_uuid = id.char_indices().all(|(i, c)| match i {
        8 | 13 | 18 | 23 => c == '-',
        _ => c.is_ascii_hexdigit(),
    });
    if is_uuid && (prefix.is_empty() || prefix.ends_with('-')) {
        format!("{prefix}{}…{}", &id[..8], &id[UUID - 4..]).into()
    } else {
        name.into()
    }
}

pub(crate) fn printable(text: &str) -> Cow<'_, str> {
    use senate_core::worker::is_display_control;
    if !text.chars().any(is_display_control) {
        return text.into();
    }
    text.chars()
        .map(|ch| if is_display_control(ch) { ' ' } else { ch })
        .collect::<String>()
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_names_shorten_session_ids_and_leave_quoted_lines_alone() {
        let text = "[seq 3] [senator::claude-6d8b2e5d-4a60-486b-b3d8-21f2d82e4382] · to [senator::alice], [ledger::senator::codex]\n  ask [senator::alice] about it\n│ [senator::bob]\n";
        assert_eq!(
            brief_names(text),
            "[seq 3] claude-6d8b2e5d…4382 · to alice, [ledger::senator::codex]\n  ask [senator::alice] about it\n│ [senator::bob]\n"
        );
        assert_eq!(
            brief_names("[senator::] [senator::a b] [senator::x"),
            "[senator::] [senator::a b] [senator::x"
        );
        assert_eq!(
            brief_names("[senator::codex-<session-id>]"),
            "codex-<session-id>"
        );
    }

    #[test]
    fn fields_and_tables_align_by_display_width() {
        assert_eq!(
            fields(None, &[("委员会", "senate".into()), ("Log", "x".into())]),
            "委员会  senate\nLog     x\n"
        );
        assert_eq!(
            table(
                None,
                &["A", "Branch"],
                &[vec!["long-name".into(), "b".into()]]
            ),
            "A          Branch\nlong-name  b\n"
        );
    }
}
