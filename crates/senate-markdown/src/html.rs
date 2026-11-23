use std::fmt::Write as _;

use crate::highlight::Token;
use crate::render::Layout;
use crate::{Alignment, Block, Inline};

pub(crate) fn layout(block: &Block) -> Layout {
    let mut layout = Layout::default();
    write_block(block, &mut layout, false);
    layout
}

fn write_block(block: &Block, layout: &mut Layout, tight: bool) {
    let out = layout.text();
    match block {
        Block::Paragraph(content) => {
            if !tight {
                out.push_str("<p>");
            }
            inlines(content, out);
            if !tight {
                out.push_str("</p>");
            }
            out.push('\n');
        }
        Block::Heading { level, content } => {
            let level = (*level).clamp(1, 6);
            let _ = write!(out, "<h{level}>");
            inlines(content, out);
            let _ = writeln!(out, "</h{level}>");
        }
        Block::List {
            start,
            tight,
            items,
        } => {
            let tag = if start.is_some() { "ol" } else { "ul" };
            let _ = write!(out, "<{tag}");
            if let Some(start) = start.filter(|n| *n != 1) {
                let _ = write!(out, " start=\"{start}\"");
            }
            if items.iter().any(|item| item.checked.is_some()) {
                out.push_str(" class=\"task-list\"");
            }
            out.push_str(">\n");
            for item in items {
                if let Some(checked) = item.checked {
                    let label = if checked { "done" } else { "open" };
                    let marker = if checked { "☑" } else { "☐" };
                    let _ = write!(
                        layout.text(),
                        "<li class=\"task-item\"><span class=\"task-marker\" data-state=\"{label}\">{marker}</span> "
                    );
                } else {
                    layout.text().push_str("<li>");
                }
                for child in &item.blocks {
                    write_block(child, layout, *tight);
                }
                layout.text().push_str("</li>\n");
            }
            let _ = writeln!(layout.text(), "</{tag}>");
        }
        Block::Code(code) => {
            out.push_str("<pre><code");
            if let Some(language) = &code.language {
                out.push_str(" data-language=\"");
                escape(language, out);
                out.push('"');
            }
            out.push('>');
            layout.code(Default::default());
            layout.text().push_str("</code></pre>\n");
        }
        Block::Quote(blocks) => {
            out.push_str("<blockquote>\n");
            for block in blocks {
                write_block(block, layout, false);
            }
            layout.text().push_str("</blockquote>\n");
        }
        Block::Table {
            alignment,
            header,
            rows,
        } => {
            out.push_str("<table>\n<thead>\n");
            table_row(header, alignment, "th", out);
            out.push_str("</thead>\n<tbody>\n");
            for row in rows {
                table_row(row, alignment, "td", out);
            }
            out.push_str("</tbody>\n</table>\n");
        }
        Block::ThematicBreak => out.push_str("<hr>\n"),
    }
}

fn table_row(row: &[Vec<Inline>], alignment: &[Alignment], tag: &str, out: &mut String) {
    out.push_str("<tr>");
    for (i, content) in row.iter().enumerate() {
        let class = match alignment.get(i) {
            Some(Alignment::Left) => " class=\"align-left\"",
            Some(Alignment::Center) => " class=\"align-center\"",
            Some(Alignment::Right) => " class=\"align-right\"",
            _ => "",
        };
        let _ = write!(out, "<{tag}{class}>");
        inlines(content, out);
        let _ = write!(out, "</{tag}>");
    }
    out.push_str("</tr>\n");
}

fn inlines(items: &[Inline], out: &mut String) {
    for item in items {
        match item {
            Inline::Text(text) => escape(text, out),
            Inline::Code(text) => {
                out.push_str("<code>");
                escape(text, out);
                out.push_str("</code>");
            }
            Inline::Strong(content) => span("strong", content, out),
            Inline::Emphasis(content) => span("em", content, out),
            Inline::Strikethrough(content) => span("del", content, out),
            Inline::SoftBreak => out.push('\n'),
            Inline::HardBreak => out.push_str("<br>\n"),
            Inline::Link { url, content } if safe_url(url) => {
                out.push_str("<a href=\"");
                escape(url, out);
                out.push_str("\">");
                inlines(content, out);
                out.push_str("</a>");
            }
            Inline::Link { content, .. } => inlines(content, out),
        }
    }
}

fn span(tag: &str, content: &[Inline], out: &mut String) {
    let _ = write!(out, "<{tag}>");
    inlines(content, out);
    let _ = write!(out, "</{tag}>");
}

fn safe_url(url: &str) -> bool {
    let Some((scheme, path)) = url.split_once(':') else {
        return false;
    };
    !path.is_empty()
        && !url
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
        && (scheme.eq_ignore_ascii_case("https")
            || scheme.eq_ignore_ascii_case("http")
            || scheme.eq_ignore_ascii_case("mailto"))
}

pub(crate) fn escape(value: &str, out: &mut String) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            ch if ch.is_control() && !matches!(ch, '\n' | '\t') => out.push('�'),
            ch => out.push(ch),
        }
    }
}

pub(crate) fn code(spans: &[(Option<Token>, &str)]) -> String {
    let mut out = String::new();
    for &(token, value) in spans {
        if let Some(token) = token {
            let _ = write!(out, "<span class=\"{}\">", token.class());
        }
        escape(value, &mut out);
        if token.is_some() {
            out.push_str("</span>");
        }
    }
    out
}
