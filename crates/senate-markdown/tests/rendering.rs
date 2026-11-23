#![allow(clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use senate_markdown::{
    Alignment, Block, Color, Document, Format, Inline, Part, RenderCache, Terminal,
};
use textwrap::core::display_width;

const SPECIMEN: &str = include_str!("fixtures/specimen.md");
const THREAD: &str = include_str!("fixtures/thread-2148.md");

fn render(document: &Document, cache: &RenderCache, format: Format) -> String {
    let mut out = String::new();
    for entry in document.blocks() {
        assert!(cache.write_block(entry.id, format, &mut out));
    }
    out
}

fn rendered(text: &str, format: Format, terminal: Terminal) -> String {
    let document = Document::parse(text);
    let mut cache = RenderCache::new(terminal);
    cache.reset(&document);
    render(&document, &cache, format)
}

#[test]
fn both_specimens_render_as_typed_blocks() {
    for example in [SPECIMEN, THREAD] {
        let document = Document::parse(example);
        assert!(
            document
                .blocks()
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id)
        );
        assert!(document.blocks().iter().any(|entry| matches!(entry.block.as_ref(),
            Block::Table { alignment, .. } if alignment == &[Alignment::Left, Alignment::Center, Alignment::Right, Alignment::Default]
        )));
        let mut cache = RenderCache::new(Terminal::new(48, Color::None));
        cache.reset(&document);
        let html = render(&document, &cache, Format::Html);
        for value in [
            "<strong>strong</strong>",
            "<em>emphasis</em>",
            "<del>struck</del>",
            "<a href=\"https://senate.dev\">",
            "<thead>",
            "<ol",
            "data-state=\"done\"",
            "<blockquote>",
            "<hr>",
            "tok-keyword",
            "中文",
            "[senator::",
        ] {
            assert!(html.contains(value), "missing {value:?}");
        }
        let terminal = render(&document, &cache, Format::Terminal);
        assert!(terminal.lines().all(|line| display_width(line) <= 48));
        assert!(
            terminal.contains("│") && terminal.contains("[x]") && terminal.contains("[senator::")
        );
    }
}

#[test]
fn existing_bracketed_mentions_remain_literal_even_with_link_definitions() {
    let text = "请[senator::codex]看一下，[speaker]，[committee]。\n\n[senator::codex]: https://example.com\n[speaker]: https://example.com\n[committee]: https://example.com\n\n`[senator::codex]` and [guide][guide].\n\n[guide]: https://senate.dev";
    let document = Document::parse(text);
    let Block::Paragraph(inlines) = document.blocks()[0].block.as_ref() else {
        panic!("paragraph")
    };
    assert!(
        inlines
            .iter()
            .all(|inline| matches!(inline, Inline::Text(_)))
    );
    let html = rendered(text, Format::Html, Terminal::new(80, Color::None));
    assert!(html.contains("请[senator::codex]看一下，[speaker]，[committee]。"));
    assert!(html.contains("<code>[senator::codex]</code>"));
    assert!(html.contains("<a href=\"https://senate.dev\">guide</a>"));
}

#[test]
fn partial_constructs_keep_their_identity_and_settle() {
    let mut document = Document::parse("Unchanged\n\n| name | age |\n");
    let earlier = document.blocks()[0].block.clone();
    let tail = document.blocks()[1].id;
    let diff = document.update("Unchanged\n\n| name | age |\n| :--- | ---: |\n");
    assert_eq!(diff.replaced, [tail]);
    assert!(Arc::ptr_eq(&earlier, &document.blocks()[0].block));
    assert!(matches!(document.block(tail), Some(Block::Table { .. })));

    let mut document = Document::parse("Top\n\n**unfinished\n\n- thi");
    let ids: Vec<_> = document.blocks().iter().map(|entry| entry.id).collect();
    assert_eq!(
        document
            .update("Top\n\n**unfinished**\n\n- thing\n")
            .replaced,
        ids[1..]
    );
    assert!(
        matches!(document.block(ids[1]), Some(Block::Paragraph(content)) if matches!(content[0], Inline::Strong(_)))
    );
    let diff = document.update("Top\n\nReplacement");
    assert_eq!(diff.removed, [ids[2]]);
    assert_eq!(diff.replaced, [ids[1]]);
}

#[test]
fn completed_reference_can_replace_an_earlier_block() {
    let mut document = Document::parse("[guide][later]\n\nSeveral blocks later.\n\n");
    let first = document.blocks()[0].id;
    let diff = document
        .update("[guide][later]\n\nSeveral blocks later.\n\n[later]: https://senate.dev\n\n");
    assert_eq!(diff.replaced, [first]);
    assert!(diff.appended.is_empty());
    assert_eq!(diff.unchanged, 1);
}

#[test]
fn append_and_incomplete_lines_keep_multiline_highlight_and_code_prefix() {
    let prefix = "Intro\n\n> ```rust\n> /* a comment\n> interior\n";
    let terminal = Terminal::new(38, Color::TrueColor);
    let mut document = Document::parse(prefix);
    let mut cache = RenderCache::new(terminal);
    let initial = cache.reset(&document);
    assert_eq!(initial.highlighted_lines, 2);
    let id = document.blocks()[1].id;
    let text = format!("{prefix}> still a comment */\n");
    let diff = document.update(&text);
    let update = cache.apply(&document, &diff);
    assert_eq!(update.highlighted_lines, 1);
    assert_eq!(update.rendered_code_lines, 1);
    assert!(!update.blocks[0].layout_changed);
    assert_eq!(update.blocks[0].code[0].from_line, 2);
    assert!(cache.code_lines(id, 0, Format::Html).unwrap()[2].contains("tok-comment"));

    let mut growing = format!("{text}> let ze");
    let diff = document.update(&growing);
    let update = cache.apply(&document, &diff);
    assert_eq!(update.highlighted_lines, 1);
    growing.push_str("ro = 0;\n");
    let diff = document.update(&growing);
    let update = cache.apply(&document, &diff);
    assert_eq!(update.highlighted_lines, 1);
    assert_eq!(update.blocks[0].code[0].from_line, 3);
    assert_eq!(
        render(&document, &cache, Format::Html),
        rendered(&growing, Format::Html, terminal)
    );
}

#[test]
fn randomized_utf8_stream_patches_only_semantically_changed_blocks() {
    let reply = format!(
        "{SPECIMEN}\n\n{THREAD}\n\n```rust\n{}\n```\n",
        "    let 結果 = 123; // some text\n".repeat(380)
    );
    let final_document = Document::parse(&reply);
    let terminal = Terminal::new(72, Color::Ansi256);
    let mut one_shot = RenderCache::new(terminal);
    let full_work = one_shot.reset(&final_document);

    for mut rng in [4_u32, 1783, 2188] {
        let mut document = Document::default();
        let mut cache = RenderCache::new(terminal);
        let mut bytes = 0;
        let mut total_lines = 0;
        let mut updates = 0;
        let mut full_ablation = 0;
        while bytes < reply.len() {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            bytes = (bytes + 1 + rng as usize % 67).min(reply.len());
            while !reply.is_char_boundary(bytes) {
                bytes += 1;
            }
            let old = document.blocks().to_vec();
            let diff = document.update(&reply[..bytes]);
            let touched: BTreeSet<_> = diff
                .replaced
                .iter()
                .chain(&diff.appended)
                .chain(&diff.inserted)
                .copied()
                .collect();
            let work = cache.apply(&document, &diff);
            total_lines += work.highlighted_lines;
            updates += 1;
            full_ablation += document
                .blocks()
                .iter()
                .filter_map(|entry| {
                    let Block::Code(code) = entry.block.as_ref() else {
                        return None;
                    };
                    code.language
                        .as_ref()
                        .map(|_| code.text.split_inclusive('\n').count())
                })
                .sum::<usize>();
            for patch in &work.blocks {
                assert!(touched.contains(&patch.id));
            }
            for entry in document.blocks() {
                if let Some(before) = old.iter().find(|before| before.id == entry.id) {
                    assert_eq!(touched.contains(&entry.id), before.block != entry.block);
                    if before.block == entry.block {
                        assert!(Arc::ptr_eq(&before.block, &entry.block));
                    }
                }
            }
            for item in &diff.removed {
                assert!(document.block(*item).is_none());
            }
            // This sample has no reference definitions. The only change
            // before the last block can be a table absorbing a
            // provisional paragraph for an unfinished final row.
            if let Some(last) = old.last() {
                for &id in &diff.replaced {
                    let absorbed_partial_row =
                        matches!(document.block(id), Some(Block::Table { .. }))
                            && old
                                .iter()
                                .filter(|entry| entry.id > id)
                                .all(|entry| diff.removed.contains(&entry.id));
                    assert!(
                        id == last.id || absorbed_partial_row,
                        "append changed a block before the active tail at byte {bytes}: {diff:?}"
                    );
                }
            }
        }
        assert_eq!(document, final_document);
        for format in [Format::Html, Format::Terminal] {
            assert_eq!(
                render(&document, &cache, format),
                render(&final_document, &one_shot, format)
            );
        }
        assert!(total_lines < full_ablation / 10);
        println!(
            "{updates} updates, {total_lines} incremental highlighted lines, {full_ablation} full-refresh lines, {} one-shot lines",
            full_work.highlighted_lines
        );
    }
}

#[test]
fn output_from_hostile_markdown_stays_within_the_html_contract() {
    let long = "x".repeat(70_000);
    let text = format!(
        "<script>alert(1)</script>\n\n<img src=x onerror=alert(2)>\n\n\
        [x](javascript:alert%281%29) [x](JaVaScRiPt:alert%282%29) [x](jav&#x61;script:bad)\n\n\
        [x](data:text/html,bad) [x](//bad.invalid) ![alt](https://bad.invalid/a.png)\n\n\
        [safe](https://site.invalid/?x=\x22y\x22&z=1) <person@example.com>\n\n\
        [[[[senator::codex]]]] `<script>&\x1b]52;evil\x07`\n\n\
        ```rust\n{long}\n<svg onload=evil()>\x1b[31m\x00"
    );
    let terminal = Terminal::new(80, Color::None);
    let html = rendered(&text, Format::Html, terminal);
    let allowed = [
        "p",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "ul",
        "ol",
        "li",
        "span",
        "pre",
        "code",
        "blockquote",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
        "hr",
        "br",
        "strong",
        "em",
        "del",
        "a",
    ];
    for tag in html.split('<').skip(1) {
        let (tag, _) = tag.split_once('>').unwrap();
        let (name, mut attrs) = tag
            .trim_start_matches('/')
            .split_once(' ')
            .unwrap_or((tag.trim_start_matches('/'), ""));
        assert!(allowed.contains(&name), "emitted {name}");
        while !attrs.is_empty() {
            let (key, raw) = attrs.trim_start().split_once("=\"").unwrap();
            let (value, remaining) = raw.split_once('"').unwrap();
            assert!(
                match (name, key) {
                    ("a", "href") => ["https:", "http:", "mailto:"]
                        .iter()
                        .any(|scheme| value.starts_with(scheme)),
                    ("span", "class") => [
                        "task-marker",
                        "tok-comment",
                        "tok-string",
                        "tok-keyword",
                        "tok-number",
                        "tok-name",
                        "tok-type",
                        "tok-operator"
                    ]
                    .contains(&value),
                    ("ul" | "ol", "class") => value == "task-list",
                    ("li", "class") => value == "task-item",
                    ("td" | "th", "class") =>
                        ["align-left", "align-center", "align-right"].contains(&value),
                    ("span", "data-state") => value == "done" || value == "open",
                    ("code", "data-language") => true,
                    ("ol", "start") => value.parse::<usize>().is_ok(),
                    _ => false,
                },
                "emitted {name} {key}={value}"
            );
            attrs = remaining.trim_start();
        }
    }
    assert!(html.contains("&lt;script&gt;"));
    assert!(!html.contains('\x1b') && !html.contains('\0'));
    assert!(!html.contains("bad.invalid"));
    let plain = rendered(&text, Format::Terminal, terminal);
    assert!(!plain.contains('\x1b') && !plain.contains('\x07') && !plain.contains('\0'));
    assert!(plain.lines().all(|line| display_width(line) <= 80));
}

#[test]
fn exposes_code_parts_and_terminal_color_depth_explicitly() {
    let mut document = Document::parse("```rust\nlet n = 123;\n```");
    for (color, prefix) in [
        (Color::None, ""),
        (Color::Ansi16, "\x1b["),
        (Color::Ansi256, "\x1b[38;5;"),
        (Color::TrueColor, "\x1b[38;2;"),
    ] {
        let mut cache = RenderCache::new(Terminal::new(28, color));
        cache.reset(&document);
        let id = document.blocks()[0].id;
        assert!(
            cache
                .parts(id, Format::Html)
                .unwrap()
                .any(|part| matches!(part, Part::Code { index: 0, lines } if lines.len() == 1))
        );
        let render = render(&document, &cache, Format::Terminal);
        if color == Color::None {
            assert!(!render.contains('\x1b'));
        } else {
            assert!(render.contains(prefix));
        }
    }
    let diff = document.update("");
    assert_eq!(diff.removed.len(), 1);
}

#[test]
fn middle_insertions_keep_later_ids_and_indented_code_stays_literal() {
    let mut document = Document::parse("Top\n\n\n\nBottom\n");
    let bottom = document.blocks()[1].id;
    let diff = document.update("Top\n\nMiddle\n\nBottom\n");
    assert_eq!(diff.unchanged, 2);
    assert!(diff.appended.is_empty());
    assert_eq!(diff.inserted, [document.blocks()[1].id]);
    assert_eq!(document.blocks()[2].id, bottom);

    let code = "    [senator::codex] <raw>\n";
    let document = Document::parse(code);
    assert!(matches!(
        document.blocks()[0].block.as_ref(),
        Block::Code(block) if !block.fenced && block.language.is_none()
    ));
    let html = rendered(code, Format::Html, Terminal::new(80, Color::None));
    assert_eq!(
        html,
        "<pre><code>[senator::codex] &lt;raw&gt;\n</code></pre>\n"
    );
}
