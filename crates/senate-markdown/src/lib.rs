//! A renderer-independent Markdown block tree and a cache for streamed updates.
//! See the crate README for the renderer contract and supported syntax.

mod highlight;
mod html;
mod parse;
mod render;
mod terminal;

pub use render::{BlockPatch, CodePatch, Format, Part, RenderCache, RenderDiff};
pub use terminal::{Color, Terminal};

use std::sync::Arc;

/// The 1-based source position of the start of a top-level block. An append
/// preserves existing IDs, including when a paragraph becomes a table or
/// heading. Edits that move a block to a new position can assign it a new ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: BlockId,
    /// UTF-8 bytes covered by the original source span.
    pub source_len: usize,
    pub block: Arc<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    List {
        start: Option<usize>,
        tight: bool,
        items: Vec<ListItem>,
    },
    Code(CodeBlock),
    Quote(Vec<Block>),
    Table {
        alignment: Vec<Alignment>,
        header: Vec<Vec<Inline>>,
        rows: Vec<Vec<Vec<Inline>>>,
    },
    ThematicBreak,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListItem {
    /// None for an ordinary item, Some(false) for an unchecked task.
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeBlock {
    pub fenced: bool,
    pub language: Option<String>,
    pub text: String,
}

impl CodeBlock {
    pub fn line_count(&self) -> usize {
        self.text.lines().count()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alignment {
    Default,
    Left,
    Center,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Inline {
    Text(String),
    Strong(Vec<Inline>),
    Emphasis(Vec<Inline>),
    Strikethrough(Vec<Inline>),
    Code(String),
    Link { url: String, content: Vec<Inline> },
    SoftBreak,
    HardBreak,
}

/// Sparse changes, in document order within each group. An insertion can
/// occur in the middle when an entire message is replaced. A replacement
/// keeps its old ID. A caller can retain all entries absent from the diff.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub unchanged: usize,
    pub replaced: Vec<BlockId>,
    pub appended: Vec<BlockId>,
    pub inserted: Vec<BlockId>,
    pub removed: Vec<BlockId>,
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Document {
    blocks: Vec<Entry>,
}

impl Document {
    pub fn parse(text: &str) -> Self {
        Self {
            blocks: parse::document(text),
        }
    }

    pub fn blocks(&self) -> &[Entry] {
        &self.blocks
    }

    pub(crate) fn entry(&self, id: BlockId) -> Option<&Entry> {
        let at = self
            .blocks
            .binary_search_by_key(&id, |entry| entry.id)
            .ok()?;
        self.blocks.get(at)
    }

    pub fn block(&self, id: BlockId) -> Option<&Block> {
        Some(&self.entry(id)?.block)
    }

    /// Reparse the message so constructs that refer across blocks can
    /// resolve even if their definition arrives after the original block.
    /// Equal blocks retain their allocation and require no layout work.
    pub fn update(&mut self, new_text: &str) -> Diff {
        let mut blocks = parse::document(new_text);
        let mut diff = Diff::default();
        let mut old = self.blocks.iter().peekable();
        let old_end = self.blocks.last().map(|entry| entry.id);
        for entry in &mut blocks {
            while old.peek().is_some_and(|before| before.id < entry.id) {
                if let Some(before) = old.next() {
                    diff.removed.push(before.id);
                }
            }
            if old.peek().is_some_and(|before| before.id == entry.id) {
                if let Some(before) = old.next() {
                    if before.block == entry.block {
                        entry.block = before.block.clone();
                        diff.unchanged += 1;
                    } else {
                        diff.replaced.push(entry.id);
                    }
                }
            } else if old_end.is_none_or(|end| entry.id > end) {
                diff.appended.push(entry.id);
            } else {
                diff.inserted.push(entry.id);
            }
        }
        diff.removed.extend(old.map(|entry| entry.id));
        self.blocks = blocks;
        diff
    }
}
