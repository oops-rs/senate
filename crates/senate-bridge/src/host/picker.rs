//! Choosing one hosted session (or one of an alias's senators) on the
//! terminal: a full-screen list, one line per choice.
//!
//! | key | does |
//! | --- | --- |
//! | ↑ ↓, `k` `j` | move |
//! | PgUp / PgDn | a screen up, down |
//! | Enter | choose |
//! | `q`, Esc, Ctrl-C, Ctrl-] | cancel |
//!
//! `senate attach` with no name opens it over `senate session list`' rows
//! ([`pick_session`]); a bare alias that several committees use opens it
//! over those senators ([`pick`]).

use std::fmt::Write as _;

use senate_core::SenatorName;
use textwrap::core::display_width;

use super::terminal::{RESET_DISPLAY, RawTerminal};
use super::view::{Key, keys};
use crate::ipc::SessionRow;

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// What it is called: `committee::alias`, else the senator name.
    pub label: String,
    /// The rest of its line: committee, runtime, state, directory.
    pub detail: String,
    /// The senator chosen by choosing it.
    pub primary: SenatorName,
    /// Shown dim: a stopped session.
    pub dim: bool,
}

impl Choice {
    /// A session as `senate session list` lists it.
    pub fn of_row(row: &SessionRow) -> Self {
        let state = if row.paused {
            "paused"
        } else {
            row.state.as_str()
        };
        let mut detail = format!("{} · {state}", row.runtime.as_str());
        if row.attached {
            detail.push_str(" · attached");
        }
        let _ = write!(detail, " · {}", row.dir.display());
        Self {
            label: row.label(),
            detail,
            primary: row.primary.clone(),
            dim: !row.is_running(),
        }
    }
}

/// Lets the person choose one of `rows` under `prompt`: the chosen
/// session's senator, or `None` when they cancel. Fails when not run
/// on a terminal.
pub async fn pick_session(
    rows: &[SessionRow],
    prompt: &str,
) -> anyhow::Result<Option<SenatorName>> {
    let choices: Vec<Choice> = rows.iter().map(Choice::of_row).collect();
    pick(&choices, prompt).await
}

/// [`pick_session`] over any `choices`.
pub async fn pick(choices: &[Choice], prompt: &str) -> anyhow::Result<Option<SenatorName>> {
    anyhow::ensure!(!choices.is_empty(), "nothing to choose from");
    let terminal = RawTerminal::open(true).await?;
    let mut list = List::new(choices, prompt);
    let mut pending = Vec::new();
    let mut buf = [0u8; 256];
    let outcome = loop {
        let (rows, cols) = terminal.window().unwrap_or((24, 80));
        terminal.draw(list.paint(rows, cols).as_bytes()).await?;
        let n = terminal.read(&mut buf).await?;
        if n == 0 {
            break None;
        }
        pending.extend_from_slice(&buf[..n]);
        // A lone Esc read on its own is the key, not the start of one.
        if pending == [0x1b] {
            break None;
        }
        let mut done = None;
        for key in keys(&mut pending) {
            if let Some(outcome) = list.step(key, rows) {
                done = Some(outcome);
                break;
            }
        }
        if let Some(outcome) = done {
            break outcome;
        }
    };
    terminal.draw(RESET_DISPLAY).await?;
    Ok(outcome.map(|i| choices[i].primary.clone()))
}

/// The list's state: which line is focused, and which shows first.
struct List<'a> {
    choices: &'a [Choice],
    prompt: &'a str,
    focus: usize,
    top: usize,
}

/// Rows above the list (the prompt and a blank) and below it (the keys).
const CHROME: u16 = 3;

impl<'a> List<'a> {
    fn new(choices: &'a [Choice], prompt: &'a str) -> Self {
        Self {
            choices,
            prompt,
            focus: 0,
            top: 0,
        }
    }

    fn height(rows: u16) -> usize {
        usize::from(rows.saturating_sub(CHROME).max(1))
    }

    /// `Some(Some(i))`: chose line `i`; `Some(None)`: cancelled.
    fn step(&mut self, key: Key, rows: u16) -> Option<Option<usize>> {
        let last = self.choices.len().saturating_sub(1);
        let page = Self::height(rows);
        match key {
            Key::Up | Key::Left => self.focus = self.focus.saturating_sub(1),
            Key::Down | Key::Right => self.focus = (self.focus + 1).min(last),
            Key::PageUp => self.focus = self.focus.saturating_sub(page),
            Key::PageDown => self.focus = (self.focus + page).min(last),
            Key::Scroll(_) | Key::Typing => {}
            Key::Open => return Some(Some(self.focus)),
            Key::Quit | Key::Detach => return Some(None),
        }
        if self.focus < self.top {
            self.top = self.focus;
        } else if self.focus >= self.top + page {
            self.top = self.focus + 1 - page;
        }
        None
    }

    fn paint(&self, rows: u16, cols: u16) -> String {
        let width = usize::from(cols);
        let mut out = String::from("\x1b[?25l\x1b[H\x1b[2J");
        let _ = write!(out, "\x1b[1m{}\x1b[0m\r\n\r\n", clip(self.prompt, width));
        let label_width = self
            .choices
            .iter()
            .map(|c| display_width(&c.label))
            .max()
            .unwrap_or(0);
        for (i, choice) in self
            .choices
            .iter()
            .enumerate()
            .skip(self.top)
            .take(Self::height(rows))
        {
            let pad = label_width.saturating_sub(display_width(&choice.label));
            let line = format!(
                "{} {}{}  {}",
                if i == self.focus { '>' } else { ' ' },
                choice.label,
                " ".repeat(pad),
                choice.detail
            );
            let style = match (i == self.focus, choice.dim) {
                (true, _) => "\x1b[7m",
                (false, true) => "\x1b[2m",
                (false, false) => "",
            };
            let _ = write!(out, "{style}{}\x1b[0m\r\n", clip(&line, width));
        }
        let _ = write!(
            out,
            "\x1b[{rows};1H\x1b[2m{}\x1b[0m",
            clip("↑/↓ move · Enter chooses · q or Esc cancels", width)
        );
        out
    }
}

/// `text` cut to `width` columns.
pub(super) fn clip(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = display_width(ch.encode_utf8(&mut [0; 4]));
        if used + w > width {
            break;
        }
        used += w;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choice(label: &str, dim: bool) -> Choice {
        Choice {
            label: label.into(),
            detail: "codex · running · /work".into(),
            primary: label.rsplit("::").next().unwrap().parse().expect("a name"),
            dim,
        }
    }

    #[test]
    fn keys_move_scroll_choose_and_cancel() {
        let choices: Vec<Choice> = (0..10)
            .map(|i| choice(&format!("c::s{i}"), false))
            .collect();
        let mut list = List::new(&choices, "Which session?");
        // Five rows: two lines of the list fit.
        assert_eq!(list.step(Key::Up, 5), None);
        assert_eq!(list.focus, 0);
        list.step(Key::Down, 5);
        list.step(Key::Down, 5);
        assert_eq!((list.focus, list.top), (2, 1), "scrolled to keep it shown");
        list.step(Key::PageDown, 5);
        assert_eq!(list.focus, 4);
        for _ in 0..20 {
            list.step(Key::Down, 5);
        }
        assert_eq!(list.focus, 9, "stops at the last");
        list.step(Key::PageUp, 5);
        assert_eq!(list.focus, 7);
        assert_eq!(list.step(Key::Open, 5), Some(Some(7)));
        assert_eq!(list.step(Key::Quit, 5), Some(None));
        assert_eq!(list.step(Key::Detach, 5), Some(None));
    }

    #[test]
    fn the_focused_line_is_marked_and_a_stopped_one_dim() {
        let choices = vec![
            choice("refund-v2::reviewer", false),
            choice("ios::reviewer", true),
        ];
        let list = List::new(&choices, "reviewer is on several committees");
        let painted = list.paint(10, 80);
        assert!(painted.contains("reviewer is on several committees"));
        assert!(
            painted.contains("\x1b[7m> refund-v2::reviewer  codex"),
            "{painted:?}"
        );
        assert!(painted.contains("\x1b[2m  ios::reviewer"), "{painted:?}");
        assert!(painted.contains("q or Esc cancels"));
    }

    #[test]
    fn the_raw_keys_include_ctrl_c_as_cancel() {
        let mut pending = b"j\x1b[B\r\x03".to_vec();
        assert_eq!(
            keys(&mut pending),
            [Key::Down, Key::Down, Key::Open, Key::Quit]
        );
    }
}
