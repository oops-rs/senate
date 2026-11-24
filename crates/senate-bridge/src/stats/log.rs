use super::Counts;
use rusqlite::{Connection, params};
use std::collections::BTreeSet;

pub(crate) fn log_progress(
    db: &Connection,
    committee: &str,
    epoch: &str,
) -> anyhow::Result<senate_core::Seq> {
    use rusqlite::OptionalExtension as _;
    let cursor: Option<i64> = db
        .query_row(
            "SELECT seq FROM stat_log_cursors WHERE committee=?1 AND epoch=?2",
            params![committee, epoch],
            |row| row.get(0),
        )
        .optional()?;
    Ok(senate_core::Seq(cursor.unwrap_or_default() as u64))
}

pub(crate) fn log_page(
    db: &mut Connection,
    committee: &str,
    epoch: &str,
    cursor: senate_core::Seq,
    page: Vec<serde_json::Value>,
    person: &senate_core::Address,
    agents: &BTreeSet<senate_core::SenatorName>,
) -> anyhow::Result<senate_core::Seq> {
    use crate::client::{Decoded, decode_event};
    use senate_core::{Kind, Seq, StatusState, meta, task::StatusPayload};
    let tx = db.transaction()?;
    let mut last = cursor;
    let previous = log_progress(&tx, committee, epoch)?;
    if previous != cursor {
        return Ok(previous);
    }
    if cursor == Seq::ZERO {
        tx.execute("DELETE FROM stat_logs WHERE committee=?1", [committee])?;
        tx.execute("DELETE FROM stat_threads WHERE committee=?1", [committee])?;
    }
    for event in page {
        let Some(seq) = crate::client::seq_of(&event) else {
            continue;
        };
        if seq <= last {
            continue;
        }
        last = seq;
        let Decoded::Event(event) = decode_event(event) else {
            continue;
        };
        let from_person = event.from == *person && event.on_behalf_of.is_none();
        let for_person = event.to.iter().any(|addr| {
            addr == person
                || addr
                    .as_senator()
                    .is_some_and(|agent| agents.contains(agent))
        });
        let from_local = event
            .from
            .as_senator()
            .is_some_and(|agent| agents.contains(agent));
        let mut counts = Counts::default();
        if !event.is_simulated() {
            let is_message = matches!(
                event.kind,
                Kind::Question | Kind::Request | Kind::Answer | Kind::Note
            );
            if is_message && from_person {
                counts.sent.insert(event.kind.as_str().into(), 1);
            }
            if is_message && for_person && !from_person {
                counts.received.insert(event.kind.as_str().into(), 1);
            }
            if event.thread.is_none()
                && matches!(event.kind, Kind::Question | Kind::Request)
                && (from_person || for_person || from_local)
            {
                counts.threads_opened = 1;
                tx.execute(
                    "INSERT OR IGNORE INTO stat_threads(committee,root,local) VALUES (?1,?2,1)",
                    params![committee, i64::try_from(seq.0)?],
                )?;
            }
            if let Some(thread) = event.thread {
                let root = i64::try_from(thread.0)?;
                let status = StatusPayload::of(&event);
                let local = from_person
                    || from_local
                    || for_person
                    || tx.query_row(
                        "SELECT EXISTS(SELECT 1 FROM stat_threads WHERE committee=?1 AND root=?2)",
                        params![committee, root],
                        |row| row.get::<_, bool>(0),
                    )?;
                if local {
                    tx.execute(
                        "INSERT OR IGNORE INTO stat_threads(committee,root,local) VALUES (?1,?2,1)",
                        params![committee, root],
                    )?;
                }
                if let Some(status) = status {
                    if local
                        && status.state == StatusState::Archived
                        && !event.meta.contains_key(meta::HOSTED_APPROVAL)
                    {
                        counts.threads_closed = tx.execute("UPDATE stat_threads SET closed=1 WHERE committee=?1 AND root=?2 AND closed=0",params![committee,root])? as u64;
                    }
                    if from_person
                        && status.takeover.as_ref().is_some_and(|name| {
                            person
                                .as_senator()
                                .is_some_and(|p| p.as_str() == name.as_str())
                        })
                    {
                        counts.threads_taken = tx.execute("UPDATE stat_threads SET taken=1 WHERE committee=?1 AND root=?2 AND taken=0",params![committee,root])? as u64;
                    }
                }
                if from_person && event.kind == Kind::Answer {
                    counts.threads_answered = tx.execute("UPDATE stat_threads SET answered=1 WHERE committee=?1 AND root=?2 AND answered=0",params![committee,root])? as u64;
                }
                // An archived approval notice alone can also mean the
                // terminal or dialog disappeared. Count explicit decisions.
                if from_local
                    && event
                        .meta
                        .get(meta::HOSTED_APPROVAL)
                        .is_some_and(serde_json::Value::is_string)
                    && event.meta.get(meta::APPROVAL_WAIT).is_some_and(|meta| {
                        meta["decision"].is_string()
                            && meta["actor"].as_str().is_some_and(|actor| {
                                person.as_senator().is_some_and(|p| p.as_str() == actor)
                            })
                    })
                {
                    counts.approvals_decided += 1;
                }
            }
        }
        if counts != Counts::default() {
            tx.execute(
                "INSERT INTO stat_logs VALUES (?1,?2,?3,?4)",
                params![
                    committee,
                    i64::try_from(seq.0)?,
                    event.ts.timestamp(),
                    serde_json::to_vec(&counts)?
                ],
            )?;
        }
    }
    tx.execute(
        "INSERT OR REPLACE INTO stat_log_cursors VALUES (?1,?2,?3)",
        params![committee, i64::try_from(last.0)?, epoch],
    )?;
    tx.commit()?;
    Ok(last)
}
