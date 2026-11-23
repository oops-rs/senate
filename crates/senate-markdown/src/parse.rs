use std::sync::Arc;

use comrak::nodes::{ListType, Node, NodeValue, Sourcepos, TableAlignment};
use comrak::{Arena, Options, parse_document};
use senate_core::Mention;

use crate::{Alignment, Block, BlockId, CodeBlock, Entry, Inline, ListItem};

pub(crate) fn document(text: &str) -> Vec<Entry> {
    let mut options = Options::default();
    options.extension.autolink = true;
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.tasklist = true;

    let arena = Arena::new();
    let root = parse_document(&arena, text, &options);
    let source = Source::new(text);
    root.children()
        .filter_map(|node| {
            let start = node.data().sourcepos.start;
            Some(Entry {
                id: BlockId {
                    line: start.line,
                    column: start.column,
                },
                block: Arc::new(source.block(node)?),
            })
        })
        .collect()
}

struct Source<'a> {
    text: &'a str,
    lines: Vec<usize>,
}

impl<'a> Source<'a> {
    fn new(text: &'a str) -> Self {
        let mut lines = vec![0];
        for (i, &byte) in text.as_bytes().iter().enumerate() {
            if byte == b'\n' || (byte == b'\r' && text.as_bytes().get(i + 1) != Some(&b'\n')) {
                lines.push(i + 1);
            }
        }
        Self { text, lines }
    }

    fn span(&self, position: Sourcepos) -> Option<&str> {
        let start = *self.lines.get(position.start.line.checked_sub(1)?)?;
        self.text
            .get((start + position.start.column.checked_sub(1)?)..self.end_byte(position)?)
    }

    fn end_byte(&self, position: Sourcepos) -> Option<usize> {
        self.lines
            .get(position.end.line.checked_sub(1)?)?
            .checked_add(position.end.column)
    }

    fn block(&self, node: Node<'_>) -> Option<Block> {
        let children = || {
            node.children()
                .filter_map(|child| self.block(child))
                .collect()
        };
        Some(match &node.data().value {
            NodeValue::Paragraph => Block::Paragraph(self.inlines(node)),
            NodeValue::Heading(heading) => Block::Heading {
                level: heading.level,
                content: self.inlines(node),
            },
            NodeValue::CodeBlock(code) => {
                let mut text = code.literal.clone();
                // Comrak adds a newline for parsing at EOF. Keep an unfinished
                // source line unfinished so the highlighter can revise it.
                if (!code.fenced || !code.closed)
                    && self.end_byte(node.data().sourcepos) == Some(self.text.len())
                    && !self.text.ends_with(['\r', '\n'])
                    && text.ends_with('\n')
                {
                    text.pop();
                }
                Block::Code(CodeBlock {
                    fenced: code.fenced,
                    language: code.info.split_whitespace().next().map(str::to_owned),
                    text,
                })
            }
            NodeValue::BlockQuote => Block::Quote(children()),
            NodeValue::List(list) => Block::List {
                start: (list.list_type == ListType::Ordered).then_some(list.start),
                tight: list.tight,
                items: node
                    .children()
                    .map(|item| {
                        let checked = match &item.data().value {
                            NodeValue::TaskItem(task) => Some(task.symbol.is_some()),
                            _ => None,
                        };
                        ListItem {
                            checked,
                            blocks: item
                                .children()
                                .filter_map(|child| self.block(child))
                                .collect(),
                        }
                    })
                    .collect(),
            },
            NodeValue::Table(table) => {
                let mut rows = node
                    .children()
                    .map(|row| row.children().map(|cell| self.inlines(cell)).collect());
                Block::Table {
                    alignment: table
                        .alignments
                        .iter()
                        .map(|align| match align {
                            TableAlignment::None => Alignment::Default,
                            TableAlignment::Left => Alignment::Left,
                            TableAlignment::Center => Alignment::Center,
                            TableAlignment::Right => Alignment::Right,
                        })
                        .collect(),
                    header: rows.next().unwrap_or_default(),
                    rows: rows.collect(),
                }
            }
            NodeValue::ThematicBreak(_) => Block::ThematicBreak,
            NodeValue::HtmlBlock(html) => {
                Block::Paragraph(vec![Inline::Text(html.literal.clone())])
            }
            _ => return None,
        })
    }

    fn inlines(&self, node: Node<'_>) -> Vec<Inline> {
        node.children()
            .filter_map(|child| self.inline(child))
            .collect()
    }

    fn inline(&self, node: Node<'_>) -> Option<Inline> {
        let data = node.data();
        Some(match &data.value {
            NodeValue::Text(text) => Inline::Text(text.to_string()),
            NodeValue::HtmlInline(html) => Inline::Text(html.clone()),
            NodeValue::Code(code) => Inline::Code(code.literal.clone()),
            NodeValue::SoftBreak => Inline::SoftBreak,
            NodeValue::LineBreak => Inline::HardBreak,
            NodeValue::Strong(_) => Inline::Strong(self.inlines(node)),
            NodeValue::Emph(_) => Inline::Emphasis(self.inlines(node)),
            NodeValue::Strikethrough(_) => Inline::Strikethrough(self.inlines(node)),
            NodeValue::Link(link) => {
                // A Senate label is prose even when a CommonMark reference
                // definition uses the same label. Keep the original bytes
                // so callers can still substitute their display names.
                if let Some(span) = self.span(data.sourcepos)
                    && span.starts_with('[')
                    && let Some(close) = span.find(']')
                    && span[..=close].parse::<Mention>().is_ok()
                {
                    return Some(Inline::Text(span.to_owned()));
                }
                Inline::Link {
                    url: link.url.clone(),
                    content: self.inlines(node),
                }
            }
            NodeValue::Image(_) => {
                // Keep image captions. Markdown cannot initiate an image load.
                let mut text = String::new();
                crate::terminal::plain_inlines(&self.inlines(node), &mut text);
                Inline::Text(text)
            }
            _ => return None,
        })
    }
}
