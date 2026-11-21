//! What comes back to the person, in words: their inbox, and (below) what
//! `senate inbox --follow`, `--wait` and the desktop notification say.
//!
//! The decision is core's ([`senate_core::ThreadSummary::waits_on_person`]);
//! this module only writes it down, in either language, with ids as
//! bracketed mentions. Read state is this machine's: the highest seq of each
//! thread the person has seen ([`crate::state::Store::mark_seen`]).

use senate_core::{Address, Directed, Kind, Locale, Mention, ThreadSummary};

use crate::format;
use crate::ipc::InboxReply;
use crate::mcp::{self, ShowAll};

/// Who wrote an event addressed to the person, as a mention.
pub fn byline(from: &Address, on_behalf_of: Option<&Address>) -> String {
    Mention::author(from, on_behalf_of)
        .map(|m| format::one_line(&m.to_string()))
        .unwrap_or_else(|| format::mention(from))
}

/// What the writer did, in a clause that follows their byline.
pub fn did(kind: &Kind, escalated: bool, locale: Locale) -> &'static str {
    if escalated {
        return locale.pick("把它转给了你", "escalated this to you");
    }
    match kind {
        Kind::Answer => locale.pick("回答了", "answered"),
        Kind::Note => locale.pick("留了一条 note", "left a note"),
        Kind::Question => locale.pick("问了你", "asked you"),
        Kind::Request => locale.pick("向你提了一个 request", "sent you a request"),
        Kind::Decision => locale.pick("记了一个决定", "recorded a decision"),
        _ => locale.pick("写了", "wrote"),
    }
}

/// The commands for reading, answering and taking over thread `root`.
pub fn commands(
    committee: &senate_core::CommitteeId,
    root: senate_core::Seq,
    locale: Locale,
) -> String {
    match locale {
        Locale::Zh => format!(
            "读：`senate thread {root} --committee {committee}` · 回复：`senate reply {root} \
             --committee {committee} <文字>` · 接管：`senate take {root} --committee {committee}`"
        ),
        Locale::En => format!(
            "read: `senate thread {root} --committee {committee}` · reply: `senate reply {root} \
             --committee {committee} <text>` · take over: `senate take {root} --committee {committee}`"
        ),
    }
}

fn indented_lines(text: &str, max: usize, root: senate_core::Seq, locale: Locale) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for line in lines.iter().take(max) {
        out.push_str("\n      ");
        out.push_str(&format::one_line(line));
    }
    if lines.len() > max {
        let more = lines.len() - max;
        out.push_str(&match locale {
            Locale::Zh => format!("\n      …（还有 {more} 行，见 `senate thread {root}`）"),
            Locale::En => format!("\n      … ({more} more lines: `senate thread {root}`)"),
        });
    }
    out
}

/// One thread that waits on the person: its title, what is new (who wrote
/// it, what kind, its first line), the escalation with its reason, and the
/// commands to read and answer.
fn person_thread(reply: &InboxReply, t: &ThreadSummary, locale: Locale) -> String {
    let seen = reply.seen_of(t.root);
    let unread = t.unread_for(&reply.person, seen);
    let title = format::one_line(&t.title);
    let mut out = format!("\n- thread {} · {title}", t.root);
    if !unread.is_empty() {
        out.push_str(locale.pick(" · 对你是新的", " · new for you"));
    }
    let mut shown_escalation = false;
    let escalation = t.escalation.as_ref().filter(|e| e.to == reply.person);
    for d in &unread {
        if d.escalated {
            shown_escalation = true;
        }
        out.push_str(&directed_line(t, d, escalation, locale));
    }
    if let Some(e) = escalation
        && !shown_escalation
    {
        // Seen, but it still waits: it lasts until they act.
        let by = byline(&e.by, None);
        out.push_str(&format!(
            "\n    {by} {}{}",
            did(&Kind::Status, true, locale),
            locale.pick(
                "（你已看过，等你处理）：",
                " (seen; it waits until you act):"
            )
        ));
        out.push_str(&reason_lines(e.reason.as_deref(), t.root, locale));
    }
    out.push_str("\n    ");
    out.push_str(&commands(&reply.committee, t.root, locale));
    out
}

fn reason_lines(reason: Option<&str>, root: senate_core::Seq, locale: Locale) -> String {
    match reason.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => indented_lines(r, 24, root, locale),
        None => format!(
            "\n      {}",
            locale.pick("（没有写原因）", "(no reason given)")
        ),
    }
}

/// One event addressed to the person, in a thread: `[by] answered: first
/// line`, or for an escalation the full reason under it.
fn directed_line(
    t: &ThreadSummary,
    d: &Directed,
    escalation: Option<&senate_core::Escalation>,
    locale: Locale,
) -> String {
    let by = byline(&d.from, d.on_behalf_of.as_ref());
    let verb = did(&d.kind, d.escalated, locale);
    if d.escalated {
        let reason = escalation
            .filter(|e| e.seq == d.seq)
            .and_then(|e| e.reason.as_deref())
            .unwrap_or(&d.line);
        return format!(
            "\n    {by} {verb}{}{}",
            locale.pick("：", ":"),
            reason_lines(Some(reason), t.root, locale)
        );
    }
    let line = format::one_line(&d.line);
    if line.trim().is_empty() {
        format!("\n    {by} {verb}")
    } else {
        format!(
            "\n    {by} {verb}{line_sep}{line}",
            line_sep = locale.pick("：", ": ")
        )
    }
}

/// `senate inbox`: first what waits on the person (the news for them
/// marked, who wrote it, the kind, its first line, an escalation's reason),
/// then the threads that wait on their agents or are parked on them, then
/// how many settled ones were left out. With nothing for the person it is
/// the agents' listing ([`mcp::render_inbox`]).
pub fn render_inbox(reply: &InboxReply, locale: Locale, how: ShowAll) -> String {
    let (mut mine, others): (Vec<&ThreadSummary>, Vec<&ThreadSummary>) = reply
        .threads
        .iter()
        .partition(|t| reply.waits_on_person(t).is_some());
    if mine.is_empty() {
        return mcp::render_inbox(reply, locale, how);
    }
    // News first; the order of the daemon's listing within each.
    mine.sort_by_key(|t| reply.waits_on_person(t).is_none_or(|w| w.unread.is_none()));
    let news = mine
        .iter()
        .filter(|t| reply.waits_on_person(t).is_some_and(|w| w.unread.is_some()))
        .count();
    let me = Address::senior(&reply.person).in_text();
    let c = &reply.committee;
    let n = mine.len();
    let mut out = match locale {
        Locale::Zh => format!("{c} 上有 {n} 个 thread 在等你（{me}），其中 {news} 个有新内容："),
        Locale::En => format!("{n} thread(s) on {c} wait on you ({me}); {news} with news:"),
    };
    for t in &mine {
        out.push_str(&person_thread(reply, t, locale));
    }
    if others.is_empty() {
        if let Some(line) = mcp::settled_line(reply.settled, how, locale) {
            out.push('\n');
            out.push_str(&line);
        }
    } else {
        out.push_str("\n\n");
        let rest = InboxReply {
            threads: others.into_iter().cloned().collect(),
            ..reply.clone()
        };
        out.push_str(&mcp::render_inbox(&rest, locale, how));
    }
    out
}

/// The threads [`render_inbox`] lists as waiting on the person, with the
/// seq to mark each seen up to (its last event).
pub fn unread_marks(reply: &InboxReply) -> Vec<crate::ipc::ThreadSeen> {
    reply
        .threads
        .iter()
        .filter(|t| reply.waits_on_person(t).is_some_and(|w| w.unread.is_some()))
        .map(|t| crate::ipc::ThreadSeen {
            thread: t.root,
            seen: t.updated,
        })
        .collect()
}

/// `senate inbox --mark-read`: what it did.
pub fn render_marked(n: usize, locale: Locale) -> String {
    match locale {
        Locale::Zh => format!("已把 {n} 个 thread 标为已读（升级转给你的仍在等你处理）。"),
        Locale::En => {
            format!("Marked {n} thread(s) as read (escalations to you still wait until you act).")
        }
    }
}

/// Most lines of an event's text a `watch` block shows.
const NEWS_LINES: usize = 6;

/// One event addressed to the person, as `senate inbox --follow` prints it: the
/// byline, what it did, when, where; the first lines of what it says; the
/// commands to read and reply.
pub fn render_news(
    event: &senate_core::Event,
    escalated: bool,
    text: &str,
    locale: Locale,
) -> String {
    let root = event.thread.unwrap_or(event.seq);
    let by = format::author(event, locale);
    let verb = did(&event.kind, escalated, locale);
    let c = &event.committee;
    let mut out = match locale {
        Locale::Zh => format!(
            "{by} {verb} · {} · {c} · thread {root}",
            event.ts.format("%Y-%m-%d %H:%M UTC")
        ),
        Locale::En => format!(
            "{by} {verb} · {} · {c} · thread {root}",
            event.ts.format("%Y-%m-%d %H:%M UTC")
        ),
    };
    let lines: Vec<&str> = text.trim().lines().collect();
    if lines.is_empty() {
        out.push_str(&format!(
            "\n  {}",
            locale.pick("（没有写原因）", "(nothing written)")
        ));
    }
    for line in lines.iter().take(NEWS_LINES) {
        out.push_str("\n  ");
        out.push_str(&format::one_line(line));
    }
    if lines.len() > NEWS_LINES {
        out.push_str("\n  …");
    }
    out.push_str(&match locale {
        Locale::Zh => format!(
            "\n  读：`senate thread {root} --committee {c}` · 回复：`senate reply {root} --committee {c} <文字>`"
        ),
        Locale::En => format!(
            "\n  read: `senate thread {root} --committee {c}` · reply: `senate reply {root} --committee {c} <text>`"
        ),
    });
    out
}

/// What `senate inbox --follow` says on stderr when it starts.
pub fn render_watching(committee: Option<&senate_core::CommitteeId>, locale: Locale) -> String {
    match (committee, locale) {
        (Some(c), Locale::Zh) => format!("正在看 {c} 上给你的消息（Ctrl-C 结束）……"),
        (Some(c), Locale::En) => {
            format!("Watching what is addressed to you on {c} (Ctrl-C to stop)…")
        }
        (None, Locale::Zh) => "正在看这台机器上各委员会里给你的消息（Ctrl-C 结束）……".to_string(),
        (None, Locale::En) => {
            "Watching what is addressed to you on this machine's committees (Ctrl-C to stop)…"
                .to_string()
        }
    }
}

/// `senate inbox --follow` lost the daemon.
pub fn render_watch_closed(locale: Locale) -> &'static str {
    locale.pick(
        "守护进程关闭了连接；重新运行 `senate inbox --follow`。",
        "the daemon closed the connection; run `senate inbox --follow` again.",
    )
}
