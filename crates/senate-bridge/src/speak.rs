//! The person's own voice on the command line: `senate ask`, `tell`,
//! `request`, `reply` and `thread`. They speak as the person, never as an
//! agent, through the daemon's `say` and `thread` requests ([`crate::ipc`]).
//!
//! This module is the part with no I/O: how `<to>` is read ([`parse_target`])
//! and what each outcome says, in either language, with ids as bracketed
//! mentions.
//!
//! # `<to>`
//!
//! A qualified name ([`Locator`]), in its short form: a bare last name is a
//! senator.
//!
//! | Written | Means |
//! | --- | --- |
//! | `claude` | `senator::claude`: that senator, here (this committee) |
//! | `*` | `senator::*`: whichever agent on this committee |
//! | `speaker` | the Speaker |
//! | `ledger::claude` | `senator::claude` on the committee `ledger` (the nearest Senate that has it) |
//! | `ledger::speaker` | the Speaker of `ledger` |
//! | `/acme::ledger::claude` | that seat in the Senate `/acme` |
//! | `senator::…`, `[…]` | the full forms, as stored or bracketed |
//!
//! A name is one flat senator name: a person's handle or an agent's name,
//! the same namespace. A person's message is delivered directly, on the
//! committee named, by the daemon ([`crate::ipc::Request::Say`]).

use std::path::Path;
use std::time::Duration;

use senate_core::{
    Address, CommitteeId, Event, Kind, Locale, Locator, Name, Seq, StatusPayload, is_provisional,
};

use crate::format;
use crate::ipc::{
    self, ClientError, IpcError, Pong, Posted, Request, SayKind, SessionSelector, ThreadReply,
    codes,
};

/// Who is speaking.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    pub me: &'a Name,
}

/// Why a `<to>` was not accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetError {
    Empty,
    /// `committee` or a guest: not someone a person writes to from here.
    NotReachable(String),
    /// Not a name, a path or an address form.
    Malformed(String),
    /// It names the speaker themself.
    Yourself,
}

impl TargetError {
    pub fn message(&self, locale: Locale) -> String {
        match self {
            Self::Empty => locale
                .pick(
                    "没有写发给谁：写某个 senator（claude、bob）、*、speaker，或带委员会的名字\
                     （ledger::claude）。",
                    "name who it is for: a senator (claude, bob), *, speaker, or a name on \
                     another committee (ledger::claude).",
                )
                .to_string(),
            Self::NotReachable(what) => match locale {
                Locale::Zh => {
                    format!("{what} 不是你能从命令行直接写给的对象：写某个 senator、* 或 speaker。")
                }
                Locale::En => format!(
                    "{what} is not someone you can write to from the command line: name a \
                     senator, * or speaker."
                ),
            },
            Self::Malformed(text) => match locale {
                Locale::Zh => format!(
                    "看不懂 `{text}`：写 claude、bob、*、speaker、ledger::claude，或 senator::… \
                     的完整地址。名字里没有路径：一个 agent 自己的团队是一个子 Senate。"
                ),
                Locale::En => format!(
                    "cannot read `{text}`: write claude, bob, *, speaker, ledger::claude, or a \
                     full senator::… address. A name holds no path: an agent's own team is a \
                     child Senate."
                ),
            },
            Self::Yourself => locale
                .pick(
                    "这是你自己；写给某个 agent（例如 claude），或别人。",
                    "that is you; write to an agent (for example claude), or to someone else.",
                )
                .to_string(),
        }
    }
}

/// Read `<to>` as the qualified name it names ([module docs](self)).
pub fn parse_target(input: &str, ctx: &Context<'_>) -> Result<Locator, TargetError> {
    let text = input.trim();
    let text = text
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .unwrap_or(text)
        .trim();
    if text.is_empty() {
        return Err(TargetError::Empty);
    }
    let locator =
        Locator::parse_short(text).map_err(|_| TargetError::Malformed(input.trim().to_string()))?;
    match locator.address() {
        Address::Committee | Address::Guest(_) | Address::Other(_) => {
            Err(TargetError::NotReachable(locator.address().in_text()))
        }
        Address::Senator(name) if locator.is_here() && name == ctx.me => Err(TargetError::Yourself),
        _ => Ok(locator),
    }
}

fn word(kind: SayKind) -> &'static str {
    match kind {
        SayKind::Question => "question",
        SayKind::Note => "note",
        SayKind::Request => "request",
    }
}

/// Where a message went, set beside what the person wrote when the server
/// resolved it (`*` to one agent, or to the Speaker to pick).
fn to_phrase(wrote: &Locator, posted: &Posted, locale: Locale) -> String {
    let went = if posted.to.is_empty() {
        locale.pick("所有人", "no one in particular").to_string()
    } else {
        posted
            .to
            .iter()
            .map(format::mention)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let here = wrote.as_address();
    if posted.to.len() == 1 && here.as_ref() == Some(&posted.to[0]) {
        return went;
    }
    if wrote.committee().is_some_and(|c| c == &posted.committee) && wrote.senate().is_none() {
        return went;
    }
    let wrote = wrote.in_text();
    if posted.to == [Address::Speaker] && wrote != "[speaker]" {
        return match locale {
            Locale::Zh => format!("{went}（你写的是 {wrote}；由 Speaker 挑选哪个 agent）"),
            Locale::En => format!("{went} (you wrote {wrote}; the Speaker picks which agent)"),
        };
    }
    match locale {
        Locale::Zh => format!("{went}（你写的是 {wrote}）"),
        Locale::En => format!("{went} (you wrote {wrote})"),
    }
}

/// What `ask`, `tell` and `request` print once the message is stored.
/// `waiting`: the command stays for the answer, so it does not say how to
/// read it later.
pub fn render_sent(
    kind: SayKind,
    wrote: &Locator,
    posted: &Posted,
    waiting: bool,
    locale: Locale,
) -> String {
    let c = &posted.committee;
    let seq = posted.seq;
    let to = to_phrase(wrote, posted, locale);
    let k = word(kind);
    let mut out = match (locale, kind) {
        (Locale::Zh, SayKind::Note) => format!("已发出 {k}（seq {seq}）到 {c}，给 {to}。"),
        (Locale::Zh, _) => format!("已发出 {k} 到 {c}：thread {seq}，给 {to}。"),
        (Locale::En, SayKind::Note) => format!("Sent a {k} on {c} (seq {seq}) to {to}."),
        (Locale::En, _) => format!("Sent a {k} on {c}: thread {seq}, to {to}."),
    };
    if posted.duplicate {
        out.push_str(locale.pick(" 这条之前已经发出过。", " It had been sent already."));
    }
    if kind != SayKind::Note && !waiting {
        out.push('\n');
        out.push_str(&match locale {
            Locale::Zh => format!(
                "用 `senate thread {seq}` 看回答；用 `senate reply {seq} <文字>` 接着说。\n{HEAR_BACK_ZH}"
            ),
            Locale::En => format!(
                "Read the answer with `senate thread {seq}`; follow up with `senate reply {seq} <text>`.\n{HEAR_BACK_EN}"
            ),
        });
    }
    out
}

/// How the person hears back, in the words `ask`, `request` and the session
/// start use.
pub const HEAR_BACK_ZH: &str = "回复和转给你的事项会通过桌面通知（`senate.toml` 的 `[notify]`）、\
`senate inbox` 找到你，`senate inbox --follow` 会实时打印；也可以加 `--wait` 等在这里。";
pub const HEAR_BACK_EN: &str = "You will hear back through a desktop notification (`[notify]` in \
senate.toml), in `senate inbox`, and live in `senate inbox --follow`; or add `--wait` to stay here for it.";

/// What `reply` prints.
pub fn render_replied(thread: Seq, posted: &Posted, locale: Locale) -> String {
    let c = &posted.committee;
    let seq = posted.seq;
    let to = if posted.to.is_empty() {
        locale.pick("thread 里的人", "the thread").to_string()
    } else {
        posted
            .to
            .iter()
            .map(format::mention)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = match locale {
        Locale::Zh => format!("已在 {c} 的 thread {thread} 里回复（seq {seq}），给 {to}。"),
        Locale::En => format!("Replied in thread {thread} on {c} (seq {seq}), to {to}."),
    };
    if let Some(esc) = posted.unseen_escalation {
        out.push('\n');
        out.push_str(&unseen_escalation_line(thread, esc, locale));
    }
    out
}

/// The one line `reply` adds, after its confirmation, when what the person
/// wrote crossed an escalation to them: the reply is recorded, and this is
/// for their information, not a refusal. The escalation is still in force
/// and they have not read it.
pub fn unseen_escalation_line(thread: Seq, escalation: Seq, locale: Locale) -> String {
    match locale {
        Locale::Zh => format!(
            "注意：thread {thread} 有你还没读的 seq {escalation}（转给你的，仍有效）；用 `senate thread {thread}` 查看。"
        ),
        Locale::En => format!(
            "Note: thread {thread} has seq {escalation}, which you have not read yet (escalated to you, still in force); see it with `senate thread {thread}`."
        ),
    }
}

/// The answer `ask --wait` was waiting for, with its byline.
pub fn render_answer(event: &Event, locale: Locale) -> String {
    let root = event.thread.unwrap_or(event.seq);
    let by = format::author(event, locale);
    let head = match locale {
        Locale::Zh => format!("{by} 在 thread {root} 里回答："),
        Locale::En => format!("{by} answered in thread {root}:"),
    };
    format!("{head}\n{}", event.body.trim_end())
}

/// What `--wait` was waiting for: an answer, or the thread escalated back
/// to the asker (with its reason).
pub fn render_arrival(event: &Event, locale: Locale) -> String {
    let Some(status) = StatusPayload::of(event) else {
        return render_answer(event, locale);
    };
    let root = event.thread.unwrap_or(event.seq);
    let by = format::author(event, locale);
    let reason = status
        .reason
        .as_deref()
        .map(str::trim_end)
        .filter(|r| !r.is_empty())
        .map_or_else(
            || {
                locale
                    .pick("（没有写原因）", "(no reason given)")
                    .to_string()
            },
            str::to_string,
        );
    match locale {
        Locale::Zh => format!(
            "{by} 把 thread {root} 转给了你：\n{reason}\n用 `senate reply {root} <文字>` 回复。"
        ),
        Locale::En => format!(
            "{by} escalated thread {root} to you:\n{reason}\nReply with `senate reply {root} <text>`."
        ),
    }
}

/// `--wait` ran out.
pub fn render_timeout(thread: Seq, secs: u64, locale: Locale) -> String {
    match locale {
        Locale::Zh => format!(
            "thread {thread} 里 {secs} 秒内没有回答；它还开着，用 `senate thread {thread}` 稍后再看。"
        ),
        Locale::En => format!(
            "no answer in thread {thread} after {secs}s; it is still open: look again with \
             `senate thread {thread}`."
        ),
    }
}

/// The server refused `*` with the agents to choose from.
pub fn render_ambiguous(error: &IpcError, locale: Locale) -> String {
    let list = error
        .candidates
        .iter()
        .map(|c| Address::senator(c).in_text())
        .collect::<Vec<_>>()
        .join(", ");
    let example = error
        .candidates
        .first()
        .map(ToString::to_string)
        .unwrap_or_else(|| "claude".into());
    let any = Address::any_agent().in_text();
    match locale {
        Locale::Zh => format!(
            "{any} 在这个委员会上有好几个 agent：{list}。选一个再来，例如 `senate ask {example} …`。"
        ),
        Locale::En => format!(
            "{any} could reach several agents on this committee: {list}. Name one and send it \
             again, for example `senate ask {example} …`."
        ),
    }
}

fn one_status(event: &Event, locale: Locale) -> Option<String> {
    let status = StatusPayload::of(event)?;
    let mut line = format!("{} {}", locale.pick("状态", "status"), status.state);
    if let Some(title) = &status.title {
        line.push_str(&format!(" · {}", format::one_line(title)));
    }
    if let Some(reason) = &status.reason {
        line.push_str(&format!(" · {}", format::one_line(reason)));
    }
    Some(line)
}

/// `senate thread`: each event with its byline, time and body, the body as
/// written (what is piped).
pub fn render_thread(reply: &ThreadReply, locale: Locale) -> String {
    render_thread_with(reply, locale, &|body| body.to_owned(), format::Clock::Utc)
}

/// [`render_thread`] with each body laid out by `body` first (Markdown for a
/// terminal: [`crate::format::terminal_markdown`]) and times on `clock`
/// (its zone named once, in the first line).
pub fn render_thread_with(
    reply: &ThreadReply,
    locale: Locale,
    lay_out: &dyn Fn(&str) -> String,
    clock: format::Clock,
) -> String {
    let n = reply.events.len();
    let zone = clock.zone();
    let mut out = match (locale, &zone) {
        (Locale::Zh, None) => format!("{} 的 thread {}，共 {n} 条：", reply.committee, reply.root),
        (Locale::Zh, Some(zone)) => format!(
            "{} 的 thread {}，共 {n} 条（本地时间，{zone}）：",
            reply.committee, reply.root
        ),
        (Locale::En, None) => format!(
            "Thread {} on {}, {n} event(s):",
            reply.root, reply.committee
        ),
        (Locale::En, Some(zone)) => format!(
            "Thread {} on {}, {n} event(s), local times ({zone}):",
            reply.root, reply.committee
        ),
    };
    for e in &reply.events {
        let mut head = format!(
            "\n\n[seq {}] {} · {} · {}",
            e.seq,
            format::author(e, locale),
            clock.stamp(e.ts),
            e.kind
        );
        if !e.to.is_empty() && e.kind != Kind::Status {
            let to =
                e.to.iter()
                    .map(format::mention)
                    .collect::<Vec<_>>()
                    .join(", ");
            head.push_str(&match locale {
                Locale::Zh => format!(" · 给 {to}"),
                Locale::En => format!(" · to {to}"),
            });
        }
        out.push_str(&head);
        let body = e.body.trim_end();
        let text = if body.is_empty() {
            one_status(e, locale).unwrap_or_default()
        } else {
            lay_out(body)
        };
        for line in text.lines() {
            out.push_str("\n  ");
            out.push_str(line);
        }
    }
    out
}

/// How often `--wait` looks at the thread.
pub const POLL: Duration = Duration::from_millis(250);

async fn call<T: serde::de::DeserializeOwned>(
    socket: &Path,
    request: &Request,
) -> Result<T, ClientError> {
    ipc::request(
        socket,
        request,
        Duration::from_secs(1),
        Duration::from_secs(20),
    )
    .await
}

/// A message the person sent.
#[derive(Debug, Clone, PartialEq)]
pub struct Sent {
    pub kind: SayKind,
    /// The recipient as the person wrote it, before the daemon and the
    /// server resolved it.
    pub wrote: Locator,
    pub posted: Posted,
    /// The daemon's language: what everything printed is in.
    pub locale: Locale,
    /// The person's own handle.
    pub me: Name,
    /// The committee is not the one this directory binds: the commands
    /// suggested name it with `--committee`.
    pub elsewhere: bool,
}

impl Sent {
    /// What was sent, to whom it resolved, and the thread seq.
    pub fn render(&self, waiting: bool) -> String {
        let mut out = render_sent(self.kind, &self.wrote, &self.posted, waiting, self.locale);
        if self.elsewhere {
            let seq = self.posted.seq;
            let flag = format!(" --committee {}", self.posted.committee);
            out = out
                .replace(
                    &format!("`senate thread {seq}`"),
                    &format!("`senate thread {seq}{flag}`"),
                )
                .replace(
                    &format!("`senate reply {seq} "),
                    &format!("`senate reply{flag} {seq} "),
                );
        }
        out
    }
}

/// The daemon's person and language.
async fn who(socket: &Path) -> anyhow::Result<(Name, Locale)> {
    let pong: Pong = call(socket, &Request::Ping).await?;
    Ok((pong.person, pong.locale))
}

/// A refusal from the daemon, written for the person: `ambiguous_recipient`
/// lists the agents and says how to name one.
fn refusal(error: ClientError, locale: Locale) -> anyhow::Error {
    match error {
        ClientError::Remote(e) if e.code == codes::AMBIGUOUS_RECIPIENT => {
            anyhow::anyhow!(render_ambiguous(&e, locale))
        }
        ClientError::Remote(e) => anyhow::anyhow!(e.message),
        other => anyhow::anyhow!(other),
    }
}

/// Say `text` as the person to `to` (see [`parse_target`]) in a new thread:
/// the request `senate ask`, `tell` and `request` send.
pub async fn send(
    socket: &Path,
    committee: CommitteeId,
    kind: SayKind,
    to: &str,
    text: String,
) -> anyhow::Result<Sent> {
    let (me, locale) = who(socket).await?;
    let wrote =
        parse_target(to, &Context { me: &me }).map_err(|e| anyhow::anyhow!(e.message(locale)))?;
    let (to, at) = match wrote.as_address() {
        Some(address) => (vec![address], None),
        None => (Vec::new(), Some(wrote.clone())),
    };
    let posted: Posted = call(
        socket,
        &Request::Say {
            session: SessionSelector::default(),
            committee: Some(committee),
            kind,
            to,
            at,
            body: text,
            thread: None,
            id: Some(senate_core::EventId::new()),
        },
    )
    .await
    .map_err(|e| refusal(e, locale))?;
    Ok(Sent {
        kind,
        wrote,
        posted,
        locale,
        me,
        elsewhere: false,
    })
}

/// Say `text` as the person in the thread `thread` is in (`senate reply`):
/// a `note` to whoever took part in it.
pub async fn reply(
    socket: &Path,
    committee: CommitteeId,
    thread: Seq,
    text: String,
) -> anyhow::Result<(Posted, Locale)> {
    let pong: Pong = call(socket, &Request::Ping).await?;
    let posted: Posted = call(
        socket,
        &Request::Say {
            session: SessionSelector::default(),
            committee: Some(committee),
            kind: SayKind::Note,
            to: Vec::new(),
            at: None,
            body: text,
            thread: Some(thread),
            id: Some(senate_core::EventId::new()),
        },
    )
    .await
    .map_err(|e| refusal(e, pong.locale))?;
    Ok((posted, pong.locale))
}

/// The events of a thread (`senate thread`), and the daemon's language.
pub async fn thread(
    socket: &Path,
    committee: CommitteeId,
    thread: Seq,
) -> anyhow::Result<(ThreadReply, Locale)> {
    let pong: Pong = call(socket, &Request::Ping).await?;
    let reply: ThreadReply = call(
        socket,
        &Request::Thread {
            session: SessionSelector::default(),
            committee: Some(committee),
            thread,
            // Showing a thread is reading it.
            mark_seen: true,
        },
    )
    .await
    .map_err(|e| refusal(e, pong.locale))?;
    Ok((reply, pong.locale))
}

/// A simulated chat conversation (`senate chat thread`) by any of its
/// refs. With `wait`, polled until the Speaker has posted something after
/// the first `since` entries (default: through the latest message said),
/// or until `timeout`; the flag says whether it did.
pub async fn chat_thread(
    socket: &Path,
    committee: CommitteeId,
    message_ref: String,
    wait: Option<(Option<usize>, Duration)>,
) -> Result<(senate_core::chat_sim::SimTranscript, bool), ClientError> {
    let request = Request::ChatThread {
        session: SessionSelector::default(),
        committee: Some(committee),
        message_ref,
    };
    let mut transcript: senate_core::chat_sim::SimTranscript = call(socket, &request).await?;
    let Some((since, timeout)) = wait else {
        return Ok((transcript, true));
    };
    let since = since.unwrap_or_else(|| transcript.through_last_inbound());
    let deadline = tokio::time::Instant::now() + timeout;
    while transcript.speaker_posts_after(since).next().is_none() {
        if tokio::time::Instant::now() >= deadline {
            return Ok((transcript, false));
        }
        tokio::time::sleep(CHAT_POLL).await;
        transcript = call(socket, &request).await?;
    }
    Ok((transcript, true))
}

const CHAT_POLL: Duration = Duration::from_millis(250);

/// The first thing that comes back in the thread `sent` started: an answer
/// someone else wrote (a provisional one only says where the question
/// stands) or the thread escalated to the person who asked, or `None` when
/// `timeout` passes first. What is returned counts as seen ([`mark_seen`]).
pub async fn wait_answer(
    socket: &Path,
    sent: &Sent,
    timeout: Duration,
) -> anyhow::Result<Option<Event>> {
    let deadline = tokio::time::Instant::now() + timeout;
    let me = Address::senior(&sent.me);
    loop {
        let reply: ThreadReply = call(
            socket,
            &Request::Thread {
                session: SessionSelector::default(),
                committee: Some(sent.posted.committee.clone()),
                thread: sent.posted.seq,
                mark_seen: false,
            },
        )
        .await
        .map_err(|e| refusal(e, sent.locale))?;
        let back = reply.events.into_iter().find(|e| {
            e.seq > sent.posted.seq
                && match e.kind {
                    Kind::Answer => !is_provisional(e) && e.from != me,
                    Kind::Status => StatusPayload::of(e).is_some_and(|s| {
                        s.escalated_to.as_ref() == Some(&sent.me) && s.takeover.is_none()
                    }),
                    _ => false,
                }
        });
        if let Some(event) = back {
            mark_seen(socket, sent, &event).await;
            return Ok(Some(event));
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(POLL.min(deadline - tokio::time::Instant::now())).await;
    }
}

/// What the person was just shown counts as read. Best effort: not being
/// able to say so changes nothing about what was shown.
async fn mark_seen(socket: &Path, sent: &Sent, shown: &Event) {
    let mut marks = vec![ipc::ThreadSeen {
        thread: sent.posted.seq,
        seen: shown.seq,
    }];
    if let Some(root) = shown.thread.filter(|t| *t != sent.posted.seq) {
        marks.push(ipc::ThreadSeen {
            thread: root,
            seen: shown.seq,
        });
    }
    let seen: Result<serde_json::Value, _> = call(
        socket,
        &Request::Seen {
            committee: sent.posted.committee.clone(),
            marks,
        },
    )
    .await;
    if let Err(e) = seen {
        tracing::debug!("could not mark the thread read: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn parse(input: &str) -> Result<Locator, TargetError> {
        let me = name("alice");
        parse_target(input, &Context { me: &me })
    }

    fn ok(input: &str) -> String {
        parse(input).unwrap().to_string()
    }

    #[test]
    fn a_bare_name_is_a_senator_here() {
        // Replaces "an agent of yours is named relative to you", "a handle
        // that is not one of your agents is that person" and "an agent of
        // yours wins over a person of the same name": a name is flat, and a
        // person's handle and an agent's name share one namespace.
        assert_eq!(ok("claude"), "senator::claude");
        assert_eq!(ok("codex"), "senator::codex");
        assert_eq!(ok("bob"), "senator::bob");
        assert_eq!(ok("pm"), "senator::pm");
    }

    #[test]
    fn a_path_in_a_name_is_refused() {
        // Replaces "a full path names that agent": no owner prefix, no
        // nesting.
        for bad in ["alice/claude", "bob/claude", "claude/reviewer", "bob/*"] {
            assert!(
                matches!(parse(bad), Err(TargetError::Malformed(_))),
                "{bad}"
            );
        }
        let text = TargetError::Malformed("claude/reviewer".into()).message(Locale::En);
        assert!(text.contains("child Senate"), "{text}");
    }

    #[test]
    fn whichever_agent_is_a_wildcard() {
        assert_eq!(ok("*"), "senator::*");
        assert_eq!(ok("senator::*"), "senator::*");
    }

    #[test]
    fn a_name_on_another_committee_is_qualified() {
        assert_eq!(ok("ledger::claude"), "ledger::senator::claude");
        assert_eq!(ok("ledger::speaker"), "ledger::speaker");
        assert_eq!(ok("ledger::*"), "ledger::senator::*");
        assert_eq!(ok("acme::ledger::codex"), "acme::ledger::senator::codex");
        assert_eq!(ok("[ledger::senator::codex]"), "ledger::senator::codex");
        assert!(parse("ledger::claude").unwrap().as_address().is_none());
        // Yourself elsewhere is someone else's committee: not refused here.
        assert_eq!(ok("ledger::alice"), "ledger::senator::alice");
    }

    #[test]
    fn the_speaker_and_the_full_forms_are_accepted() {
        assert_eq!(ok("speaker"), "speaker");
        assert_eq!(ok("[speaker]"), "speaker");
        assert_eq!(ok("senator::codex"), "senator::codex");
        assert_eq!(ok("[senator::codex]"), "senator::codex");
        assert_eq!(ok("senator::bob"), "senator::bob");
        assert_eq!(ok("  codex "), "senator::codex");
    }

    #[test]
    fn what_cannot_be_written_to_is_refused() {
        assert_eq!(parse(""), Err(TargetError::Empty));
        assert_eq!(parse("  [ ] "), Err(TargetError::Empty));
        assert_eq!(
            parse("committee"),
            Err(TargetError::NotReachable("[committee]".into()))
        );
        assert_eq!(
            parse("guest::feed/x"),
            Err(TargetError::NotReachable("[guest::feed/x]".into()))
        );
        assert_eq!(parse("alice"), Err(TargetError::Yourself));
        assert_eq!(parse("senator::alice"), Err(TargetError::Yourself));
        for bad in [
            "Bob",
            "bob/",
            "/claude",
            "bob//claude",
            "*/x",
            "senator::",
            "senator::bob/claude/*",
            "has space",
            "a::b::c::d",
        ] {
            assert!(
                matches!(parse(bad), Err(TargetError::Malformed(_))),
                "{bad}: {:?}",
                parse(bad)
            );
        }
    }

    #[test]
    fn an_answer_and_a_thread_read_in_both_languages() {
        use chrono::{TimeZone, Utc};
        use senate_core::{CommitteeId, NewEvent, StatusState, meta};

        let c = CommitteeId::new("refund-v2").unwrap();
        let at = Utc.with_ymd_and_hms(2026, 11, 17, 10, 2, 0).unwrap();
        let question = NewEvent::new(
            c.clone(),
            Kind::Question,
            "senator::alice".parse().unwrap(),
            "a\nb",
        )
        .to("senator::claude".parse().unwrap())
        .into_event(Seq(4), at);
        let status = NewEvent::new(
            c.clone(),
            Kind::Status,
            "senator::claude".parse().unwrap(),
            "",
        )
        .in_thread(Seq(4))
        .with_meta(
            meta::STATUS,
            StatusPayload::new(StatusState::Processing).value(),
        )
        .into_event(Seq(5), at);
        let answer = NewEvent::new(c.clone(), Kind::Answer, Address::Speaker, "done\n")
            .on_behalf_of("senator::claude".parse().unwrap())
            .in_thread(Seq(4))
            .with_meta(meta::TAKEOVER, true.into())
            .into_event(Seq(6), at);
        assert_eq!(
            render_answer(&answer, Locale::En),
            "[speaker::claude] takeover answered in thread 4:\ndone"
        );
        assert_eq!(
            render_answer(&answer, Locale::Zh),
            "[speaker::claude] 接管 在 thread 4 里回答：\ndone"
        );
        let reply = ThreadReply {
            committee: c,
            root: Seq(4),
            events: vec![question, status, answer],
        };
        let en = render_thread(&reply, Locale::En);
        assert_eq!(
            en,
            "Thread 4 on refund-v2, 3 event(s):\n\n\
             [seq 4] [senator::alice] · 2026-11-17 10:02 UTC · question · to [senator::claude]\n  a\n  b\n\n\
             [seq 5] [senator::claude] · 2026-11-17 10:02 UTC · status\n  status processing\n\n\
             [seq 6] [speaker::claude] takeover · 2026-11-17 10:02 UTC · answer\n  done"
        );
        let zh = render_thread(&reply, Locale::Zh);
        assert!(zh.starts_with("refund-v2 的 thread 4，共 3 条："), "{zh}");
        assert!(
            zh.contains("· question · 给 [senator::claude]\n  a\n  b"),
            "{zh}"
        );
        assert!(zh.contains("\n  状态 processing"), "{zh}");
        assert!(zh.contains("[speaker::claude] 接管 · "), "{zh}");

        // On a terminal the bodies are laid out as Markdown; piped, the text
        // above stays as it was written.
        let md = ThreadReply {
            events: vec![
                NewEvent::new(
                    reply.committee.clone(),
                    Kind::Question,
                    Address::senior(&senate_core::Name::new("alice").unwrap()),
                    "Is the **retry** limit in `config.rs`?\n\n- one\n- two",
                )
                .into_event(Seq(7), at),
            ],
            ..reply.clone()
        };
        assert!(
            render_thread(&md, Locale::En).contains("\n  Is the **retry** limit in `config.rs`?")
        );
        let plain = senate_markdown::Terminal::new(40, senate_markdown::Color::None);
        let laid = render_thread_with(
            &md,
            Locale::En,
            &|body| crate::format::markdown_for(plain, body),
            format::Clock::Utc,
        );
        assert!(
            laid.contains("\n  Is the retry limit in config.rs?"),
            "{laid}"
        );
        assert!(
            laid.contains("\n  - one\n  - two") || laid.contains("\n  • one\n  • two"),
            "{laid}"
        );
        let ansi = senate_markdown::Terminal::new(40, senate_markdown::Color::Ansi16);
        let coloured = render_thread_with(
            &md,
            Locale::En,
            &|body| crate::format::markdown_for(ansi, body),
            format::Clock::Utc,
        );
        assert!(coloured.contains("\u{1b}[1mretry"), "{coloured}");
        // Local times: the zone once, in the first line, none on each event.
        let local = render_thread_with(&md, Locale::En, &|b| b.to_owned(), format::Clock::Local);
        let first = local.lines().next().unwrap();
        assert!(first.contains(", local times (UTC"), "{local}");
        assert!(!local.contains(" UTC ·"), "{local}");
    }

    #[test]
    fn every_refusal_reads_in_both_languages() {
        for e in [
            TargetError::Empty,
            TargetError::NotReachable("[committee]".into()),
            TargetError::Malformed("Bob".into()),
            TargetError::Yourself,
        ] {
            assert_ne!(e.message(Locale::En), e.message(Locale::Zh));
        }
    }
}

#[cfg(test)]
#[test]
fn a_message_sent_to_another_committee_names_it_in_the_commands_it_suggests() {
    let posted = Posted {
        committee: CommitteeId::new("refund").unwrap(),
        seq: senate_core::Seq(35),
        to: Vec::new(),
        thread: None,
        duplicate: false,
        intended: None,
        unseen_escalation: None,
    };
    let sent = |elsewhere| Sent {
        kind: SayKind::Question,
        wrote: "senator::rev".parse().unwrap(),
        posted: posted.clone(),
        locale: Locale::En,
        me: Name::new("alice").unwrap(),
        elsewhere,
    };
    let here = sent(false).render(false);
    assert!(
        here.contains("`senate thread 35`; follow up with `senate reply 35 <text>`"),
        "{here}"
    );
    let there = sent(true).render(false);
    assert!(
        there.contains(
            "`senate thread 35 --committee refund`; follow up with `senate reply --committee refund 35 <text>`"
        ),
        "{there}"
    );
}
