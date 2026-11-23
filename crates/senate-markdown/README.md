# senate-markdown

Parse CommonMark with GFM tables, task lists, strikethrough and autolinks into a Rust block tree. The tree has paragraphs, headings, lists, code, quotes, tables and thematic breaks, with nested inline styles and links. HTML in the source is represented as text. Image captions are retained and images never load. Senate text such as `[senator::codex]`, `ledger::chair` and `#127` remains text. A Senate bracketed label stays literal when it could otherwise become a Markdown link reference.

```rust
use senate_markdown::{Color, Document, Format, RenderCache, Terminal};

let mut message = Document::parse("A **message**\n\n```rust\nlet n = 1;\n");
let mut view = RenderCache::new(Terminal::new(80, Color::None));
view.reset(&message);

let diff = message.update("A **message**\n\n```rust\nlet n = 1;\nlet m = 2;\n");
let changes = view.apply(&message, &diff);
assert_eq!(changes.highlighted_lines, 1);

let mut html = String::new();
for entry in message.blocks() {
    view.write_block(entry.id, Format::Html, &mut html);
}
```

## Streaming

Every top-level block has a source-position ID. Position lets a frontend replace a partially written paragraph when it becomes a table or heading. A replacement that shifts a position may remove and insert that block. `Document::update` reparses the full input and compares the normalized blocks so a late reference definition can change an earlier paragraph. Unchanged blocks retain their allocation and the diff lists only changes.

Call `RenderCache::reset` for a fresh document or terminal configuration. Apply consecutive document diffs to the same cache. A block patch with `layout_changed` calls for replacing that block using `parts` or `write_block`. Code bodies are separate parts, including fences nested in lists or quotes. When the wrappers stay the same, use each patch's `from_line` to replace only the suffix of that code body. The cache keeps the previous lines and their output. An appended line uses carried syntax state. Growth of a partial last line revises that one line. Changes before the last line can re-lex that code block. `write_block` explicitly joins the cached output if a client needs a complete string.

Comrak runs with defaults disabled and the four GFM extensions above enabled. Syntect (MIT) uses the pure-Rust regex engine and the linked two-face 0.5.1 syntax pack. The supported fence labels cover Rust, Swift, TypeScript, TSX, JavaScript, JSX, Python, Go, C, C#, C++, Java, Kotlin, Ruby, Bash, JSON, YAML, TOML, HTML, CSS, SQL, Diff, Markdown, Dockerfile, Makefile, Lua, Perl, PHP, Scala and XML. JSX uses the pack's JSX-capable TypeScriptReact grammar. Unknown labels and individual lines above 16 KiB render as plain code. Use `RenderCache::with_highlight_limit` to skip highlighting on larger blocks or pass zero to defer all highlighting until a fresh reset with a highlighting cache.

Two-face is dual licensed under MIT or Apache-2.0. The linked grammar data has separate licenses, including MIT for Swift, Dockerfile and TOML, and Apache-2.0 for Kotlin and TypeScript/TSX. The [bundled syntax notices](THIRD-PARTY-SYNTAX-NOTICES.md) cover the linked pack, including internal grammar dependencies. Carry the notice file and the dependency notices with binary distributions. The [upstream full acknowledgement list](https://github.com/CosmicHorrorDev/two-face/blob/v0.5.1/generated/acknowledgements_full.md) records the licenses of the individual assets.

## Rendering contract

The HTML renderer escapes text and attributes and emits these tags:

- `p`, `h1` through `h6`, `ul`, `ol`, `li`, `pre`, `code`, `blockquote`, `table`, `thead`, `tbody`, `tr`, `th`, `td`, `hr`, `br`, `strong`, `em`, `del`, `a`, and `span`.
- `href` on links uses HTTP, HTTPS or mailto URLs. The remaining attributes are `start` on ordered lists, `data-language` on fenced code, `data-state="done|open"` on task markers, and the classes below.
- `task-list`, `task-item` and `task-marker` style task lists. `align-left`, `align-center` and `align-right` align table cells.
- `tok-comment`, `tok-string`, `tok-keyword`, `tok-number`, `tok-name`, `tok-type` and `tok-operator` style code. All CSS is supplied by the client.

The terminal renderer uses the same display-width calculation as the CLI, and formats code to the same width as prose. Plain headings use a hash prefix per level. Task boxes replace the list bullet and use ASCII markers in non-UTF-8 locales. Lists use hanging indents, narrow tables can stack by row, and links display their destination when it differs from the label. `Terminal::stdout` checks for a TTY, honors `NO_COLOR` and `TERM=dumb`, and chooses 16, 256 or true color using TERM and COLORTERM. `Terminal::new` accepts explicit capabilities for remote terminals and tests.

## Sample

From the repository root:

```sh
cargo run --release -p senate-markdown --example render -- \
  crates/senate-markdown/tests/fixtures/thread-2148.md \
  --width 80 --html /tmp/senate-markdown.html
```

Add `--stream` to replay deterministic UTF-8 chunks. Standard error reports the parser and renderer time and the work done for each update. Standard output is the complete terminal sample.
