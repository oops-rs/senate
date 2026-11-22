//! The fixed syntax: always understood, model or no model.
//!
//! `/bind <committee>`, `/unbind`, `/tell <name> <text>` (a person or an agent),
//! `/status`, `/decide [--supersedes <seq>] <text>`, `/brief`, `/agents [<committee>]` (this group
//! is the committee's Agent group), `/invite <handle>…` (into the Agent
//! group), `/output answers|status|all` (the Agent group's output level),
//! `/work <name> <instruction>` (coding work from that agent's
//! checkpoint), `/reassign <from> <to>` (hand an agent's work to
//! another), and `/memory [<words>]`, `/forget <seq>…`, `/correct <seq>
//! <text>` (what the committee knows). It is parsed before the Brain is consulted, so a command never
//! costs a model call, and it is what the Speaker points people at when the
//! Brain fails.
//!
//! Commands and the short words that answer the Speaker (a yes to a
//! restatement, handing a parked thread to one's agent, which agent was
//! meant) are accepted in Chinese and in English whatever the committee's
//! language: people type what comes naturally, and a mixed team should not
//! have to remember which language a group is set to. Only what the Speaker
//! writes back follows the locale.

use senate_core::{Locale, OutputLevel, SenatorName, Seq};

use crate::brain::{Intent, RelayKind};

/// The one-line reminder of the fixed syntax.
pub(crate) fn usage(locale: Locale) -> &'static str {
    locale.pick(
        "固定写法：/tell <name> <内容>、/decide [--supersedes <seq>] <决策>、/brief、/memory [<关键词>]、/forget <seq>、/correct <seq> <内容>、/bind <committee>、/unbind、/status、/agents [<committee>]、/invite <handle>、/output answers|status|all、/work <name> <要做的事>、/reassign <from> <to>",
        "Commands: /tell <name> <text>, /decide [--supersedes <seq>] <decision>, /brief, /memory [<words>], /forget <seq>, /correct <seq> <text>, /bind <committee>, /unbind, /status, /agents [<committee>], /invite <handle>, /output answers|status|all, /work <name> <what to do>, /reassign <from> <to>",
    )
}

/// `用法：/bind <committee>。<usage>` / `Usage: /bind <committee>. <usage>`.
fn wrong_use(locale: Locale, form: &str) -> String {
    match locale {
        Locale::Zh => format!("用法：{form}。{}", usage(locale)),
        Locale::En => format!("Usage: {form}. {}", usage(locale)),
    }
}

/// `None` when `text` is not a command at all; `Some(Err(usage))` when it is
/// a command used wrongly, the usage written in `locale`.
pub(crate) fn parse_command(text: &str, locale: Locale) -> Option<Result<Intent, String>> {
    let text = text.trim();
    let rest = text.strip_prefix('/').or_else(|| text.strip_prefix('／'))?;
    let (verb, args) = match rest.find(char::is_whitespace) {
        Some(at) => (&rest[..at], rest[at..].trim()),
        None => (rest, ""),
    };
    let l = locale;
    Some(match verb.to_lowercase().as_str() {
        "bind" if !args.is_empty() && !args.contains(char::is_whitespace) => Ok(Intent::Bind {
            committee: args.to_owned(),
        }),
        "bind" => Err(wrong_use(l, "/bind <committee>")),
        "unbind" => Ok(Intent::Unbind),
        "status" => Ok(Intent::Status),
        "brief" => Ok(Intent::Recall),
        "memory" | "记忆" => Ok(Intent::Remember {
            query: args.to_owned(),
        }),
        "forget" | "忘掉" => {
            let targets: Option<Vec<Seq>> = args
                .split(|c: char| c.is_whitespace() || c == ',' || c == '，')
                .filter(|w| !w.is_empty())
                .map(|w| w.trim_start_matches('#').parse().ok())
                .collect();
            match targets {
                Some(targets) if !targets.is_empty() => Ok(Intent::Forget { targets }),
                _ => Err(wrong_use(l, "/forget <seq>…")),
            }
        }
        "correct" | "更正" => match args.split_once(char::is_whitespace) {
            Some((seq, text)) if !text.trim().is_empty() => {
                match seq.trim_start_matches('#').parse() {
                    Ok(target) => Ok(Intent::Correct {
                        target,
                        text: text.trim().to_owned(),
                    }),
                    Err(_) => Err(wrong_use(
                        l,
                        l.pick("/correct <seq> <内容>", "/correct <seq> <text>"),
                    )),
                }
            }
            _ => Err(wrong_use(
                l,
                l.pick("/correct <seq> <内容>", "/correct <seq> <text>"),
            )),
        },
        "decide" => parse_decision(args).ok_or_else(|| {
            wrong_use(
                l,
                l.pick(
                    "/decide [--supersedes <seq>] <决策>",
                    "/decide [--supersedes <seq>] <decision>",
                ),
            )
        }),
        "agents" if !args.contains(char::is_whitespace) => Ok(Intent::AdoptAgentGroup {
            committee: (!args.is_empty()).then(|| args.to_owned()),
        }),
        "agents" => Err(wrong_use(l, "/agents [<committee>]")),
        "invite" => Ok(Intent::InviteToAgentGroup {
            who: args
                .split_whitespace()
                .map(|w| w.trim_start_matches('@').to_owned())
                .filter(|w| !w.is_empty())
                .collect(),
        }),
        "output" => match output_level(args) {
            Some(level) => Ok(Intent::SetOutputLevel { level }),
            None => Err(wrong_use(l, "/output answers|status|all")),
        },
        "work" => match args.find(char::is_whitespace) {
            Some(at) if !args[at..].trim().is_empty() => Ok(Intent::Work {
                target: args[..at].trim_start_matches('@').to_owned(),
                instruction: args[at..].trim().to_owned(),
            }),
            _ => Err(wrong_use(
                l,
                l.pick("/work <name> <要做的事>", "/work <name> <what to do>"),
            )),
        },
        "reassign" => {
            let names: Vec<&str> = args
                .split_whitespace()
                .map(|w| w.trim_start_matches('@'))
                .filter(|w| !w.is_empty())
                .collect();
            match names[..] {
                [from, to] => Ok(Intent::Reassign {
                    from: from.to_owned(),
                    to: to.to_owned(),
                }),
                _ => Err(wrong_use(
                    l,
                    l.pick("/reassign <原负责人> <新负责人>", "/reassign <from> <to>"),
                )),
            }
        }
        "tell" => match args.find(char::is_whitespace) {
            Some(at) if !args[at..].trim().is_empty() => {
                let body = args[at..].trim();
                Ok(Intent::Relay {
                    targets: vec![args[..at].trim_start_matches('@').to_owned()],
                    kind: relay_kind(body),
                    body: body.to_owned(),
                })
            }
            _ => Err(wrong_use(
                l,
                l.pick("/tell <name> <内容>", "/tell <name> <text>"),
            )),
        },
        _ => Err(match l {
            Locale::Zh => format!("我不认识这个命令。{}", usage(l)),
            Locale::En => format!("I do not know that command. {}", usage(l)),
        }),
    })
}

/// `/tell` relays a question when it reads like one, else a note: a
/// question mark in either script, Chinese 吗 at the end, or an English
/// sentence that opens like a question.
fn relay_kind(body: &str) -> RelayKind {
    let body = body.trim_end();
    if body.ends_with('?') || body.ends_with('？') || body.ends_with('吗') {
        return RelayKind::Question;
    }
    let first = body
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_lowercase();
    const OPENERS: [&str; 16] = [
        "what", "which", "who", "whom", "whose", "when", "where", "why", "how", "is", "are",
        "does", "do", "can", "could", "should",
    ];
    if OPENERS.contains(&first.as_str()) {
        RelayKind::Question
    } else {
        RelayKind::Note
    }
}

fn parse_decision(args: &str) -> Option<Intent> {
    let first = args.split_whitespace().next()?;
    let (body, supersedes) = if first == "--supersedes" {
        let (seq, body) = args[first.len()..]
            .trim_start()
            .split_once(char::is_whitespace)?;
        (body.trim(), Some(seq.parse().ok()?))
    } else {
        (args, None)
    };
    Some(Intent::Decide {
        body: body.to_owned(),
        supersedes,
    })
}

fn output_level(arg: &str) -> Option<OutputLevel> {
    match arg.trim().to_lowercase().as_str() {
        "answers" | "answer" | "答案" | "只回答案" => Some(OutputLevel::Answers),
        "status" | "状态" => Some(OutputLevel::Status),
        "all" | "everything" | "全部" | "全部往返" => Some(OutputLevel::All),
        _ => None,
    }
}

/// Lowercased, with surrounding space and trailing punctuation dropped.
fn normalize(text: &str) -> String {
    text.trim()
        .trim_end_matches(|c: char| {
            c.is_whitespace() || matches!(c, '。' | '.' | '!' | '！' | '~' | '～' | '，' | ',')
        })
        .to_lowercase()
}

/// A person's reply in a thread parked on them that hands it to their own
/// agent: 「交给我的 agent」, "hand it to my agent" and close variants.
/// `Some(None)` when no agent is named (the thread's own agent, or the one
/// the Speaker settles on), `Some(Some(name))` for 「交给我的 codex」 /
/// "hand it to my codex". Case, spaces and trailing punctuation are ignored.
pub(crate) fn handoff(text: &str) -> Option<Option<String>> {
    let compact: String = normalize(text)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let compact = compact.trim_end_matches('吧');
    if matches!(
        compact,
        "handoff" | "交给agent" | "交给我agent" | "给我agent"
    ) {
        return Some(None);
    }
    // "my" and what follows it, in either language.
    const MINE: [&str; 10] = [
        "交给我的",
        "给我的",
        "让我的",
        "我的",
        "handittomy",
        "giveittomy",
        "passittomy",
        "sendittomy",
        "letmy",
        "handtomy",
    ];
    const THEN: [&str; 9] = [
        "处理", "做", "来", "doit", "handleit", "takeit", "takeover", "dothis", "",
    ];
    for prefix in MINE {
        let Some(rest) = compact.strip_prefix(prefix) else {
            continue;
        };
        for suffix in THEN {
            let Some(name) = rest.strip_suffix(suffix) else {
                continue;
            };
            if name == "agent" {
                return Some(None);
            }
            if !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && senate_core::Name::new(name).is_ok()
            {
                return Some(Some(name.to_owned()));
            }
        }
    }
    None
}

// Advertise the common replies. The parser also accepts conversational variants.
const ZH_CONFIRM: [&str; 2] = ["对", "是"];
const EN_CONFIRM: &str = "yes";

pub(crate) fn proposal_confirmation(l: Locale) -> String {
    match l {
        Locale::Zh => format!("回复「{}」或「{}」确认", ZH_CONFIRM[0], ZH_CONFIRM[1]),
        Locale::En => format!("Reply \"{EN_CONFIRM}\" to confirm."),
    }
}

/// Accept either language regardless of the locale of the proposal.
pub(crate) fn is_affirmative(text: &str) -> bool {
    let word = normalize(text);
    ZH_CONFIRM.contains(&word.as_str())
        || word == EN_CONFIRM
        || matches!(
            word.as_str(),
            "好" | "确认"
                | "对的"
                | "是的"
                | "好的"
                | "可以"
                | "y"
                | "yep"
                | "yeah"
                | "ok"
                | "okay"
                | "sure"
                | "confirm"
                | "confirmed"
                | "correct"
                | "go ahead"
        )
}

/// One person's agent named in what a person wrote: `alice 的 codex`,
/// `alice's codex`. Returns the person as written and the agent's name.
/// A bare name (`codex`, `[senator::codex]`) is one senator, whoever it is:
/// this is `None` for it, and the roster says what it names.
pub(crate) fn agent_named(raw: &str) -> Option<(String, String)> {
    let text = raw.trim().trim_start_matches('@');
    for joint in ["'s ", "’s ", " 的 ", "的"] {
        if let Some((person, agent)) = text.split_once(joint) {
            let (person, agent) = (person.trim(), agent.trim().to_lowercase());
            // "alice's agent" is the person's agent, not one called "agent".
            if !person.is_empty()
                && agent != "agent"
                && senate_core::Name::new(agent.as_str()).is_ok()
            {
                return Some((person.to_owned(), agent));
            }
        }
    }
    None
}

/// Which of `candidates` a reply to "which agent?" names: its number in the
/// list (1-based), its name (`codex`) or its mention (`[senator::codex]`).
/// `None` unless exactly one matches.
pub(crate) fn choice(text: &str, candidates: &[SenatorName]) -> Option<SenatorName> {
    let lower = normalize(text);
    if let Ok(n) = lower
        .trim_start_matches('#')
        .trim_start_matches('第')
        .trim_end_matches('个')
        .parse::<usize>()
    {
        return n.checked_sub(1).and_then(|i| candidates.get(i)).cloned();
    }
    let tokens: Vec<&str> = lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '/' | ':')))
        .map(|t| t.trim_start_matches("senator::"))
        .filter(|t| !t.is_empty())
        .collect();
    let mut found = candidates.iter().filter(|candidate| {
        let name = candidate.as_str();
        tokens.contains(&name)
    });
    match (found.next(), found.next()) {
        (Some(one), None) => Some(one.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> SenatorName {
        s.parse().expect("path")
    }

    #[test]
    fn commands_parse() {
        let zh = Locale::Zh;
        assert_eq!(
            parse_command(" /bind refund-v2 ", zh),
            Some(Ok(Intent::Bind {
                committee: "refund-v2".into()
            }))
        );
        assert_eq!(parse_command("/unbind", zh), Some(Ok(Intent::Unbind)));
        assert_eq!(parse_command("／STATUS", zh), Some(Ok(Intent::Status)));
        assert_eq!(
            parse_command("/tell @alice 退款上限是 30 天", zh),
            Some(Ok(Intent::Relay {
                targets: vec!["alice".into()],
                kind: RelayKind::Note,
                body: "退款上限是 30 天".into(),
            }))
        );
        assert!(matches!(
            parse_command("/tell bob 网关用哪个 header？", zh),
            Some(Ok(Intent::Relay {
                kind: RelayKind::Question,
                ..
            }))
        ));
        assert_eq!(
            parse_command("/decide 退款上限 30 天", zh),
            Some(Ok(Intent::Decide {
                body: "退款上限 30 天".into(),
                supersedes: None,
            }))
        );
        assert_eq!(parse_command("/brief", zh), Some(Ok(Intent::Recall)));
        for wrong in [
            "/bind",
            "/bind a b",
            "/tell bob",
            "/decide",
            "/decide --supersedes",
            "/decide --supersedes 33",
            "/decide --supersedes old text",
            "/frobnicate",
        ] {
            assert!(matches!(parse_command(wrong, zh), Some(Err(_))), "{wrong}");
        }
        assert_eq!(parse_command("告诉 alice 的 agent", zh), None);
        assert_eq!(
            parse_command("/agents", zh),
            Some(Ok(Intent::AdoptAgentGroup { committee: None }))
        );
        assert_eq!(
            parse_command("/agents refund-v2", zh),
            Some(Ok(Intent::AdoptAgentGroup {
                committee: Some("refund-v2".into())
            }))
        );
        assert_eq!(
            parse_command("/invite @alice bob", zh),
            Some(Ok(Intent::InviteToAgentGroup {
                who: vec!["alice".into(), "bob".into()]
            }))
        );
        assert_eq!(
            parse_command("/output answers", zh),
            Some(Ok(Intent::SetOutputLevel {
                level: OutputLevel::Answers
            }))
        );
        assert!(matches!(parse_command("/output loud", zh), Some(Err(_))));
        assert_eq!(
            parse_command("/work @alice 把重试路径做完 并补测试", zh),
            Some(Ok(Intent::Work {
                target: "alice".into(),
                instruction: "把重试路径做完 并补测试".into(),
            }))
        );
        assert_eq!(
            parse_command("/reassign alice @bob", zh),
            Some(Ok(Intent::Reassign {
                from: "alice".into(),
                to: "bob".into(),
            }))
        );
        for wrong in ["/work", "/work alice", "/reassign alice", "/reassign a b c"] {
            assert!(matches!(parse_command(wrong, zh), Some(Err(_))), "{wrong}");
        }
        assert!(matches!(parse_command("/agents a b", zh), Some(Err(_))));
    }

    #[test]
    fn commands_name_one_agent_outright() {
        assert_eq!(
            parse_command("/tell codex is the cap 3?", Locale::En),
            Some(Ok(Intent::Relay {
                targets: vec!["codex".into()],
                kind: RelayKind::Question,
                body: "is the cap 3?".into(),
            }))
        );
        assert!(matches!(
            parse_command("/work codex finish the retry path", Locale::En),
            Some(Ok(Intent::Work { target, .. })) if target == "codex"
        ));
    }

    #[test]
    fn usage_follows_the_locale_and_commands_do_not() {
        let Some(Err(zh)) = parse_command("/bind", Locale::Zh) else {
            panic!("usage")
        };
        assert!(
            zh.starts_with("用法：/bind <committee>。固定写法："),
            "{zh}"
        );
        let Some(Err(en)) = parse_command("/bind", Locale::En) else {
            panic!("usage")
        };
        assert!(
            en.starts_with("Usage: /bind <committee>. Commands:"),
            "{en}"
        );
        let Some(Err(en)) = parse_command("/frobnicate", Locale::En) else {
            panic!("usage")
        };
        assert!(en.starts_with("I do not know that command."), "{en}");
        // The same command in either locale.
        for locale in Locale::ALL {
            assert_eq!(parse_command("/status", locale), Some(Ok(Intent::Status)));
        }
    }

    #[test]
    fn memory_commands_take_seqs_and_words() {
        let en = Locale::En;
        assert_eq!(
            parse_command("/memory release day", en),
            Some(Ok(Intent::Remember {
                query: "release day".into()
            }))
        );
        assert_eq!(
            parse_command("/记忆", en),
            Some(Ok(Intent::Remember {
                query: String::new()
            }))
        );
        assert_eq!(
            parse_command("/forget 12, #14", en),
            Some(Ok(Intent::Forget {
                targets: vec![Seq(12), Seq(14)]
            }))
        );
        assert_eq!(
            parse_command("/更正 12 发布日期改为周一", en),
            Some(Ok(Intent::Correct {
                target: Seq(12),
                text: "发布日期改为周一".into()
            }))
        );
        assert!(matches!(parse_command("/forget twelve", en), Some(Err(_))));
        assert!(matches!(parse_command("/correct 12", en), Some(Err(_))));
    }

    #[test]
    fn english_questions_are_relayed_as_questions() {
        for q in [
            "which header does the gateway use",
            "is the cap 3",
            "how so?",
        ] {
            assert_eq!(relay_kind(q), RelayKind::Question, "{q}");
        }
        for n in ["the cap is 3", "please use X-Idem", "退款上限是 30 天"] {
            assert_eq!(relay_kind(n), RelayKind::Note, "{n}");
        }
    }

    #[test]
    fn handoffs_are_close_variants_of_one_phrase_in_either_language() {
        for yes in [
            "交给我的 agent",
            "交给我的agent",
            " 交给我的 Agent 吧。",
            "让我的 agent 处理",
            "交给 agent",
            "handoff",
            "Hand it to my agent.",
            "give it to my agent",
            "let my agent do it",
        ] {
            assert_eq!(handoff(yes), Some(None), "{yes}");
        }
        assert_eq!(handoff("交给我的 codex"), Some(Some("codex".into())));
        assert_eq!(handoff("hand it to my Codex!"), Some(Some("codex".into())));
        assert_eq!(handoff("让我的 claude 来"), Some(Some("claude".into())));
        for no in [
            "不要交给我的 agent",
            "我来回答：用 X",
            "对",
            "agent 是谁",
            "my agent is busy, I'll answer",
            "交给 bob",
        ] {
            assert_eq!(handoff(no), None, "{no}");
        }
    }

    #[test]
    fn confirmations_are_strict_words_in_either_language() {
        for yes in [
            "对",
            "是",
            "好",
            "确认",
            "YES",
            "ok",
            " 对。",
            "确认！",
            "好的",
            "Confirm",
            "okay.",
        ] {
            assert!(is_affirmative(yes), "{yes}");
        }
        for no in [
            "不对",
            "取消",
            "no",
            "对吗？",
            "ok but wait",
            "cancel",
            "好吗？",
        ] {
            assert!(!is_affirmative(no), "{no}");
        }
    }

    #[test]
    fn a_persons_agent_is_named_by_possessive() {
        for raw in [
            "alice 的 codex",
            "alice的codex",
            "Alice's codex",
            "爱丽丝的 Codex",
        ] {
            let (person, agent) = agent_named(raw).unwrap_or_else(|| panic!("{raw}"));
            assert!(
                person.eq_ignore_ascii_case("alice") || person == "爱丽丝",
                "{raw}"
            );
            assert_eq!(agent, "codex", "{raw}");
        }
        // A bare name or mention is one senator, person or agent, which the
        // roster tells apart; an owner-prefixed name names nobody's agent.
        for one in [
            "alice",
            "@alice",
            "Alice",
            "senator::alice",
            "codex",
            "[senator::codex]",
            "alice/codex",
            "senator::alice/codex",
            "alice 的 agent",
            "alice's agent",
        ] {
            assert_eq!(agent_named(one), None, "{one}");
        }
    }

    #[test]
    fn a_choice_is_a_number_or_a_name() {
        let candidates = [path("claude"), path("codex")];
        for (reply, want) in [
            ("2", "codex"),
            ("第1个", "claude"),
            ("codex", "codex"),
            ("Claude 吧", "claude"),
            ("the codex one", "codex"),
            ("[senator::codex]", "codex"),
            ("claude", "claude"),
        ] {
            assert_eq!(choice(reply, &candidates), Some(path(want)), "{reply}");
        }
        for reply in ["3", "0", "either", "claude or codex", "随便"] {
            assert_eq!(choice(reply, &candidates), None, "{reply}");
        }
    }
}
