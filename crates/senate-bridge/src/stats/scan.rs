use super::Tokens;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

pub(crate) const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS stat_sessions (
        id TEXT PRIMARY KEY, started INTEGER NOT NULL, runtime TEXT NOT NULL,
        committee TEXT NOT NULL, repo TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS stat_scans (id TEXT PRIMARY KEY, state BLOB NOT NULL);
    CREATE TABLE IF NOT EXISTS stat_points (
        session TEXT NOT NULL, key TEXT NOT NULL, at INTEGER NOT NULL,
        kind TEXT NOT NULL, input INTEGER NOT NULL, output INTEGER NOT NULL,
        PRIMARY KEY(session,key)
    );
    CREATE INDEX IF NOT EXISTS stat_points_time ON stat_points(at);
    CREATE INDEX IF NOT EXISTS stat_points_session_time ON stat_points(session,at);
    CREATE TABLE IF NOT EXISTS stat_logs (
        committee TEXT NOT NULL, seq INTEGER NOT NULL, at INTEGER NOT NULL, counts BLOB NOT NULL,
        PRIMARY KEY(committee,seq)
    );
    CREATE INDEX IF NOT EXISTS stat_logs_time ON stat_logs(at);
    CREATE TABLE IF NOT EXISTS stat_threads (
        committee TEXT NOT NULL, root INTEGER NOT NULL, local INTEGER NOT NULL,
        closed INTEGER NOT NULL DEFAULT 0, taken INTEGER NOT NULL DEFAULT 0,
        answered INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(committee,root)
    );
    CREATE TABLE IF NOT EXISTS stat_log_cursors (
        committee TEXT PRIMARY KEY, seq INTEGER NOT NULL, epoch TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS stat_days (
        tz TEXT NOT NULL, date TEXT NOT NULL, begin INTEGER NOT NULL, end INTEGER NOT NULL,
        day BLOB NOT NULL, PRIMARY KEY(tz,date)
    );
    CREATE TRIGGER IF NOT EXISTS stat_point_insert AFTER INSERT ON stat_points BEGIN
        DELETE FROM stat_days WHERE end > new.at - 300 AND begin <= new.at + 300;
    END;
    CREATE TRIGGER IF NOT EXISTS stat_point_delete AFTER DELETE ON stat_points BEGIN
        DELETE FROM stat_days WHERE end > old.at - 300 AND begin <= old.at + 300;
    END;
    CREATE TRIGGER IF NOT EXISTS stat_point_update AFTER UPDATE ON stat_points BEGIN
        DELETE FROM stat_days WHERE end > min(old.at,new.at) - 300 AND begin <= max(old.at,new.at) + 300;
    END;
    CREATE TRIGGER IF NOT EXISTS stat_log_insert AFTER INSERT ON stat_logs BEGIN
        DELETE FROM stat_days WHERE end > new.at AND begin <= new.at;
    END;
    CREATE TRIGGER IF NOT EXISTS stat_log_delete AFTER DELETE ON stat_logs BEGIN
        DELETE FROM stat_days WHERE end > old.at AND begin <= old.at;
    END;
";

pub(crate) fn session(db: &Connection, s: &crate::session::Session) -> anyhow::Result<()> {
    let committee = s.committee().map_or("", senate_core::CommitteeId::as_str);
    let repo = s
        .git_root
        .as_ref()
        .and_then(|root| root.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.chars().any(char::is_control))
        .unwrap_or("");
    let changed = db.execute(
        "INSERT INTO stat_sessions VALUES (?1,?2,?3,?4,?5)
        ON CONFLICT(id) DO UPDATE SET started=excluded.started,runtime=excluded.runtime,
        committee=excluded.committee,repo=excluded.repo
        WHERE started!=excluded.started OR runtime!=excluded.runtime OR committee!=excluded.committee OR repo!=excluded.repo",
        params![s.session_id, s.started_at.timestamp(), s.runtime.as_str(), committee, repo],
    )?;
    if changed != 0 {
        db.execute("DELETE FROM stat_days WHERE (end > ?2 AND begin <= ?2)
            OR EXISTS(SELECT 1 FROM stat_points WHERE session=?1 AND at >= begin-300 AND at < end+300)",
            params![s.session_id, s.started_at.timestamp()])?;
    }
    Ok(())
}

pub(crate) fn point(
    db: &Connection,
    session: &str,
    key: &str,
    at: i64,
    kind: &str,
    tokens: Tokens,
) -> anyhow::Result<()> {
    db.execute(
        "INSERT INTO stat_points VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(session,key) DO UPDATE SET at=min(at,excluded.at),kind=excluded.kind,
         input=max(input,excluded.input),output=max(output,excluded.output)
         WHERE excluded.at<at OR excluded.kind!=kind OR excluded.input>input OR excluded.output>output",
        params![session,key,at,kind,i64::try_from(tokens.input)?,i64::try_from(tokens.output)?],
    )?;
    Ok(())
}

pub(crate) fn forget(db: &Connection, id: &str) -> anyhow::Result<()> {
    db.execute("DELETE FROM stat_days WHERE EXISTS(SELECT 1 FROM stat_sessions WHERE id=?1 AND end>started AND begin<=started)", [id])?;
    invalidate_source(db, id)?;
    db.execute("DELETE FROM stat_sessions WHERE id=?1", [id])?;
    Ok(())
}

pub(crate) fn invalidate_source(db: &Connection, id: &str) -> anyhow::Result<()> {
    db.execute("DELETE FROM stat_scans WHERE id=?1", [id])?;
    db.execute("DELETE FROM stat_points WHERE session=?1", [id])?;
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Scan {
    pub last_seq: u64,
    pub turn_started: bool,
    pub totals: Option<Tokens>,
    pub generation_high: Option<i64>,
}

impl Scan {
    pub(crate) fn timeline(
        &mut self,
        db: &Connection,
        id: &str,
        item: &crate::ipc::TimelineItem,
    ) -> anyhow::Result<()> {
        use crate::ipc::{TimelineKind as K, TimelineToolDetail as D};
        if item.seq <= self.last_seq {
            return Ok(());
        }
        self.last_seq = item.seq;
        let kind = match &item.kind {
            K::UserMessage { .. } => "prompt",
            K::SenateMessage { .. } => "delivered",
            K::AssistantMessage { .. } | K::Reasoning { .. } => "agent",
            K::ToolCall { name, detail, .. } => match detail {
                _ if name.starts_with("mcp__") => "mcp",
                D::Shell { .. } => "command",
                D::Edit { .. } | D::Write { .. } => "edit",
                _ => "tool",
            },
            _ => return Ok(()),
        };
        let agent = !matches!(kind, "prompt" | "delivered");
        if let Some(ts) = item.ts {
            point(
                db,
                id,
                &format!("i{}", item.seq),
                ts.timestamp(),
                kind,
                Tokens::default(),
            )?;
            if agent && !self.turn_started {
                point(
                    db,
                    id,
                    &format!("t{}", item.seq),
                    ts.timestamp(),
                    "turn",
                    Tokens::default(),
                )?;
            }
        }
        self.turn_started = agent;
        Ok(())
    }
}

pub(crate) struct Usage {
    pub key: String,
    pub at: i64,
    pub tokens: Tokens,
    pub cumulative: bool,
}

impl Usage {
    pub(crate) fn jsonl(
        runtime: crate::session::Runtime,
        value: &serde_json::Value,
        at: DateTime<Utc>,
        offset: u64,
    ) -> Option<Self> {
        use crate::session::Runtime;
        let (key, usage, cumulative) = match runtime {
            Runtime::Claude if value["type"] == "assistant" && value["isSidechain"] != true => {
                let message = &value["message"];
                (
                    format!("c{}", message["id"].as_str()?),
                    &message["usage"],
                    false,
                )
            }
            Runtime::Codex
                if value["type"] == "event_msg" && value["payload"]["type"] == "token_count" =>
            {
                (
                    format!("x{offset}"),
                    &value["payload"]["info"]["total_token_usage"],
                    true,
                )
            }
            _ => return None,
        };
        let mut input = usage["input_tokens"].as_u64()?;
        if !cumulative {
            input = input
                .checked_add(usage["cache_creation_input_tokens"].as_u64().unwrap_or(0))?
                .checked_add(usage["cache_read_input_tokens"].as_u64().unwrap_or(0))?;
        }
        Some(Self {
            key,
            at: at.timestamp(),
            tokens: Tokens {
                input,
                output: usage["output_tokens"].as_u64()?,
            },
            cumulative,
        })
    }
}

impl Scan {
    pub(crate) fn usage(&mut self, db: &Connection, id: &str, usage: Usage) -> anyhow::Result<()> {
        let mut tokens = usage.tokens;
        if i64::try_from(tokens.input).is_err() || i64::try_from(tokens.output).is_err() {
            return Ok(());
        }

        if usage.cumulative {
            if let Some(old) = self.totals {
                let (Some(input), Some(output)) = (
                    tokens.input.checked_sub(old.input),
                    tokens.output.checked_sub(old.output),
                ) else {
                    // Without a monotonic cumulative sequence, a backwards
                    // snapshot cannot safely be charged as new usage.
                    return Ok(());
                };
                self.totals = Some(tokens);
                tokens = Tokens { input, output };
            } else {
                self.totals = Some(tokens);
            }
        }
        point(db, id, &usage.key, usage.at, "tokens", tokens)
    }
}
