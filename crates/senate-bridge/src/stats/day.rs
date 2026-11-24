use super::{Counts, GAP_SECONDS, Progress, Range, Rank, StatsDay, StatsReply};
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

fn day(db: &Connection, range: Range, date: NaiveDate) -> anyhow::Result<StatsDay> {
    let mut day = StatsDay::empty(date);
    let (begin, end) = range.bounds(date);
    if begin >= end {
        return Ok(day);
    }
    let mut by_session: BTreeMap<String, (String, String, String, Counts)> = BTreeMap::new();
    let mut starts = db.prepare_cached(
        "SELECT id,runtime,committee,repo FROM stat_sessions WHERE started>=?1 AND started<?2",
    )?;
    let mut start = starts.query(params![begin, end])?;
    while let Some(row) = start.next()? {
        by_session.insert(
            row.get(0)?,
            (
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                Counts {
                    sessions_started: 1,
                    ..Counts::default()
                },
            ),
        );
    }
    let mut stmt = db.prepare_cached(
        "SELECT s.id,p.at,p.kind,p.input,p.output,s.runtime,s.committee,s.repo
        FROM stat_points p JOIN stat_sessions s ON s.id=p.session
        WHERE p.at>=?1 AND p.at<?2 ORDER BY s.id,p.at,p.rowid",
    )?;
    let mut rows = stmt.query(params![begin - GAP_SECONDS, end + GAP_SECONDS])?;
    let mut last: Option<(String, i64)> = None;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        let at: i64 = row.get(1)?;
        let kind: String = row.get(2)?;
        let activity = matches!(
            kind.as_str(),
            "prompt" | "delivered" | "agent" | "command" | "edit" | "mcp" | "tool"
        );
        let counts = &mut by_session
            .entry(id.clone())
            .or_insert((row.get(5)?, row.get(6)?, row.get(7)?, Counts::default()))
            .3;
        if activity {
            if let Some((prev, since)) = last.as_ref()
                && *prev == id
                && at - since <= GAP_SECONDS
                && !matches!(kind.as_str(), "prompt" | "delivered")
            {
                let seconds = (at.min(end) - (*since).max(begin)).max(0) as u64;
                if seconds > 0 {
                    counts.active_seconds += seconds;
                    counts.sessions_active = 1;
                }
            }
            last = Some((id.clone(), at));
        }
        if at < begin || at >= end {
            continue;
        }
        if activity {
            counts.sessions_active = 1;
            if let Some(time) = DateTime::from_timestamp(at, 0) {
                day.hours[time.with_timezone(&range.tz).hour() as usize] += 1;
            }
        }
        match kind.as_str() {
            "prompt" => counts.prompts += 1,
            "turn" => counts.agent_turns += 1,
            "command" => {
                counts.tool_calls += 1;
                counts.commands += 1;
            }
            "edit" => {
                counts.tool_calls += 1;
                counts.edits += 1;
            }
            "mcp" => {
                counts.tool_calls += 1;
                counts.mcp_calls += 1;
            }
            "tool" => counts.tool_calls += 1,
            "tokens" => {
                let runtime: String = row.get(5)?;
                let tokens = counts.tokens.entry(runtime).or_default();
                tokens.input = tokens
                    .input
                    .saturating_add(u64::try_from(row.get::<_, i64>(3)?)?);
                tokens.output = tokens
                    .output
                    .saturating_add(u64::try_from(row.get::<_, i64>(4)?)?);
            }
            _ => {}
        }
    }
    for (_, (runtime, committee, repo, counts)) in by_session {
        if counts == Counts::default() {
            continue;
        }
        if !repo.is_empty() {
            *day.repositories.entry(repo).or_default() += counts.active_seconds;
        }
        day.counts.merge(&counts);
        day.by_runtime.entry(runtime).or_default().merge(&counts);
        if !committee.is_empty() {
            day.by_committee
                .entry(committee)
                .or_default()
                .merge(&counts);
        }
    }
    let mut logs = db.prepare_cached("SELECT at,counts FROM stat_logs WHERE at>=?1 AND at<?2")?;
    let mut logs = logs.query(params![begin, end])?;
    while let Some(row) = logs.next()? {
        let ts: i64 = row.get(0)?;
        let counts: Counts = serde_json::from_slice(&row.get::<_, Vec<u8>>(1)?)?;
        if let Some(time) = DateTime::from_timestamp(ts, 0) {
            day.hours[time.with_timezone(&range.tz).hour() as usize] += 1;
        }
        day.counts.merge(&counts);
    }
    Ok(day)
}

/// Rebuild dirty days in the history worker, with a separate transaction for
/// each day so readers can see progress as it commits.
pub(crate) fn warm_days(
    db: &mut Connection,
    range: Range,
    stopped: impl Fn() -> bool,
) -> anyhow::Result<()> {
    let name = range.tz.name();
    let today = Utc::now().with_timezone(&range.tz).date_naive();
    let cached = {
        let mut stmt =
            db.prepare_cached("SELECT date FROM stat_days WHERE tz=?1 AND date BETWEEN ?2 AND ?3")?;
        stmt.query_map(
            params![name, range.from.to_string(), range.to.to_string()],
            |row| row.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()?
    };
    for date in range.dates() {
        if stopped() {
            break;
        }
        let date_key = date.to_string();
        if date != today && cached.contains(&date_key) {
            continue;
        }
        let tx = db.transaction()?;
        let (begin, end) = range.bounds(date);
        let value = day(&tx, range, date)?;
        tx.execute(
            "INSERT OR REPLACE INTO stat_days VALUES(?1,?2,?3,?4,?5)",
            params![name, date_key, begin, end, serde_json::to_vec(&value)?],
        )?;
        tx.commit()?;
    }
    // Keep a small, bounded cache of alternate zones.
    db.execute("DELETE FROM stat_days WHERE tz NOT IN (SELECT tz FROM stat_days GROUP BY tz ORDER BY max(rowid) DESC LIMIT 3)", [])?;
    Ok(())
}

pub(crate) fn report(
    db: Option<&Connection>,
    range: Range,
    sessions: Option<crate::ipc::SessionIndexProgress>,
    logs_current: bool,
) -> anyhow::Result<StatsReply> {
    let days_total = (range.to - range.from).num_days() as usize + 1;
    let mut report = StatsReply {
        from: range.from, to: range.to, tz: range.tz.name().into(), days: Vec::new(),
        totals: Counts::default(), current_streak: 0, longest_streak: 0,
        hours: [0;24], top_committees: Vec::new(), top_repositories: Vec::new(), indexing: true,
        progress: Progress { days_cached: 0, days_total, sessions, logs_current },
        notes: vec![
            "Recorded local sessions are attributed to their current or last known committee.".into(),
            "Active time sums observed within-turn intervals of at most five minutes, split at local midnight. Concurrent sessions add together.".into(),
            "Dates and activity hours use the requested timezone. Changing it re-buckets the same recorded facts.".into(),
            "Active-session totals sum daily counts. Current streak ends on the last requested date.".into(),
            "Senate counts cover the served Senate’s readable committee logs. Received means explicitly addressed to you or a registered local agent, not confirmed delivery.".into(),
            "Thread counts use the first recorded action of each kind. Approval counts cover explicit decisions preserved in committee audit events.".into(),
            "agy usage covers recognized Gemini generation records. Missing or unsupported usage is omitted.".into(),
            "A missing token entry means no usable usage was recorded in that period. Recorded usage is not a billing estimate.".into(),
        ],
    };
    let mut by_day = BTreeMap::new();
    if let Some(db) = db
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name='stat_days')",
            [],
            |row| row.get::<_, bool>(0),
        )?
    {
        let mut stmt =
            db.prepare("SELECT day FROM stat_days WHERE tz=?1 AND date BETWEEN ?2 AND ?3")?;
        let mut rows = stmt.query(params![
            range.tz.name(),
            range.from.to_string(),
            range.to.to_string()
        ])?;
        while let Some(row) = rows.next()? {
            let day: StatsDay = serde_json::from_slice(&row.get::<_, Vec<u8>>(0)?)?;
            by_day.insert(day.date, day);
        }
    }
    report.progress.days_cached = by_day.len();
    report.indexing =
        by_day.len() != days_total || report.progress.sessions.is_some() || !logs_current;
    let mut committees = BTreeMap::new();
    let mut repos = BTreeMap::new();
    for date in range.dates() {
        let day = by_day
            .remove(&date)
            .unwrap_or_else(|| StatsDay::empty(date));
        report.totals.merge(&day.counts);
        report.current_streak = if day.counts.counts_as_activity() {
            report.current_streak + 1
        } else {
            0
        };
        report.longest_streak = report.longest_streak.max(report.current_streak);
        for (total, count) in report.hours.iter_mut().zip(day.hours) {
            *total += count;
        }
        for (name, c) in &day.by_committee {
            *committees.entry(name.clone()).or_default() += c.active_seconds;
        }
        for (name, secs) in &day.repositories {
            *repos.entry(name.clone()).or_default() += secs;
        }
        report.days.push(day);
    }
    let rank = |values: BTreeMap<String, u64>| {
        let mut ranks: Vec<_> = values
            .into_iter()
            .filter(|(_, active_seconds)| *active_seconds > 0)
            .map(|(name, active_seconds)| Rank {
                name,
                active_seconds,
            })
            .collect();
        ranks.sort_by_key(|r| (std::cmp::Reverse(r.active_seconds), r.name.clone()));
        ranks.truncate(10);
        ranks
    };
    report.top_committees = rank(committees);
    report.top_repositories = rank(repos);
    Ok(report)
}

pub(crate) fn summary(report: &StatsReply) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let total = &report.totals;
    let _ = writeln!(text, "{} to {} ({})", report.from, report.to, report.tz);
    let _ = writeln!(
        text,
        "Sessions started {} · Active session-days {} · Active time {}m",
        total.sessions_started,
        total.sessions_active,
        total.active_seconds / 60
    );
    let _ = writeln!(
        text,
        "{} prompts · {} agent turns · {} tool calls ({} commands, {} edits, {} MCP)",
        total.prompts,
        total.agent_turns,
        total.tool_calls,
        total.commands,
        total.edits,
        total.mcp_calls
    );
    let _ = writeln!(
        text,
        "Messages sent {} · Received {} · Approvals decided {}",
        total.sent.values().sum::<u64>(),
        total.received.values().sum::<u64>(),
        total.approvals_decided
    );
    let _ = writeln!(
        text,
        "Threads opened {} · Closed {} · Taken {} · Answered {}",
        total.threads_opened, total.threads_closed, total.threads_taken, total.threads_answered
    );
    let _ = writeln!(
        text,
        "Streak in days {} · Longest {}",
        report.current_streak, report.longest_streak
    );
    for (runtime, tokens) in &total.tokens {
        let _ = writeln!(
            text,
            "{runtime} recorded tokens: {} input, {} output",
            tokens.input, tokens.output
        );
    }
    for day in &report.days {
        if day.counts == Counts::default() {
            continue;
        }
        let _ = writeln!(
            text,
            "{}  {:>3} active  {:>5}m  {:>3} prompts  {:>3} turns",
            day.date,
            day.counts.sessions_active,
            day.counts.active_seconds / 60,
            day.counts.prompts,
            day.counts.agent_turns
        );
    }
    if report.indexing {
        let _ = writeln!(
            text,
            "Indexing: {} of {} days cached",
            report.progress.days_cached, report.progress.days_total
        );
    }
    text
}
