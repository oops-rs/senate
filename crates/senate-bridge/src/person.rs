//! What comes back to the person, in words: their inbox, and (below) what
//! `senate inbox --follow`, `--wait` and the desktop notification say.
//!
//! The decision is core's ([`senate_core::ThreadSummary::waits_on_person`]);
//! this module only writes it down, in either language, with ids as
//! bracketed mentions. Read state is this machine's: the highest seq of each
//! thread the person has seen ([`crate::state::Store::mark_seen`]).

use senate_core::{Address, CommissionBucket, Directed, Kind, Locale, Mention, ThreadSummary};

use crate::format;
use crate::ipc::InboxReply;
use crate::mcp::{self, ShowAll};

/// Who wrote an event addressed to the person, as a mention.
pub fn byline(from: &Address, on_behalf_of: Option<&Address>) -> String {
    Mention::author(from, on_behalf_of)
        .map(|m| format::one_line(&m.to_string()))
        .unwrap_or_else(|| format::mention(from))
}

/// How people are shown a mention, as the app shows it everywhere (its
/// README, "Names"): an agent by its alias on `committee`, else its
/// runtime and the first eight hex digits of its id (`agy-36358e84`); a
/// person by their name; `copilot (Speaker)`; never brackets, `senator::`
/// or a whole id. `aliases` is what is known of the Senate's aliases;
/// without them, the short form. A seat in another Senate is its short
/// name with that Senate's name (the app knows the alias a child declares,
/// and says that instead).
pub fn shown(
    mention: &Mention,
    committee: Option<&senate_core::CommitteeId>,
    aliases: Option<&senate_core::alias::KnownAliases>,
    locale: Locale,
) -> String {
    let senator = |p: &senate_core::SenatorName, c: Option<&senate_core::CommitteeId>| {
        senate_core::alias::display_name(aliases, c, p)
    };
    match mention {
        Mention::Senator(p) => senator(p, committee),
        Mention::AnyAgent => locale.pick("任一代理", "any agent").to_string(),
        Mention::Speaker => "Speaker".to_string(),
        Mention::SpeakerFor(p) => role(locale, &senator(p, committee), "Speaker"),
        Mention::Chair => locale.pick("主席", "chair").to_string(),
        Mention::Committee => locale.pick("委员会", "committee").to_string(),
        Mention::Guest(g) => g.to_string(),
        Mention::Locator(loc) => {
            let c = loc.committee();
            match (Mention::of(loc.address()), loc.senate()) {
                (Some(Mention::Senator(p)), Some(senate)) => role(
                    locale,
                    &senate_core::alias::short_name(&p),
                    senate.name().as_str(),
                ),
                (Some(Mention::Senator(p)), None) => {
                    let name = senator(&p, c);
                    c.map_or(name.clone(), |c| role(locale, &name, c.as_str()))
                }
                (Some(other), _) => {
                    let name = shown(&other, c, aliases, locale);
                    c.map_or(name.clone(), |c| role(locale, &name, c.as_str()))
                }
                (None, _) => loc.to_string(),
            }
        }
    }
}

/// `name (what)` / `name（what）`.
pub fn role(locale: Locale, name: &str, what: &str) -> String {
    match locale {
        Locale::Zh => format!("{name}（{what}）"),
        Locale::En => format!("{name} ({what})"),
    }
}

/// `text` with every bracketed mention in it written as people are shown
/// it (`name`: [`shown`], or the app's own resolution). Brackets that
/// hold anything else (`[1]`, `[draft]`) stay.
pub fn plain(text: &str, name: impl Fn(&Mention) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for found in Mention::scan(text) {
        out.push_str(&text[at..found.range.start]);
        out.push_str(&name(&found.mention));
        at = found.range.end;
    }
    out.push_str(&text[at..]);
    out
}

/// What a desktop notification says about an event addressed to the
/// person: who, what they did, and its first line, every mention named
/// by `name` (`agy-36358e84 sent you a request: …`). The daemon's
/// notifier and the app say it the same way.
pub fn notification_line(
    from: &Address,
    on_behalf_of: Option<&Address>,
    kind: &Kind,
    escalated: bool,
    text: &str,
    locale: Locale,
    name: impl Fn(&Mention) -> String,
) -> String {
    let by = plain(&byline(from, on_behalf_of), &name);
    let verb = did(kind, escalated, locale);
    let line = plain(
        &format::one_line(text.lines().next().unwrap_or_default().trim()),
        &name,
    );
    if line.is_empty() {
        format!("{by} {verb}")
    } else {
        format!("{by} {verb}{}{line}", locale.pick("：", ": "))
    }
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
fn person_thread(
    reply: &InboxReply,
    t: &ThreadSummary,
    locale: Locale,
    display_byline: &impl Fn(String) -> String,
) -> String {
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
        out.push_str(&directed_line(t, d, escalation, locale, display_byline));
    }
    if let Some(e) = escalation
        && !shown_escalation
    {
        // Seen, but it still waits: it lasts until they act.
        let by = display_byline(byline(&e.by, None));
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
    display_byline: &impl Fn(String) -> String,
) -> String {
    let by = display_byline(byline(&d.from, d.on_behalf_of.as_ref()));
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
    render_person_inbox(reply, locale, how, |by| by)
}

pub(crate) fn render_named_inbox(
    reply: &InboxReply,
    locale: Locale,
    aliases: Option<&senate_core::alias::KnownAliases>,
) -> String {
    render_person_inbox(reply, locale, ShowAll::Flag, |by| {
        senate_core::alias::display_mentions(&by, aliases, Some(&reply.committee))
    })
}

fn commission_bucket_label(bucket: CommissionBucket, locale: Locale) -> &'static str {
    match (bucket, locale) {
        (CommissionBucket::Blocked, Locale::Zh) => "阻塞中",
        (CommissionBucket::Blocked, Locale::En) => "blocked",
        (CommissionBucket::PendingAcceptance, Locale::Zh) => "待验收",
        (CommissionBucket::PendingAcceptance, Locale::En) => "pending acceptance",
        (CommissionBucket::CompletedUnread, Locale::Zh) => "已完成（未读）",
        (CommissionBucket::CompletedUnread, Locale::En) => "completed (unread)",
        (CommissionBucket::InProgress, Locale::Zh) => "推进中",
        (CommissionBucket::InProgress, Locale::En) => "in progress",
        (CommissionBucket::CompletedRead, Locale::Zh) => "已完成",
        (CommissionBucket::CompletedRead, Locale::En) => "completed",
    }
}

fn commission_line(
    reply: &InboxReply,
    t: &ThreadSummary,
    bucket: CommissionBucket,
    locale: Locale,
    display_byline: &impl Fn(String) -> String,
) -> String {
    let title = format::one_line(&t.title);
    let label = commission_bucket_label(bucket, locale);
    let c = &reply.committee;
    let mut out = format!("\n- [{label}] thread {} · {title}", t.root);
    let detail = match bucket {
        CommissionBucket::Blocked => t
            .escalation
            .as_ref()
            .and_then(|e| e.reason.as_deref())
            .or(t.reason.as_deref()),
        CommissionBucket::CompletedUnread | CommissionBucket::CompletedRead => {
            t.answer_line.as_deref()
        }
        CommissionBucket::PendingAcceptance | CommissionBucket::InProgress => {
            t.steps.last().map(|s| s.label.as_str())
        }
    };
    if let Some(detail) = detail {
        let detail = display_byline(format::one_line(detail));
        if !detail.is_empty() {
            out.push_str(&format!("\n    {detail}"));
        }
    }
    let root = t.root;
    out.push_str(&match locale {
        Locale::Zh => format!("\n    阅读：`senate thread {root} --committee {c}`"),
        Locale::En => format!("\n    read: `senate thread {root} --committee {c}`"),
    });
    out
}

fn render_person_inbox(
    reply: &InboxReply,
    locale: Locale,
    how: ShowAll,
    display_byline: impl Fn(String) -> String,
) -> String {
    let (mut mine, rest): (Vec<&ThreadSummary>, Vec<&ThreadSummary>) = reply
        .threads
        .iter()
        .partition(|t| reply.waits_on_person(t).is_some());
    let mut sponsored: Vec<(CommissionBucket, &ThreadSummary)> = Vec::new();
    let mut others: Vec<&ThreadSummary> = Vec::new();
    for t in rest {
        let seen = reply.seen_of(t.root);
        let bucket = t.commission_bucket(seen);
        if t.is_sponsored_by(&reply.person) && bucket != CommissionBucket::CompletedRead {
            sponsored.push((bucket, t));
            if !t.is_settled() {
                others.push(t);
            }
        } else {
            others.push(t);
        }
    }
    if mine.is_empty() && sponsored.is_empty() {
        return mcp::render_inbox(reply, locale, how);
    }
    let mut out = String::new();
    if !mine.is_empty() {
        // News first; the order of the daemon's listing within each.
        mine.sort_by_key(|t| reply.waits_on_person(t).is_none_or(|w| w.unread.is_none()));
        let news = mine
            .iter()
            .filter(|t| reply.waits_on_person(t).is_some_and(|w| w.unread.is_some()))
            .count();
        let me = Address::senior(&reply.person).in_text();
        let c = &reply.committee;
        let n = mine.len();
        out.push_str(&match locale {
            Locale::Zh => {
                format!("{c} 上有 {n} 个 thread 在等你（{me}），其中 {news} 个有新内容：")
            }
            Locale::En => format!("{n} thread(s) on {c} wait on you ({me}); {news} with news:"),
        });
        for t in &mine {
            out.push_str(&person_thread(reply, t, locale, &display_byline));
        }
    }
    if !sponsored.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        sponsored.sort_by_key(|(bucket, t)| (*bucket, std::cmp::Reverse(t.updated), t.root));
        let c = &reply.committee;
        let n = sponsored.len();
        out.push_str(&match locale {
            Locale::Zh => format!("委派追踪（{c}，{n} 个）："),
            Locale::En => format!("My Commissions ({c}, {n}):"),
        });
        for (bucket, t) in sponsored {
            out.push_str(&commission_line(reply, t, bucket, locale, &display_byline));
        }
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

/// The threads [`render_inbox`] lists as waiting on the person or completed
/// commissions unread by them, with the seq to mark each seen up to (its last event).
pub fn unread_marks(reply: &InboxReply) -> Vec<crate::ipc::ThreadSeen> {
    reply
        .threads
        .iter()
        .filter(|t| {
            reply.waits_on_person(t).is_some_and(|w| w.unread.is_some())
                || (t.is_sponsored_by(&reply.person)
                    && t.commission_bucket(reply.seen_of(t.root))
                        == CommissionBucket::CompletedUnread)
        })
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
    render_news_with(event, escalated, text, locale, None, format::Clock::Utc)
}

/// [`render_news`] with the text laid out by `lay_out` (Markdown for a
/// terminal: [`format::terminal_markdown`]) before its first lines are
/// taken.
pub fn render_news_with(
    event: &senate_core::Event,
    escalated: bool,
    text: &str,
    locale: Locale,
    lay_out: Option<&dyn Fn(&str) -> String>,
    clock: format::Clock,
) -> String {
    let root = event.thread.unwrap_or(event.seq);
    let by = format::author(event, locale);
    let verb = did(&event.kind, escalated, locale);
    let c = &event.committee;
    let mut out = match locale {
        Locale::Zh => format!(
            "{by} {verb} · {} · {c} · thread {root}",
            clock.stamp(event.ts)
        ),
        Locale::En => format!(
            "{by} {verb} · {} · {c} · thread {root}",
            clock.stamp(event.ts)
        ),
    };
    let text = if event.from.as_guest().is_some()
        || event
            .on_behalf_of
            .as_ref()
            .is_some_and(|by| by.as_guest().is_some())
    {
        std::borrow::Cow::Owned(senate_core::worker::display_body(text))
    } else {
        std::borrow::Cow::Borrowed(text)
    };
    let laid_out = lay_out.map(|lay_out| lay_out(text.trim()));
    let lines: Vec<&str> = laid_out
        .as_deref()
        .unwrap_or(&text)
        .trim()
        .lines()
        .collect();
    if lines.is_empty() {
        out.push_str(&format!(
            "\n  {}",
            locale.pick("（没有写原因）", "(nothing written)")
        ));
    }
    for line in lines.iter().take(NEWS_LINES) {
        out.push_str("\n  ");
        if laid_out.is_some() {
            // Already made printable, with its styles kept.
            out.push_str(line);
        } else {
            out.push_str(&format::one_line(line));
        }
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
pub fn render_watching(
    committee: Option<&senate_core::CommitteeId>,
    locale: Locale,
    clock: format::Clock,
) -> String {
    let watching = watching(committee, locale);
    match (clock.zone(), locale) {
        (None, _) => watching,
        (Some(zone), Locale::Zh) => format!("{watching}（本地时间，{zone}）"),
        (Some(zone), Locale::En) => format!("{watching} Times are local ({zone})."),
    }
}

fn watching(committee: Option<&senate_core::CommitteeId>, locale: Locale) -> String {
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use senate_core::alias::{CommitteeAliases, KnownAliases};
    use senate_core::{CommitteeId, SenatorName};

    const AGY: &str = "agy-36358e84-ac54-409e-beb4-9b57f93310e0";

    fn c(name: &str) -> CommitteeId {
        CommitteeId::new(name).unwrap()
    }

    fn senator(name: &str) -> Address {
        Address::senator(&name.parse::<SenatorName>().unwrap())
    }

    /// `ledger` is an alias of a Codex session on `payments`.
    fn aliases() -> KnownAliases {
        let mut t = CommitteeAliases::new();
        t.set(
            &c("payments"),
            "ledger".parse().unwrap(),
            "codex-01a206b8-8ed9-7a73-934c-f967b877774a"
                .parse()
                .unwrap(),
        )
        .unwrap();
        KnownAliases::Scoped(t)
    }

    fn line(
        from: &Address,
        kind: Kind,
        text: &str,
        committee: &str,
        aliases: Option<&KnownAliases>,
    ) -> String {
        let committee = c(committee);
        notification_line(from, None, &kind, false, text, Locale::En, |m| {
            shown(m, Some(&committee), aliases, Locale::En)
        })
    }

    #[test]
    fn wendells_two_notifications_name_the_agent_by_its_short_form() {
        // As seen on 0.10.0 (before): the whole id, bracketed, in the
        // byline and in the quoted line.
        let request = format!("From `ibot::chair` ([senator::{AGY}]): please review the dial plan");
        assert_eq!(
            line(&senator(AGY), Kind::Request, &request, "ibot", None),
            "agy-36358e84 sent you a request: From `ibot::chair` (agy-36358e84): please review the dial plan"
        );
        assert_eq!(
            line(
                &senator(AGY),
                Kind::Question,
                "(Re-sending from `ibot` thread 99 because `dial::chair` is currently not seated.)",
                "ibot",
                None,
            ),
            "agy-36358e84 asked you: (Re-sending from `ibot` thread 99 because `dial::chair` is currently not seated.)"
        );
    }

    #[test]
    fn an_agent_with_an_alias_a_person_and_the_speaker() {
        let a = aliases();
        let ledger = senator("codex-01a206b8-8ed9-7a73-934c-f967b877774a");
        // With its alias on the committee in view.
        assert_eq!(
            line(&ledger, Kind::Answer, "done", "payments", Some(&a)),
            "ledger answered: done"
        );
        // On another committee, where it has none: the short form.
        assert_eq!(
            line(&ledger, Kind::Answer, "done", "docs", Some(&a)),
            "codex-01a206b8 answered: done"
        );
        // A person by their name.
        assert_eq!(
            line(
                &senator("alice"),
                Kind::Note,
                "hi [senator::bob]",
                "docs",
                None
            ),
            "alice left a note: hi bob"
        );
        // The Speaker, and the Speaker speaking for an agent.
        let speaker = Address::Speaker;
        assert_eq!(
            line(&speaker, Kind::Question, "", "docs", None),
            "Speaker asked you"
        );
        assert_eq!(
            shown(
                &Mention::SpeakerFor("copilot".parse().unwrap()),
                None,
                None,
                Locale::En
            ),
            "copilot (Speaker)"
        );
        // Brackets that are not a mention stay.
        assert_eq!(
            plain("see [1] and [draft]", |_| "x".into()),
            "see [1] and [draft]"
        );
    }
}

#[cfg(test)]
#[test]
fn followed_news_is_laid_out_as_markdown_only_when_asked() {
    use senate_core::{Address, CommitteeId, Kind, Name, NewEvent, Seq};
    let event = NewEvent::new(
        CommitteeId::new("refund").unwrap(),
        Kind::Answer,
        "senator::rev".parse::<Address>().unwrap(),
        "The **limit** is in `config.rs`.",
    )
    .to(Address::senior(&Name::new("alice").unwrap()))
    .into_event(Seq(9), chrono::Utc::now());
    let raw = render_news(&event, false, &event.body, Locale::En);
    assert!(
        raw.contains("\n  The **limit** is in `config.rs`."),
        "{raw}"
    );
    let plain = senate_markdown::Terminal::new(60, senate_markdown::Color::None);
    let lay_out = |text: &str| format::markdown_for(plain, text);
    let laid = render_news_with(
        &event,
        false,
        &event.body,
        Locale::En,
        Some(&lay_out),
        format::Clock::Local,
    );
    assert!(!laid.contains(" UTC ·"), "{laid}");
    assert!(laid.contains("\n  The limit is in config.rs."), "{laid}");
    assert!(
        laid.contains("read: `senate thread 9 --committee refund`"),
        "{laid}"
    );
}

#[cfg(test)]
#[test]
fn sponsored_commissions_render_sorted_and_mark_unread_completions() {
    use chrono::Utc;
    use senate_core::{CommitteeId, Name, SenatorName, Seq, StatusState};

    let alice = Name::new("alice").unwrap();
    let alice_senator: SenatorName = "alice".parse().unwrap();
    let codex: SenatorName = "codex".parse().unwrap();
    let now = Utc::now();
    let thread = |root: u64, state: StatusState, answer_seq: Option<u64>| {
        let root_seq = Seq(root);
        let updated = Seq(answer_seq.unwrap_or(root + 1));
        ThreadSummary {
            root: root_seq,
            kind: Kind::Request,
            from: Address::senator(&alice_senator),
            on_behalf_of: None,
            to: vec![Address::senator(&codex)],
            agents: vec![codex.clone()],
            title: format!("task {root}"),
            state,
            since: root_seq,
            since_ts: now,
            updated,
            activity: None,
            steps: Vec::new(),
            links: Vec::new(),
            owner: None,
            triage: None,
            escalated_to: None,
            takeover: None,
            reason: None,
            answered: answer_seq.is_some(),
            answered_by: answer_seq.map(|_| Address::senator(&codex)),
            reopened: false,
            escalation: None,
            directed: Vec::new(),
            desk_person: None,
            execution_auth: None,
            origin: None,
            answer_seq: answer_seq.map(Seq),
            answer_line: answer_seq.map(|_| "done".into()),
        }
    };
    let reply = InboxReply {
        committee: CommitteeId::new("refund").unwrap(),
        agents: vec![alice_senator.clone()],
        person: alice,
        threads: vec![
            thread(1, StatusState::Processing, None),
            thread(2, StatusState::Active, Some(10)),
            thread(3, StatusState::Suspended, None),
            thread(4, StatusState::Active, Some(15)),
        ],
        settled: 0,
        seen: vec![crate::ipc::ThreadSeen {
            thread: Seq(4),
            seen: Seq(15),
        }],
        at_desk: Vec::new(),
    };
    let rendered = render_inbox(&reply, Locale::En, ShowAll::Flag);
    let commissions_section = rendered.split("\n\n").next().unwrap();
    assert!(
        commissions_section.contains("My Commissions (refund, 3):"),
        "{rendered}"
    );
    let idx_blocked = commissions_section.find("[blocked] thread 3").unwrap();
    let idx_completed = commissions_section
        .find("[completed (unread)] thread 2")
        .unwrap();
    let idx_progress = commissions_section.find("[in progress] thread 1").unwrap();
    assert!(
        idx_blocked < idx_completed && idx_completed < idx_progress,
        "{rendered}"
    );
    assert!(
        !commissions_section.contains("thread 4"),
        "CompletedRead should be hidden from My Commissions: {rendered}"
    );

    let marks = unread_marks(&reply);
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].thread, Seq(2));
    assert_eq!(marks[0].seen, Seq(10));
}
