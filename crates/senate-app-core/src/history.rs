//! Ended and stopped sessions, as the app's History reads them: a page of
//! them, a search across past and live conversations, and forgetting one.

use std::time::SystemTime;

use senate_bridge::ipc::{SessionHistoryRow, SessionSearchHit, SessionSearchKind};
use senate_core::alias::short_name;

use crate::text;

/// One ended or stopped session.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HistoryRow {
    pub session_id: String,
    /// The name people are shown: its shown name, else its alias, else its
    /// senator's short name.
    pub name: String,
    /// What a restore names it by: `committee::alias`, else its senator.
    pub label: String,
    pub committee: Option<String>,
    pub runtime: String,
    /// `opus · high`, when not the runtime's default.
    pub model: Option<String>,
    pub dir: String,
    pub branch: Option<String>,
    pub title: Option<String>,
    pub started_at: SystemTime,
    /// The last activity seen (not necessarily when it stopped).
    pub last_activity: SystemTime,
    /// Its transcript can still be read.
    pub readable: bool,
    pub restorable: bool,
    pub item_count: Option<u64>,
}

/// One page of History, newest first; `next` reads the page after.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct HistoryPage {
    pub rows: Vec<HistoryRow>,
    pub next: Option<String>,
}

/// Where a search matched: in what someone typed, a reply, or a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum SearchKind {
    User,
    Reply,
    Tool,
}

/// One match: which session, which item, the text around it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchHit {
    pub session_id: String,
    pub seq: u64,
    pub kind: SearchKind,
    /// Plain text around the match.
    pub snippet: String,
    pub at: Option<SystemTime>,
    /// The session is live (in the sidebar), not ended.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub next: Option<String>,
    /// The daemon is still indexing: these hits are partial (and `next`
    /// is none); ask the same query again, from the first page.
    pub indexing: bool,
    /// Sessions indexed so far, of how many, while indexing.
    pub indexed: Option<u64>,
    pub total: Option<u64>,
}

impl From<SessionHistoryRow> for HistoryRow {
    fn from(r: SessionHistoryRow) -> Self {
        let label = match (&r.committee, &r.alias) {
            (Some(c), Some(a)) => format!("{c}::{a}"),
            _ => r.primary.to_string(),
        };
        Self {
            session_id: r.session_id,
            name: r
                .shown_name
                .clone()
                .or_else(|| r.alias.as_ref().map(ToString::to_string))
                .unwrap_or_else(|| short_name(&r.primary)),
            label,
            committee: r.committee.map(|c| c.to_string()),
            runtime: r.runtime.as_str().to_string(),
            model: text::model_label(&r.model),
            dir: r.dir.display().to_string(),
            branch: r.branch,
            title: r.title,
            started_at: r.started_at.into(),
            last_activity: r.last_activity.into(),
            readable: r.readable,
            restorable: r.restorable,
            item_count: r.item_count,
        }
    }
}

impl From<SessionSearchHit> for SearchHit {
    fn from(h: SessionSearchHit) -> Self {
        Self {
            session_id: h.session_id,
            seq: h.seq,
            kind: match h.kind {
                SessionSearchKind::UserMessage => SearchKind::User,
                SessionSearchKind::AssistantMessage => SearchKind::Reply,
                SessionSearchKind::ToolCall => SearchKind::Tool,
            },
            snippet: h.snippet,
            at: h.ts.map(Into::into),
            live: h.live,
        }
    }
}
