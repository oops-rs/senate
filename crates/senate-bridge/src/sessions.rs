//! `senate session list`: the Senate-hosted sessions on this machine.
//!
//! One listing ([`crate::ipc::SessionRow`], from the daemon's
//! `Request::Sessions`) serves every command that names a hosted session:
//! `senate session list` prints it, `session stop` picks from it, the picker
//! ([`crate::host::picker`]) offers it, and `senate session watch` tiles it.
//!
//! # Stopping
//!
//! `senate session stop <name>… | --idle | --all` ends each chosen
//! session's terminal: its TUI's process group is asked to end, then
//! killed. A session someone typed into in the last
//! [`TYPED_WINDOW_SECS`] is left alone unless `--force`: it may be in use.
//! `--idle` takes only the running sessions nobody is attached to.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use senate_core::Locale;

use crate::ipc::{SessionRow, SessionsReply};

/// A session typed into this recently is in use: `session stop` skips it
/// unless forced.
pub const TYPED_WINDOW_SECS: u64 = 5 * 60;

/// Which sessions `session stop` is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopSelector {
    /// These, by label or senator name.
    Named(Vec<String>),
    /// The running sessions nobody is attached to.
    Idle,
    /// Every running session of one committee.
    Committee(senate_core::CommitteeId),
    /// Every running session.
    All,
}

/// What `session stop` takes from the listing: the rows to stop, and the
/// ones it leaves because someone typed into them recently.
#[derive(Debug, Default)]
pub struct StopPlan<'a> {
    pub stop: Vec<&'a SessionRow>,
    /// Each with how many seconds ago it was typed into.
    pub kept: Vec<(&'a SessionRow, u64)>,
}

/// What `session stop` does with the listing. A name that matches no
/// running session is an error.
pub fn select_for_stop<'a>(
    rows: &'a [SessionRow],
    selector: &StopSelector,
    force: bool,
) -> Result<StopPlan<'a>, String> {
    let chosen = select(rows, selector, SessionRow::is_running, "running")?;
    let mut plan = StopPlan::default();
    for row in chosen {
        match row.typed_secs_ago {
            Some(ago) if !force && ago < TYPED_WINDOW_SECS => plan.kept.push((row, ago)),
            _ => plan.stop.push(row),
        }
    }
    Ok(plan)
}

/// The sessions `senate session resume` is for: those recorded paused. A
/// name that matches no paused session is an error.
pub fn select_for_resume<'a>(
    rows: &'a [SessionRow],
    selector: &StopSelector,
) -> Result<Vec<&'a SessionRow>, String> {
    select(rows, selector, |r| r.paused, "paused")
}

/// The rows `selector` picks among those `eligible` (`what` they are, for
/// an error). A bare alias names the one eligible session that has it;
/// several are an error listing them, to name one as `committee::alias`.
fn select<'a>(
    rows: &'a [SessionRow],
    selector: &StopSelector,
    eligible: impl Fn(&SessionRow) -> bool,
    what: &str,
) -> Result<Vec<&'a SessionRow>, String> {
    let ok = || rows.iter().filter(|r| eligible(r));
    Ok(match selector {
        StopSelector::All => ok().collect(),
        StopSelector::Idle => ok().filter(|r| !r.attached).collect(),
        StopSelector::Committee(committee) => ok()
            .filter(|r| r.committee.as_ref() == Some(committee))
            .collect(),
        StopSelector::Named(names) => {
            let mut out = Vec::new();
            for name in names {
                let exact = ok().find(|r| r.label() == *name || r.primary.as_str() == name);
                let by_alias: Vec<&SessionRow> = ok()
                    .filter(|r| r.alias.as_ref().is_some_and(|a| a.as_str() == name))
                    .collect();
                match (exact, by_alias.as_slice()) {
                    (Some(row), _) | (None, &[row]) => out.push(row),
                    (None, []) => return Err(format!("no {what} hosted session named {name}")),
                    (None, several) => {
                        let labels: Vec<String> = several.iter().map(|r| r.label()).collect();
                        return Err(format!(
                            "{name} names several {what} sessions; name one: {}",
                            labels.join(", ")
                        ));
                    }
                }
            }
            out
        }
    })
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

/// `senate session list`: one line per session, the running first, then the
/// hosts running with no session.
pub fn render(reply: &SessionsReply, now: DateTime<Utc>, locale: Locale) -> String {
    render_with_ids(reply, now, locale, false)
}

pub fn render_with_ids(
    reply: &SessionsReply,
    now: DateTime<Utc>,
    locale: Locale,
    verbose: bool,
) -> String {
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
        locale.pick("模型", "MODEL"),
        locale.pick("状态", "STATE"),
        locale.pick("运行", "UPTIME"),
        locale.pick("查看者", "VIEWER"),
        locale.pick("活动", "ACTIVITY"),
        locale.pick("待读", "WAITING"),
        locale.pick("目录", "DIR"),
    ];
    let lines: Vec<[String; 10]> = reply
        .rows
        .iter()
        .map(|row| {
            [
                row.display_name(verbose),
                row.committee
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), ToString::to_string),
                row.runtime.as_str().to_owned(),
                row.model.label(),
                format!(
                    "{}{}",
                    first_line_within(&state(row, locale), CELL_COLUMNS),
                    marker(row)
                ),
                row.started_at
                    .map_or_else(|| "-".to_owned(), |at| uptime(at, now)),
                viewer(row, locale),
                row.delivery
                    .activity
                    .map_or("-", |activity| activity.label(locale))
                    .to_owned(),
                waiting(&row.delivery, now),
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
    if verbose {
        for row in &reply.rows {
            if let Some(health) = &row.host_health {
                let _ = writeln!(out, "{}: {}", row.display_name(verbose), health.notice());
            }
            if row.codex_scope_advisory {
                let warning = locale.pick(
                    "使用共享 Codex app-server，守卫仅为提示",
                    "shared Codex app-server; tool process protection is advisory",
                );
                let _ = writeln!(out, "{}: {warning}", row.display_name(verbose));
            }
            if let Some(warning) = &row.model.verification_warning {
                let _ = writeln!(out, "{}: {warning}", row.display_name(verbose));
            }
        }
    } else {
        for line in summary(reply, locale) {
            let _ = writeln!(out, "{line}");
        }
    }
    for id in &reply.orphans {
        let _ = writeln!(
            out,
            "{}",
            match locale {
                Locale::Zh =>
                    format!("孤立的终端宿主 {id}：没有会话记录（`senate session prune` 会停止它）"),
                Locale::En => format!(
                    "orphaned host {id}: no session on record (`senate session prune` stops it)"
                ),
            }
        );
    }
    out.trim_end().to_owned()
}

/// The STATE cell's note: `*` its host needs a restart (or has not
/// verified this daemon), `~` it shares Codex's app-server, `!` its model
/// has a warning. [`summary`] counts them; `--verbose` says each.
fn marker(row: &SessionRow) -> String {
    let mut marker = String::new();
    if row.host_health.is_some() {
        marker.push('*');
    }
    if row.codex_scope_advisory {
        marker.push('~');
    }
    if row.model.verification_warning.is_some() {
        marker.push('!');
    }
    if marker.is_empty() {
        marker
    } else {
        format!(" {marker}")
    }
}

/// At most two lines under the table: the hosts that need a restart, and
/// the rest of the notes.
pub(crate) fn summary(reply: &SessionsReply, locale: Locale) -> Vec<String> {
    let count = |test: fn(&SessionRow) -> bool| reply.rows.iter().filter(|row| test(row)).count();
    let restart = count(|row| row.host_health.is_some());
    let shared = count(|row| row.codex_scope_advisory);
    let model = count(|row| row.model.verification_warning.is_some());
    let mut lines = Vec::new();
    if restart > 0 {
        lines.push(match locale {
            Locale::Zh => format!(
                "* {restart} 个宿主需要重启才受保护：senate session restart --all-outdated --dry-run"
            ),
            Locale::En => format!(
                "* {restart} host(s) need a restart to be protected: senate session restart --all-outdated --dry-run"
            ),
        });
    }
    let mut notes = Vec::new();
    if shared > 0 {
        notes.push(match locale {
            Locale::Zh => format!("~ {shared} 个使用共享 Codex app-server（守卫仅为提示）"),
            Locale::En => format!("~ {shared} on the shared Codex app-server (advisory)"),
        });
    }
    if model > 0 {
        notes.push(match locale {
            Locale::Zh => format!("! {model} 个模型有警告"),
            Locale::En => format!("! {model} with a model warning"),
        });
    }
    if !notes.is_empty() {
        lines.push(format!(
            "{}{}",
            notes.join("; "),
            locale.pick("；详情见 --verbose", "; details with --verbose")
        ));
    }
    lines
}

/// Most columns a waiting prompt's command takes in the STATE cell: a
/// multi-line heredoc must not push the other columns off the screen.
/// `senate session approve` shows the whole of it.
const PROMPT_COLUMNS: usize = 40;

/// Most columns any cell but the last (DIR) takes, whatever it holds.
const CELL_COLUMNS: usize = 60;

/// The first non-blank line of `text`, its control characters made spaces,
/// cut to `max` display columns with `…` when longer.
fn first_line_within(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let more_lines = text.trim().lines().count() > 1;
    let line: String = line
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out = String::new();
    let mut width = 0;
    for c in line.chars() {
        let w = unicode_width_of(c);
        if width + w > max.saturating_sub(1) {
            out.push('…');
            return out;
        }
        width += w;
        out.push(c);
    }
    if more_lines {
        out.push('…');
    }
    out
}

fn unicode_width_of(c: char) -> usize {
    textwrap::core::display_width(c.encode_utf8(&mut [0; 4]))
}

/// Its state, and the prompt waiting in its terminal, if one is.
fn state(row: &SessionRow, locale: Locale) -> String {
    if row.paused {
        return locale.pick("已暂停", "paused").to_owned();
    }
    let state = row.state.as_str();
    match &row.prompt {
        Some(prompt) if row.is_running() => {
            let what = match (&prompt.tool, prompt.screen) {
                (_, true) => locale.pick("等待终端提示", "prompt in terminal").to_owned(),
                (Some(tool), false) => {
                    let tool = first_line_within(tool, PROMPT_COLUMNS);
                    match locale {
                        Locale::Zh => format!("等待批准 {tool}"),
                        Locale::En => format!("approval: {tool}"),
                    }
                }
                (None, false) => locale.pick("等待批准", "approval").to_owned(),
            };
            format!("{state} · {what}")
        }
        _ => state.to_owned(),
    }
}

/// What is held for it or was handed to it without showing in its own
/// record: how many, and how long the oldest has waited (`2 · 6m`); `-`
/// for none.
pub(crate) fn waiting(delivery: &crate::ipc::SessionDelivery, now: DateTime<Utc>) -> String {
    match (delivery.waiting, delivery.waiting_since) {
        (0, _) => "-".to_owned(),
        (n, Some(since)) => format!("{n} · {}", uptime(since, now)),
        (n, None) => n.to_string(),
    }
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

pub(crate) fn short(secs: u64) -> String {
    let now = Utc::now();
    uptime(now - chrono::Duration::seconds(secs.cast_signed()), now)
}

/// What `senate session replace` prints: each step and what it did.
pub fn render_replace(reply: &crate::ipc::ReplaceReply, locale: Locale) -> String {
    let old = senate_core::Address::senator(&reply.old).in_text();
    let new = senate_core::Address::senator(&reply.new).in_text();
    let mut out = match locale {
        Locale::Zh => format!("{new} 在委员会 `{}` 替换了 {old}：\n", reply.committee),
        Locale::En => format!("{new} replaced {old} on `{}`:\n", reply.committee),
    };
    for report in &reply.steps {
        let earlier = if report.earlier {
            locale.pick("（之前已完成）", " (done earlier)")
        } else {
            ""
        };
        out.push_str(&format!(
            "  {:<9} {}{earlier}\n",
            report.step.as_str(),
            report.detail
        ));
    }
    // By the alias it took over, the name the person knows it by.
    let name = reply.alias.as_ref().unwrap_or(&reply.new);
    out.push_str(&match locale {
        Locale::Zh => format!("用 `senate attach {name}` 打开它的终端。"),
        Locale::En => format!("Open its terminal with `senate attach {name}`."),
    });
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ipc::HostState;
    use crate::session::Runtime;
    use senate_core::{CommitteeId, EventId};

    pub(crate) fn row(name: &str, alias: Option<&str>, state: HostState) -> SessionRow {
        SessionRow {
            host_health: None,
            codex_scope_advisory: false,
            paused: false,
            delivery: crate::ipc::SessionDelivery::default(),
            chair: false,
            shown_name: None,
            host_id: EventId::new(),
            session_id: format!("s-{name}"),
            primary: name.parse().unwrap(),
            alias: alias.map(|a| a.parse().unwrap()),
            committee: Some(CommitteeId::new("refund-v2").unwrap()),
            runtime: Runtime::Codex,
            working: false,
            last_activity: chrono::DateTime::default(),
            title: None,
            branch: None,
            model: crate::ipc::HostedModel::default(),
            dir: "/work/refund".into(),
            state,
            started_at: None,
            attached: false,
            typed_secs_ago: None,
            viewer_tty: None,
            tty: "/home/hosts/x/tty".into(),
            prompt: None,
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
    fn pause_takes_a_committee_and_resume_only_the_paused() {
        let running = row("claude-1", Some("reviewer"), HostState::Running);
        let mut elsewhere = row("codex-2", Some("builder"), HostState::Running);
        elsewhere.committee = Some(CommitteeId::new("ledger").unwrap());
        let mut paused = row("claude-3", Some("main"), HostState::Stopped);
        paused.paused = true;
        let stopped = row("codex-4", Some("idle"), HostState::Stopped);
        let rows = vec![running, elsewhere, paused, stopped];
        let labels = |v: &[&SessionRow]| v.iter().map(|r| r.label()).collect::<Vec<_>>();

        let committee = StopSelector::Committee(CommitteeId::new("refund-v2").unwrap());
        let StopPlan { stop, .. } = select_for_stop(&rows, &committee, false).unwrap();
        assert_eq!(labels(&stop), ["refund-v2::reviewer"], "running, on it");

        let resume = select_for_resume(&rows, &StopSelector::All).unwrap();
        assert_eq!(labels(&resume), ["refund-v2::main"]);
        let named = StopSelector::Named(vec!["main".into()]);
        assert_eq!(
            labels(&select_for_resume(&rows, &named).unwrap()),
            ["refund-v2::main"]
        );
        let stopped = StopSelector::Named(vec!["idle".into()]);
        assert_eq!(
            select_for_resume(&rows, &stopped).unwrap_err(),
            "no paused hosted session named idle",
            "stopped on purpose is not paused"
        );
        let text = render(
            &SessionsReply {
                rows: rows.clone(),
                local_rows: Vec::new(),
                orphans: Vec::new(),
            },
            Utc::now(),
            Locale::En,
        );
        let line = text.lines().find(|l| l.starts_with("main")).unwrap();
        assert!(line.contains("paused"), "{text}");
        let line = text.lines().find(|l| l.starts_with("idle")).unwrap();
        assert!(line.contains("stopped"), "{text}");
    }

    /// Seven sessions with notes: a marker each in STATE and at most two
    /// lines under the table; each one's note only with --verbose.
    #[test]
    fn notes_are_markers_and_two_summary_lines() {
        let health = || {
            Some(Box::new(crate::ipc::HostHealth {
                started_by: "0.12.3".into(),
                outdated: true,
                daemon_unverified: false,
            }))
        };
        let mut rows = Vec::new();
        for (i, (outdated, shared)) in [
            (true, false),
            (true, false),
            (true, false),
            (true, true),
            (false, true),
            (false, true),
            (false, false),
        ]
        .into_iter()
        .enumerate()
        {
            let mut row = row(
                &format!("codex-{i}"),
                Some(&format!("m{i}")),
                HostState::Running,
            );
            row.host_health = if outdated { health() } else { None };
            row.codex_scope_advisory = shared;
            rows.push(row);
        }
        let reply = SessionsReply {
            rows,
            local_rows: Vec::new(),
            orphans: Vec::new(),
        };
        let text = render(&reply, Utc::now(), Locale::En);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1 + 7 + 2, "{text}");
        assert!(lines[4].contains("running *~"), "{text}");
        assert!(lines[8].starts_with("* 4 host(s) need a restart"), "{text}");
        assert!(lines[9].starts_with("~ 3 on the shared Codex"), "{text}");
        let verbose = render_with_ids(&reply, Utc::now(), Locale::En, true);
        assert!(verbose.lines().count() > 10, "{verbose}");
        assert!(
            verbose.contains("needs restart to be protected"),
            "{verbose}"
        );
    }

    /// A Codex speed shows with the model; a speed Codex did not take is
    /// the model warning's `!`, said only with --verbose.
    #[test]
    fn a_speed_shows_in_the_model_and_its_mismatch_is_a_model_warning() {
        let mut quick = row("codex-quick", Some("quick"), HostState::Running);
        quick.model = crate::ipc::HostedModel::new(
            Some("gpt-6.2-sol".into()),
            Some("high".into()),
            Some("ultrafast".into()),
        );
        quick.model.verification_warning =
            Some("Codex: requested speed ultrafast, app-server reports no explicit speed".into());
        let reply = SessionsReply {
            rows: vec![quick],
            local_rows: Vec::new(),
            orphans: Vec::new(),
        };
        let text = render(&reply, Utc::now(), Locale::En);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1 + 1 + 1, "{text}");
        assert!(
            lines[1].contains("gpt-6.2-sol/high (ultrafast)  running !"),
            "{text}"
        );
        assert_eq!(lines[2], "! 1 with a model warning; details with --verbose");
        assert!(!text.contains("requested speed"), "{text}");
        let verbose = render_with_ids(&reply, Utc::now(), Locale::En, true);
        assert!(
            verbose.contains(
                ": Codex: requested speed ultrafast, app-server reports no explicit speed"
            ),
            "{verbose}"
        );
    }

    #[test]
    fn the_listing_names_each_session_and_its_state() {
        let now = Utc::now();
        let mut running = row("claude-1", Some("reviewer"), HostState::Running);
        running.started_at = Some(now - chrono::Duration::minutes(90));
        running.attached = true;
        running.typed_secs_ago = Some(120);
        running.model =
            crate::ipc::HostedModel::new(Some("opus".into()), Some("high".into()), None);
        let stopped = row("codex-2", None, HostState::Stopped);
        let orphan = EventId::new();
        let reply = SessionsReply {
            rows: vec![running, stopped],
            local_rows: Vec::new(),
            orphans: vec![orphan.clone()],
        };
        let text = render(&reply, now, Locale::En);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("NAME"), "{text}");
        let column = lines[0].find("MODEL").unwrap();
        assert_eq!(lines[0].find("STATE"), Some(column + "opus/high  ".len()));
        assert!(lines[1].starts_with("reviewer"), "{text}");
        assert_eq!(&lines[1][column..column + 9], "opus/high", "{text}");
        assert_eq!(&lines[2][column..column + 7], "default", "{text}");
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

    /// What each session is doing and what waits for it unseen, in two
    /// columns before DIR; the columns before them stay where they were.
    #[test]
    fn activity_and_what_waits_unseen_are_two_columns_before_the_directory() {
        use crate::ipc::{SessionActivity, SessionDelivery};
        let now = Utc::now();
        let mut stuck = row("agy-1", Some("triage"), HostState::Running);
        stuck.delivery = SessionDelivery {
            activity: Some(SessionActivity::NotResponding),
            waiting: 2,
            waiting_since: Some(now - chrono::Duration::minutes(6)),
            stuck: true,
        };
        let mut busy = row("claude-1", Some("builder"), HostState::Running);
        busy.delivery = SessionDelivery {
            activity: Some(SessionActivity::Active),
            waiting: 1,
            waiting_since: Some(now - chrono::Duration::seconds(20)),
            stuck: false,
        };
        let mut idle = row("codex-1", Some("review"), HostState::Running);
        idle.delivery.activity = Some(SessionActivity::Idle);
        let stopped = row("codex-2", Some("old"), HostState::Stopped);
        let reply = SessionsReply {
            rows: vec![stuck, busy, idle, stopped],
            local_rows: Vec::new(),
            orphans: Vec::new(),
        };
        let text = render(&reply, now, Locale::En);
        let lines: Vec<&str> = text.lines().collect();
        let header: Vec<&str> = lines[0].split_whitespace().collect();
        assert_eq!(
            header,
            [
                "NAME",
                "COMMITTEE",
                "RUNTIME",
                "MODEL",
                "STATE",
                "UPTIME",
                "VIEWER",
                "ACTIVITY",
                "WAITING",
                "DIR"
            ]
        );
        let (activity, waiting, dir) = (
            lines[0].find("ACTIVITY").unwrap(),
            lines[0].find("WAITING").unwrap(),
            lines[0].find("DIR").unwrap(),
        );
        let cell = |line: &str, from: usize, to: usize| line[from..to].trim().to_owned();
        let line = |name: &str| *lines.iter().find(|l| l.starts_with(name)).unwrap();
        assert_eq!(cell(line("triage"), activity, waiting), "not responding");
        assert_eq!(cell(line("triage"), waiting, dir), "2 · 6m");
        assert_eq!(cell(line("builder"), activity, waiting), "active");
        assert_eq!(cell(line("builder"), waiting, dir), "1 · 20s");
        assert_eq!(cell(line("review"), activity, waiting), "idle");
        assert_eq!(cell(line("review"), waiting, dir), "-");
        assert_eq!(cell(line("old"), activity, waiting), "-", "not running");
        assert!(line("triage").ends_with("/work/refund"), "{text}");
    }

    #[test]
    fn a_waiting_prompt_shows_in_the_state() {
        let now = Utc::now();
        let mut waiting = row("codex-1", None, HostState::Running);
        waiting.prompt = Some(crate::ipc::HostedPrompt {
            tool: None,
            since: now,
            screen: true,
        });
        let mut asking = row("claude-1", None, HostState::Running);
        asking.prompt = Some(crate::ipc::HostedPrompt {
            tool: Some("Bash".into()),
            since: now,
            screen: false,
        });
        let reply = SessionsReply {
            rows: vec![waiting, asking],
            local_rows: Vec::new(),
            orphans: Vec::new(),
        };
        let text = render(&reply, now, Locale::En);
        assert!(text.contains("running · prompt in terminal"), "{text}");
        assert!(text.contains("running · approval: Bash"), "{text}");
    }

    #[test]
    fn a_long_waiting_command_never_widens_the_table() {
        let now = Utc::now();
        let mut asking = row("claude-1", Some("app"), HostState::Running);
        let heredoc = format!(
            "python3 - <<'EOF'\nimport json\n{}\nEOF",
            "print('x' * 400)\n".repeat(50)
        );
        asking.prompt = Some(crate::ipc::HostedPrompt {
            tool: Some(heredoc),
            since: now,
            screen: false,
        });
        let reply = SessionsReply {
            rows: vec![asking],
            local_rows: Vec::new(),
            orphans: Vec::new(),
        };
        let text = render(&reply, now, Locale::En);
        assert_eq!(text.lines().count(), 2, "one header, one row: {text}");
        let line = text.lines().nth(1).unwrap();
        assert!(
            line.contains("running · approval: python3 - <<'EOF'…"),
            "{line}"
        );
        assert!(line.ends_with("/work/refund"), "DIR still shows: {line}");
        assert!(textwrap::core::display_width(line) < 160, "{line}");
        assert_eq!(first_line_within("ls -la", 40), "ls -la");
        assert_eq!(
            first_line_within(&"a".repeat(50), 10),
            format!("{}…", "a".repeat(9))
        );
    }
}

#[cfg(test)]
#[test]
fn replace_names_the_new_seat_by_the_alias_it_took_over() {
    use senate_core::CommitteeId;
    let reply = |alias: Option<&str>| crate::ipc::ReplaceReply {
        committee: CommitteeId::new("refund").unwrap(),
        old: "claude-5bde5f64".parse().unwrap(),
        new: "claude-9b630d87".parse().unwrap(),
        session_id: "s".into(),
        steps: Vec::new(),
        alias: alias.map(|a| a.parse().unwrap()),
    };
    let named = render_replace(&reply(Some("rev")), Locale::En);
    assert!(
        named.ends_with("Open its terminal with `senate attach rev`."),
        "{named}"
    );
    let zh = render_replace(&reply(Some("rev")), Locale::Zh);
    assert!(
        zh.ends_with("用 `senate attach rev` 打开它的终端。"),
        "{zh}"
    );
    // No alias to take over: by its senator name, as before.
    let bare = render_replace(&reply(None), Locale::En);
    assert!(bare.ends_with("`senate attach claude-9b630d87`."), "{bare}");
}
