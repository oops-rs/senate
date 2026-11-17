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
//! | Written | Means |
//! | --- | --- |
//! | `claude`, `claude/reviewer` | an agent of yours, relative to you |
//! | `alice/claude` | that agent by its full path |
//! | `alice` | that person |
//! | `alice/*` | whichever of alice's agents should get it |
//! | `*` | whichever of your own agents |
//! | `speaker` | the Speaker of the Senate |
//! | `senator::…`, `[senator::…]` | the full address forms, as stored |
//!
//! A name or path that is not spelled as a full address is told apart by
//! whom it names: a first name that is your handle, or is not the name of an
//! agent of yours, is a full path (`alice/claude`) or a person (`alice`);
//! one that is an agent of yours (`claude`, `claude/reviewer`) is relative
//! to you. Your agents are those seated on the committee here, and always
//! `claude` and `codex`, the harnesses' default names. So an agent of yours wins over a person who shares its name;
//! write `senator::<name>` for the person.

use std::path::Path;
use std::time::Duration;

use senate_core::{
    Address, CommitteeId, Event, Kind, Locale, Name, SenatorPath, Seq, StatusPayload,
    is_provisional,
};

use crate::format;
use crate::ipc::{
    self, ClientError, IpcError, Pong, Posted, Request, SayKind, SessionSelector, StatusReply,
    ThreadReply, codes,
};

/// Who is speaking and which agents of theirs are seated here.
#[derive(Debug, Clone)]
pub struct Context<'a> {
    pub me: &'a Name,
    /// The agents this machine seats for `me` (full paths).
    pub mine: &'a [SenatorPath],
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
                    "没有写发给谁：写你的 agent（claude）、完整路径（alice/claude）、某个人（alice）、\
                     alice/*、*、或 speaker。",
                    "name who it is for: one of your agents (claude), a full path (alice/claude), \
                     a person (alice), alice/*, *, or speaker.",
                )
                .to_string(),
            Self::NotReachable(what) => match locale {
                Locale::Zh => format!(
                    "{what} 不是你能从命令行直接写给的对象：写某个 agent、某个人、alice/*、* 或 speaker。"
                ),
                Locale::En => format!(
                    "{what} is not someone you can write to from the command line: name an \
                     agent, a person, alice/*, * or speaker."
                ),
            },
            Self::Malformed(text) => match locale {
                Locale::Zh => format!(
                    "看不懂 `{text}`：写 claude、claude/reviewer、alice/claude、alice、alice/*、*、\
                     speaker，或 senator::… 的完整地址。"
                ),
                Locale::En => format!(
                    "cannot read `{text}`: write claude, claude/reviewer, alice/claude, alice, \
                     alice/*, *, speaker, or a full senator::… address."
                ),
            },
            Self::Yourself => locale
                .pick(
                    "这是你自己；写给你的 agent（例如 claude），或别人。",
                    "that is you; write to one of your agents (for example claude), or to someone else.",
                )
                .to_string(),
        }
    }
}

/// Read `<to>` as the address it names ([module docs](self)).
pub fn parse_target(input: &str, ctx: &Context<'_>) -> Result<Address, TargetError> {
    let text = input.trim();
    let text = text
        .strip_prefix('[')
        .and_then(|t| t.strip_suffix(']'))
        .unwrap_or(text)
        .trim();
    if text.is_empty() {
        return Err(TargetError::Empty);
    }
    let malformed = || TargetError::Malformed(input.trim().to_string());
    let address = if text.contains("::") || text == "speaker" || text == "committee" {
        text.parse::<Address>().map_err(|_| malformed())?
    } else if text == "*" {
        Address::agent_of(ctx.me)
    } else if let Some(owner) = text.strip_suffix("/*") {
        Address::agent_of(&Name::new(owner).map_err(|_| malformed())?)
    } else {
        let names = text
            .split('/')
            .map(|n| Name::new(n).map_err(|_| malformed()))
            .collect::<Result<Vec<_>, _>>()?;
        let path = path_of(names, ctx).map_err(|()| malformed())?;
        Address::senator(&path)
    };
    match &address {
        Address::Committee | Address::Guest(_) | Address::Other(_) => {
            Err(TargetError::NotReachable(address.in_text()))
        }
        a if a.is_senior() && a.as_senator().is_some_and(|p| p.owner() == ctx.me) => {
            Err(TargetError::Yourself)
        }
        _ => Ok(address),
    }
}

/// `names` as a path: relative to the speaker when it starts with one of
/// their agents' names, else a full path (a person's handle first).
fn path_of(names: Vec<Name>, ctx: &Context<'_>) -> Result<SenatorPath, ()> {
    let first = &names[0];
    let relative = first != ctx.me
        && ctx
            .mine
            .iter()
            .any(|a| a.owner() == ctx.me && a.agents().first() == Some(first));
    let (owner, agents) = if first == ctx.me || !relative && names.len() > 1 {
        (names[0].clone(), names[1..].to_vec())
    } else if relative {
        (ctx.me.clone(), names)
    } else {
        // One name that is not an agent of yours: a person.
        (names[0].clone(), Vec::new())
    };
    SenatorPath::new(owner, agents).map_err(|_| ())
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
fn to_phrase(wrote: &Address, posted: &Posted, locale: Locale) -> String {
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
    if posted.to.len() == 1 && &posted.to[0] == wrote {
        return went;
    }
    let wrote = format::mention(wrote);
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
    wrote: &Address,
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
`senate inbox` 找到你，`senate watch` 会实时打印；也可以加 `--wait` 等在这里。";
pub const HEAR_BACK_EN: &str = "You will hear back through a desktop notification (`[notify]` in \
senate.toml), in `senate inbox`, and live in `senate watch`; or add `--wait` to stay here for it.";

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

/// The one line `reply` adds when what the person wrote crossed an
/// escalation to them: it is still in force, and they have not read it.
pub fn unseen_escalation_line(thread: Seq, escalation: Seq, locale: Locale) -> String {
    match locale {
        Locale::Zh => format!(
            "thread {thread} 里有比你已读更新的内容：seq {escalation} 转给了你，仍然有效，你的回复在它之前写成；用 `senate thread {thread}` 阅读。"
        ),
        Locale::En => format!(
            "There is something newer in thread {thread} that you have not read: it was escalated to you at seq {escalation} and still waits on you; read it with `senate thread {thread}`."
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

/// The server refused `*` or `person/*` with the agents to choose from.
pub fn render_ambiguous(error: &IpcError, locale: Locale) -> String {
    let list = error
        .candidates
        .iter()
        .map(|c| Address::senator(c).in_text())
        .collect::<Vec<_>>()
        .join(", ");
    let owner = error
        .candidates
        .first()
        .map(|c| Address::senior(c.owner()).in_text())
        .unwrap_or_default();
    let example = error
        .candidates
        .first()
        .map(ToString::to_string)
        .unwrap_or_else(|| "alice/claude".into());
    match locale {
        Locale::Zh => format!(
            "{owner} 在这里有好几个 agent：{list}。选一个再来，例如 `senate ask {example} …`。"
        ),
        Locale::En => format!(
            "{owner} has several agents here: {list}. Name one and send it again, for example \
             `senate ask {example} …`."
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

/// `senate thread`: each event with its byline, time and body.
pub fn render_thread(reply: &ThreadReply, locale: Locale) -> String {
    let n = reply.events.len();
    let mut out = match locale {
        Locale::Zh => format!("{} 的 thread {}，共 {n} 条：", reply.committee, reply.root),
        Locale::En => format!(
            "Thread {} on {}, {n} event(s):",
            reply.root, reply.committee
        ),
    };
    for e in &reply.events {
        let mut head = format!(
            "\n\n[seq {}] {} · {} · {}",
            e.seq,
            format::author(e, locale),
            e.ts.format("%Y-%m-%d %H:%M UTC"),
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
            body.to_string()
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
    /// The recipient as resolved from what they wrote, before the server
    /// resolved a wildcard.
    pub wrote: Address,
    pub posted: Posted,
    /// The daemon's language: what everything printed is in.
    pub locale: Locale,
    /// The person's own handle.
    pub me: Name,
}

impl Sent {
    /// What was sent, to whom it resolved, and the thread seq.
    pub fn render(&self, waiting: bool) -> String {
        render_sent(self.kind, &self.wrote, &self.posted, waiting, self.locale)
    }
}

/// The daemon's person and language, and the agents of that person seated
/// on `committee`.
async fn who(
    socket: &Path,
    committee: &CommitteeId,
) -> anyhow::Result<(Name, Locale, Vec<SenatorPath>)> {
    let pong: Pong = call(socket, &Request::Ping).await?;
    let status: StatusReply = call(socket, &Request::Status).await?;
    let mut mine: Vec<SenatorPath> = Vec::new();
    for s in std::iter::once(&status).chain(&status.others) {
        for c in s.committees.iter().filter(|c| &c.committee == committee) {
            if !mine.contains(&c.agent) {
                mine.push(c.agent.clone());
            }
        }
    }
    // The two harnesses' default agents are yours even before a session of
    // theirs has opened here.
    for harness in ["claude", "codex"] {
        let agent = SenatorPath::new(pong.person.clone(), vec![Name::new(harness)?])?;
        if !mine.contains(&agent) {
            mine.push(agent);
        }
    }
    Ok((pong.person, pong.locale, mine))
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
    let (me, locale, mine) = who(socket, &committee).await?;
    let wrote = parse_target(
        to,
        &Context {
            me: &me,
            mine: &mine,
        },
    )
    .map_err(|e| anyhow::anyhow!(e.message(locale)))?;
    let posted: Posted = call(
        socket,
        &Request::Say {
            session: SessionSelector::default(),
            committee: Some(committee),
            kind,
            to: vec![wrote.clone()],
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
    fn path(s: &str) -> SenatorPath {
        s.parse().unwrap()
    }

    fn parse(input: &str) -> Result<Address, TargetError> {
        let me = name("alice");
        let mine = [path("alice/claude"), path("alice/codex")];
        parse_target(
            input,
            &Context {
                me: &me,
                mine: &mine,
            },
        )
    }

    fn ok(input: &str) -> String {
        parse(input).unwrap().to_string()
    }

    #[test]
    fn an_agent_of_yours_is_named_relative_to_you() {
        assert_eq!(ok("claude"), "senator::alice/claude");
        assert_eq!(ok("codex"), "senator::alice/codex");
        assert_eq!(ok("claude/reviewer"), "senator::alice/claude/reviewer");
    }

    #[test]
    fn a_full_path_names_that_agent() {
        assert_eq!(ok("alice/claude"), "senator::alice/claude");
        assert_eq!(ok("bob/claude"), "senator::bob/claude");
        assert_eq!(ok("bob/claude/reviewer"), "senator::bob/claude/reviewer");
        // Your own handle first is a full path even for a name you also use
        // for an agent.
        assert_eq!(ok("alice/codex/x"), "senator::alice/codex/x");
    }

    #[test]
    fn a_handle_that_is_not_one_of_your_agents_is_that_person() {
        assert_eq!(ok("bob"), "senator::bob");
        assert_eq!(ok("pm"), "senator::pm");
    }

    #[test]
    fn whichever_agent_is_a_wildcard() {
        assert_eq!(ok("*"), "senator::alice/*");
        assert_eq!(ok("bob/*"), "senator::bob/*");
        assert_eq!(ok("alice/*"), "senator::alice/*");
    }

    #[test]
    fn the_speaker_and_the_full_forms_are_accepted() {
        assert_eq!(ok("speaker"), "speaker");
        assert_eq!(ok("[speaker]"), "speaker");
        assert_eq!(ok("senator::bob/claude"), "senator::bob/claude");
        assert_eq!(ok("[senator::bob/claude]"), "senator::bob/claude");
        assert_eq!(ok("senator::bob"), "senator::bob");
        assert_eq!(ok("senator::bob/*"), "senator::bob/*");
        assert_eq!(ok("  bob/claude "), "senator::bob/claude");
    }

    #[test]
    fn an_agent_of_yours_wins_over_a_person_of_the_same_name() {
        let me = name("alice");
        let mine = [path("alice/bob")];
        let ctx = Context {
            me: &me,
            mine: &mine,
        };
        assert_eq!(
            parse_target("bob", &ctx).unwrap().to_string(),
            "senator::alice/bob"
        );
        assert_eq!(
            parse_target("senator::bob", &ctx).unwrap().to_string(),
            "senator::bob"
        );
    }

    #[test]
    fn nothing_known_about_your_agents_reads_a_single_name_as_a_person() {
        let me = name("alice");
        let ctx = Context { me: &me, mine: &[] };
        assert_eq!(
            parse_target("claude", &ctx).unwrap().to_string(),
            "senator::claude"
        );
        // A path is a full path.
        assert_eq!(
            parse_target("claude/reviewer", &ctx).unwrap().to_string(),
            "senator::claude/reviewer"
        );
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
            "bob/*/x",
            "*/x",
            "senator::",
            "senator::bob/claude/*",
            "has space",
            "a::b",
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
        .to("senator::alice/claude".parse().unwrap())
        .into_event(Seq(4), at);
        let status = NewEvent::new(
            c.clone(),
            Kind::Status,
            "senator::alice/claude".parse().unwrap(),
            "",
        )
        .in_thread(Seq(4))
        .with_meta(
            meta::STATUS,
            StatusPayload::new(StatusState::Processing).value(),
        )
        .into_event(Seq(5), at);
        let answer = NewEvent::new(c.clone(), Kind::Answer, Address::Speaker, "done\n")
            .on_behalf_of("senator::alice/claude".parse().unwrap())
            .in_thread(Seq(4))
            .with_meta(meta::TAKEOVER, true.into())
            .into_event(Seq(6), at);
        assert_eq!(
            render_answer(&answer, Locale::En),
            "[speaker::alice/claude] takeover answered in thread 4:\ndone"
        );
        assert_eq!(
            render_answer(&answer, Locale::Zh),
            "[speaker::alice/claude] 接管 在 thread 4 里回答：\ndone"
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
             [seq 4] [senator::alice] · 2026-11-17 10:02 UTC · question · to [senator::alice/claude]\n  a\n  b\n\n\
             [seq 5] [senator::alice/claude] · 2026-11-17 10:02 UTC · status\n  status processing\n\n\
             [seq 6] [speaker::alice/claude] takeover · 2026-11-17 10:02 UTC · answer\n  done"
        );
        let zh = render_thread(&reply, Locale::Zh);
        assert!(zh.starts_with("refund-v2 的 thread 4，共 3 条："), "{zh}");
        assert!(
            zh.contains("· question · 给 [senator::alice/claude]\n  a\n  b"),
            "{zh}"
        );
        assert!(zh.contains("\n  状态 processing"), "{zh}");
        assert!(zh.contains("[speaker::alice/claude] 接管 · "), "{zh}");
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
