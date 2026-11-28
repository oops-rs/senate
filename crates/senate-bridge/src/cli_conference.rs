//! `senate conference`: convene, inspect, RSVP, start, post to, and adjourn
//! cross-committee multi-party conferences.

use clap::{Args, Subcommand};
use senate_core::{
    Address, CommitteeId, ConferenceActionItem, ConferenceDetail, ConferenceId, ConferenceList,
    ConferenceRsvpAction, ConferenceSeatInput, ConferenceSummary, LinkedThread, Locator,
    PostConferenceEventResponse, SeatStatus, Seq,
};

use crate::Paths;
use crate::ipc::Request;

use super::{daemon_call, print_json, runtime, session_from_env};

#[derive(Args)]
pub(super) struct Conference {
    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Convene a conference inviting one or more committee chairs or seats.
    Convene {
        /// Short conference topic.
        topic: String,
        /// Invited seats, e.g. `senate::chair`, `commons::chair`, `senator::bob`.
        #[arg(long = "seat", required = true)]
        seats: Vec<String>,
        /// Optional linked threads (`<committee>:<seq>`).
        #[arg(long = "thread")]
        threads: Vec<String>,
        /// Print JSON instead of a summary line.
        #[arg(long)]
        json: bool,
    },
    /// List conferences visible to you.
    List {
        /// Print JSON instead of a summary list.
        #[arg(long)]
        json: bool,
    },
    /// Show a conference's seats, state, and transcript.
    Show {
        /// Conference identifier (`conf-<ulid>`).
        id: ConferenceId,
        /// Print JSON instead of formatted text.
        #[arg(long)]
        json: bool,
    },
    /// Attend a seat personally or delegate a senator.
    Rsvp {
        /// Conference identifier (`conf-<ulid>`).
        id: ConferenceId,
        /// Optional invited target (`senate::chair`, `commons::chair`, etc.).
        #[arg(long)]
        target: Option<String>,
        /// Delegate to another senator instead of attending directly.
        #[arg(long)]
        delegate: Option<String>,
        /// Scoped delegation brief (when `--delegate` is used).
        #[arg(long)]
        brief: Option<String>,
        /// Mark the delegated senator as ephemeral for this conference only.
        #[arg(long)]
        ephemeral: bool,
        /// Print JSON instead of a summary line.
        #[arg(long)]
        json: bool,
    },
    /// Start a convening conference early, bypassing any Absent seats.
    Start {
        /// Conference identifier (`conf-<ulid>`).
        id: ConferenceId,
        /// Print JSON instead of a summary line.
        #[arg(long)]
        json: bool,
    },
    /// Post a message inside an active conference room.
    Post {
        /// Conference identifier (`conf-<ulid>`).
        id: ConferenceId,
        /// Message body to post inside the conference room.
        #[arg(required = true)]
        text: Vec<String>,
        /// Explicit recipient mentions (`[senator::<name>]` or `<name>`).
        #[arg(long)]
        to: Vec<String>,
        /// Print JSON instead of a summary line.
        #[arg(long)]
        json: bool,
    },
    /// Adjourn a conference, lock it read-only, and publish a report.
    Adjourn {
        /// Conference identifier (`conf-<ulid>`).
        id: ConferenceId,
        /// Summary of the conference outcome and conclusion.
        summary: String,
        /// Decision recorded in the conference report.
        #[arg(long = "decision")]
        decisions: Vec<String>,
        /// Follow-up action item recorded in the conference report.
        #[arg(long = "action-item")]
        action_items: Vec<String>,
        /// Print JSON instead of a summary line.
        #[arg(long)]
        json: bool,
    },
}

fn parse_linked_thread(raw: &str) -> anyhow::Result<LinkedThread> {
    let (committee, seq) = raw
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("linked thread must be <committee>:<seq>, got {raw:?}"))?;
    let committee = CommitteeId::new(committee)?;
    let root: u64 = seq.parse()?;
    Ok(LinkedThread {
        committee,
        root: Seq(root),
    })
}

fn parse_address(raw: &str) -> anyhow::Result<Address> {
    if raw.contains("::") {
        Ok(raw.parse()?)
    } else {
        Ok(Address::Senator(raw.parse()?))
    }
}

pub(super) fn run(args: Conference) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let session = session_from_env().unwrap_or_default();
    runtime(false)?.block_on(async move {
        match args.action {
            Action::Convene {
                topic,
                seats,
                threads,
                json,
            } => {
                let parsed_seats = seats
                    .into_iter()
                    .map(|s| {
                        Ok(ConferenceSeatInput {
                            target: Locator::parse_short(&s)?,
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let linked_threads = threads
                    .into_iter()
                    .map(|t| parse_linked_thread(&t))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let summary: ConferenceSummary = daemon_call(
                    &paths,
                    &Request::ConferenceConvene {
                        session,
                        topic,
                        seats: parsed_seats,
                        linked_threads,
                    },
                )
                .await?;
                if json {
                    return print_json(&summary);
                }
                println!("{} ({:?}) — {}", summary.id, summary.state, summary.topic);
                Ok(())
            }
            Action::List { json } => {
                let list: ConferenceList =
                    daemon_call(&paths, &Request::ConferenceList { session }).await?;
                if json {
                    return print_json(&list);
                }
                if list.conferences.is_empty() {
                    println!("No conferences.");
                } else {
                    for c in &list.conferences {
                        println!("{} [{:?}] {}", c.id, c.state, c.topic);
                    }
                }
                Ok(())
            }
            Action::Show { id, json } => {
                let detail: ConferenceDetail =
                    daemon_call(&paths, &Request::ConferenceGet { session, id }).await?;
                if json {
                    return print_json(&detail);
                }
                let s = &detail.summary;
                println!("{} [{:?}] {}", s.id, s.state, s.topic);
                for seat in &s.seats {
                    let st = match &seat.status {
                        SeatStatus::Pending => "Pending".to_owned(),
                        SeatStatus::Absent => "Absent".to_owned(),
                        SeatStatus::Seated {
                            participant,
                            delegated_by,
                            ephemeral,
                        } => match (delegated_by, ephemeral) {
                            (Some(d), true) => {
                                format!("Delegate {participant} (by {d}, ephemeral)")
                            }
                            (Some(d), false) => format!("Delegate {participant} (by {d})"),
                            (None, _) => format!("Seated {participant}"),
                        },
                    };
                    println!("  seat {}: {}", seat.target, st);
                }
                for ev in &detail.events {
                    println!("[#{} {}] {}", ev.seq, ev.from, ev.body);
                }
                Ok(())
            }
            Action::Rsvp {
                id,
                target,
                delegate,
                brief,
                ephemeral,
                json,
            } => {
                let target = target.as_deref().map(Locator::parse_short).transpose()?;
                let action = match delegate {
                    Some(del) => ConferenceRsvpAction::Delegate {
                        target,
                        senator: parse_address(&del)?,
                        delegated_by: None,
                        brief,
                        ephemeral,
                    },
                    None => ConferenceRsvpAction::Attend {
                        target,
                        participant: None,
                    },
                };
                let summary: ConferenceSummary = daemon_call(
                    &paths,
                    &Request::ConferenceRsvp {
                        session,
                        id,
                        action,
                    },
                )
                .await?;
                if json {
                    return print_json(&summary);
                }
                println!("{} [{:?}]", summary.id, summary.state);
                Ok(())
            }
            Action::Start { id, json } => {
                let summary: ConferenceSummary =
                    daemon_call(&paths, &Request::ConferenceStart { session, id }).await?;
                if json {
                    return print_json(&summary);
                }
                println!("{} [{:?}]", summary.id, summary.state);
                Ok(())
            }
            Action::Post { id, text, to, json } => {
                let to = to
                    .into_iter()
                    .map(|raw| parse_address(&raw))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let resp: PostConferenceEventResponse = daemon_call(
                    &paths,
                    &Request::ConferencePost {
                        session,
                        id,
                        body: text.join(" "),
                        to,
                    },
                )
                .await?;
                if json {
                    return print_json(&resp);
                }
                println!(
                    "Posted #{} (round {}, hops remaining {})",
                    resp.event.seq, resp.floor.round, resp.floor.agent_hops_remaining
                );
                Ok(())
            }
            Action::Adjourn {
                id,
                summary,
                decisions,
                action_items,
                json,
            } => {
                let items = action_items
                    .into_iter()
                    .map(|description| ConferenceActionItem {
                        description,
                        owner: None,
                        committee: None,
                    })
                    .collect();
                let res: ConferenceSummary = daemon_call(
                    &paths,
                    &Request::ConferenceAdjourn {
                        session,
                        id,
                        summary,
                        decisions,
                        action_items: items,
                    },
                )
                .await?;
                if json {
                    return print_json(&res);
                }
                println!("Adjourned {} [{:?}]", res.id, res.state);
                Ok(())
            }
        }
    })
}
