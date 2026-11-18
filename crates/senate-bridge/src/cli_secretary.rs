use clap::{Args, Subcommand};
use senate_bridge::{
    Paths,
    ipc::{Request, SessionSelector},
};
use senate_core::api::Secretaries;
use senate_core::secretary_work::{
    SecretaryWorkOp, SecretaryWorkReply, SecretaryWorkUpdate, WorkThread,
};
use senate_core::{CommitteeId, EventId, SenatorName, Seq};

use super::{daemon_call, parse_seat_name, print_json, runtime};

#[derive(Args)]
pub(super) struct SecretaryTeam {
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
    /// Appoint a specific session to the secretary team.
    Appoint(Membership),
    /// Dismiss a specific session from the secretary team.
    Remove(Dismissal),
    /// List the team's appointed sessions by their stable senator IDs.
    Members,
    /// List the team's work items.
    List {
        #[arg(long)]
        after: Option<EventId>,
    },
    /// Read an item and its history.
    Read {
        item: EventId,
        #[arg(long, default_value_t)]
        offset: usize,
    },
    /// Record a new work item and optionally link a committee thread.
    Create {
        title: String,
        #[arg(long, requires = "thread")]
        committee: Option<CommitteeId>,
        #[arg(long, requires = "committee")]
        thread: Option<Seq>,
        /// Reuse the ID to retry the same creation.
        #[arg(long)]
        id: Option<EventId>,
    },
    /// Assign an item and notify its new owner.
    Assign {
        item: EventId,
        #[arg(long, value_parser = parse_seat_name)]
        to: SenatorName,
        /// Reuse the ID to retry the same assignment.
        #[arg(long)]
        id: Option<EventId>,
    },
}

pub(super) fn run(args: SecretaryTeam) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let principal = args.principal;
    let operation = match args.action {
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
        Action::Remove(member) => {
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
        Action::Members => SecretaryWorkOp::Members,
        Action::List { after } => SecretaryWorkOp::List { after },
        Action::Read { item, offset } => SecretaryWorkOp::Read { item, offset },
        Action::Create {
            title,
            committee,
            thread,
            id,
        } => SecretaryWorkOp::Create {
            id: id.unwrap_or_default(),
            title,
            thread: committee
                .zip(thread)
                .map(|(committee, root)| WorkThread { committee, root }),
        },
        Action::Assign { item, to, id } => SecretaryWorkOp::Change {
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

fn change_member(paths: &Paths, request: Request) -> anyhow::Result<()> {
    let reply: Secretaries = runtime(false)?.block_on(daemon_call(paths, &request))?;
    print_json(&reply)
}
