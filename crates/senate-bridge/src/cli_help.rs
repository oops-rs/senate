use std::io::{Write, stdout};

use clap::{Command, CommandFactory, FromArgMatches, Parser, error::ErrorKind};
use senate_bridge::terminal::Terminal;

use super::Cli;

const FIRST_COMMANDS: [&str; 5] = ["status", "where", "inbox", "watch", "thread"];
const SUMMARIES: &[(&str, &str)] = &[
    ("where", "Show this directory's committee."),
    ("inbox", "Read unread messages and open threads."),
    ("watch", "Follow new messages."),
    ("thread", "Read the messages in one thread."),
    ("init", "Create a profile for a local Senate."),
    ("join", "Bind this worktree or set a session alias."),
    ("aliases", "List senator aliases."),
    ("leave", "Remove this worktree's committee binding."),
    ("seat", "Change a senator's committee membership."),
    ("pin", "Set the worktree's suggested agent name."),
    ("unpin", "Clear the worktree's suggested agent name."),
    ("list", "List saved worktree bindings."),
    ("free", "List expired local data (dry run)."),
    ("ask", "Send a question as yourself."),
    ("tell", "Send a note as yourself."),
    ("request", "Ask an agent to do work."),
    ("reply", "Reply in an existing thread."),
    ("take", "Take over a thread as yourself."),
    ("mcp", "Run the MCP server over stdio."),
    ("hook", "Run a coding session's hook."),
    ("install", "Set up MCP and hooks for coding agents."),
    ("service", "Manage the background daemon."),
    ("codex-smoke", "Check the Codex app-server connection."),
];

pub(super) fn parse(terminal: Option<Terminal>) -> Cli {
    let Some(terminal) = terminal else {
        return Cli::parse();
    };
    let mut command = command(terminal);
    let parsed = command
        .try_get_matches_from_mut(std::env::args_os())
        .and_then(|mut matches| {
            Cli::from_arg_matches_mut(&mut matches).map_err(|error| error.format(&mut command))
        });
    match parsed {
        Ok(cli) => cli,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            let help = render(&error.to_string(), terminal);
            let _ = stdout().lock().write_all(help.as_bytes());
            std::process::exit(error.exit_code());
        }
        Err(error) => error.exit(),
    }
}

fn command(terminal: Terminal) -> Command {
    Cli::command()
        .about("Read and reply on your Senate from the terminal.")
        .term_width(terminal.columns())
        .next_line_help(terminal.columns() < 72)
        .after_help("Examples:\n  senate status\n  senate inbox\n  senate help status")
        .mut_subcommands(orient)
}

fn orient(mut command: Command) -> Command {
    let order = FIRST_COMMANDS
        .iter()
        .position(|name| *name == command.get_name())
        .unwrap_or_else(|| {
            command
                .get_display_order()
                .saturating_add(FIRST_COMMANDS.len())
        });
    command = command.display_order(order);
    if command.get_name() == "status" {
        return command
            .about("Show the Senate, committee, chair, sessions, and delivery.")
            .mut_arg("full_ids", |arg| arg.help("Show complete session and senator IDs"))
            .after_help(
                "Examples:\n  senate status\n  senate status --global\n  senate status --committee commons",
            );
    }
    if let Some((_, summary)) = SUMMARIES
        .iter()
        .find(|(name, _)| *name == command.get_name())
    {
        if command.get_long_about().is_none()
            && let Some(about) = command.get_about().cloned()
        {
            command = command.long_about(about);
        }
        command = command.about(*summary);
    }
    command
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
        terminal.line(&mut out, text, indent, heading);
    }
    out
}
