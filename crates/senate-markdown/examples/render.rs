use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use senate_markdown::{Document, Format, RenderCache, Terminal};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(
        args.next()
            .ok_or("usage: render FILE [--html FILE] [--width COLUMNS] [--stream]")?,
    );
    let mut html = None;
    let mut width = None;
    let mut stream = false;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--html") => {
                html = Some(PathBuf::from(args.next().ok_or("--html requires a file")?))
            }
            Some("--width") => {
                width = Some(
                    args.next()
                        .and_then(|arg| arg.to_str()?.parse().ok())
                        .ok_or("--width requires a positive column count")?,
                )
            }
            Some("--stream") => stream = true,
            _ => return Err(format!("unknown option {}", arg.to_string_lossy()).into()),
        }
    }

    let text = std::fs::read_to_string(input)?;
    let mut cache = RenderCache::new(Terminal::stdout(width));
    let document = if stream {
        replay(&text, &mut cache)
    } else {
        let document = Document::parse(&text);
        cache.reset(&document);
        document
    };
    let mut terminal = String::new();
    let mut markup = String::new();
    for (index, entry) in document.blocks().iter().enumerate() {
        if index != 0 {
            terminal.push('\n');
        }
        cache.write_block(entry.id, Format::Terminal, &mut terminal);
        if html.is_some() {
            cache.write_block(entry.id, Format::Html, &mut markup);
        }
    }
    print!("{terminal}");
    if let Some(path) = html {
        std::fs::write(path, format!("{HTML_START}{markup}</body></html>\n"))?;
    }
    Ok(())
}

fn replay(text: &str, cache: &mut RenderCache) -> Document {
    let mut document = Document::default();
    let mut end = 0;
    let mut random = 2172_u32;
    while end < text.len() {
        random ^= random << 13;
        random ^= random >> 17;
        random ^= random << 5;
        let previous = end;
        end = (end + 1 + random as usize % 23).min(text.len());
        while !text.is_char_boundary(end) {
            end += 1;
        }
        let start = Instant::now();
        let changes = document.update(&text[..end]);
        let parse = start.elapsed();
        let start = Instant::now();
        let rendered = cache.apply(&document, &changes);
        eprintln!(
            "+{:>2} bytes at {:>5}  kept={} replaced={} appended={} removed={} patched={} highlighted_lines={} rendered_code_lines={} parse={:?} render={:?}",
            end - previous,
            end,
            changes.unchanged,
            changes.replaced.len(),
            changes.appended.len() + changes.inserted.len(),
            changes.removed.len(),
            rendered.blocks.len(),
            rendered.highlighted_lines,
            rendered.rendered_code_lines,
            parse,
            start.elapsed()
        );
    }
    document
}

const HTML_START: &str = r#"<!doctype html>
<html lang="en"><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Senate Markdown</title>
<style>
:root{color-scheme:light dark}
body{font:17px/1.55 system-ui;max-width:72ch;margin:3rem auto;padding:0 1.2rem;overflow-wrap:anywhere}
pre{padding:1em;overflow:auto;background:color-mix(in srgb,currentColor 8%,transparent)}
code{font-family:ui-monospace,monospace}
blockquote{margin-inline:0;padding-left:1em;border-left:3px solid #868c9b}
table{border-collapse:collapse;width:100%;table-layout:fixed}
th,td{border:1px solid #868c9b;padding:.4em .65em}
.align-left{text-align:left}.align-center{text-align:center}.align-right{text-align:right}
.task-item{list-style:none}.task-marker{display:inline-block;min-width:1.35em}
.tok-comment{color:light-dark(#576878,#a3b3c3)}.tok-string{color:light-dark(#27633d,#94ca8e)}
.tok-keyword{color:light-dark(#7136a3,#d1a2f2)}.tok-number{color:light-dark(#715522,#ebc88c)}
.tok-name{color:light-dark(#174d91,#99c3ff)}.tok-type{color:light-dark(#176272,#83d5d7)}
.tok-operator{color:light-dark(#963d49,#f0a0aa)}
</style><body>
"#;
