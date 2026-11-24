use std::io::{self, IsTerminal, Read};
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use senate_core::{CommitteeId, Seq, alias::AliasName};
use senate_observe::{
    AdminRequest, CreateGrant, GrantInfo, IssuedGrant, LinePolicy, Preview, Scope,
};

use super::{Paths, daemon_call, print_json, runtime};
use crate::ipc::{CardTarget, Request};

#[cfg(feature = "observe-cli")]
#[path = "cli_observe_record.rs"]
mod recording;

#[derive(Args)]
pub(super) struct Observe {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Manage observer grants from the person's terminal.
    Token {
        #[command(subcommand)]
        action: Token,
    },
    /// Publish or withdraw one reviewed message preview.
    Line {
        #[command(subcommand)]
        action: Line,
    },
    /// Record a scoped Observe session to a new owner-only file.
    #[cfg(feature = "observe-cli")]
    Record(recording::Record),
    /// Serve a recording or synthetic scene over the local Observe protocol.
    #[cfg(feature = "observe-cli")]
    Replay(recording::Replay),
}

#[derive(Subcommand)]
enum Line {
    /// Share the exact first line after reviewing it.
    Publish {
        /// Committee containing the message.
        committee: CommitteeId,
        /// Committed message sequence on that committee.
        seq: u64,
    },
    /// Withdraw a previously published preview.
    Withdraw {
        /// Committee containing the message.
        committee: CommitteeId,
        /// Committed message sequence on that committee.
        seq: u64,
    },
}

#[derive(Subcommand)]
enum Token {
    /// Issue a read-only token. The secret is printed once.
    #[command(after_help = "Examples:
  senate observe token create --name scene --committees senate
  senate observe token create --name town --committees all --topology")]
    Create {
        /// Display name for the consuming app.
        #[arg(long)]
        name: String,
        /// Comma-separated committee IDs or all (including future committees).
        #[arg(long)]
        committees: Option<String>,
        /// Request metadata or lines access.
        #[arg(long, value_enum, default_value = "metadata")]
        scope: GrantScope,
        /// Use automatic previews or publish individual lines.
        #[arg(long, value_enum)]
        lines: Option<Lines>,
        /// A browser origin authorized to use this token. Repeat for each origin.
        #[arg(long = "origin")]
        origins: Vec<String>,
        /// Include topology and the local profile.
        #[arg(long)]
        topology: bool,
        /// Grant lifetime in days, for example 30d.
        #[arg(long, value_parser = expiry)]
        expires_in: Option<u16>,
    },
    /// List active grants without any credentials.
    List,
    /// Revoke a grant by the public ID printed by list.
    Revoke {
        /// Public grant ID from token list.
        token_id: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum GrantScope {
    Metadata,
    Lines,
}

#[derive(Clone, Copy, ValueEnum)]
enum Lines {
    Auto,
    Published,
}

fn confirm(message: &str) -> anyhow::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("Confirm Observe sharing in your own terminal");
    }
    eprintln!("{message} Type yes to continue.");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim() != "yes" {
        bail!("Observe sharing was declined");
    }
    Ok(())
}

fn expiry(value: &str) -> Result<u16, String> {
    value
        .strip_suffix('d')
        .and_then(|days| days.parse::<u16>().ok())
        .filter(|days| (1..=90).contains(days))
        .ok_or_else(|| "expected a lifetime from 1d to 90d".to_owned())
}

pub(super) fn run(args: Observe) -> anyhow::Result<()> {
    let action = match args.action {
        Action::Token { action } => action,
        Action::Line { action } => return line(&Paths::from_env()?, action),
        #[cfg(feature = "observe-cli")]
        Action::Record(args) => return runtime(false)?.block_on(recording::record(args)),
        #[cfg(feature = "observe-cli")]
        Action::Replay(args) => return runtime(false)?.block_on(recording::replay(args)),
    };
    let paths = Paths::from_env()?;
    match action {
        Token::Create {
            name,
            committees,
            scope,
            lines,
            origins,
            topology,
            expires_in,
        } => {
            if !io::stdout().is_terminal() {
                bail!("Create observer tokens from a terminal so the secret is shown only there");
            }
            let all_committees = committees.as_deref() == Some("all");
            let committees = committees
                .filter(|_| !all_committees)
                .map(|value| {
                    value
                        .split(',')
                        .map(str::parse::<CommitteeId>)
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()
                .context("invalid committee selection")?;
            let policy = match (scope, lines) {
                (GrantScope::Metadata, None) => None,
                (GrantScope::Lines, Some(Lines::Auto)) => Some(LinePolicy::Auto),
                (GrantScope::Lines, Some(Lines::Published)) => Some(LinePolicy::Published),
                _ => bail!("Choose --lines auto or --lines published with --scope lines"),
            };
            if policy == Some(LinePolicy::Auto) {
                eprintln!(
                    "Automatic previews can disclose eligible first lines containing credentials, paths or commands."
                );
            }
            let command = AdminRequest::Create {
                grant: CreateGrant {
                    name,
                    committees,
                    all_committees,
                    scope: match scope {
                        GrantScope::Metadata => Scope::Metadata,
                        GrantScope::Lines => Scope::Lines,
                    },
                    lines: policy,
                    origins,
                    topology,
                    expires_in_days: expires_in,
                },
            };
            let issued: IssuedGrant = runtime(false)?
                .block_on(daemon_call(&paths, &Request::ObserveAdmin { command }))?;
            print_json(&issued)
        }
        Token::List => {
            let grants: Vec<GrantInfo> = runtime(false)?.block_on(daemon_call(
                &paths,
                &Request::ObserveAdmin {
                    command: AdminRequest::List,
                },
            ))?;
            print_json(&grants)
        }
        Token::Revoke { token_id } => {
            runtime(false)?.block_on(daemon_call::<()>(
                &paths,
                &Request::ObserveAdmin {
                    command: AdminRequest::Revoke { id: token_id },
                },
            ))?;
            println!("Observer grant revoked.");
            Ok(())
        }
    }
}

fn line(paths: &Paths, action: Line) -> anyhow::Result<()> {
    let runtime = runtime(false)?;
    let request = match action {
        Line::Publish { committee, seq } => {
            let seq = Seq(seq);
            let reviewed: Preview = runtime.block_on(daemon_call(
                paths,
                &Request::ObserveAdmin {
                    command: AdminRequest::Preview {
                        committee: committee.clone(),
                        seq,
                    },
                },
            ))?;
            confirm(&format!(
                "Publish this exact preview?\n{}",
                serde_json::to_string(&reviewed.line)?
            ))?;
            AdminRequest::Publish {
                committee,
                seq,
                reviewed,
            }
        }
        Line::Withdraw { committee, seq } => AdminRequest::Withdraw {
            committee,
            seq: Seq(seq),
        },
    };
    runtime.block_on(daemon_call::<()>(
        paths,
        &Request::ObserveAdmin { command: request },
    ))
}

#[derive(Args)]
pub(super) struct CardCommand {
    #[command(subcommand)]
    action: CardAction,
}

#[derive(Subcommand)]
enum CardAction {
    /// Replace a card with a JSON document.
    #[command(after_help = "Examples:
  senate card set --file my-card.json
  senate card set --committee senate --file workshop.json")]
    Set {
        /// Local JSON file, or - to read stdin.
        #[arg(long)]
        file: PathBuf,
        #[command(flatten)]
        subject: CardSelection,
    },
    /// Clear a card.
    Clear {
        #[command(flatten)]
        subject: CardSelection,
    },
}

#[derive(Args)]
struct CardSelection {
    /// Write a committee card as its chair.
    #[arg(long, conflicts_with = "senator")]
    committee: Option<CommitteeId>,
    /// Write the card of an agent you own, by committee::alias.
    #[arg(long)]
    senator: Option<AliasName>,
}

pub(super) fn run_card(args: CardCommand) -> anyhow::Result<()> {
    let (document, selection) = match args.action {
        CardAction::Set { file, subject } => {
            let mut document = String::new();
            let input: Box<dyn Read> = if file.as_os_str() == "-" {
                Box::new(io::stdin())
            } else {
                Box::new(std::fs::File::open(file)?)
            };
            input.take(4097).read_to_string(&mut document)?;
            (document, subject)
        }
        CardAction::Clear { subject } => ("null".into(), subject),
    };
    let target = if let Some(senator) = selection.senator {
        CardTarget::Member { senator }
    } else if let Some(committee) = selection.committee {
        CardTarget::Committee {
            committee: Some(committee),
        }
    } else {
        CardTarget::Own
    };
    let request = Request::SetCard {
        session: super::session_from_env().unwrap_or_default(),
        target,
        document,
    };
    let paths = Paths::from_env()?;
    runtime(false)?.block_on(daemon_call::<()>(&paths, &request))?;
    println!("Profile card updated.");
    Ok(())
}
