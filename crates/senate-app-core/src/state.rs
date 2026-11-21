//! What the app renders: a full, flat snapshot of the [`Model`], with every
//! sentence already written in the app's language ([`crate::text`]) and every
//! id written as a bracketed mention (`[senator::claude]`), the form
//! [`crate::ffi::split_mentions`] splits for display.
//!
//! Enum-valued fields come twice where the UI needs both: the wire spelling
//! to decide on (`suspended`) and a label to show (`state_label`).

use std::time::SystemTime;

use chrono::{DateTime, Utc};
use senate_bridge::approvals::Approval;
use senate_bridge::ipc::{SenateDocket, SessionRow, StatusReply, StreamState};
use senate_core::{
    Address, Awaiting, DocketItem, Locale, Mention, SenatorName, SpeakerSeat, ThreadSummary,
};
use serde::Serialize;

use crate::model::{Model, Waiting, escalated_to_now, presider_of};
use crate::text;

/// The app's whole picture of the daemon.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct AppState {
    /// A `watch` stream is open.
    pub online: bool,
    /// Why the daemon is unreachable, when it is.
    pub offline_reason: Option<String>,
    /// The person this machine authenticates as: `alice`.
    pub person: Option<String>,
    /// Every Senate the daemon serves, the primary first.
    pub senates: Vec<SenateInfo>,
    /// The person's agents, in every Senate the daemon serves.
    pub agents: Vec<AgentInfo>,
    /// The menu-bar extra's text.
    pub menu: MenuState,
    /// Threads waiting on the person, merged across agents, longest waiting
    /// first.
    pub inbox: Vec<InboxThread>,
    /// Pending Codex approvals, oldest first.
    pub approvals: Vec<ApprovalRequest>,
    pub codex: Vec<CodexThreadInfo>,
    /// What awaits the Speaker of each Senate one of the person's agents
    /// presides over; empty when none does. Read-only: the presiding agent
    /// routes and covers, not the person.
    pub docket: Vec<DocketSenate>,
    /// This machine's hosted sessions, running first.
    pub sessions: Vec<HostedSession>,
    pub counts: Counts,
    /// The last failed action or read.
    pub last_error: Option<String>,
}

/// One hosted session: a terminal this machine runs for a senator.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HostedSession {
    /// What commands take to name it: `committee::alias`, else its id.
    pub label: String,
    pub session_id: String,
    /// The senator it speaks as, as a mention.
    pub mention: String,
    pub committee: Option<String>,
    /// `claude`, `codex`.
    pub runtime: String,
    /// `opus · high`, when not the runtime's default.
    pub model: Option<String>,
    pub dir: String,
    pub running: bool,
    pub started_at: Option<SystemTime>,
    /// Someone is attached to its terminal now.
    pub attached: bool,
    /// Seconds since a person last typed into it.
    pub typed_secs_ago: Option<u64>,
    /// A permission prompt waits in it: what it asks about.
    pub prompt_tool: Option<String>,
    /// Since when a prompt waits.
    pub prompt_since: Option<SystemTime>,
}

impl From<&SessionRow> for HostedSession {
    fn from(r: &SessionRow) -> Self {
        Self {
            label: r.label(),
            session_id: r.session_id.clone(),
            mention: Address::senator(&r.primary).in_text(),
            committee: r.committee.as_ref().map(ToString::to_string),
            runtime: r.runtime.as_str().to_string(),
            model: text::model_label(r.model.model.as_deref(), r.model.effort.as_deref()),
            dir: r.dir.display().to_string(),
            running: r.is_running(),
            started_at: r.started_at.map(Into::into),
            attached: r.attached,
            typed_secs_ago: r.typed_secs_ago,
            prompt_tool: r.prompt.as_ref().and_then(|p| p.tool.clone()),
            prompt_since: r.prompt.as_ref().map(|p| p.since.into()),
        }
    }
}

/// The menu-bar extra: the lines of its menu and the badge.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MenuState {
    /// `Senate: online as alice` / `Senate: offline`.
    pub title: String,
    /// The longer form, with the offline reason.
    pub tooltip: String,
    /// `2 pending threads, 1 approval`.
    pub pending: String,
    /// One line per stream: `claude · refund-v2 · live · seq 120`.
    pub lines: Vec<String>,
    /// `Docket: 3 awaiting the Speaker`, only while an agent presides.
    pub docket: Option<String>,
    /// `1 terminal waiting for your permission`, only while one waits.
    pub prompts: Option<String>,
    /// Shown next to the icon when something waits on the person.
    pub badge: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct Counts {
    pub threads: u32,
    pub approvals: u32,
    pub codex: u32,
    /// Items awaiting a Speaker the person's agent holds.
    pub docket: u32,
    /// Hosted terminals with a permission prompt waiting for a person.
    pub prompts: u32,
    /// `threads + approvals`: what waits on the person.
    pub pending: u32,
}

/// One Senate the daemon serves, and who the person is in it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SenateInfo {
    /// The Senate's path (`acme/north-desk`), empty until the daemon knows
    /// it. Unique.
    pub key: String,
    /// `Senate` / `Senate acme/north-desk`.
    pub title: String,
    /// The Senate's path, when known.
    pub senate: Option<String>,
    /// The Senate where the person's agents sit directly: the one whose
    /// inboxes the app can read and answer in.
    pub primary: bool,
    pub server_url: String,
    /// The person, as a senior senator there: `[senator::alice]`.
    pub me: String,
    /// Who holds the Speaker's seat: `[speaker]` for the built-in Speaker,
    /// else the senator's mention.
    pub speaker: Option<String>,
    /// The person's agent holding the seat, when it is one of theirs.
    pub presiding_agent: Option<String>,
    pub committees: Vec<CommitteeLine>,
    /// Each committee's chair, as this Senate's roles say.
    pub chairs: Vec<ChairLine>,
    /// The Senates registered below this one.
    pub children: Vec<ChildLine>,
    /// This Senate's registration with the Senate above, when it has one:
    /// `registered as dark-conclave with https://…`.
    pub uplink: Option<String>,
}

/// One committee's chair.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChairLine {
    pub committee: String,
    /// The chair as a mention; `None`: the committee has none.
    pub chair: Option<String>,
}

/// A Senate registered below this one, and the seats it declares.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChildLine {
    pub name: String,
    /// Its Speaker, as a mention.
    pub speaker: String,
    /// It declared its seats recently.
    pub online: bool,
    pub last_seen: Option<SystemTime>,
    pub seats: Vec<ChildSeatLine>,
}

/// A seat a child Senate declares: who, where, with what duty.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ChildSeatLine {
    pub committee: String,
    /// As people are shown it: `senate-dev（dark-conclave）`.
    pub name: String,
    /// Its id in the child.
    pub senator: String,
    pub duty: Option<String>,
    /// `online`, `disconnected`, `absent`, ….
    pub presence: String,
}

/// One agent's stream on one committee.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CommitteeLine {
    /// The agent's path: `claude`.
    pub agent: String,
    pub committee: String,
    /// `live`, `connecting`, `behind, retry in 12s (attempt 3)`, `stopped`.
    pub stream: String,
    pub live: bool,
    /// The agent's duty on the committee, when it has one.
    pub duty: Option<String>,
    /// The seq delivered so far.
    pub delivered: u64,
    pub sessions: u32,
}

/// One of the person's agents in one Senate.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AgentInfo {
    /// `claude`: what an inbox filter compares.
    pub path: String,
    /// `[senator::claude]`.
    pub mention: String,
    /// The [`SenateInfo::key`] of the Senate it sits in.
    pub senate: String,
    /// `claude` or `codex`, from its sessions, when it has any.
    pub runtime: Option<String>,
    /// Sessions of it that are not ended.
    pub sessions: u32,
    /// Threads waiting on the person that are for this agent or escalated.
    /// Counted only where the app can read the inbox.
    pub pending: u32,
    /// It holds the Speaker's seat of a Senate here.
    pub presiding: bool,
    /// The app can read its inbox (it sits in the primary Senate).
    pub inbox: bool,
}

/// An open thread waiting on the person. Addresses are bracketed mentions;
/// enum-valued fields carry the wire spelling and a label.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct InboxThread {
    pub committee: String,
    /// The thread's root seq: what reply, take and close name.
    pub root: u64,
    /// The thread's last seq: what `mark_read` takes to clear its news.
    pub updated: u64,
    pub title: String,
    pub kind: String,
    pub kind_label: String,
    /// `suspended` or `processing`: an answered (`active`) thread is not
    /// listed.
    pub state: String,
    pub state_label: String,
    /// Who wrote it: `[senator::copilot]`, or `[speaker::claude]`
    /// for the Speaker speaking for a senator.
    pub from: String,
    /// Whom a sender other than the Speaker spoke for.
    pub on_behalf_of: Option<String>,
    pub to: Vec<String>,
    /// When it entered its state.
    pub since: SystemTime,
    /// The agent session in charge: `[senator::claude]`.
    pub owner: Option<String>,
    pub triage: Option<String>,
    pub triage_label: Option<String>,
    pub reason: Option<String>,
    pub escalated_to: Option<String>,
    pub takeover: Option<String>,
    pub answered: bool,
    /// The person's agents (paths) it was addressed to.
    pub for_agents: Vec<String>,
    /// It was put to the person: it waits on them whichever agent it was for.
    pub escalated: bool,
    /// Something was said to the person in it that they have not seen:
    /// `news_*` describe the latest such event. It may be listed for this
    /// alone, with `state` `active` (answered).
    pub unread: bool,
    /// Who wrote it: `[senator::claude]`.
    pub news_from: Option<String>,
    /// Its kind (`answer`, `note`, `status` for an escalation) and a label.
    pub news_kind: Option<String>,
    pub news_kind_label: Option<String>,
    /// Its first line (an escalation's reason).
    pub news_line: Option<String>,
}

/// A Codex app-server approval request the daemon relays.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ApprovalRequest {
    pub id: String,
    /// The Codex thread asking.
    pub thread: String,
    /// `command`, `file_change`, …
    pub kind: String,
    pub summary: String,
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub reason: Option<String>,
    pub files: Vec<String>,
    pub received_at: SystemTime,
}

/// A Codex thread registered with the daemon.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CodexThreadInfo {
    pub thread: String,
    /// The agent it speaks as: `codex`.
    pub agent: String,
    pub committee: Option<String>,
    /// `live`, `stale`.
    pub state: String,
    pub state_label: String,
    pub cwd: String,
    pub branch: Option<String>,
    pub last_seen: SystemTime,
    /// Its pending approvals.
    pub approvals: u32,
}

/// What awaits the Speaker of one Senate.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DocketSenate {
    /// The [`SenateInfo::key`] of the Senate.
    pub senate: String,
    pub title: String,
    /// Who holds the seat, as a mention.
    pub speaker: Option<String>,
    /// Committees with something awaiting, each with the agents seated.
    pub committees: Vec<DocketCommittee>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DocketCommittee {
    pub committee: String,
    pub items: Vec<DocketEntry>,
    pub seats: Vec<SeatLine>,
}

/// One item awaiting the Speaker.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DocketEntry {
    pub committee: String,
    pub seq: u64,
    pub kind_label: String,
    /// The author as shown to people, with the takeover or companion word.
    pub by: String,
    /// The event's text; may hold bracketed mentions.
    pub body: String,
    /// `pick`, `addressed` or `away`.
    pub awaiting: String,
    /// Why it waits, in words; holds bracketed mentions.
    pub why: String,
    /// The agents to choose from or that are away, as mentions.
    pub agents: Vec<String>,
    pub since: SystemTime,
}

/// One agent seated in a Senate, as the Speaker sees it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SeatLine {
    pub agent: String,
    pub connected: bool,
    /// The summary line of its latest checkpoint.
    pub checkpoint: Option<String>,
    pub open_threads: u32,
}

impl AppState {
    pub fn from_model(model: &Model, now: DateTime<Utc>) -> Self {
        let locale = model.locale;
        let status = model.status.as_ref().filter(|_| model.online);
        // Offline, nothing is claimed to wait: what the model still holds
        // is the last picture, not the present.
        let waiting = if model.online {
            model.waiting()
        } else {
            Vec::new()
        };
        let approvals: Vec<&Approval> = {
            let mut a: Vec<&Approval> = model.approvals.values().collect();
            a.sort_by_key(|a| a.received_at);
            a
        };
        let codex = if model.online {
            codex_threads(model, &approvals)
        } else {
            Vec::new()
        };
        let docket = docket(model);
        let docket_items = if model.online {
            model.docket_items()
        } else {
            0
        };
        let senates: Vec<SenateInfo> = if model.online {
            model
                .senates()
                .enumerate()
                .map(|(i, s)| senate_info(s, i == 0, locale, now))
                .collect()
        } else {
            Vec::new()
        };
        let agents = if model.online {
            agents(model, &waiting)
        } else {
            Vec::new()
        };
        let sessions: Vec<HostedSession> = if model.online {
            model
                .sessions
                .iter()
                .flat_map(|s| &s.rows)
                .map(HostedSession::from)
                .collect()
        } else {
            Vec::new()
        };
        let prompts = if model.online {
            model.waiting_prompts()
        } else {
            0
        };
        let counts = Counts {
            threads: count(waiting.len()),
            approvals: count(approvals.len()),
            codex: count(codex.len()),
            docket: count(docket_items),
            prompts: count(prompts),
            pending: count(waiting.len() + approvals.len() + prompts),
        };
        let offline_reason = model.offline.as_ref().map(|o| o.text(locale));
        let menu = menu(
            model,
            &senates,
            waiting.len(),
            approvals.len(),
            docket_items,
            prompts,
            offline_reason.as_deref(),
        );
        Self {
            online: model.online,
            offline_reason,
            person: status.map(|s| s.person.to_string()),
            senates,
            agents,
            menu,
            inbox: waiting.iter().map(|w| inbox_thread(w, locale)).collect(),
            approvals: approvals.into_iter().map(Into::into).collect(),
            codex,
            docket,
            sessions,
            counts,
            last_error: model.last_error.as_ref().map(|f| f.text(locale)),
        }
    }
}

fn menu(
    model: &Model,
    senates: &[SenateInfo],
    threads: usize,
    approvals: usize,
    docket_items: usize,
    prompts: usize,
    offline: Option<&str>,
) -> MenuState {
    let locale = model.locale;
    if !model.online {
        return MenuState {
            title: text::offline_title(locale),
            tooltip: text::offline_tooltip(locale, offline),
            pending: text::offline_pending(locale),
            lines: Vec::new(),
            docket: None,
            prompts: None,
            badge: None,
        };
    }
    let person = model
        .status
        .as_ref()
        .map(|s| s.person.to_string())
        .unwrap_or_default();
    let pending = text::pending(locale, threads, approvals);
    let lines = senates
        .iter()
        .flat_map(|s| &s.committees)
        .map(|c| {
            format!(
                "{} · {} · {} · seq {}",
                c.agent, c.committee, c.stream, c.delivered
            )
        })
        .collect();
    let total = threads + approvals + prompts;
    MenuState {
        title: text::online_title(locale, &person),
        tooltip: format!("Senate · {pending}"),
        pending,
        lines,
        docket: model
            .presiding()
            .then(|| text::docket_line(locale, docket_items)),
        prompts: (prompts > 0).then(|| text::prompts_line(locale, prompts)),
        badge: (total > 0).then(|| total.to_string()),
    }
}

fn senate_info(s: &StatusReply, primary: bool, locale: Locale, now: DateTime<Utc>) -> SenateInfo {
    let senate = s.senate.as_ref().map(ToString::to_string);
    SenateInfo {
        key: senate.clone().unwrap_or_default(),
        title: text::senate_title(locale, senate.as_deref()),
        senate,
        primary,
        server_url: s.server_url.clone(),
        me: Address::senior(&s.person).in_text(),
        speaker: s.speaker.as_ref().map(seat_mention),
        presiding_agent: presider_of(s).map(ToString::to_string),
        committees: s
            .committees
            .iter()
            .map(|c| CommitteeLine {
                agent: c.agent.to_string(),
                committee: c.committee.to_string(),
                stream: text::stream(locale, c.stream.as_ref(), now),
                live: matches!(c.stream, Some(StreamState::Connected { .. })),
                duty: c.duty.clone(),
                delivered: c.delivered.0,
                sessions: count(c.sessions),
            })
            .collect(),
        chairs: s
            .roles
            .iter()
            .flat_map(|r| &r.chairs)
            .map(|c| ChairLine {
                committee: c.committee.to_string(),
                chair: c.chair.as_ref().map(|p| Address::senator(p).in_text()),
            })
            .collect(),
        children: s
            .roles
            .iter()
            .flat_map(|r| &r.children)
            .map(|c| child_line(c, locale, now))
            .collect(),
        uplink: s
            .roles
            .as_ref()
            .and_then(|r| r.uplink.as_ref())
            .map(|u| text::uplink(locale, u)),
    }
}

fn child_line(c: &senate_core::ChildSenate, locale: Locale, now: DateTime<Utc>) -> ChildLine {
    ChildLine {
        name: c.name.to_string(),
        speaker: Address::senator(&c.speaker).in_text(),
        online: c.is_online(now),
        last_seen: c.last_seen.map(Into::into),
        seats: c
            .seats
            .iter()
            .map(|seat| ChildSeatLine {
                committee: seat.committee.to_string(),
                name: crate::names::seat_name(
                    locale,
                    seat.alias.as_ref().unwrap_or(&seat.senator).as_str(),
                    c.name.as_str(),
                ),
                senator: seat.senator.to_string(),
                duty: seat.duty.clone(),
                presence: seat.presence.as_str().to_string(),
            })
            .collect(),
    }
}

fn seat_mention(seat: &SpeakerSeat) -> String {
    match seat {
        SpeakerSeat::BuiltIn => Mention::Speaker.to_string(),
        SpeakerSeat::Senator(p) => Address::senator(p).in_text(),
    }
}

/// The person's agents: every agent a stream or a live session names, in
/// each Senate, the primary Senate's first.
fn agents(model: &Model, waiting: &[Waiting]) -> Vec<AgentInfo> {
    let presiding: Vec<&SenatorName> = model.senates().filter_map(presider_of).collect();
    let mut out = Vec::new();
    for (i, s) in model.senates().enumerate() {
        let senate = s
            .senate
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();
        let mut paths: Vec<&SenatorName> = s.committees.iter().map(|c| &c.agent).collect();
        for x in s
            .sessions
            .iter()
            .filter(|x| x.state != senate_bridge::session::SessionState::Ended)
        {
            paths.push(&x.agent);
        }
        paths.sort();
        paths.dedup();
        for path in paths {
            let sessions: Vec<_> = s
                .sessions
                .iter()
                .filter(|x| {
                    &x.agent == path && x.state != senate_bridge::session::SessionState::Ended
                })
                .collect();
            out.push(AgentInfo {
                path: path.to_string(),
                mention: Address::senator(path).in_text(),
                senate: senate.clone(),
                runtime: sessions.first().map(|x| x.runtime.as_str().to_string()),
                sessions: count(sessions.len()),
                pending: if i == 0 {
                    count(
                        waiting
                            .iter()
                            .filter(|w| w.escalated || w.for_agents.contains(path))
                            .count(),
                    )
                } else {
                    0
                },
                presiding: presiding.contains(&path),
                inbox: i == 0,
            });
        }
    }
    out
}

pub(crate) fn inbox_thread(w: &Waiting, locale: Locale) -> InboxThread {
    let t: &ThreadSummary = &w.thread;
    let author = Mention::author(&t.from, t.on_behalf_of.as_ref());
    InboxThread {
        committee: w.committee.to_string(),
        root: t.root.0,
        updated: t.updated.0,
        title: t.title.clone(),
        kind: t.kind.as_str().to_string(),
        kind_label: text::kind(locale, &t.kind),
        state: wire(&t.state),
        state_label: text::state(locale, t.state).to_string(),
        from: author
            .as_ref()
            .map_or_else(|| t.from.in_text(), ToString::to_string),
        on_behalf_of: match author {
            Some(_) => None,
            None => t.on_behalf_of.as_ref().map(Address::in_text),
        },
        to: t.to.iter().map(Address::in_text).collect(),
        since: t.since_ts.into(),
        owner: t
            .owner
            .as_ref()
            .map(|o| Address::senator(&o.agent).in_text()),
        triage: t.triage.as_ref().map(wire),
        triage_label: t.triage.map(|v| text::triage(locale, v).to_string()),
        reason: t.reason.clone(),
        escalated_to: escalated_to_now(t).map(|n| Address::senior(n).in_text()),
        takeover: t.takeover.as_ref().map(|n| Address::senior(n).in_text()),
        answered: t.answered,
        for_agents: w.for_agents.iter().map(ToString::to_string).collect(),
        escalated: w.escalated,
        unread: w.unread.is_some(),
        news_from: w.unread.as_ref().map(|d| {
            Mention::author(&d.from, d.on_behalf_of.as_ref())
                .map_or_else(|| d.from.in_text(), |m| m.to_string())
        }),
        news_kind: w.unread.as_ref().map(|d| {
            if d.escalated {
                "escalated".to_string()
            } else {
                d.kind.as_str().to_string()
            }
        }),
        news_kind_label: w.unread.as_ref().map(|d| {
            if d.escalated {
                text::escalated_word(locale).to_string()
            } else {
                text::kind(locale, &d.kind)
            }
        }),
        news_line: w.unread.as_ref().map(|d| d.line.clone()),
    }
}

fn codex_threads(model: &Model, approvals: &[&Approval]) -> Vec<CodexThreadInfo> {
    model
        .codex_sessions()
        .into_iter()
        .map(|x| CodexThreadInfo {
            thread: x.session_id.clone(),
            agent: x.agent.to_string(),
            committee: x.committee().map(ToString::to_string),
            state: wire(&x.state),
            state_label: text::session_state(model.locale, x.is_live()).to_string(),
            cwd: x.cwd.display().to_string(),
            branch: x.branch.clone(),
            last_seen: x.last_seen.into(),
            approvals: count(
                approvals
                    .iter()
                    .filter(|a| a.session_id == x.session_id)
                    .count(),
            ),
        })
        .collect()
}

fn docket(model: &Model) -> Vec<DocketSenate> {
    let Some(reply) = model.docket.as_ref().filter(|_| model.online) else {
        return Vec::new();
    };
    reply
        .senates
        .iter()
        .map(|s| docket_senate(s, model.locale))
        .collect()
}

fn docket_senate(s: &SenateDocket, locale: Locale) -> DocketSenate {
    let senate = s.senate.as_ref().map(ToString::to_string);
    DocketSenate {
        senate: senate.clone().unwrap_or_default(),
        title: text::senate_title(locale, senate.as_deref()),
        speaker: s
            .pages
            .iter()
            .find_map(|p| p.speaker.as_ref())
            .map(seat_mention),
        committees: s
            .pages
            .iter()
            .filter(|p| !p.items.is_empty())
            .map(|p| DocketCommittee {
                committee: p.committee.to_string(),
                items: p.items.iter().map(|i| docket_entry(i, locale)).collect(),
                seats: p
                    .seats
                    .iter()
                    .map(|seat| SeatLine {
                        agent: Address::senator(&seat.agent).in_text(),
                        connected: seat.connected,
                        checkpoint: seat.checkpoint.as_ref().map(|c| c.summary.clone()),
                        open_threads: count(seat.open_threads),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn docket_entry(item: &DocketItem, locale: Locale) -> DocketEntry {
    let e = &item.event;
    let mentions = |paths: &[SenatorName]| -> Vec<String> {
        paths
            .iter()
            .map(|p| Address::senator(p).in_text())
            .collect()
    };
    let (awaiting, agents, why) = match &item.awaiting {
        Awaiting::Pick { candidates, .. } => {
            let agents = mentions(candidates);
            let why = text::awaiting_pick(locale, &agents);
            ("pick", agents, why)
        }
        Awaiting::Addressed => ("addressed", Vec::new(), text::awaiting_addressed(locale)),
        Awaiting::Away { agents } => {
            let agents = mentions(agents);
            let why = text::awaiting_away(locale, &agents);
            ("away", agents, why)
        }
        Awaiting::ChildOffline { child } => (
            "child_offline",
            Vec::new(),
            text::awaiting_child_offline(locale, child.as_str()),
        ),
    };
    DocketEntry {
        committee: e.committee.to_string(),
        seq: e.seq.0,
        kind_label: text::kind(locale, &e.kind),
        by: e.byline(locale).unwrap_or_else(|| e.from.in_text()),
        body: e.body.clone(),
        awaiting: awaiting.to_string(),
        why,
        agents,
        since: e.ts.into(),
    }
}

impl From<&Approval> for ApprovalRequest {
    fn from(a: &Approval) -> Self {
        Self {
            id: a.id.clone(),
            thread: a.session_id.clone(),
            kind: wire(&a.kind),
            summary: a.summary.clone(),
            command: a.command.clone(),
            cwd: a.cwd.clone(),
            reason: a.reason.clone(),
            files: a.files.clone(),
            received_at: a.received_at.into(),
        }
    }
}

/// An enum's wire spelling (`needs_human`), as the daemon writes it.
pub fn wire<T: Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
