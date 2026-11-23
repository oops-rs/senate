use crate::{
    Paths,
    ipc::{DeskReply, Request, SessionSelector},
};
use clap::{Args, Subcommand};
use senate_core::api::Secretaries;
use senate_core::secretary_work::{
    SecretaryWorkOp, SecretaryWorkReply, SecretaryWorkUpdate, WorkThread,
};
use senate_core::{CommitteeId, EventId, SenatorName, Seq};

use super::{daemon_call, parse_seat_name, print_json, runtime};

#[derive(Args)]
pub(super) struct Secretary {
    /// Your Senate handle. Defaults to the signed-in person.
    #[arg(long = "for", global = true, value_parser = parse_seat_name)]
    principal: Option<SenatorName>,
    #[command(subcommand)]
    action: Action,
}

#[derive(Args)]
struct Membership {
    /// Full ID of a registered native coding session.
    #[arg(long)]
    session: String,
}

#[derive(Args)]
struct Dismissal {
    /// Full ID of a session registered on this machine.
    #[arg(long, required_unless_present = "senator", conflicts_with = "senator")]
    session: Option<String>,
    /// A stable senator ID from the team's membership list.
    #[arg(long, value_parser = parse_seat_name)]
    senator: Option<SenatorName>,
}

#[derive(Subcommand)]
enum Action {
    /// Ask this machine's default secretary to do something, starting it
    /// first if it is not running.
    ///
    /// The default secretary is a coding agent on the Speaker's desk,
    /// `office::secretary`, working in a directory Senate owns. Asked to
    /// carry work across projects, it opens a committee per project, starts
    /// each chair, and reports back. The first ask starts it (with
    /// `[local] secretary_profile`, or the first runtime configured); a
    /// paused or ended one is brought back; one stopped on purpose is not.
    #[command(after_help = "Examples:
  senate secretary ask 'Open a committee for the bug in ~/dev/shop'
  senate secretary ask - < plan.md       The request, read from stdin
  senate thread 12 --committee office    Read its answer")]
    Ask {
        /// What to ask; `-` reads it from stdin.
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// List the secretary team's appointed sessions by their stable senator
    /// IDs.
    List,
    /// Appoint a specific session to the secretary team.
    Appoint(Membership),
    /// Dismiss a specific session from the secretary team.
    Dismiss(Dismissal),
    /// Show or set the Speaker's desk workers: its secretaries, in order.
    ///
    /// The desk is everything addressed to the Speaker. Its workers are the
    /// Speaker itself when it is an agent, then the Speaker's secretaries in
    /// this order; the first one present acts. This is not the secretary
    /// team that `list`, `appoint` and `dismiss` manage: the team shares
    /// your work items, while the desk workers answer for the Speaker.
    #[command(after_help = "Examples:
  senate secretary desk                     Show the Speaker's desk workers
  senate secretary desk set claude-1 codex  claude-1 first, then codex
  senate secretary desk set                 None: the desk waits on the Speaker")]
    Desk {
        #[command(subcommand)]
        command: Option<DeskCmd>,
    },
    /// List, read, record and assign the team's shared work items.
    Work {
        #[command(subcommand)]
        command: Work,
    },
}

#[derive(Subcommand)]
enum DeskCmd {
    /// Replace the desk workers with these senators, in this order. Name
    /// none to clear them.
    Set {
        /// Senators (agents of this Senate), first acting first.
        #[arg(value_parser = parse_seat_name)]
        senators: Vec<SenatorName>,
    },
}

#[derive(Subcommand)]
enum Work {
    /// List the team's work items.
    List {
        /// Continue the listing after this item ID.
        #[arg(long)]
        after: Option<EventId>,
    },
    /// Show an item and its history.
    Show {
        /// The item's ID, as `work list` prints it.
        item: EventId,
        /// Skip this many history entries.
        #[arg(long, default_value_t)]
        offset: usize,
    },
    /// Record a new work item and optionally link a committee thread.
    Create {
        /// What the item is about.
        title: String,
        /// The committee of the thread to link (with --thread).
        #[arg(long, requires = "thread")]
        committee: Option<CommitteeId>,
        /// The root seq of the thread to link (with --committee).
        #[arg(long, requires = "committee")]
        thread: Option<Seq>,
        /// Reuse the ID to retry the same creation.
        #[arg(long)]
        id: Option<EventId>,
    },
    /// Assign an item and notify its new owner.
    Assign {
        /// The item's ID, as `work list` prints it.
        item: EventId,
        /// The new owner: a senator ID from `senate secretary list`.
        #[arg(long, value_parser = parse_seat_name)]
        to: SenatorName,
        /// Reuse the ID to retry the same assignment.
        #[arg(long)]
        id: Option<EventId>,
    },
}

pub(super) fn run(args: Secretary) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let principal = args.principal;
    let operation = match args.action {
        Action::Ask { text } => {
            if principal.is_some() {
                anyhow::bail!("--for does not apply to `secretary ask`: it is your secretary");
            }
            return ask(&paths, super::message_text(text)?);
        }
        Action::Desk { command } => {
            if principal.is_some() {
                anyhow::bail!(
                    "--for does not apply to `secretary desk`: the desk is the Speaker's"
                );
            }
            let set = command.map(|DeskCmd::Set { senators }| senators);
            let reply: DeskReply =
                runtime(false)?.block_on(daemon_call(&paths, &Request::Desk { set }))?;
            return print_json(&reply);
        }
        Action::Appoint(member) => {
            return change_member(
                &paths,
                Request::SetSecretarySession {
                    target_session: Some(member.session),
                    senator: None,
                    principal,
                    appointed: true,
                },
            );
        }
        Action::Dismiss(member) => {
            return change_member(
                &paths,
                Request::SetSecretarySession {
                    target_session: member.session,
                    senator: member.senator,
                    principal,
                    appointed: false,
                },
            );
        }
        Action::List => SecretaryWorkOp::Members,
        Action::Work {
            command: Work::List { after },
        } => SecretaryWorkOp::List { after },
        Action::Work {
            command: Work::Show { item, offset },
        } => SecretaryWorkOp::Read { item, offset },
        Action::Work {
            command:
                Work::Create {
                    title,
                    committee,
                    thread,
                    id,
                },
        } => SecretaryWorkOp::Create {
            id: id.unwrap_or_default(),
            title,
            thread: committee
                .zip(thread)
                .map(|(committee, root)| WorkThread { committee, root }),
        },
        Action::Work {
            command: Work::Assign { item, to, id },
        } => SecretaryWorkOp::Change {
            item,
            id: id.unwrap_or_default(),
            update: SecretaryWorkUpdate::Assign { to },
        },
    };
    let reply: SecretaryWorkReply = runtime(false)?.block_on(daemon_call(
        &paths,
        &Request::SecretaryWork {
            session: SessionSelector::default(),
            principal,
            operation,
        },
    ))?;
    print_json(&reply)
}

/// `senate secretary ask`: make the default secretary run, saying on
/// stderr what that took, then send it the request from the person.
fn ask(paths: &Paths, text: String) -> anyhow::Result<()> {
    use crate::ipc::{DefaultSecretary, EnsureAction};
    let locale = crate::config::BridgeConfig::locale_at(&paths.config());
    runtime(false)?.block_on(async {
        let secretary: DefaultSecretary = crate::ipc::request(
            &paths.socket(),
            &Request::EnsureSecretary,
            std::time::Duration::from_secs(1),
            crate::host::REQUEST_TIMEOUT,
        )
        .await?;
        let who = secretary
            .senator
            .as_ref()
            .map_or_else(|| "secretary".to_owned(), ToString::to_string);
        let said = match (secretary.action, locale) {
            (Some(EnsureAction::Started), senate_core::Locale::Zh) => {
                Some(format!("已启动默认 secretary（{who}）。"))
            }
            (Some(EnsureAction::Started), senate_core::Locale::En) => {
                Some(format!("Started the default secretary ({who})."))
            }
            (Some(EnsureAction::Resumed), senate_core::Locale::Zh) => {
                Some(format!("已恢复暂停的默认 secretary（{who}）。"))
            }
            (Some(EnsureAction::Resumed), senate_core::Locale::En) => {
                Some(format!("Resumed the default secretary ({who})."))
            }
            (Some(EnsureAction::Restored), senate_core::Locale::Zh) => {
                Some(format!("已重新打开默认 secretary（{who}）。"))
            }
            (Some(EnsureAction::Restored), senate_core::Locale::En) => {
                Some(format!("Restored the default secretary ({who})."))
            }
            _ => None,
        };
        if let Some(said) = said {
            eprintln!("{said}");
        }
        let sent = crate::speak::send(
            &paths.socket(),
            secretary.committee.clone(),
            crate::ipc::SayKind::Request,
            secretary.alias.as_str(),
            text,
        )
        .await?;
        let mut sent = sent;
        // `office` is rarely this directory's committee: the commands
        // suggested name it.
        sent.elsewhere = true;
        println!("{}", sent.render(false));
        Ok(())
    })
}

fn change_member(paths: &Paths, request: Request) -> anyhow::Result<()> {
    let reply: Secretaries = runtime(false)?.block_on(daemon_call(paths, &request))?;
    print_json(&reply)
}
