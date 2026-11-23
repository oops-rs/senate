//! SPIKE: messages' Markdown for the app, parsed and rendered by
//! `senate-markdown`. The core keeps one document and one render cache per
//! message (keyed by the app's message identity); the app passes text and
//! gets block ids, the diff of an update, and a block's HTML when it asks.
//! Nothing is parsed on the Swift or the page side.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use senate_markdown::{Block, BlockId, Color, Document, Format, Part, RenderCache, Terminal};

/// Code blocks larger than this render as plain text.
const HIGHLIGHT_LIMIT: usize = 64 * 1024;

/// A top-level block as the page needs it before rendering: enough to hold
/// a placeholder of about the right size.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MdBlock {
    /// `line:column` of the block's start in the message.
    pub id: String,
    /// paragraph, heading, list, code, quote, table or rule.
    pub kind: String,
    pub source_len: u32,
    /// Lines of code in it (code blocks, also nested in lists and quotes),
    /// rows of a table (with its header), items of a list.
    pub lines: u32,
    pub language: Option<String>,
}

/// What an update changed. `blocks` is the whole message, in order.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MdUpdate {
    pub blocks: Vec<MdBlock>,
    pub unchanged: u32,
    pub replaced: Vec<String>,
    pub appended: Vec<String>,
    pub inserted: Vec<String>,
    pub removed: Vec<String>,
    pub micros: u64,
}

/// New code lines for one code body of a block, from `from_line` on.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MdCodePatch {
    pub index: u32,
    pub from_line: u32,
    pub lines: Vec<String>,
}

/// One block rendered: its whole HTML (when asked for in full, or when its
/// structure changed), else only code lines from where they changed; both
/// empty when nothing changed since the last render.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MdRendered {
    pub id: String,
    pub html: Option<String>,
    pub code: Vec<MdCodePatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MdRender {
    pub blocks: Vec<MdRendered>,
    pub highlighted_lines: u32,
    pub code_lines: u32,
    pub micros: u64,
}

struct Message {
    text: String,
    document: Document,
    cache: RenderCache,
}

/// Messages' documents and render caches, by message key. Each message
/// has its own lock: rendering one (off the main thread) never holds up
/// an update to another (on it).
#[derive(uniffi::Object)]
pub struct MarkdownStore {
    messages: Mutex<HashMap<String, Arc<Mutex<Message>>>>,
}

impl MarkdownStore {
    fn message(&self, key: &str) -> Option<Arc<Mutex<Message>>> {
        let messages = self.messages.lock().unwrap_or_else(|e| e.into_inner());
        messages.get(key).cloned()
    }
}

#[uniffi::export]
impl MarkdownStore {
    #[uniffi::constructor]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            messages: Mutex::new(HashMap::new()),
        })
    }

    /// The message `key` now reads `text`: parse it (a new message) or
    /// reparse and diff it against what it read before.
    pub fn update(&self, key: String, text: String) -> MdUpdate {
        let started = Instant::now();
        let shared = match self.message(&key) {
            Some(m) => m,
            None => {
                let m = Arc::new(Mutex::new(Message {
                    text: String::new(),
                    document: Document::default(),
                    cache: RenderCache::with_highlight_limit(
                        Terminal::new(80, Color::None),
                        HIGHLIGHT_LIMIT,
                    ),
                }));
                let mut messages = self.messages.lock().unwrap_or_else(|e| e.into_inner());
                messages.entry(key).or_insert(m).clone()
            }
        };
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        let m = &mut *guard;
        let diff = if m.text == text && !m.document.blocks().is_empty() {
            None
        } else {
            let diff = m.document.update(&text);
            m.cache.invalidate(&diff);
            m.text = text;
            Some(diff)
        };
        let document = &m.document;
        let diff = diff.unwrap_or_else(|| senate_markdown::Diff {
            unchanged: document.blocks().len(),
            ..Default::default()
        });
        let ids = |v: &[BlockId]| v.iter().map(|id| id_text(*id)).collect();
        MdUpdate {
            blocks: document
                .blocks()
                .iter()
                .map(|e| describe(e.id, e.source_len, &e.block))
                .collect(),
            unchanged: diff.unchanged as u32,
            replaced: ids(&diff.replaced),
            appended: ids(&diff.appended),
            inserted: ids(&diff.inserted),
            removed: ids(&diff.removed),
            micros: started.elapsed().as_micros() as u64,
        }
    }

    /// Render these blocks of `key` to HTML. `full`: the page has nothing
    /// of them (whole HTML); else only what changed since the last render.
    pub fn render(&self, key: String, ids: Vec<String>, full: bool) -> MdRender {
        let started = Instant::now();
        let mut out = MdRender {
            blocks: Vec::new(),
            highlighted_lines: 0,
            code_lines: 0,
            micros: 0,
        };
        let Some(shared) = self.message(&key) else {
            return out;
        };
        let mut guard = shared.lock().unwrap_or_else(|e| e.into_inner());
        let m = &mut *guard;
        for text in ids {
            let Some(id) = parse_id(&text) else { continue };
            if m.document.block(id).is_none() {
                continue;
            }
            let work = m.cache.render_block(&m.document, id, Format::Html);
            out.highlighted_lines += work.highlighted_lines as u32;
            out.code_lines += work.rendered_code_lines as u32;
            let patch = work.blocks.into_iter().find(|p| p.id == id);
            let whole = full || patch.as_ref().is_some_and(|p| p.layout_changed);
            let mut rendered = MdRendered {
                id: text,
                html: None,
                code: Vec::new(),
            };
            if whole {
                rendered.html = Some(html(&m.cache, id));
            } else if let Some(patch) = patch {
                for c in patch.code {
                    let lines = m
                        .cache
                        .code_lines(id, c.index, Format::Html)
                        .unwrap_or_default();
                    rendered.code.push(MdCodePatch {
                        index: c.index as u32,
                        from_line: c.from_line as u32,
                        lines: lines[c.from_line.min(lines.len())..]
                            .iter()
                            .map(|l| line(l))
                            .collect(),
                    });
                }
            }
            out.blocks.push(rendered);
        }
        out.micros = started.elapsed().as_micros() as u64;
        out
    }

    /// Load the highlighter's syntaxes now (they load on first use, which
    /// takes a while): call once, off the main thread, at launch.
    pub fn warm(&self) {
        let d = Document::parse(concat!(
            "```rust\nfn main() {}\n```\n```swift\nlet a = 1\n```\n```python\nx = 1\n```\n",
            "```typescript\nconst a = 1;\n```\n```bash\necho a\n```\n```json\n{}\n```\n"
        ));
        let mut c = RenderCache::new(Terminal::new(80, Color::None));
        let mut out = String::new();
        c.render_document(&d, Format::Html, &mut out);
    }

    /// Let go of these messages (a session no longer shown).
    pub fn forget(&self, keys: Vec<String>) {
        let mut messages = self.messages.lock().unwrap_or_else(|e| e.into_inner());
        for k in keys {
            messages.remove(&k);
        }
    }
}

/// A rendered block, each code line in its own span so the page can
/// replace a code body's tail.
fn html(cache: &RenderCache, id: BlockId) -> String {
    let mut out = String::new();
    if let Some(parts) = cache.parts(id, Format::Html) {
        for part in parts {
            match part {
                Part::Text(t) => out.push_str(t),
                Part::Code { lines, .. } => lines.iter().for_each(|l| out.push_str(&line(l))),
            }
        }
    }
    out
}

fn line(l: &str) -> String {
    format!("<span class=\"ln\">{l}</span>")
}

fn id_text(id: BlockId) -> String {
    format!("{}:{}", id.line, id.column)
}

fn parse_id(text: &str) -> Option<BlockId> {
    let (line, column) = text.split_once(':')?;
    Some(BlockId {
        line: line.parse().ok()?,
        column: column.parse().ok()?,
    })
}

fn describe(id: BlockId, source_len: usize, block: &Block) -> MdBlock {
    let (kind, lines, language) = match block {
        Block::Paragraph(_) => ("paragraph", 0, None),
        Block::Heading { .. } => ("heading", 0, None),
        Block::List { items, .. } => ("list", items.len() + code_lines(block), None),
        Block::Code(c) => ("code", c.line_count(), c.language.clone()),
        Block::Quote(_) => ("quote", code_lines(block), None),
        Block::Table { rows, .. } => ("table", rows.len() + 1, None),
        Block::ThematicBreak => ("rule", 0, None),
        _ => ("other", 0, None),
    };
    MdBlock {
        id: id_text(id),
        kind: kind.into(),
        source_len: source_len as u32,
        lines: lines as u32,
        language,
    }
}

fn code_lines(block: &Block) -> usize {
    match block {
        Block::Code(c) => c.line_count(),
        Block::Quote(blocks) => blocks.iter().map(code_lines).sum(),
        Block::List { items, .. } => items
            .iter()
            .flat_map(|i| i.blocks.iter())
            .map(code_lines)
            .sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_a_code_block_patches_only_its_tail() {
        let s = MarkdownStore::new();
        let first = s.update("m".into(), "Hi\n\n```rust\nlet a = 1;\n".into());
        assert_eq!(first.appended.len(), 2);
        let code = first.blocks[1].id.clone();
        let r = s.render("m".into(), vec![code.clone()], true);
        assert!(r.blocks[0].html.as_deref().unwrap().contains("class=\"ln\""));
        let u = s.update("m".into(), "Hi\n\n```rust\nlet a = 1;\nlet b = 2;\n".into());
        assert_eq!(u.replaced, vec![code.clone()]);
        assert!(u.appended.is_empty());
        let r = s.render("m".into(), vec![code], false);
        let b = &r.blocks[0];
        assert!(b.html.is_none(), "only the code tail");
        assert_eq!(b.code[0].from_line, 1);
        assert_eq!(b.code[0].lines.len(), 1);
        assert_eq!(r.highlighted_lines, 1);
    }

    #[test]
    fn the_same_text_changes_nothing() {
        let s = MarkdownStore::new();
        s.update("m".into(), "# A\n\nb".into());
        let u = s.update("m".into(), "# A\n\nb".into());
        assert_eq!((u.unchanged, u.replaced.len(), u.appended.len()), (2, 0, 0));
    }

    #[test]
    fn raw_html_stays_text() {
        let s = MarkdownStore::new();
        let u = s.update("m".into(), "<script>alert(1)</script> <img src=x onerror=y>".into());
        let r = s.render("m".into(), u.blocks.iter().map(|b| b.id.clone()).collect(), true);
        let html = r.blocks[0].html.clone().unwrap();
        assert!(!html.contains("<script") && !html.contains("<img"), "{html}");
    }
}
