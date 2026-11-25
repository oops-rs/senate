//! The machine's limited desk-worker API. It grants no person or senator identity.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{Address, CommitteeId, Event, EventId, Kind, Name, Office, Seq, meta};

pub const ADMIN_PATH: &str = "/v1/local/desk-worker";
pub const IDENTITY_PATH: &str = "/v1/desk-worker/identity";
pub const QUESTIONS_PATH: &str = "/v1/desk-worker/questions";
pub const REPLIES_PATH: &str = "/v1/desk-worker/replies";
pub const GUEST_PLATFORM: &str = "desk-worker";

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
