//! The machine's limited desk-worker API. It grants no person or senator identity.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{Address, CommitteeId, Event, EventId, Kind, Name, Office, Seq, meta};

pub const ADMIN_PATH: &str = "/v1/local/desk-worker";
pub const IDENTITY_PATH: &str = "/v1/desk-worker/identity";
pub const QUESTIONS_PATH: &str = "/v1/desk-worker/questions";
pub const REPLIES_PATH: &str = "/v1/desk-worker/replies";
pub const GUEST_PLATFORM: &str = "desk-worker";

/// Terminal controls and Unicode format controls must never cross a display boundary.
pub fn is_display_control(ch: char) -> bool {
    ch.is_control()
        || unicode_general_category::get_general_category(ch)
            == unicode_general_category::GeneralCategory::Format
}

/// Normalize worker text once so downstream log readers get the same safe lines.
pub fn display_body(body: &str) -> String {
    body.replace("\r\n", "\n")
        .chars()
        .map(|ch| match ch {
            '\r' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}' => '\n',
            '\n' | '\t' => ch,
            ch if is_display_control(ch) => ' ',
            ch => ch,
        })
        .collect()
}

pub fn is_question(event: &Event) -> bool {
    event.kind == Kind::Question
        && event.thread.is_none()
        && event.to == [Address::Speaker]
        && event
            .from
            .as_guest()
            .is_some_and(|guest| guest.platform() == GUEST_PLATFORM)
}

pub fn is_intake(event: &Event) -> bool {
    is_question(event)
        && event
            .meta
            .get(meta::DESK_WORKER_INTAKE)
            .and_then(serde_json::Value::as_bool)
            == Some(true)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Admin {
    Issue {
        worker: Name,
        committees: BTreeSet<CommitteeId>,
        #[serde(default)]
        desk_inbox: Option<CommitteeId>,
    },
    Rotate {
        id: EventId,
    },
    Revoke {
        id: EventId,
    },
}

#[derive(Serialize, Deserialize)]
pub struct Issued {
    pub id: EventId,
    pub token: String,
}

#[derive(Serialize, Deserialize)]
pub struct Identity {
    pub id: EventId,
    pub worker: Name,
    pub senate: Name,
    pub epoch: String,
    pub committees: BTreeSet<CommitteeId>,
    pub desk_inbox: Option<CommitteeId>,
    pub office: Option<Office>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: EventId,
    #[serde(default)]
    pub committee: Option<CommitteeId>,
    #[serde(default)]
    pub desk_intake: bool,
    pub question: String,
    #[serde(default)]
    pub caller: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Receipt {
    pub id: EventId,
    pub committee: CommitteeId,
    pub seq: Seq,
    pub worker: Name,
    pub caller: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepliesQuery {
    pub committee: CommitteeId,
    pub root: Seq,
    pub after: Seq,
}

#[derive(Serialize, Deserialize)]
pub struct Replies {
    pub events: Vec<Event>,
    pub next: Seq,
}

#[cfg(test)]
mod tests {
    #[test]
    fn display_normalizes_line_breaks_and_removes_terminal_and_format_controls() {
        assert_eq!(
            super::display_body("line one\rline two\u{2028}line\u{1b} three\u{202e}\r\nend"),
            "line one\nline two\nline  three \nend"
        );
    }
}
