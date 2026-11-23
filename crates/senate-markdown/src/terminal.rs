use std::fmt::Write as _;
use std::io::{IsTerminal, Stdout};

use textwrap::core::display_width;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::highlight::Token;
use crate::render::Layout;
use crate::{Alignment, Block, Inline};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    None,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Terminal {
    width: usize,
    color: Color,
    utf8: bool,
}

impl Terminal {
    /// Pass explicit capabilities, for instance if rendering for a remote TTY.
    pub fn new(width: usize, color: Color) -> Self {
        Self {
            width: width.clamp(2, 4096),
            color,
            utf8: true,
        }
    }

    /// Choose marker glyphs for a remote terminal. Explicit capabilities
    /// default to UTF-8 markers. Message text always remains UTF-8.
    pub fn with_utf8(mut self, utf8: bool) -> Self {
        self.utf8 = utf8;
        self
    }

    pub fn stdout(width: Option<usize>) -> Self {
        let out = std::io::stdout();
        let width = width.unwrap_or_else(|| stdout_width(&out));
        let term = std::env::var("TERM").unwrap_or_default();
        let color = if !out.is_terminal()
            || term == "dumb"
            || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
        {
            Color::None
        } else if std::env::var("COLORTERM")
            .is_ok_and(|value| matches!(value.as_str(), "truecolor" | "24bit"))
        {
            Color::TrueColor
        } else if term.contains("256color") {
            Color::Ansi256
        } else {
            Color::Ansi16
        };
        let utf8 = ["LC_ALL", "LC_CTYPE", "LANG"]
            .into_iter()
            .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
            .is_some_and(|locale| {
                locale.split('@').next().is_some_and(|locale| {
                    let locale = locale.to_ascii_lowercase();
                    matches!(locale.rsplit('.').next(), Some("utf-8" | "utf8"))
                })
            });
        Self::new(width, color).with_utf8(utf8)
    }

    pub(crate) fn layout(self, block: &Block) -> Layout {
        let mut output = Layout::default();
        self.block(block, &Indent::default(), &mut output);
        output
    }

    fn block(self, block: &Block, indent: &Indent, layout: &mut Layout) {
        match block {
            Block::Paragraph(items) => self.line(&self.inline_string(items), indent, layout.text()),
            Block::Heading { level, content } => {
                let text = self.inline_string(content);
                if self.color == Color::None {
                    let count = usize::from((*level).clamp(1, 6));
                    let marker = format!("{} ", "#".repeat(count));
                    let hanging = " ".repeat(marker.len());
                    self.line(&text, &indent.extend(&marker, &hanging), layout.text());
                } else {
                    // Bold cyan matches the CLI's terminal emphasis style.
                    self.line(&self.style("1;36", &text), indent, layout.text());
                }
            }
            Block::Quote(blocks) => {
                for (index, block) in blocks.iter().enumerate() {
                    self.block(block, &indent.following(index).extend("│ ", "│ "), layout);
                }
            }
            Block::List { start, items, .. } => {
                for (index, item) in items.iter().enumerate() {
                    let marker = match (item.checked, self.utf8) {
                        (Some(true), true) => "☑ ".into(),
                        (Some(false), true) => "☐ ".into(),
                        (Some(true), false) => "[x] ".into(),
                        (Some(false), false) => "[ ] ".into(),
                        (None, _) => start.map_or_else(
                            || {
                                if self.utf8 {
                                    "• ".into()
                                } else {
                                    "- ".into()
                                }
                            },
                            |n| format!("{}. ", n.saturating_add(index)),
                        ),
                    };
                    let hanging = " ".repeat(display_width(&marker));
                    let item_indent = indent.following(index).extend(&marker, &hanging);
                    if item.blocks.is_empty() {
                        self.line("", &item_indent, layout.text());
                    }
                    for (part, block) in item.blocks.iter().enumerate() {
                        self.block(block, &item_indent.following(part), layout);
                    }
                }
            }
            Block::Code(_) => layout.code(indent.extend("  ", "  ")),
            Block::Table {
                alignment,
                header,
                rows,
            } => self.table(alignment, header, rows, indent, layout.text()),
            Block::ThematicBreak => {
                let width = self.width - display_width(self.indent(&indent.first));
                self.line(&"─".repeat(width), indent, layout.text());
            }
        }
    }

    fn style(self, style: &str, text: &str) -> String {
        if self.color == Color::None {
            return text.to_owned();
        }
        format!("\x1b[{style}m{text}\x1b[0m")
    }

    fn inline_string(self, items: &[Inline]) -> String {
        let mut out = String::new();
        inlines(self.color, items, &mut out);
        out
    }

    fn indent(self, text: &str) -> &str {
        // Leave room for a CJK character at any nesting depth.
        let max = self.width.saturating_sub(2);
        let mut width = 0;
        for (index, part) in text.grapheme_indices(true) {
            width += display_width(part);
            if width > max {
                return &text[..index];
            }
        }
        text
    }

    fn line(self, value: &str, indent: &Indent, out: &mut String) {
        let first = self.indent(&indent.first);
        let rest = self.indent(&indent.rest);
        let options = textwrap::Options::new(self.width)
            .initial_indent(first)
            .subsequent_indent(rest);
        for (index, line) in value.split('\n').enumerate() {
            let initial = if index == 0 { first } else { rest };
            if line.is_empty() {
                out.push_str(initial);
                out.push('\n');
                continue;
            }
            for wrapped in textwrap::wrap(line, options.clone().initial_indent(initial)) {
                out.push_str(&wrapped);
                out.push('\n');
            }
        }
    }

    fn table(
        self,
        alignment: &[Alignment],
        header: &[Vec<Inline>],
        rows: &[Vec<Vec<Inline>>],
        indent: &Indent,
        out: &mut String,
    ) {
        let cell = |inline: &[Inline]| {
            let mut value = String::new();
            plain_inlines(inline, &mut value);
            value.replace(['\r', '\n', '\t'], " ")
        };
        let all: Vec<Vec<String>> = std::iter::once(header)
            .chain(rows.iter().map(Vec::as_slice))
            .map(|row| row.iter().map(|items| cell(items)).collect())
            .collect();
        let count = header.len();
        if count == 0 {
            return;
        }

        let width = self.width
            - display_width(self.indent(&indent.first))
                .max(display_width(self.indent(&indent.rest)));
        let gutters = 3usize.saturating_mul(count).saturating_add(1);
        if width < gutters + count {
            // Narrow terminals keep all columns by stacking each row.
            for (row_index, row) in all.iter().enumerate().skip(1) {
                for (column, text) in row.iter().enumerate() {
                    let label = all
                        .first()
                        .and_then(|row| row.get(column))
                        .map_or("", String::as_str);
                    let indent = indent.following(row_index - 1 + column);
                    self.line(&format!("{label}: {text}"), &indent, out);
                }
                out.push('\n');
            }
            if rows.is_empty() {
                self.line(&all[0].join(" / "), indent, out);
            }
            return;
        }

        let mut sizes = vec![1; count];
        for row in &all {
            for (width, text) in sizes.iter_mut().zip(row) {
                *width = (*width).max(display_width(text).min(self.width));
            }
        }
        fit_columns(&mut sizes, width - gutters, self.width);
        for (row, texts) in all.iter().enumerate() {
            let prefix = self.indent(if row == 0 {
                &indent.first
            } else {
                &indent.rest
            });
            out.push_str(prefix);
            out.push('│');
            for (column, &width) in sizes.iter().enumerate() {
                let value = texts.get(column).map_or("", String::as_str);
                let value = truncate(value, width);
                let space = width.saturating_sub(display_width(&value));
                let left = match alignment.get(column) {
                    Some(Alignment::Right) => space,
                    Some(Alignment::Center) => space / 2,
                    _ => 0,
                };
                out.push(' ');
                out.push_str(&" ".repeat(left));
                if row == 0 {
                    out.push_str(&self.style("1;36", &value));
                } else {
                    out.push_str(&value);
                }
                out.push_str(&" ".repeat(space - left));
                out.push_str(" │");
            }
            out.push('\n');
            if row == 0 {
                out.push_str(self.indent(&indent.rest));
                out.push('├');
                for (column, width) in sizes.iter().enumerate() {
                    out.push_str(&"─".repeat(width + 2));
                    out.push(if column + 1 == count { '┤' } else { '┼' });
                }
                out.push('\n');
            }
        }
    }

    pub(crate) fn code(
        self,
        scopes: &[(Option<Token>, &str)],
        indent: &Indent,
        first: bool,
    ) -> String {
        let initial = self.indent(if first { &indent.first } else { &indent.rest });
        let rest = self.indent(&indent.rest);
        let mut out = initial.to_owned();
        let mut col = display_width(initial);
        for &(token, text) in scopes {
            let color = self.token_color(token);
            out.push_str(color);
            for cluster in text.trim_end_matches('\n').graphemes(true) {
                let plain;
                let cluster = if cluster == "\t" {
                    plain = " ".repeat((4 - col % 4).min(self.width - display_width(rest)));
                    plain.as_str()
                } else if cluster.chars().any(char::is_control) {
                    "�"
                } else {
                    cluster
                };
                let width = display_width(cluster);
                if col + width > self.width {
                    if !color.is_empty() {
                        out.push_str("\x1b[0m");
                    }
                    out.push('\n');
                    out.push_str(rest);
                    out.push_str(color);
                    col = display_width(rest);
                }
                out.push_str(cluster);
                col += width;
            }
            if !color.is_empty() {
                out.push_str("\x1b[0m");
            }
        }
        out.push('\n');
        out
    }

    fn token_color(self, token: Option<Token>) -> &'static str {
        let Some(token) = token else { return "" };
        ansi(self.color, token)
    }
}

fn ansi(color: Color, token: Token) -> &'static str {
    use Token::*;
    let n = match token {
        Comment => 0,
        String => 1,
        Keyword => 2,
        Number => 3,
        Name => 4,
        Type => 5,
        Operator => 6,
    };
    match color {
        Color::None => "",
        Color::Ansi16 => [
            "\x1b[90m", "\x1b[32m", "\x1b[35m", "\x1b[33m", "\x1b[34m", "\x1b[36m", "\x1b[31m",
        ][n],
        Color::Ansi256 => [
            "\x1b[38;5;244m",
            "\x1b[38;5;114m",
            "\x1b[38;5;176m",
            "\x1b[38;5;179m",
            "\x1b[38;5;75m",
            "\x1b[38;5;80m",
            "\x1b[38;5;167m",
        ][n],
        Color::TrueColor => [
            "\x1b[38;2;137;147;157m",
            "\x1b[38;2;126;185;112m",
            "\x1b[38;2;198;120;221m",
            "\x1b[38;2;229;192;123m",
            "\x1b[38;2;97;175;239m",
            "\x1b[38;2;86;182;194m",
            "\x1b[38;2;224;108;117m",
        ][n],
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Indent {
    first: String,
    rest: String,
}

impl Indent {
    fn following(&self, index: usize) -> Self {
        Self {
            first: if index == 0 {
                self.first.clone()
            } else {
                self.rest.clone()
            },
            rest: self.rest.clone(),
        }
    }

    fn extend(&self, first: &str, rest: &str) -> Self {
        Self {
            first: format!("{}{first}", self.first),
            rest: format!("{}{rest}", self.rest),
        }
    }
}

fn fit_columns(widths: &mut [usize], budget: usize, max: usize) {
    if widths.iter().sum::<usize>() <= budget {
        return;
    }
    // Find the largest shared width cap that fits. A cap lets short
    // columns keep their widths without shrinking long columns one
    // cell at a time.
    let capped_total = |cap| widths.iter().map(|&width| width.min(cap)).sum::<usize>();
    let (mut low, mut high) = (1, max);
    while low + 1 < high {
        let mid = (low + high) / 2;
        if capped_total(mid) <= budget {
            low = mid;
        } else {
            high = mid;
        }
    }
    let mut extra = budget - capped_total(low);
    for width in widths {
        if *width > low {
            *width = low + usize::from(extra > 0);
            extra = extra.saturating_sub(1);
        }
    }
}

fn truncate(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for cluster in text.graphemes(true) {
        used += display_width(cluster);
        if used >= width {
            break;
        }
        out.push_str(cluster);
    }
    out.push('…');
    out
}

fn stdout_width(out: &Stdout) -> usize {
    terminal_size::terminal_size_of(out)
        .map(|(width, _)| width.0)
        .or_else(|| std::env::var("COLUMNS").ok()?.parse::<u16>().ok())
        .filter(|width| *width > 0)
        .map_or(80, usize::from)
}

fn printable(text: &str, out: &mut String) {
    for ch in text.chars() {
        out.push(match ch {
            '\t' => ' ',
            ch if ch.is_control() && ch != '\n' => '�',
            ch => ch,
        });
    }
}

pub(crate) fn plain_inlines(items: &[Inline], out: &mut String) {
    inlines(Color::None, items, out);
}

fn inlines(color: Color, items: &[Inline], out: &mut String) {
    for item in items {
        match item {
            Inline::Text(text) => printable(text, out),
            Inline::SoftBreak => out.push(' '),
            Inline::HardBreak => out.push('\n'),
            Inline::Strong(children) => inline_style(color, "1", "22", children, out),
            Inline::Emphasis(children) => inline_style(color, "3", "23", children, out),
            Inline::Strikethrough(children) => inline_style(color, "9", "29", children, out),
            Inline::Code(text) => {
                if color != Color::None {
                    out.push_str("\x1b[36m");
                }
                printable(text, out);
                if color != Color::None {
                    out.push_str("\x1b[39m");
                }
            }
            Inline::Link { url, content } => {
                let mut label = String::new();
                plain_inlines(content, &mut label);
                inline_style(color, "4", "24", content, out);
                if url != &label && url.strip_prefix("mailto:") != Some(label.as_str()) {
                    out.push_str(" (");
                    printable(url, out);
                    out.push(')');
                }
            }
        }
    }
}

fn inline_style(color: Color, begin: &str, end: &str, items: &[Inline], out: &mut String) {
    if color != Color::None {
        let _ = write!(out, "\x1b[{begin}m");
    }
    inlines(color, items, out);
    if color != Color::None {
        let _ = write!(out, "\x1b[{end}m");
    }
}
