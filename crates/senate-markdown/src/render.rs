use std::collections::HashMap;

use crate::highlight::{Lexer, syntax};
use crate::terminal::{Indent, Terminal};
use crate::{Block, BlockId, CodeBlock, Diff, Document, html};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Html,
    Terminal,
}

/// A rendered top-level block is markup/text interleaved with independently
/// replaceable code bodies. Code indices are local to that block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part<'a> {
    Text(&'a str),
    Code { index: usize, lines: &'a [String] },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodePatch {
    pub index: usize,
    /// Keep every line before this index, then use the cache's new suffix.
    pub from_line: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BlockPatch {
    pub id: BlockId,
    /// Insert the block or replace its wrappers and prose. When false, use
    /// only the code patches, keeping the existing block and line prefix.
    pub layout_changed: bool,
    pub code: Vec<CodePatch>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RenderDiff {
    pub removed: Vec<BlockId>,
    pub blocks: Vec<BlockPatch>,
    /// Every source code line rendered by both renderers during this update.
    pub rendered_code_lines: usize,
    /// Source lines successfully processed by the syntax highlighter.
    pub highlighted_lines: usize,
}

/// Keep one cache per streamed document and terminal configuration. Calls
/// to apply must use consecutive diffs from that document.
pub struct RenderCache {
    terminal: Terminal,
    blocks: HashMap<BlockId, CachedBlock>,
}

impl RenderCache {
    pub fn new(terminal: Terminal) -> Self {
        Self {
            terminal,
            blocks: HashMap::new(),
        }
    }

    /// Render a document from scratch. Report the work and initial blocks
    /// with the same shape as subsequent updates.
    pub fn reset(&mut self, document: &Document) -> RenderDiff {
        self.blocks.clear();
        let mut changes = RenderDiff::default();
        for entry in document.blocks() {
            self.refresh(entry.id, &entry.block, &mut changes);
        }
        changes
    }

    pub fn apply(&mut self, document: &Document, diff: &Diff) -> RenderDiff {
        let mut changes = RenderDiff {
            removed: diff.removed.clone(),
            ..RenderDiff::default()
        };
        for id in &diff.removed {
            self.blocks.remove(id);
        }
        for &id in diff
            .replaced
            .iter()
            .chain(&diff.inserted)
            .chain(&diff.appended)
        {
            if let Some(block) = document.block(id) {
                self.refresh(id, block, &mut changes);
            }
        }
        changes
    }

    /// Access a single block without joining any of its code lines.
    pub fn parts(&self, id: BlockId, format: Format) -> Option<impl Iterator<Item = Part<'_>>> {
        let cached = self.blocks.get(&id)?;
        let layout = match format {
            Format::Html => &cached.html,
            Format::Terminal => &cached.terminal,
        };
        Some(layout.pieces.iter().map(move |piece| match piece {
            Piece::Text(text) => Part::Text(text),
            Piece::Code(index) => Part::Code {
                index: *index,
                lines: cached.code[*index].lines(format),
            },
        }))
    }

    pub fn code_lines(&self, id: BlockId, index: usize, format: Format) -> Option<&[String]> {
        Some(self.blocks.get(&id)?.code.get(index)?.lines(format))
    }

    /// Convenience export for a client that wants one complete HTML element
    /// or a plain/ANSI terminal block. Streaming clients can use the parts
    /// and patches without allocating this concatenation.
    pub fn write_block(&self, id: BlockId, format: Format, out: &mut String) -> bool {
        let Some(parts) = self.parts(id, format) else {
            return false;
        };
        for part in parts {
            match part {
                Part::Text(text) => out.push_str(text),
                Part::Code { lines, .. } => lines.iter().for_each(|line| out.push_str(line)),
            }
        }
        true
    }

    fn refresh(&mut self, id: BlockId, block: &Block, changes: &mut RenderDiff) {
        let html = html::layout(block);
        let terminal = self.terminal.layout(block);
        let mut code_blocks = Vec::new();
        code_in(block, &mut code_blocks);
        debug_assert_eq!(code_blocks.len(), html.indents.len());
        debug_assert_eq!(code_blocks.len(), terminal.indents.len());

        let existing = self.blocks.get_mut(&id);
        let layout_changed = existing
            .as_ref()
            .is_none_or(|old| old.html != html || old.terminal != terminal);
        let cached = match existing {
            Some(existing) => existing,
            None => self.blocks.entry(id).or_default(),
        };
        let mut patch = BlockPatch {
            id,
            layout_changed,
            code: Vec::new(),
        };
        cached.code.truncate(code_blocks.len());
        cached
            .code
            .resize_with(code_blocks.len(), CodeCache::default);
        for (index, (code, indent)) in code_blocks.into_iter().zip(&terminal.indents).enumerate() {
            if let Some(from_line) =
                cached.code[index].refresh(code, indent, self.terminal, changes)
            {
                patch.code.push(CodePatch { index, from_line });
            }
        }
        if layout_changed {
            cached.html = html;
            cached.terminal = terminal;
        }
        if layout_changed || !patch.code.is_empty() {
            changes.blocks.push(patch);
        }
    }
}

#[derive(Default)]
struct CachedBlock {
    html: Layout,
    terminal: Layout,
    code: Vec<CodeCache>,
}

#[derive(Default, PartialEq, Eq)]
pub(crate) struct Layout {
    pieces: Vec<Piece>,
    indents: Vec<Indent>,
}

#[derive(PartialEq, Eq)]
enum Piece {
    Text(String),
    Code(usize),
}

impl Layout {
    pub(crate) fn text(&mut self) -> &mut String {
        if !matches!(self.pieces.last(), Some(Piece::Text(_))) {
            self.pieces.push(Piece::Text(String::new()));
        }
        let Some(Piece::Text(text)) = self.pieces.last_mut() else {
            unreachable!("a text fragment was just inserted");
        };
        text
    }

    pub(crate) fn code(&mut self, indent: Indent) {
        self.pieces.push(Piece::Code(self.indents.len()));
        self.indents.push(indent);
    }
}

fn code_in<'a>(block: &'a Block, out: &mut Vec<&'a CodeBlock>) {
    match block {
        Block::Code(code) => out.push(code),
        Block::Quote(blocks) => blocks.iter().for_each(|child| code_in(child, out)),
        Block::List { items, .. } => {
            for item in items {
                item.blocks.iter().for_each(|child| code_in(child, out));
            }
        }
        _ => {}
    }
}

#[derive(Default)]
struct CodeCache {
    language: Option<String>,
    indent: Indent,
    source: String,
    html: Vec<String>,
    terminal: Vec<String>,
    lexer: Option<Lexer>,
    before_last: Option<Lexer>,
    last_offset: usize,
}

impl CodeCache {
    fn lines(&self, format: Format) -> &[String] {
        match format {
            Format::Html => &self.html,
            Format::Terminal => &self.terminal,
        }
    }

    fn refresh(
        &mut self,
        code: &CodeBlock,
        indent: &Indent,
        terminal: Terminal,
        stats: &mut RenderDiff,
    ) -> Option<usize> {
        let same_context = self.language == code.language && &self.indent == indent;
        if same_context && self.source == code.text {
            return None;
        }
        let (offset, from_line) =
            if same_context && self.source.ends_with('\n') && code.text.starts_with(&self.source) {
                (self.source.len(), self.html.len())
            } else if same_context
                && self.last_offset > 0
                && code.text.starts_with(&self.source[..self.last_offset])
            {
                self.lexer = self.before_last.take();
                (self.last_offset, self.html.len() - 1)
            } else {
                self.language.clone_from(&code.language);
                self.indent.clone_from(indent);
                self.lexer = syntax(code.language.as_deref()).map(Lexer::new);
                (0, 0)
            };

        self.html.truncate(from_line);
        self.terminal.truncate(from_line);
        self.source.truncate(offset);
        let mut at = offset;
        for line in code.text[offset..].split_inclusive('\n') {
            self.last_offset = at;
            self.before_last.clone_from(&self.lexer);
            let spans = self.lexer.as_mut().and_then(|lexer| lexer.line(line));
            if spans.is_some() {
                stats.highlighted_lines += 1;
            }
            let plain = [(None, line)];
            let spans = spans.as_deref().unwrap_or(&plain);
            self.html.push(html::code(spans));
            self.terminal
                .push(terminal.code(spans, indent, self.terminal.is_empty()));
            stats.rendered_code_lines += 1;
            at += line.len();
        }
        self.source.push_str(&code.text[offset..]);
        // A suffix can disappear, such as a line turning into a closing
        // fence. The old final checkpoint is no longer a valid checkpoint
        // for a future rewrite in that situation.
        if at == offset {
            self.last_offset = 0;
            self.before_last = None;
        }
        Some(from_line)
    }
}
