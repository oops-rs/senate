//! What a senator does on a committee: its duty.
//!
//! A duty is a short description of a seat's job ("reviews the server
//! crate", "owns the macOS app"), set by a `note` carrying
//! [`meta::DUTY`](crate::meta::DUTY) = [`DutyPayload`]. The committee's log
//! is the source of truth: the latest such note per seat wins, and an empty
//! duty clears it ([`DutyFold`]).
//!
//! Who may set a seat's duty is the server's to check, once, on append: the
//! senator itself, the person who owns it, or the committee's chair. A
//! senator's default duty lives in its bridge's local config; the bridge
//! posts it as the seat's duty when the seat has none yet.
//!
//! The duty is what the Speaker routes an unaddressed `senator::*` question
//! by ([`SeatFacts::duty`](crate::SeatFacts::duty)), and what the brief and
//! presence listings show next to each seat.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::address::Address;
use crate::event::{Event, Kind, NewEvent, meta};
use crate::ids::{CommitteeId, Seq};
use crate::senator::SenatorName;

/// The longest duty, in characters. One or two sentences; the rest belongs
/// in the brief or a checkpoint.
pub const MAX_DUTY_CHARS: usize = 300;

/// `meta["duty"]`: the duty of `senator`'s seat on the committee. Empty
/// `duty` clears it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DutyPayload {
    pub senator: SenatorName,
    #[serde(default)]
    pub duty: String,
}

impl DutyPayload {
    pub const META_KEY: &'static str = meta::DUTY;

    /// The payload of a `note` from a senator; `None` for anything else
    /// (the server refuses the key elsewhere, so nothing else is read).
    pub fn of(event: &Event) -> Option<Self> {
        if event.kind != Kind::Note || event.from.as_senator().is_none() {
            return None;
        }
        Self::from_value(event.meta.get(meta::DUTY)?)
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        serde_json::from_value(value.clone()).ok()
    }

    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }

    /// Whether the payload is sound: a duty of at most
    /// [`MAX_DUTY_CHARS`] on one line.
    pub fn check(&self) -> Result<(), String> {
        let chars = self.duty.chars().count();
        if chars > MAX_DUTY_CHARS {
            return Err(format!(
                "a duty is at most {MAX_DUTY_CHARS} characters, not {chars}"
            ));
        }
        if self.duty.contains(['\n', '\r']) {
            return Err("a duty is one line".to_owned());
        }
        Ok(())
    }

    /// The note that sets it, written `from` the setter.
    pub fn note(&self, committee: CommitteeId, from: Address) -> NewEvent {
        let body = if self.duty.is_empty() {
            format!(
                "{} has no duty set.",
                Address::senator(&self.senator).in_text()
            )
        } else {
            format!(
                "Duty of {}: {}",
                Address::senator(&self.senator).in_text(),
                self.duty
            )
        };
        NewEvent::new(committee, Kind::Note, from, body).with_meta(meta::DUTY, self.value())
    }
}

/// Normalizes a duty as given: trimmed, inner whitespace runs (newlines
/// included) folded to one space.
pub fn normalize(duty: &str) -> String {
    duty.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The duty of each seat on one committee, folded from its log: the latest
/// [`DutyPayload`] per senator wins; an empty one clears.
#[derive(Debug, Clone, Default)]
pub struct DutyFold {
    head: Seq,
    duties: BTreeMap<SenatorName, (Seq, String)>,
}

impl DutyFold {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn head(&self) -> Seq {
        self.head
    }

    pub fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        let Some(payload) = DutyPayload::of(event) else {
            return;
        };
        let duty = normalize(&payload.duty);
        if duty.is_empty() {
            self.duties.remove(&payload.senator);
        } else {
            self.duties.insert(payload.senator, (event.seq, duty));
        }
    }

    /// `senator`'s duty here, if it has one.
    pub fn duty(&self, senator: &SenatorName) -> Option<&str> {
        self.duties.get(senator).map(|(_, d)| d.as_str())
    }

    /// Every seat's duty, by senator.
    pub fn duties(&self) -> BTreeMap<SenatorName, String> {
        self.duties
            .iter()
            .map(|(s, (_, d))| (s.clone(), d.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn committee() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn name(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn duty_event(seq: u64, from: Address, senator: &str, duty: &str) -> Event {
        let new = DutyPayload {
            senator: name(senator),
            duty: duty.into(),
        }
        .note(committee(), from);
        new.into_event(Seq(seq), Utc::now())
    }

    #[test]
    fn latest_per_seat_wins_and_empty_clears() {
        let mut fold = DutyFold::new();
        fold.observe(&duty_event(
            1,
            Address::senator(&name("codex")),
            "codex",
            "reviews",
        ));
        fold.observe(&duty_event(
            2,
            Address::senator(&name("wendell")),
            "claude",
            "  owns\n the  app ",
        ));
        fold.observe(&duty_event(
            3,
            Address::senator(&name("codex")),
            "codex",
            "builds the server",
        ));
        assert_eq!(fold.duty(&name("codex")), Some("builds the server"));
        assert_eq!(fold.duty(&name("claude")), Some("owns the app"));
        fold.observe(&duty_event(
            4,
            Address::senator(&name("claude")),
            "claude",
            "",
        ));
        assert_eq!(fold.duty(&name("claude")), None);
        assert_eq!(fold.duties().len(), 1);
        assert_eq!(fold.head(), Seq(4));
    }

    #[test]
    fn only_senators_notes_count() {
        let mut fold = DutyFold::new();
        fold.observe(&duty_event(1, Address::Speaker, "codex", "forged"));
        assert_eq!(fold.duty(&name("codex")), None);
    }

    #[test]
    fn check_bounds_the_duty() {
        let ok = DutyPayload {
            senator: name("codex"),
            duty: "x".repeat(MAX_DUTY_CHARS),
        };
        assert!(ok.check().is_ok());
        let long = DutyPayload {
            senator: name("codex"),
            duty: "x".repeat(MAX_DUTY_CHARS + 1),
        };
        assert!(long.check().is_err());
        let lines = DutyPayload {
            senator: name("codex"),
            duty: "a\nb".into(),
        };
        assert!(lines.check().is_err());
    }
}
