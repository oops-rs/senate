//! Fence aliases and lexical rules for the supported languages.

use super::{End, Quote, char_at, identifier_cont, identifier_start};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Language {
    Bash,
    C,
    Cpp,
    Csharp,
    Css,
    Diff,
    Docker,
    Go,
    Html,
    Java,
    Js,
    Json,
    Jsx,
    Kotlin,
    Lua,
    Make,
    Markdown,
    Perl,
    Php,
    Python,
    Ruby,
    Rust,
    Scala,
    Sql,
    Swift,
    Toml,
    TypeScript,
    Xml,
    Yaml,
}

pub(crate) fn syntax(language: Option<&str>) -> Option<Language> {
    use Language::*;
    Some(match language?.to_ascii_lowercase().as_str() {
        "bash" | "sh" | "shell" | "zsh" => Bash,
        "c" => C,
        "cs" | "csharp" | "c#" => Csharp,
        "cpp" | "c++" | "cxx" => Cpp,
        "css" => Css,
        "diff" | "patch" => Diff,
        "dockerfile" | "docker" => Docker,
        "go" | "golang" => Go,
        "html" => Html,
        "java" => Java,
        "js" | "javascript" => Js,
        "json" => Json,
        "jsx" | "tsx" => Jsx,
        "kt" | "kotlin" => Kotlin,
        "lua" => Lua,
        "make" | "makefile" => Make,
        "md" | "markdown" => Markdown,
        "pl" | "perl" => Perl,
        "php" => Php,
        "py" | "python" => Python,
        "rb" | "ruby" => Ruby,
        "rs" | "rust" => Rust,
        "scala" => Scala,
        "sql" => Sql,
        "swift" => Swift,
        "toml" => Toml,
        "ts" | "typescript" => TypeScript,
        "xml" => Xml,
        "yml" | "yaml" => Yaml,
        _ => return None,
    })
}

fn lua_bracket(text: &str) -> Option<(usize, End)> {
    let tail = text.strip_prefix('[')?.as_bytes();
    let equals = tail.iter().take_while(|&&byte| byte == b'=').count();
    (tail.get(equals) == Some(&b'['))
        .then_some((equals + 2, End::LuaBracket(u16::try_from(equals).ok()?)))
}

impl Language {
    pub(super) fn comment(self, s: &str) -> Option<(usize, End, bool)> {
        use Language::*;
        let slash = matches!(
            self,
            C | Cpp
                | Csharp
                | Css
                | Go
                | Java
                | Js
                | Jsx
                | Kotlin
                | Php
                | Rust
                | Scala
                | Sql
                | Swift
                | TypeScript
        );
        if slash && s.starts_with("/*") {
            Some((
                2,
                End::Literal("*/"),
                matches!(self, Rust | Swift | Kotlin | Scala),
            ))
        } else if matches!(self, Html | Xml | Markdown | Jsx) && s.starts_with("<!--") {
            Some((4, End::Literal("-->"), false))
        } else if self == Lua
            && let Some((n, close)) = s.strip_prefix("--").and_then(lua_bracket)
        {
            Some((2 + n, close, false))
        } else {
            None
        }
    }

    pub(super) fn line_comment(self, s: &str, previous: Option<char>) -> bool {
        use Language::*;
        match self {
            C | Cpp | Csharp | Go | Java | Js | Jsx | Kotlin | Rust | Scala | Swift
            | TypeScript => s.starts_with("//"),
            Bash | Docker | Make => s.starts_with('#') && previous.is_none_or(char::is_whitespace),
            Perl | Python | Ruby | Toml | Yaml => s.starts_with('#'),
            Php => s.starts_with("//") || s.starts_with('#'),
            Lua | Sql => s.starts_with("--"),
            _ => false,
        }
    }

    pub(super) fn quote(self, s: &str) -> Option<(usize, Quote)> {
        use Language::*;
        let mut prefix = 0;
        if self == Cpp {
            for prefix in ["u8R\"", "uR\"", "UR\"", "LR\"", "R\""] {
                if let Some(tail) = s.strip_prefix(prefix) {
                    let open = tail.bytes().take(17).position(|byte| byte == b'(')?;
                    let mut delimiter = [0; 16];
                    if open > delimiter.len()
                        || tail[..open]
                            .bytes()
                            .any(|byte| !byte.is_ascii_graphic() || matches!(byte, b')' | b'\\'))
                    {
                        return None;
                    }
                    delimiter[..open].copy_from_slice(&tail.as_bytes()[..open]);
                    return Some((
                        prefix.len() + open + 1,
                        Quote {
                            end: End::CppRaw {
                                delimiter,
                                len: u8::try_from(open).ok()?,
                            },
                            escape: false,
                            multiline: true,
                            template: false,
                            doubled: false,
                        },
                    ));
                }
            }
        }
        if self == Csharp {
            for prefix in ["$@\"", "@$\"", "@\""] {
                if s.starts_with(prefix) {
                    return Some((
                        prefix.len(),
                        Quote {
                            end: End::Literal("\""),
                            escape: false,
                            multiline: true,
                            template: false,
                            doubled: true,
                        },
                    ));
                }
            }
        }
        if self == Rust {
            for leading in ["br", "cr", "r"] {
                if let Some(rest) = s.strip_prefix(leading) {
                    let n = rest.bytes().take_while(|&b| b == b'#').count();
                    if rest.as_bytes().get(n) == Some(&b'"')
                        && let Ok(n) = u8::try_from(n)
                    {
                        return Some((
                            leading.len() + 1 + usize::from(n),
                            Quote {
                                end: End::HashQuote {
                                    quote: "\"",
                                    hashes: n,
                                },
                                escape: false,
                                multiline: true,
                                template: false,
                                doubled: false,
                            },
                        ));
                    }
                }
            }
            // Rust lifetimes and labels start with a single quote too.
            if let Some(rest) = s.strip_prefix('\'')
                && !rest.starts_with('\\')
            {
                let width = char_at(rest).len_utf8();
                if rest.as_bytes().get(width) != Some(&b'\'') {
                    return None;
                }
            }
        } else if self == Swift && s.starts_with('#') {
            let hashes = s.bytes().take_while(|&byte| byte == b'#').count();
            let tail = &s[hashes..];
            let (quote, multiline) = if tail.starts_with("\"\"\"") {
                ("\"\"\"", true)
            } else if tail.starts_with('"') {
                ("\"", false)
            } else {
                return None;
            };
            return Some((
                hashes + quote.len(),
                Quote {
                    end: End::HashQuote {
                        quote,
                        hashes: u8::try_from(hashes).ok()?,
                    },
                    escape: false,
                    multiline,
                    template: false,
                    doubled: false,
                },
            ));
        } else if self == Lua
            && let Some((width, end)) = lua_bracket(s)
        {
            return Some((
                width,
                Quote {
                    end,
                    escape: false,
                    multiline: true,
                    template: false,
                    doubled: false,
                },
            ));
        } else if self == Python {
            prefix = s
                .bytes()
                .take(2)
                .take_while(|b| matches!(b.to_ascii_lowercase(), b'r' | b'b' | b'f' | b'u'))
                .count();
        }
        let tail = &s[prefix..];
        let first = *tail.as_bytes().first()?;
        if matches!(self, Html | Xml | Json)
            && first != b'"'
            && !(matches!(self, Html | Xml) && first == b'\'')
        {
            return None;
        }
        if (self == Markdown && first != b'`') || (self == Swift && first == b'\'') {
            return None;
        }
        if first == b'`' && !matches!(self, Bash | Go | Js | Jsx | Markdown | Sql | TypeScript) {
            return None;
        }
        if !matches!(first, b'"' | b'\'' | b'`') {
            return None;
        }
        let triple = matches!(self, Python | Toml | Swift | Kotlin | Java | Scala)
            && (tail.starts_with("\"\"\"") || tail.starts_with("'''"));
        let marker = match (first, triple) {
            (b'"', true) => "\"\"\"",
            (b'\'', true) => "'''",
            (b'"', _) => "\"",
            (b'\'', _) => "'",
            _ => "`",
        };
        Some((
            prefix + marker.len(),
            Quote {
                end: End::Literal(marker),
                escape: !(matches!(self, Html | Xml | Sql)
                    || triple && matches!(self, Kotlin | Scala)
                    || first == b'\'' && matches!(self, Bash | Toml | Yaml)
                    || first == b'`' && self == Go),
                multiline: triple
                    || matches!(self, Bash | Html | Xml | Ruby | Perl | Php | Sql)
                    || first == b'"' && self == Rust
                    || first == b'`' && matches!(self, Go | Js | Jsx | TypeScript),
                template: first == b'`' && matches!(self, Js | Jsx | TypeScript),
                doubled: self == Sql || self == Yaml && first == b'\'',
            },
        ))
    }

    pub(super) fn annotation(self, text: &str) -> Option<usize> {
        use Language::*;
        let prefix = match self {
            Rust if text.starts_with("#![") => 3,
            Rust if text.starts_with("#[") => 2,
            Swift | Python | Kotlin | Java | Scala | Js | Jsx | TypeScript
                if text.starts_with('@') =>
            {
                1
            }
            _ => return None,
        };
        let tail = &text[prefix..];
        if !tail.chars().next().is_some_and(identifier_start) {
            return None;
        }
        Some(prefix + tail.find(|ch| !identifier_cont(ch)).unwrap_or(tail.len()))
    }

    pub(super) fn keyword(self, word: &str) -> bool {
        use Language::*;
        let rules = match self {
            Rust => {
                "as async await break const continue crate dyn else enum extern false fn for \
                if impl in let loop match mod move mut pub ref return self Self static \
                struct super trait true type unsafe use where while"
            }
            Swift => {
                "actor any as async await break case catch class continue defer deinit do \
                else enum extension fallthrough false for func guard if import in init inout \
                is let nil nonisolated open operator private protocol public repeat return \
                self Self some static struct subscript super switch throw throws true try \
                typealias var where while"
            }
            Js | Jsx | TypeScript => {
                "abstract as async await break case catch class const continue declare \
                default delete do else enum export extends false finally for from function \
                get if implements import in infer instanceof interface keyof let namespace \
                never new null of private protected public readonly return satisfies set \
                static super switch this throw true try type typeof undefined var void while \
                with yield"
            }
            Python => {
                "and as assert async await break case class continue def del elif else except \
                False finally for from global if import in is lambda match None nonlocal not \
                or pass raise return True try while with yield"
            }
            Go => {
                "break case chan const continue default defer else fallthrough false for func \
                go goto if import interface map nil package range return select struct \
                switch true type var"
            }
            C | Cpp => {
                "alignas auto break case class const constexpr continue default define delete \
                do else enum explicit extern false for goto if include inline namespace new \
                nullptr operator private protected public return sizeof static struct switch \
                template this throw true typedef typename union using virtual volatile while"
            }
            Java | Csharp => {
                "abstract as assert async await break case catch checked class const continue \
                default do else enum extends false final finally for foreach if implements \
                import in interface is namespace new null override package private protected \
                public return sealed static super switch synchronized this throw throws true \
                try using var virtual void while yield"
            }
            Kotlin => {
                "as break by class companion const continue data do else enum false for fun \
                if import in interface is null object override package private protected \
                public return sealed super suspend this throw true try typealias val var \
                when where while"
            }
            Scala => {
                "abstract case catch class def do else enum extends false final finally for \
                given if implicit import lazy match new null object override package private \
                protected return sealed super then this throw trait true try type using val \
                var while with yield"
            }
            Ruby => {
                "alias and begin break case class def do else elsif end ensure false for if \
                in module next nil not or redo rescue return self super then true undef \
                unless until when while yield"
            }
            Bash => {
                "case do done elif else esac export fi for function if in local readonly \
                return select then time until while"
            }
            Json => "false null true",
            Yaml => "false null true yes no on off",
            Toml => "false true",
            Docker => {
                "add arg cmd copy entrypoint env expose from healthcheck label maintainer \
                onbuild run shell stopsignal user volume workdir"
            }
            Sql => {
                "add alter and as asc between by case create cross database default delete \
                desc distinct drop else end exists false from full group having in inner \
                insert into is join left like limit not null on or order outer primary \
                references return returning right select set table then true union unique \
                update values when where with"
            }
            Lua => {
                "and break do else elseif end false for function goto if in local nil not or \
                repeat return then true until while"
            }
            Perl | Php => {
                "and as break case class const continue default die do echo else elseif elsif \
                false for foreach function if include last local my new next null or package \
                print require return static sub switch true undef unless until use while"
            }
            Css => "important inherit initial unset",
            Make => {
                "define else endef endif export ifdef ifeq ifndef ifneq include override \
                private undefine unexport"
            }
            Diff | Html | Markdown | Xml => "",
        };
        if matches!(self, Sql | Docker | Yaml) {
            rules
                .split_ascii_whitespace()
                .any(|candidate| candidate.eq_ignore_ascii_case(word))
        } else {
            rules
                .split_ascii_whitespace()
                .any(|candidate| candidate == word)
        }
    }

    pub(super) fn type_word(self, word: &str) -> bool {
        use Language::*;
        if matches!(
            self,
            Bash | Css
                | Diff
                | Docker
                | Html
                | Json
                | Lua
                | Make
                | Markdown
                | Perl
                | Sql
                | Toml
                | Xml
                | Yaml
        ) {
            return false;
        }
        (self != Go && word.starts_with(char::is_uppercase) && word.chars().any(char::is_lowercase))
            || matches!(
                word,
                "bool"
                    | "byte"
                    | "char"
                    | "double"
                    | "float"
                    | "int"
                    | "long"
                    | "number"
                    | "short"
                    | "str"
                    | "string"
                    | "usize"
                    | "isize"
                    | "void"
                    | "u8"
                    | "u16"
                    | "u32"
                    | "u64"
                    | "u128"
                    | "i8"
                    | "i16"
                    | "i32"
                    | "i64"
                    | "i128"
                    | "f32"
                    | "f64"
            )
    }
}
