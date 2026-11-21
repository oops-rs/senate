//! The prompt the coding agent gets. Written once, in English; the agent
//! is told which language to write its closing summary in, so what reaches
//! people is in the order's locale.

use senate_core::{Address, Locale};

pub use crate::order::NOTES_DIR;
use crate::order::{WorkOrder, checkpoint_notes};

/// The language's name, as the prompt asks for it.
fn language(locale: Locale) -> &'static str {
    match locale {
        Locale::Zh => "Chinese",
        Locale::En => "English",
    }
}

/// The prompt for `claude -p`: who it works for, whose checkpoint it starts
/// from, the checkpoint notes, the task, the rules, and how to close.
pub fn build_prompt(order: &WorkOrder) -> String {
    let notes = checkpoint_notes(&order.checkpoint_files);
    let notes_line = if notes.is_empty() {
        "There are no checkpoint notes for this run.".to_owned()
    } else {
        let names: Vec<String> = notes
            .iter()
            .map(|(name, _)| format!("{NOTES_DIR}/{name}"))
            .collect();
        format!(
            "The agent's latest checkpoint notes are in {}. Read them first: \
             progress says where the work stands, decisions are in force and must be \
             followed, open lists the questions still open.",
            names.join(", ")
        )
    };
    format!(
        "You are the Senate Speaker's coding agent for the committee `{committee}`, working on \
         behalf of {person} ({requester}).\n\
         \n\
         The current directory is a checkout of {slug} at commit {commit}, the latest \
         checkpoint of {agent} on branch `{base}`. Your work will be committed to a new branch \
         `{branch}` and opened as a pull request for review; the developer's own branch is \
         never changed.\n\
         \n\
         {notes_line}\n\
         \n\
         The task, in {person}'s words:\n\
         <task>\n{instruction}\n</task>\n\
         \n\
         Rules:\n\
         - Work only inside the current directory. There is no git repository here and no \
         credentials: do not run git, do not push, do not try to reach other hosts. The \
         runner records and pushes your changes when you stop.\n\
         - Do not edit or delete anything under {NOTES_DIR}/.\n\
         - Keep the change focused on the task. Follow the project's conventions. Build \
         and run the relevant tests if the project has them, and fix what you broke.\n\
         - Do not print, copy or store secrets or environment variables.\n\
         - If you cannot finish, stop with the tree in the best state you can: partial work \
         is kept and a person picks it up from there.\n\
         \n\
         End your final message with these two parts, written in {language} (keep the two \
         markers in English):\n\
         SUMMARY: <one or two sentences on what you did and what state it is in>\n\
         NEXT STEPS:\n\
         - <a step a person should take next>\n\
         - <another, if any>\n",
        committee = order.committee,
        person = order.on_behalf_of.trim(),
        requester = Address::senior(&order.requester).in_text(),
        agent = Address::senator(&order.target_agent).in_text(),
        slug = order.repo.slug,
        commit = order.base_commit,
        base = order.base_branch,
        branch = order.branch(),
        instruction = order.instruction.trim(),
        language = language(order.locale),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::sample_order;
    use senate_core::CheckpointFiles;

    #[test]
    fn prompt_names_person_agent_checkpoint_branch_and_task() {
        let p = build_prompt(&sample_order());
        for want in [
            "committee `refund-v2`",
            "on behalf of Bob ([senator::bob])",
            "example/refund at commit 0123456789abcdef0123456789abcdef01234567",
            "checkpoint of [senator::claude] on branch `senate/refund-v2/alice`",
            "new branch `senate/refund-v2/speaker-7`",
            ".senate/PROGRESS.md, .senate/OPEN.md",
            "<task>\n把 alice 的重试路径做完\nwith tests\n</task>",
            "do not run git, do not push",
            "written in Chinese",
            "SUMMARY:",
            "NEXT STEPS:",
        ] {
            assert!(p.contains(want), "missing {want:?} in\n{p}");
        }
        assert!(!p.contains("DECISIONS.md"));
        assert!(!p.contains("user:"));
        // Checkpoint contents are in files, not in the prompt.
        assert!(!p.contains("retry path half done"));
    }

    #[test]
    fn prompt_without_notes_and_in_english() {
        let mut o = sample_order();
        o.checkpoint_files = CheckpointFiles::default();
        o.locale = Locale::En;
        let p = build_prompt(&o);
        assert!(p.contains("There are no checkpoint notes"));
        assert!(p.contains("written in English"));
    }
}
