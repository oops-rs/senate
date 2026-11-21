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
//!   prose rarely looks like it.
//!
//! While both hold the host keeps a marker file, `screen-prompt`, in its
//! directory, holding only when the screen went quiet. The daemon reads it
//! beside the approval notices ([`crate::daemon`]): no screen text is
//! written anywhere.

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
    let row = row.trim_start();
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

/// Keep the marker while the screen is quiet and shows a choice. Never
/// returns.
pub(super) async fn watch(dir: &Path, screen: &Screen, quiet: Duration) -> anyhow::Result<()> {
    let marker = dir.join(MARKER);
    let quiet = u64::try_from(quiet.as_nanos()).unwrap_or(u64::MAX);
    let mut shown: Option<ScreenPrompt> = None;
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let since = screen.last_output();
        let waiting =
            observed_at().saturating_sub(since) >= quiet && looks_like_choice(&screen.rows());
        let now = waiting.then_some(ScreenPrompt { since });
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
        // The TUI writes again: the wait is over.
        screen.output(b"\x1b[2J");
        tokio::select! {
            _ = &mut watching => unreachable!(),
            () = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        assert!(!marker.exists());
    }
}
