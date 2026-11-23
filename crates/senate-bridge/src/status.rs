//! `senate status`: is the daemon up, which Senate it is connected
//! to (by its full path: `/acme/alice-desk` for a child) and as which
//! person, its roles (the Speaker as a senator id and what kind of senator
//! it is, who works the Speaker's desk in order and who of them is present,
//! each committee's chair, the child Senates registered with it, and for a
//! child the Senate above and whether the registration was taken), which
//! agents are joined to which committees and how far each stream got, and
//! which sessions are registered (as which agent).
//!
//! With the daemon down it still reads `state.db` (read-only) so the last
//! known cursors and sessions are visible. It is written in the bridge's
//! locale.

mod terminal;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate_core::{Address, Locale, SenatePath};

use crate::config::BridgeConfig;
use crate::daemon::senate_words;
use crate::ipc::{
    self, ClientError, CommitteeStatus, HookDropStatus, Registration, Request, RolesReply,
    SecretaryProblem, SecretaryProblems, StatusReply, StreamState, UplinkStatus,
};
use crate::paths::Paths;
use crate::session::{Runtime, Session, SessionState};
use crate::state::{QueueCounts, Store};

pub use terminal::{render_brief, render_terminal};

/// Everything `status` prints.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusView {
    pub aliases: Option<senate_core::alias::KnownAliases>,
    /// The daemon's pid, when it answered.
    pub daemon: Option<u32>,
    pub socket: PathBuf,
    /// `senator::<person>`.
    pub person: Option<String>,
    /// The Senate served, by its path (`None`: not known yet).
    pub senate: Option<SenatePath>,
    pub server_url: Option<String>,
    pub committees: Vec<CommitteeStatus>,
    pub sessions: Vec<Session>,
    /// Agents whose hosted session is recorded paused.
    pub paused: std::collections::BTreeSet<senate_core::SenatorName>,
    pub hosted: std::collections::BTreeMap<String, Option<String>>,
    pub hosted_unbound: std::collections::BTreeMap<String, String>,
    pub hosted_viewer_unavailable: std::collections::BTreeSet<String>,
    pub dropped_hooks: Option<HookDropStatus>,
    pub queue: QueueCounts,
    pub locale: Locale,
    /// Who holds that Senate's Speaker's seat, as the daemon heard.
    pub speaker: Option<senate_core::SpeakerSeat>,
    /// The office, the chairs, the children and the Senate above.
    pub roles: Option<RolesReply>,
    /// Errors in the built-in session's configuration files, one per
    /// broken file and line.
    pub secretary_problems: Vec<SecretaryProblem>,
}

impl StatusView {
    pub fn retain_active(&mut self) {
        self.sessions.retain(|s| {
            let presence = (self.daemon.is_some() && s.is_live())
                .then_some(senate_core::PresenceState::Online);
            senate_core::continuity::roster_active(
                presence,
                self.hosted.contains_key(&s.session_id),
                self.paused.contains(&s.agent),
            )
        });
        self.committees.retain(|c| {
            c.is_active()
                || self
                    .sessions
                    .iter()
                    .any(|s| s.agent == c.agent && s.committee() == Some(&c.committee))
        });
    }

    fn display(
        &self,
        agent: &senate_core::SenatorName,
        committee: Option<&senate_core::CommitteeId>,
        full_ids: bool,
    ) -> String {
        if full_ids {
            return agent.to_string();
        }
        senate_core::alias::display_name(self.aliases.as_ref(), committee, agent)
    }

    fn from_reply(reply: StatusReply, socket: PathBuf) -> Self {
        Self {
            aliases: None,
            daemon: Some(reply.pid),
            socket,
            person: Some(Address::senior(&reply.person).in_text()),
            senate: reply.senate,
            server_url: Some(reply.server_url),
            committees: reply.committees,
            sessions: reply.sessions,
            paused: reply.paused,
            hosted: reply.hosted,
            hosted_unbound: reply.hosted_unbound,
            hosted_viewer_unavailable: reply.hosted_viewer_unavailable,
            dropped_hooks: reply.dropped_hooks,
            queue: reply.queue,
            locale: reply.locale,
            speaker: reply.speaker,
            roles: reply.roles,
            secretary_problems: Vec::new(),
        }
    }
}

/// The errors the machine's Senate last wrote for its built-in session's
/// configuration (they are there with the daemon up or down).
fn secretary_problems(paths: &Paths) -> Vec<SecretaryProblem> {
    SecretaryProblems::read(&paths.hosted(crate::paths::HOSTED_KEY)).problems
}

/// Ask the daemon; fall back to the database.
pub async fn collect(paths: &Paths) -> anyhow::Result<StatusView> {
    let socket = paths.socket();
    match ipc::request::<StatusReply>(
        &socket,
        &Request::Status,
        Duration::from_millis(500),
        Duration::from_secs(5),
    )
    .await
    {
        Ok(reply) => {
            let mut view = StatusView::from_reply(reply, socket);
            view.secretary_problems = secretary_problems(paths);
            Ok(view)
        }
        Err(ClientError::Connect { .. } | ClientError::Timeout) => offline(paths),
        Err(e) => Err(e.into()),
    }
}

/// The CLI's directory-scoped view. Service and app callers continue to use the
/// machine-wide snapshot. Queue totals use the daemon's local SQLite store.
pub async fn collect_for(
    paths: &Paths,
    committee: Option<&senate_core::CommitteeId>,
) -> anyhow::Result<StatusView> {
    let mut view = collect(paths).await?;
    if view.daemon.is_some() {
        view.aliases = ipc::request(
            &paths.socket(),
            &Request::Aliases,
            Duration::from_millis(500),
            Duration::from_secs(2),
        )
        .await
        .ok();
    }
    if let Some(c) = committee {
        view.committees.retain(|s| &s.committee == c);
        view.sessions.retain(|s| s.committee() == Some(c));
        if let Some(roles) = &mut view.roles {
            roles.chairs.retain(|chair| &chair.committee == c);
        }
        let filename = format!("{c}.secretary.toml");
        view.secretary_problems.retain(|item| {
            item.file.file_name().is_some_and(|name| {
                name == "secretary.toml" || name == std::ffi::OsStr::new(&filename)
            })
        });
        view.queue = if paths.state_db().try_exists()? {
            Store::open_read_only(&paths.state_db())?.counts_for(Some(c))?
        } else {
            QueueCounts::default()
        };
    }
    Ok(view)
}

/// What the files say of the roles with the daemon down: the office
/// `[local]` configures, and the registration the uplink last wrote.
fn offline_roles(paths: &Paths, config: &BridgeConfig) -> Option<RolesReply> {
    let hosted = crate::config::Hosted::load(&paths.config()).ok()?;
    hosted.local.as_ref()?;
    let uplink = UplinkStatus::read(&paths.hosted(crate::paths::HOSTED_KEY))
        .filter(|_| hosted.under_a_parent(config));
    let parent = uplink.as_ref().and_then(|u| u.parent.clone());
    let (speaker, secretaries, _) = hosted.office_config(&config.person, parent.as_ref()).ok()?;
    let name = hosted.name(&config.person);
    let senate = match &parent {
        Some(p) => p.clone().absolute().child(name).ok(),
        None => SenatePath::top(name).ok(),
    };
    let here = speaker.id.senate().is_none();
    let kind = if speaker.id.name().name() == &config.person {
        senate_core::SenatorKind::Person
    } else {
        senate_core::SenatorKind::Agent
    };
    let mut desk = Vec::new();
    if here && kind == senate_core::SenatorKind::Agent {
        desk.push(speaker.id.name().clone());
    }
    for secretary in secretaries {
        if !desk.contains(&secretary) {
            desk.push(secretary);
        }
    }
    Some(RolesReply {
        senate,
        office: Some(senate_core::Office {
            speaker: speaker.id,
            kind,
            desk,
            built_in: None,
        }),
        uplink,
        ..RolesReply::default()
    })
}

fn offline(paths: &Paths) -> anyhow::Result<StatusView> {
    let config = BridgeConfig::load(&paths.config())
        .ok()
        .or_else(|| BridgeConfig::load_unchecked(&paths.config()).ok());
    let roles = config.as_ref().and_then(|c| offline_roles(paths, c));
    let mut view = StatusView {
        aliases: None,
        daemon: None,
        socket: paths.socket(),
        person: config
            .as_ref()
            .map(|c| Address::senior(&c.person).in_text()),
        senate: roles.as_ref().and_then(|r| r.senate.clone()).or_else(|| {
            crate::config::Hosted::load(&paths.config())
                .ok()
                .filter(|h| !h.is_empty())
                .zip(config.as_ref())
                .and_then(|(h, c)| SenatePath::top(h.name(&c.person)).ok())
        }),
        server_url: config.as_ref().map(|c| c.server_url.clone()),
        committees: Vec::new(),
        sessions: Vec::new(),
        paused: Default::default(),
        hosted: Default::default(),
        hosted_unbound: Default::default(),
        hosted_viewer_unavailable: Default::default(),
        dropped_hooks: None,
        queue: QueueCounts::default(),
        locale: config
            .as_ref()
            .map_or_else(|| BridgeConfig::locale_at(&paths.config()), |c| c.locale),
        speaker: None,
        roles,
        secretary_problems: secretary_problems(paths),
    };
    if !paths.state_db().exists() {
        return Ok(view);
    }
    let store = Store::open_read_only(&paths.state_db())?;
    view.sessions = store.sessions()?;
    let host_status = crate::host::status(paths, &store, &view.sessions)?;
    view.hosted = host_status.hosted;
    view.hosted_unbound = host_status.unbound;
    view.hosted_viewer_unavailable = host_status.viewer_unavailable;
    let configured: Vec<_> = config.map(|c| c.committees).unwrap_or_default();
    view.committees = store
        .cursors()?
        .into_iter()
        .map(|(agent, committee, c)| CommitteeStatus {
            configured: configured.contains(&committee),
            sessions: view
                .sessions
                .iter()
                .filter(|s| s.is_live() && s.agent == agent && s.committee() == Some(&committee))
                .count(),
            delivered: c.delivered,
            reported: c.reported,
            stream: None,
            duty: None,
            agent,
            committee,
        })
        .collect();
    view.queue = store.counts()?;
    Ok(view)
}

fn ago(t: DateTime<Utc>, now: DateTime<Utc>, locale: Locale) -> String {
    let secs = (now - t).num_seconds().max(0);
    let (n, zh, en) = match secs {
        0..=59 => (secs, "秒前", "s ago"),
        60..=3599 => (secs / 60, "分钟前", "m ago"),
        3600..=86_399 => (secs / 3600, "小时前", "h ago"),
        _ => (secs / 86_400, "天前", "d ago"),
    };
    match locale {
        Locale::Zh => format!("{n} {zh}"),
        Locale::En => format!("{n}{en}"),
    }
}

fn stream_text(state: Option<&StreamState>, now: DateTime<Utc>, locale: Locale) -> String {
    match (state, locale) {
        (None, _) => "—".into(),
        (Some(StreamState::Connecting { attempt: 0 }), _) => {
            locale.pick("连接中", "connecting").into()
        }
        (Some(StreamState::Connecting { attempt }), Locale::Zh) => {
            format!("连接中（第 {attempt} 次尝试）")
        }
        (Some(StreamState::Connecting { attempt }), Locale::En) => {
            format!("connecting (attempt {attempt})")
        }
        (Some(StreamState::Connected { since }), Locale::Zh) => {
            format!("已连接，自 {}", ago(*since, now, locale))
        }
        (Some(StreamState::Connected { since }), Locale::En) => {
            format!("connected since {}", ago(*since, now, locale))
        }
        (
            Some(StreamState::Backoff {
                attempt,
                retry_at,
                error,
            }),
            _,
        ) => {
            let wait = (*retry_at - now).num_seconds().max(0);
            match locale {
                Locale::Zh => format!("{wait} 秒后重试（第 {attempt} 次）：{error}"),
                Locale::En => format!("retrying in {wait}s (attempt {attempt}): {error}"),
            }
        }
        (Some(StreamState::Stopped), _) => locale.pick("已停止", "stopped").into(),
    }
}

pub(crate) fn home_relative(p: &Path) -> String {
    match dirs::home_dir() {
        Some(home) => match p.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        },
        None => p.display().to_string(),
    }
}

fn session_committee(session: &Session, locale: Locale) -> String {
    match (session.committee(), session.effective_source()) {
        (Some(f), Some(src)) => format!("{f} ({})", src.label(locale)),
        (Some(f), None) => f.to_string(),
        (None, _) => locale.pick("未加入", "unbound").into(),
    }
}

fn session_push(session: &Session, locale: Locale) -> String {
    match (session.runtime, &session.socket, session.socket_ok) {
        // agy has no channel Senate can push into: it takes what is
        // waiting at its next hook.
        (Runtime::Agy, _, _) => locale.pick("钩子时", "at hooks").into(),
        (Runtime::Codex | Runtime::Acp, _, health) => {
            let runtime = session.runtime.as_str();
            match health {
                None => runtime.into(),
                Some(true) => format!("{runtime} ok"),
                Some(false) => format!("{runtime} {}", locale.pick("失败", "failed")),
            }
        }
        (Runtime::Claude, None, _) => locale.pick("无", "none").into(),
        (Runtime::Claude, Some(_), None) => locale.pick("未试过", "untested").into(),
        (Runtime::Claude, Some(_), Some(true)) => "ok".into(),
        (Runtime::Claude, Some(_), Some(false)) => locale.pick("失败", "failed").into(),
    }
}

fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if let Some(w) = widths.get_mut(i) {
                *w = (*w).max(cell.chars().count());
            }
        }
    }
    let line = |cells: Vec<&str>| {
        let mut s = String::from(" ");
        for (i, cell) in cells.iter().enumerate() {
            let w = widths.get(i).copied().unwrap_or(0);
            let pad = w.saturating_sub(cell.chars().count());
            let _ = write!(s, " {cell}{}", " ".repeat(pad));
            if i + 1 < cells.len() {
                s.push(' ');
            }
        }
        s.trim_end().to_string()
    };
    let mut out = line(headers.to_vec());
    for row in rows {
        out.push('\n');
        out.push_str(&line(row.iter().map(String::as_str).collect()));
    }
    out
}

pub(crate) fn render_committees(
    roster: &[crate::ipc::RosterCommittee],
    locale: Locale,
    full_ids: bool,
) -> String {
    let headers = locale.pick(
        ["委员会", "主席", "状态", "活跃成员", "待处理"],
        ["COMMITTEE", "CHAIR", "STATE", "ACTIVE MEMBERS", "PENDING"],
    );
    let rows: Vec<_> = roster
        .iter()
        .map(|row| {
            let chair = if full_ids {
                row.chair.as_ref().map(ToString::to_string)
            } else {
                row.chair_display_name.clone()
            };
            vec![
                row.committee.to_string(),
                chair.unwrap_or_else(|| "—".to_owned()),
                if row.chair.is_none() {
                    "—"
                } else if row.chair_live {
                    locale.pick("在线", "online")
                } else {
                    locale.pick("离线", "offline")
                }
                .to_owned(),
                row.active_members().to_string(),
                row.pending.to_string(),
            ]
        })
        .collect();
    table(&headers, &rows)
}

fn advisories(view: &StatusView) -> Vec<String> {
    let l = view.locale;
    let mut lines = Vec::new();
    if let Some(drops) = &view.dropped_hooks {
        let count = drops.count;
        let kind = drops.last.kind.event_name();
        let last = drops.last_at.format("%Y-%m-%d %H:%M:%S UTC");
        let mut summary = l.pick(
            format!("守护进程启动后丢弃了 {count} 个钩子上报。最近一次为 {last} 的 {kind}。"),
            format!(
                "Dropped hook reports since daemon start ({count}). Last was {kind} at {last}."
            ),
        );
        if let Some(host) = &drops.last.host_id {
            summary.push_str(&l.pick(
                format!(" 上报的托管主机 ID 为 {host}。"),
                format!(" Reported host ID {host}."),
            ));
        }
        lines.push(summary);
    }
    if view.sessions.iter().any(|session| {
        session.runtime == Runtime::Codex && view.hosted.contains_key(&session.session_id)
    }) {
        lines.push(l.pick(
            "在托管 Codex 中执行 /new 或 /clear 后，界面切换到新线程，但 Senate 仍向原线程投递。退出当前终端，再用 senate session restore 返回原线程。",
            "After /new or /clear in hosted Codex, Senate still delivers to the original thread, which the TUI no longer shows. Exit the TUI and use senate session restore to return.",
        ).into());
        lines.push(l.pick(
            "若恢复前 Codex 会话已在线，则关闭托管终端后该会话仍显示在线，因为守护进程无法判断是否还有原生客户端连接。",
            "A Codex session that was live before restore stays live when the hosted terminal closes because the daemon cannot tell whether a native client remains.",
        ).into());
    }
    for session in &view.sessions {
        let seat = &session.agent;
        if let Some(at) = view
            .dropped_hooks
            .as_ref()
            .and_then(|drops| drops.hosted_last_at.get(&session.session_id))
        {
            let last = at.format("%Y-%m-%d %H:%M:%S UTC");
            lines.push(l.pick(
                format!("托管 Claude 席位 {seat} 可能漏掉了工具授权或上下文钩子。最近一次钩子上报丢弃于 {last}。"),
                format!("Hosted Claude seat {seat} may have missed an approval or context hook. Its last hook report was dropped at {last}."),
            ));
        }
        if view.hosted_viewer_unavailable.contains(&session.session_id) {
            lines.push(l.pick(
                format!("托管席位 {seat} 的终端无法提供查看者活动，工具授权提醒不可用。"),
                format!("Approval notices for hosted seat {seat} are unavailable because the terminal has no readable viewer activity."),
            ));
        }
        let unbound = view.hosted_unbound.get(&session.session_id);
        if let Some(new) = unbound {
            lines.push(l.pick(
                format!("托管 Claude 席位 {seat} 无法绑定原生会话 {new}。消息暂存于原会话。运行 senate attach {seat} 检查终端，或退出终端后运行 senate session restore {seat}。"),
                format!("Hosted Claude seat {seat} could not bind native session {new}. Messages wait for the original session. Run senate attach {seat} to inspect the terminal, or exit it and run senate session restore {seat}."),
            ));
        }
        if let Some(Some(refused)) = view.hosted.get(&session.session_id)
            && Some(refused) != unbound
        {
            lines.push(l.pick(
                format!("托管 Claude 席位 {seat} 拒绝了原生会话 {refused} 的钩子上报。运行 senate attach {seat} 可检查当前会话。"),
                format!("Hosted Claude seat {seat} refused a hook reporting native session {refused}. Use senate attach {seat} to check the current conversation."),
            ));
        }
    }
    lines
}

pub fn render(view: &StatusView, now: DateTime<Utc>) -> String {
    render_with_ids(view, now, false)
}

pub fn render_with_ids(view: &StatusView, now: DateTime<Utc>, full_ids: bool) -> String {
    render_scope(view, now, full_ids, None)
}

pub fn render_scope(
    view: &StatusView,
    now: DateTime<Utc>,
    full_ids: bool,
    scope: Option<&senate_core::CommitteeId>,
) -> String {
    let l = view.locale;
    let mut out = String::new();
    if let Some(c) = scope {
        let _ = writeln!(
            out,
            "{}",
            l.pick(format!("委员会 {c}"), format!("Committee {c}"))
        );
    }
    let socket = view.socket.display();
    let _ = match (view.daemon, l) {
        (Some(pid), Locale::Zh) => writeln!(out, "daemon：运行中（pid {pid}），在 {socket}"),
        (Some(pid), Locale::En) => writeln!(out, "daemon: running (pid {pid}) on {socket}"),
        (None, Locale::Zh) => writeln!(
            out,
            "daemon：未运行（{socket} 没有应答）；用 `senated --machine` 启动"
        ),
        (None, Locale::En) => writeln!(
            out,
            "daemon: not running (no answer on {socket}); start it with `senated --machine`"
        ),
    };
    render_senate(view, now, full_ids, scope, &mut out);
    render_secretary_problems(&view.secretary_problems, l, &mut out);
    out
}

/// The built-in session's broken configuration files: one line each, the
/// file, the line and what is wrong. Nothing when none is.
fn render_secretary_problems(problems: &[SecretaryProblem], l: Locale, out: &mut String) {
    if problems.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "\n{}",
        l.pick(
            "内置会话的配置有错误（上一份可用的配置仍在生效）",
            "the built-in session's configuration has errors (the last good one stays in force)"
        )
    );
    for problem in problems {
        let file = home_relative(&problem.file);
        // One line each, whatever the parser's message spans.
        let message = problem
            .message
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let _ = match problem.line {
            Some(line) => writeln!(out, "  {file}:{line}: {message}"),
            None => writeln!(out, "  {file}: {message}"),
        };
    }
}

/// Who holds a Senate's Speaker's seat, in words.
fn speaker_words(seat: Option<&senate_core::SpeakerSeat>, locale: Locale) -> String {
    match (seat, locale) {
        (Some(senate_core::SpeakerSeat::BuiltIn), Locale::Zh) => "内置 Speaker".into(),
        (Some(senate_core::SpeakerSeat::BuiltIn), Locale::En) => "the built-in Speaker".into(),
        (Some(senate_core::SpeakerSeat::Senator(p)), _) => Address::senator(p).in_text(),
        (None, Locale::Zh) => "无人主持".into(),
        (None, Locale::En) => "nobody presides".into(),
    }
}

/// What kind of senator, in words.
fn kind_words(kind: senate_core::SenatorKind, locale: Locale) -> &'static str {
    use senate_core::SenatorKind as K;
    match kind {
        K::Person => locale.pick("人", "a person"),
        K::Team => locale.pick("团队 principal", "a team principal"),
        K::Service => locale.pick("服务 principal", "a service principal"),
        K::Agent => locale.pick("agent", "an agent"),
        K::BuiltIn => locale.pick("内置会话", "the built-in session"),
    }
}

/// The Speaker as a senator id and what kind of senator it is.
fn office_speaker_words(office: &senate_core::Office, locale: Locale) -> String {
    format!(
        "{} ({})",
        office.speaker.in_text(),
        kind_words(office.kind, locale)
    )
}

/// Who works the desk, in order, and which are present.
fn desk_line(
    office: &senate_core::Office,
    present: &[senate_core::SenatorName],
    l: Locale,
) -> String {
    if office.desk.is_empty() {
        return match l {
            Locale::Zh => format!(
                "desk：没有 secretary；交给 Speaker 的事等 {} 处理（`senate inbox`）",
                office.speaker.in_text()
            ),
            Locale::En => format!(
                "desk: no secretary; what is put to the Speaker waits for {} (`senate inbox`)",
                office.speaker.in_text()
            ),
        };
    }
    let workers: Vec<String> = office
        .desk
        .iter()
        .map(|w| {
            let state = if office.built_in.as_ref() == Some(w) {
                l.pick("内置", "built in")
            } else if present.contains(w) {
                l.pick("在场", "present")
            } else {
                l.pick("不在", "away")
            };
            format!("{} ({state})", Address::senator(w).in_text())
        })
        .collect();
    let any_present = office
        .desk
        .iter()
        .any(|w| present.contains(w) || office.built_in.as_ref() == Some(w));
    match (l, any_present) {
        (Locale::Zh, true) => format!("desk（依次，第一个在场的处理）：{}", workers.join("，")),
        (Locale::Zh, false) => format!(
            "desk（依次）：{}；都不在场，交给 Speaker 的事等 {} 处理",
            workers.join("，"),
            office.speaker.in_text()
        ),
        (Locale::En, true) => format!(
            "desk (in order, the first present acts): {}",
            workers.join(", ")
        ),
        (Locale::En, false) => format!(
            "desk (in order): {}; none is present, so what is put to the Speaker waits for {}",
            workers.join(", "),
            office.speaker.in_text()
        ),
    }
}

/// Each committee's chair.
fn chairs_line(chairs: &[senate_core::api::ChairInfo], l: Locale) -> String {
    let list: Vec<String> = chairs
        .iter()
        .map(|c| match &c.chair {
            Some(chair) => format!("{} {}", c.committee, Address::senator(chair).in_text()),
            None => format!("{} {}", c.committee, l.pick("（无）", "(none)")),
        })
        .collect();
    match l {
        Locale::Zh => format!("chair：{}", list.join("；")),
        Locale::En => format!("chairs: {}", list.join("; ")),
    }
}

/// The child Senates registered with this one.
fn children_line(children: &[senate_core::api::ChildSenate], l: Locale) -> String {
    let list: Vec<String> = children
        .iter()
        .map(|c| match l {
            Locale::Zh => format!(
                "[{}]（Speaker {}）",
                c.name,
                Address::senator(&c.speaker).in_text()
            ),
            Locale::En => format!(
                "[{}] (Speaker {})",
                c.name,
                Address::senator(&c.speaker).in_text()
            ),
        })
        .collect();
    match l {
        Locale::Zh => format!("子 Senate：{}", list.join("，")),
        Locale::En => format!("child Senates: {}", list.join(", ")),
    }
}

/// The Senate above and where the registration with it stands.
fn parent_line(uplink: &UplinkStatus, l: Locale) -> String {
    let parent = match &uplink.parent {
        Some(p) => senate_words(Some(p), l),
        None => l.pick("上一级 Senate", "the Senate above").to_owned(),
    };
    let url = &uplink.url;
    let name = &uplink.name;
    let speaker = uplink.speaker.in_text();
    match (&uplink.registration, l) {
        (Registration::Registered, Locale::Zh) => {
            format!("上一级：{parent}（{url}），已注册为子 Senate [{name}]，Speaker {speaker}")
        }
        (Registration::Registered, Locale::En) => format!(
            "parent: {parent} at {url}; registered as the child Senate [{name}], Speaker {speaker}"
        ),
        (Registration::Pending, Locale::Zh) => {
            format!("上一级：{parent}（{url}），子 Senate [{name}] 还没有注册")
        }
        (Registration::Pending, Locale::En) => {
            format!("parent: {parent} at {url}; the child Senate [{name}] is not registered yet")
        }
        (Registration::Refused { reason }, Locale::Zh) => format!(
            "上一级：{parent}（{url}），拒绝了子 Senate [{name}] 的注册（Speaker {speaker}）：{reason}"
        ),
        (Registration::Refused { reason }, Locale::En) => format!(
            "parent: {parent} at {url} refused to register the child Senate [{name}] (Speaker {speaker}): {reason}"
        ),
        (Registration::Unreachable { error }, Locale::Zh) => {
            format!("上一级：{parent}（{url}）没有应答，子 Senate [{name}] 下次连上时注册：{error}")
        }
        (Registration::Unreachable { error }, Locale::En) => format!(
            "parent: {parent} at {url} did not answer; the child Senate [{name}] registers when it does: {error}"
        ),
    }
}

/// The roles: the office, its desk, the chairs, the children, the parent.
fn render_roles(roles: &RolesReply, l: Locale, out: &mut String) {
    if let Some(office) = &roles.office {
        let _ = writeln!(out, "{}", desk_line(office, &roles.present, l));
    }
    if !roles.chairs.is_empty() {
        let _ = writeln!(out, "{}", chairs_line(&roles.chairs, l));
    }
    if !roles.children.is_empty() {
        let _ = writeln!(out, "{}", children_line(&roles.children, l));
    }
    if let Some(uplink) = &roles.uplink {
        let _ = writeln!(out, "{}", parent_line(uplink, l));
    }
}

/// One Senate the daemon serves: its server, committees, sessions, queue.
fn render_senate(
    view: &StatusView,
    now: DateTime<Utc>,
    full_ids: bool,
    scope: Option<&senate_core::CommitteeId>,
    out: &mut String,
) {
    let l = view.locale;
    if let (Some(person), Some(url)) = (&view.person, &view.server_url) {
        let senate = senate_words(
            view.roles
                .as_ref()
                .and_then(|r| r.senate.as_ref())
                .or(view.senate.as_ref()),
            l,
        );
        let speaker = match view.roles.as_ref().and_then(|r| r.office.as_ref()) {
            Some(office) => office_speaker_words(office, l),
            None => speaker_words(view.speaker.as_ref(), l),
        };
        let _ = match l {
            Locale::Zh => writeln!(
                out,
                "server：{url}，{senate}，身份 {person}；Speaker：{speaker}"
            ),
            Locale::En => writeln!(
                out,
                "server: {url}, {senate}, as {person}; Speaker: {speaker}"
            ),
        };
    }
    if let Some(roles) = &view.roles {
        if scope.is_none() {
            render_roles(roles, l, out);
        } else if !roles.chairs.is_empty() {
            let _ = writeln!(out, "{}", chairs_line(&roles.chairs, l));
        }
    }

    let _ = writeln!(out, "\n{}", l.pick("委员会", "committees"));
    if view.committees.is_empty() {
        let _ = writeln!(out, "  {}", l.pick("还没有加入任何委员会", "none joined"));
    } else {
        let rows: Vec<Vec<String>> = view
            .committees
            .iter()
            .map(|f| {
                vec![
                    format!("{}{}", f.committee, if f.configured { " *" } else { "" }),
                    if full_ids {
                        Address::senator(&f.agent).in_text()
                    } else {
                        view.display(&f.agent, Some(&f.committee), false)
                    },
                    f.delivered.to_string(),
                    f.reported.to_string(),
                    f.sessions.to_string(),
                    stream_text(f.stream.as_ref(), now, l),
                ]
            })
            .collect();
        let headers = match l {
            Locale::Zh => ["委员会", "AGENT", "已接收", "已上报", "会话", "STREAM"],
            Locale::En => [
                "COMMITTEE",
                "AGENT",
                "DELIVERED",
                "REPORTED",
                "SESSIONS",
                "STREAM",
            ],
        };
        let _ = writeln!(out, "{}", table(&headers, &rows));
        if view.committees.iter().any(|f| f.configured) {
            let _ = writeln!(
                out,
                "  {}",
                l.pick(
                    "* 列在 senate.toml 里（没有会话时也保持打开）",
                    "* listed in senate.toml (kept open with no session)"
                )
            );
        }
    }

    for advisory in advisories(view) {
        let _ = writeln!(out, "  {advisory}");
    }
    let _ = writeln!(out, "\n{}", l.pick("会话", "sessions"));
    if view.sessions.is_empty() {
        let _ = writeln!(
            out,
            "  {}",
            l.pick(
                "没有登记的会话（会话由钩子上报；见 `senate setup install`）",
                "none registered (hooks report sessions; see `senate setup install`)"
            )
        );
    } else {
        let rows: Vec<Vec<String>> = view
            .sessions
            .iter()
            .map(|s| {
                vec![
                    if full_ids {
                        s.session_id.clone()
                    } else {
                        s.session_id.chars().take(8).collect()
                    },
                    format!(
                        "{} ({})",
                        if full_ids {
                            Address::senator(&s.agent).in_text()
                        } else {
                            view.display(&s.agent, s.committee(), false)
                        },
                        s.agent_source
                    ),
                    if s.state == SessionState::Ended && view.paused.contains(&s.agent) {
                        l.pick("已暂停", "paused").to_string()
                    } else {
                        s.state.as_str().to_string()
                    },
                    session_committee(s, l),
                    s.branch.clone().unwrap_or_else(|| "—".into()),
                    home_relative(&s.cwd),
                    ago(s.last_seen, now, l),
                    session_push(s, l),
                ]
            })
            .collect();
        let headers = match l {
            Locale::Zh => [
                "ID",
                "AGENT",
                "状态",
                "委员会",
                "分支",
                "CWD",
                "最近活动",
                "SOCKET",
            ],
            Locale::En => [
                "ID",
                "AGENT",
                "STATE",
                "COMMITTEE",
                "BRANCH",
                "CWD",
                "LAST SEEN",
                "SOCKET",
            ],
        };
        let _ = writeln!(out, "{}", table(&headers, &rows));
    }
    let q = &view.queue;
    let _ = match l {
        Locale::Zh => writeln!(
            out,
            "\n队列：{} 条待投递，{} 条投递中，{} 条留给钩子",
            q.pending, q.inflight, q.queued
        ),
        Locale::En => writeln!(
            out,
            "\nqueue: {} pending, {} in flight, {} held for hooks",
            q.pending, q.inflight, q.queued
        ),
    };
}

#[cfg(test)]
mod tests {
    use senate_core::{CommitteeId, SenatorName, Seq};
    use textwrap::core::display_width;

    use super::*;
    use crate::terminal::Terminal;

    fn view(locale: Locale) -> StatusView {
        let now = Utc::now();
        let agent = |s: &str| s.parse::<SenatorName>().unwrap();
        let row = |a: &str, sessions| CommitteeStatus {
            agent: agent(a),
            committee: CommitteeId::new("refund-v2").unwrap(),
            delivered: Seq(12),
            reported: Seq(10),
            stream: Some(StreamState::Connected {
                since: now - chrono::Duration::seconds(90),
            }),
            configured: true,
            sessions,
            duty: None,
        };
        StatusView {
            aliases: None,
            daemon: None,
            socket: "/h/bridge.sock".into(),
            person: Some("[senator::alice]".into()),
            senate: None,
            server_url: Some("http://x".into()),
            committees: vec![row("claude", 1), row("codex", 0)],
            sessions: Vec::new(),
            paused: Default::default(),
            hosted: Default::default(),
            hosted_unbound: Default::default(),
            hosted_viewer_unavailable: Default::default(),
            dropped_hooks: None,
            queue: QueueCounts::default(),
            locale,
            speaker: None,
            roles: None,
            secretary_problems: Vec::new(),
        }
    }

    #[test]
    fn the_senate_served_is_shown_by_its_path_with_who_presides() {
        // Replaces "every Senate served is shown with who presides": a
        // machine serves one Senate; it is named by its path.
        let now = Utc::now();
        let mut v = view(Locale::En);
        v.senate = Some("acme/north-desk".parse().unwrap());
        v.speaker = Some(senate_core::SpeakerSeat::Senator("claude".parse().unwrap()));
        let text = render(&v, now);
        assert!(
            text.contains(
                "server: http://x, the Senate [acme/north-desk], as [senator::alice]; Speaker: [senator::claude]"
            ),
            "{text}"
        );
        assert_eq!(text.matches("daemon:").count(), 1);
    }

    #[test]
    fn broken_secretary_files_are_one_line_each_in_both_languages() {
        let now = Utc::now();
        let problems = vec![
            SecretaryProblem {
                file: "/h/secretary.toml".into(),
                line: Some(3),
                message: "mode must be one of".into(),
            },
            SecretaryProblem {
                file: "/h/refund-v2.secretary.toml".into(),
                line: None,
                message: "cannot be\nread".into(),
            },
        ];
        for locale in Locale::ALL {
            let mut v = view(locale);
            assert!(
                !render(&v, now).contains("secretary.toml"),
                "none when fine"
            );
            v.secretary_problems = problems.clone();
            let text = render(&v, now);
            assert!(
                text.contains("/h/secretary.toml:3: mode must be one of"),
                "{text}"
            );
            assert!(
                text.contains("/h/refund-v2.secretary.toml: cannot be read"),
                "{text}"
            );
            assert_eq!(text.matches("secretary.toml").count(), 2, "{text}");
        }
        let (mut zh, mut en) = (view(Locale::Zh), view(Locale::En));
        zh.secretary_problems = problems.clone();
        en.secretary_problems = problems;
        assert!(render(&zh, now).contains("内置会话的配置有错误"));
        assert!(render(&en, now).contains("the built-in session's configuration has errors"));
    }

    #[test]
    fn the_problems_file_round_trips_and_is_removed_when_there_are_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(SecretaryProblems::read(dir.path()).problems.is_empty());
        let broken = SecretaryProblems {
            problems: vec![SecretaryProblem {
                file: "secretary.toml".into(),
                line: Some(2),
                message: "bad".into(),
            }],
        };
        broken.write(dir.path());
        assert_eq!(SecretaryProblems::read(dir.path()), broken);
        SecretaryProblems::default().write(dir.path());
        assert!(!dir.path().join(SecretaryProblems::FILE).exists());
        assert_eq!(
            SecretaryProblems::read(dir.path()),
            SecretaryProblems::default()
        );
    }

    #[test]
    fn renders_offline_view() {
        let now = Utc::now();
        let text = render(&view(Locale::En), now);
        assert!(text.contains("daemon: not running"));
        assert!(text.contains("server: http://x, this Senate, as [senator::alice]"));
        assert!(text.contains("refund-v2 *"));
        assert!(text.contains("claude"));
        assert!(text.contains("codex"));
        assert!(text.contains("connected since 1m ago"));
        assert!(text.contains("none registered"));
    }

    fn child_roles(registration: Registration, present: &[&str]) -> RolesReply {
        let name = |s: &str| s.parse::<SenatorName>().unwrap();
        RolesReply {
            senate: Some("/acme/alice-desk".parse().unwrap()),
            office: Some(senate_core::Office {
                speaker: "/acme::senator::alice".parse().unwrap(),
                kind: senate_core::SenatorKind::Person,
                desk: vec![name("claude"), name("codex")],
                built_in: None,
            }),
            present: present.iter().map(|p| name(p)).collect(),
            chairs: vec![senate_core::api::ChairInfo {
                committee: CommitteeId::new("refund-v2").unwrap(),
                chair: Some(name("codex")),
                source: None,
            }],
            children: vec![senate_core::api::ChildSenate::new(
                senate_core::Name::new("bob-desk").unwrap(),
                name("bob"),
            )],
            uplink: Some(UplinkStatus {
                url: "https://senate.example.com".into(),
                parent: Some("/acme".parse().unwrap()),
                name: senate_core::Name::new("alice-desk").unwrap(),
                speaker: "/acme::senator::alice".parse().unwrap(),
                registration,
                at: Utc::now(),
            }),
        }
    }

    #[test]
    fn a_child_senate_shows_its_path_office_desk_chairs_children_and_parent() {
        let now = Utc::now();
        let mut v = view(Locale::En);
        v.roles = Some(child_roles(Registration::Registered, &["codex"]));
        let text = render(&v, now);
        assert!(
            text.contains(
                "server: http://x, the Senate [/acme/alice-desk], as [senator::alice]; Speaker: [/acme::senator::alice] (a person)"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "desk (in order, the first present acts): [senator::claude] (away), [senator::codex] (present)"
            ),
            "{text}"
        );
        assert!(
            text.contains("chairs: refund-v2 [senator::codex]"),
            "{text}"
        );
        assert!(
            text.contains("child Senates: [bob-desk] (Speaker [senator::bob])"),
            "{text}"
        );
        assert!(
            text.contains(
                "parent: the Senate [/acme] at https://senate.example.com; registered as the child Senate [alice-desk], Speaker [/acme::senator::alice]"
            ),
            "{text}"
        );

        let mut v = view(Locale::Zh);
        v.roles = Some(child_roles(
            Registration::Refused {
                reason: "not on the roster".into(),
            },
            &[],
        ));
        let text = render(&v, now);
        assert!(
            text.contains("Senate [/acme/alice-desk]，身份 [senator::alice]；Speaker：[/acme::senator::alice] (人)"),
            "{text}"
        );
        assert!(text.contains("都不在场"), "{text}");
        assert!(text.contains("chair：refund-v2 [senator::codex]"), "{text}");
        assert!(text.contains("子 Senate：[bob-desk]"), "{text}");
        assert!(
            text.contains("拒绝了子 Senate [alice-desk] 的注册（Speaker [/acme::senator::alice]）：not on the roster"),
            "{text}"
        );
    }

    #[test]
    fn a_desk_with_no_secretary_waits_for_the_speaker_in_both_languages() {
        let office = senate_core::Office {
            speaker: "senator::alice".parse().unwrap(),
            kind: senate_core::SenatorKind::Person,
            desk: Vec::new(),
            built_in: None,
        };
        assert_eq!(
            desk_line(&office, &[], Locale::En),
            "desk: no secretary; what is put to the Speaker waits for [senator::alice] (`senate inbox`)"
        );
        assert!(desk_line(&office, &[], Locale::Zh).contains("没有 secretary"));
        // An agent Speaker of a root is said to be one.
        let agent = senate_core::Office {
            speaker: "senator::claude".parse().unwrap(),
            kind: senate_core::SenatorKind::Agent,
            desk: vec!["claude".parse().unwrap()],
            built_in: None,
        };
        assert_eq!(
            office_speaker_words(&agent, Locale::En),
            "[senator::claude] (an agent)"
        );
        assert_eq!(
            office_speaker_words(&agent, Locale::Zh),
            "[senator::claude] (agent)"
        );
        for (registration, en, zh) in [
            (Registration::Pending, "is not registered yet", "还没有注册"),
            (
                Registration::Unreachable {
                    error: "timeout".into(),
                },
                "did not answer",
                "没有应答",
            ),
        ] {
            let mut roles = child_roles(registration, &[]);
            let uplink = roles.uplink.take().unwrap();
            assert!(parent_line(&uplink, Locale::En).contains(en));
            assert!(parent_line(&uplink, Locale::Zh).contains(zh));
        }
    }

    #[test]
    fn renders_in_chinese_and_names_the_senate_by_its_path() {
        let now = Utc::now();
        let mut v = view(Locale::Zh);
        v.senate = Some("alice-desk".parse().unwrap());
        let text = render(&v, now);
        assert!(text.contains("daemon：未运行"), "{text}");
        assert!(
            text.contains("Senate [alice-desk]，身份 [senator::alice]"),
            "{text}"
        );
        assert!(text.contains("已连接，自 1 分钟前"), "{text}");
        assert!(text.contains("没有登记的会话"), "{text}");
    }

    #[test]
    fn acp_shows_the_last_push_result_and_agy_its_hooks_in_both_status_views() {
        let now = Utc::now();
        for locale in [Locale::En, Locale::Zh] {
            let mut view = view(locale);
            let session: Session = serde_json::from_value(serde_json::json!({
                "session_id": "example-thread", "agent": "tester", "agent_source": "session",
                "cwd": "/work", "state": "live", "started_at": now, "last_seen": now,
            }))
            .unwrap();
            view.sessions.push(session);
            for runtime in [Runtime::Agy, Runtime::Acp] {
                for (health, suffix) in [
                    (None, ""),
                    (Some(true), " ok"),
                    (Some(false), locale.pick(" 失败", " failed")),
                ] {
                    view.sessions[0].runtime = runtime;
                    view.sessions[0].socket_ok = health;
                    let expected = if runtime == Runtime::Agy {
                        locale.pick("钩子时", "at hooks").to_owned()
                    } else {
                        format!("{}{suffix}", runtime.as_str())
                    };
                    let pipe = render(&view, now);
                    let tty = render_terminal(&view, now, false, None, Terminal::new(24, false));
                    assert!(
                        pipe.lines().any(|l| l.trim_end().ends_with(&expected)),
                        "{pipe}"
                    );
                    let push_field = format!("{}  {expected}", locale.pick("推送", "Push"));
                    assert!(tty.lines().any(|l| l.trim() == push_field), "{tty}");
                }
            }
        }
    }

    #[test]
    fn terminal_status_aligns_chinese_columns_and_wraps_details_without_losing_full_ids() {
        let now = Utc::now();
        let mut view = view(Locale::Zh);
        view.roles = Some(child_roles(Registration::Registered, &[]));
        view.committees[0].stream = Some(StreamState::Backoff {
            attempt: 2,
            retry_at: now + chrono::Duration::seconds(3),
            error: "连接失败\n\x1b[31m本地苏醒后连接失败".into(),
        });
        let id = "01a1eafc-3ba0-7a21-85d4-1b1afa3e931d";
        view.sessions.push(
            serde_json::from_value(serde_json::json!({
                "session_id": id,
                "runtime": "codex",
                "agent": format!("codex-{id}"),
                "branch": "senate/终端",
                "agent_source": "session",
                "cwd": "/项目/非常长的中文目录/同一个目录前缀/应用",
                "state": "live",
                "inferred_committee": "refund-v2",
                "committee_source": "file",
                "started_at": now,
                "last_seen": now,
                "socket_ok": true,
            }))
            .unwrap(),
        );
        let committee = CommitteeId::new("refund-v2").unwrap();
        let wide = render_terminal(
            &view,
            now,
            false,
            Some(&committee),
            Terminal::new(220, false),
        );
        let mut lines = wide.lines();
        assert_eq!(lines.next(), Some("Senate  /acme/alice-desk"));
        assert_eq!(lines.next(), Some("委员会  refund-v2"));
        assert_eq!(lines.next(), Some("主席  codex"));
        let header = wide
            .lines()
            .find(|line| line.contains("已接收") && line.contains("已上报"))
            .unwrap();
        let row = wide
            .lines()
            .find(|line| line.contains("claude *") && line.contains("12"))
            .unwrap();
        assert_eq!(
            display_width(&header[..header.find("已接收").unwrap()]),
            display_width(&row[..row.find("12").unwrap()]),
        );
        let session = wide
            .lines()
            .find(|line| line.contains("senate/终端"))
            .unwrap();
        assert!(session.contains(id) && session.contains(&format!("codex-{}", &id[..8])));
        assert!(session.contains("/项目/非常长的中文目录/同一个目录前缀/应用"));
        let headers = wide.lines().find(|line| line.contains("最近活动")).unwrap();
        assert!(headers.contains("绑定") && headers.contains("分支") && headers.contains("目录"));

        let medium = render_terminal(
            &view,
            now,
            false,
            Some(&committee),
            Terminal::new(80, false),
        );
        assert!(
            medium.contains(id) && medium.contains(&format!("codex-{}", &id[..8])),
            "{medium}"
        );
        assert!(medium.contains("目录  /项目/"), "{medium}");
        assert!(medium.lines().all(|line| display_width(line) <= 80));
        for columns in [24, 40] {
            let narrow = render_terminal(
                &view,
                now,
                true,
                Some(&committee),
                Terminal::new(columns, false),
            );
            assert!(
                narrow.lines().all(|line| display_width(line) <= columns),
                "{narrow}"
            );
            assert!(!narrow.contains('\x1b'));
            assert!(
                narrow.contains("已接收  12") && narrow.contains("codex ok"),
                "{narrow}"
            );
            assert!(
                narrow.split_whitespace().collect::<String>().contains(id),
                "{narrow}"
            );
        }
    }
}
