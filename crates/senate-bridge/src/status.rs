//! `senate-bridge status`: is the daemon up, which Senate it is connected
//! to and as which person, which agents are joined to which committees and
//! how far each stream got, and which sessions are registered (as which
//! agent).
//!
//! With the daemon down it still reads `state.db` (read-only) so the last
//! known cursors and sessions are visible. It is written in the bridge's
//! locale.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate_core::{Address, Locale, SenatorPath};

use crate::config::BridgeConfig;
use crate::daemon::senate_words;
use crate::ipc::{self, ClientError, CommitteeStatus, Request, StatusReply, StreamState};
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
    /// The Senate connected to, by its presider (`None`: the top one, or
    /// not known).
    pub presider: Option<SenatorPath>,
    pub server_url: Option<String>,
    pub committees: Vec<CommitteeStatus>,
    pub sessions: Vec<Session>,
    pub queue: QueueCounts,
    pub locale: Locale,
}

impl StatusView {
    fn from_reply(reply: StatusReply, socket: PathBuf) -> Self {
        Self {
            daemon: Some(reply.pid),
            socket,
            person: Some(Address::senior(&reply.person).in_text()),
            presider: reply.presider,
            server_url: Some(reply.server_url),
            committees: reply.committees,
            sessions: reply.sessions,
            queue: reply.queue,
            locale: reply.locale,
        }
    }
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
        Ok(reply) => Ok(StatusView::from_reply(reply, socket)),
        Err(ClientError::Connect { .. } | ClientError::Timeout) => offline(paths),
        Err(e) => Err(e.into()),
    }
}

fn offline(paths: &Paths) -> anyhow::Result<StatusView> {
    let config = BridgeConfig::load(&paths.config()).ok();
    let mut view = StatusView {
        daemon: None,
        socket: paths.socket(),
        person: config
            .as_ref()
            .map(|c| Address::senior(&c.person).in_text()),
        presider: None,
        server_url: config.as_ref().map(|c| c.server_url.clone()),
        committees: Vec::new(),
        sessions: Vec::new(),
        queue: QueueCounts::default(),
        locale: config
            .as_ref()
            .map_or_else(|| BridgeConfig::locale_at(&paths.config()), |c| c.locale),
    };
    if !paths.state_db().exists() {
        return Ok(view);
    }
    let store = Store::open_read_only(&paths.state_db())?;
    if let Some(url) = &view.server_url {
        view.presider = store.seating(url.trim_end_matches('/'))?.flatten();
    }
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
            "daemon：未运行（{socket} 没有应答）；用 `senate-bridge daemon` 启动"
        ),
        (None, Locale::En) => writeln!(
            out,
            "daemon: not running (no answer on {socket}); start it with `senate-bridge daemon`"
        ),
    };
    if let (Some(person), Some(url)) = (&view.person, &view.server_url) {
        let senate = senate_words(view.presider.as_ref(), l);
        let _ = match l {
            Locale::Zh => writeln!(out, "server：{url}，{senate}，身份 {person}"),
            Locale::En => writeln!(out, "server: {url}, {senate}, as {person}"),
        };
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
                    "* 列在 bridge.toml 里（没有会话时也保持打开）",
                    "* listed in bridge.toml (kept open with no session)"
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
                "没有登记的会话（会话由钩子上报；见 `senate-bridge install`）",
                "none registered (hooks report sessions; see `senate-bridge install`)"
            )
        );
    } else {
        let rows: Vec<Vec<String>> = view
            .sessions
            .iter()
            .map(|s| {
                let committee = match (s.committee(), s.effective_source()) {
                    (Some(f), Some(src)) => format!("{f} ({})", src.as_str()),
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
    out
}

#[cfg(test)]
mod tests {
    use senate_core::{CommitteeId, Seq};

    use super::*;

    fn view(locale: Locale) -> StatusView {
        let now = Utc::now();
        let agent = |s: &str| s.parse::<SenatorPath>().unwrap();
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
            presider: None,
            server_url: Some("http://x".into()),
            committees: vec![row("alice/claude", 1), row("alice/codex", 0)],
            sessions: Vec::new(),
            queue: QueueCounts::default(),
            locale,
        }
    }

    #[test]
    fn renders_offline_view() {
        let now = Utc::now();
        let text = render(&view(Locale::En), now);
        assert!(text.contains("daemon: not running"));
        assert!(text.contains("server: http://x, the top Senate, as [senator::alice]"));
        assert!(text.contains("refund-v2 *"));
        assert!(text.contains("[senator::alice/claude]"));
        assert!(text.contains("[senator::alice/codex]"));
        assert!(text.contains("connected since 1m ago"));
        assert!(text.contains("none registered"));
    }

    #[test]
    fn renders_in_chinese_and_names_a_nested_senate() {
        let now = Utc::now();
        let mut v = view(Locale::Zh);
        v.presider = Some("alice/claude".parse().unwrap());
        let text = render(&v, now);
        assert!(text.contains("daemon：未运行"), "{text}");
        assert!(
            text.contains("由 [senator::alice/claude] 主持的 Senate，身份 [senator::alice]"),
            "{text}"
        );
        assert!(text.contains("已连接，自 1 分钟前"), "{text}");
        assert!(text.contains("没有登记的会话"), "{text}");
    }
}
