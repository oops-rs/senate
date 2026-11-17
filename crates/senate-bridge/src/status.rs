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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate_core::{Address, Locale, SenatePath};

use crate::config::BridgeConfig;
use crate::daemon::senate_words;
use crate::ipc::{
    self, ClientError, CommitteeStatus, Registration, Request, RolesReply, SecretaryProblem,
    SecretaryProblems, StatusReply, StreamState, UplinkStatus,
};
use crate::paths::Paths;
use crate::session::{Runtime, Session};
use crate::state::{QueueCounts, Store};

/// Everything `status` prints.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusView {
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
    fn from_reply(reply: StatusReply, socket: PathBuf) -> Self {
        Self {
            daemon: Some(reply.pid),
            socket,
            person: Some(Address::senior(&reply.person).in_text()),
            senate: reply.senate,
            server_url: Some(reply.server_url),
            committees: reply.committees,
            sessions: reply.sessions,
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

fn home_relative(p: &Path) -> String {
    match dirs::home_dir() {
        Some(home) => match p.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => p.display().to_string(),
        },
        None => p.display().to_string(),
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

pub fn render(view: &StatusView, now: DateTime<Utc>) -> String {
    let l = view.locale;
    let mut out = String::new();
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
    render_senate(view, now, &mut out);
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
fn render_senate(view: &StatusView, now: DateTime<Utc>, out: &mut String) {
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
        render_roles(roles, l, out);
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
                    Address::senator(&f.agent).in_text(),
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

    let _ = writeln!(out, "\n{}", l.pick("会话", "sessions"));
    if view.sessions.is_empty() {
        let _ = writeln!(
            out,
            "  {}",
            l.pick(
                "没有登记的会话（会话由钩子上报；见 `senate install`）",
                "none registered (hooks report sessions; see `senate install`)"
            )
        );
    } else {
        let rows: Vec<Vec<String>> = view
            .sessions
            .iter()
            .map(|s| {
                let committee = match (s.committee(), s.effective_source()) {
                    (Some(f), Some(src)) => format!("{f} ({})", src.label(l)),
                    (Some(f), None) => f.to_string(),
                    (None, _) => l.pick("未加入", "unbound").into(),
                };
                let socket = match (s.runtime, &s.socket, s.socket_ok) {
                    (Runtime::Codex, _, None) => "codex".to_string(),
                    (Runtime::Codex, _, Some(true)) => "codex ok".to_string(),
                    (Runtime::Codex, _, Some(false)) => {
                        format!("codex {}", l.pick("失败", "failed"))
                    }
                    (Runtime::Claude, None, _) => l.pick("无", "none").into(),
                    (Runtime::Claude, Some(_), None) => l.pick("未试过", "untested").into(),
                    (Runtime::Claude, Some(_), Some(true)) => "ok".into(),
                    (Runtime::Claude, Some(_), Some(false)) => l.pick("失败", "failed").into(),
                };
                vec![
                    s.session_id.chars().take(8).collect(),
                    format!(
                        "{} ({})",
                        Address::senator(&s.agent).in_text(),
                        s.agent_source
                    ),
                    s.state.as_str().to_string(),
                    committee,
                    s.branch.clone().unwrap_or_else(|| "—".into()),
                    home_relative(&s.cwd),
                    ago(s.last_seen, now, l),
                    socket,
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

    use super::*;

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
        };
        StatusView {
            daemon: None,
            socket: "/h/bridge.sock".into(),
            person: Some("[senator::alice]".into()),
            senate: None,
            server_url: Some("http://x".into()),
            committees: vec![row("claude", 1), row("codex", 0)],
            sessions: Vec::new(),
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
        assert!(text.contains("[senator::claude]"));
        assert!(text.contains("[senator::codex]"));
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
            children: vec![senate_core::api::ChildSenate {
                name: senate_core::Name::new("bob-desk").unwrap(),
                speaker: name("bob"),
            }],
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
}
