//! The profile’s typed view of the bridge's local activity report.

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StatsDay {
    pub date: String,
    pub sessions: u64,
    pub sessions_started: u64,
    pub active_secs: u64,
    pub prompts: u64,
    pub agent_turns: u64,
    pub tool_calls: u64,
    pub commands: u64,
    pub edits: u64,
    pub mcp_calls: u64,
    pub messages: u64,
    pub threads_opened: u64,
    pub threads_closed: u64,
    pub threads_taken: u64,
    pub threads_answered: u64,
    pub approvals_decided: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StatsRank {
    pub name: String,
    /// Observed active seconds.
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StatsReport {
    pub days: Vec<StatsDay>,
    pub hours: Vec<u64>,
    pub current_streak: u32,
    pub longest_streak: u32,
    pub top_committees: Vec<StatsRank>,
    pub top_repos: Vec<StatsRank>,
    pub indexing: bool,
    pub indexed: Option<u64>,
    pub total: Option<u64>,
    pub notes: Vec<String>,
}

impl From<senate_bridge::stats::StatsReply> for StatsReport {
    fn from(reply: senate_bridge::stats::StatsReply) -> Self {
        let ranks = |rows: Vec<senate_bridge::stats::Rank>| {
            rows.into_iter()
                .map(|row| StatsRank {
                    name: row.name,
                    count: row.active_seconds,
                })
                .collect()
        };
        let progress = reply.progress.sessions.map_or(
            (reply.progress.days_cached, reply.progress.days_total),
            |p| (p.indexed, p.total),
        );
        Self {
            days: reply
                .days
                .into_iter()
                .map(|day| {
                    let c = day.counts;
                    StatsDay {
                        date: day.date.to_string(),
                        sessions: c.sessions_active,
                        sessions_started: c.sessions_started,
                        active_secs: c.active_seconds,
                        prompts: c.prompts,
                        agent_turns: c.agent_turns,
                        tool_calls: c.tool_calls,
                        commands: c.commands,
                        edits: c.edits,
                        mcp_calls: c.mcp_calls,
                        messages: c.sent.values().chain(c.received.values()).sum(),
                        threads_opened: c.threads_opened,
                        threads_closed: c.threads_closed,
                        threads_taken: c.threads_taken,
                        threads_answered: c.threads_answered,
                        approvals_decided: c.approvals_decided,
                    }
                })
                .collect(),
            hours: reply.hours.into(),
            current_streak: reply.current_streak,
            longest_streak: reply.longest_streak,
            top_committees: ranks(reply.top_committees),
            top_repos: ranks(reply.top_repositories),
            indexing: reply.indexing,
            indexed: reply.indexing.then_some(progress.0 as u64),
            total: reply.indexing.then_some(progress.1 as u64),
            notes: reply.notes,
        }
    }
}
