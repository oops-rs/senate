//! The machine's limited desk-worker API. It grants no person or senator identity.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{CommitteeId, Event, EventId, Name, Office, Seq};

pub const ADMIN_PATH: &str = "/v1/local/desk-worker";
pub const IDENTITY_PATH: &str = "/v1/desk-worker/identity";
pub const QUESTIONS_PATH: &str = "/v1/desk-worker/questions";
pub const REPLIES_PATH: &str = "/v1/desk-worker/replies";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Admin {
    Issue {
        worker: Name,
        committees: BTreeSet<CommitteeId>,
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
    pub worker: Name,
    pub senate: Name,
    pub epoch: String,
    pub committees: BTreeSet<CommitteeId>,
    pub office: Option<Office>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: EventId,
    pub committee: CommitteeId,
    pub question: String,
    #[serde(default)]
    pub caller: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Receipt {
    pub id: EventId,
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
