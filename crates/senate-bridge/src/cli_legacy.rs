//! The command names before the CLI was grouped (0.x), each kept working
//! for one minor release: the old argv is rewritten to the new one before
//! clap sees it, with one line on stderr naming the new command.
//!
//! Remove this module, its call in `cli_help::parse` and `join --duty` in
//! the release after the one that introduced the groups.

use std::ffi::OsString;

/// One renamed command: the old words after `senate`, and what they are now.
struct Rename {
    old: &'static [&'static str],
    new: &'static [&'static str],
}

const fn r(old: &'static [&'static str], new: &'static [&'static str]) -> Rename {
    Rename { old, new }
}

/// Longest first where one old name begins another.
const RENAMES: &[Rename] = &[
    r(&["watch-all"], &["session", "watch"]),
    r(&["watch"], &["inbox", "--follow"]),
    r(&["sessions", "stop"], &["session", "stop"]),
    r(&["sessions", "prune"], &["session", "prune"]),
    r(&["sessions"], &["session", "list"]),
    r(&["stop"], &["session", "stop"]),
    r(&["restore"], &["session", "restore"]),
    r(&["aliases"], &["senator", "list"]),
    r(&["alias", "rm"], &["senator", "unalias"]),
    r(&["pin"], &["join", "--alias"]),
    r(&["unpin"], &["leave", "--alias"]),
    r(&["seat", "join"], &["senator", "seat"]),
    r(&["seat", "unseat"], &["senator", "unseat"]),
    r(&["list"], &["where", "--all"]),
    r(&["init"], &["setup", "init"]),
    r(&["install"], &["setup", "install"]),
    r(&["service"], &["setup", "service"]),
    r(&["free"], &["setup", "clean"]),
    r(&["receipt"], &["debug", "receipt"]),
];

/// `secretary-team <action>`: the action's new words under `secretary`.
const SECRETARY: &[(&str, &[&str])] = &[
    ("members", &["list"]),
    ("appoint", &["appoint"]),
    ("remove", &["dismiss"]),
    ("list", &["work", "list"]),
    ("read", &["work", "show"]),
    ("create", &["work", "create"]),
    ("assign", &["work", "assign"]),
];

/// Says on stderr that `old` is now `new`.
pub(super) fn point(old: &str, new: &str) {
    eprintln!(
        "senate: `senate {old}` is now `senate {new}`; the old name goes in the next minor release."
    );
}

/// `args` (the program name first) with an old command name replaced by
/// its new one, and the pointer to print for it; `None` when nothing in
/// it is an old name.
pub(super) fn rewrite(args: &[OsString]) -> Option<(Vec<OsString>, String, String)> {
    // The global flag may come before the command.
    let at = 1 + args[1..]
        .iter()
        .take_while(|a| matches!(a.to_str(), Some("-v" | "--verbose")))
        .count();
    let words: Vec<&str> = args[at..]
        .iter()
        .map(|a| a.to_str().unwrap_or(""))
        .collect();
    let (old, new, rest): (Vec<&str>, Vec<&str>, Vec<OsString>) =
        if words.first() == Some(&"secretary-team") {
            secretary(&words, &args[at..])?
        } else if let Some(rename) = RENAMES
            .iter()
            .find(|r| words.len() >= r.old.len() && words[..r.old.len()] == *r.old)
        {
            let mut rest: Vec<OsString> = args[at + rename.old.len()..].to_vec();
            if rename.old[0] == "seat" {
                // `--senator X` is the positional now.
                rest.retain(|a| a != "--senator");
                for a in &mut rest {
                    if let Some(v) = a.to_str().and_then(|a| a.strip_prefix("--senator=")) {
                        *a = v.into();
                    }
                }
            }
            (rename.old.to_vec(), rename.new.to_vec(), rest)
        } else if words.first() == Some(&"join") && joins_another_session(&words) {
            (
                vec!["join", "--alias", "…", "--session", "…"],
                vec!["senator", "alias"],
                alias_another_session(&args[at + 1..]),
            )
        } else if words.first() == Some(&"join") && words.iter().any(|w| is_flag(w, "--senator")) {
            // Renamed in place: `--alias` stands where `--senator` was.
            let rest = args[at + 1..]
                .iter()
                .map(|a| match a.to_str() {
                    Some("--senator") => "--alias".into(),
                    Some(v) if v.starts_with("--senator=") => {
                        v.replacen("--senator", "--alias", 1).into()
                    }
                    _ => a.clone(),
                })
                .collect();
            (vec!["join", "--senator"], vec!["join", "--alias"], rest)
        } else {
            return None;
        };
    let mut out: Vec<OsString> = args[..at].to_vec();
    // A flag renamed in place leaves only the command word in front.
    let head: &[&str] = if old == ["join", "--senator"] {
        &["join"]
    } else {
        &new
    };
    out.extend(
        head.iter()
            .filter(|w| **w != "…")
            .map(|w| OsString::from(*w)),
    );
    out.extend(rest);
    Some((out, old.join(" "), new.join(" ")))
}

fn is_flag(word: &str, flag: &str) -> bool {
    word == flag || word.strip_prefix(flag).is_some_and(|v| v.starts_with('='))
}

fn joins_another_session(words: &[&str]) -> bool {
    words.iter().any(|w| is_flag(w, "--session"))
        && words
            .iter()
            .any(|w| is_flag(w, "--alias") || is_flag(w, "--senator"))
}

/// Old `join [committee] --alias A --session ID [--duty D]` as the
/// arguments of `senator alias A [--committee C] --session ID`. A duty is
/// dropped with a word: it is `senator duty` now.
fn alias_another_session(args: &[OsString]) -> Vec<OsString> {
    let mut alias = None;
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let text = arg.to_str().unwrap_or("");
        let (flag, inline) = match text.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(OsString::from(value))),
            _ => (text, None),
        };
        let mut value = || inline.clone().or_else(|| it.next().cloned());
        match flag {
            "--alias" | "--senator" => alias = value(),
            "--session" => {
                out.push("--session".into());
                out.extend(value());
            }
            "--duty" => {
                let _ = value();
                point("join --duty", "senator duty");
            }
            _ if text.starts_with('-') => out.push(arg.clone()),
            _ => {
                out.push("--committee".into());
                out.push(arg.clone());
            }
        }
    }
    alias.into_iter().chain(out).collect()
}

fn secretary<'a>(
    words: &[&'a str],
    args: &[OsString],
) -> Option<(Vec<&'a str>, Vec<&'static str>, Vec<OsString>)> {
    // `--for P` may come before the action.
    let mut i = 1;
    while i < words.len() && words[i].starts_with('-') {
        i += if words[i] == "--for" { 2 } else { 1 };
    }
    let action = words.get(i).copied();
    let found = action.and_then(|a| SECRETARY.iter().find(|(old, _)| *old == a));
    let mut new = vec!["secretary"];
    let mut rest: Vec<OsString> = args[1..i.min(args.len())].to_vec();
    let mut old = vec!["secretary-team"];
    if let Some((name, words)) = found {
        old.push(name);
        new.extend(words.iter().copied());
        rest.extend(args[i + 1..].iter().cloned());
    } else {
        rest.extend(args[i.min(args.len())..].iter().cloned());
    }
    Some((old, new, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn rewritten(old: &str) -> Option<(String, String, String)> {
        let args: Vec<OsString> = std::iter::once("senate")
            .chain(old.split(' '))
            .map(OsString::from)
            .collect();
        rewrite(&args).map(|(args, old, new)| {
            let args: Vec<&str> = args[1..].iter().map(|a| a.to_str().unwrap()).collect();
            (args.join(" "), old, new)
        })
    }

    /// Each old command line, what it becomes, and the pointer's two names.
    const CASES: &[(&str, &str, &str, &str)] = &[
        (
            "watch --global --json",
            "inbox --follow --global --json",
            "watch",
            "inbox --follow",
        ),
        ("watch-all", "session watch", "watch-all", "session watch"),
        ("sessions", "session list", "sessions", "session list"),
        (
            "sessions stop --idle",
            "session stop --idle",
            "sessions stop",
            "session stop",
        ),
        (
            "sessions prune",
            "session prune",
            "sessions prune",
            "session prune",
        ),
        (
            "stop reviewer --force",
            "session stop reviewer --force",
            "stop",
            "session stop",
        ),
        (
            "restore reviewer --model opus",
            "session restore reviewer --model opus",
            "restore",
            "session restore",
        ),
        ("aliases", "senator list", "aliases", "senator list"),
        (
            "alias rm nous::main",
            "senator unalias nous::main",
            "alias rm",
            "senator unalias",
        ),
        (
            "pin reviewer",
            "join --alias reviewer",
            "pin",
            "join --alias",
        ),
        ("unpin", "leave --alias", "unpin", "leave --alias"),
        (
            "seat join --committee c --senator codex",
            "senator seat --committee c codex",
            "seat join",
            "senator seat",
        ),
        (
            "seat unseat --committee c --senator=codex --json",
            "senator unseat --committee c codex --json",
            "seat unseat",
            "senator unseat",
        ),
        ("list --json", "where --all --json", "list", "where --all"),
        (
            "init --local --person alice",
            "setup init --local --person alice",
            "init",
            "setup init",
        ),
        (
            "install --yes",
            "setup install --yes",
            "install",
            "setup install",
        ),
        (
            "service status",
            "setup service status",
            "service",
            "setup service",
        ),
        ("free --yes", "setup clean --yes", "free", "setup clean"),
        (
            "receipt 12 --json",
            "debug receipt 12 --json",
            "receipt",
            "debug receipt",
        ),
        (
            "secretary-team members",
            "secretary list",
            "secretary-team members",
            "secretary list",
        ),
        (
            "secretary-team --for bob remove --senator x",
            "secretary dismiss --for bob --senator x",
            "secretary-team remove",
            "secretary dismiss",
        ),
        (
            "secretary-team appoint --session s",
            "secretary appoint --session s",
            "secretary-team appoint",
            "secretary appoint",
        ),
        (
            "secretary-team list",
            "secretary work list",
            "secretary-team list",
            "secretary work list",
        ),
        (
            "secretary-team read 01HZY0000000000000000000AA",
            "secretary work show 01HZY0000000000000000000AA",
            "secretary-team read",
            "secretary work show",
        ),
        (
            "secretary-team create title",
            "secretary work create title",
            "secretary-team create",
            "secretary work create",
        ),
        (
            "secretary-team assign 01HZY0000000000000000000AA --to x",
            "secretary work assign 01HZY0000000000000000000AA --to x",
            "secretary-team assign",
            "secretary work assign",
        ),
        (
            "join refund --alias reviewer --session abc",
            "senator alias reviewer --committee refund --session abc",
            "join --alias … --session …",
            "senator alias",
        ),
        (
            "join --senator=reviewer --session=abc",
            "senator alias reviewer --session abc",
            "join --alias … --session …",
            "senator alias",
        ),
        (
            "join --senator reviewer",
            "join --alias reviewer",
            "join --senator",
            "join --alias",
        ),
        (
            "join refund --senator x",
            "join refund --alias x",
            "join --senator",
            "join --alias",
        ),
        ("-v sessions", "-v session list", "sessions", "session list"),
    ];

    #[test]
    fn every_old_name_becomes_its_new_command_and_parses() {
        for (old, new, from, to) in CASES {
            let (args, said_old, said_new) =
                rewritten(old).unwrap_or_else(|| panic!("`{old}` is not rewritten"));
            assert_eq!(&args, new, "`{old}`");
            assert_eq!((&said_old[..], &said_new[..]), (*from, *to), "`{old}`");
            let argv = std::iter::once("senate").chain(args.split(' '));
            if let Err(error) = super::super::Cli::try_parse_from(argv) {
                panic!("`{old}` -> `{args}` does not parse: {error}");
            }
        }
    }

    #[test]
    fn every_old_name_has_a_case() {
        for rename in RENAMES {
            let old = rename.old.join(" ");
            assert!(
                CASES.iter().any(|(_, _, from, _)| *from == old),
                "no case for `{old}`"
            );
        }
        for (name, _) in SECRETARY {
            let old = format!("secretary-team {name}");
            assert!(
                CASES.iter().any(|(_, _, from, _)| *from == old),
                "no case for `{old}`"
            );
        }
    }

    #[test]
    fn the_new_names_are_left_alone() {
        for new in [
            "status",
            "inbox --follow",
            "session list",
            "senator list",
            "where --all",
            "join refund --alias reviewer",
            "leave --alias",
            "setup install",
            "secretary work list",
            "hook session-start",
            "mcp",
        ] {
            assert_eq!(rewritten(new), None, "`{new}`");
        }
    }
}
