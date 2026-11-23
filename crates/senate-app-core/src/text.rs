//! Every sentence the core writes for a person, in Chinese or English.
//!
//! The split with the Swift side: text that is built from data (counts,
//! stream states, who a thread waits on, why an item awaits the Speaker) is
//! written here, for the [`Locale`] the app passes in, so a plural or a word
//! order is decided once. Text that is a fixed label (a button, a tab) lives
//! in the app's `Localizable.strings`. Senate, senator and Speaker are never
//! translated, and an id is a bracketed mention in both languages.
//!
//! Messages that come from the daemon are shown as the daemon wrote them, in
//! the language of `senate.toml`; they are not translated again here.

use chrono::{DateTime, Utc};
use senate_bridge::ipc::StreamState;
use senate_core::{Kind, Locale, StatusState, TriageVerdict};

/// `Senate: online as alice` / `Senate：已作为 alice 在线`.
pub fn online_title(locale: Locale, person: &str) -> String {
    match locale {
        Locale::Zh => format!("Senate：已作为 {person} 在线"),
        Locale::En => format!("Senate: online as {person}"),
    }
}

pub fn offline_title(locale: Locale) -> String {
    locale.pick("Senate：离线", "Senate: offline").to_string()
}

pub fn offline_tooltip(locale: Locale, reason: Option<&str>) -> String {
    let head = locale.pick("Senate 守护进程离线", "Senate daemon offline");
    match reason {
        Some(r) => format!("{head}{}{r}", locale.pick("：", ": ")),
        None => head.to_string(),
    }
}

pub fn offline_pending(locale: Locale) -> String {
    locale
        .pick(
            "守护进程未运行（senated --machine）",
            "the daemon is not running (senated --machine)",
        )
        .to_string()
}

/// `2 pending threads, 1 approval` / `2 个待处理线程，1 个审批`. The
/// approval part is left out when there is none.
pub fn pending(locale: Locale, threads: usize, approvals: usize) -> String {
    match locale {
        Locale::Zh => {
            let mut s = format!("{threads} 个待处理线程");
            if approvals > 0 {
                s.push_str(&format!("，{approvals} 个审批"));
            }
            s
        }
        Locale::En => {
            let mut s = format!("{threads} pending {}", plural(threads, "thread", "threads"));
            if approvals > 0 {
                s.push_str(&format!(
                    ", {approvals} {}",
                    plural(approvals, "approval", "approvals")
                ));
            }
            s
        }
    }
}

/// The menu line for a presiding agent's docket.
pub fn docket_line(locale: Locale, items: usize) -> String {
    match locale {
        Locale::Zh => format!("议程：{items} 项等待 Speaker"),
        Locale::En => format!("Docket: {items} awaiting the Speaker"),
    }
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// One Senate's name: the top Senate, or the one a presiding agent holds.
pub fn senate_title(locale: Locale, senate: Option<&str>) -> String {
    match (senate, locale) {
        (None, _) => "Senate".to_string(),
        (Some(p), _) => format!("Senate {p}"),
    }
}

/// One stream's state, short, for a menu line.
pub fn stream(locale: Locale, state: Option<&StreamState>, now: DateTime<Utc>) -> String {
    match (state, locale) {
        (None, l) => l.pick("无连接", "no stream").to_string(),
        (Some(StreamState::Connecting { attempt: 0 }), l) => l.pick("连接中", "connecting").into(),
        (Some(StreamState::Connecting { attempt }), Locale::Zh) => {
            format!("连接中（第 {attempt} 次）")
        }
        (Some(StreamState::Connecting { attempt }), Locale::En) => {
            format!("connecting (attempt {attempt})")
        }
        (Some(StreamState::Connected { .. }), l) => l.pick("在线", "live").to_string(),
        (
            Some(StreamState::Backoff {
                attempt, retry_at, ..
            }),
            l,
        ) => {
            let wait = (*retry_at - now).num_seconds().max(0);
            match l {
                Locale::Zh => format!("落后，{wait} 秒后重试（第 {attempt} 次）"),
                Locale::En => format!("behind, retry in {wait}s (attempt {attempt})"),
            }
        }
        (Some(StreamState::Stopped), l) => l.pick("已停止", "stopped").to_string(),
    }
}

pub fn state(locale: Locale, state: StatusState) -> &'static str {
    match state {
        StatusState::Processing => locale.pick("处理中", "processing"),
        StatusState::Active => locale.pick("进行中", "active"),
        StatusState::Suspended => locale.pick("等待中", "suspended"),
        StatusState::Archived => locale.pick("已归档", "archived"),
    }
}

pub fn kind(locale: Locale, kind: &Kind) -> String {
    match kind {
        Kind::Question => locale.pick("问题", "question").to_string(),
        Kind::Request => locale.pick("请求", "request").to_string(),
        Kind::Note => locale.pick("笔记", "note").to_string(),
        Kind::Answer => locale.pick("回答", "answer").to_string(),
        other => other.as_str().to_string(),
    }
}

/// An escalation to the person, as the kind of a thing that came back.
pub fn escalated_word(locale: Locale) -> &'static str {
    locale.pick("转给你", "escalated to you")
}

pub fn triage(locale: Locale, verdict: TriageVerdict) -> &'static str {
    match verdict {
        TriageVerdict::NeedsHuman => locale.pick("需要人决定", "needs a person"),
        TriageVerdict::Agent => locale.pick("交给 agent", "for the agent"),
        TriageVerdict::Unsure => locale.pick("不确定", "unsure"),
    }
}

pub fn session_state(locale: Locale, live: bool) -> &'static str {
    if live {
        locale.pick("在线", "live")
    } else {
        locale.pick("失联", "stale")
    }
}

/// Why an item awaits the Speaker. `who` are bracketed mentions.
pub fn awaiting_pick(locale: Locale, candidates: &[String]) -> String {
    let who = list(locale, candidates);
    match locale {
        Locale::Zh => format!("要在 {who} 中选一个转交"),
        Locale::En => format!("route to one of {who}"),
    }
}

pub fn awaiting_addressed(locale: Locale) -> String {
    locale
        .pick(
            "发给 [speaker]，需要回答",
            "addressed to [speaker]: needs an answer",
        )
        .to_string()
}

pub fn awaiting_away(locale: Locale, agents: &[String]) -> String {
    let who = list(locale, agents);
    match locale {
        Locale::Zh => format!("问了 {who}，都不在线：依其检查点代答"),
        Locale::En => format!("asked of {who}, none connected: cover from a checkpoint"),
    }
}

pub fn awaiting_child_offline(locale: Locale, child: &str) -> String {
    match locale {
        Locale::Zh => format!("问了子 Senate [{child}]，它离线：代答，或告知已保留"),
        Locale::En => format!(
            "asked of the child Senate [{child}], which is offline: answer for it, or say it is held"
        ),
    }
}

fn list(locale: Locale, items: &[String]) -> String {
    items.join(locale.pick("、", ", "))
}

/// `the reply is empty`.
pub fn empty_reply(locale: Locale) -> String {
    locale
        .pick("回复内容为空", "the reply is empty")
        .to_string()
}

pub fn not_started(locale: Locale) -> String {
    locale
        .pick("核心尚未启动", "the core is not started")
        .to_string()
}

pub fn already_started(locale: Locale) -> String {
    locale
        .pick("核心已经启动", "the core is already started")
        .to_string()
}

/// A failed read, for the error bar: `inbox of [senator::claude] on
/// refund-v2: <the daemon's message>`.
pub fn read_failed(locale: Locale, what: &str, message: &str) -> String {
    format!("{what}{}{message}", locale.pick("：", ": "))
}

pub fn inbox_of(locale: Locale, agent: &str, committee: &str) -> String {
    match locale {
        Locale::Zh => format!("{agent} 在 {committee} 的收件箱"),
        Locale::En => format!("inbox of {agent} on {committee}"),
    }
}

pub fn approvals_word(locale: Locale) -> &'static str {
    locale.pick("审批", "approvals")
}

/// This Senate's registration with the Senate above.
pub fn uplink(locale: Locale, u: &senate_bridge::ipc::UplinkStatus) -> String {
    use senate_bridge::ipc::Registration;
    let above = u
        .parent
        .as_ref()
        .map_or_else(|| u.url.clone(), ToString::to_string);
    match (&u.registration, locale) {
        (Registration::Registered, Locale::Zh) => format!("已作为 {} 注册到 {above}", u.name),
        (Registration::Registered, Locale::En) => format!("registered as {} with {above}", u.name),
        (Registration::Pending, Locale::Zh) => format!("正在以 {} 注册到 {above}", u.name),
        (Registration::Pending, Locale::En) => format!("registering as {} with {above}", u.name),
        (Registration::Refused { reason }, Locale::Zh) => {
            format!("{above} 拒绝了注册 {}：{reason}", u.name)
        }
        (Registration::Refused { reason }, Locale::En) => {
            format!("{above} refused {}: {reason}", u.name)
        }
        (Registration::Unreachable { error }, Locale::Zh) => format!("无法连接 {above}：{error}"),
        (Registration::Unreachable { error }, Locale::En) => {
            format!("{above} is unreachable: {error}")
        }
    }
}

/// A seat's presence, in words.
pub fn presence(locale: Locale, state: senate_core::PresenceState) -> &'static str {
    use senate_core::PresenceState as P;
    match state {
        P::Online => locale.pick("在线", "online"),
        P::Disconnected => locale.pick("已断开", "disconnected"),
        P::Absent => locale.pick("离开", "away"),
        P::TakenOver => locale.pick("已接管", "taken over"),
        P::Reassigned => locale.pick("已转交", "reassigned"),
    }
}

pub fn sessions_word(locale: Locale) -> &'static str {
    locale.pick("托管会话", "hosted sessions")
}

/// The menu line for terminals waiting on a person.
pub fn prompts_line(locale: Locale, prompts: usize) -> String {
    match locale {
        Locale::Zh => format!("{prompts} 个终端在等你授权"),
        Locale::En => format!(
            "{prompts} {} waiting for your permission",
            plural(prompts, "terminal", "terminals")
        ),
    }
}

/// A hosted session's model, when it is not the runtime's default:
/// `opus · high`.
pub fn model_label(model: Option<&str>, effort: Option<&str>) -> Option<String> {
    match (model, effort) {
        (None, None) => None,
        (Some(m), None) => Some(m.to_string()),
        (None, Some(e)) => Some(e.to_string()),
        (Some(m), Some(e)) => Some(format!("{m} · {e}")),
    }
}

pub fn no_session(locale: Locale, id: &str) -> String {
    match locale {
        Locale::Zh => format!("本机没有会话 {id}"),
        Locale::En => format!("no session {id} on this machine"),
    }
}

pub fn say_needs_committee(locale: Locale) -> String {
    locale
        .pick(
            "这个会话不在任何委员会，没有地方可以对它说话。",
            "This session is on no committee, so there is nowhere to speak to it.",
        )
        .to_string()
}

/// The sidebar group of sessions on no committee.
pub fn no_committee(locale: Locale) -> &'static str {
    locale.pick("无委员会", "No committee")
}

pub fn docket_word(locale: Locale) -> &'static str {
    locale.pick("议程", "docket")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_in_both_languages() {
        assert_eq!(pending(Locale::En, 1, 0), "1 pending thread");
        assert_eq!(pending(Locale::En, 2, 1), "2 pending threads, 1 approval");
        assert_eq!(pending(Locale::Zh, 2, 1), "2 个待处理线程，1 个审批");
        assert_eq!(pending(Locale::Zh, 0, 0), "0 个待处理线程");
    }

    #[test]
    fn senate_is_never_translated() {
        assert_eq!(senate_title(Locale::Zh, None), "Senate");
        for locale in [Locale::Zh, Locale::En] {
            assert_eq!(
                senate_title(locale, Some("acme/north-desk")),
                "Senate acme/north-desk"
            );
        }
    }

    #[test]
    fn backoff_reads_as_behind() {
        let now = Utc::now();
        let s = StreamState::Backoff {
            attempt: 3,
            retry_at: now + chrono::Duration::seconds(12),
            error: "502".into(),
        };
        assert_eq!(
            stream(Locale::En, Some(&s), now),
            "behind, retry in 12s (attempt 3)"
        );
        assert_eq!(
            stream(Locale::Zh, Some(&s), now),
            "落后，12 秒后重试（第 3 次）"
        );
    }
}
