use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use crate::highlight::{Lexer, Token, syntax};
use crate::terminal::{Indent, Terminal};
use crate::{Block, BlockId, CodeBlock, Diff, Document, html};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Html,
    Terminal,
}

impl Format {
    fn index(self) -> usize {
        match self {
            Self::Html => 0,
            Self::Terminal => 1,
        }
    }
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
    pub blocks: Vec<BlockPatch>,
    /// Source code lines formatted for the requested output.
    pub rendered_code_lines: usize,
    /// Source lines successfully processed by the syntax highlighter.
    pub highlighted_lines: usize,
}

impl RenderDiff {
    fn extend(&mut self, other: Self) {
        self.blocks.extend(other.blocks);
        self.rendered_code_lines += other.rendered_code_lines;
        self.highlighted_lines += other.highlighted_lines;
    }
}

/// A lazy cache for one document and one terminal configuration. A caller
/// can schedule requests in any order. No entry exists before its first
/// request. HTML and terminal share code highlighting but format their
/// requested blocks independently.
pub struct RenderCache {
    terminal: Terminal,
    max_highlight_bytes: usize,
    blocks: HashMap<BlockId, CachedBlock>,
}

impl RenderCache {
    pub fn new(terminal: Terminal) -> Self {
        Self::with_highlight_limit(terminal, usize::MAX)
    }

    /// Render a code block as plain text when it exceeds this byte budget.
    /// Zero disables highlighting. To defer all highlighting, use a zero
    /// budget first and create a highlighting cache when the view is ready.
    pub fn with_highlight_limit(terminal: Terminal, max_bytes: usize) -> Self {
        Self {
            terminal,
            max_highlight_bytes: max_bytes,
            blocks: HashMap::new(),
        }
    }

    /// Drop removed blocks and mark previously requested replacements
    /// stale. Existing lexer checkpoints survive until the next request.
    pub fn invalidate(&mut self, diff: &Diff) {
        for id in &diff.removed {
            self.blocks.remove(id);
        }
        for id in &diff.replaced {
            if let Some(block) = self.blocks.get_mut(id) {
                block
                    .layouts
                    .iter_mut()
                    .for_each(|layout| layout.ready = false);
            }
        }
    }

    /// Render only this block for the requested format. Missing IDs and
    /// already-cached output do no work.
    pub fn render_block(&mut self, document: &Document, id: BlockId, format: Format) -> RenderDiff {
        let Some(entry) = document.entry(id) else {
            return RenderDiff::default();
        };
        let mut work = RenderDiff::default();
        let cache = self
            .blocks
            .entry(id)
            .or_insert_with(|| CachedBlock::new(entry.block.clone()));
        if !Arc::ptr_eq(&cache.source, &entry.block) {
            cache.source = entry.block.clone();
            cache
                .layouts
                .iter_mut()
                .for_each(|layout| layout.ready = false);
        }
        let slot = &mut cache.layouts[format.index()];
        if slot.ready {
            return work;
        }

        let layout = match format {
            Format::Html => html::layout(&entry.block),
            Format::Terminal => self.terminal.layout(&entry.block),
        };
        let mut code_blocks = Vec::new();
        code_in(&entry.block, &mut code_blocks);
        debug_assert_eq!(code_blocks.len(), layout.indents.len());
        cache.code.truncate(code_blocks.len());
        cache
            .code
            .resize_with(code_blocks.len(), CodeCache::default);

        let layout_changed = slot.layout.as_ref() != Some(&layout);
        let mut patch = BlockPatch {
            id,
            layout_changed,
            code: Vec::new(),
        };
        for (index, (code, indent)) in code_blocks.into_iter().zip(&layout.indents).enumerate() {
            let cache = &mut cache.code[index];
            let highlight =
                self.max_highlight_bytes > 0 && code.text.len() <= self.max_highlight_bytes;
            cache.refresh(code, highlight, &mut work);
            if let Some(from_line) = cache.format(format, indent, self.terminal, &mut work) {
                patch.code.push(CodePatch { index, from_line });
            }
        }
        if layout_changed {
            slot.layout = Some(layout);
        }
        slot.ready = true;
        if layout_changed || !patch.code.is_empty() {
            work.blocks.push(patch);
        }
        work
    }

    /// Render and concatenate each block in document order. Use the
    /// per-block API when a view can defer offscreen content.
    pub fn render_document(
        &mut self,
        document: &Document,
        format: Format,
        out: &mut String,
    ) -> RenderDiff {
        let mut work = RenderDiff::default();
        for (index, entry) in document.blocks().iter().enumerate() {
            work.extend(self.render_block(document, entry.id, format));
            if index > 0 && format == Format::Terminal {
                out.push('\n');
            }
            self.write_block(entry.id, format, out);
        }
        work
    }

    /// Access one ready block without joining its code lines. Stale and
    /// unrequested output is absent.
    pub fn parts(&self, id: BlockId, format: Format) -> Option<impl Iterator<Item = Part<'_>>> {
        let cached = self.blocks.get(&id)?;
        let slot = &cached.layouts[format.index()];
        let layout = slot.ready.then_some(slot.layout.as_ref()).flatten()?;
        Some(layout.pieces.iter().map(move |piece| match piece {
            Piece::Text(text) => Part::Text(text),
            Piece::Code(index) => Part::Code {
                index: *index,
                lines: cached.code[*index].lines(format),
            },
        }))
    }

    pub fn code_lines(&self, id: BlockId, index: usize, format: Format) -> Option<&[String]> {
        let block = self.blocks.get(&id)?;
        block.layouts[format.index()]
            .ready
            .then_some(block.code.get(index)?.lines(format))
    }

    /// Append an already rendered block. Returns false if it is not ready.
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
}

struct CachedBlock {
    source: Arc<Block>,
    layouts: [OutputLayout; 2],
    code: Vec<CodeCache>,
}

impl CachedBlock {
    fn new(source: Arc<Block>) -> Self {
        Self {
            source,
            layouts: Default::default(),
            code: Vec::new(),
        }
    }
}

#[derive(Default)]
struct OutputLayout {
    ready: bool,
    layout: Option<Layout>,
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
    highlight: bool,
    language: Option<String>,
    source: String,
    spans: Vec<Vec<(Option<Token>, Range<usize>)>>,
    lexer: Option<Lexer>,
    before_last: Option<Lexer>,
    last_offset: usize,
    rendered: [CodeOutput; 2],
}

#[derive(Default)]
struct CodeOutput {
    indent: Indent,
    lines: Vec<String>,
    valid: usize,
}

impl CodeCache {
    fn lines(&self, format: Format) -> &[String] {
        &self.rendered[format.index()].lines
    }

    fn refresh(&mut self, code: &CodeBlock, highlight: bool, stats: &mut RenderDiff) {
        let same_context = self.highlight == highlight && self.language == code.language;
        if same_context && self.source == code.text {
            return;
        }
        let (offset, from_line) =
            if same_context && self.source.ends_with('\n') && code.text.starts_with(&self.source) {
                (self.source.len(), self.spans.len())
            } else if same_context
                && self.last_offset > 0
                && code.text.starts_with(&self.source[..self.last_offset])
            {
                self.lexer = self.before_last.take();
                (self.last_offset, self.spans.len() - 1)
            } else {
                self.language.clone_from(&code.language);
                self.highlight = highlight;
                self.lexer = if highlight {
                    syntax(code.language.as_deref()).map(Lexer::new)
                } else {
                    None
                };
                (0, 0)
            };

        self.spans.truncate(from_line);
        self.rendered
            .iter_mut()
            .for_each(|output| output.valid = output.valid.min(from_line));
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
            let mut position = at;
            self.spans.push(
                spans
                    .as_deref()
                    .unwrap_or(&plain)
                    .iter()
                    .map(|&(token, value)| {
                        let end = position + value.len();
                        let range = position..end;
                        position = end;
                        (token, range)
                    })
                    .collect(),
            );
            at += line.len();
        }
        self.source.push_str(&code.text[offset..]);
        // A line turning into a closing fence may remove a suffix.
        if at == offset {
            self.last_offset = 0;
            self.before_last = None;
        }
    }

    fn format(
        &mut self,
        format: Format,
        indent: &Indent,
        terminal: Terminal,
        stats: &mut RenderDiff,
    ) -> Option<usize> {
        let output = &mut self.rendered[format.index()];
        if &output.indent != indent {
            output.indent.clone_from(indent);
            output.valid = 0;
        }
        let from = output.valid;
        if from == self.spans.len() && from == output.lines.len() {
            return None;
        }
        output.lines.truncate(from);
        let mut spans = Vec::new();
        for line in &self.spans[from..] {
            spans.clear();
            spans.extend(
                line.iter()
                    .map(|(token, range)| (*token, &self.source[range.clone()])),
            );
            output.lines.push(match format {
                Format::Html => html::code(&spans),
                Format::Terminal => terminal.code(&spans, indent, output.lines.is_empty()),
            });
            stats.rendered_code_lines += 1;
        }
        output.valid = self.spans.len();
        Some(from)
    }
}
