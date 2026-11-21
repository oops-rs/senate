//! Everything the Speaker says, in one place, in each [`Locale`].
//!
//! Rendering is a pure function of its inputs, so a restart that replays a
//! post sends byte-identical markdown under the same `operation_id` and the
//! chat surface answers `duplicate` instead of `operation_conflict`.
//!
//! Who wrote something is never part of a stored body: a copy is headed by
//! the event's byline ([`Event::byline`]), and every id in text is a
//! bracketed mention ([`Address::in_text`]).

use std::fmt::Write as _;

use senate_core::{
    Address, AgentPresence, Brief, Card, CheckpointPayload, CommitteeId, Event, Kind, Locale, Name,
    OutputLevel, PresencePayload, PresenceState, Roster, SenatorName, Seq, StatusPayload,
    StatusState, ThreadSummary, TriagePayload, TriageVerdict,
};

use crate::syntax::usage;

mod work;
pub(crate) use work::*;
mod route;
pub(crate) use route::*;

/// Bodies longer than this are cut on the chat surface; the log keeps them
/// whole.
const MAX_BODY_CHARS: usize = 3_000;
/// The brief as one chat reply: well inside a message's limits, short enough
/// to read in a chat.
pub(crate) const MAX_BRIEF_CHARS: usize = 3_000;
/// One decision or question in the brief reply.
const BRIEF_ITEM_CHARS: usize = 200;
/// Lines of the brief's top, and characters per line.
const TOP_LINES: usize = 3;
const TOP_LINE_CHARS: usize = 120;

// ----- small pieces ----------------------------------------------------------

/// Items of a list in running text: `a、b` / `a, b`.
fn join(l: Locale, items: impl IntoIterator<Item = String>) -> String {
    items
        .into_iter()
        .collect::<Vec<_>>()
        .join(l.pick("、", ", "))
}

/// Addresses as mentions in running text.
fn mentions(l: Locale, to: &[Address]) -> String {
    join(l, to.iter().map(Address::in_text))
}

/// A child Senate's seat as people are shown it: its alias there with the
/// child's name, `senate-dev（dark-conclave）` / `senate-dev (dark-conclave)`.
pub(crate) fn child_seat_name(l: Locale, alias: &SenatorName, child: &Name) -> String {
    match l {
        Locale::Zh => format!("{alias}（{child}）"),
        Locale::En => format!("{alias} ({child})"),
    }
}

/// What the Speaker knows of how people are shown a seat: the aliases
/// here and the seats child Senates declare with theirs. A seat with no
/// alias is shown by its id.
#[derive(Clone, Copy)]
pub(crate) struct SeatNames<'a> {
    pub aliases: &'a senate_core::alias::CommitteeAliases,
    pub children: &'a [senate_core::ChildSenate],
}

impl SeatNames<'_> {
    /// An agent here as people are shown it on `committee`: its alias
    /// there as a mention, else its id.
    fn local(&self, committee: &CommitteeId, address: &Address) -> String {
        address
            .as_senator()
            .and_then(|agent| self.aliases.alias_of(committee, agent))
            .map_or_else(|| address.in_text(), senator)
    }

    /// A seat in a child Senate, when the child declares an alias for it.
    fn child(&self, l: Locale, seat: &senate_core::Locator) -> Option<String> {
        let child = seat.senate()?.name();
        let committee = seat.committee()?;
        let senator = seat.address().as_senator()?;
        let declared = self
            .children
            .iter()
            .find(|c| c.name == *child)?
            .seats
            .iter()
            .find(|s| s.committee == *committee && s.senator == *senator)?;
        Some(child_seat_name(l, declared.alias.as_ref()?, child))
    }
}

/// A senator (person or agent) as a mention: `[senator::claude]`.
fn senator(path: &SenatorName) -> String {
    Address::senator(path).in_text()
}

/// A person as a mention: `[senator::alice]`.
fn person(name: &Name) -> String {
    Address::senior(name).in_text()
}

/// `（seq 118）` / ` (seq 118)`.
fn seq_note(l: Locale, seq: Seq) -> String {
    match l {
        Locale::Zh => format!("（seq {seq}）"),
        Locale::En => format!(" (seq {seq})"),
    }
}

/// `N events behind`, in words.
fn lag_words(l: Locale, lag: u64) -> String {
    match l {
        Locale::Zh => format!("落后 {lag} 条事件"),
        Locale::En if lag == 1 => "1 event behind".to_owned(),
        Locale::En => format!("{lag} events behind"),
    }
}

/// `3 more` after a cut list: `（还有 3 条）` / `(3 more)`.
fn more(l: Locale, n: usize) -> String {
    match l {
        Locale::Zh => format!("（还有 {n} 条）"),
        Locale::En => format!("({n} more)"),
    }
}

// ----- mirroring -------------------------------------------------------------

/// `已转给 [senator::claude]（seq 120）` /
/// `Passed to [senator::claude] (seq 120)`.
#[cfg(test)]
pub(crate) fn relay_ack(l: Locale, targets: &[Address], seq: Seq) -> String {
    relay_ack_to(l, &mentions(l, targets), seq)
}

fn relay_ack_to(l: Locale, whom: &str, seq: Seq) -> String {
    match l {
        Locale::Zh => format!("已转给 {whom}（seq {seq}）"),
        Locale::En => format!("Passed to {whom} (seq {seq})"),
    }
}

/// A new root in the mirror group:
/// `[senator::claude] → [senator::copilot] <body>（seq 118）`.
pub(crate) fn root_mirror(l: Locale, event: &Event) -> String {
    let seq = seq_note(l, event.seq);
    let label = label(l, event);
    let body = body(l, event);
    match quoted_for(event, &body) {
        None => format!("{label} {body}{seq}"),
        Some(quote) => format!("{label}{seq}\n{quote}"),
    }
}

/// A reply in a mirrored thread: `[senator::copilot] <body>`, or
/// `[speaker::copilot] <body>` when the Speaker answered for it.
pub(crate) fn thread_mirror(l: Locale, event: &Event) -> String {
    let label = label(l, event);
    let body = body(l, event);
    match quoted_for(event, &body) {
        None => format!("{label} {body}"),
        Some(quote) => format!("{label}\n{quote}"),
    }
}

/// An agent's proposed decision, as a new root in the mirror group:
/// `[senator::claude] 提议记为决策：<body>（seq 130）回复「对」确认` /
/// `[senator::claude] proposes a decision: <body> (seq 130). Reply "yes" to confirm.`
pub(crate) fn proposal_root(l: Locale, event: &Event) -> String {
    let author = author(l, event);
    let seq = event.seq;
    let body = body(l, event);
    match (l, quoted_for(event, &body)) {
        (Locale::Zh, None) => format!("{author} 提议记为决策：{body}（seq {seq}）回复「对」确认"),
        (Locale::Zh, Some(quote)) => {
            format!("{author} 提议记为决策（seq {seq}）回复「对」确认\n{quote}")
        }
        (Locale::En, None) => {
            format!("{author} proposes a decision: {body} (seq {seq}). Reply \"yes\" to confirm.")
        }
        (Locale::En, Some(quote)) => {
            format!("{author} proposes a decision (seq {seq}). Reply \"yes\" to confirm.\n{quote}")
        }
    }
}

/// `已记录决策（seq 131）` / `Decision recorded (seq 131)`.
pub(crate) fn decision_ack(l: Locale, seq: Seq) -> String {
    match l {
        Locale::Zh => format!("已记录决策（seq {seq}）"),
        Locale::En => format!("Decision recorded (seq {seq})"),
    }
}

/// [`quoted`], except for the Speaker's own canned text: a provisional
/// answer ("[senator::claude] is not online …") names the agent
/// first on purpose and is not anyone's words to guard against.
pub(super) fn quoted_for(event: &Event, body: &str) -> Option<String> {
    if senate_core::is_provisional(event) && event.from == Address::Speaker {
        return None;
    }
    quoted(body)
}

/// An agent's body is posted as markdown, so one of its lines could pass
/// for a byline of ours (`[speaker::claude] …`) or a mention. When any
/// line looks like one, the whole body goes under the real byline as a
/// blockquote, every line prefixed, so nothing in it can read as a line of
/// the Speaker's own. Ordinary bodies stay inline.
fn quoted(body: &str) -> Option<String> {
    let forged = body.lines().any(|line| {
        let start = line
            .trim_start_matches(|c: char| {
                c.is_whitespace() || matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}')
            })
            .to_lowercase();
        ["[senator::", "[speaker", "[guest::", "[committee", "@"]
            .iter()
            .any(|prefix| start.starts_with(prefix))
    });
    forged.then(|| {
        body.lines()
            .map(|line| {
                if line.is_empty() {
                    ">".to_owned()
                } else {
                    format!("> {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

/// A Speaker answer written while it had taken over for an away agent.
pub(crate) fn is_takeover(event: &Event) -> bool {
    senate_core::is_takeover(event)
}

/// Who wrote `event`: its byline (`[senator::copilot]`,
/// `[speaker::claude] 接管`, `[senator::copilot] companion`); for
/// the Speaker speaking for someone who is not a senator,
/// `[speaker] [guest::chat/u_1]`; otherwise the bare sender.
fn author(l: Locale, event: &Event) -> String {
    if let Some(byline) = event.byline(l) {
        return byline;
    }
    match (&event.from, &event.on_behalf_of) {
        (Address::Speaker, Some(behalf)) => {
            format!("{} {}", Address::Speaker.in_text(), behalf.in_text())
        }
        (from, _) => from.in_text(),
    }
}

/// The author, with an arrow to the recipients on a directed root:
/// `[senator::claude] → [senator::copilot]`.
fn label(l: Locale, event: &Event) -> String {
    let author = author(l, event);
    let directed = matches!(event.kind, Kind::Question | Kind::Request) || event.thread.is_none();
    if directed && !event.to.is_empty() {
        let to: Vec<String> = event.to.iter().map(Address::in_text).collect();
        format!("{author} → {}", to.join(", "))
    } else {
        author
    }
}

fn body(l: Locale, event: &Event) -> String {
    let text = event.body.trim();
    if text.chars().count() <= MAX_BODY_CHARS {
        return text.to_owned();
    }
    let cut: String = text.chars().take(MAX_BODY_CHARS).collect();
    match l {
        Locale::Zh => format!("{cut}…（全文见 seq {}）", event.seq),
        Locale::En => format!("{cut}… (full text at seq {})", event.seq),
    }
}

// ----- confirmations and actions ---------------------------------------------

pub(crate) fn confirm_action(l: Locale, description: &str) -> String {
    match l {
        Locale::Zh => format!("我理解为：{description}回复「对」确认"),
        Locale::En => format!("I understand: {description} Reply \"yes\" to confirm."),
    }
}

pub(crate) fn describe_invite(l: Locale, handles: &[String]) -> String {
    match l {
        Locale::Zh => format!("邀请 {} 进 Agent 群。", handles.join("、")),
        Locale::En => format!("Invite {} into the Agent group.", handles.join(", ")),
    }
}

pub(crate) fn describe_output_level(l: Locale, level: OutputLevel) -> String {
    match l {
        Locale::Zh => format!("{}。", output_level_note(l, level)),
        Locale::En => format!(
            "Set the Agent group's output level to {}.",
            level_label(l, level)
        ),
    }
}

pub(crate) fn confirm_plan(l: Locale, descriptions: &[String]) -> String {
    let lines: Vec<_> = descriptions
        .iter()
        .enumerate()
        .map(|(i, description)| format!("{}. {description}", i + 1))
        .collect();
    match l {
        Locale::Zh => format!("我理解为：\n{}\n回复「对」确认", lines.join("\n")),
        Locale::En => format!(
            "I understand:\n{}\nReply \"yes\" to confirm.",
            lines.join("\n")
        ),
    }
}

pub(crate) fn action_completed(l: Locale, index: usize, description: &str, text: &str) -> String {
    let detail = if text.is_empty() {
        String::new()
    } else {
        format!("\n{text}")
    };
    match l {
        Locale::Zh => format!("{}. 已执行：{description}{detail}", index + 1),
        Locale::En => format!("{}. Done: {description}{detail}", index + 1),
    }
}

pub(crate) fn action_refused(
    l: Locale,
    index: usize,
    description: &str,
    remaining: usize,
) -> String {
    match l {
        Locale::Zh => format!(
            "{}. 执行失败：{description}操作被拒绝，已停止；后续 {remaining} 项未执行。部分操作可能已生效。",
            index + 1
        ),
        Locale::En => format!(
            "{}. Failed: {description} The action was refused, so I stopped; the remaining {remaining} {} not run. Some steps may already have taken effect.",
            index + 1,
            if remaining == 1 { "was" } else { "were" }
        ),
    }
}

/// An agent plan with more state-changing steps than one confirmation may
/// carry.
pub(crate) fn too_many_actions(l: Locale, max: usize) -> String {
    match l {
        Locale::Zh => format!("一次最多执行 {max} 项操作；这次计划没有执行。"),
        Locale::En => {
            format!("I run at most {max} operations at a time; nothing in this plan was run.")
        }
    }
}

pub(crate) fn action_not_allowed(l: Locale) -> String {
    l.pick(
        "操作被拒绝，未执行。",
        "The action was refused and not run.",
    )
    .to_owned()
}

/// An agent plan's line for a step on `committee`:
/// `在 agent-trial-2 记录决策「…」。` / `On agent-trial-2: Record the decision "…".`
pub(crate) fn on_committee(l: Locale, committee: &CommitteeId, description: &str) -> String {
    match l {
        Locale::Zh => format!("在 {committee} {description}"),
        Locale::En => format!("On {committee}: {description}"),
    }
}

pub(crate) fn describe_relay(l: Locale, to: &[Address], body: &str) -> String {
    match l {
        Locale::Zh => format!("转告 {}：「{}」。", mentions(l, to), one_line(body)),
        Locale::En => format!("Tell {}: \"{}\".", mentions(l, to), one_line(body)),
    }
}

pub(crate) fn describe_status(l: Locale) -> String {
    l.pick(
        "查看本群的绑定和在线的 agent。",
        "Show this group's binding and which agents are online.",
    )
    .to_owned()
}

pub(crate) fn describe_recall(l: Locale) -> String {
    l.pick("回顾已经定下的决策。", "Go over the decisions made so far.")
        .to_owned()
}

/// A step an agent plan puts after an Unbind of the same plan: the group
/// is then bound to nothing, so only a Bind or an Agent-group adoption of a
/// named committee can follow.
pub(crate) fn step_after_unbind(l: Locale) -> String {
    l.pick(
        "这一步排在解绑之后，那时本群已不绑定任何委员会，所以没有列入计划。",
        "This step comes after the unbind, when this group is bound to no committee, so it is not in the plan.",
    )
    .to_owned()
}

pub(crate) fn describe_bind(l: Locale, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!("把本群绑定到委员会 {committee}。"),
        Locale::En => format!("Bind this group to committee {committee}."),
    }
}

pub(crate) fn describe_rebind(l: Locale, from: &CommitteeId, to: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!("把本群从委员会 {from} 改绑到委员会 {to}。"),
        Locale::En => format!("Move this group from committee {from} to committee {to}."),
    }
}

pub(crate) fn describe_unbind(l: Locale, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!("解除本群与委员会 {committee} 的绑定。"),
        Locale::En => format!("Unbind this group from committee {committee}."),
    }
}

pub(crate) fn describe_decide(l: Locale, body: &str, supersedes: Option<Seq>) -> String {
    match l {
        Locale::Zh => {
            let replaces = supersedes
                .map(|seq| format!("，取代 seq {seq} 的决策"))
                .unwrap_or_default();
            format!("记录决策「{}」{replaces}。", one_line(body))
        }
        Locale::En => {
            let replaces = supersedes
                .map(|seq| format!(", replacing the decision at seq {seq}"))
                .unwrap_or_default();
            format!("Record the decision \"{}\"{replaces}.", one_line(body))
        }
    }
}

pub(crate) fn empty_decision(l: Locale) -> String {
    match l {
        Locale::Zh => format!("要记录什么决策？{}", usage(l)),
        Locale::En => format!("What decision should I record? {}", usage(l)),
    }
}

pub(crate) fn not_a_decision(l: Locale, seq: Seq) -> String {
    match l {
        Locale::Zh => format!("seq {seq} 不是本委员会的一条决策，没法取代它。"),
        Locale::En => {
            format!("seq {seq} is not a decision of this committee, so it cannot be replaced.")
        }
    }
}

pub(crate) fn bound(l: Locale, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!("已绑定到委员会 {committee}。"),
        Locale::En => format!("Bound to committee {committee}."),
    }
}

pub(crate) fn unbound(l: Locale, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!("已解除本群与委员会 {committee} 的绑定。"),
        Locale::En => format!("This group is no longer bound to committee {committee}."),
    }
}

pub(crate) fn already_bound(l: Locale, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!("本群已经绑定在委员会 {committee}。"),
        Locale::En => format!("This group is already bound to committee {committee}."),
    }
}

pub(crate) fn cancelled(l: Locale) -> String {
    l.pick("好的，已取消。", "OK, cancelled.").to_owned()
}

/// The body of a binding event: `绑定群 <group>` / `Bound group <group>`.
pub(crate) fn binding_body(l: Locale, bind: bool, group: &str) -> String {
    match (l, bind) {
        (Locale::Zh, true) => format!("绑定群 {group}"),
        (Locale::Zh, false) => format!("解除绑定群 {group}"),
        (Locale::En, true) => format!("Bound group {group}"),
        (Locale::En, false) => format!("Unbound group {group}"),
    }
}

// ----- status and presence ---------------------------------------------------

/// One seat on the committee as people read it, in `/status` and the
/// pinned card: the name people are shown, presence and duty, from the
/// same seats the Speaker's prompt lists.
/// `senate-dev（dark-conclave） 在线 · 职责：Senate 开发` /
/// `senate-dev (dark-conclave) online · duty: Senate development`.
pub(crate) fn seat_line(l: Locale, seat: &crate::brain::CommitteeSeat) -> String {
    let presence = match seat.presence {
        _ if seat.online => l.pick("在线", "online").to_owned(),
        Some(state) if state != PresenceState::Online => presence_word(l, state, None),
        _ => l.pick("离线", "offline").to_owned(),
    };
    let mut line = format!("{} {presence}", seat.people_name());
    duty_suffix(l, &mut line, seat.duty.as_deref());
    line
}

/// `/status`: the binding, which seats are online (here and in child
/// Senates), each seat with its presence and duty ([`seat_line`]), and
/// the presence line of each agent here ([`presence_status`]).
pub(crate) fn status(
    l: Locale,
    committee: &CommitteeId,
    seats: &[crate::brain::CommitteeSeat],
    presence: &[String],
) -> String {
    let list = |online: bool| {
        let names: Vec<String> = seats
            .iter()
            .filter(|s| s.online == online)
            .map(crate::brain::CommitteeSeat::people_name)
            .collect();
        if names.is_empty() {
            l.pick("无", "none").to_owned()
        } else {
            join(l, names)
        }
    };
    let mut text = match l {
        Locale::Zh => format!(
            "本群绑定在委员会 {committee}。\n在线 agent：{}\n离线 agent：{}",
            list(true),
            list(false)
        ),
        Locale::En => format!(
            "This group is bound to committee {committee}.\nAgents online: {}\nAgents offline: {}",
            list(true),
            list(false)
        ),
    };
    if !seats.is_empty() {
        text.push_str(l.pick("\n席位：", "\nSeats:"));
        for seat in seats {
            let _ = write!(text, "\n- {}", seat_line(l, seat));
        }
    }
    if !presence.is_empty() {
        text.push_str(l.pick("\n在场：", "\nPresence:"));
        for line in presence {
            let _ = write!(text, "\n- {line}");
        }
    }
    text
}

/// A presence state in words, as the Agent group, `/status` and the pinned
/// card show it.
pub(crate) fn presence_word(
    l: Locale,
    state: PresenceState,
    new_owner: Option<&SenatorName>,
) -> String {
    match (state, new_owner) {
        (PresenceState::Absent, _) => l
            .pick("缺席（租约过期）", "absent (lease expired)")
            .to_owned(),
        (PresenceState::Reassigned, Some(owner)) => match l {
            Locale::Zh => format!("已交给 {}", senator(owner)),
            Locale::En => format!("handed to {}", senator(owner)),
        },
        (state, _) => state.label(l).to_owned(),
    }
}

/// The Agent group's line for a presence transition:
/// `[senator::claude] 失联` / `缺席（租约过期）` / `由 Speaker 接管` /
/// `在线` / `已交给 [senator::copilot]`.
pub(crate) fn presence_line(l: Locale, payload: &PresencePayload) -> String {
    match (payload.state, &payload.new_owner, payload.previous) {
        (PresenceState::Reassigned, Some(owner), _) => {
            return reassigned_line(l, &payload.agent, owner);
        }
        (PresenceState::Online, _, Some(PresenceState::Reassigned)) => {
            return work_handback_line(l, &payload.agent);
        }
        _ => {}
    }
    format!(
        "{} {}",
        senator(&payload.agent),
        presence_word(l, payload.state, payload.new_owner.as_ref())
    )
}

/// `3 分钟` / `2 小时` / `3 天`; `3 minutes` / `2 hours` / `3 days`.
pub(crate) fn age(l: Locale, age: chrono::Duration) -> String {
    let minutes = age.num_minutes().max(0);
    let (n, zh, en) = if minutes < 60 {
        (minutes, "分钟", "minute")
    } else if minutes < 48 * 60 {
        (minutes / 60, "小时", "hour")
    } else {
        (minutes / (24 * 60), "天", "day")
    };
    match l {
        Locale::Zh => format!("{n} {zh}"),
        Locale::En if n == 1 => format!("1 {en}"),
        Locale::En => format!("{n} {en}s"),
    }
}

/// One agent in `/status`:
/// `[senator::claude] 缺席（租约过期） · 落后 14 条事件 · 检查点 3 小时前（seq 40）` /
/// `[senator::claude] absent (lease expired) · 14 events behind · checkpoint 3 hours ago (seq 40)`.
pub(crate) fn presence_status(
    l: Locale,
    agent: &SenatorName,
    presence: Option<&AgentPresence>,
    checkpoint: Option<(Seq, chrono::Duration)>,
) -> String {
    let mut line = senator(agent);
    match presence {
        Some(p) => {
            let _ = write!(line, " {}", presence_word(l, p.state, p.new_owner.as_ref()));
            if let Some(lag) = p.lag.filter(|lag| *lag > 0) {
                let _ = write!(line, " · {}", lag_words(l, lag));
            }
            duty_suffix(l, &mut line, p.duty.as_deref());
        }
        None => line.push_str(l.pick(" 在场未知", " presence unknown")),
    }
    match (l, checkpoint) {
        (Locale::Zh, Some((seq, since))) => {
            let _ = write!(line, " · 检查点 {}前（seq {seq}）", age(l, since));
        }
        (Locale::En, Some((seq, since))) => {
            let _ = write!(line, " · checkpoint {} ago (seq {seq})", age(l, since));
        }
        (_, None) => line.push_str(l.pick(" · 没有检查点", " · no checkpoint")),
    }
    line
}

/// ` · 职责：<duty>` / ` · duty: <duty>`, when the seat has one.
fn duty_suffix(l: Locale, line: &mut String, duty: Option<&str>) {
    if let Some(duty) = duty {
        let _ = match l {
            Locale::Zh => write!(line, " · 职责：{duty}"),
            Locale::En => write!(line, " · duty: {duty}"),
        };
    }
}

/// The Agent group's line for a checkpoint:
/// `[senator::claude] 检查点 · feature/x@a41f9c2（已推送）· seq 40` /
/// `[senator::claude] checkpoint · feature/x@a41f9c2 (pushed) · seq 40`.
pub(crate) fn checkpoint_line(l: Locale, event: &Event, payload: &CheckpointPayload) -> String {
    let author = author(l, event);
    match l {
        Locale::Zh => format!(
            "{author} 检查点 · {}（{}）· seq {}",
            payload.at(),
            if payload.pushed {
                "已推送"
            } else {
                "未推送"
            },
            event.seq
        ),
        Locale::En => format!(
            "{author} checkpoint · {} ({}) · seq {}",
            payload.at(),
            if payload.pushed {
                "pushed"
            } else {
                "not pushed"
            },
            event.seq
        ),
    }
}

// ----- takeover --------------------------------------------------------------

/// Why a takeover went to people instead of being answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoCover {
    /// No checkpoint at all.
    Missing,
    /// The newest checkpoint is older than `takeover_stale_hours`.
    Stale,
    /// The model said the checkpoint, brief and log do not cover it.
    Uncovered,
    /// The model could not answer at all.
    Unavailable,
}

/// `[senator::claude] 不在，检查点太旧，这条需要人：<question>` /
/// `[senator::claude] is away and its checkpoint is too old, so this needs a person: <question>`.
pub(crate) fn takeover_escalation(
    l: Locale,
    agent: &Address,
    why: NoCover,
    question: &str,
) -> String {
    let question = cut(&one_line(question), BRIEF_ITEM_CHARS);
    let agent = agent.in_text();
    match l {
        Locale::Zh => {
            let why = match why {
                NoCover::Missing => "检查点没有",
                NoCover::Stale => "检查点太旧",
                NoCover::Uncovered => "检查点没有覆盖这个问题",
                NoCover::Unavailable => "Speaker 现在答不了",
            };
            format!("{agent} 不在，{why}，这条需要人：{question}")
        }
        Locale::En => {
            let why = match why {
                NoCover::Missing => "it left no checkpoint",
                NoCover::Stale => "its checkpoint is too old",
                NoCover::Uncovered => "its checkpoint does not cover this",
                NoCover::Unavailable => "the Speaker cannot answer right now",
            };
            format!("{agent} is away and {why}, so this needs a person: {question}")
        }
    }
}

/// One answer the Speaker gave while it had taken over.
pub(crate) struct HandedBack {
    pub(crate) question_seq: Seq,
    pub(crate) answer_seq: Seq,
    pub(crate) question: String,
    pub(crate) answer: String,
}

/// The note to the Speaker when the built-in session's configuration files
/// have new errors: where each is, and that the last good configuration
/// stays in force. `problems` are rendered in the deployment's language
/// already.
pub(crate) fn config_problems_note(l: Locale, problems: &[crate::Problem]) -> String {
    let mut text = l
        .pick(
            "内置会话的配置文件有错误，上一份可用的配置仍在生效：",
            "The built-in session's configuration files have errors; the last good configuration stays in force:",
        )
        .to_owned();
    for problem in problems {
        let _ = write!(text, "\n- {problem}");
    }
    text.push_str(l.pick(
        "\n改好后会自动重新读取；`senate status` 也列出这些错误。",
        "\nFix the file and it is read again by itself; `senate status` lists these errors too.",
    ));
    text
}

/// The note pushed to an agent that came back: every answer given for it.
pub(crate) fn handback_note(l: Locale, since: Seq, answers: &[HandedBack]) -> String {
    let n = answers.len();
    let mut text = match l {
        Locale::Zh => {
            format!("你不在时 Speaker 接管了你的信箱（seq {since} 起），代你回答了 {n} 条：")
        }
        Locale::En => format!(
            "While you were away the Speaker took over your inbox (from seq {since}) and answered {n} {} for you:",
            if n == 1 { "question" } else { "questions" }
        ),
    };
    for a in answers {
        let question = cut(&one_line(&a.question), PINNED_TITLE_CHARS);
        let answer = cut(&one_line(&a.answer), PINNED_TITLE_CHARS);
        let _ = match l {
            Locale::Zh => write!(
                text,
                "\n- seq {} · 问：{question} → 答（seq {}）：{answer}",
                a.question_seq, a.answer_seq
            ),
            Locale::En => write!(
                text,
                "\n- seq {} · Q: {question} → A (seq {}): {answer}",
                a.question_seq, a.answer_seq
            ),
        };
    }
    text.push_str(l.pick(
        "\n有不对的地方请在对应话题里更正。",
        "\nIf anything is wrong, correct it in its thread.",
    ));
    text
}

/// `[senator::claude] 回来了，Speaker 交还 2 条代答` /
/// `[senator::claude] is back; the Speaker handed back 2 answers`.
pub(crate) fn handback_line(l: Locale, agent: &Address, count: usize) -> String {
    let agent = agent.in_text();
    match l {
        Locale::Zh => format!("{agent} 回来了，Speaker 交还 {count} 条代答"),
        Locale::En => format!(
            "{agent} is back; the Speaker handed back {count} {}",
            if count == 1 { "answer" } else { "answers" }
        ),
    }
}

// ----- bindings and relays ---------------------------------------------------

pub(crate) fn unbound_status(l: Locale) -> String {
    l.pick(
        "本群还没有绑定委员会。用 /bind <committee> 绑定。",
        "This group is not bound to a committee yet. Bind it with /bind <committee>.",
    )
    .to_owned()
}

pub(crate) fn nothing_to_unbind(l: Locale) -> String {
    l.pick(
        "本群没有绑定任何委员会。",
        "This group is not bound to any committee.",
    )
    .to_owned()
}

pub(crate) fn relay_needs_binding(l: Locale) -> String {
    l.pick(
        "本群还没有绑定委员会，我没法转达。先用 /bind <committee> 绑定。",
        "This group is not bound to a committee yet, so I cannot pass anything on. Bind it first with /bind <committee>.",
    )
    .to_owned()
}

pub(crate) fn decide_needs_binding(l: Locale) -> String {
    l.pick(
        "本群还没有绑定委员会，我没法记录决策。先用 /bind <committee> 绑定。",
        "This group is not bound to a committee yet, so I cannot record a decision. Bind it first with /bind <committee>.",
    )
    .to_owned()
}

pub(crate) fn recall_needs_binding(l: Locale) -> String {
    l.pick(
        "本群还没有绑定委员会，还没有可以回顾的记录。先用 /bind <committee> 绑定。",
        "This group is not bound to a committee yet, so there is no record to go over. Bind it first with /bind <committee>.",
    )
    .to_owned()
}

pub(crate) fn invalid_committee(l: Locale, raw: &str) -> String {
    match l {
        Locale::Zh => {
            format!("「{raw}」不是合法的委员会名：只能用小写字母、数字和 -，以字母或数字开头。")
        }
        Locale::En => format!(
            "\"{raw}\" is not a valid committee name: use lowercase letters, digits and -, starting with a letter or digit."
        ),
    }
}

/// Whom to relay to, when some names were not on the roster (or none was
/// given): every person on the roster, by handle and first name, and how to
/// name one agent.
pub(crate) fn unknown_targets(l: Locale, unknown: &[String], roster: &Roster) -> String {
    let known: Vec<String> = roster
        .principals()
        .iter()
        .map(|s| match (l, s.names.first()) {
            (Locale::Zh, Some(name)) => format!("{}（{name}）", s.handle),
            (Locale::En, Some(name)) => format!("{} ({name})", s.handle),
            (_, None) => s.handle.to_string(),
        })
        .collect();
    let known = if known.is_empty() {
        l.pick("（名册里还没有人）", "(no one is on the roster yet)")
            .to_owned()
    } else {
        join(l, known)
    };
    match (l, unknown.is_empty()) {
        (Locale::Zh, true) => {
            format!("要转给谁？名册上的人：{known}。要指明某个 agent，直接写它的名字。")
        }
        (Locale::Zh, false) => format!(
            "我不认识 {}。名册上的人：{known}。要指明某个 agent，直接写它的名字。",
            unknown.join("、")
        ),
        (Locale::En, true) => format!(
            "Whom should I pass it to? People on the roster: {known}. To name one agent, write its own name."
        ),
        (Locale::En, false) => format!(
            "I don't know {}. People on the roster: {known}. To name one agent, write its own name.",
            unknown.join(", ")
        ),
    }
}

pub(crate) fn empty_relay(l: Locale) -> String {
    match l {
        Locale::Zh => format!("要转达什么内容？{}", usage(l)),
        Locale::En => format!("What should I pass on? {}", usage(l)),
    }
}

/// The one reply to a guest who asks for anything that changes state.
pub(crate) fn guest_refused(l: Locale) -> String {
    l.pick(
        "只有名册上的人可以让我改动东西：绑定、决策、邀请、输出等级、代码任务、改派。你仍然可以问我问题。",
        "Only people on the roster can have me change anything: bindings, decisions, invitations, the output level, work orders, reassignment. You can still ask me questions.",
    )
    .to_owned()
}

/// The reply to a guest who asks to pass on a note or an instruction.
pub(crate) fn guest_note_refused(l: Locale) -> String {
    l.pick(
        "我可以把你的问题转给 agent，但不能转达留言或指示。",
        "I can pass on your questions to agents, but not notes or instructions.",
    )
    .to_owned()
}

pub(crate) fn not_understood(l: Locale) -> String {
    match l {
        Locale::Zh => format!("抱歉，我现在没法理解这句话。{}", usage(l)),
        Locale::En => format!("Sorry, I can't make sense of that right now. {}", usage(l)),
    }
}

/// The on-behalf answer when the Brain could not produce one: honest, and it
/// still tells the asker where things stand.
pub(crate) fn fallback_unavailable(l: Locale, agent: &Address) -> String {
    let agent = agent.in_text();
    match l {
        Locale::Zh => format!(
            "{agent} 现在不在线，我也没能从记录里找到可靠的答案。等 {agent} 上线后会收到这个问题。"
        ),
        Locale::En => format!(
            "{agent} is not online right now, and I could not find a reliable answer in the record. {agent} will get this question when it comes back online."
        ),
    }
}

/// The on-behalf answer when the Brain could not produce one and the agent's
/// bridge is still connected: the question was delivered, but the agent has
/// not answered within `answer_fallback_secs` (busy, or waiting for its
/// person). The engine picks this over [`fallback_unavailable`] whenever
/// the agent is connected, so a working agent is never called offline; its
/// own answer still lands in the thread when it comes.
pub(crate) fn fallback_pending(l: Locale, agent: &Address) -> String {
    let agent = agent.in_text();
    match l {
        Locale::Zh => format!(
            "{agent} 已经收到这个问题，还没有回复；我也没能从记录里找到可靠的答案。{agent} 回复后会发在这里。"
        ),
        Locale::En => format!(
            "{agent} has the question but has not replied yet, and I could not find a reliable answer in the record. {agent}'s reply will appear here when it comes."
        ),
    }
}

// ----- the brief -------------------------------------------------------------

/// The brief as a reply in the group, at most [`MAX_BRIEF_CHARS`]: the
/// top, the decisions in force, the open questions. When it is too long the
/// oldest questions go first, then the oldest decisions, each with a note of
/// how many were left out.
pub(crate) fn brief(l: Locale, brief: &Brief) -> String {
    let mut head = match l {
        Locale::Zh => format!(
            "委员会 {} 的简报（截至 seq {}）\n",
            brief.committee, brief.head
        ),
        Locale::En => format!(
            "Brief for committee {} (as of seq {})\n",
            brief.committee, brief.head
        ),
    };
    if let Some(top) = &brief.top {
        let _ = match l {
            Locale::Zh => write!(
                head,
                "\n当前重点（seq {}）：\n{}\n",
                top.seq,
                top.body.trim()
            ),
            Locale::En => write!(
                head,
                "\nCurrent focus (seq {}):\n{}\n",
                top.seq,
                top.body.trim()
            ),
        };
    }
    let colon = l.pick("：", ": ");
    let decisions: Vec<String> = brief
        .decisions
        .iter()
        .map(|d| {
            format!(
                "- seq {} · {} · {}{colon}{}",
                d.seq,
                d.by.in_text(),
                d.ts.format("%m-%d"),
                item(&d.body)
            )
        })
        .collect();
    let questions: Vec<String> = brief
        .open_questions
        .iter()
        .map(|q| {
            let to = if q.to.is_empty() {
                String::new()
            } else {
                format!(" → {}", mentions(l, &q.to))
            };
            format!(
                "- seq {} · {}{to}{colon}{}",
                q.seq,
                q.from.in_text(),
                item(&q.body)
            )
        })
        .collect();
    let (mut kept_decisions, mut kept_questions) = (decisions.len(), questions.len());
    loop {
        let mut out = head.clone();
        out.push_str(&section(
            l,
            l.pick("已定决策", "Decisions in force"),
            &decisions[decisions.len() - kept_decisions..],
            decisions.len(),
            l.pick("还没有记录。", "none recorded yet."),
        ));
        out.push_str(&section(
            l,
            l.pick("待回答的问题", "Open questions"),
            &questions[questions.len() - kept_questions..],
            questions.len(),
            l.pick("无。", "none."),
        ));
        if out.chars().count() <= MAX_BRIEF_CHARS || (kept_decisions == 0 && kept_questions == 0) {
            return cut(&out, MAX_BRIEF_CHARS);
        }
        if kept_questions > 0 {
            kept_questions -= 1;
        } else {
            kept_decisions -= 1;
        }
    }
}

fn section(l: Locale, title: &str, shown: &[String], total: usize, none: &str) -> String {
    let colon = l.pick("：", ":");
    if total == 0 {
        let space = l.pick("", " ");
        return format!("\n{title}{colon}{space}{none}\n");
    }
    let mut out = format!("\n{title}{colon}\n");
    if shown.len() < total {
        let _ = match l {
            Locale::Zh => writeln!(out, "（更早的 {} 条没有列出）", total - shown.len()),
            Locale::En => writeln!(out, "({} earlier not shown)", total - shown.len()),
        };
    }
    for line in shown {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// What the Brain wrote for the brief's top, as stored: at most
/// [`TOP_LINES`] non-empty lines of at most [`TOP_LINE_CHARS`] each. Empty
/// when there was nothing usable.
pub(crate) fn top_lines(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(TOP_LINES)
        .map(|line| cut(line, TOP_LINE_CHARS))
        .collect::<Vec<_>>()
        .join("\n")
}

fn item(text: &str) -> String {
    cut(&one_line(text), BRIEF_ITEM_CHARS)
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cut(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        None => text.to_owned(),
        Some((at, _)) => format!("{}…", &text[..at]),
    }
}

// ----- triage, task threads, the Agent group ---------------------------------

/// Characters of the latest answer shown on a task card.
const CARD_ANSWER_CHARS: usize = 200;
/// Lines on the pinned "waiting on people" card.
pub(crate) const WAITING_LINES: usize = 10;
/// Items per section of the pinned "today" card.
const TODAY_ITEMS: usize = 8;
/// Title characters in one line of a pinned card.
const PINNED_TITLE_CHARS: usize = 40;

/// The start of an answer, as a card shows it.
pub(crate) fn excerpt(text: &str) -> String {
    cut(&one_line(text), CARD_ANSWER_CHARS)
}

/// `已转给 [senator::claude]（seq 120）`, and when triage parked it on
/// the person: `。Speaker 判断这条需要 [senator::alice] 本人处理，已提醒 [senator::alice]`.
/// The person is the first recipient's: a person themselves, or the person
/// the roster says answers for an agent.
///
/// A relay meant for one seat in a child Senate names the seat, not the
/// desk it goes through: `已转给 [north-desk::refund-v2::senator::codex]（seq 120）`.
/// A seat with an alias is shown by it (`已转给 senate-dev（dark-conclave）`,
/// `已转给 [senator::reviewer]`), never by its primary id.
pub(crate) fn relay_ack_for(
    l: Locale,
    event: &Event,
    roster: &Roster,
    names: SeatNames<'_>,
) -> String {
    let ack = match (event.to.as_slice(), senate_core::locator::intended(event)) {
        ([Address::Child { .. }], Some(seat)) => relay_ack_to(
            l,
            &names.child(l, &seat).unwrap_or_else(|| seat.in_text()),
            event.seq,
        ),
        _ => relay_ack_to(
            l,
            &join(l, event.to.iter().map(|a| names.local(&event.committee, a))),
            event.seq,
        ),
    };
    let parked = TriagePayload::of(event).is_some_and(|t| t.verdict == TriageVerdict::NeedsHuman);
    let owner = event.to.iter().find_map(|a| {
        let senator = a.as_senator()?;
        if roster.is_person(senator) {
            Some(person(senator.name()))
        } else {
            roster.owner_of(senator).map(person)
        }
    });
    match (parked, owner, l) {
        (true, Some(owner), Locale::Zh) => {
            format!("{ack}。Speaker 判断这条需要 {owner} 本人处理，已提醒 {owner}")
        }
        (true, Some(owner), Locale::En) => format!(
            "{ack}. The Speaker judged that this needs {owner} in person and has let {owner} know."
        ),
        _ => ack,
    }
}

fn verdict_label(l: Locale, verdict: TriageVerdict) -> &'static str {
    match verdict {
        TriageVerdict::NeedsHuman => l.pick("需要本人", "needs the person"),
        TriageVerdict::Agent => l.pick("交给 agent", "for the agent"),
        TriageVerdict::Unsure => l.pick(
            "不确定，agent 会先问本人",
            "unsure; the agent will ask its person first",
        ),
    }
}

/// The body of the Speaker's triage `note`: one line.
/// `分流：交给 agent（p 本人 0.10 · agent 0.80 · 不确定 0.10）` /
/// `Triage: for the agent (p person 0.10 · agent 0.80 · unsure 0.10)`.
pub(crate) fn triage_note(l: Locale, triage: &TriagePayload) -> String {
    let mut line = match l {
        Locale::Zh => format!("分流：{}", verdict_label(l, triage.verdict)),
        Locale::En => format!("Triage: {}", verdict_label(l, triage.verdict)),
    };
    if let Some(rule) = &triage.rule {
        let _ = match l {
            Locale::Zh => write!(line, "（规则 {rule}）"),
            Locale::En => write!(line, " (rule {rule})"),
        };
    } else if triage.p_needs_human + triage.p_agent + triage.p_unsure > 0.0 {
        let _ = match l {
            Locale::Zh => write!(
                line,
                "（p 本人 {:.2} · agent {:.2} · 不确定 {:.2}）",
                triage.p_needs_human, triage.p_agent, triage.p_unsure
            ),
            Locale::En => write!(
                line,
                " (p person {:.2} · agent {:.2} · unsure {:.2})",
                triage.p_needs_human, triage.p_agent, triage.p_unsure
            ),
        };
    }
    if let Some(reason) = &triage.reason {
        let _ = write!(line, "{}{}", l.pick("：", ": "), one_line(reason));
    }
    line
}

/// Why a parked thread waits on its person, for the status and the mention:
/// the reason, with the rule that matched.
pub(crate) fn triage_reason(l: Locale, triage: &TriagePayload) -> String {
    let reason = triage.reason.as_deref().map(one_line).unwrap_or_else(|| {
        l.pick(
            "Speaker 判断这条要你本人来定",
            "The Speaker judged that this is yours to decide",
        )
        .to_owned()
    });
    match (&triage.rule, l) {
        (Some(rule), Locale::Zh) => format!("{reason}（规则 {rule}）"),
        (Some(rule), Locale::En) => format!("{reason} (rule {rule})"),
        (None, _) => reason,
    }
}

/// The reason the hard rule `request` gives.
pub(crate) fn request_rule_reason(l: Locale) -> String {
    l.pick(
        "改动请求要先经过本人",
        "A change request goes to the person first",
    )
    .to_owned()
}

/// The reason when a person asks their own agent.
pub(crate) fn own_agent_reason(l: Locale) -> String {
    l.pick(
        "本人交给自己的 agent",
        "The person handed it to their own agent",
    )
    .to_owned()
}

/// The reason given when no model could judge in time.
pub(crate) fn triage_unavailable(l: Locale) -> String {
    l.pick(
        "Speaker 这次没能判断",
        "The Speaker could not judge this time",
    )
    .to_owned()
}

/// After the native mention: `这条需要你本人：<reason>。回复「交给我的 agent」或直接回答` /
/// `This needs you in person: <reason>. Reply "hand it to my agent" or answer it directly.`
pub(crate) fn needs_human(l: Locale, reason: &str) -> String {
    match l {
        Locale::Zh => format!(
            "这条需要你本人：{}。回复「交给我的 agent」或直接回答",
            one_line(reason)
        ),
        Locale::En => format!(
            "This needs you in person: {}. Reply \"hand it to my agent\" or answer it directly.",
            one_line(reason)
        ),
    }
}

/// After the native mention: `[senator::copilot] 把这条交给你：<reason>` /
/// `[senator::copilot] handed this to you: <reason>`.
pub(crate) fn escalated(l: Locale, agent: &Address, reason: Option<&str>) -> String {
    let agent = agent.in_text();
    match (reason.map(one_line).filter(|r| !r.is_empty()), l) {
        (Some(reason), Locale::Zh) => format!("{agent} 把这条交给你：{reason}"),
        (Some(reason), Locale::En) => format!("{agent} handed this to you: {reason}"),
        (None, Locale::Zh) => format!("{agent} 把这条交给你"),
        (None, Locale::En) => format!("{agent} handed this to you"),
    }
}

/// A mention written out, for a surface without native mentions. No words of
/// its own, so no locale.
pub(crate) fn with_at(name: &str, text: &str) -> String {
    format!("@{name} {text}")
}

/// `已转给 [senator::alice] 本人` / `Passed to [senator::alice] in person`.
pub(crate) fn escalated_ack(l: Locale, who: &Name) -> String {
    let who = person(who);
    match l {
        Locale::Zh => format!("已转给 {who} 本人"),
        Locale::En => format!("Passed to {who} in person"),
    }
}

/// The reason of a human-first takeover.
pub(crate) fn takeover_reason(l: Locale) -> String {
    l.pick("人工处理中", "A person is handling it").to_owned()
}

/// The reason on the status of an approved request.
pub(crate) fn approved_reason(l: Locale, agent: &Address) -> String {
    match l {
        Locale::Zh => format!("本人已交给 {}", agent.in_text()),
        Locale::En => format!("The person handed it to {}", agent.in_text()),
    }
}

/// The reply to 「交给我的 agent」 / "hand it to my agent".
pub(crate) fn handed_off(l: Locale, agent: &Address) -> String {
    match l {
        Locale::Zh => format!("好的，已交给 {}。", agent.in_text()),
        Locale::En => format!("OK, handed to {}.", agent.in_text()),
    }
}

pub(crate) fn already_handed_off(l: Locale) -> String {
    l.pick(
        "这条已经交给 agent 了。",
        "This has already been handed to the agent.",
    )
    .to_owned()
}

/// The handoff phrase from someone the thread does not wait on.
pub(crate) fn handoff_not_yours(l: Locale, waits_on: &Name) -> String {
    let who = person(waits_on);
    match l {
        Locale::Zh => format!("这条在等 {who} 本人，只有 TA 能交给自己的 agent。"),
        Locale::En => {
            format!("This is waiting on {who} in person; only they can hand it to their own agent.")
        }
    }
}

/// The Agent group's name on the chat surface: `[Agents] refund-v2` /
/// `[Agent 群] refund-v2`.
pub(crate) fn agents_group_name(l: Locale, committee: &CommitteeId) -> String {
    format!("{} {committee}", l.pick("[Agent 群]", "[Agents]"))
}

/// The Agent group's name once archived: `[Archived] refund-v2` /
/// `[已归档] refund-v2`.
pub(crate) fn archived_group_name(l: Locale, committee: &CommitteeId) -> String {
    format!("{} {committee}", l.pick("[已归档]", "[Archived]"))
}

/// Posted once in the first people group when the Agent group cannot be
/// created. `bot_name` is the Speaker's name on the chat surface.
pub(crate) fn no_agents_group_hint(l: Locale, committee: &CommitteeId, bot_name: &str) -> String {
    match l {
        Locale::Zh => format!(
            "还没有 Agent 群：建一个群，把 @{bot_name} 拉进去，说「这个群是 {committee} 的 agent 群」"
        ),
        Locale::En => format!(
            "There is no Agent group yet: create a group, add @{bot_name} to it, and say \"/agents {committee}\" there."
        ),
    }
}

pub(crate) fn describe_adopt(l: Locale, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!(
            "把本群设为委员会 {committee} 的 Agent 群（agent 之间的往来和任务卡片都发在这里）。"
        ),
        Locale::En => format!(
            "Make this group the Agent group of committee {committee} (traffic between agents and the task cards are posted here)."
        ),
    }
}

pub(crate) fn adopted(l: Locale, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!("本群现在是委员会 {committee} 的 Agent 群。"),
        Locale::En => format!("This group is now the Agent group of committee {committee}."),
    }
}

pub(crate) fn adopt_needs_committee(l: Locale) -> String {
    match l {
        Locale::Zh => format!(
            "要设为哪个委员会的 Agent 群？用 /agents <committee>。{}",
            usage(l)
        ),
        Locale::En => format!(
            "Which committee should this be the Agent group of? Use /agents <committee>. {}",
            usage(l)
        ),
    }
}

pub(crate) fn adopt_other_committee(l: Locale, bound: &CommitteeId, asked: &CommitteeId) -> String {
    match l {
        Locale::Zh => {
            format!("本群绑定在委员会 {bound}，不能设为委员会 {asked} 的 Agent 群。先 /unbind。")
        }
        Locale::En => format!(
            "This group is bound to committee {bound}, so it cannot be the Agent group of committee {asked}. /unbind first."
        ),
    }
}

pub(crate) fn agents_need_binding(l: Locale) -> String {
    l.pick(
        "本群还没有绑定委员会。先用 /bind <committee> 绑定，或用 /agents <committee> 设为 Agent 群。",
        "This group is not bound to a committee yet. Bind it with /bind <committee>, or make it an Agent group with /agents <committee>.",
    )
    .to_owned()
}

pub(crate) fn no_agents_group(l: Locale, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!(
            "委员会 {committee} 还没有 Agent 群。在一个群里说「这个群是 {committee} 的 agent 群」。"
        ),
        Locale::En => format!(
            "Committee {committee} has no Agent group yet. In a group, say \"/agents {committee}\"."
        ),
    }
}

pub(crate) fn invite_whom(l: Locale) -> String {
    match l {
        Locale::Zh => format!("要把谁拉进 Agent 群？{}", usage(l)),
        Locale::En => format!("Whom should I add to the Agent group? {}", usage(l)),
    }
}

pub(crate) fn invited(l: Locale, names: &[String]) -> String {
    match l {
        Locale::Zh => format!("已邀请 {} 进 Agent 群。", names.join("、")),
        Locale::En => format!("Invited {} into the Agent group.", names.join(", ")),
    }
}

pub(crate) fn invite_unsupported(l: Locale, names: &[String]) -> String {
    match l {
        Locale::Zh => format!(
            "我没有权限直接拉人进群，请群主手动邀请 {}。",
            names.join("、")
        ),
        Locale::En => format!(
            "I am not allowed to add people to the group; please ask the group owner to invite {}.",
            names.join(", ")
        ),
    }
}

pub(crate) fn agents_group_members_missing(l: Locale, group: &str, names: &[String]) -> String {
    match l {
        Locale::Zh => format!(
            "已建好 {group}，但没能把 {} 拉进去（聊天应用可能缺少添加群成员的权限），请手动邀请。",
            names.join("、")
        ),
        Locale::En => format!(
            "Created {group}, but could not add {} (the chat app may lack permission to add group members); please invite them by hand.",
            names.join(", ")
        ),
    }
}

pub(crate) fn invite_failed(l: Locale, names: &[String]) -> String {
    match l {
        Locale::Zh => format!("没能邀请 {}，请群主手动邀请。", names.join("、")),
        Locale::En => format!(
            "Could not invite {}; please ask the group owner to invite them.",
            names.join(", ")
        ),
    }
}

fn level_label(l: Locale, level: OutputLevel) -> &'static str {
    match level {
        OutputLevel::Answers => l.pick("只回答案", "answers only"),
        OutputLevel::Status => l.pick("答案和状态", "answers and status"),
        OutputLevel::All => l.pick("全部往返", "all traffic"),
    }
}

/// The body of the Speaker's output-level `note`.
pub(crate) fn output_level_note(l: Locale, level: OutputLevel) -> String {
    match l {
        Locale::Zh => format!("Agent 群输出等级：{}", level_label(l, level)),
        Locale::En => format!("Agent group output level: {}", level_label(l, level)),
    }
}

pub(crate) fn output_level_set(l: Locale, level: OutputLevel) -> String {
    match l {
        Locale::Zh => format!("好的，Agent 群以后{}。", level_label(l, level)),
        Locale::En => format!(
            "OK, the Agent group will show {} from now on.",
            level_label(l, level)
        ),
    }
}

/// `由 <who> 回答 · 来自 refund-v2 的 Agent 群` /
/// `Answered by <who> · from the refund-v2 Agent group`.
fn signature_of(l: Locale, who: &str, committee: &CommitteeId) -> String {
    match l {
        Locale::Zh => format!("由 {who} 回答 · 来自 {committee} 的 Agent 群"),
        Locale::En => format!("Answered by {who} · from the {committee} Agent group"),
    }
}

/// The last line of an answer mirrored back to a people group:
/// `由 [senator::claude] 回答 · 来自 refund-v2 的 Agent 群`.
pub(crate) fn signature(l: Locale, event: &Event) -> String {
    signature_of(l, &author(l, event), &event.committee)
}

/// An answer for a people group: the usual reply, then the signature.
pub(crate) fn signed(l: Locale, event: &Event) -> String {
    format!("{}\n\n{}", thread_mirror(l, event), signature(l, event))
}

fn state_label(l: Locale, state: StatusState) -> &'static str {
    match state {
        StatusState::Processing => l.pick("处理中", "in progress"),
        StatusState::Active => l.pick("已回答", "answered"),
        StatusState::Suspended => l.pick("等人处理", "waiting on a person"),
        StatusState::Archived => l.pick("已归档", "archived"),
    }
}

/// A task's card: who asked whom, the triage line, why it waits, the latest
/// answer. `verdict` overrides the thread's own when the note is not folded
/// in yet.
pub(crate) fn task_card(
    l: Locale,
    thread: &ThreadSummary,
    verdict: Option<TriageVerdict>,
    answer: Option<&str>,
) -> Card {
    let mut markdown = String::new();
    let asker = match (&thread.from, &thread.on_behalf_of, l) {
        (Address::Speaker, Some(behalf), Locale::Zh) => {
            format!("{}（经 Speaker）", behalf.in_text())
        }
        (Address::Speaker, Some(behalf), Locale::En) => {
            format!("{} (via the Speaker)", behalf.in_text())
        }
        (from, _, _) => from.in_text(),
    };
    let seq = seq_note(l, thread.root);
    if thread.kind == Kind::Note {
        // A work run: the Speaker itself does it, for the asker.
        let _ = write!(
            markdown,
            "**{}** {asker} → {}{seq}",
            l.pick("代码任务", "Coding task"),
            Address::Speaker.in_text()
        );
    } else {
        let what = if thread.kind == Kind::Request {
            l.pick("请求", "Request")
        } else {
            l.pick("提问", "Question")
        };
        let _ = write!(
            markdown,
            "**{what}** {asker} → {}{seq}",
            mentions(l, &thread.to)
        );
    }
    let _ = write!(
        markdown,
        "\n**{}** {}",
        l.pick("状态", "State"),
        state_label(l, thread.state)
    );
    if let Some(verdict) = verdict.or(thread.triage) {
        let _ = write!(
            markdown,
            "\n**{}** {}",
            l.pick("分流", "Triage"),
            verdict_label(l, verdict)
        );
    }
    if let Some(who) = &thread.takeover {
        let _ = write!(
            markdown,
            "\n**{}** {}",
            l.pick("接手", "Handled by"),
            person(who)
        );
    } else if let Some(who) = &thread.escalated_to
        && thread.state == StatusState::Suspended
    {
        let _ = write!(
            markdown,
            "\n**{}** {}",
            l.pick("等", "Waiting on"),
            person(who)
        );
    }
    if let Some(reason) = &thread.reason {
        let _ = write!(
            markdown,
            "\n**{}** {}",
            l.pick("原因", "Reason"),
            one_line(reason)
        );
    }
    if let Some(answer) = answer {
        let _ = write!(
            markdown,
            "\n**{}** {}",
            l.pick("最新回答", "Latest answer"),
            cut(&one_line(answer), CARD_ANSWER_CHARS)
        );
    }
    Card {
        title: thread.title.clone(),
        state: thread.state,
        markdown,
        steps: thread.steps.clone(),
        links: thread.links.clone(),
    }
}

/// A status as a thread reply (output level "everything"):
/// `[senator::claude] 处理中 · ✓ 已读日志 · ☐ 起草回答`.
pub(crate) fn status_reply(l: Locale, event: &Event, status: &StatusPayload) -> String {
    let mut line = format!("{} {}", author(l, event), state_label(l, status.state));
    if let Some(title) = &status.title {
        let _ = write!(line, " · {}", one_line(title));
    }
    for step in status.steps.iter().flatten() {
        let mark = if step.done { "✓" } else { "☐" };
        let _ = write!(line, " · {mark} {}", one_line(&step.label));
    }
    if let Some(reason) = &status.reason {
        let _ = write!(line, " · {}", one_line(reason));
    }
    line
}

/// The pinned "waiting on people" card: every suspended thread, oldest
/// first, at most [`WAITING_LINES`] lines.
pub(crate) fn waiting_card(l: Locale, threads: &[&ThreadSummary]) -> Card {
    let mut markdown = String::new();
    if threads.is_empty() {
        markdown.push_str(l.pick("没有等人处理的话题。", "Nothing is waiting on a person."));
    }
    for thread in threads.iter().take(WAITING_LINES) {
        let who = thread
            .takeover
            .as_ref()
            .or(thread.escalated_to.as_ref())
            .map(|who| match l {
                Locale::Zh => format!(" · 等 {}", person(who)),
                Locale::En => format!(" · waiting on {}", person(who)),
            })
            .unwrap_or_default();
        let reason = thread
            .reason
            .as_deref()
            .map(|r| {
                let r = cut(&one_line(r), PINNED_TITLE_CHARS);
                match l {
                    Locale::Zh => format!("（{r}）"),
                    Locale::En => format!(" ({r})"),
                }
            })
            .unwrap_or_default();
        let _ = writeln!(
            markdown,
            "- seq {} · {}{who}{reason}",
            thread.root,
            cut(&one_line(&thread.title), PINNED_TITLE_CHARS)
        );
    }
    if threads.len() > WAITING_LINES {
        markdown.push_str(&more(l, threads.len() - WAITING_LINES));
    }
    Card {
        title: l.pick("等人处理", "Waiting on people").to_owned(),
        state: StatusState::Suspended,
        markdown: markdown.trim_end().to_owned(),
        steps: Vec::new(),
        links: Vec::new(),
    }
}

/// What the pinned "today" card lists.
pub(crate) struct Today<'a> {
    pub(crate) date: chrono::NaiveDate,
    pub(crate) decisions: Vec<&'a senate_core::brief::BriefDecision>,
    pub(crate) questions: Vec<&'a senate_core::brief::BriefQuestion>,
    pub(crate) threads: Vec<&'a ThreadSummary>,
    /// `(agent, seq, what)`, one per agent, most recent first.
    pub(crate) activity: Vec<(SenatorName, Seq, String)>,
    /// Every seat on the committee, here and in child Senates, as the
    /// Speaker's prompt lists them.
    pub(crate) seats: &'a [crate::brain::CommitteeSeat],
}

/// The pinned "today" card.
pub(crate) fn today_card(l: Locale, today: &Today<'_>) -> Card {
    let mut markdown = String::new();
    let colon = l.pick("：", ": ");
    let none = l.pick("无。", "None.");
    let mut section = |title: &str, lines: Vec<String>, none: &str| {
        let _ = writeln!(markdown, "**{title}**");
        if lines.is_empty() {
            let _ = writeln!(markdown, "{none}");
        }
        for line in lines.iter().take(TODAY_ITEMS) {
            let _ = writeln!(markdown, "- {line}");
        }
        if lines.len() > TODAY_ITEMS {
            let _ = writeln!(markdown, "{}", more(l, lines.len() - TODAY_ITEMS));
        }
        markdown.push('\n');
    };
    section(
        l.pick("今天的决策", "Today's decisions"),
        today
            .decisions
            .iter()
            .map(|d| {
                format!(
                    "seq {}{colon}{}",
                    d.seq,
                    cut(&one_line(&d.body), BRIEF_ITEM_CHARS)
                )
            })
            .collect(),
        none,
    );
    section(
        l.pick("待回答的问题", "Open questions"),
        today
            .questions
            .iter()
            .map(|q| {
                format!(
                    "seq {} · {}{colon}{}",
                    q.seq,
                    q.from.in_text(),
                    cut(&one_line(&q.body), PINNED_TITLE_CHARS)
                )
            })
            .collect(),
        none,
    );
    section(
        l.pick("今天的任务", "Today's tasks"),
        today
            .threads
            .iter()
            .map(|t| {
                format!(
                    "seq {} · {} · {}",
                    t.root,
                    cut(&one_line(&t.title), PINNED_TITLE_CHARS),
                    state_label(l, t.state)
                )
            })
            .collect(),
        none,
    );
    section(
        l.pick("各 agent 最近在做", "What each agent did last"),
        today
            .activity
            .iter()
            .map(|(agent, seq, what)| format!("{} · seq {seq} · {what}", senator(agent)))
            .collect(),
        l.pick("今天还没有动静。", "Nothing yet today."),
    );
    if !today.seats.is_empty() {
        section(
            l.pick("各 agent 在场", "Agent presence"),
            today.seats.iter().map(|seat| seat_line(l, seat)).collect(),
            "",
        );
    }
    let date = today.date.format("%m-%d");
    Card {
        title: match l {
            Locale::Zh => format!("今天的摘要 · {date}"),
            Locale::En => format!("Today · {date}"),
        },
        state: StatusState::Active,
        markdown: markdown.trim_end().to_owned(),
        steps: Vec::new(),
        links: Vec::new(),
    }
}

/// One agent event as a few words for the "today" card.
pub(crate) fn activity(l: Locale, event: &Event) -> String {
    let what = match event.kind {
        Kind::Answer => l.pick("回答", "answer"),
        Kind::Question => l.pick("提问", "question"),
        Kind::Request => l.pick("请求", "request"),
        Kind::Status => l.pick("更新状态", "status"),
        Kind::Note => l.pick("留言", "note"),
        Kind::Checkpoint => l.pick("检查点", "checkpoint"),
        _ => l.pick("动态", "activity"),
    };
    let body = cut(&one_line(&event.body), 24);
    if body.is_empty() {
        what.to_owned()
    } else {
        format!("{what}{}{body}", l.pick("：", ": "))
    }
}

/// The desk's note in the asker's thread: where the message was forwarded.
pub(crate) fn crossing_forwarded(l: Locale, to: &senate_core::Locator) -> String {
    let to = to.in_text();
    match l {
        Locale::Zh => format!("Speaker 的办公室已把这条消息转给 {to}；回答会回到这个 thread。"),
        Locale::En => format!(
            "The Speaker's office forwarded this to {to}; the answer will come back to this thread."
        ),
    }
}

/// The desk's answer in the asker's thread: it will not go there, and why.
pub(crate) fn crossing_refused(l: Locale, to: &senate_core::Locator, reason: &str) -> String {
    let to = to.in_text();
    let reason = reason.trim();
    match l {
        Locale::Zh => format!("Speaker 的办公室没有把这条消息转给 {to}：{reason}"),
        Locale::En => format!("The Speaker's office did not forward this to {to}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seat as the engine lists it: relay target, the name people are
    /// shown, presence and duty.
    fn seat(
        target: &str,
        shown: Option<&str>,
        online: bool,
        duty: Option<&str>,
    ) -> crate::brain::CommitteeSeat {
        crate::brain::CommitteeSeat {
            target: target.into(),
            shown: shown.map(Into::into),
            duty: duty.map(Into::into),
            online,
            presence: None,
        }
    }

    /// Thread 773: `/status` and the today card in a group bound to
    /// `commons-demo` list both child seats online by alias with their
    /// duties, and nothing of the built-in session (the engine leaves it
    /// out of the seats).
    #[test]
    fn status_and_the_today_card_list_every_seat_by_the_name_people_see() {
        let seats = [
            seat(
                "dark-conclave::claude-11a084c4",
                Some("senate-dev（dark-conclave）"),
                true,
                Some("Senate 开发"),
            ),
            seat(
                "wyak::codex-01a1fc99",
                Some("commons-dev（wyak）"),
                true,
                Some("commons 的开发"),
            ),
            seat("dark-conclave::archivist", None, false, None),
        ];
        let c = CommitteeId::new("commons-demo").unwrap();
        let text = status(Locale::Zh, &c, &seats, &[]);
        assert_eq!(
            text,
            "本群绑定在委员会 commons-demo。\n\
             在线 agent：senate-dev（dark-conclave）、commons-dev（wyak）\n\
             离线 agent：[dark-conclave::archivist]\n\
             席位：\n\
             - senate-dev（dark-conclave） 在线 · 职责：Senate 开发\n\
             - commons-dev（wyak） 在线 · 职责：commons 的开发\n\
             - [dark-conclave::archivist] 离线"
        );
        assert!(!text.contains("secretary"), "{text}");
        assert!(!text.contains("claude-11a084c4"), "no primary id: {text}");

        let today = Today {
            date: chrono::NaiveDate::from_ymd_opt(2026, 11, 21).unwrap(),
            decisions: Vec::new(),
            questions: Vec::new(),
            threads: Vec::new(),
            activity: Vec::new(),
            seats: &seats,
        };
        let card = today_card(Locale::Zh, &today);
        assert!(
            card.markdown.ends_with(
                "**各 agent 在场**\n\
                 - senate-dev（dark-conclave） 在线 · 职责：Senate 开发\n\
                 - commons-dev（wyak） 在线 · 职责：commons 的开发\n\
                 - [dark-conclave::archivist] 离线"
            ),
            "{}",
            card.markdown
        );
        let card = today_card(Locale::En, &today);
        assert!(
            card.markdown
                .contains("- senate-dev（dark-conclave） online · duty: Senate 开发"),
            "{}",
            card.markdown
        );
    }

    /// No aliases known: every seat is shown by its id.
    fn no_names() -> SeatNames<'static> {
        static NONE: std::sync::LazyLock<senate_core::alias::CommitteeAliases> =
            std::sync::LazyLock::new(Default::default);
        SeatNames {
            aliases: &NONE,
            children: &[],
        }
    }

    #[test]
    fn a_child_seat_is_shown_by_its_alias_and_child() {
        let alias: SenatorName = "senate-dev".parse().unwrap();
        let child = Name::new("dark-conclave").unwrap();
        assert_eq!(
            child_seat_name(Locale::Zh, &alias, &child),
            "senate-dev（dark-conclave）"
        );
        assert_eq!(
            child_seat_name(Locale::En, &alias, &child),
            "senate-dev (dark-conclave)"
        );
    }
    use chrono::Utc;
    use senate_core::brief::{BriefDecision, BriefQuestion};
    use senate_core::{EventId, Principal, SeatedAgent, Step, TaskOwner, meta};
    use serde_json::{Map, Value};

    use crate::tests_support::assert_senate_vocabulary;

    fn event(kind: Kind, from: Address, to: Vec<Address>, thread: Option<Seq>) -> Event {
        Event {
            seq: Seq(118),
            committee: CommitteeId::new("refund-v2").expect("committee"),
            id: EventId::new(),
            ts: Utc::now(),
            kind,
            from,
            to,
            thread,
            on_behalf_of: None,
            source: None,
            body: "网关的幂等键用哪个 header？".into(),
            refs: None,
            mirror: None,
            meta: Map::new(),
        }
    }

    fn path(s: &str) -> SenatorName {
        s.parse().expect("senator path")
    }

    fn name(s: &str) -> Name {
        Name::new(s).expect("name")
    }

    /// `senator::<s>`: `alice` is a person, `claude` an agent.
    fn sen(s: &str) -> Address {
        Address::senator(&path(s))
    }

    fn committee() -> CommitteeId {
        CommitteeId::new("refund-v2").expect("committee")
    }

    #[test]
    fn labels_follow_the_mirroring_table() {
        let l = Locale::Zh;
        let question = event(Kind::Question, sen("claude"), vec![sen("copilot")], None);
        assert_eq!(
            root_mirror(l, &question),
            "[senator::claude] → [senator::copilot] 网关的幂等键用哪个 header？（seq 118）"
        );
        let mut answer = event(
            Kind::Answer,
            sen("copilot"),
            vec![sen("claude")],
            Some(Seq(118)),
        );
        answer.body = "Idempotency-Key".into();
        assert_eq!(
            thread_mirror(l, &answer),
            "[senator::copilot] Idempotency-Key"
        );
        answer.from = Address::Speaker;
        answer.on_behalf_of = Some(sen("copilot"));
        assert_eq!(
            thread_mirror(l, &answer),
            "[speaker::copilot] Idempotency-Key"
        );
        assert_eq!(
            relay_ack(l, &[sen("claude")], Seq(120)),
            "已转给 [senator::claude]（seq 120）"
        );
    }

    #[test]
    fn mirror_lines_and_acks_in_english() {
        let l = Locale::En;
        let mut question = event(
            Kind::Question,
            sen("claude"),
            vec![sen("copilot"), sen("codex")],
            None,
        );
        question.body = "Which header carries the idempotency key?".into();
        assert_eq!(
            root_mirror(l, &question),
            "[senator::claude] → [senator::copilot], [senator::codex] Which header carries the idempotency key? (seq 118)"
        );
        let mut answer = event(
            Kind::Answer,
            Address::Speaker,
            vec![sen("claude")],
            Some(Seq(118)),
        );
        answer.on_behalf_of = Some(sen("copilot"));
        answer.body = "Idempotency-Key".into();
        assert_eq!(
            thread_mirror(l, &answer),
            "[speaker::copilot] Idempotency-Key"
        );
        assert_eq!(
            relay_ack(l, &[sen("claude"), sen("bob")], Seq(120)),
            "Passed to [senator::claude], [senator::bob] (seq 120)"
        );
        assert_eq!(decision_ack(l, Seq(131)), "Decision recorded (seq 131)");
        assert_eq!(decision_ack(Locale::Zh, Seq(131)), "已记录决策（seq 131）");
    }

    #[test]
    fn the_speaker_for_a_guest_or_an_unknown_form_still_has_a_label() {
        let guest: Address = "guest::chat/u_8f3a".parse().expect("guest");
        let mut relayed = event(Kind::Question, Address::Speaker, vec![sen("claude")], None);
        relayed.on_behalf_of = Some(guest);
        relayed.body = "When does the refund land?".into();
        assert_eq!(
            root_mirror(Locale::En, &relayed),
            "[speaker] [guest::chat/u_8f3a] → [senator::claude] When does the refund land? (seq 118)"
        );
        let mut other = event(
            Kind::Answer,
            Address::Other("role:reviewer".into()),
            vec![],
            Some(Seq(1)),
        );
        other.body = "ok".into();
        assert_eq!(thread_mirror(Locale::Zh, &other), "[role:reviewer] ok");
    }

    #[test]
    fn takeover_and_companion_answers_say_so() {
        let mut answer = event(
            Kind::Answer,
            Address::Speaker,
            vec![sen("copilot")],
            Some(Seq(118)),
        );
        answer.on_behalf_of = Some(sen("claude"));
        answer.body = "用 Idempotency-Key（检查点 seq 7）".into();
        answer.meta.insert(meta::TAKEOVER.into(), Value::Bool(true));
        assert!(is_takeover(&answer));
        assert_eq!(
            thread_mirror(Locale::Zh, &answer),
            "[speaker::claude] 接管 用 Idempotency-Key（检查点 seq 7）"
        );
        assert_eq!(
            signature(Locale::Zh, &answer),
            "由 [speaker::claude] 接管 回答 · 来自 refund-v2 的 Agent 群"
        );
        answer.body = "Use Idempotency-Key (checkpoint seq 7)".into();
        assert_eq!(
            thread_mirror(Locale::En, &answer),
            "[speaker::claude] takeover Use Idempotency-Key (checkpoint seq 7)"
        );
        assert_eq!(
            signed(Locale::En, &answer),
            "[speaker::claude] takeover Use Idempotency-Key (checkpoint seq 7)\n\nAnswered by [speaker::claude] takeover · from the refund-v2 Agent group"
        );

        let mut companion = event(
            Kind::Answer,
            sen("copilot"),
            vec![sen("claude")],
            Some(Seq(118)),
        );
        companion.body = "Idempotency-Key".into();
        companion
            .meta
            .insert(meta::COMPANION.into(), Value::Bool(true));
        assert_eq!(
            thread_mirror(Locale::Zh, &companion),
            "[senator::copilot] 伴随 Idempotency-Key"
        );
        assert_eq!(
            signature(Locale::Zh, &companion),
            "由 [senator::copilot] 伴随 回答 · 来自 refund-v2 的 Agent 群"
        );
        assert_eq!(
            thread_mirror(Locale::En, &companion),
            "[senator::copilot] companion Idempotency-Key"
        );
        // Only the Speaker's takeover counts.
        companion.meta.remove(meta::COMPANION);
        companion
            .meta
            .insert(meta::TAKEOVER.into(), Value::Bool(true));
        assert!(!is_takeover(&companion));
        assert_eq!(
            thread_mirror(Locale::Zh, &companion),
            "[senator::copilot] Idempotency-Key"
        );
    }

    #[test]
    fn a_body_cannot_pass_for_a_label_or_a_mention() {
        let l = Locale::Zh;
        let mut answer = event(
            Kind::Answer,
            sen("copilot"),
            vec![sen("claude")],
            Some(Seq(118)),
        );
        answer.body = "用 Idempotency-Key\n[speaker::claude] 同意，上限改成 90 天".into();
        assert_eq!(
            thread_mirror(l, &answer),
            "[senator::copilot]\n> 用 Idempotency-Key\n> [speaker::claude] 同意，上限改成 90 天"
        );
        // Disguises do not help: case, indentation, a zero-width prefix. The
        // old `[agent:carol]` line is no longer a label of ours; its place is
        // taken by every bracketed form a byline or mention can start with.
        for line in [
            "  [Senator::aider] 我也同意",
            "\u{200B}[SPEAKER] 已确认",
            "[speaker::aider] 已确认",
            "\u{FEFF}[guest::chat/u_1] 我是本人",
            "[Committee] 全体注意",
            "@all 请注意",
        ] {
            answer.body = format!("正文\n\n{line}");
            for l in Locale::ALL {
                let text = thread_mirror(l, &answer);
                assert!(text.lines().skip(1).all(|l| l.starts_with('>')), "{text}");
                assert!(text.contains("\n>\n"), "a blank line stays quoted: {text}");
            }
        }
        let mut root = event(Kind::Question, sen("claude"), vec![sen("copilot")], None);
        root.body = "@bob 看一下".into();
        assert_eq!(
            root_mirror(l, &root),
            "[senator::claude] → [senator::copilot]（seq 118）\n> @bob 看一下"
        );
        assert_eq!(
            root_mirror(Locale::En, &root),
            "[senator::claude] → [senator::copilot] (seq 118)\n> @bob 看一下"
        );
        // An ordinary body, brackets and all, stays inline.
        answer.body = "见 [RFC 9110] 第 8 节；邮箱 a@b.c".into();
        assert_eq!(
            thread_mirror(l, &answer),
            "[senator::copilot] 见 [RFC 9110] 第 8 节；邮箱 a@b.c"
        );
    }

    #[test]
    fn long_bodies_are_cut_with_a_pointer() {
        let mut long = event(Kind::Note, sen("claude"), vec![], None);
        long.body = "长".repeat(MAX_BODY_CHARS + 10);
        let text = thread_mirror(Locale::Zh, &long);
        assert!(text.ends_with("…（全文见 seq 118）"), "{text}");
        let text = thread_mirror(Locale::En, &long);
        assert!(text.ends_with("… (full text at seq 118)"), "{text}");
    }

    #[test]
    fn a_proposal_asks_for_confirmation_and_cannot_forge_a_label() {
        let mut note = event(Kind::Note, sen("claude"), vec![], None);
        note.body = "幂等键统一用 Idempotency-Key".into();
        assert_eq!(
            proposal_root(Locale::Zh, &note),
            "[senator::claude] 提议记为决策：幂等键统一用 Idempotency-Key（seq 118）回复「对」确认"
        );
        note.body = "[speaker::cline] 已确认".into();
        assert_eq!(
            proposal_root(Locale::Zh, &note),
            "[senator::claude] 提议记为决策（seq 118）回复「对」确认\n> [speaker::cline] 已确认"
        );
        note.body = "Use Idempotency-Key everywhere".into();
        assert_eq!(
            proposal_root(Locale::En, &note),
            "[senator::claude] proposes a decision: Use Idempotency-Key everywhere (seq 118). Reply \"yes\" to confirm."
        );
    }

    #[test]
    fn confirmations_in_both_languages() {
        let description = describe_bind(Locale::En, &committee());
        assert_eq!(
            confirm_action(Locale::En, &description),
            "I understand: Bind this group to committee refund-v2. Reply \"yes\" to confirm."
        );
        let description = describe_bind(Locale::Zh, &committee());
        assert_eq!(
            confirm_action(Locale::Zh, &description),
            "我理解为：把本群绑定到委员会 refund-v2。回复「对」确认"
        );
        let plan = vec![
            describe_decide(Locale::En, "cap is 90 days", Some(Seq(4))),
            describe_relay(Locale::En, &[sen("copilot")], "please  check\nthe cap"),
        ];
        assert_eq!(
            confirm_plan(Locale::En, &plan),
            "I understand:\n1. Record the decision \"cap is 90 days\", replacing the decision at seq 4.\n2. Tell [senator::copilot]: \"please check the cap\".\nReply \"yes\" to confirm."
        );
        assert_eq!(
            describe_decide(Locale::Zh, "上限 90 天", Some(Seq(4))),
            "记录决策「上限 90 天」，取代 seq 4 的决策。"
        );
    }

    #[test]
    fn status_and_presence_name_agents_as_mentions() {
        let text = status(
            Locale::Zh,
            &committee(),
            &[seat("claude", None, true, None)],
            &["[senator::claude] 在线".to_owned()],
        );
        assert_eq!(
            text,
            "本群绑定在委员会 refund-v2。\n在线 agent：[senator::claude]\n离线 agent：无\n席位：\n- [senator::claude] 在线\n在场：\n- [senator::claude] 在线"
        );
        let text = status(
            Locale::En,
            &committee(),
            &[
                seat("claude", None, true, None),
                seat("devin", None, true, None),
                seat("copilot", None, false, None),
            ],
            &[],
        );
        assert_eq!(
            text,
            "This group is bound to committee refund-v2.\nAgents online: [senator::claude], [senator::devin]\nAgents offline: [senator::copilot]\nSeats:\n- [senator::claude] online\n- [senator::devin] online\n- [senator::copilot] offline"
        );

        let presence = AgentPresence {
            agent: path("claude"),
            state: PresenceState::Absent,
            since: Utc::now(),
            since_seq: Seq(10),
            seq: Some(Seq(11)),
            last_seen: None,
            cursor: None,
            lag: Some(14),
            new_owner: None,
            duty: None,
        };
        let since = chrono::Duration::hours(3);
        assert_eq!(
            presence_status(
                Locale::Zh,
                &path("claude"),
                Some(&presence),
                Some((Seq(40), since))
            ),
            "[senator::claude] 缺席（租约过期） · 落后 14 条事件 · 检查点 3 小时前（seq 40）"
        );
        assert_eq!(
            presence_status(
                Locale::En,
                &path("claude"),
                Some(&presence),
                Some((Seq(40), since))
            ),
            "[senator::claude] absent (lease expired) · 14 events behind · checkpoint 3 hours ago (seq 40)"
        );
        assert_eq!(
            presence_status(Locale::En, &path("copilot"), None, None),
            "[senator::copilot] presence unknown · no checkpoint"
        );
        let with_duty = AgentPresence {
            duty: Some("reviews the server".into()),
            ..presence.clone()
        };
        assert_eq!(
            presence_status(Locale::En, &path("claude"), Some(&with_duty), None),
            "[senator::claude] absent (lease expired) · 14 events behind · duty: reviews the server · no checkpoint"
        );
        assert_eq!(
            presence_status(Locale::Zh, &path("claude"), Some(&with_duty), None),
            "[senator::claude] 缺席（租约过期） · 落后 14 条事件 · 职责：reviews the server · 没有检查点"
        );
        assert_eq!(age(Locale::En, chrono::Duration::minutes(1)), "1 minute");
        assert_eq!(age(Locale::Zh, chrono::Duration::days(3)), "3 天");

        let reassigned = PresencePayload {
            agent: path("claude"),
            state: PresenceState::Reassigned,
            since: Utc::now(),
            since_seq: Seq(10),
            previous: Some(PresenceState::Absent),
            last_seen: None,
            cursor: None,
            lag: None,
            new_owner: Some(path("copilot")),
        };
        assert_eq!(
            presence_line(Locale::Zh, &reassigned),
            "[senator::claude] 的工作已交给 [senator::copilot]"
        );
        let back = PresencePayload {
            state: PresenceState::Online,
            previous: Some(PresenceState::Reassigned),
            new_owner: None,
            ..reassigned.clone()
        };
        assert_eq!(
            presence_line(Locale::En, &back),
            "[senator::claude]'s work is back with [senator::claude]"
        );
        let lost = PresencePayload {
            state: PresenceState::Disconnected,
            previous: Some(PresenceState::Online),
            new_owner: None,
            ..reassigned
        };
        assert_eq!(presence_line(Locale::Zh, &lost), "[senator::claude] 失联");
        assert_eq!(
            presence_line(Locale::En, &lost),
            "[senator::claude] disconnected"
        );
    }

    #[test]
    fn takeover_escalation_and_hand_back() {
        let agent = sen("claude");
        assert_eq!(
            takeover_escalation(Locale::Zh, &agent, NoCover::Stale, "上限\n是多少？"),
            "[senator::claude] 不在，检查点太旧，这条需要人：上限 是多少？"
        );
        assert_eq!(
            takeover_escalation(Locale::En, &agent, NoCover::Missing, "What is the cap?"),
            "[senator::claude] is away and it left no checkpoint, so this needs a person: What is the cap?"
        );
        let answers = [HandedBack {
            question_seq: Seq(12),
            answer_seq: Seq(13),
            question: "What is the cap?".into(),
            answer: "90 days".into(),
        }];
        assert_eq!(
            handback_note(Locale::En, Seq(10), &answers),
            "While you were away the Speaker took over your inbox (from seq 10) and answered 1 question for you:\n- seq 12 · Q: What is the cap? → A (seq 13): 90 days\nIf anything is wrong, correct it in its thread."
        );
        assert!(
            handback_note(Locale::Zh, Seq(10), &answers)
                .starts_with("你不在时 Speaker 接管了你的信箱（seq 10 起），代你回答了 1 条：")
        );
        assert_eq!(
            handback_line(Locale::Zh, &agent, 2),
            "[senator::claude] 回来了，Speaker 交还 2 条代答"
        );
        assert_eq!(
            handback_line(Locale::En, &agent, 2),
            "[senator::claude] is back; the Speaker handed back 2 answers"
        );
    }

    #[test]
    fn unknown_targets_list_people_and_how_to_name_an_agent() {
        let mut alice = Principal::new(name("alice"));
        alice.names = vec!["Alice".into(), "爱丽丝".into()];
        let roster = Roster::new(vec![alice, Principal::new(name("bob"))], vec![]).expect("roster");
        assert_eq!(
            unknown_targets(Locale::Zh, &["carol".into()], &roster),
            "我不认识 carol。名册上的人：alice（Alice）、bob。要指明某个 agent，直接写它的名字。"
        );
        assert_eq!(
            unknown_targets(Locale::En, &[], &roster),
            "Whom should I pass it to? People on the roster: alice (Alice), bob. To name one agent, write its own name."
        );
        let empty = Roster::new(vec![], vec![]).expect("roster");
        assert!(unknown_targets(Locale::En, &[], &empty).contains("no one is on the roster"));
    }

    #[test]
    fn a_parked_relay_names_the_person() {
        let mut relay = event(Kind::Question, Address::Speaker, vec![sen("claude")], None);
        relay.seq = Seq(120);
        relay.meta.insert(
            meta::TRIAGE.into(),
            TriagePayload {
                verdict: TriageVerdict::NeedsHuman,
                p_needs_human: 0.9,
                p_agent: 0.05,
                p_unsure: 0.05,
                rule: None,
                reason: None,
            }
            .value(),
        );
        let roster = Roster::new(
            vec![Principal::new(name("alice"))],
            vec![SeatedAgent::new(path("claude"), name("alice"))],
        )
        .expect("roster");
        assert_eq!(
            relay_ack_for(Locale::Zh, &relay, &roster, no_names()),
            "已转给 [senator::claude]（seq 120）。Speaker 判断这条需要 [senator::alice] 本人处理，已提醒 [senator::alice]"
        );
        assert_eq!(
            relay_ack_for(Locale::En, &relay, &roster, no_names()),
            "Passed to [senator::claude] (seq 120). The Speaker judged that this needs [senator::alice] in person and has let [senator::alice] know."
        );
    }

    #[test]
    fn the_brief_reply_is_bounded_and_keeps_the_newest() {
        let events: Vec<Event> = (1..=40)
            .map(|n| {
                let mut e = event(Kind::Decision, sen("pm"), vec![], None);
                e.seq = Seq(n);
                e.body = format!("决策 {n} {}", "很长".repeat(60));
                e
            })
            .collect();
        let brief = Brief::from_events(committee(), &events);
        for l in Locale::ALL {
            let text = super::brief(l, &brief);
            assert!(
                text.chars().count() <= MAX_BRIEF_CHARS + 1,
                "{}",
                text.chars().count()
            );
            assert!(text.contains("决策 40 "), "{text}");
            assert!(!text.contains("决策 11 "), "the oldest go first: {text}");
            assert!(text.contains("[senator::pm]"), "{text}");
        }
        let zh = super::brief(Locale::Zh, &brief);
        assert!(zh.contains("条没有列出"), "{zh}");
        assert!(zh.contains("待回答的问题：无。"), "{zh}");
        let en = super::brief(Locale::En, &brief);
        assert!(
            en.starts_with("Brief for committee refund-v2 (as of seq 40)"),
            "{en}"
        );
        assert!(en.contains("earlier not shown"), "{en}");
        assert!(en.contains("Open questions: none."), "{en}");

        let empty = Brief::from_events(committee(), &[]);
        let zh = super::brief(Locale::Zh, &empty);
        assert!(zh.contains("已定决策：还没有记录。"), "{zh}");
        let en = super::brief(Locale::En, &empty);
        assert!(
            en.contains("Decisions in force: none recorded yet."),
            "{en}"
        );
    }

    #[test]
    fn the_top_is_at_most_three_short_lines() {
        assert_eq!(top_lines("  a \n\n b\nc\nd"), "a\nb\nc");
        assert_eq!(top_lines(" \n "), "");
        let long = "长".repeat(TOP_LINE_CHARS + 5);
        assert_eq!(top_lines(&long).chars().count(), TOP_LINE_CHARS + 1);
    }

    fn thread() -> ThreadSummary {
        ThreadSummary {
            root: Seq(118),
            kind: Kind::Question,
            from: Address::Speaker,
            on_behalf_of: Some(sen("bob")),
            to: vec![sen("claude")],
            agents: vec![path("claude")],
            title: "Which header?".into(),
            state: StatusState::Suspended,
            since: Seq(118),
            since_ts: Utc::now(),
            updated: Seq(119),
            steps: vec![Step {
                id: "1".into(),
                label: "read the log".into(),
                done: true,
            }],
            links: Vec::new(),
            owner: Some(TaskOwner {
                agent: path("claude"),
                session: "s1".into(),
            }),
            triage: Some(TriageVerdict::NeedsHuman),
            escalated_to: Some(name("alice")),
            takeover: None,
            reason: Some("needs a product call".into()),
            answered: false,
            answered_by: None,
            reopened: false,
            escalation: None,
            directed: Vec::new(),
        }
    }

    #[test]
    fn cards_name_people_and_agents_as_mentions() {
        let t = thread();
        let card = task_card(Locale::Zh, &t, None, Some("用 Idempotency-Key"));
        assert_eq!(
            card.markdown,
            "**提问** [senator::bob]（经 Speaker） → [senator::claude]（seq 118）\n**状态** 等人处理\n**分流** 需要本人\n**等** [senator::alice]\n**原因** needs a product call\n**最新回答** 用 Idempotency-Key"
        );
        let card = task_card(Locale::En, &t, Some(TriageVerdict::Agent), None);
        assert_eq!(
            card.markdown,
            "**Question** [senator::bob] (via the Speaker) → [senator::claude] (seq 118)\n**State** waiting on a person\n**Triage** for the agent\n**Waiting on** [senator::alice]\n**Reason** needs a product call"
        );
        assert_eq!(card.title, "Which header?");

        let waiting = waiting_card(Locale::En, &[&t]);
        assert_eq!(waiting.title, "Waiting on people");
        assert_eq!(
            waiting.markdown,
            "- seq 118 · Which header? · waiting on [senator::alice] (needs a product call)"
        );
        assert_eq!(
            waiting_card(Locale::Zh, &[&t]).markdown,
            "- seq 118 · Which header? · 等 [senator::alice]（needs a product call）"
        );
        assert_eq!(
            waiting_card(Locale::Zh, &[]).markdown,
            "没有等人处理的话题。"
        );

        let decision = BriefDecision {
            seq: Seq(5),
            ts: Utc::now(),
            by: sen("pm"),
            body: "cap is 90 days".into(),
        };
        let question = BriefQuestion {
            seq: Seq(6),
            ts: Utc::now(),
            from: sen("copilot"),
            to: vec![sen("claude")],
            body: "Which header?".into(),
        };
        let today = Today {
            date: chrono::NaiveDate::from_ymd_opt(2026, 4, 12).expect("date"),
            decisions: vec![&decision],
            questions: vec![&question],
            threads: vec![&t],
            activity: vec![(path("copilot"), Seq(7), "answer: 90 days".into())],
            seats: &[seat("claude", None, true, Some("reviews"))],
        };
        let card = today_card(Locale::En, &today);
        assert_eq!(card.title, "Today · 04-12");
        assert_eq!(
            card.markdown,
            "**Today's decisions**\n- seq 5: cap is 90 days\n\n**Open questions**\n- seq 6 · [senator::copilot]: Which header?\n\n**Today's tasks**\n- seq 118 · Which header? · waiting on a person\n\n**What each agent did last**\n- [senator::copilot] · seq 7 · answer: 90 days\n\n**Agent presence**\n- [senator::claude] online · duty: reviews"
        );
        let card = today_card(Locale::Zh, &today);
        assert_eq!(card.title, "今天的摘要 · 04-12");
        assert!(
            card.markdown
                .contains("- [senator::claude] 在线 · 职责：reviews"),
            "{}",
            card.markdown
        );
    }

    #[test]
    fn a_status_reply_is_headed_by_the_byline() {
        let mut e = event(Kind::Status, sen("claude"), vec![], Some(Seq(118)));
        e.body = String::new();
        let mut status = StatusPayload::new(StatusState::Processing);
        status.steps = Some(vec![
            Step {
                id: "1".into(),
                label: "已读日志".into(),
                done: true,
            },
            Step {
                id: "2".into(),
                label: "起草回答".into(),
                done: false,
            },
        ]);
        assert_eq!(
            status_reply(Locale::Zh, &e, &status),
            "[senator::claude] 处理中 · ✓ 已读日志 · ☐ 起草回答"
        );
        assert_eq!(
            status_reply(Locale::En, &e, &status),
            "[senator::claude] in progress · ✓ 已读日志 · ☐ 起草回答"
        );
    }

    #[test]
    fn no_text_uses_the_old_vocabulary() {
        let c = committee();
        let agent = sen("claude");
        let mut answer = event(
            Kind::Answer,
            Address::Speaker,
            vec![sen("copilot")],
            Some(Seq(118)),
        );
        answer.on_behalf_of = Some(agent.clone());
        answer.meta.insert(meta::TAKEOVER.into(), Value::Bool(true));
        let question = event(Kind::Question, agent.clone(), vec![sen("copilot")], None);
        let roster = Roster::new(vec![Principal::new(name("alice"))], vec![]).expect("roster");
        let t = thread();
        let triage = TriagePayload {
            verdict: TriageVerdict::Unsure,
            p_needs_human: 0.3,
            p_agent: 0.3,
            p_unsure: 0.4,
            rule: None,
            reason: Some("unclear".into()),
        };
        let brief = Brief::from_events(c.clone(), std::slice::from_ref(&question));
        for l in Locale::ALL {
            let texts = [
                relay_ack(l, std::slice::from_ref(&agent), Seq(1)),
                root_mirror(l, &question),
                thread_mirror(l, &answer),
                proposal_root(l, &question),
                decision_ack(l, Seq(1)),
                signed(l, &answer),
                confirm_action(l, &describe_bind(l, &c)),
                describe_rebind(l, &c, &c),
                describe_unbind(l, &c),
                describe_adopt(l, &c),
                describe_invite(l, &["alice".into()]),
                describe_output_level(l, OutputLevel::All),
                describe_status(l),
                describe_recall(l),
                step_after_unbind(l),
                on_committee(l, &c, "x"),
                action_completed(l, 0, "x", "y"),
                action_refused(l, 0, "x", 2),
                action_not_allowed(l),
                too_many_actions(l, 5),
                empty_decision(l),
                not_a_decision(l, Seq(3)),
                bound(l, &c),
                unbound(l, &c),
                already_bound(l, &c),
                cancelled(l),
                binding_body(l, true, "chat-room"),
                binding_body(l, false, "chat-room"),
                status(l, &c, &[seat("claude", None, true, None)], &[]),
                takeover_escalation(l, &agent, NoCover::Unavailable, "q"),
                handback_note(l, Seq(1), &[]),
                handback_line(l, &agent, 1),
                unbound_status(l),
                nothing_to_unbind(l),
                relay_needs_binding(l),
                decide_needs_binding(l),
                recall_needs_binding(l),
                invalid_committee(l, "X"),
                unknown_targets(l, &["carol".into()], &roster),
                empty_relay(l),
                guest_refused(l),
                guest_note_refused(l),
                not_understood(l),
                fallback_unavailable(l, &agent),
                fallback_pending(l, &agent),
                super::brief(l, &brief),
                relay_ack_for(l, &question, &roster, no_names()),
                triage_note(l, &triage),
                triage_reason(l, &triage),
                request_rule_reason(l),
                own_agent_reason(l),
                triage_unavailable(l),
                needs_human(l, "r"),
                escalated(l, &agent, Some("r")),
                escalated_ack(l, &name("alice")),
                agents_group_name(l, &c),
                archived_group_name(l, &c),
                takeover_reason(l),
                approved_reason(l, &agent),
                handed_off(l, &agent),
                already_handed_off(l),
                handoff_not_yours(l, &name("alice")),
                no_agents_group_hint(l, &c, "speaker-bot"),
                adopted(l, &c),
                adopt_needs_committee(l),
                adopt_other_committee(l, &c, &c),
                agents_need_binding(l),
                no_agents_group(l, &c),
                invite_whom(l),
                invited(l, &["alice".into()]),
                invite_unsupported(l, &["alice".into()]),
                agents_group_members_missing(l, "g", &["alice".into()]),
                invite_failed(l, &["alice".into()]),
                output_level_note(l, OutputLevel::Answers),
                output_level_set(l, OutputLevel::Status),
                task_card(l, &t, None, Some("a")).markdown,
                waiting_card(l, &[&t]).markdown,
                activity(l, &question),
            ];
            for text in texts {
                assert_senate_vocabulary(&text);
            }
        }
    }
}
