//! The sidebar's tree: every committee, and under it each session of that
//! committee on this machine (hosted by Senate or started by the person),
//! with the one status that matters most for it now.
//!
//! The status, most urgent first ([`SessionStatus`]): a permission prompt
//! waits for the person; a message for it is not getting through; the
//! runtime is mid-turn; it said something to the person they have not read;
//! idle; paused; stopped. A committee shows its most urgent session's.

use std::collections::BTreeMap;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use senate_bridge::ipc::{LocalSessionRow, SessionActivity, SessionDelivery, SessionRow};
use senate_bridge::session::Runtime;
use senate_core::{Address, Locale, SenatorName};

use crate::model::{Model, Waiting};
use crate::state::HostedSession;
use crate::text;

/// A session's state in the sidebar, most urgent first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, uniffi::Enum)]
pub enum SessionStatus {
    /// A permission prompt waits in it for the person.
    NeedsYou,
    /// A message for it is not getting through: it sits idle and did not
    /// answer Senate's wakes, or something a person must clear holds it up.
    /// Its sender and the chair were told.
    NotResponding,
    /// The runtime is mid-turn.
    Working,
    /// It said something to the person that they have not read.
    Unread,
    Idle,
    /// Recorded paused: resume brings it back where it was.
    Paused,
    /// Its terminal is not running.
    Stopped,
}

impl SessionStatus {
    pub fn label(self, locale: Locale) -> &'static str {
        match self {
            Self::NeedsYou => locale.pick("等你决定", "needs you"),
            Self::NotResponding => locale.pick("无响应", "not responding"),
            Self::Working => locale.pick("工作中", "working"),
            Self::Unread => locale.pick("有未读", "unread"),
            Self::Idle => locale.pick("空闲", "idle"),
            Self::Paused => locale.pick("已暂停", "paused"),
            Self::Stopped => locale.pick("已停止", "stopped"),
        }
    }
}

/// One committee in the sidebar, with this machine's sessions on it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SessionGroup {
    /// The committee; `None` for the sessions of no committee.
    pub committee: Option<String>,
    /// The committee's name, or `No committee`.
    pub title: String,
    /// What waits on the person there: threads for them, items for the
    /// Speaker, prompts and Codex approvals of its sessions.
    pub waiting: u32,
    /// The most urgent status of its sessions; `None` without sessions.
    pub status: Option<SessionStatus>,
    /// Worth showing without *Show inactive*: an active session or
    /// member, a live chair, or something waiting on the person there.
    pub active: bool,
    /// Its sessions on this machine, the chair first.
    pub sessions: Vec<SessionLine>,
}

/// One session on this machine.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SessionLine {
    /// The runtime's session id: unique, what the page and the timeline
    /// name it by.
    pub session_id: String,
    /// The name people are shown: its alias, per the naming rules.
    pub display_name: String,
    /// The senator it speaks as, as a mention.
    pub mention: String,
    pub committee: Option<String>,
    /// It chairs its committee.
    pub chair: bool,
    /// `claude`, `codex`, `agy`.
    pub runtime: String,
    /// `opus · high`, when not the runtime's default.
    pub model: Option<String>,
    pub dir: String,
    pub branch: Option<String>,
    /// The session's own title (its summary, a Codex thread's name).
    pub title: Option<String>,
    pub status: SessionStatus,
    pub status_label: String,
    pub working: bool,
    /// Threads in which it said something to the person they have not read.
    pub unread: u32,
    pub last_activity: SystemTime,
    /// Not hidden by *Show inactive*: running, paused, or waiting on the
    /// person.
    pub active: bool,
    /// A Senate terminal host runs it: what attach, stop, pause and the
    /// prompt act on. `None`: the person started it themselves.
    pub hosted: Option<HostedSession>,
    /// What can be read of it: `true` when the runtime keeps a transcript
    /// Senate reads (Claude Code, Codex).
    pub readable: bool,
}

/// The tree: every committee of the primary Senate's roster and every
/// committee a session or a waiting item names, by name; the sessions of
/// no committee last.
pub fn groups(model: &Model, waiting: &[Waiting]) -> Vec<SessionGroup> {
    let locale = model.locale;
    let mut lines: Vec<SessionLine> = Vec::new();
    if let Some(reply) = &model.sessions {
        lines.extend(reply.rows.iter().map(|r| hosted_line(r, waiting, locale)));
        lines.extend(
            reply
                .local_rows
                .iter()
                .filter(|r| !reply.rows.iter().any(|h| h.session_id == r.session_id))
                .map(|r| local_line(r, waiting, locale)),
        );
    }

    #[derive(Default)]
    struct Group {
        waiting: usize,
        active: bool,
        sessions: Vec<SessionLine>,
    }
    let mut by: BTreeMap<Option<String>, Group> = BTreeMap::new();
    for c in &model.roster {
        let g = by.entry(Some(c.committee.to_string())).or_default();
        g.active |= c.active_members() > 0 || c.chair_live;
    }
    for w in waiting.iter().filter(|w| w.for_person) {
        by.entry(Some(w.committee.to_string())).or_default().waiting += 1;
    }
    for page in model
        .docket
        .iter()
        .flat_map(|d| &d.senates)
        .flat_map(|s| &s.pages)
    {
        if !page.items.is_empty() {
            by.entry(Some(page.committee.to_string()))
                .or_default()
                .waiting += page.items.len();
        }
    }
    for a in model.approvals.values() {
        let committee = model
            .codex_sessions()
            .into_iter()
            .find(|s| s.session_id == a.session_id)
            .and_then(|s| s.committee().map(ToString::to_string));
        by.entry(committee).or_default().waiting += 1;
    }
    for line in lines {
        let g = by.entry(line.committee.clone()).or_default();
        if line.status == SessionStatus::NeedsYou {
            g.waiting += 1;
        }
        g.active |= line.active;
        g.sessions.push(line);
    }

    let mut out: Vec<SessionGroup> = by
        .into_iter()
        .map(|(committee, mut g)| {
            g.sessions.sort_by(|a, b| {
                b.chair
                    .cmp(&a.chair)
                    .then_with(|| a.display_name.cmp(&b.display_name))
                    .then_with(|| a.session_id.cmp(&b.session_id))
            });
            SessionGroup {
                title: committee
                    .clone()
                    .unwrap_or_else(|| text::no_committee(locale).to_string()),
                committee,
                waiting: u32::try_from(g.waiting).unwrap_or(u32::MAX),
                status: g.sessions.iter().map(|s| s.status).min(),
                active: g.active || g.waiting > 0,
                sessions: g.sessions,
            }
        })
        .collect();
    // `None` sorts first in the map; the sessions of no committee go last.
    if out.first().is_some_and(|g| g.committee.is_none()) {
        let none = out.remove(0);
        out.push(none);
    }
    out
}

/// Threads waiting on the person where `senator` said what they have not
/// read.
fn unread_from(senator: &SenatorName, waiting: &[Waiting]) -> u32 {
    let me = Address::senator(senator);
    let n = waiting
        .iter()
        .filter(|w| w.unread.as_ref().is_some_and(|d| d.from == me))
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn status(
    prompt: bool,
    delivery: &SessionDelivery,
    working: bool,
    unread: u32,
    paused: bool,
    running: bool,
) -> SessionStatus {
    if prompt {
        SessionStatus::NeedsYou
    } else if delivery.activity == Some(SessionActivity::NotResponding) {
        SessionStatus::NotResponding
    } else if working {
        SessionStatus::Working
    } else if unread > 0 {
        SessionStatus::Unread
    } else if paused {
        SessionStatus::Paused
    } else if !running {
        SessionStatus::Stopped
    } else {
        SessionStatus::Idle
    }
}

/// The status in words, then how many messages wait for the session
/// unseen, when any do: `not responding · 2 waiting`, `working · 1 waiting`.
fn status_label(status: SessionStatus, delivery: &SessionDelivery, locale: Locale) -> String {
    let label = status.label(locale);
    match (delivery.waiting, locale) {
        (0, _) => label.to_string(),
        (n, Locale::Zh) => format!("{label} · {n} 条待读"),
        (n, Locale::En) => format!("{label} · {n} waiting"),
    }
}

/// Whether Senate can read a transcript of this runtime.
pub fn readable(runtime: Runtime) -> bool {
    matches!(runtime, Runtime::Claude | Runtime::Codex | Runtime::Agy)
}

fn time(t: DateTime<Utc>) -> SystemTime {
    t.into()
}

fn hosted_line(r: &SessionRow, waiting: &[Waiting], locale: Locale) -> SessionLine {
    let unread = unread_from(&r.primary, waiting);
    let running = r.is_running();
    let working = r.working && running;
    let st = status(
        r.prompt.is_some(),
        &r.delivery,
        working,
        unread,
        r.paused,
        running,
    );
    let hosted = HostedSession::from(r);
    SessionLine {
        session_id: r.session_id.clone(),
        display_name: hosted.display_name.clone(),
        mention: hosted.mention.clone(),
        committee: hosted.committee.clone(),
        chair: r.chair,
        runtime: hosted.runtime.clone(),
        model: hosted.model.clone(),
        dir: hosted.dir.clone(),
        branch: r.branch.clone(),
        title: r.title.clone(),
        status: st,
        status_label: status_label(st, &r.delivery, locale),
        working,
        unread,
        last_activity: time(r.last_activity),
        active: r.is_active() || r.paused || r.prompt.is_some(),
        hosted: Some(hosted),
        readable: readable(r.runtime),
    }
}

fn local_line(r: &LocalSessionRow, waiting: &[Waiting], locale: Locale) -> SessionLine {
    let unread = unread_from(&r.primary, waiting);
    let st = status(false, &r.delivery, r.working, unread, false, true);
    SessionLine {
        session_id: r.session_id.clone(),
        display_name: r
            .shown_name
            .clone()
            .or_else(|| r.alias.as_ref().map(ToString::to_string))
            .unwrap_or_else(|| senate_core::alias::short_name(&r.primary)),
        mention: Address::senator(&r.primary).in_text(),
        committee: r.committee.as_ref().map(ToString::to_string),
        chair: r.chair,
        runtime: r.runtime.as_str().to_string(),
        model: None,
        dir: r.dir.display().to_string(),
        branch: r.branch.clone(),
        title: r.title.clone(),
        status: st,
        status_label: status_label(st, &r.delivery, locale),
        working: r.working,
        unread,
        last_activity: time(r.last_activity),
        active: true,
        hosted: None,
        readable: readable(r.runtime),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A waiting prompt comes first; then a message that is not getting
    /// through; a busy session with something unseen is working, not stuck.
    #[test]
    fn a_session_that_is_not_taking_its_messages_says_so_with_the_count() {
        let delivery = |activity, waiting| SessionDelivery {
            activity: Some(activity),
            waiting,
            waiting_since: None,
            stuck: activity == SessionActivity::NotResponding,
        };
        let stuck = delivery(SessionActivity::NotResponding, 2);
        let st = status(false, &stuck, false, 3, false, true);
        assert_eq!(st, SessionStatus::NotResponding);
        assert_eq!(
            status_label(st, &stuck, Locale::En),
            "not responding · 2 waiting"
        );
        assert_eq!(status_label(st, &stuck, Locale::Zh), "无响应 · 2 条待读");
        assert_eq!(
            status(true, &stuck, false, 0, false, true),
            SessionStatus::NeedsYou,
            "the prompt is what a person can act on"
        );
        let busy = delivery(SessionActivity::Active, 1);
        let st = status(false, &busy, true, 0, false, true);
        assert_eq!(st, SessionStatus::Working);
        assert_eq!(status_label(st, &busy, Locale::En), "working · 1 waiting");
        let idle = delivery(SessionActivity::Idle, 0);
        let st = status(false, &idle, false, 0, false, true);
        assert_eq!(status_label(st, &idle, Locale::En), "idle");
        assert!(SessionStatus::NeedsYou < SessionStatus::NotResponding);
        assert!(SessionStatus::NotResponding < SessionStatus::Working);
    }
}
