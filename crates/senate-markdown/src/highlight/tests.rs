use super::*;
use crate::{Block, Document};

#[test]
fn fences_color_representative_tokens() {
    let specimens = Document::parse(include_str!("../../tests/fixtures/highlight.md"));
    let expected = [
        ("rust", Token::Name, "#[derive"),
        ("swift", Token::Name, "@MainActor"),
        ("typescript", Token::Keyword, "interface"),
        ("tsx", Token::Name, "Panel"),
        ("javascript", Token::Name, "fetch"),
        ("jsx", Token::Name, "aria-label"),
        ("python", Token::Name, "@cache"),
        ("go", Token::Name, "Sprintf"),
        ("c", Token::Number, "0x1.8p+2"),
        ("cpp", Token::Keyword, "template"),
        ("java", Token::Name, "@Override"),
        ("kotlin", Token::Keyword, "data"),
        ("ruby", Token::String, "Hello, #{name}!"),
        ("bash", Token::String, "Hello, world"),
        ("json", Token::Name, "\"enabled\""),
        ("yaml", Token::String, "This is literal text."),
        ("toml", Token::Name, "enabled"),
        ("html", Token::Name, "aria-label"),
        ("css", Token::Name, "margin-inline"),
        ("sql", Token::String, "'O''Neal'"),
        ("diff", Token::Keyword, "@@"),
        ("markdown", Token::String, "`code`"),
        ("dockerfile", Token::Keyword, "ENTRYPOINT"),
        ("makefile", Token::Comment, "# Configuration"),
        ("csharp", Token::Name, "GetAsync"),
        ("lua", Token::Comment, "comment ]=]"),
        ("perl", Token::Keyword, "my"),
        ("php", Token::Keyword, "function"),
        ("scala", Token::Keyword, "case"),
        ("xml", Token::Name, "ns:message"),
    ];
    let mut samples = specimens
        .blocks()
        .iter()
        .filter_map(|entry| match &*entry.block {
            Block::Code(code) => Some(code),
            _ => None,
        });
    for (label, token, value) in expected {
        let code = samples.next().expect("each label should have a specimen");
        assert_eq!(code.language.as_deref(), Some(label));
        let mut lexer = Lexer::new(syntax(Some(label)).expect("supported label"));
        assert!(
            code.text.split_inclusive('\n').any(|line| {
                scan(&mut lexer, line)
                    .iter()
                    .any(|(kind, span)| *kind == Some(token) && span.contains(value))
            }),
            "{label} is missing {value:?} as {token:?}"
        );
    }
    assert!(samples.next().is_none());
    assert_eq!(syntax(Some("zsh")), syntax(Some("sh")));
    assert_eq!(syntax(Some("BASH")), syntax(Some("sh")));
    assert_eq!(syntax(Some("zzzz")), None);
}

#[test]
fn resumes_inside_multiline_constructs() {
    use Token::*;
    let cases: &[(&str, &str, &str, &str, Token)] = &[
        (
            "rust",
            "/* outer /* inner */\n",
            "continued */\n",
            "let n = 1;\n",
            Comment,
        ),
        (
            "swift",
            "/* outer /* inner */\n",
            "continued */\n",
            "let n = 1\n",
            Comment,
        ),
        ("rust", "r##\"start\n", "end\"##\n", "let n = 1;\n", String),
        (
            "swift",
            "#\"\"\"\n",
            "hello\n\"\"\"#",
            "let n = 1\n",
            String,
        ),
        (
            "cpp",
            "R\"text(start\n",
            "end)text\"\n",
            "return 1;\n",
            String,
        ),
        (
            "csharp",
            "@\"start\n",
            "end\"\"quoted\"\"\",\n",
            "return 1;\n",
            String,
        ),
        ("python", "r'''start\n", "end'''\n", "return 1\n", String),
        (
            "toml",
            "value = '''start\n",
            "end'''\n",
            "enabled = true\n",
            String,
        ),
        (
            "bash",
            "cat <<-END\n",
            "\tbody\n\tEND\n",
            "echo done\n",
            String,
        ),
        (
            "ruby",
            "text = <<~END\n",
            "  body\n  END\n",
            "puts(text)\n",
            String,
        ),
        (
            "yaml",
            "literal: >2-\n",
            "  123\n  @plain\n",
            "enabled: true\n",
            String,
        ),
        (
            "typescript",
            "const x = `a ${`b ${f({\n",
            "x: 1})}`}`\n",
            "const y = 1\n",
            String,
        ),
        (
            "tsx",
            "const x = <Panel size={\n",
            "size()}>outside</Panel>\n",
            "const y = 1\n",
            Name,
        ),
    ];
    for &(lang, opening, middle, after, style) in cases {
        let mut lexer = Lexer::new(syntax(Some(lang)).expect("supported label"));
        scan(&mut lexer, opening);
        let mut restored = lexer.clone();
        let mut decorated = false;
        for line in middle.split_inclusive('\n') {
            let spans = scan(&mut lexer, line);
            decorated |= spans.iter().any(|&(kind, _)| kind == Some(style));
            assert_eq!(spans, scan(&mut restored, line));
        }
        assert!(decorated, "{lang} lost its multiline {style:?} state");
        let mut fresh = Lexer::new(lexer.language);
        assert_eq!(
            scan(&mut lexer, after),
            scan(&mut fresh, after),
            "{lang} did not exit multiline state"
        );
    }
}

#[test]
fn arbitrary_utf8_is_partitioned_without_change() {
    let body = include_str!("../../tests/fixtures/highlight.md");
    let specimen = Document::parse(body);
    let alphabet = [
        'a', 'é', '界', '🦀', '\u{301}', '\0', '\t', '\\', '\r', '/', '*', ' ', '\'', '"', '#',
        '-', '+', '|', '(', '}', '=', '`', '$', '>', '<', ':', '!', '@', ';', '[', ']', '0', '9',
    ];
    let mut seed = 0xcafe_babe_u32;
    for block in specimen.blocks() {
        let Block::Code(code) = &*block.block else {
            continue;
        };
        let mut lexer = Lexer::new(syntax(code.language.as_deref()).expect("supported label"));
        for line in 0..256 {
            let mut text = String::new();
            for _ in 0..(line % 113) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                text.push(alphabet[seed as usize % alphabet.len()]);
            }
            text.push('\n');
            scan(&mut lexer, &text);
        }
    }
}

fn scan<'a>(lexer: &mut Lexer, line: &'a str) -> Vec<(Option<Token>, &'a str)> {
    let spans = lexer.line(line).expect("normal-size line");
    assert!(spans.iter().all(|(_, content)| !content.is_empty()));
    assert_eq!(
        spans
            .iter()
            .map(|(_, content)| *content)
            .collect::<String>(),
        line
    );
    spans
}
