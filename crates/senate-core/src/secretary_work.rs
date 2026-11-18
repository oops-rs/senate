//! Explicit work shared by a principal and its appointed secretary sessions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{CommitteeId, EventId, SenatorName, Seq, StatusState, Step};

/// The committee is part of a thread's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkThread {
    pub committee: CommitteeId,
    pub root: Seq,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetSecretarySession {
    pub senator: SenatorName,
    pub appointed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acted_by: Option<SenatorName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretaryWorkRequest {
    /// The principal speaking as itself, or a member's stable senator id.
    pub actor: SenatorName,
    #[serde(flatten)]
    pub operation: SecretaryWorkOp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SecretaryWorkOp {
    Members,
    List {
        #[serde(default)]
        after: Option<EventId>,
    },
    Read {
        item: EventId,
        #[serde(default)]
        offset: usize,
    },
    Create {
        id: EventId,
        title: String,
        #[serde(default)]
        thread: Option<WorkThread>,
    },
    Change {
        item: EventId,
        id: EventId,
        update: SecretaryWorkUpdate,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretaryWorkUpdate {
    Claim,
    Assign {
        to: SenatorName,
    },
    Progress {
        state: StatusState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steps: Option<Vec<Step>>,
    },
    Note {
        text: String,
    },
    /// With no destination, the item is available for another member to claim.
    Handoff {
        #[serde(default)]
        to: Option<SenatorName>,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretaryWorkItem {
    pub id: EventId,
    pub title: String,
    pub thread: Option<WorkThread>,
    pub created_by: SenatorName,
    pub created_at: DateTime<Utc>,
    pub state: StatusState,
    pub owner: Option<SenatorName>,
    pub steps: Vec<Step>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretaryWorkEntry {
    pub id: EventId,
    pub by: SenatorName,
    pub at: DateTime<Utc>,
    pub update: SecretaryWorkUpdate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum SecretaryWorkReply {
    Members {
        principal: SenatorName,
        members: Vec<SenatorName>,
    },
    List {
        items: Vec<SecretaryWorkItem>,
        next: Option<EventId>,
    },
    History {
        item: SecretaryWorkItem,
        entries: Vec<SecretaryWorkEntry>,
        next: Option<usize>,
    },
    Saved {
        item: SecretaryWorkItem,
        duplicate: bool,
    },
}

/// Contains enough to find the item from the assigned session.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SecretaryReminder {
    pub principal: SenatorName,
    pub item: EventId,
    pub id: EventId,
    pub to: SenatorName,
}
