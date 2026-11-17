//! Texts about which of a person's agents is meant, and the few labels a
//! work run's card and status need.

use std::fmt::Write as _;

use chrono::Utc;
use senate_core::continuity::short_commit;
use senate_core::{Address, Locale, Name, SenatorName};

use crate::brain::AgentCandidate;

/// Characters of a checkpoint summary or a thread title in the question.
const WHAT_CHARS: usize = 60;

fn clip(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= WHAT_CHARS {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(WHAT_CHARS).collect();
    out.push('…');
    out
}

fn mentions(candidates: &[SenatorName], l: Locale) -> String {
    candidates
        .iter()
        .map(|c| Address::senator(c).in_text())
        .collect::<Vec<_>>()
        .join(l.pick("、", ", "))
}

/// Asks which agent was meant, listing each with what it is doing, numbered
/// so the person can answer with the number or the agent's name.
///
/// ```text
/// [senator::alice] 有 2 个 agent，你指的是哪一个？回复编号或名字：
/// 1. [senator::claude] 在线 · 检查点 feature/refund-v2/alice@a41f9c2：重试路径做了一半 · 2 个进行中的任务
/// 2. [senator::codex] 不在线
/// ```
///
/// `owner` is `None` for "whichever agent on this committee"
/// (`[senator::*]`): the agents here, nobody's in particular.
pub(crate) fn which_agent(
    l: Locale,
    owner: Option<&Name>,
    candidates: &[AgentCandidate],
) -> String {
    let n = candidates.len();
    let mut out = match (l, owner.map(|o| Address::senior(o).in_text())) {
        (Locale::Zh, Some(person)) => {
            format!("{person} 有 {n} 个 agent，你指的是哪一个？回复编号或名字：")
        }
        (Locale::Zh, None) => format!("这里有 {n} 个 agent，你指的是哪一个？回复编号或名字："),
        (Locale::En, Some(person)) => format!(
            "{person} has {n} agents here. Which one do you mean? Reply with its number or name:"
        ),
        (Locale::En, None) => format!(
            "There are {n} agents here. Which one do you mean? Reply with its number or name:"
        ),
    };
    for (i, c) in candidates.iter().enumerate() {
        let _ = write!(
            out,
            "\n{}. {} {}",
            i + 1,
            Address::senator(&c.agent).in_text(),
            l.pick(
                if c.present { "在线" } else { "不在线" },
                if c.present { "online" } else { "not online" }
            )
        );
        if let Some(record) = &c.checkpoint {
            let at = format!(
                "{}@{}",
                record.checkpoint.branch,
                short_commit(&record.checkpoint.commit)
            );
            let hours = (Utc::now() - record.ts).num_hours().max(0);
            let summary = clip(&record.body);
            let _ = match l {
                Locale::Zh if hours > 0 => {
                    write!(out, " · 检查点 {at}（{hours} 小时前）：{summary}")
                }
                Locale::Zh => write!(out, " · 检查点 {at}：{summary}"),
                Locale::En if hours > 0 => {
                    write!(out, " · checkpoint {at} ({hours} h ago): {summary}")
                }
                Locale::En => write!(out, " · checkpoint {at}: {summary}"),
            };
        }
        let open = c.open_threads.len();
        if open > 0 {
            let _ = match l {
                Locale::Zh => write!(out, " · {open} 个进行中的任务"),
                Locale::En if open == 1 => write!(out, " · 1 open task"),
                Locale::En => write!(out, " · {open} open tasks"),
            };
        }
        if c.owns_thread {
            out.push_str(l.pick(" · 这个线程是它的", " · this thread is its"));
        }
    }
    out
}

/// The reply to "which agent?" named none of them.
pub(crate) fn choice_not_understood(l: Locale, candidates: &[SenatorName]) -> String {
    let list = mentions(candidates, l);
    match l {
        Locale::Zh => format!("没认出是哪一个。请回复编号或名字：{list}"),
        Locale::En => format!("I could not tell which one. Reply with a number or a name: {list}"),
    }
}

/// The Speaker cannot ask here: the person names one and asks again.
pub(crate) fn name_one_of(l: Locale, owner: Option<&Name>, candidates: &[SenatorName]) -> String {
    let list = mentions(candidates, l);
    match (l, owner.map(|o| Address::senior(o).in_text())) {
        (Locale::Zh, Some(person)) => {
            format!("{person} 有几个 agent，请指明是哪一个再说一次：{list}")
        }
        (Locale::Zh, None) => format!("这里有几个 agent，请指明是哪一个再说一次：{list}"),
        (Locale::En, Some(person)) => {
            format!("{person} has several agents here; say which one and ask again: {list}")
        }
        (Locale::En, None) => {
            format!("There are several agents here; say which one and ask again: {list}")
        }
    }
}

/// A person hands a thread to "my agent" and has none here.
pub(crate) fn no_agent_of_yours(l: Locale) -> String {
    l.pick(
        "你在这里没有 agent，这条只能由你本人处理。",
        "You have no agent here, so this one is yours to handle in person.",
    )
    .to_owned()
}

/// A person hands a thread to "my <name>" and has no agent by that name.
pub(crate) fn no_agent_named(l: Locale, owner: &Name, agent: &str) -> String {
    let person = Address::senior(owner).in_text();
    match l {
        Locale::Zh => format!("{person} 在这里没有叫 {agent} 的 agent。"),
        Locale::En => format!("{person} has no agent called {agent} here."),
    }
}

/// A run's work is to start from an agent's checkpoint, and the person
/// named has no agent here.
pub(crate) fn no_agent_to_work_from(l: Locale, person: &Name) -> String {
    let person = Address::senior(person).in_text();
    match l {
        Locale::Zh => format!("{person} 在这里没有 agent，没有检查点可以接着做。"),
        Locale::En => {
            format!("{person} has no agent here, so there is no checkpoint to continue from.")
        }
    }
}

/// Work moves between agents, and the person named has none here.
pub(crate) fn no_agent_to_reassign(l: Locale, person: &Name) -> String {
    let person = Address::senior(person).in_text();
    match l {
        Locale::Zh => format!("{person} 在这里没有 agent；工作只能在 agent 之间交接。"),
        Locale::En => format!("{person} has no agent here; work is handed between agents."),
    }
}

/// The summary of a run whose task ended without a word (it crashed).
pub(crate) fn run_crashed(l: Locale) -> String {
    l.pick(
        "运行环境异常退出。",
        "The run's environment exited unexpectedly.",
    )
    .to_owned()
}

/// A card link to a run's branch.
pub(crate) fn branch_label(l: Locale, branch: &str) -> String {
    match l {
        Locale::Zh => format!("分支 {branch}"),
        Locale::En => format!("Branch {branch}"),
    }
}

/// A run's card title.
pub(crate) fn work_title(l: Locale, instruction: &str) -> String {
    match l {
        Locale::Zh => format!("代码任务：{instruction}"),
        Locale::En => format!("Coding task: {instruction}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests_support::assert_senate_vocabulary;

    fn path(s: &str) -> SenatorName {
        s.parse().expect("path")
    }

    fn candidate(agent: &str, present: bool) -> AgentCandidate {
        AgentCandidate {
            agent: path(agent),
            present,
            checkpoint: None,
            open_threads: Vec::new(),
            owns_thread: false,
        }
    }

    #[test]
    fn the_question_lists_each_agent_with_what_it_is_doing() {
        let alice = Name::new("alice").expect("name");
        let mut claude = candidate("claude", true);
        claude.owns_thread = true;
        let candidates = [claude, candidate("codex", false)];
        let zh = which_agent(Locale::Zh, Some(&alice), &candidates);
        assert_eq!(
            zh,
            "[senator::alice] 有 2 个 agent，你指的是哪一个？回复编号或名字：\n\
             1. [senator::claude] 在线 · 这个线程是它的\n\
             2. [senator::codex] 不在线"
        );
        let en = which_agent(Locale::En, Some(&alice), &candidates);
        assert_eq!(
            en,
            "[senator::alice] has 2 agents here. Which one do you mean? Reply with its number or name:\n\
             1. [senator::claude] online · this thread is its\n\
             2. [senator::codex] not online"
        );
        assert_eq!(
            which_agent(Locale::Zh, None, &candidates),
            "这里有 2 个 agent，你指的是哪一个？回复编号或名字：\n\
             1. [senator::claude] 在线 · 这个线程是它的\n\
             2. [senator::codex] 不在线"
        );
        assert!(
            which_agent(Locale::En, None, &candidates)
                .starts_with("There are 2 agents here. Which one do you mean?")
        );
        let paths = [path("claude"), path("codex")];
        for l in Locale::ALL {
            for text in [
                which_agent(l, Some(&alice), &candidates),
                which_agent(l, None, &candidates),
                choice_not_understood(l, &paths),
                name_one_of(l, Some(&alice), &paths),
                name_one_of(l, None, &paths),
                no_agent_of_yours(l),
                no_agent_named(l, &alice, "gemini"),
                no_agent_to_work_from(l, &alice),
                no_agent_to_reassign(l, &alice),
                run_crashed(l),
                branch_label(l, "feature/refund-v2/speaker-7"),
                work_title(l, "finish the retry path"),
            ] {
                assert_senate_vocabulary(&text);
            }
        }
        assert_eq!(
            choice_not_understood(Locale::En, &paths),
            "I could not tell which one. Reply with a number or a name: [senator::claude], [senator::codex]"
        );
    }
}
