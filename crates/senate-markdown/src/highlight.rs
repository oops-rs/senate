use std::sync::LazyLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_newlines);

pub(crate) fn syntax(language: Option<&str>) -> Option<&'static SyntaxReference> {
    let name = match language?.to_ascii_lowercase().as_str() {
        "bash" | "sh" | "shell" | "zsh" => "Bourne Again Shell (bash)",
        "c" => "C",
        "cs" | "csharp" | "c#" => "C#",
        "cpp" | "c++" | "cxx" => "C++",
        "css" => "CSS",
        "diff" | "patch" => "Diff",
        "dockerfile" | "docker" => "Dockerfile",
        "go" | "golang" => "Go",
        "html" => "HTML",
        "java" => "Java",
        "js" | "javascript" => "JavaScript",
        "json" => "JSON",
        "jsx" | "tsx" => "TypeScriptReact",
        "kt" | "kotlin" => "Kotlin",
        "lua" => "Lua",
        "make" | "makefile" => "Makefile",
        "md" | "markdown" => "Markdown",
        "pl" | "perl" => "Perl",
        "php" => "PHP",
        "py" | "python" => "Python",
        "rb" | "ruby" => "Ruby",
        "rs" | "rust" => "Rust",
        "scala" => "Scala",
        "sql" => "SQL",
        "swift" => "Swift",
        "toml" => "TOML",
        "ts" | "typescript" => "TypeScript",
        "xml" => "XML",
        "yml" | "yaml" => "YAML",
        _ => return None,
    };
    SYNTAXES.find_syntax_by_name(name)
}

#[derive(Clone, Copy)]
pub(crate) enum Token {
    Comment,
    String,
    Keyword,
    Number,
    Name,
    Type,
    Operator,
}

impl Token {
    pub(crate) fn class(self) -> &'static str {
        match self {
            Self::Comment => "tok-comment",
            Self::String => "tok-string",
            Self::Keyword => "tok-keyword",
            Self::Number => "tok-number",
            Self::Name => "tok-name",
            Self::Type => "tok-type",
            Self::Operator => "tok-operator",
        }
    }
}

static CLASSES: LazyLock<Vec<(Scope, Token)>> = LazyLock::new(|| {
    [
        ("comment", Token::Comment),
        ("string", Token::String),
        ("keyword.operator", Token::Operator),
        ("keyword", Token::Keyword),
        ("storage", Token::Keyword),
        ("constant.numeric", Token::Number),
        ("constant.language", Token::Keyword),
        ("entity.name.function", Token::Name),
        ("entity.name.tag", Token::Name),
        ("support.function", Token::Name),
        ("entity.name.type", Token::Type),
        ("support.type", Token::Type),
        ("storage.type", Token::Type),
    ]
    .into_iter()
    .filter_map(|(scope, token)| Some((Scope::new(scope).ok()?, token)))
    .collect()
});

fn token(scopes: &ScopeStack) -> Option<Token> {
    scopes.as_slice().iter().rev().find_map(|scope| {
        CLASSES
            .iter()
            .find_map(|&(prefix, token)| prefix.is_prefix_of(*scope).then_some(token))
    })
}

#[derive(Clone)]
pub(crate) struct Lexer {
    parser: ParseState,
    scopes: ScopeStack,
}

impl Lexer {
    pub(crate) fn new(syntax: &SyntaxReference) -> Self {
        Self {
            parser: ParseState::new(syntax),
            scopes: ScopeStack::new(),
        }
    }

    pub(crate) fn line<'a>(&mut self, line: &'a str) -> Option<Vec<(Option<Token>, &'a str)>> {
        // Fancy regexes expect a contiguous line. Bound the work on
        // minified files and other pathological input.
        if line.len() > 16 * 1024 {
            return None;
        }
        let mut next = self.clone();
        let ops = next.parser.parse_line(line, &SYNTAXES).ok()?;
        let mut spans = Vec::new();
        let mut start = 0;
        for (end, op) in ops {
            if start < end {
                spans.push((token(&next.scopes), line.get(start..end)?));
            }
            next.scopes.apply(&op).ok()?;
            start = end;
        }
        if start < line.len() {
            spans.push((token(&next.scopes), line.get(start..)?));
        }
        *self = next;
        Some(spans)
    }
}
