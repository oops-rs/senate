//! What the Speaker says about work runs and reassignment.

use std::fmt::Write as _;

use senate_core::continuity::short_commit;
use senate_core::{
    BranchNaming, CheckpointRecord, CommitteeId, Event, Locale, Name, SenatorName, Seq, WorkRun,
    WorkState,
};

use super::{SeatNames, author, body, cut, one_line, person, quoted_for, senator, signature_of};
use crate::syntax::usage;

/// Characters of the instruction in a card's title and a one-line reason.
const INSTRUCTION_CHARS: usize = 60;
/// Characters of the summary on the card.
const SUMMARY_CHARS: usize = 200;

fn at(branch: &str, commit: &str) -> String {
    format!("{branch}@{}", short_commit(commit))
}

fn base(run: &WorkRun) -> String {
    at(
        run.base_branch.as_deref().unwrap_or("?"),
        run.base_commit.as_deref().unwrap_or("?"),
    )
}

/// The restatement of a work order, asking for a yes:
/// `我理解为：从 [senator::claude] 的检查点 senate/refund-v2/alice@a41f9c2 拉新分支 …，代表 [senator::bob]。…回复「对」确认`.
pub(crate) fn confirm_work(l: Locale, run: &WorkRun) -> String {
    super::confirm_action(l, &describe_work(l, run))
}

pub(crate) fn describe_work(l: Locale, run: &WorkRun) -> String {
    let agent = senator(&run.target_agent);
    let instruction = one_line(&run.instruction);
    let who = person(&run.requester);
    let (at, branch) = (base(run), &run.branch);
    match l {
        Locale::Zh => format!(
            "从 {agent} 的检查点 {at} 拉新分支 {branch}，做：{instruction}；完成后推送并开 PR，代表 {who}。不会改动 {agent} 自己的分支。"
        ),
        Locale::En => format!(
            "Branch {branch} off {agent}'s checkpoint {at} and do: {instruction}; then push and open a PR, on behalf of {who}. {agent}'s own branch is not touched."
        ),
    }
}

/// The body of a run's root note: its card title comes from the first line.
pub(crate) fn work_root(l: Locale, run: &WorkRun) -> String {
    let instruction = cut(&one_line(&run.instruction), INSTRUCTION_CHARS);
    let who = person(&run.requester);
    let agent = senator(&run.target_agent);
    let (at, branch) = (base(run), &run.branch);
    match l {
        Locale::Zh => {
            format!("代码任务：{instruction}\n代表 {who}，从 {agent} 的检查点 {at}，分支 {branch}")
        }
        Locale::En => format!(
            "Coding task: {instruction}\nFor {who}, from {agent}'s checkpoint {at}, branch {branch}"
        ),
    }
}

/// The reply to the yes: where to watch.
pub(crate) fn work_started(l: Locale, seq: Seq) -> String {
    match l {
        Locale::Zh => {
            format!("好的，开始了（任务 seq {seq}）。进度在任务卡上，做完我会把 PR 发在这里。")
        }
        Locale::En => format!(
            "OK, started (task seq {seq}). Progress is on the task card; I will post the PR here when it is done."
        ),
    }
}

pub(crate) fn work_unconfigured(l: Locale) -> String {
    l.pick(
        "这台服务器没有配置代码运行环境，我没法写代码。",
        "This server has no environment for running code, so I cannot write code.",
    )
    .to_owned()
}

pub(crate) fn not_a_developer(l: Locale) -> String {
    l.pick(
        "只有这个委员会的开发可以让我写代码。可以请一位开发来提。",
        "Only this committee's developers can have me write code. Ask one of them to request it.",
    )
    .to_owned()
}

/// An agent asked: only people may.
pub(crate) fn agents_cannot_work(l: Locale) -> String {
    l.pick(
        "我做不了这个：只有人可以让 Speaker 写代码或改派工作，agent 不行。请让你的人在群里提。",
        "I cannot do this: only people can have the Speaker write code or reassign work, not agents. Ask your person to request it in the group.",
    )
    .to_owned()
}

pub(crate) fn work_busy(l: Locale, root: Seq) -> String {
    match l {
        Locale::Zh => format!("这个委员会已经有一个代码任务在运行（seq {root}），等它结束再提。"),
        Locale::En => format!(
            "This committee already has a coding task running (seq {root}); ask again when it is done."
        ),
    }
}

pub(crate) fn work_whom(l: Locale) -> String {
    match l {
        Locale::Zh => format!(
            "要接着谁的工作做什么？用 /work <name> <要做的事>。{}",
            usage(l)
        ),
        Locale::En => format!(
            "Whose work should I continue, and with what? Use /work <name> <what to do>. {}",
            usage(l)
        ),
    }
}

pub(crate) fn work_needs_binding(l: Locale) -> String {
    l.pick(
        "这个群还没有绑定委员会，我不知道在哪个委员会上做。先 /bind <committee>。",
        "This group is not bound to a committee yet, so I do not know which committee to work on. /bind <committee> first.",
    )
    .to_owned()
}

pub(crate) fn no_checkpoint(l: Locale, agent: &SenatorName) -> String {
    let agent = senator(agent);
    match l {
        Locale::Zh => {
            format!("{agent} 在这个委员会上还没有检查点，我没有可以接着做的分支。")
        }
        Locale::En => format!(
            "{agent} has no checkpoint on this committee yet, so there is no branch for me to continue from."
        ),
    }
}

pub(crate) fn checkpoint_detached(l: Locale, agent: &SenatorName) -> String {
    let agent = senator(agent);
    match l {
        Locale::Zh => format!("{agent} 的检查点不在一个分支上（HEAD 分离），我没法从它拉分支。"),
        Locale::En => format!(
            "{agent}'s checkpoint is not on a branch (detached HEAD), so I cannot branch from it."
        ),
    }
}

pub(crate) fn checkpoint_no_remote(l: Locale, agent: &SenatorName) -> String {
    let agent = senator(agent);
    match l {
        Locale::Zh => format!("{agent} 的检查点没有远程仓库地址，我拿不到代码。"),
        Locale::En => {
            format!("{agent}'s checkpoint has no remote repository, so I cannot get the code.")
        }
    }
}

pub(crate) fn checkpoint_not_pushed(
    l: Locale,
    agent: &SenatorName,
    record: &CheckpointRecord,
) -> String {
    let agent = senator(agent);
    let at = record.checkpoint.at();
    match l {
        Locale::Zh => format!(
            "{agent} 的检查点 {at} 还没推送到远程，我拿不到这份代码。请先推送（/senate handoff）再提。"
        ),
        Locale::En => format!(
            "{agent}'s checkpoint {at} is not pushed yet, so I cannot get that code. Push it first (/senate handoff), then ask again."
        ),
    }
}

pub(crate) fn repo_refused(l: Locale, reason: &str) -> String {
    match l {
        Locale::Zh => format!("这个仓库不能用：{reason}"),
        Locale::En => format!("This repository cannot be used: {reason}"),
    }
}

/// One line on the card: what the run came to.
pub(crate) fn work_reason(l: Locale, run: &WorkRun) -> String {
    let summary = run
        .summary
        .as_deref()
        .map(|s| cut(&one_line(s), SUMMARY_CHARS))
        .unwrap_or_default();
    match run.state {
        WorkState::Queued => l.pick("等待运行环境", "waiting for a runner").to_owned(),
        WorkState::Running => format!("{} · {}", run.state.label(l), run.branch),
        _ if summary.is_empty() => run.state.label(l).to_owned(),
        _ => format!("{} · {summary}", run.state.label(l)),
    }
}

/// What a finished run says in its threads: the outcome, the PR or the
/// branch, and, when it did not succeed, the next steps as numbered options.
pub(crate) fn work_result(l: Locale, run: &WorkRun, delivery_note: Option<&str>) -> String {
    let mut out = match (run.state, l) {
        (WorkState::Succeeded, Locale::Zh) => {
            format!("代码任务完成（代表 {}）", person(&run.requester))
        }
        (WorkState::Succeeded, Locale::En) => {
            format!("Coding task done (for {})", person(&run.requester))
        }
        (WorkState::Failed, _) => l
            .pick("代码任务没有完成", "Coding task did not finish")
            .to_owned(),
        (WorkState::TimedOut, _) => l
            .pick(
                "代码任务到了时间上限，停下了",
                "Coding task hit its time limit and stopped",
            )
            .to_owned(),
        (WorkState::Refused, _) => l
            .pick("代码任务没有运行", "Coding task did not run")
            .to_owned(),
        (WorkState::Queued | WorkState::Running, _) => l
            .pick("代码任务进行中", "Coding task in progress")
            .to_owned(),
    };
    let _ = write!(
        out,
        "{}{}",
        l.pick("：", ": "),
        cut(&one_line(&run.instruction), INSTRUCTION_CHARS)
    );
    if let Some(summary) = run.summary.as_deref().filter(|s| !s.trim().is_empty()) {
        let _ = write!(out, "\n{}", summary.trim());
    }
    match (&run.pr_url, &run.pushed_commit) {
        (Some(url), _) => {
            let _ = write!(out, "\nPR{}{url}", l.pick("：", ": "));
        }
        (None, Some(commit)) => {
            let commit = short_commit(commit);
            let _ = match l {
                Locale::Zh => write!(out, "\n分支：{}（{commit}，已推送）", run.branch),
                Locale::En => write!(out, "\nBranch: {} ({commit}, pushed)", run.branch),
            };
        }
        (None, None) if run.state != WorkState::Refused => {
            let _ = match l {
                Locale::Zh => write!(out, "\n分支 {} 没有推送新的提交。", run.branch),
                Locale::En => write!(out, "\nNo new commits were pushed to {}.", run.branch),
            };
        }
        (None, None) => {}
    }
    if let Some(note) = delivery_note {
        let _ = write!(out, "\n{note}");
    }
    if run.state != WorkState::Succeeded && !run.next_steps.is_empty() {
        out.push_str(l.pick("\n接下来可以：", "\nNext, you can:"));
        for (i, step) in run.next_steps.iter().enumerate() {
            let _ = write!(out, "\n{}. {}", i + 1, one_line(step));
        }
    }
    out
}

/// The summary of a run cut short by a restart.
pub(crate) fn work_interrupted(l: Locale) -> String {
    l.pick(
        "Speaker 重启，运行中断。分支上已推送的工作都保留着；不会自动重跑。",
        "The Speaker restarted and the run was cut short. Work already pushed to the branch is kept; it will not be rerun automatically.",
    )
    .to_owned()
}

pub(crate) fn work_conflict(l: Locale, other: Seq) -> String {
    match l {
        Locale::Zh => format!("同一个委员会已有代码任务在运行（seq {other}），这次没有运行。"),
        Locale::En => format!(
            "Another coding task is already running on this committee (seq {other}), so this one did not run."
        ),
    }
}

pub(crate) fn pr_failed(l: Locale, reason: &str) -> String {
    let reason = one_line(reason);
    match l {
        Locale::Zh => format!("PR 没开成（{reason}），分支已推送，可以手动开。"),
        Locale::En => format!(
            "The PR could not be opened ({reason}); the branch is pushed, so you can open one by hand."
        ),
    }
}

// ----- reassignment ----------------------------------------------------------

pub(crate) fn describe_reassign(
    l: Locale,
    committee: &CommitteeId,
    from: &SenatorName,
    to: &SenatorName,
) -> String {
    let (from, to) = (senator(from), senator(to));
    match l {
        Locale::Zh => format!(
            "把 {from} 在 {committee} 的工作交给 {to}。之后发给 {from} 的消息也会转给 {to}，{to} 会收到 {from} 最新的检查点。"
        ),
        Locale::En => format!(
            "Hand {from}'s work on {committee} to {to}. Messages for {from} will then go to {to} as well, and {to} gets {from}'s latest checkpoint."
        ),
    }
}

pub(crate) fn describe_handback(
    l: Locale,
    committee: &CommitteeId,
    agent: &SenatorName,
    owner: &SenatorName,
) -> String {
    let (agent, owner) = (senator(agent), senator(owner));
    match l {
        Locale::Zh => format!(
            "把 {committee} 上 {agent} 的工作从 {owner} 交还给 {agent}，消息不再转给 {owner}。"
        ),
        Locale::En => format!(
            "Give {agent}'s work on {committee} back to it from {owner}; messages no longer go to {owner}."
        ),
    }
}

pub(crate) fn reassigned(l: Locale, from: &SenatorName, to: &SenatorName) -> String {
    let (from, to) = (senator(from), senator(to));
    match l {
        Locale::Zh => format!("好的，{from} 的工作已交给 {to}。"),
        Locale::En => format!("OK, {from}'s work is now with {to}."),
    }
}

pub(crate) fn handed_back(l: Locale, agent: &SenatorName) -> String {
    let agent = senator(agent);
    match l {
        Locale::Zh => format!("好的，工作已交还给 {agent}。"),
        Locale::En => format!("OK, the work is back with {agent}."),
    }
}

pub(crate) fn already_reassigned(l: Locale, from: &SenatorName, to: &SenatorName) -> String {
    let (from, to) = (senator(from), senator(to));
    match l {
        Locale::Zh => format!("{from} 的工作已经在 {to} 那里了。"),
        Locale::En => format!("{from}'s work is already with {to}."),
    }
}

pub(crate) fn not_reassigned(l: Locale, agent: &SenatorName) -> String {
    let agent = senator(agent);
    match l {
        Locale::Zh => format!("{agent} 的工作没有交给别人，不需要交还。"),
        Locale::En => {
            format!(
                "{agent}'s work was not handed to anyone else, so there is nothing to give back."
            )
        }
    }
}

pub(crate) fn reassign_refused(l: Locale, reason: &str) -> String {
    match l {
        Locale::Zh => format!("没能改派：{}", one_line(reason)),
        Locale::En => format!("Could not reassign: {}", one_line(reason)),
    }
}

/// The Agent group's line for a reassignment.
pub(crate) fn reassigned_line(l: Locale, from: &SenatorName, to: &SenatorName) -> String {
    let (from, to) = (senator(from), senator(to));
    match l {
        Locale::Zh => format!("{from} 的工作已交给 {to}"),
        Locale::En => format!("{from}'s work is now with {to}"),
    }
}

/// The Agent group's line for a hand-back.
pub(crate) fn work_handback_line(l: Locale, agent: &SenatorName) -> String {
    let agent = senator(agent);
    match l {
        Locale::Zh => format!("{agent} 的工作已交还给 {agent}"),
        Locale::En => format!("{agent}'s work is back with {agent}"),
    }
}

/// The one note the new owner's bridge is pushed: the old owner's latest
/// checkpoint and how to start from its branch. The suggested branch is the
/// new owner's person's own branch on the committee (`to_person`, the
/// person who answers for the new owner)
/// ([`BranchNaming::developer`] with the default prefix).
pub(crate) fn reassign_note(
    l: Locale,
    committee: &CommitteeId,
    from: &SenatorName,
    to_person: &Name,
    by: Option<&Name>,
    checkpoint: Option<&CheckpointRecord>,
    branches: &BranchNaming,
) -> String {
    let from_m = senator(from);
    let mut out = match l {
        Locale::Zh => format!("{committee} 上 {from_m} 的工作交给你了"),
        Locale::En => format!("{from_m}'s work on {committee} is yours now"),
    };
    if let Some(by) = by {
        let _ = match l {
            Locale::Zh => write!(out, "（{} 确认）", person(by)),
            Locale::En => write!(out, " (confirmed by {})", person(by)),
        };
    }
    let _ = match l {
        Locale::Zh => write!(
            out,
            "。之后发给 {from_m} 的消息也会推给你，回答时照常用 answer。"
        ),
        Locale::En => write!(
            out,
            ". Messages for {from_m} will be pushed to you too; answer them with answer as usual."
        ),
    };
    let Some(record) = checkpoint else {
        let _ = match l {
            Locale::Zh => write!(
                out,
                "\n{from_m} 没有留下检查点：先读 brief（senate://{committee}/brief）和 catch_up。"
            ),
            Locale::En => write!(
                out,
                "\n{from_m} left no checkpoint: read the brief (senate://{committee}/brief) and catch_up first."
            ),
        };
        return out;
    };
    let c = &record.checkpoint;
    let pushed = match (c.pushed, l) {
        (true, Locale::Zh) => "（已推送）",
        (false, Locale::Zh) => "（未推送，可能需要找 TA 推送）",
        (true, Locale::En) => " (pushed)",
        (false, Locale::En) => " (not pushed; you may need to ask its owner to push)",
    };
    let _ = match l {
        Locale::Zh => write!(
            out,
            "\n\n检查点（seq {}）：分支 {}，提交 {}{pushed}",
            record.seq, c.branch, c.commit
        ),
        Locale::En => write!(
            out,
            "\n\nCheckpoint (seq {}): branch {}, commit {}{pushed}",
            record.seq, c.branch, c.commit
        ),
    };
    if let Some(remote) = &c.remote_url {
        let _ = match l {
            Locale::Zh => write!(out, "，仓库 {remote}"),
            Locale::En => write!(out, ", repository {remote}"),
        };
    }
    let own = branches.developer(committee, to_person);
    let _ = match l {
        Locale::Zh => write!(
            out,
            "\n从那个分支开始，不要直接在 TA 的分支上提交：\n    git fetch origin && git switch -c {own} origin/{}",
            c.branch
        ),
        Locale::En => write!(
            out,
            "\nStart from that branch, and do not commit on it directly:\n    git fetch origin && git switch -c {own} origin/{}",
            c.branch
        ),
    };
    for (name, text) in [
        ("PROGRESS.md", &c.files.progress),
        ("DECISIONS.md", &c.files.decisions),
        ("OPEN.md", &c.files.open),
    ] {
        if let Some(text) = text.as_deref().filter(|t| !t.trim().is_empty()) {
            let _ = write!(out, "\n\n--- {name} ---\n{}", text.trim());
        }
    }
    out
}

/// The note to the agent that held the work, when it is handed back.
pub(crate) fn work_handback_note(
    l: Locale,
    committee: &CommitteeId,
    agent: &SenatorName,
) -> String {
    let agent = senator(agent);
    match l {
        Locale::Zh => {
            format!("{committee} 上 {agent} 的工作已交还给 TA，发给 {agent} 的消息不再推给你。")
        }
        Locale::En => format!(
            "{agent}'s work on {committee} is back with it; messages for {agent} are no longer pushed to you."
        ),
    }
}

/// `[senator::copilot]（接手 [senator::claude]）` /
/// `[senator::copilot] (covering for [senator::claude])`.
fn covering_label(l: Locale, names: SeatNames<'_>, event: &Event, old: &SenatorName) -> String {
    let (who, old) = (author(l, names, event), senator(old));
    match l {
        Locale::Zh => format!("{who}（接手 {old}）"),
        Locale::En => format!("{who} (covering for {old})"),
    }
}

/// A reply in a mirrored thread addressed to `old`, from the agent its work
/// was reassigned to:
/// `[senator::copilot]（接手 [senator::claude]）<body>`.
pub(crate) fn thread_mirror_covering(
    l: Locale,
    names: SeatNames<'_>,
    event: &Event,
    old: &SenatorName,
) -> String {
    let label = covering_label(l, names, event, old);
    let body = body(l, event);
    match quoted_for(event, &body) {
        None => format!("{label} {body}"),
        Some(quote) => format!("{label}\n{quote}"),
    }
}

/// The same for a people group, signed.
pub(crate) fn signed_covering(
    l: Locale,
    names: SeatNames<'_>,
    event: &Event,
    old: &SenatorName,
) -> String {
    format!(
        "{}\n\n{}",
        thread_mirror_covering(l, names, event, old),
        signature_of(l, &covering_label(l, names, event, old), &event.committee)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use senate_core::{Address, CheckpointFiles, CheckpointPayload, CheckpointTrigger, Kind};

    use crate::render::tests::no_names;
    use crate::tests_support::assert_senate_vocabulary;

    fn path(s: &str) -> SenatorName {
        s.parse().expect("senator path")
    }

    fn name(s: &str) -> Name {
        Name::new(s).expect("name")
    }

    fn committee() -> CommitteeId {
        CommitteeId::new("refund-v2").expect("committee")
    }

    fn run(state: WorkState) -> WorkRun {
        WorkRun {
            run_id: 7,
            requester: name("bob"),
            target_agent: path("claude"),
            instruction: "把重试路径做完".into(),
            branch: "senate/refund-v2/speaker-7".into(),
            state,
            base_branch: Some("senate/refund-v2/alice".into()),
            base_commit: Some("a41f9c2e0000".into()),
            repo_url: Some("git@github.com:o/r.git".into()),
            checkpoint_seq: Some(Seq(3)),
            requested_in: Some(Seq(7)),
            pr_url: None,
            pushed_commit: None,
            summary: None,
            next_steps: Vec::new(),
        }
    }

    fn record(pushed: bool) -> CheckpointRecord {
        CheckpointRecord {
            seq: Seq(40),
            ts: chrono::Utc::now(),
            agent: path("claude"),
            body: String::new(),
            checkpoint: CheckpointPayload {
                branch: "senate/refund-v2/alice".into(),
                commit: "a41f9c2e0000".into(),
                pushed,
                remote_url: Some("git@github.com:o/r.git".into()),
                files: CheckpointFiles {
                    progress: Some("step 2 of 3".into()),
                    decisions: None,
                    open: None,
                },
                session_transcript: false,
                trigger: CheckpointTrigger::Handoff,
            },
        }
    }

    #[test]
    fn the_restatement_names_branch_commit_instruction_and_whom() {
        let text = confirm_work(Locale::Zh, &run(WorkState::Queued));
        for part in [
            "[senator::claude] 的检查点 senate/refund-v2/alice@a41f9c2",
            "senate/refund-v2/speaker-7",
            "做：把重试路径做完",
            "代表 [senator::bob]",
            "回复「对」确认",
        ] {
            assert!(text.contains(part), "{part}: {text}");
        }
        let mut r = run(WorkState::Queued);
        r.instruction = "finish the retry path".into();
        assert_eq!(
            confirm_work(Locale::En, &r),
            "I understand: Branch senate/refund-v2/speaker-7 off [senator::claude]'s checkpoint senate/refund-v2/alice@a41f9c2 and do: finish the retry path; then push and open a PR, on behalf of [senator::bob]. [senator::claude]'s own branch is not touched. Reply \"yes\" to confirm."
        );
        assert_eq!(
            work_root(Locale::En, &r),
            "Coding task: finish the retry path\nFor [senator::bob], from [senator::claude]'s checkpoint senate/refund-v2/alice@a41f9c2, branch senate/refund-v2/speaker-7"
        );
    }

    #[test]
    fn a_failed_run_lists_numbered_next_steps() {
        let mut r = run(WorkState::TimedOut);
        r.pushed_commit = Some("fffeee1234".into());
        r.summary = Some("Stopped at the time limit.".into());
        r.next_steps = vec!["重试".into(), "换个做法".into(), "交给一个人".into()];
        let text = work_result(Locale::Zh, &r, None);
        assert!(text.contains("时间上限"), "{text}");
        assert!(text.contains("分支：senate/refund-v2/speaker-7（fffeee1，已推送）"));
        assert!(
            text.contains("1. 重试\n2. 换个做法\n3. 交给一个人"),
            "{text}"
        );
        r.instruction = "finish the retry path".into();
        r.next_steps = vec!["retry".into(), "try another way".into()];
        assert_eq!(
            work_result(Locale::En, &r, None),
            "Coding task hit its time limit and stopped: finish the retry path\nStopped at the time limit.\nBranch: senate/refund-v2/speaker-7 (fffeee1, pushed)\nNext, you can:\n1. retry\n2. try another way"
        );
        let mut ok = run(WorkState::Succeeded);
        ok.pr_url = Some("https://github.com/o/r/pull/1".into());
        ok.next_steps = vec!["x".into()];
        let text = work_result(Locale::Zh, &ok, None);
        assert!(
            text.starts_with("代码任务完成（代表 [senator::bob]）"),
            "{text}"
        );
        assert!(text.contains("PR：https://github.com/o/r/pull/1"), "{text}");
        assert!(!text.contains("接下来"));
        let text = work_result(Locale::En, &ok, Some("Delivered."));
        assert!(
            text.starts_with("Coding task done (for [senator::bob]): "),
            "{text}"
        );
        assert!(
            text.ends_with("PR: https://github.com/o/r/pull/1\nDelivered."),
            "{text}"
        );
        assert!(!text.contains("Next"), "{text}");
    }

    #[test]
    fn a_covering_answer_says_who_took_over() {
        let e = senate_core::NewEvent::new(
            committee(),
            Kind::Answer,
            Address::senator(&path("copilot")),
            "做到第二步了",
        )
        .in_thread(Seq(3))
        .into_event(Seq(9), chrono::Utc::now());
        let old = path("claude");
        assert_eq!(
            thread_mirror_covering(Locale::Zh, no_names(), &e, &old),
            "[senator::copilot]（接手 [senator::claude]） 做到第二步了"
        );
        assert!(
            signed_covering(Locale::Zh, no_names(), &e, &old)
                .contains("由 [senator::copilot]（接手 [senator::claude]） 回答"),
            "{}",
            signed_covering(Locale::Zh, no_names(), &e, &old)
        );
        assert_eq!(
            thread_mirror_covering(Locale::En, no_names(), &e, &old),
            "[senator::copilot] (covering for [senator::claude]) 做到第二步了"
        );
        assert!(
            signed_covering(Locale::En, no_names(), &e, &old).ends_with(
                "\n\nAnswered by [senator::copilot] (covering for [senator::claude]) · from the refund-v2 Agent group"
            )
        );
    }

    #[test]
    fn reassignment_names_agents_as_mentions() {
        let (alice, bob) = (path("claude"), path("copilot"));
        assert_eq!(
            reassigned(Locale::Zh, &alice, &bob),
            "好的，[senator::claude] 的工作已交给 [senator::copilot]。"
        );
        assert_eq!(
            reassigned(Locale::En, &alice, &bob),
            "OK, [senator::claude]'s work is now with [senator::copilot]."
        );
        assert_eq!(
            work_handback_line(Locale::En, &alice),
            "[senator::claude]'s work is back with [senator::claude]"
        );
        let note = reassign_note(
            Locale::En,
            &committee(),
            &alice,
            &name("bob"),
            Some(&name("carol")),
            Some(&record(true)),
            &BranchNaming::default(),
        );
        assert!(
            note.starts_with(
                "[senator::claude]'s work on refund-v2 is yours now (confirmed by [senator::carol])."
            ),
            "{note}"
        );
        assert!(
            note.contains(
                "git fetch origin && git switch -c senate/refund-v2/bob origin/senate/refund-v2/alice"
            ),
            "{note}"
        );
        assert!(
            note.contains("\n\n--- PROGRESS.md ---\nstep 2 of 3"),
            "{note}"
        );
        let note = reassign_note(
            Locale::Zh,
            &committee(),
            &alice,
            &name("bob"),
            None,
            None,
            &BranchNaming::default(),
        );
        assert!(note.contains("[senator::claude] 没有留下检查点"), "{note}");
    }

    #[test]
    fn no_work_text_uses_the_old_vocabulary() {
        let c = committee();
        let (alice, bob) = (path("claude"), path("copilot"));
        let e = senate_core::NewEvent::new(c.clone(), Kind::Answer, Address::senator(&bob), "ok")
            .in_thread(Seq(3))
            .into_event(Seq(9), chrono::Utc::now());
        for l in Locale::ALL {
            let texts = [
                confirm_work(l, &run(WorkState::Queued)),
                work_root(l, &run(WorkState::Queued)),
                work_started(l, Seq(7)),
                work_unconfigured(l),
                not_a_developer(l),
                agents_cannot_work(l),
                work_busy(l, Seq(7)),
                work_whom(l),
                work_needs_binding(l),
                no_checkpoint(l, &alice),
                checkpoint_detached(l, &alice),
                checkpoint_no_remote(l, &alice),
                checkpoint_not_pushed(l, &alice, &record(false)),
                repo_refused(l, "r"),
                work_reason(l, &run(WorkState::Running)),
                work_reason(l, &run(WorkState::Failed)),
                work_result(l, &run(WorkState::Failed), None),
                work_result(l, &run(WorkState::Refused), None),
                work_interrupted(l),
                work_conflict(l, Seq(7)),
                pr_failed(l, "r"),
                describe_reassign(l, &c, &alice, &bob),
                describe_handback(l, &c, &alice, &bob),
                reassigned(l, &alice, &bob),
                handed_back(l, &alice),
                already_reassigned(l, &alice, &bob),
                not_reassigned(l, &alice),
                reassign_refused(l, "r"),
                reassigned_line(l, &alice, &bob),
                work_handback_line(l, &alice),
                reassign_note(
                    l,
                    &c,
                    &alice,
                    &name("bob"),
                    Some(&name("carol")),
                    Some(&record(false)),
                    &BranchNaming::new("team"),
                ),
                reassign_note(
                    l,
                    &c,
                    &alice,
                    &name("bob"),
                    None,
                    None,
                    &BranchNaming::default(),
                ),
                work_handback_note(l, &c, &alice),
                thread_mirror_covering(l, no_names(), &e, &alice),
                signed_covering(l, no_names(), &e, &alice),
            ];
            for text in texts {
                assert_senate_vocabulary(&text);
            }
        }
    }
}
