use chrono::{DateTime, Utc};
use senate_core::{CommitteeId, Locale};
use textwrap::core::display_width;

use super::{
    StatusView, ago, children_line, desk_line, home_relative, office_speaker_words, parent_line,
    render_secretary_problems, session_committee, session_push, speaker_words, stream_text,
};
use crate::ipc::StreamState;
use crate::session::SessionState;
use crate::terminal::{Terminal, printable};

/// Present the same directory-scoped snapshot for reading at a terminal.
pub fn render_terminal(
    view: &StatusView,
    now: DateTime<Utc>,
    full_ids: bool,
    scope: Option<&CommitteeId>,
    terminal: Terminal,
) -> String {
    let mut out = String::new();
    let l = view.locale;
    let roles = view.roles.as_ref();
    let senate = roles
        .and_then(|r| r.senate.as_ref())
        .or(view.senate.as_ref())
        .map_or_else(|| l.pick("未知", "unknown").into(), ToString::to_string);
    terminal.line(&mut out, &format!("Senate  {senate}"), 0, true);
    field(
        terminal,
        &mut out,
        l.pick("委员会", "Committee"),
        &scope.map_or_else(|| l.pick("全部", "all").into(), ToString::to_string),
        0,
    );
    if let Some(committee) = scope {
        let chair = roles
            .and_then(|r| r.chairs.iter().find(|c| &c.committee == committee))
            .map_or_else(
                || l.pick("未知", "unknown").into(),
                |info| chair_name(info, full_ids, l),
            );
        field(terminal, &mut out, l.pick("主席", "Chair"), &chair, 0);
    } else if let Some(roles) = roles {
        for info in &roles.chairs {
            field(
                terminal,
                &mut out,
                l.pick("主席", "Chair"),
                &format!("{}  {}", info.committee, chair_name(info, full_ids, l)),
                0,
            );
        }
    }
    let daemon = match view.daemon {
        Some(pid) => l.pick(
            format!("运行中 · pid {pid}"),
            format!("running · pid {pid}"),
        ),
        None => l
            .pick(
                "未运行 · 启动命令 senated --machine",
                "offline · start with senated --machine",
            )
            .into(),
    };
    field(terminal, &mut out, l.pick("守护进程", "Daemon"), &daemon, 0);

    out.push('\n');
    terminal.line(&mut out, l.pick("投递", "Delivery"), 0, true);
    let q = &view.queue;
    let queue = l.pick(
        format!(
            "{} 待投递 · {} 投递中 · {} 留给钩子",
            q.pending, q.inflight, q.queued
        ),
        format!(
            "{} pending · {} in flight · {} held for hooks",
            q.pending, q.inflight, q.queued
        ),
    );
    terminal.line(&mut out, &queue, 2, false);
    delivery(view, now, full_ids, scope.is_none(), terminal, &mut out);

    out.push('\n');
    let title = if view.sessions.is_empty() {
        l.pick("会话", "Sessions").to_owned()
    } else {
        let total = view.sessions.len();
        let live = view.sessions.iter().filter(|s| s.is_live()).count();
        l.pick(
            format!("会话  {live}/{total} 活跃"),
            format!("Sessions  {live}/{total} live"),
        )
    };
    terminal.line(&mut out, &title, 0, true);
    sessions(view, now, full_ids, scope.is_none(), terminal, &mut out);
    connection(view, scope, terminal, &mut out);
    out
}

fn chair_name(info: &senate_core::api::ChairInfo, full_ids: bool, l: Locale) -> String {
    info.chair.as_ref().map_or_else(
        || l.pick("暂无", "none").into(),
        |chair| format!("[senator::{}]", display_id(chair.as_str(), full_ids)),
    )
}

fn display_id(id: &str, full_ids: bool) -> String {
    // Byte offsets are safe for ASCII IDs. Preserve Unicode names in full.
    if !full_ids && id.is_ascii() && id.len() > 20 {
        format!("{}…{}", &id[..12], &id[id.len() - 6..])
    } else {
        id.to_owned()
    }
}

fn field(terminal: Terminal, out: &mut String, label: &str, value: &str, indent: usize) {
    terminal.line(out, &format!("{label}  {value}"), indent, false);
}

struct Row {
    cells: Vec<String>,
    details: Vec<(&'static str, String)>,
}

fn records(terminal: Terminal, out: &mut String, headers: &[&str], mut rows: Vec<Row>) {
    let mut widths: Vec<_> = headers.iter().map(|h| display_width(h)).collect();
    for row in &mut rows {
        for (cell, width) in row.cells.iter_mut().zip(&mut widths) {
            *cell = printable(cell).into_owned();
            *width = (*width).max(display_width(cell));
        }
    }
    let fits = widths.iter().sum::<usize>() + 2 * headers.len() <= terminal.columns();
    if fits {
        let heading = table_line(headers.iter().copied(), &widths);
        out.push_str(&terminal.emphasis(&heading));
        out.push('\n');
    }
    for (i, row) in rows.into_iter().enumerate() {
        if fits {
            out.push_str(&table_line(row.cells.iter().map(String::as_str), &widths));
            out.push('\n');
        } else {
            if i != 0 {
                out.push('\n');
            }
            for (label, cell) in headers.iter().zip(&row.cells) {
                field(terminal, out, label, cell, 2);
            }
        }
        for (label, value) in row.details {
            field(terminal, out, label, &value, 4);
        }
    }
}

fn table_line<'a>(cells: impl Iterator<Item = &'a str>, widths: &[usize]) -> String {
    let mut line = String::from("  ");
    for (index, (cell, w)) in cells.zip(widths).enumerate() {
        line.push_str(cell);
        if index + 1 < widths.len() {
            line.push_str(&" ".repeat(w - display_width(cell) + 2));
        }
    }
    line
}

fn delivery(
    view: &StatusView,
    now: DateTime<Utc>,
    full_ids: bool,
    show_committee: bool,
    terminal: Terminal,
    out: &mut String,
) {
    let l = view.locale;
    if view.committees.is_empty() {
        terminal.line(out, l.pick("没有投递流", "No delivery streams"), 2, false);
        return;
    }
    let mut headers = l.pick(
        vec!["Agent", "已接收", "已上报", "会话", "连接"],
        vec!["Agent", "Received", "Reported", "Sessions", "Stream"],
    );
    if show_committee {
        headers.insert(0, l.pick("委员会", "Committee"));
    }
    let rows = view
        .committees
        .iter()
        .map(|f| {
            let mut details = Vec::new();
            let stream = match f.stream.as_ref() {
                Some(StreamState::Backoff {
                    attempt,
                    retry_at,
                    error,
                }) => {
                    if !error.is_empty() {
                        details.push((l.pick("错误", "Error"), error.clone()));
                    }
                    let wait = (*retry_at - now).num_seconds().max(0);
                    l.pick(
                        format!("{wait} 秒后重试（{attempt}）"),
                        format!("retry in {wait}s ({attempt})"),
                    )
                }
                state => stream_text(state, now, l),
            };
            let mut cells = vec![
                format!(
                    "{}{}",
                    display_id(f.agent.as_str(), full_ids),
                    if f.configured { " *" } else { "" },
                ),
                f.delivered.to_string(),
                f.reported.to_string(),
                f.sessions.to_string(),
                stream,
            ];
            if show_committee {
                cells.insert(0, f.committee.to_string());
            }
            Row { cells, details }
        })
        .collect();
    records(terminal, out, &headers, rows);
    if view.committees.iter().any(|f| f.configured) {
        terminal.line(
            out,
            l.pick("* 在 senate.toml 中配置", "* configured in senate.toml"),
            2,
            false,
        );
    }
}

fn sessions(
    view: &StatusView,
    now: DateTime<Utc>,
    full_ids: bool,
    show_committee: bool,
    terminal: Terminal,
    out: &mut String,
) {
    let l = view.locale;
    if view.sessions.is_empty() {
        terminal.line(
            out,
            l.pick(
                "没有登记的会话 · 配置钩子见 senate install",
                "No registered sessions · see senate install",
            ),
            2,
            false,
        );
        return;
    }
    let mut headers = l.pick(
        vec!["ID", "Agent", "状态", "推送", "最近活动"],
        vec!["ID", "Agent", "State", "Push", "Last seen"],
    );
    if show_committee {
        headers.insert(2, l.pick("委员会", "Committee"));
    }
    let rows = view
        .sessions
        .iter()
        .map(|s| {
            let state = match s.state {
                SessionState::Live => l.pick("活跃", "live"),
                SessionState::Stale => l.pick("失联", "stale"),
                SessionState::Ended => l.pick("已结束", "ended"),
            };
            let mut cells = vec![
                display_id(&s.session_id, full_ids),
                display_id(s.agent.as_str(), full_ids),
                state.into(),
                session_push(s, l),
                ago(s.last_seen, now, l),
            ];
            let mut details = Vec::new();
            if show_committee {
                cells.insert(2, session_committee(s, l));
            } else if let Some(source) = s.effective_source() {
                details.push((l.pick("绑定", "Binding"), source.label(l).into()));
            }
            if let Some(branch) = &s.branch {
                details.push((l.pick("分支", "Branch"), branch.clone()));
            }
            details.push((l.pick("目录", "CWD"), home_relative(&s.cwd)));
            Row { cells, details }
        })
        .collect();
    records(terminal, out, &headers, rows);
}

fn connection(
    view: &StatusView,
    scope: Option<&CommitteeId>,
    terminal: Terminal,
    out: &mut String,
) {
    let l = view.locale;
    let roles = view.roles.as_ref();
    let office = roles.and_then(|r| r.office.as_ref());
    out.push('\n');
    terminal.line(out, l.pick("连接信息", "Connection"), 0, true);
    if let Some(url) = &view.server_url {
        field(terminal, out, l.pick("服务器", "Server"), url, 2);
    }
    if let Some(person) = &view.person {
        field(terminal, out, l.pick("身份", "Identity"), person, 2);
    }
    field(
        terminal,
        out,
        "Speaker",
        &office.map_or_else(
            || speaker_words(view.speaker.as_ref(), l),
            |o| office_speaker_words(o, l),
        ),
        2,
    );
    field(terminal, out, "Socket", &home_relative(&view.socket), 2);
    if scope.is_none()
        && let Some(roles) = roles
    {
        if let Some(office) = office {
            terminal.line(out, &desk_line(office, &roles.present, l), 2, false);
        }
        if !roles.children.is_empty() {
            terminal.line(out, &children_line(&roles.children, l), 2, false);
        }
        if let Some(uplink) = &roles.uplink {
            terminal.line(out, &parent_line(uplink, l), 2, false);
        }
    }
    let mut problems = String::new();
    render_secretary_problems(&view.secretary_problems, l, &mut problems);
    for line in problems.lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            let heading = !line.starts_with(' ');
            terminal.line(out, line.trim_start(), if heading { 0 } else { 2 }, heading);
        }
    }
}
