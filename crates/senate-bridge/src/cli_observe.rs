use std::io::{self, IsTerminal};

use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use senate_core::CommitteeId;
use senate_observe::{AdminRequest, CreateGrant, GrantInfo, IssuedGrant, Scope};

use super::{Paths, daemon_call, print_json, runtime};
use crate::ipc::Request;

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
}

#[derive(Subcommand)]
enum Token {
    /// Issue a read-only token. The secret is printed once.
    Create {
        #[arg(long)]
        name: String,
        /// Comma-separated committee IDs or all (including future committees).
        #[arg(long)]
        committees: Option<String>,
        /// Request metadata or lines access.
        #[arg(long, value_enum, default_value = "metadata")]
        scope: GrantScope,
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
    Revoke { token_id: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum GrantScope {
    Metadata,
    Lines,
}

fn expiry(value: &str) -> Result<u16, String> {
    value
        .strip_suffix('d')
        .and_then(|days| days.parse::<u16>().ok())
        .filter(|days| (1..=90).contains(days))
        .ok_or_else(|| "expected a lifetime from 1d to 90d".to_owned())
}

pub(super) fn run(args: Observe) -> anyhow::Result<()> {
    let Action::Token { action } = args.action;
    let paths = Paths::from_env()?;
    match action {
        Token::Create {
            name,
            committees,
            scope,
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
            let command = AdminRequest::Create {
                grant: CreateGrant {
                    name,
                    committees,
                    all_committees,
                    scope: match scope {
                        GrantScope::Metadata => Scope::Metadata,
                        GrantScope::Lines => Scope::Lines,
                    },
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
