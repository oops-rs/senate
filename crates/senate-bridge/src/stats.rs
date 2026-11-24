//! Local activity counts. The history index stores only numeric facts and
//! committee/repository names for this view, never bodies or full paths.

use std::collections::BTreeMap;

use chrono::{Datelike, Duration, NaiveDate, TimeZone};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

mod day;
mod log;
mod scan;
pub(crate) use day::{report, summary, warm_days};
pub(crate) use log::{log_page, log_progress};
pub(crate) use scan::{SCHEMA, Scan, Usage, forget, invalidate_source, session};
pub const GAP_SECONDS: i64 = 300;
pub const MAX_DAYS: i64 = 366;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsReply {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub tz: String,
    pub days: Vec<StatsDay>,
    pub totals: Counts,
    pub current_streak: u32,
    pub longest_streak: u32,
    pub hours: [u64; 24],
    pub top_committees: Vec<Rank>,
    pub top_repositories: Vec<Rank>,
    pub indexing: bool,
    pub progress: Progress,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub days_cached: usize,
    pub days_total: usize,
    pub sessions: Option<crate::ipc::SessionIndexProgress>,
    pub logs_current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rank {
    pub name: String,
    pub active_seconds: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub sessions_started: u64,
    pub sessions_active: u64,
    pub active_seconds: u64,
    pub prompts: u64,
    pub agent_turns: u64,
    pub tool_calls: u64,
    pub commands: u64,
    pub edits: u64,
    pub mcp_calls: u64,
    pub sent: BTreeMap<String, u64>,
    pub received: BTreeMap<String, u64>,
    pub threads_opened: u64,
    pub threads_closed: u64,
    pub threads_taken: u64,
    pub threads_answered: u64,
    pub approvals_decided: u64,
    /// Absent runtimes have no comparable recorded usage in this period.
    pub tokens: BTreeMap<String, Tokens>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    /// Recorded input including cached input. Never a billing estimate.
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsDay {
    pub date: NaiveDate,
    #[serde(default, skip_serializing_if = "counts_empty")]
    pub counts: Counts,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_runtime: BTreeMap<String, SessionCounts>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_committee: BTreeMap<String, SessionCounts>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repositories: BTreeMap<String, u64>,
    /// Histogram of recorded activity points in the local hour.
    #[serde(default, skip_serializing_if = "hours_empty")]
    pub hours: [u64; 24],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCounts {
    pub sessions_started: u64,
    pub sessions_active: u64,
    pub active_seconds: u64,
}

impl SessionCounts {
    fn merge(&mut self, counts: &Counts) {
        self.sessions_started += counts.sessions_started;
        self.sessions_active += counts.sessions_active;
        self.active_seconds += counts.active_seconds;
    }
}

fn counts_empty(counts: &Counts) -> bool {
    *counts == Counts::default()
}
fn hours_empty(hours: &[u64; 24]) -> bool {
    hours.iter().all(|count| *count == 0)
}

impl StatsDay {
    fn empty(date: NaiveDate) -> Self {
        Self {
            date,
            counts: Counts::default(),
            by_runtime: BTreeMap::new(),
            by_committee: BTreeMap::new(),
            repositories: BTreeMap::new(),
            hours: [0; 24],
        }
    }
}

impl Counts {
    fn merge(&mut self, other: &Self) {
        macro_rules! add {
            ($($field:ident),*) => { $(self.$field = self.$field.saturating_add(other.$field);)* };
        }
        add!(
            sessions_started,
            sessions_active,
            active_seconds,
            prompts,
            agent_turns,
            tool_calls,
            commands,
            edits,
            mcp_calls,
            threads_opened,
            threads_closed,
            threads_taken,
            threads_answered,
            approvals_decided
        );
        for (dst, src) in [
            (&mut self.sent, &other.sent),
            (&mut self.received, &other.received),
        ] {
            for (key, count) in src {
                *dst.entry(key.clone()).or_default() += count;
            }
        }
        for (runtime, tokens) in &other.tokens {
            let total = self.tokens.entry(runtime.clone()).or_default();
            total.input = total.input.saturating_add(tokens.input);
            total.output = total.output.saturating_add(tokens.output);
        }
    }

    fn counts_as_activity(&self) -> bool {
        self.sessions_active != 0
            || !self.sent.is_empty()
            || self.threads_taken != 0
            || self.approvals_decided != 0
            || self.threads_closed != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Range {
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub tz: Tz,
}

impl Range {
    pub fn new(from: NaiveDate, to: NaiveDate, tz: &str) -> Result<Self, crate::ipc::IpcError> {
        let tz = tz.parse().map_err(|_| {
            crate::ipc::IpcError::bad_request("use an IANA timezone such as America/New_York")
        })?;
        if from > to || (to - from).num_days() >= MAX_DAYS || from.year() < 1970 || to.year() > 9998
        {
            return Err(crate::ipc::IpcError::bad_request(
                "choose up to 366 consecutive local dates",
            ));
        }
        Ok(Self { from, to, tz })
    }

    fn dates(self) -> impl Iterator<Item = NaiveDate> {
        self.from
            .iter_days()
            .take_while(move |date| *date <= self.to)
    }

    fn bounds(self, date: NaiveDate) -> (i64, i64) {
        // Some zones have skipped midnight, or even a whole civil date.
        // Find the first valid minute at or after midnight. On ambiguous
        // midnights the earlier occurrence begins the day.
        let start = |date: NaiveDate| {
            let start = date.and_hms_opt(0, 0, 0).unwrap_or_default();
            (0..=1440)
                .find_map(|minute| {
                    self.tz
                        .from_local_datetime(&(start + Duration::minutes(minute)))
                        .earliest()
                })
                .map_or(start.and_utc().timestamp(), |at| at.timestamp())
        };
        (start(date), start(date + Duration::days(1)))
    }
}
