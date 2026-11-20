use chrono::{DateTime, Utc};
use senate_core::{CommitteeId, Locale};
use textwrap::core::display_width;

use super::{
    StatusView, advisories, ago, children_line, desk_line, home_relative, office_speaker_words,
    parent_line, render_secretary_problems, session_committee, session_push, speaker_words,
    stream_text,
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
    let chair_label = l.pick("主席", "Chair");
    let chair_width = if full_ids {
        usize::MAX
    } else {
        terminal
            .columns()
            .saturating_sub(display_width(chair_label) + 2)
    };
    if let Some(committee) = scope {
        let chair = roles
            .and_then(|r| r.chairs.iter().find(|c| &c.committee == committee))
            .map_or_else(
                || l.pick("未知", "unknown").into(),
                |info| chair_name(info, "", chair_width, l),
            );
        field(terminal, &mut out, chair_label, &chair, 0);
    } else if let Some(roles) = roles {
        for info in &roles.chairs {
            field(
                terminal,
                &mut out,
                chair_label,
                &chair_name(info, &format!("{}  ", info.committee), chair_width, l),
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
    for advisory in advisories(view) {
        terminal.line(&mut out, &advisory, 2, false);
    }

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
    problems(view, terminal, &mut out);
    out
}

/// The everyday view: whether the daemon runs, and the sessions that are
/// here. It leaves out each agent's delivery counters, ended sessions (it
/// says how many), bindings and branches, and the connection details;
/// [`render_terminal`] has them all.
pub fn render_brief(
    view: &StatusView,
    now: DateTime<Utc>,
    full_ids: bool,
    scope: Option<&CommitteeId>,
    terminal: Terminal,
) -> String {
    let l = view.locale;
    let roles = view.roles.as_ref();
    let name = |id: &str| {
        if full_ids {
            id.to_owned()
        } else {
            crate::terminal::brief_name(id).into_owned()
        }
    };
    let senate = roles
        .and_then(|r| r.senate.as_ref())
        .or(view.senate.as_ref())
        .map_or_else(|| l.pick("未知", "unknown").into(), ToString::to_string);
    let mut rows = vec![
        ("Senate", senate),
        (
            l.pick("委员会", "Committee"),
            scope.map_or_else(|| l.pick("全部", "all").into(), ToString::to_string),
        ),
    ];
    if let Some(committee) = scope {
        let chair = roles
            .and_then(|r| r.chairs.iter().find(|c| &c.committee == committee))
            .map_or_else(
                || l.pick("未知", "unknown").to_owned(),
                |info| {
                    info.chair.as_ref().map_or_else(
                        || l.pick("暂无", "none").to_owned(),
                        |chair| name(chair.as_str()),
                    )
                },
            );
        rows.push((l.pick("主席", "Chair"), chair));
    }
    rows.push((
        l.pick("守护进程", "Daemon"),
        match view.daemon {
            Some(_) => l.pick("运行中", "running"),
            None => l.pick(
                "未运行 · 启动命令 senated --machine",
                "offline · start with senated --machine",
            ),
        }
        .to_owned(),
    ));
    let q = &view.queue;
    let retrying = view
        .committees
        .iter()
        .filter(|f| matches!(f.stream, Some(StreamState::Backoff { .. })))
        .count();
    let mut delivery = l.pick(
        format!(
            "{} 待投递 · {} 投递中 · {} 留给钩子",
            q.pending, q.inflight, q.queued
        ),
        format!(
            "{} pending · {} in flight · {} held for hooks",
            q.pending, q.inflight, q.queued
        ),
    );
    if retrying > 0 {
        delivery.push_str(&l.pick(
            format!(" · {retrying} 个连接在重试"),
            format!(" · {retrying} stream(s) retrying"),
        ));
    }
    rows.push((l.pick("投递", "Delivery"), delivery));
    let mut out = crate::terminal::fields(Some(terminal), &rows);
    for advisory in advisories(view) {
        terminal.line(&mut out, &advisory, 0, false);
    }

    out.push('\n');
    let count = |state| view.sessions.iter().filter(|s| s.state == state).count();
    let (live, stale, ended) = (
        count(SessionState::Live),
        count(SessionState::Stale),
        count(SessionState::Ended),
    );
    let mut title = l.pick(
        format!("会话  {live} 活跃"),
        format!("Sessions  {live} live"),
    );
    if stale > 0 {
        title.push_str(&l.pick(format!(" · {stale} 失联"), format!(" · {stale} stale")));
    }
    if ended > 0 {
        title.push_str(&l.pick(
            format!(" · {ended} 已结束（未列出）"),
            format!(" · {ended} ended (not listed)"),
        ));
    }
    terminal.line(&mut out, &title, 0, true);
    let cwd = l.pick("目录", "CWD");
    let mut headers = l.pick(
        vec!["Agent", "状态", "最近活动"],
        vec!["Agent", "State", "Last seen"],
    );
    if scope.is_none() {
        headers.insert(1, l.pick("委员会", "Committee"));
    }
    let rows: Vec<Row> = view
        .sessions
        .iter()
        .filter(|s| s.state != SessionState::Ended)
        .map(|s| {
            let mut state = match s.state {
                SessionState::Live => l.pick("活跃", "live"),
                SessionState::Stale => l.pick("失联", "stale"),
                SessionState::Ended => l.pick("已结束", "ended"),
            }
            .to_owned();
            if s.socket_ok == Some(false) {
                state.push_str(l.pick(" · 推送失败", " · push failed"));
            }
            let mut cells = vec![
                Cell::Text(name(s.agent.as_str())),
                Cell::Text(state),
                Cell::Text(ago(s.last_seen, now, l)),
            ];
            if scope.is_none() {
                cells.insert(1, Cell::Text(session_committee(s, l)));
            }
            Row {
                cells,
                details: vec![(cwd, home_relative(&s.cwd))],
            }
        })
        .collect();
    if rows.is_empty() {
        terminal.line(
            &mut out,
            l.pick(
                "没有活跃的会话 · 配置钩子见 senate install",
                "No live sessions · see senate install",
            ),
            2,
            false,
        );
    } else {
        records(terminal, &mut out, headers, rows, full_ids, &[cwd]);
    }
    problems(view, terminal, &mut out);
    out.push('\n');
    terminal.line(
        &mut out,
        l.pick(
            "--verbose 显示每个 agent 的投递、会话细节和连接信息。",
            "--verbose adds each agent's delivery, session details and the connection.",
        ),
        0,
        false,
    );
    out
}

const COMPACT_ID_WIDTH: usize = 19;

fn chair_name(
    info: &senate_core::api::ChairInfo,
    prefix: &str,
    width: usize,
    locale: Locale,
) -> String {
    let Some(chair) = &info.chair else {
        return format!("{prefix}{}", locale.pick("暂无", "none"));
    };
    let id_width = width.saturating_sub(display_width(prefix) + "[senator::]".len());
    format!(
        "{prefix}[senator::{}]",
        display_id(chair.as_str(), id_width)
    )
}

fn display_id(id: &str, width: usize) -> String {
    let width = width.max(COMPACT_ID_WIDTH);
    if id.is_ascii() && id.len() > width {
        format!("{}…{}", &id[..width - 7], &id[id.len() - 6..])
    } else {
        id.to_owned()
    }
}

fn field(terminal: Terminal, out: &mut String, label: &str, value: &str, indent: usize) {
    terminal.line(out, &format!("{label}  {value}"), indent, false);
}

/// ID cells may abbreviate their value. A suffix such as the configured marker
/// always keeps its own space.
enum Cell {
    Text(String),
    Id(String, &'static str),
}

impl Cell {
    fn fit(&self, width: usize) -> String {
        match self {
            Self::Text(text) => printable(text).into_owned(),
            Self::Id(id, suffix) => {
                let id = printable(id);
                format!(
                    "{}{suffix}",
                    display_id(&id, width.saturating_sub(suffix.len()))
                )
            }
        }
    }
}

struct Row {
    cells: Vec<Cell>,
    details: Vec<(&'static str, String)>,
}

fn records(
    terminal: Terminal,
    out: &mut String,
    mut headers: Vec<&'static str>,
    mut rows: Vec<Row>,
    full_ids: bool,
    optional_columns: &[&'static str],
) {
    // Start with the smallest useful ID in each column, or the complete ID
    // when explicitly requested. Measure in display columns for both locales.
    let mut widths: Vec<_> = headers.iter().map(|h| display_width(h)).collect();
    let mut natural = widths.clone();
    let min_width = if full_ids { usize::MAX } else { 0 };
    for row in &rows {
        for (i, cell) in row.cells.iter().enumerate() {
            widths[i] = widths[i].max(display_width(&cell.fit(min_width)));
            natural[i] = natural[i].max(display_width(&cell.fit(usize::MAX)));
        }
    }
    let minimum = widths.iter().sum::<usize>() + 2 * headers.len();
    let fits = minimum <= terminal.columns();
    if fits {
        let mut spare = terminal.columns() - minimum;
        // Share space between abbreviated columns. Satisfy smaller deficits
        // first so one short ID never strands space a longer ID could use.
        let mut elastic: Vec<_> = (0..widths.len())
            .filter(|&i| natural[i] > widths[i])
            .collect();
        elastic.sort_by_key(|&i| natural[i] - widths[i]);
        for (pos, &i) in elastic.iter().enumerate() {
            let extra = (natural[i] - widths[i]).min(spare.div_ceil(elastic.len() - pos));
            widths[i] += extra;
            spare -= extra;
        }

        for &label in optional_columns {
            let Some(max_value) = rows
                .iter()
                .flat_map(|row| &row.details)
                .filter(|(key, _)| *key == label)
                .map(|(_, value)| display_width(&printable(value)))
                .max()
            else {
                continue;
            };
            let width = display_width(label).max(max_value);
            if width + 2 > spare {
                continue;
            }
            headers.push(label);
            widths.push(width);
            spare -= width + 2;
            for row in &mut rows {
                let value = row
                    .details
                    .iter()
                    .position(|(key, _)| *key == label)
                    .map_or_else(|| "—".into(), |pos| row.details.remove(pos).1);
                row.cells.push(Cell::Text(value));
            }
        }
        let heading = table_line(headers.iter().copied(), &widths);
        out.push_str(&terminal.emphasis(&heading));
        out.push('\n');
    }
    for (i, row) in rows.into_iter().enumerate() {
        if fits {
            let values: Vec<_> = row
                .cells
                .iter()
                .zip(&widths)
                .map(|(cell, &w)| cell.fit(w))
                .collect();
            out.push_str(&table_line(values.iter().map(String::as_str), &widths));
            out.push('\n');
        } else {
            if i != 0 {
                out.push('\n');
            }
            for (label, cell) in headers.iter().zip(&row.cells) {
                let available = if full_ids {
                    usize::MAX
                } else {
                    terminal.columns().saturating_sub(4 + display_width(label))
                };
                field(terminal, out, label, &cell.fit(available), 2);
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
                Cell::Id(f.agent.to_string(), if f.configured { " *" } else { "" }),
                Cell::Text(f.delivered.to_string()),
                Cell::Text(f.reported.to_string()),
                Cell::Text(f.sessions.to_string()),
                Cell::Text(stream),
            ];
            if show_committee {
                cells.insert(0, Cell::Text(f.committee.to_string()));
            }
            Row { cells, details }
        })
        .collect();
    records(terminal, out, headers, rows, full_ids, &[]);
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
    let binding = l.pick("绑定", "Binding");
    let branch_label = l.pick("分支", "Branch");
    let cwd = l.pick("目录", "CWD");
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
                Cell::Id(s.session_id.clone(), ""),
                Cell::Id(s.agent.to_string(), ""),
                Cell::Text(state.into()),
                Cell::Text(session_push(s, l)),
                Cell::Text(ago(s.last_seen, now, l)),
            ];
            let mut details = Vec::new();
            if show_committee {
                cells.insert(2, Cell::Text(session_committee(s, l)));
            } else if let Some(source) = s.effective_source() {
                details.push((binding, source.label(l).into()));
            }
            if let Some(branch) = &s.branch {
                details.push((branch_label, branch.clone()));
            }
            details.push((cwd, home_relative(&s.cwd)));
            Row { cells, details }
        })
        .collect();
    records(
        terminal,
        out,
        headers,
        rows,
        full_ids,
        &[binding, branch_label, cwd],
    );
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
}

fn problems(view: &StatusView, terminal: Terminal, out: &mut String) {
    let mut problems = String::new();
    render_secretary_problems(&view.secretary_problems, view.locale, &mut problems);
    for line in problems.lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            let heading = !line.starts_with(' ');
            terminal.line(out, line.trim_start(), if heading { 0 } else { 2 }, heading);
        }
    }
}
