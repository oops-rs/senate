//! Typed committee activity records shared by the sidebar and Town.
use std::collections::HashMap;
use std::time::SystemTime;

use senate_bridge::ipc;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CommitteeThreadPage {
    pub rows: Vec<CommitteeThread>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CommitteeThread {
    pub committee: String,
    pub root: u64,
    pub title: String,
    pub kind: String,
    pub opener: String,
    pub addressees: Vec<String>,
    pub owner: Option<String>,
    pub state: String,
    pub waits_on: Option<String>,
    pub escalated_to: Option<String>,
    pub takeover: Option<String>,
    pub last_seq: u64,
    pub last_at: Option<SystemTime>,
    pub last_author: Option<String>,
    pub message_count: Option<u64>,
    pub other_committee: Option<String>,
}

impl From<ipc::CommitteeThreadPage> for CommitteeThreadPage {
    fn from(page: ipc::CommitteeThreadPage) -> Self {
        Self {
            rows: page.rows.into_iter().map(Into::into).collect(),
            next_cursor: page.next_cursor,
        }
    }
}

impl From<ipc::CommitteeThread> for CommitteeThread {
    fn from(row: ipc::CommitteeThread) -> Self {
        Self {
            committee: row.committee.to_string(),
            root: row.root.0,
            title: row.title,
            kind: row.kind,
            opener: row.opener,
            addressees: row.addressees,
            owner: row.owner,
            state: row.state,
            waits_on: row.waits_on,
            escalated_to: row.escalated_to,
            takeover: row.takeover,
            last_seq: row.last_seq.0,
            last_at: row.last_at.map(Into::into),
            last_author: row.last_author,
            message_count: row.message_count,
            other_committee: row.other_committee.map(|c| c.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ActivityBatch {
    pub events: Vec<ActivityEntry>,
    pub cursors: HashMap<String, u64>,
    pub reset: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ActivityEntry {
    pub committee: String,
    pub seq: u64,
    pub at: SystemTime,
    pub kind: String,
    pub from: String,
    pub to: Vec<String>,
    pub on_behalf_of: Option<String>,
    pub thread: Option<u64>,
    pub line: String,
    pub state: Option<String>,
}

impl From<ipc::ActivityBatch> for ActivityBatch {
    fn from(batch: ipc::ActivityBatch) -> Self {
        Self {
            events: batch
                .events
                .into_iter()
                .map(|entry| ActivityEntry {
                    committee: entry.committee.to_string(),
                    seq: entry.seq.0,
                    at: entry.at.into(),
                    kind: entry.kind,
                    from: entry.from,
                    to: entry.to,
                    on_behalf_of: entry.on_behalf_of,
                    thread: entry.thread.map(|seq| seq.0),
                    line: entry.line,
                    state: entry.state,
                })
                .collect(),
            cursors: batch
                .cursors
                .into_iter()
                .map(|(c, seq)| (c.to_string(), seq.0))
                .collect(),
            reset: batch.reset,
        }
    }
}

/// Activity callbacks run on the core's runtime, like timeline callbacks.
#[uniffi::export(foreign)]
pub trait ActivityListener: Send + Sync {
    fn on_batch(&self, batch: ActivityBatch);
    fn on_end(&self, error: Option<String>);
}
