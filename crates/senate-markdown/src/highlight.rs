//! Line-oriented lexical highlighting. No compiled grammar or global cache.

use std::{collections::VecDeque, sync::Arc};

mod language;
#[cfg(test)]
mod tests;

use language::Language;
pub(crate) use language::syntax;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy)]
enum End {
    Literal(&'static str),
    HashQuote { quote: &'static str, hashes: u8 },
    LuaBracket(u16),
    CppRaw { delimiter: [u8; 16], len: u8 },
}

impl End {
    fn width(self, s: &str) -> Option<usize> {
        match self {
            Self::Literal(value) => s.starts_with(value).then_some(value.len()),
            Self::HashQuote { quote, hashes } => {
                let hashes = usize::from(hashes);
                let tail = s.strip_prefix(quote)?.as_bytes().get(..hashes)?;
                tail.iter()
                    .all(|&byte| byte == b'#')
                    .then_some(quote.len() + hashes)
            }
            Self::CppRaw { delimiter, len } => {
                let tail = s.strip_prefix(')')?.as_bytes();
                let len = usize::from(len);
                (tail.starts_with(&delimiter[..len]) && tail.get(len) == Some(&b'"'))
                    .then_some(len + 2)
            }
            Self::LuaBracket(equals) => {
                let equals = usize::from(equals);
                let tail = s.strip_prefix(']')?.as_bytes();
                (tail.get(equals) == Some(&b']') && tail[..equals].iter().all(|&byte| byte == b'='))
                    .then_some(equals + 2)
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Quote {
    end: End,
    escape: bool,
    multiline: bool,
    template: bool,
    doubled: bool,
}

#[derive(Clone, Copy)]
enum Mode {
    Code,
    Quoted {
        quote: Quote,
        token: Token,
    },
    Comment {
        close: End,
        nested: bool,
        depth: usize,
    },
}

#[derive(Clone, Copy)]
enum Tag {
    Opening,
    Closing,
    Markup,
}

#[derive(Clone, Copy)]
enum Resume {
    Template(Quote),
    Jsx { depth: usize, tag: Option<Tag> },
}

#[derive(Clone, Copy)]
struct Expression {
    resume: Resume,
    braces: usize,
}

#[derive(Clone, Copy)]
enum Indent {
    Exact,
    Tabs,
    Any,
}

#[derive(Clone)]
struct Heredoc {
    delimiter: Arc<str>,
    indent: Indent,
}

#[derive(Clone, Copy)]
struct Scalar {
    parent: usize,
    indentation: Option<usize>,
}

#[derive(Clone)]
pub(crate) struct Lexer {
    language: Language,
    mode: Mode,
    expressions: Vec<Expression>,
    tag: Option<Tag>,
    jsx_depth: usize,
    heredocs: VecDeque<Heredoc>,
    scalar: Option<Scalar>,
    css_block_depth: usize,
}

struct Spans<'a> {
    text: &'a str,
    at: usize,
    result: Vec<(Option<Token>, &'a str)>,
}

impl<'a> Spans<'a> {
    fn push(&mut self, count: usize, style: Option<Token>) {
        if count != 0 {
            let start = self.at;
            self.at += count;
            if let Some((previous, span)) = self.result.last_mut()
                && *previous == style
            {
                *span = &self.text[start - span.len()..self.at];
            } else {
                self.result.push((style, &self.text[start..self.at]));
            }
        }
    }
}

impl Lexer {
    pub(crate) fn new(language: Language) -> Self {
        Self {
            language,
            mode: Mode::Code,
            expressions: Vec::new(),
            tag: None,
            jsx_depth: 0,
            heredocs: VecDeque::new(),
            scalar: None,
            css_block_depth: 0,
        }
    }

    pub(crate) fn line<'a>(&mut self, text: &'a str) -> Option<Vec<(Option<Token>, &'a str)>> {
        // Match the existing policy for very long or minified lines.
        if text.len() > 16 * 1024 {
            return None;
        }
        let mut spans = Spans {
            text,
            at: 0,
            result: Vec::new(),
        };
        if self.language == Language::Diff {
            let style = match text.as_bytes().first() {
                Some(b'+') => Some(Token::String),
                Some(b'-') => Some(Token::Comment),
                Some(b'@') => Some(Token::Keyword),
                _ => None,
            };
            spans.push(text.len(), style);
            return Some(spans.result);
        }
        if self.block_text(text) {
            spans.push(text.len(), Some(Token::String));
            return Some(spans.result);
        }
        while spans.at < text.len() {
            let remaining = &text[spans.at..];
            match self.mode {
                Mode::Code => self.code(remaining, &mut spans),
                Mode::Quoted { quote, token } => self.quoted(remaining, quote, token, &mut spans),
                Mode::Comment {
                    close,
                    nested,
                    depth,
                } => {
                    self.comment(remaining, close, nested, depth, &mut spans);
                }
            }
        }
        Some(spans.result)
    }

    fn block_text(&mut self, text: &str) -> bool {
        if let Some(heredoc) = self.heredocs.front() {
            let line = text.trim_end_matches(['\r', '\n']);
            let line = match heredoc.indent {
                Indent::Exact => line,
                Indent::Tabs => line.trim_start_matches('\t'),
                Indent::Any => line.trim_start_matches([' ', '\t']),
            };
            if line == &*heredoc.delimiter {
                self.heredocs.pop_front();
            }
            return true;
        }
        if let Some(scalar) = &mut self.scalar {
            if text.trim().is_empty() {
                return true;
            }
            let indentation = text.bytes().take_while(|&byte| byte == b' ').count();
            let minimum = scalar.indentation.unwrap_or(scalar.parent + 1);
            if indentation >= minimum {
                scalar.indentation.get_or_insert(indentation);
                return true;
            }
            self.scalar = None;
        }
        false
    }

    fn enter_expression(&mut self, resume: Resume) -> bool {
        if self.expressions.len() == 32 {
            return false;
        }
        self.expressions.push(Expression { resume, braces: 0 });
        self.mode = Mode::Code;
        true
    }

    fn expression_brace(&mut self, first: char) {
        let Some(expression) = self.expressions.last_mut() else {
            return;
        };
        match first {
            '{' => expression.braces += 1,
            '}' if expression.braces > 0 => expression.braces -= 1,
            '}' => {
                match expression.resume {
                    Resume::Template(quote) => {
                        self.mode = Mode::Quoted {
                            quote,
                            token: Token::String,
                        }
                    }
                    Resume::Jsx { depth, tag } => {
                        self.jsx_depth = depth;
                        self.tag = tag;
                    }
                }
                self.expressions.pop();
            }
            _ => {}
        }
    }

    fn comment(
        &mut self,
        text: &str,
        close: End,
        nested: bool,
        mut depth: usize,
        spans: &mut Spans<'_>,
    ) {
        let mut end = 0;
        while end < text.len() {
            let rest = &text[end..];
            if let Some(width) = close.width(rest) {
                end += width;
                depth -= 1;
                if depth == 0 {
                    self.mode = Mode::Code;
                    break;
                }
            } else if nested && rest.starts_with("/*") {
                depth += 1;
                end += 2;
            } else {
                end += char_at(rest).len_utf8();
            }
        }
        if depth > 0 {
            self.mode = Mode::Comment {
                close,
                nested,
                depth,
            };
        }
        spans.push(end, Some(Token::Comment));
    }

    fn quoted(&mut self, text: &str, quote: Quote, token: Token, spans: &mut Spans<'_>) {
        let mut end = 0;
        let mut continued = false;
        while end < text.len() {
            let rest = &text[end..];
            if let Some(width) = quote.end.width(rest) {
                if quote.doubled && quote.end.width(&rest[width..]).is_some() {
                    end += 2 * width;
                    continue;
                }
                self.mode = Mode::Code;
                spans.push(end + width, Some(token));
                return;
            }
            if quote.template
                && rest.starts_with("${")
                && self.enter_expression(Resume::Template(quote))
            {
                spans.push(end, Some(token));
                spans.push(2, Some(Token::Operator));
                return;
            }
            if quote.escape && rest.starts_with('\\') {
                if let Some(escaped) = rest[1..].chars().next() {
                    end += 1 + escaped.len_utf8();
                    continued = matches!(escaped, '\r' | '\n')
                        && rest[1..].trim_matches(['\r', '\n']).is_empty();
                } else {
                    end += 1;
                }
            } else {
                end += char_at(rest).len_utf8();
            }
        }
        if !quote.multiline && !continued {
            self.mode = Mode::Code;
        }
        spans.push(end, Some(token));
    }

    fn code(&mut self, text: &str, spans: &mut Spans<'_>) {
        use Language::*;
        use Token::*;
        let lang = self.language;
        let first = char_at(text);
        if first.is_whitespace() {
            let n = text
                .find(|ch: char| !ch.is_whitespace())
                .unwrap_or(text.len());
            spans.push(n, None);
            return;
        }

        let before = &spans.text[..spans.at];
        let line_prefix = before.chars().next_back();

        if lang == Jsx
            && (self.tag.is_some() || self.jsx_depth > 0)
            && first == '{'
            && self.enter_expression(Resume::Jsx {
                depth: self.jsx_depth,
                tag: self.tag,
            })
        {
            self.tag = None;
            self.jsx_depth = 0;
            spans.push(1, Some(Operator));
            return;
        }

        if self.markup(text, before, spans) {
            return;
        }

        if let Some((open, close, nested)) = lang.comment(text) {
            self.mode = Mode::Comment {
                close,
                nested,
                depth: 1,
            };
            spans.push(open, Some(Comment));
            return;
        }
        if lang.line_comment(text, line_prefix) {
            spans.push(text.len(), Some(Comment));
            return;
        }

        if matches!(lang, Bash | Ruby)
            && !before.ends_with('<')
            && self.heredocs.len() < 16
            && let Some((width, heredoc)) = heredoc(lang, text)
        {
            self.heredocs.push_back(heredoc);
            spans.push(width, Some(String));
            return;
        }
        if lang == Yaml
            && let Some(scalar) = yaml_scalar(before, text)
        {
            self.scalar = Some(scalar);
            let width = text.find(char::is_whitespace).unwrap_or(text.len());
            spans.push(width, Some(Operator));
            return;
        }

        if let Some((width, quote)) = lang.quote(text) {
            let token = if quoted_key(lang, &text[width..], quote) {
                Name
            } else {
                String
            };
            self.mode = Mode::Quoted { quote, token };
            spans.push(width, Some(token));
            return;
        }

        self.expression_brace(first);
        if lang == Css {
            match first {
                '{' => self.css_block_depth += 1,
                '}' => self.css_block_depth = self.css_block_depth.saturating_sub(1),
                _ => {}
            }
        }

        if let Some(width) = lang.annotation(text) {
            spans.push(width, Some(Name));
            return;
        }
        if first.is_ascii_digit() {
            spans.push(number(text), Some(Number));
        } else if first == '.'
            && text.as_bytes().get(1).is_some_and(u8::is_ascii_digit)
            && !before.ends_with('.')
            && matches!(
                lang,
                C | Cpp
                    | Csharp
                    | Css
                    | Go
                    | Java
                    | Js
                    | Jsx
                    | Kotlin
                    | Lua
                    | Perl
                    | Php
                    | Python
                    | Ruby
                    | Scala
                    | Sql
                    | TypeScript
            )
        {
            spans.push(1 + number(&text[1..]), Some(Number));
        } else if identifier_start(first) {
            let width = text
                .find(|ch| {
                    !identifier_cont(ch)
                        && !(ch == '-' && (self.tag.is_some() || matches!(lang, Css | Toml | Yaml)))
                })
                .unwrap_or(text.len());
            let word = &text[..width];
            let after = text[width..].trim_start();
            let style = if self.tag.is_some()
                || (lang == Css && (self.css_block_depth == 0 || after.starts_with(':')))
                || (matches!(lang, Json | Yaml) && after.starts_with(':'))
                || (lang == Toml
                    && (after.starts_with('=') || before.trim_start().starts_with('[')))
            {
                Some(Name)
            } else if lang.keyword(word) {
                Some(Keyword)
            } else if !matches!(lang, Json | Markdown | Toml | Yaml)
                && (after.starts_with('(') || before.ends_with("fn "))
            {
                Some(Name)
            } else if lang.type_word(word) {
                Some(Type)
            } else {
                None
            };
            spans.push(width, style);
        } else if !matches!(lang, Markdown) && "+-*/=<>!&|^%~?".contains(first) {
            spans.push(first.len_utf8(), Some(Operator));
        } else if lang == Markdown
            && first == '#'
            && before.trim().is_empty()
            && text
                .trim_start_matches('#')
                .starts_with(char::is_whitespace)
        {
            spans.push(text.len(), Some(Keyword));
        } else {
            spans.push(first.len_utf8(), None);
        }
    }
    fn markup(&mut self, text: &str, before: &str, spans: &mut Spans<'_>) -> bool {
        let lang = self.language;
        if !matches!(lang, Language::Html | Language::Xml | Language::Jsx) {
            return false;
        }
        if let Some(tag) = self.tag {
            if let Some(end) = ["/>", "?>", ">"]
                .into_iter()
                .find(|end| text.starts_with(end))
            {
                if matches!(tag, Tag::Opening) && end == ">" {
                    self.jsx_depth += 1;
                } else if matches!(tag, Tag::Closing) {
                    self.jsx_depth = self.jsx_depth.saturating_sub(1);
                }
                self.tag = None;
                spans.push(end.len(), Some(Token::Operator));
                return true;
            }
            return false;
        }
        if text.starts_with("<!--") {
            return false;
        }
        let jsx = lang == Language::Jsx;
        let context = before.trim_end();
        let tag_position = !jsx
            || self.jsx_depth > 0
            || context.is_empty()
            || context.ends_with(['=', '(', '[', '{', ',', ':', '>', '?'])
            || context.split_whitespace().next_back() == Some("return");
        if text.starts_with('<') && tag_position {
            let closing = text.starts_with("</");
            let open = if closing || text.starts_with("<!") || text.starts_with("<?") {
                2
            } else {
                1
            };
            let tail = &text[open..];
            let width = if tail.chars().next().is_some_and(identifier_start) {
                tail.find(|ch| !identifier_cont(ch) && !matches!(ch, ':' | '-' | '.'))
                    .unwrap_or(tail.len())
            } else {
                0
            };
            if width > 0 || (jsx && tail.starts_with('>')) {
                self.tag = Some(if !jsx {
                    Tag::Markup
                } else if closing {
                    Tag::Closing
                } else {
                    Tag::Opening
                });
                spans.push(open, Some(Token::Operator));
                spans.push(width, Some(Token::Name));
                return true;
            }
        }
        if !jsx || self.jsx_depth > 0 {
            let width = char_at(text).len_utf8();
            let boundary = if jsx { &['<', '{'][..] } else { &['<'][..] };
            let next = text[width..]
                .find(boundary)
                .map_or(text.len(), |n| width + n);
            spans.push(next, None);
            return true;
        }
        false
    }
}

fn char_at(text: &str) -> char {
    text.chars().next().unwrap_or('\0')
}

fn identifier_start(ch: char) -> bool {
    ch.is_alphabetic() || matches!(ch, '_' | '$')
}

fn identifier_cont(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '$')
}

fn number(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut at = 1;
    while let Some(&b) = bytes.get(at) {
        if b.is_ascii_alphanumeric()
            || b == b'_'
            || (b == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
            || (matches!(b, b'+' | b'-')
                && matches!(bytes[at - 1], b'e' | b'E' | b'p' | b'P')
                && bytes.get(at + 1).is_some_and(u8::is_ascii_digit))
        {
            at += 1;
        } else {
            break;
        }
    }
    at
}

fn quoted_key(language: Language, text: &str, quote: Quote) -> bool {
    let separator = match language {
        Language::Json | Language::Yaml => ':',
        Language::Toml if !quote.multiline => '=',
        _ => return false,
    };
    let mut rest = text;
    while !rest.is_empty() {
        if let Some(width) = quote.end.width(rest) {
            let tail = &rest[width..];
            if quote.doubled && quote.end.width(tail).is_some() {
                rest = &tail[width..];
                continue;
            }
            return tail.trim_start().starts_with(separator);
        }
        let first = char_at(rest);
        rest = &rest[first.len_utf8()..];
        if first == '\\'
            && quote.escape
            && let Some(next) = rest.chars().next()
        {
            rest = &rest[next.len_utf8()..];
        }
    }
    false
}

fn heredoc(language: Language, text: &str) -> Option<(usize, Heredoc)> {
    let tail = text.strip_prefix("<<")?;
    let (tail, indent) = if let Some(tail) = tail.strip_prefix('-') {
        let indent = if language == Language::Bash {
            Indent::Tabs
        } else {
            Indent::Any
        };
        (tail, indent)
    } else if language == Language::Ruby
        && let Some(tail) = tail.strip_prefix('~')
    {
        (tail, Indent::Any)
    } else {
        (tail, Indent::Exact)
    };
    let tail = if language == Language::Bash {
        tail.trim_start_matches([' ', '\t'])
    } else {
        tail
    };
    let prefix = text.len() - tail.len();
    let (delimiter, width) = match tail.as_bytes().first()? {
        b'\'' | b'"' | b'`' => {
            let quote = char_at(tail);
            let end = tail[1..].find(quote)? + 1;
            (&tail[1..end], end + 1)
        }
        _ => {
            let token = tail.strip_prefix('\\').unwrap_or(tail);
            if !token.chars().next().is_some_and(identifier_start) {
                return None;
            }
            let end = token.find(|ch| !identifier_cont(ch)).unwrap_or(token.len());
            (&token[..end], tail.len() - token.len() + end)
        }
    };
    if delimiter.contains(['\r', '\n']) || delimiter.len() > 256 {
        return None;
    }
    Some((
        prefix + width,
        Heredoc {
            delimiter: Arc::from(delimiter),
            indent,
        },
    ))
}

fn yaml_scalar(before: &str, text: &str) -> Option<Scalar> {
    if !text.starts_with(['|', '>']) {
        return None;
    }
    let prefix = before.trim_end();
    if !prefix.is_empty() && !prefix.ends_with(':') && prefix != "-" {
        return None;
    }
    let (header, rest) = text.split_at(text.find(char::is_whitespace).unwrap_or(text.len()));
    let indicators = &header[1..];
    if indicators.len() > 2
        || !indicators
            .bytes()
            .all(|byte| matches!(byte, b'+' | b'-' | b'1'..=b'9'))
        || !(rest.trim().is_empty() || rest.trim_start().starts_with('#'))
    {
        return None;
    }
    let parent = before.bytes().take_while(|&byte| byte == b' ').count();
    let indentation = indicators
        .bytes()
        .find(|byte| byte.is_ascii_digit())
        .map(|byte| parent + usize::from(byte - b'0'));
    Some(Scalar {
        parent,
        indentation,
    })
}
