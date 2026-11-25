//! A hosted TUI waiting on a choice Senate does not otherwise know about.
//!
//! Senate hears of a runtime's permission prompts from its hooks (Claude)
//! or its app-server (Codex). Anything else a TUI may stop on, a "hooks
//! need review" or a trust question at startup, it never hears of, and the
//! session sits there with nobody told. The host keeps the screen, so it
//! can see the two things such a wait looks like, whatever the runtime:
//!
//! - the TUI has written nothing for [`QUIET`]: a turn that is running
//!   keeps repainting its spinner or its timer;
//! - the bottom of the screen is a choice menu: options numbered in order,
//!   `1.` then `2.`, on nearby lines, one of them marked by the selection
//!   glyph a TUI puts before the highlighted option (`›`, `❯`, `>`, `▶`,
//!   `→`). That is how Claude Code, Codex and agy draw a selection, and
//!   prose rarely looks like it. A standalone “Esc to dismiss” footer also
//!   identifies a modal that needs input.
//!
//! Once both hold the host writes a marker file, `screen-prompt`, with
//! the time the screen went quiet. The marker stays until the choice
//! disappears. The daemon reads it beside the approval notices
//! ([`crate::daemon`]). No screen text is written anywhere.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{directory, observed_at, watch::Screen};
use crate::paths::Paths;
use senate_core::EventId;

/// How long the TUI must have written nothing, unless the daemon says
/// otherwise (`Timing::screen_prompt_quiet`).
pub(crate) const QUIET: Duration = Duration::from_secs(15);
/// How often the host looks.
const EVERY: Duration = Duration::from_millis(500);
/// How many rows up from the last one drawn a menu is looked for in.
const BOTTOM_ROWS: usize = 20;
/// The most rows between two consecutive options.
const OPTION_GAP: usize = 4;
const MARKER: &str = "screen-prompt";
/// The glyphs a TUI puts before the highlighted option.
const SELECTED: [char; 5] = ['›', '❯', '>', '▶', '→'];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ScreenPrompt {
    /// When the TUI last wrote, [`observed_at`]'s clock.
    pub since: u64,
}

/// What the host's marker says, if it has one.
pub(crate) fn read(paths: &Paths, id: &EventId) -> Option<ScreenPrompt> {
    let data = std::fs::read(directory(paths, id).join(MARKER)).ok()?;
    serde_json::from_slice(&data).ok()
}

/// One row as an option: its number, and whether it is marked selected.
fn option(row: &str) -> Option<(u32, bool)> {
    // A dialog drawn in a box starts its rows with the box's side.
    let row = row.trim_start_matches(|c: char| c == '│' || c.is_whitespace());
    let (selected, rest) = match row.chars().next() {
        Some(c) if SELECTED.contains(&c) => (true, row[c.len_utf8()..].trim_start()),
        _ => (false, row),
    };
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if !(1..=2).contains(&digits) {
        return None;
    }
    let after = &rest[digits..];
    let label = after
        .strip_prefix('.')
        .or_else(|| after.strip_prefix(')'))?;
    // `1. Yes`: a separator, then words.
    if !label.starts_with(' ') || label.trim().is_empty() {
        return None;
    }
    Some((rest[..digits].parse().ok()?, selected))
}

/// Whether the bottom of the screen is a choice menu.
pub(crate) fn looks_like_choice(rows: &[String]) -> bool {
    // The bottom of what is drawn: a TUI may leave the rows below blank.
    let end = rows
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .map_or(0, |last| last + 1);
    let start = end.saturating_sub(BOTTOM_ROWS);
    let options: Vec<(usize, u32, bool)> = rows[start..end]
        .iter()
        .enumerate()
        .filter_map(|(at, row)| option(row).map(|(n, selected)| (at, n, selected)))
        .collect();
    // A run of options 1, 2, … on nearby rows, with one selected.
    let mut run: Option<(usize, u32, bool)> = None;
    for &(at, n, selected) in &options {
        run = match run {
            Some((last, prev, marked)) if n == prev + 1 && at - last <= OPTION_GAP => {
                if marked || selected {
                    return true;
                }
                Some((at, n, false))
            }
            _ if n == 1 => Some((at, 1, selected)),
            _ => None,
        };
    }
    false
}

/// A modal can also end with a standalone dismissal hint inside a box.
pub(super) fn looks_like_prompt(rows: &[String]) -> bool {
    let footer = rows
        .iter()
        .rev()
        .map(|row| row.trim_matches(|c: char| c.is_whitespace() || "│─╰╯└┘".contains(c)))
        .find(|row| !row.is_empty());
    footer.is_some_and(|text| {
        text.eq_ignore_ascii_case("esc to dismiss")
            || text.eq_ignore_ascii_case("press esc to dismiss")
    }) || looks_like_choice(rows)
}

/// The options of the choice menu at the bottom of the screen, numbered in
/// order from 1, with the label of each, if [`looks_like_choice`] sees one.
fn bottom_menu(rows: &[String]) -> Option<Vec<(u32, String)>> {
    let end = rows
        .iter()
        .rposition(|row| !row.trim().is_empty())
        .map_or(0, |last| last + 1);
    let start = end.saturating_sub(BOTTOM_ROWS);
    let mut runs: Vec<Vec<(u32, String)>> = Vec::new();
    let mut last_at = 0;
    for (at, row) in rows[start..end].iter().enumerate() {
        let row = row.trim_matches(|c: char| c == '│' || c.is_whitespace());
        let Some((n, _)) = option(row) else { continue };
        let label = row
            .trim_start_matches(|c: char| SELECTED.contains(&c) || c.is_whitespace())
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')'])
            .trim()
            .to_owned();
        match runs.last_mut() {
            Some(run)
                if run.last().is_some_and(|(prev, _)| n == prev + 1)
                    && at - last_at <= OPTION_GAP =>
            {
                run.push((n, label));
            }
            _ if n == 1 => runs.push(vec![(1, label)]),
            _ => {}
        }
        last_at = at;
    }
    looks_like_choice(rows).then(|| runs.pop()).flatten()
}

/// How a person answers a Claude Code permission dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PermissionChoice {
    /// `1. Yes`.
    Once,
    /// `Yes, and don't ask again …` or `Yes, allow all edits …`.
    Always,
    /// `No, and tell Claude what to do differently`.
    Deny,
}

/// Why a choice cannot be made on this screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoChoice {
    /// The bottom of the screen is not Claude Code's permission dialog.
    NotShown,
    /// The dialog offers no such option (not every prompt has "always").
    NoOption,
}

/// The key that picks `choice` in the Claude Code permission dialog at the
/// bottom of the screen: the option's number, which Claude Code takes as
/// choosing it. The dialog is told by its options: `1. Yes` first, an
/// option starting `No` among them, and nothing that is neither.
pub(crate) fn permission_key(rows: &[String], choice: PermissionChoice) -> Result<char, NoChoice> {
    let menu = bottom_menu(rows).ok_or(NoChoice::NotShown)?;
    let yes = |label: &str| label == "Yes";
    let always = |label: &str| label.starts_with("Yes, ");
    let no = |label: &str| label == "No" || label.starts_with("No,") || label.starts_with("No ");
    let shaped = menu.first().is_some_and(|(_, label)| yes(label))
        && menu.iter().any(|(_, label)| no(label))
        && menu
            .iter()
            .all(|(_, label)| yes(label) || always(label) || no(label));
    if !shaped || menu.len() > 9 {
        return Err(NoChoice::NotShown);
    }
    let wanted: &dyn Fn(&str) -> bool = match choice {
        PermissionChoice::Once => &yes,
        PermissionChoice::Always => &always,
        PermissionChoice::Deny => &no,
    };
    let (n, _) = menu
        .iter()
        .find(|(_, label)| wanted(label))
        .ok_or(NoChoice::NoOption)?;
    char::from_digit(*n, 10).ok_or(NoChoice::NotShown)
}

/// Record a choice once the screen is quiet, and keep it while a choice
/// remains visible. A repaint alone does not settle it.
pub(super) async fn watch(dir: &Path, screen: &Screen, quiet: Duration) -> anyhow::Result<()> {
    let marker = dir.join(MARKER);
    let quiet = u64::try_from(quiet.as_nanos()).unwrap_or(u64::MAX);
    let mut shown: Option<ScreenPrompt> = None;
    // The last screen read, and what it looked like: an idle TUI's screen
    // is read once, not twice a second for as long as it stays idle.
    let mut looked: Option<(u64, bool)> = None;
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let since = screen.last_output();
        let now = if (shown.is_some() || observed_at().saturating_sub(since) >= quiet) && {
            // Before the rows: a change in between is seen at the next tick.
            let revision = screen.revision();
            match looked {
                Some((at, looks)) if at == revision => looks,
                _ => {
                    let looks = looks_like_prompt(&screen.rows());
                    looked = Some((revision, looks));
                    looks
                }
            }
        } {
            shown.or(Some(ScreenPrompt { since }))
        } else {
            None
        };
        if now == shown {
            continue;
        }
        let recorded = match now {
            Some(prompt) => (|| -> std::io::Result<()> {
                let staged = marker.with_extension("tmp");
                std::fs::write(&staged, serde_json::to_vec(&prompt)?)?;
                std::fs::rename(staged, &marker)
            })(),
            None => match std::fs::remove_file(&marker) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
        };
        match recorded {
            Ok(()) => shown = now,
            Err(error) => tracing::warn!(%error, "recording a prompt on the hosted screen"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: &[&str]) -> Vec<String> {
        let mut screen = vec![String::new(); 24 - rows.len().min(24)];
        screen.extend(rows.iter().map(|r| (*r).to_owned()));
        screen
    }

    #[test]
    fn a_dismissible_overlay_waits_for_input() {
        let rows = screen(&[
            "│ New features │",
            "│ Esc to dismiss │",
            "╰────────────────╯",
        ]);
        assert!(looks_like_prompt(&rows));
        assert_eq!(
            permission_key(&rows, PermissionChoice::Once),
            Err(NoChoice::NotShown)
        );
        assert!(!looks_like_prompt(&screen(&[
            "Esc to dismiss",
            "› Type your prompt"
        ])));
    }

    #[test]
    fn the_menus_runtimes_draw_are_choices() {
        // Codex, at startup after `setup install` changed its hooks.
        assert!(looks_like_choice(&screen(&[
            "  Hooks need review",
            "  3 hooks are new or changed.",
            "",
            "› 1. Review hooks",
            "  2. Trust all and continue",
            "  3. Continue without them",
            "",
            "  Press enter to confirm or esc to go back",
        ])));
        // Claude Code.
        assert!(looks_like_choice(&screen(&[
            " Do you trust the files in this folder?",
            " ❯ 1. Yes, proceed",
            "   2. No, exit",
            "",
            " Enter to confirm · Esc to cancel",
        ])));
        // The selection need not be on the first option.
        assert!(looks_like_choice(&screen(
            &["   1) Keep", " > 2) Replace",]
        )));
        // Drawn at the top of a screen otherwise blank.
        let mut top = vec!["› 1. Yes".to_owned(), "  2. No".to_owned()];
        top.extend(std::iter::repeat_n(String::new(), 40));
        assert!(looks_like_choice(&top));
    }

    #[test]
    fn prose_and_idle_prompts_are_not() {
        // An answer with a numbered list, then the idle input.
        assert!(!looks_like_choice(&screen(&[
            "Here is the plan:",
            "1. Read the code",
            "2. Fix the bug",
            "3. Add a test",
            "",
            "────────────────",
            "> ",
            "────────────────",
        ])));
        // A selection glyph but only one option, or out of order.
        assert!(!looks_like_choice(&screen(&["› 1. Only"])));
        assert!(!looks_like_choice(&screen(&["› 2. Two", "  1. One"])));
        // Options too far apart to be one menu.
        assert!(!looks_like_choice(&screen(&[
            "› 1. One",
            "",
            "",
            "",
            "",
            "",
            "  2. Two",
        ])));
        // A version number is not an option.
        assert!(!looks_like_choice(&screen(&["› 1.2.3", "  2.0.0"])));
        // A menu scrolled far above the bottom is not waiting.
        let mut old = vec!["› 1. Yes".to_owned(), "  2. No".to_owned()];
        old.extend(std::iter::repeat_n("output".to_owned(), BOTTOM_ROWS));
        assert!(!looks_like_choice(&old));
    }

    /// Claude Code 2.x, a Bash permission dialog.
    const BASH_DIALOG: &[&str] = &[
        " Bash command",
        "",
        "   cargo test",
        "   Run the tests",
        "",
        " Do you want to proceed?",
        " ❯ 1. Yes",
        "   2. Yes, and don't ask again for cargo test commands in /work",
        "   3. No, and tell Claude what to do differently (esc)",
    ];

    #[test]
    fn claude_permission_dialogs_are_answered_by_their_option_number() {
        let rows = screen(BASH_DIALOG);
        assert_eq!(permission_key(&rows, PermissionChoice::Once), Ok('1'));
        assert_eq!(permission_key(&rows, PermissionChoice::Always), Ok('2'));
        assert_eq!(permission_key(&rows, PermissionChoice::Deny), Ok('3'));
        // An edit dialog drawn in a box; the selection moved.
        let boxed = screen(&[
            "│ Do you want to make this edit to main.rs?                  │",
            "│   1. Yes                                                   │",
            "│ ❯ 2. Yes, allow all edits during this session (shift+tab) │",
            "│   3. No, and tell Claude what to do differently (esc)      │",
        ]);
        assert_eq!(permission_key(&boxed, PermissionChoice::Always), Ok('2'));
        assert_eq!(permission_key(&boxed, PermissionChoice::Deny), Ok('3'));
        // Without an "always" option, only that choice is refused.
        let two = screen(&[" Do you want to proceed?", " ❯ 1. Yes", "   2. No"]);
        assert_eq!(permission_key(&two, PermissionChoice::Deny), Ok('2'));
        assert_eq!(
            permission_key(&two, PermissionChoice::Always),
            Err(NoChoice::NoOption)
        );
    }

    #[test]
    fn other_menus_are_not_permission_dialogs() {
        // The trust question at startup has a Yes, but its options are not
        // a permission dialog's.
        let trust = screen(&[
            " Do you trust the files in this folder?",
            " ❯ 1. Yes, proceed",
            "   2. No, exit",
        ]);
        assert_eq!(
            permission_key(&trust, PermissionChoice::Once),
            Err(NoChoice::NotShown)
        );
        let hooks = screen(&[
            "› 1. Review hooks",
            "  2. Trust all and continue",
            "  3. Continue without them",
        ]);
        assert_eq!(
            permission_key(&hooks, PermissionChoice::Deny),
            Err(NoChoice::NotShown)
        );
        // An idle prompt, and a dialog scrolled away.
        assert_eq!(
            permission_key(&screen(&["> "]), PermissionChoice::Once),
            Err(NoChoice::NotShown)
        );
        let mut old = screen(BASH_DIALOG);
        old.extend(std::iter::repeat_n("output".to_owned(), BOTTOM_ROWS));
        assert_eq!(
            permission_key(&old, PermissionChoice::Once),
            Err(NoChoice::NotShown)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_marker_is_kept_while_a_quiet_screen_shows_a_choice() {
        // Paused time drives the ticks; the quiet is measured on the
        // monotonic clock, so it is faked by an old last output.
        let dir = tempfile::tempdir().unwrap();
        let screen = Screen::new(24, 80);
        screen.output(b"\x1b[20;1H\xe2\x80\xba 1. Yes\r\n  2. No");
        let marker = dir.path().join(MARKER);
        let watching = watch(dir.path(), &screen, QUIET);
        tokio::pin!(watching);
        tokio::select! {
            _ = &mut watching => unreachable!(),
            () = tokio::time::sleep(Duration::from_secs(3)) => {}
        }
        // Just written: not quiet yet.
        assert!(!marker.exists());
        screen.set_last_output(observed_at().saturating_sub(QUIET.as_nanos() as u64 + 1));
        tokio::select! {
            _ = &mut watching => unreachable!(),
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        let prompt: ScreenPrompt =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        assert_eq!(prompt.since, screen.last_output());
        // A repaint with the same choices keeps the original notice.
        screen.output(b"\x1b[20;1H  1. Yes\r\n\xe2\x80\xba 2. No");
        tokio::select! {
            _ = &mut watching => unreachable!(),
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        let redrawn: ScreenPrompt =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        assert_eq!(redrawn, prompt);
        // The choices disappear: the wait is over.
        screen.output(b"\x1b[2J");
        tokio::select! {
            _ = &mut watching => unreachable!(),
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        assert!(!marker.exists());
    }
}
