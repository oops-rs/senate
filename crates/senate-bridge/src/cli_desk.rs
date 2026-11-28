//! `senate desk`: the Speaker's desk from the terminal, as the app shows
//! it. List what awaits the Speaker, read one item, and decide it: answer
//! it as the Speaker, close it, forward it to its intended seat, or refuse
//! it. Deciding is the Speaker's in person (or the owner of the agent
//! serving as Speaker), from a person's terminal; the daemon refuses an
//! agent session.

use std::fmt::Write as _;

use clap::{Args, Subcommand};
use clap_complete::engine::{ArgValueCandidates, CompletionCandidate};
use senate_core::{CommitteeId, EventId, Locale, Seq};

use super::{BridgeConfig, daemon_call, message_text, print_json, runtime};
use crate::Paths;
use crate::ipc::{
    DeskAction, DeskDecision, DeskItemDetail, DeskOutcome, DocketReply, Request, SessionSelector,
    StatusReply, UnsafeModeReply,
};

#[derive(Args)]
pub(super) struct Desk {
    #[command(subcommand)]
    action: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    /// What awaits the Speaker, on every committee: one line an item.
    #[command(after_help = "Examples:
  senate desk                     The same as `senate desk list`
  senate desk list --json")]
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// One item: its thread, and what you may do with it.
    Show {
        /// `committee#seq`, or the seq alone when only one item has it.
        #[arg(add = ArgValueCandidates::new(items))]
        item: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Answer an item as the Speaker, in its thread; the asker gets it.
    #[command(after_help = "Examples:
  senate desk answer refund-v2#12 'From Q3.'
  senate desk answer 12 - < answer.md     The answer, read from stdin")]
    Answer {
        /// `committee#seq`, or the seq alone when only one item has it.
        #[arg(add = ArgValueCandidates::new(items))]
        item: String,
        /// The answer; `-` reads it from stdin.
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Close an item without answering it.
    Close {
        /// `committee#seq`, or the seq alone when only one item has it.
        #[arg(add = ArgValueCandidates::new(items))]
        item: String,
        /// Why, kept with it.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Forward a request for another committee to its intended seat.
    Forward {
        /// `committee#seq`, or the seq alone when only one item has it.
        #[arg(add = ArgValueCandidates::new(items))]
        item: String,
        /// Attach direct execution authorization (`meta::EXECUTION_AUTH`) via Touch ID.
        #[arg(long)]
        authorize: bool,
        /// Optional scope/reason note attached with `--authorize`.
        #[arg(long, requires = "authorize")]
        note: Option<String>,
    },
    /// Inspect or toggle local `[desk].unsafe_ignore_execution_auth`.
    Unsafe {
        /// Enable local receiver-side bypass for unverified cross-committee requests.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Disable local unsafe bypass (default).
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
    },
    /// Refuse a request for another committee, telling the asker why.
    Refuse {
        /// `committee#seq`, or the seq alone when only one item has it.
        #[arg(add = ArgValueCandidates::new(items))]
        item: String,
        /// Why; `-` reads it from stdin.
        #[arg(required = true)]
        reason: Vec<String>,
    },
}

pub(super) fn run(args: Desk) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let locale = BridgeConfig::locale_at(&paths.config());
    let rt = runtime(false)?;
    let action = args.action.unwrap_or(Action::List { json: false });
    let (item, decision, wanted) = match action {
        Action::List { json } => {
            let docket = rt.block_on(docket(&paths))?;
            if json {
                return print_json(&docket);
            }
            print!("{}", render_list(&docket, locale));
            return Ok(());
        }
        Action::Show { item, json } => {
            let (committee, seq) = rt.block_on(resolve(&paths, &item))?;
            let detail = rt.block_on(detail(&paths, &committee, seq))?;
            if json {
                return print_json(&detail);
            }
            print!("{}", render_item(&committee, &detail, locale));
            return Ok(());
        }
        Action::Answer { item, text } => (
            item,
            DeskDecision::Answer {
                body: message_text(text)?,
            },
            DeskAction::Answer,
        ),
        Action::Close { item, reason } => (item, DeskDecision::Close { reason }, DeskAction::Close),
        Action::Forward {
            item,
            authorize,
            note,
        } => {
            if authorize {
                (
                    item,
                    DeskDecision::ForwardAndAuthorize { note },
                    DeskAction::ForwardAndAuthorize,
                )
            } else {
                (item, DeskDecision::Forward, DeskAction::Forward)
            }
        }
        Action::Unsafe { enable, disable } => {
            if enable || disable {
                let reply: UnsafeModeReply = rt.block_on(daemon_call(
                    &paths,
                    &Request::SetUnsafeMode { enabled: enable },
                ))?;
                println!(
                    "unsafe_ignore_execution_auth = {} (local receiver setting; server provenance untouched)",
                    reply.enabled
                );
            } else {
                let status: StatusReply = rt.block_on(daemon_call(&paths, &Request::Status))?;
                println!(
                    "unsafe_ignore_execution_auth = {}",
                    status.unsafe_ignore_execution_auth
                );
            }
            return Ok(());
        }
        Action::Refuse { item, reason } => (
            item,
            DeskDecision::Refuse {
                reason: message_text(reason)?,
            },
            DeskAction::Refuse,
        ),
    };
    rt.block_on(async {
        let (committee, seq) = resolve(&paths, &item).await?;
        let detail = detail(&paths, &committee, seq).await?;
        if !detail.actions.contains(&wanted) {
            anyhow::bail!(
                "{}",
                detail.waits_on.clone().unwrap_or_else(|| locale
                    .pick(
                        "这件事不能这样处理。",
                        "This item cannot be decided that way."
                    )
                    .to_owned())
            );
        }
        let outcome: DeskOutcome = daemon_call(
            &paths,
            &Request::DeskDecide {
                committee: committee.clone(),
                seq,
                item_id: detail.item.id.clone(),
                id: EventId::new(),
                decision,
            },
        )
        .await?;
        println!("{}", render_outcome(&committee, seq, &outcome, locale));
        Ok(())
    })
}

async fn docket(paths: &Paths) -> anyhow::Result<DocketReply> {
    daemon_call(
        paths,
        &Request::Docket {
            session: SessionSelector::default(),
            committee: None,
        },
    )
    .await
}

async fn detail(
    paths: &Paths,
    committee: &CommitteeId,
    seq: Seq,
) -> anyhow::Result<DeskItemDetail> {
    daemon_call(
        paths,
        &Request::DeskItem {
            committee: committee.clone(),
            seq,
        },
    )
    .await
}

/// Every item on the desk: committee, seq, and a short line about it.
fn open_items(docket: &DocketReply) -> Vec<(CommitteeId, &senate_core::DocketItem)> {
    docket
        .senates
        .iter()
        .flat_map(|senate| &senate.pages)
        .flat_map(|page| page.items.iter().map(|item| (page.committee.clone(), item)))
        .collect()
}

/// `committee#seq`, or a seq alone that names one item on the desk.
async fn resolve(paths: &Paths, item: &str) -> anyhow::Result<(CommitteeId, Seq)> {
    let item = item.trim().trim_start_matches('#');
    if let Some((committee, seq)) = item.split_once('#') {
        return Ok((
            CommitteeId::new(committee)?,
            Seq(seq
                .parse()
                .map_err(|_| anyhow::anyhow!("not a seq: {seq:?}"))?),
        ));
    }
    let seq: u64 = item.parse().map_err(|_| {
        anyhow::anyhow!("name an item as `committee#seq` (`senate desk` lists them): {item:?}")
    })?;
    let docket = docket(paths).await?;
    let matching: Vec<CommitteeId> = open_items(&docket)
        .into_iter()
        .filter(|(_, i)| i.event.seq == Seq(seq))
        .map(|(c, _)| c)
        .collect();
    match matching.as_slice() {
        [committee] => Ok((committee.clone(), Seq(seq))),
        [] => anyhow::bail!("no item #{seq} on the desk (`senate desk` lists them)"),
        several => anyhow::bail!(
            "#{seq} is on several committees: {}; name one as `committee#{seq}`",
            several
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Completion: each open item as `committee#seq`, with who asked and what.
fn items() -> Vec<CompletionCandidate> {
    let Some(docket) = super::completion::request::<DocketReply>(&Request::Docket {
        session: SessionSelector::default(),
        committee: None,
    }) else {
        return Vec::new();
    };
    open_items(&docket)
        .into_iter()
        .map(|(committee, item)| {
            CompletionCandidate::new(format!("{committee}#{}", item.event.seq.0))
                .help(Some(summary(item, 60).into()))
        })
        .collect()
}

fn first_line(text: &str, most: usize) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.chars().count() > most {
        let cut: String = line.chars().take(most.saturating_sub(1)).collect();
        format!("{cut}…")
    } else {
        line.to_owned()
    }
}

fn summary(item: &senate_core::DocketItem, most: usize) -> String {
    format!(
        "{}: {}",
        item.event.on_behalf_of.as_ref().unwrap_or(&item.event.from),
        first_line(&item.event.body, most)
    )
}

fn why(item: &senate_core::DocketItem, locale: Locale) -> String {
    use senate_core::Awaiting;
    match &item.awaiting {
        Awaiting::Addressed => match &item.intended {
            Some(to) => locale.pick(format!("给 {to}"), format!("for {to}")),
            None => locale.pick("问 Speaker", "to the Speaker").to_owned(),
        },
        Awaiting::Pick { candidates, .. } => locale.pick(
            format!("在 {} 个中选一个", candidates.len()),
            format!("pick one of {}", candidates.len()),
        ),
        Awaiting::Away { agents } => {
            let agents: Vec<String> = agents.iter().map(ToString::to_string).collect();
            locale.pick(
                format!("{} 不在", agents.join(", ")),
                format!("{} away", agents.join(", ")),
            )
        }
        Awaiting::ChildOffline { child } => locale.pick(
            format!("子 Senate {child} 离线"),
            format!("child Senate {child} offline"),
        ),
    }
}

fn render_list(docket: &DocketReply, locale: Locale) -> String {
    let items = open_items(docket);
    if items.is_empty() {
        return format!(
            "{}\n",
            locale.pick("没有等待 Speaker 的事项。", "Nothing awaits the Speaker.")
        );
    }
    let mut out = String::new();
    let width = items
        .iter()
        .map(|(c, i)| format!("{c}#{}", i.event.seq.0).chars().count())
        .max()
        .unwrap_or(0);
    for (committee, item) in &items {
        let name = format!("{committee}#{}", item.event.seq.0);
        let _ = writeln!(
            out,
            "{name:<width$}  {}  ({})",
            summary(item, 72),
            why(item, locale)
        );
    }
    let _ = writeln!(
        out,
        "\n{}",
        locale.pick(
            "`senate desk show <事项>` 查看；answer、close、forward、refuse 处理。",
            "`senate desk show <item>` reads one; answer, close, forward or refuse decides it.",
        )
    );
    out
}

fn action_word(action: DeskAction) -> &'static str {
    match action {
        DeskAction::Forward => "forward",
        DeskAction::ForwardAndAuthorize => "forward & authorize",
        DeskAction::Refuse => "refuse",
        DeskAction::Answer => "answer",
        DeskAction::Close => "close",
    }
}

fn render_item(committee: &CommitteeId, detail: &DeskItemDetail, locale: Locale) -> String {
    let mut out = String::new();
    let item = &detail.item;
    let _ = writeln!(
        out,
        "{committee}#{} · {} · {}",
        item.seq.0,
        item.kind.as_str(),
        item.on_behalf_of.as_ref().unwrap_or(&item.from)
    );
    if let Some(destination) = &detail.destination {
        let _ = writeln!(out, "{} {destination}", locale.pick("目标", "for"));
    }
    if let Some(origin) = &detail.origin {
        let _ = writeln!(out, "{} {origin}", locale.pick("来自", "from"));
    }
    for event in &detail.events {
        let _ = writeln!(
            out,
            "\n#{} {} · {}",
            event.seq.0,
            event.from,
            event.kind.as_str()
        );
        for line in event.body.lines() {
            let _ = writeln!(out, "  {line}");
        }
    }
    out.push('\n');
    if detail.actions.is_empty() {
        let _ = writeln!(
            out,
            "{}",
            detail.waits_on.as_deref().unwrap_or_else(
                || locale.pick("现在无法处理。", "Nothing can be done with it now.")
            )
        );
    } else {
        let actions: Vec<String> = detail
            .actions
            .iter()
            .map(|a| format!("senate desk {} {committee}#{}", action_word(*a), item.seq.0))
            .collect();
        let _ = writeln!(out, "{}", actions.join("\n"));
    }
    out
}

fn render_outcome(
    committee: &CommitteeId,
    seq: Seq,
    outcome: &DeskOutcome,
    locale: Locale,
) -> String {
    let item = format!("{committee}#{}", seq.0);
    match outcome {
        DeskOutcome::Delivered {
            destination, seq, ..
        } => locale.pick(
            format!("{item} 已转给 {destination}（#{}）。", seq.0),
            format!("{item} forwarded to {destination} (#{}).", seq.0),
        ),
        DeskOutcome::Refused { seq, .. } => locale.pick(
            format!("{item} 已拒绝（#{}）。", seq.0),
            format!("{item} refused (#{}).", seq.0),
        ),
        DeskOutcome::Answered { seq, .. } => locale.pick(
            format!("{item} 已作为 Speaker 回答（#{}）。", seq.0),
            format!("{item} answered as the Speaker (#{}).", seq.0),
        ),
        DeskOutcome::Closed { seq, .. } => locale.pick(
            format!("{item} 已关闭（#{}）。", seq.0),
            format!("{item} closed (#{}).", seq.0),
        ),
    }
}
