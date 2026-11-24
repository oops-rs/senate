use std::io::{self, IsTerminal, Write};
use std::process::{Command, Stdio};

use anyhow::{Context, bail};

use super::{daemon_call, runtime};
use crate::Paths;
use crate::ipc::{
    Request, SessionHistoryReply, SessionHistoryRow, SessionSearchReply, TimelineDirection,
    TimelineKind, TimelineReply, TimelineToolDetail,
};
use senate_core::CommitteeId;
use senate_markdown::Terminal;

pub(super) fn label(row: &SessionHistoryRow) -> String {
    match (&row.committee, &row.alias) {
        (Some(committee), Some(alias)) => format!("{committee}::{alias}"),
        (_, Some(alias)) => alias.to_string(),
        _ => row.primary.to_string(),
    }
}

async fn history_page(
    paths: &Paths,
    committee: Option<CommitteeId>,
    before: Option<String>,
) -> anyhow::Result<SessionHistoryReply> {
    daemon_call(
        paths,
        &Request::SessionHistory {
            committee,
            before,
            limit: Some(100),
        },
    )
    .await
}

pub(super) fn history(
    committee: Option<CommitteeId>,
    search: Option<String>,
) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    runtime(false)?.block_on(async {
        let mut cursor = None;
        let mut reported_build = false;
        loop {
            if let Some(query) = &search {
                let reply: SessionSearchReply = daemon_call(
                    &paths,
                    &Request::SessionSearch {
                        query: query.clone(),
                        committee: committee.clone(),
                        limit: Some(60),
                        cursor: cursor.clone(),
                    },
                )
                .await?;
                if reply.indexing {
                    if !reported_build {
                        eprintln!("Updating the local conversation index…");
                        reported_build = true;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    continue;
                }
                for hit in reply.hits {
                    println!(
                        "{} #{}  {}",
                        crate::terminal::printable(&hit.session_id),
                        hit.seq,
                        crate::terminal::printable(&hit.snippet)
                    );
                }
                cursor = reply.next_cursor;
            } else {
                let reply = history_page(&paths, committee.clone(), cursor).await?;
                for row in reply.rows {
                    let title = row.title.as_deref().unwrap_or("");
                    let name = label(&row);
                    println!(
                        "{}  {}  {:?}  {}  {}",
                        row.last_activity.format("%Y-%m-%d %H:%M"),
                        crate::terminal::printable(&name),
                        row.runtime,
                        crate::terminal::printable(&row.session_id),
                        crate::terminal::printable(title)
                    );
                }
                cursor = reply.next_before;
            }
            if cursor.is_none() {
                break;
            }
        }
        Ok(())
    })
}

async fn resolve(paths: &Paths, name: &str) -> anyhow::Result<String> {
    let mut selected = None;
    let mut ambiguous = false;
    let mut before = None;
    loop {
        let reply = history_page(paths, None, before).await?;
        for row in reply.rows {
            if row.session_id == name {
                return Ok(row.session_id);
            }
            if label(&row) == name
                || row.primary.to_string() == name
                || row
                    .alias
                    .as_ref()
                    .is_some_and(|alias| alias.to_string() == name)
                || row.shown_name.as_deref() == Some(name)
            {
                ambiguous |= selected.replace(row.session_id).is_some();
            }
        }
        before = reply.next_before;
        if before.is_none() {
            if ambiguous {
                bail!("ambiguous session; use its ID from senate session history");
            }
            return selected.context("no past session of that name; use senate session history");
        }
    }
}

pub(super) fn show(name: &str) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let rt = runtime(false)?;
    let id = rt.block_on(resolve(&paths, name))?;
    let terminal = Terminal::stdout(None);
    let tty = io::stdout().is_terminal();
    if tty {
        let pager = std::env::var("PAGER").unwrap_or_else(|_| "less -FRX".into());
        let mut args = pager.split_whitespace();
        if let Some(program) = args.next()
            && let Ok(mut child) = Command::new(program)
                .args(args)
                .stdin(Stdio::piped())
                .spawn()
        {
            let Some(mut writer) = child.stdin.take() else {
                let _ = child.wait();
                return Ok(());
            };
            let result = rt.block_on(print(&paths, id, terminal, &mut writer));
            drop(writer);
            let _ = child.wait();
            return result;
        }
    }
    rt.block_on(print(&paths, id, terminal, &mut io::stdout().lock()))
}

async fn print(
    paths: &Paths,
    id: String,
    terminal: Terminal,
    out: &mut impl Write,
) -> anyhow::Result<()> {
    let mut cursor = 0;
    loop {
        let page: TimelineReply = daemon_call(
            paths,
            &Request::Timeline {
                session_id: id.clone(),
                direction: TimelineDirection::After,
                cursor: Some(cursor),
                limit: Some(300),
            },
        )
        .await?;
        if !page.available {
            bail!("the session has no readable conversation");
        }
        for item in page.items {
            let (role, text) = match item.kind {
                TimelineKind::UserMessage { text } => ("User", text),
                TimelineKind::AssistantMessage { text } => ("Assistant", text),
                TimelineKind::Reasoning { text } => ("Reasoning", text),
                TimelineKind::SenateMessage { text, .. } => ("Senate", text),
                TimelineKind::Notice { text, .. } => ("Notice", text),
                TimelineKind::Todo { items } => (
                    "To do",
                    items
                        .into_iter()
                        .map(|item| format!("- {} ({})", item.text, item.status))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
                TimelineKind::ToolCall {
                    name,
                    detail,
                    output,
                    ..
                } => {
                    let (mut text, shell_output) = match detail {
                        TimelineToolDetail::Shell {
                            command, output, ..
                        } => (command, output),
                        TimelineToolDetail::Read { path }
                        | TimelineToolDetail::Write { path }
                        | TimelineToolDetail::Edit { path, .. } => (path, None),
                        TimelineToolDetail::Search { query } => (query, None),
                        TimelineToolDetail::Fetch { url } => (url, None),
                        TimelineToolDetail::SubAgent { description } => (description, None),
                        TimelineToolDetail::Other { summary } => (summary, None),
                    };
                    for output in [shell_output, output].into_iter().flatten() {
                        text.push('\n');
                        text.push_str(&output);
                    }
                    if !text.is_empty() {
                        text.insert(0, '\n');
                    }
                    text.insert_str(0, &name);
                    ("Tool", text)
                }
            };
            let output = writeln!(
                out,
                "{role}\n{}",
                crate::format::markdown_for(terminal, &text)
            );
            if output
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe)
            {
                return Ok(());
            }
            output?;
        }
        match page.last_seq.filter(|_| page.has_newer) {
            Some(last) if last > cursor => cursor = last,
            _ => return Ok(()),
        }
    }
}
