//! What a presiding session reads: the session that holds a Senate's
//! Speaker's seat, as a senator ([`senate_core::SpeakerSeat::Senator`]).
//!
//! Such a session is attached to two Senates (or to one Senate in two roles,
//! for a top Senate used alone): it sits in one as a senator and presides
//! over the other as its Speaker. Its duties as Speaker are few, and each
//! starts from the docket ([`senate_core::docket`]):
//!
//! - **pick**: route a message for one of a person's several agents to one
//!   of them (MCP `speaker_route`, IPC `route`);
//! - **cover**: answer a question to an away agent of that Senate on its
//!   behalf, from that agent's own latest checkpoint, never by rerouting it
//!   to a sibling (`speaker_cover`, `cover`);
//! - **escalate**: put what it cannot settle to the person
//!   (`speaker_escalate`, `refer`);
//! - **speak upward**: nothing of its own; the same session uses its seat
//!   in the Senate above, in its own name.
//!
//! `speaker_docket` (`docket`) lists what awaits it. The session is told it
//! presides when it starts, and is told of new items when its user next
//! writes to it. Everything here is rendered in the bridge's locale, one
//! function per message; ids are bracketed mentions; Senate, senator and
//! Speaker are not translated.

use senate_core::{Address, Awaiting, CheckpointRecord, Kind, Locale, SeatFacts, SenatorPath};

use crate::daemon::senate_words;
use crate::format;
use crate::ipc::{DocketReply, SenateDocket};

/// Characters of an item's body shown on the docket.
const ITEM_CHARS: usize = 400;
/// Characters of each checkpoint file shown for an away agent.
const CHECKPOINT_FILE_CHARS: usize = 1_500;

fn mention(path: &SenatorPath) -> String {
    Address::senator(path).in_text()
}

fn mentions(paths: &[SenatorPath]) -> String {
    paths.iter().map(mention).collect::<Vec<_>>().join(", ")
}

/// A presiding op from a session that does not preside.
pub fn not_presiding(agent: &SenatorPath, locale: Locale) -> String {
    let me = mention(agent);
    match locale {
        Locale::Zh => format!(
            "{me} 不占任何 Senate 的 Speaker 席位，所以这个会话没有主持工具。只有主持某个 Senate \
             的 agent 的会话才能代表 Speaker 发言。"
        ),
        Locale::En => format!(
            "{me} holds no Senate's Speaker's seat, so this session has no presiding tools. Only \
             the session of the agent presiding over a Senate speaks as its Speaker."
        ),
    }
}

/// Told at session start: this session presides, over which Senate, who
/// sits there, and how much awaits it.
pub fn orientation(
    speaker: &SenatorPath,
    senate: Option<&SenatorPath>,
    seated: &[SenatorPath],
    waiting: usize,
    locale: Locale,
) -> String {
    let me = mention(speaker);
    let senate = senate_words(senate, locale);
    let others: Vec<SenatorPath> = seated.iter().filter(|a| *a != speaker).cloned().collect();
    match locale {
        Locale::Zh => {
            let who = if others.is_empty() {
                "目前还没有 agent 入席".to_owned()
            } else {
                format!("在席的有 {}", mentions(&others))
            };
            let queue = if waiting == 0 {
                "现在没有等你处理的事项。".to_owned()
            } else {
                format!("现在有 {waiting} 件事等你处理。")
            };
            format!(
                "[senate] {me} 是 {senate} 的 Speaker，由这个会话主持；{who}。作为 Speaker 你要做的：\
                 有人找某人的 agent 却没说哪一个时替他选定（mcp__senate__speaker_route），\
                 某个 agent 不在时依据它自己的最新 checkpoint 代它回答，绝不转给它的兄弟 agent\
                 （mcp__senate__speaker_cover），解决不了的交给人（mcp__senate__speaker_escalate）。\
                 用 mcp__senate__speaker_docket 查看等你处理的事项和每个 agent 的在线状态、checkpoint、\
                 未结 thread。你在上一级说的话仍以你自己的名义。{queue}"
            )
        }
        Locale::En => {
            let who = if others.is_empty() {
                "no agent is seated there yet".to_owned()
            } else {
                format!("{} sit there", mentions(&others))
            };
            let queue = if waiting == 0 {
                "Nothing awaits you now.".to_owned()
            } else if waiting == 1 {
                "1 item awaits you now.".to_owned()
            } else {
                format!("{waiting} items await you now.")
            };
            format!(
                "[senate] {me} is the Speaker of {senate}, and this session presides; {who}. As \
                 Speaker you pick which agent gets a message written to someone's agent without \
                 saying which (mcp__senate__speaker_route), answer for an away agent from that \
                 agent's own latest checkpoint and never reroute it to a sibling \
                 (mcp__senate__speaker_cover), and put what you cannot settle to the person \
                 (mcp__senate__speaker_escalate). mcp__senate__speaker_docket lists what awaits \
                 you, with each agent's presence, checkpoint and open threads. What you say in the \
                 Senate above is still said in your own name. {queue}"
            )
        }
    }
}

/// Told at the user's next prompt: new items await the Speaker.
pub fn nudge(new: usize, senate: Option<&SenatorPath>, locale: Locale) -> String {
    let senate = senate_words(senate, locale);
    match locale {
        Locale::Zh => format!(
            "[senate] 作为 {senate} 的 Speaker，有 {new} 件新事项等你处理：调用 \
             mcp__senate__speaker_docket 查看。"
        ),
        Locale::En => {
            let items = if new == 1 {
                "item awaits"
            } else {
                "items await"
            };
            format!(
                "[senate] As Speaker of {senate}, {new} new {items} you: call \
                 mcp__senate__speaker_docket."
            )
        }
    }
}

fn why(awaiting: &Awaiting, locale: Locale) -> String {
    match (awaiting, locale) {
        (Awaiting::Pick { owner, candidates }, Locale::Zh) => format!(
            "写给 {} 的 agent 但没说哪一个；候选：{}。用 speaker_route 选一个，或 speaker_escalate 问人。",
            Address::senior(owner).in_text(),
            mentions(candidates)
        ),
        (Awaiting::Pick { owner, candidates }, Locale::En) => format!(
            "for one of {}'s agents, not saying which; candidates: {}. Pick one with \
             speaker_route, or ask the person with speaker_escalate.",
            Address::senior(owner).in_text(),
            mentions(candidates)
        ),
        (Awaiting::Addressed, Locale::Zh) => {
            "写给 Speaker 的。用 speaker_route 交给一个 agent，或 speaker_escalate 交给人。"
                .to_owned()
        }
        (Awaiting::Addressed, Locale::En) => "addressed to the Speaker. Hand it to an agent with \
             speaker_route, or to the person with speaker_escalate."
            .to_owned(),
        (Awaiting::Away { agents }, Locale::Zh) => format!(
            "问 {} 的问题，对方不在、还没人回答。依据它自己的 checkpoint 用 speaker_cover 代答，\
             或 speaker_escalate 交给人。",
            mentions(agents)
        ),
        (Awaiting::Away { agents }, Locale::En) => format!(
            "a question to {}, who is away, not answered yet. Answer on its behalf from its own \
             checkpoint with speaker_cover, or put it to the person with speaker_escalate.",
            mentions(agents)
        ),
    }
}

fn kind_word(kind: &Kind, locale: Locale) -> String {
    match (kind, locale) {
        (Kind::Question, Locale::Zh) => "问题".into(),
        (Kind::Request, Locale::Zh) => "请求".into(),
        (Kind::Note, Locale::Zh) => "留言".into(),
        (other, _) => other.to_string(),
    }
}

fn render_checkpoint(record: &CheckpointRecord, locale: Locale) -> String {
    let mut out = match locale {
        Locale::Zh => format!(
            "{} 的最新 checkpoint（seq {}，{}，分支 {}）：{}",
            mention(&record.agent),
            record.seq,
            record.ts.format("%Y-%m-%d %H:%M UTC"),
            record.checkpoint.branch,
            record.body
        ),
        Locale::En => format!(
            "Latest checkpoint of {} (seq {}, {}, branch {}): {}",
            mention(&record.agent),
            record.seq,
            record.ts.format("%Y-%m-%d %H:%M UTC"),
            record.checkpoint.branch,
            record.body
        ),
    };
    let files = &record.checkpoint.files;
    for (name, text) in [
        ("PROGRESS.md", &files.progress),
        ("DECISIONS.md", &files.decisions),
        ("OPEN.md", &files.open),
    ] {
        if let Some(text) = text.as_deref().filter(|t| !t.trim().is_empty()) {
            let (cut, _) = format::truncate_chars(text.trim(), CHECKPOINT_FILE_CHARS);
            out.push_str(&format!("\n  {name}:\n"));
            for line in cut.lines() {
                out.push_str(&format!("  │ {line}\n"));
            }
        }
    }
    out
}

/// What is true of an agent's connection right now, in words a person can
/// act on, then the presence state the server keeps. The state stays
/// `online` until the disconnect grace passes, so it never stands alone:
/// whether a session is open is the connection's own fact.
fn seat_line(seat: &SeatFacts, locale: Locale) -> String {
    let now = if seat.connected {
        locale.pick("已连接", "connected").to_owned()
    } else {
        let last = seat.last_seen.map(|t| t.format("%Y-%m-%d %H:%M UTC"));
        match (last, locale) {
            (Some(t), Locale::Zh) => format!("没有打开的会话（最后一次联络 {t}）"),
            (Some(t), Locale::En) => format!("no session open (last seen {t})"),
            (None, Locale::Zh) => "没有打开的会话".to_owned(),
            (None, Locale::En) => "no session open".to_owned(),
        }
    };
    match (seat.presence, locale) {
        (Some(p), Locale::Zh) => format!("{now}，presence：{}", p.label(locale)),
        (Some(p), Locale::En) => format!("{now}, presence: {}", p.label(locale)),
        (None, _) => now,
    }
}

fn render_senate(docket: &SenateDocket, locale: Locale, out: &mut String) {
    out.push_str(&format!(
        "## {}\n",
        senate_words(docket.senate.as_ref(), locale)
    ));
    let items: usize = docket.pages.iter().map(|p| p.items.len()).sum();
    if items == 0 {
        out.push_str(locale.pick(
            "没有等待 Speaker 的事项。\n",
            "Nothing awaits the Speaker.\n",
        ));
    }
    for page in &docket.pages {
        for item in &page.items {
            let event = &item.event;
            let from = event
                .author()
                .map(|m| m.to_string())
                .unwrap_or_else(|| event.from.to_string());
            let (body, cut) = format::truncate_chars(&event.body, ITEM_CHARS);
            let ellipsis = if cut { "…" } else { "" };
            out.push_str(&format!(
                "- `{}` seq {} · {} · {}: {}\n  │ {}{ellipsis}\n",
                page.committee,
                event.seq,
                kind_word(&event.kind, locale),
                from,
                why(&item.awaiting, locale),
                body.replace('\n', "\n  │ "),
            ));
        }
    }
    let seats: Vec<_> = docket
        .pages
        .iter()
        .filter(|p| !p.seats.is_empty())
        .collect();
    for page in seats {
        out.push_str(&match locale {
            Locale::Zh => format!("在席（`{}`）：\n", page.committee),
            Locale::En => format!("Seated (`{}`):\n", page.committee),
        });
        for seat in &page.seats {
            let presence = seat_line(seat, locale);
            let checkpoint = match (&seat.checkpoint, locale) {
                (Some(c), Locale::Zh) => format!("checkpoint seq {}：{}", c.seq, c.summary),
                (Some(c), Locale::En) => format!("checkpoint seq {}: {}", c.seq, c.summary),
                (None, Locale::Zh) => "没有 checkpoint".to_owned(),
                (None, Locale::En) => "no checkpoint".to_owned(),
            };
            out.push_str(&match locale {
                Locale::Zh => format!(
                    "- {} · {presence} · {checkpoint} · 未结 thread {}\n",
                    mention(&seat.agent),
                    seat.open_threads
                ),
                Locale::En => format!(
                    "- {} · {presence} · {checkpoint} · {} open threads\n",
                    mention(&seat.agent),
                    seat.open_threads
                ),
            });
        }
    }
    for record in &docket.checkpoints {
        out.push_str(&render_checkpoint(record, locale));
        out.push('\n');
    }
}

/// The docket as the presiding session (or the person) reads it.
pub fn render_docket(reply: &DocketReply, locale: Locale) -> String {
    let mut out = match (&reply.speaker, locale) {
        (Some(s), Locale::Zh) => format!("{} 作为 Speaker 的待办：\n", mention(s)),
        (Some(s), Locale::En) => format!("What awaits {} as Speaker:\n", mention(s)),
        (None, Locale::Zh) => "等待 Speaker 的事项：\n".to_owned(),
        (None, Locale::En) => "What awaits the Speaker:\n".to_owned(),
    };
    if reply.senates.is_empty() {
        out.push_str(locale.pick(
            "这台机器上没有由你的 agent 主持的 Senate。\n",
            "No Senate on this machine is presided over by one of your agents.\n",
        ));
    }
    for senate in &reply.senates {
        render_senate(senate, locale, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> SenatorPath {
        s.parse().unwrap()
    }

    #[test]
    fn a_presiding_session_is_told_where_and_over_whom_in_either_language() {
        let seated = [path("alice/claude/reviewer"), path("alice/claude/codex")];
        for locale in [Locale::Zh, Locale::En] {
            let text = orientation(
                &path("alice/claude"),
                Some(&path("alice/claude")),
                &seated,
                2,
                locale,
            );
            for needle in [
                "[senator::alice/claude]",
                "[senator::alice/claude/reviewer]",
                "[senator::alice/claude/codex]",
                "Speaker",
                "Senate",
                "mcp__senate__speaker_docket",
                "mcp__senate__speaker_route",
                "mcp__senate__speaker_cover",
                "mcp__senate__speaker_escalate",
                "2",
            ] {
                assert!(text.contains(needle), "{needle} in {text}");
            }
            assert!(!text.to_lowercase().contains("steward"));
        }
        let alone = orientation(&path("alice/claude"), None, &[], 0, Locale::En);
        assert!(alone.contains("the top Senate") && alone.contains("Nothing awaits"));
        assert!(nudge(1, None, Locale::Zh).contains("1 件"));
        assert!(not_presiding(&path("alice/codex"), Locale::En).contains("[senator::alice/codex]"));
    }

    #[test]
    fn a_seat_says_whether_a_session_is_open_and_keeps_the_presence_state() {
        use chrono::{TimeZone, Utc};
        use senate_core::PresenceState;
        let seat = |connected, presence, last_seen| SeatFacts {
            agent: path("wendell/claude"),
            connected,
            presence,
            last_seen,
            checkpoint: None,
            open_threads: 0,
        };
        let seen = Some(Utc.with_ymd_and_hms(2026, 11, 16, 10, 3, 0).unwrap());
        let online = Some(PresenceState::Online);
        // Its session just ended: the state is still online, the session is gone.
        assert_eq!(
            seat_line(&seat(false, online, seen), Locale::En),
            "no session open (last seen 2026-11-16 10:03 UTC), presence: online"
        );
        assert_eq!(
            seat_line(&seat(false, online, seen), Locale::Zh),
            "没有打开的会话（最后一次联络 2026-11-16 10:03 UTC），presence：在线"
        );
        assert_eq!(
            seat_line(&seat(true, online, seen), Locale::En),
            "connected, presence: online"
        );
        assert_eq!(
            seat_line(&seat(true, online, seen), Locale::Zh),
            "已连接，presence：在线"
        );
        assert_eq!(
            seat_line(&seat(false, None, None), Locale::En),
            "no session open"
        );
        assert_eq!(
            seat_line(&seat(false, None, None), Locale::Zh),
            "没有打开的会话"
        );
        assert_eq!(seat_line(&seat(true, None, None), Locale::En), "connected");
        assert_eq!(seat_line(&seat(true, None, None), Locale::Zh), "已连接");
    }
}
