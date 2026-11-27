//! `senate --help`: the commands under headings, in the order a person
//! meets them, the same whether it is read in a terminal or through a pipe.
//! The words come from the doc comments in `main.rs`; a terminal adds
//! wrapping to its width and emphasis on the headings.

use std::fmt::Write as _;
use std::io::{Write, stdout};

use crate::terminal::Terminal;
use clap::{Command, CommandFactory, FromArgMatches, error::ErrorKind};

use super::Cli;

/// Every visible command, once, under its heading.
const GROUPS: &[(&str, &[&str])] = &[
    (
        "See what is happening",
        &[
            "status",
            "committees",
            "inbox",
            "thread",
            "memory",
            "stats",
            "observe",
            "card",
            "open",
            "scene",
        ],
    ),
    (
        "Speak as yourself",
        &["ask", "tell", "request", "reply", "take", "desk", "chat"],
    ),
    ("This worktree", &["join", "leave", "where"]),
    (
        "Senators and sessions",
        &[
            "spawn",
            "profiles",
            "attach",
            "session",
            "terminal",
            "alias",
            "senator",
            "secretary",
            "cleanup",
        ],
    ),
    ("Set up this machine", &["setup", "desk-worker"]),
];

const FOOTER: &str = "\
Run `senate help <command>` for a command's details and examples.

Start here:
  senate                          Is the daemon up, and who is connected?
  senate inbox                    What waits for you?
  senate ask claude \"...\" --wait  Ask an agent and stay for the answer

First time on this machine:
  senate setup init --local --person <you>  Only to use Senate alone
  senate setup install --yes                Register with Claude Code and Codex
  senate setup service install --yes        Keep the daemon running, from login
  senate join                               In each repository: pick a committee
";

pub(super) fn parse(terminal: Option<Terminal>) -> Cli {
    let mut command = command(terminal);
    let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
    if let Some((renamed, old, new)) = super::cli_legacy::rewrite(&args) {
        super::cli_legacy::point(&old, &new);
        args = renamed;
    }
    let parsed = command
        .try_get_matches_from_mut(args)
        .and_then(|mut matches| {
            Cli::from_arg_matches_mut(&mut matches).map_err(|error| error.format(&mut command))
        });
    match (parsed, terminal) {
        (Ok(cli), _) => cli,
        (Err(error), Some(terminal)) if error.kind() == ErrorKind::DisplayHelp => {
            let help = render(&error.to_string(), terminal);
            let _ = stdout().lock().write_all(help.as_bytes());
            std::process::exit(error.exit_code());
        }
        (Err(error), _) => error.exit(),
    }
}

fn command(terminal: Option<Terminal>) -> Command {
    let command = Cli::command();
    let template = format!(
        "{{about}}\n\n{{usage-heading}} {{usage}}\n\n{}Options:\n{{options}}\n\n{FOOTER}",
        grouped(&command)
    );
    let command = command.help_template(template);
    match terminal {
        Some(terminal) => command
            .term_width(terminal.columns())
            .next_line_help(terminal.columns() < 72),
        None => command,
    }
}

/// The command list of the top-level help: each group's heading, then its
/// commands with their one-line summaries in one column.
fn grouped(command: &Command) -> String {
    let width = GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|name| name.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (heading, names) in GROUPS {
        let _ = writeln!(out, "{heading}:");
        // A command this platform has not (`scene` off Unix) is left out.
        for name in names
            .iter()
            .filter(|name| command.find_subcommand(name).is_some())
        {
            let about = command
                .find_subcommand(name)
                .and_then(Command::get_about)
                .map(ToString::to_string)
                .unwrap_or_default();
            let _ = writeln!(out, "  {name:width$}  {about}");
        }
        out.push('\n');
    }
    out
}

fn render(help: &str, terminal: Terminal) -> String {
    let mut out = String::new();
    for line in help.lines() {
        let text = line.trim_start_matches(' ');
        if text.is_empty() {
            out.push('\n');
            continue;
        }
        let natural = line.len() - text.len();
        let indent = if terminal.columns() < 72 {
            natural.min(if text.starts_with('-') { 2 } else { 4 })
        } else {
            natural
        };
        let heading = indent == 0 && (text.ends_with(':') || text.starts_with("Usage:"));
        terminal.line_hanging(
            &mut out,
            text,
            indent,
            hang(text, indent, terminal.columns()),
            heading,
        );
    }
    out
}

/// Where the wrapped rest of a line starts. A row of two columns (a
/// command and its summary, an example and its note) continues under the
/// second one when that leaves room to read it.
fn hang(text: &str, indent: usize, columns: usize) -> usize {
    let second = text.find("  ").map(|gap| {
        let rest = &text[gap..];
        indent + gap + (rest.len() - rest.trim_start_matches(' ').len())
    });
    match second {
        Some(column) if indent > 0 && column + 24 <= columns => column,
        _ => indent + 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn visible(command: &Command) -> Vec<&Command> {
        command
            .get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .collect()
    }

    #[test]
    fn plumbing_takes_one_runtime_flag_and_the_old_ones_for_now() {
        use super::super::{Cmd, RuntimeFlag};
        use crate::session::Runtime;
        use clap::Parser as _;
        let hook = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Some(Cmd::Hook {
                runtime,
                codex,
                agy,
                ..
            }) => RuntimeFlag::chosen(runtime, codex, agy),
            _ => unreachable!(),
        };
        let start = ["senate", "hook", "session-start"];
        assert_eq!(hook(&start), None);
        for (flag, runtime) in [
            (&["--runtime", "claude"][..], Runtime::Claude),
            (&["--runtime", "codex"], Runtime::Codex),
            (&["--runtime", "agy"], Runtime::Agy),
            (&["--codex"], Runtime::Codex),
            (&["--agy"], Runtime::Agy),
        ] {
            assert_eq!(
                hook(&[&start[..], flag].concat()),
                Some(runtime),
                "{flag:?}"
            );
        }
        assert!(
            Cli::try_parse_from([&start[..], &["--runtime", "agy", "--codex"]].concat()).is_err()
        );
        let mcp = |args: &[&str]| match Cli::try_parse_from(args).unwrap().command {
            Some(Cmd::Mcp { runtime, agy, .. }) => RuntimeFlag::chosen(runtime, false, agy),
            _ => unreachable!(),
        };
        assert_eq!(
            mcp(&["senate", "mcp", "--runtime", "agy"]),
            Some(Runtime::Agy)
        );
        assert_eq!(
            mcp(&["senate", "mcp", "--runtime", "codex"]),
            Some(Runtime::Codex)
        );
        assert_eq!(mcp(&["senate", "mcp", "--agy"]), Some(Runtime::Agy));
        assert_eq!(mcp(&["senate", "mcp"]), None);
    }

    #[test]
    fn every_visible_command_is_listed_once_with_a_one_line_summary() {
        let command = Cli::command();
        let listed: Vec<&str> = GROUPS
            .iter()
            .flat_map(|(_, names)| names.iter().copied())
            .collect();
        for sub in visible(&command) {
            let name = sub.get_name();
            assert_eq!(
                listed.iter().filter(|n| **n == name).count(),
                1,
                "`{name}` needs exactly one place in GROUPS"
            );
            let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
            assert!(!about.is_empty(), "`{name}` has no summary");
            assert!(
                about.len() <= 62 && !about.contains(['{', '}', '\n']),
                "the summary of `{name}` must fit one line of an 80-column terminal: {about}"
            );
        }
        for name in listed.iter().filter(|name| cfg!(unix) || **name != "scene") {
            assert!(
                command.find_subcommand(name).is_some(),
                "GROUPS lists `{name}`, which is not a command"
            );
        }
    }

    fn walk<'a>(command: &'a Command, path: String, each: &mut impl FnMut(&str, &'a Command)) {
        each(&path, command);
        for sub in command.get_subcommands() {
            walk(sub, format!("{path} {}", sub.get_name()), each);
        }
    }

    #[test]
    fn every_argument_is_described_and_examples_fit_80_columns() {
        let command = Cli::command();
        walk(&command, "senate".to_owned(), &mut |path, command| {
            if command.is_hide_set() {
                return;
            }
            for arg in command.get_arguments() {
                assert!(
                    arg.is_hide_set() || arg.get_help().is_some(),
                    "`{path}`: `{}` has no help",
                    arg.get_id()
                );
            }
            let examples = command
                .get_after_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            for line in examples.lines() {
                assert!(
                    line.chars().count() <= 80,
                    "`{path}`: this example is wider than 80 columns: {line}"
                );
            }
        });
        for line in FOOTER.lines().chain(grouped(&command).lines()) {
            assert!(line.chars().count() <= 80, "wider than 80 columns: {line}");
        }
    }

    #[test]
    fn the_top_level_help_groups_the_commands_piped_or_in_a_terminal() {
        let piped = command(None).render_help().to_string();
        for needle in [
            "See what is happening:\n  status",
            "Speak as yourself:\n  ask",
            "Options:\n  -v, --verbose",
            "senate help <command>",
        ] {
            assert!(piped.contains(needle), "{needle:?} in:\n{piped}");
        }
        for hidden in ["session-host", "mcp", "hook", "debug", "watch-all"] {
            assert!(
                !piped.contains(&format!("\n  {hidden} ")),
                "{hidden} in {piped}"
            );
        }

        let narrow = Terminal::new(60, false);
        let text = render(&command(Some(narrow)).render_help().to_string(), narrow);
        assert!(
            text.lines().all(|line| line.chars().count() <= 60),
            "{text}"
        );
        // A wrapped summary continues under the summary column.
        let wrapped = render(
            "  free            Forget expired local data. Dry run without --yes, as ever it was\n",
            Terminal::new(60, false),
        );
        let lines: Vec<&str> = wrapped.lines().collect();
        assert!(lines.len() > 1, "{wrapped}");
        assert!(lines[1].starts_with("                  ") && !lines[1][18..].starts_with(' '));
    }

    #[test]
    fn a_command_help_carries_its_details_and_examples() {
        let mut command = command(None);
        let ask = command.find_subcommand_mut("ask").unwrap();
        let short = ask.render_help().to_string();
        let long = ask.render_long_help().to_string();
        assert!(short.starts_with("Ask a question\n"), "{short}");
        assert!(long.contains("`senator::…` address"), "{long}");
        assert!(long.contains("Examples:\n  senate ask claude"), "{long}");
    }
}
