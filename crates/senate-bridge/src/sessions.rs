//! `senate sessions`: the Senate-hosted sessions on this machine.
//!
//! One listing ([`crate::ipc::SessionRow`], from the daemon's
//! `Request::Sessions`) serves every command that names a hosted session:
//! `senate sessions` prints it, `sessions stop` picks from it, the picker
//! ([`crate::host::picker`]) offers it, and `senate watch-all` tiles it.
//!
//! # Stopping
//!
//! `senate sessions stop <name>… | --idle | --all` ends each chosen
//! session's terminal as `senate stop` does: its TUI's process group is
//! asked to end, then killed. A session someone typed into in the last
//! [`TYPED_WINDOW_SECS`] is left alone unless `--force`: it may be in use.
//! `--idle` takes only the running sessions nobody is attached to.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use senate_core::Locale;

use crate::ipc::{SessionRow, SessionsReply};

/// A session typed into this recently is in use: `sessions stop` skips it
/// unless forced.
pub const TYPED_WINDOW_SECS: u64 = 5 * 60;

/// Which sessions `sessions stop` is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopSelector {
    /// These, by label or senator name.
    Named(Vec<String>),
    /// The running sessions nobody is attached to.
    Idle,
    /// Every running session.
    All,
}

/// What `sessions stop` takes from the listing: the rows to stop, and the
/// ones it leaves because someone typed into them recently.
#[derive(Debug, Default)]
pub struct StopPlan<'a> {
    pub stop: Vec<&'a SessionRow>,
    /// Each with how many seconds ago it was typed into.
    pub kept: Vec<(&'a SessionRow, u64)>,
}

/// What `sessions stop` does with the listing. A name that matches no
/// running session is an error.
pub fn select_for_stop<'a>(
    rows: &'a [SessionRow],
    selector: &StopSelector,
    force: bool,
) -> Result<StopPlan<'a>, String> {
    let running = rows.iter().filter(|r| r.is_running());
    let chosen: Vec<&SessionRow> = match selector {
        StopSelector::All => running.collect(),
        StopSelector::Idle => running.filter(|r| !r.attached).collect(),
        StopSelector::Named(names) => {
            let mut out = Vec::new();
            for name in names {
                let found = rows
                    .iter()
                    .filter(|r| r.is_running())
                    .find(|r| r.label() == *name || r.primary.as_str() == name);
                match found {
                    Some(row) => out.push(row),
                    None => return Err(format!("no running hosted session named {name}")),
                }
            }
            out
        }
    };
    let mut plan = StopPlan::default();
    for row in chosen {
        match row.typed_secs_ago {
            Some(ago) if !force && ago < TYPED_WINDOW_SECS => plan.kept.push((row, ago)),
            _ => plan.stop.push(row),
        }
    }
    Ok(plan)
}

/// How long since `since`, in the largest whole unit: `45s`, `12m`, `3h`,
/// `2d`.
pub fn uptime(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let secs = (now - since).num_seconds().max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// `senate sessions`: one line per session, the running first, then the
/// hosts running with no session.
pub fn render(reply: &SessionsReply, now: DateTime<Utc>, locale: Locale) -> String {
    if reply.rows.is_empty() && reply.orphans.is_empty() {
        return locale
            .pick(
                "这台机器上没有托管会话。",
                "No hosted sessions on this machine.",
            )
            .to_owned();
    }
    let header = [
        locale.pick("名称", "NAME"),
        locale.pick("委员会", "COMMITTEE"),
        locale.pick("运行时", "RUNTIME"),
        locale.pick("状态", "STATE"),
        locale.pick("运行", "UPTIME"),
        locale.pick("查看者", "VIEWER"),
        locale.pick("目录", "DIR"),
    ];
    let lines: Vec<[String; 7]> = reply
        .rows
        .iter()
        .map(|row| {
            [
                row.label(),
                row.committee
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
                row.runtime.as_str().to_owned(),
                row.state.as_str().to_owned(),
                row.started_at
                    .map_or_else(|| "-".to_owned(), |at| uptime(at, now)),
                viewer(row, locale),
                row.dir.display().to_string(),
            ]
        })
        .collect();
    let mut widths = header.map(textwrap::core::display_width);
    for line in &lines {
        for (w, cell) in widths.iter_mut().zip(line) {
            *w = (*w).max(textwrap::core::display_width(cell));
        }
    }
    let mut out = String::new();
    let mut row = |cells: &[&str]| {
        let mut line = String::new();
        for (i, (cell, w)) in cells.iter().zip(widths).enumerate() {
            if i + 1 == cells.len() {
                line.push_str(cell);
            } else {
                let pad = w.saturating_sub(textwrap::core::display_width(cell));
                let _ = write!(line, "{cell}{}  ", " ".repeat(pad));
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    };
    row(&header);
    for line in &lines {
        row(&line.each_ref().map(String::as_str));
    }
    for id in &reply.orphans {
        let _ = writeln!(
            out,
            "{}",
            match locale {
                Locale::Zh =>
                    format!("孤立的终端宿主 {id}：没有会话记录（`senate sessions prune` 会停止它）"),
                Locale::En => format!(
                    "orphaned host {id}: no session on record (`senate sessions prune` stops it)"
                ),
            }
        );
    }
    out.trim_end().to_owned()
}

/// Who is at its terminal: `attached`, how long since someone typed, or
/// `-`.
fn viewer(row: &SessionRow, locale: Locale) -> String {
    let typed = row.typed_secs_ago.map(|ago| match locale {
        Locale::Zh => format!("{}前输入", short(ago)),
        Locale::En => format!("typed {} ago", short(ago)),
    });
    match (row.attached, typed) {
        (true, Some(typed)) => format!("{} · {typed}", locale.pick("已连接", "attached")),
        (true, None) => locale.pick("已连接", "attached").to_owned(),
        (false, Some(typed)) if row.is_running() => typed,
        _ => "-".to_owned(),
    }
}

fn short(secs: u64) -> String {
    let now = Utc::now();
    uptime(now - chrono::Duration::seconds(secs.cast_signed()), now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::HostState;
    use crate::session::Runtime;
    use senate_core::{CommitteeId, EventId};

    fn row(name: &str, alias: Option<&str>, state: HostState) -> SessionRow {
        SessionRow {
            host_id: EventId::new(),
            session_id: format!("s-{name}"),
            primary: name.parse().unwrap(),
            alias: alias.map(|a| a.parse().unwrap()),
            committee: Some(CommitteeId::new("refund-v2").unwrap()),
            runtime: Runtime::Codex,
            dir: "/work/refund".into(),
            state,
            started_at: None,
            attached: false,
            typed_secs_ago: None,
            tty: "/home/hosts/x/tty".into(),
        }
    }

    #[test]
    fn stop_takes_the_named_idle_or_all_and_spares_recent_typing() {
        let mut busy = row("claude-1", Some("reviewer"), HostState::Running);
        busy.attached = true;
        busy.typed_secs_ago = Some(30);
        let mut quiet = row("codex-2", Some("builder"), HostState::Running);
        quiet.typed_secs_ago = Some(TYPED_WINDOW_SECS + 1);
        let stopped = row("claude-3", None, HostState::Stopped);
        let rows = vec![busy, quiet, stopped];
        let labels = |v: &[&SessionRow]| v.iter().map(|r| r.label()).collect::<Vec<_>>();

        let StopPlan { stop, kept } = select_for_stop(&rows, &StopSelector::All, false).unwrap();
        assert_eq!(labels(&stop), ["refund-v2::builder"]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].0.label(), "refund-v2::reviewer");
        assert_eq!(kept[0].1, 30);

        let StopPlan { stop, kept } = select_for_stop(&rows, &StopSelector::All, true).unwrap();
        assert_eq!(stop.len(), 2, "forced: nothing spared");
        assert!(kept.is_empty());

        let StopPlan { stop, .. } = select_for_stop(&rows, &StopSelector::Idle, true).unwrap();
        assert_eq!(
            labels(&stop),
            ["refund-v2::builder"],
            "the attached one is not idle"
        );

        let named = StopSelector::Named(vec!["codex-2".into(), "refund-v2::reviewer".into()]);
        let StopPlan { stop, kept } = select_for_stop(&rows, &named, false).unwrap();
        assert_eq!(labels(&stop), ["refund-v2::builder"]);
        assert_eq!(kept.len(), 1);

        let missing = StopSelector::Named(vec!["claude-3".into()]);
        assert!(
            select_for_stop(&rows, &missing, false).is_err(),
            "not running"
        );
    }

    #[test]
    fn the_listing_names_each_session_and_its_state() {
        let now = Utc::now();
        let mut running = row("claude-1", Some("reviewer"), HostState::Running);
        running.started_at = Some(now - chrono::Duration::minutes(90));
        running.attached = true;
        running.typed_secs_ago = Some(120);
        let stopped = row("codex-2", None, HostState::Stopped);
        let orphan = EventId::new();
        let reply = SessionsReply {
            rows: vec![running, stopped],
            orphans: vec![orphan.clone()],
        };
        let text = render(&reply, now, Locale::En);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("NAME"), "{text}");
        assert!(lines[1].starts_with("refund-v2::reviewer"), "{text}");
        assert!(lines[1].contains("running"), "{text}");
        assert!(lines[1].contains("1h"), "{text}");
        assert!(lines[1].contains("attached · typed 2m ago"), "{text}");
        assert!(lines[1].ends_with("/work/refund"), "{text}");
        assert!(lines[2].starts_with("codex-2"), "{text}");
        assert!(lines[2].contains("stopped"), "{text}");
        assert!(
            lines[3].contains(&format!("orphaned host {orphan}")),
            "{text}"
        );
        assert_eq!(
            render(&SessionsReply::default(), now, Locale::En),
            "No hosted sessions on this machine."
        );
    }
}
