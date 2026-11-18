//! Durable notice IDs and progress. A server retry must use the event IDs
//! assigned before its first append, even across a daemon restart.

use rusqlite::{OptionalExtension, params};
use senate_core::EventId;

use crate::host_approval::HostApprovalState;

use super::{Store, StoreError, parse_host_id};

impl Store {
    pub(crate) fn hosted_approval_states(
        &self,
    ) -> Result<Vec<(EventId, HostApprovalState)>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT host_id, state FROM hosted_approvals")?;
        let mut rows = stmt.query([])?;
        let mut states = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let raw: String = row.get(1)?;
            states.push((parse_host_id(&id)?, from_json(&raw)?));
        }
        Ok(states)
    }

    pub(crate) fn edit_hosted_approvals(
        &self,
        id: &EventId,
        edit: impl FnOnce(&mut HostApprovalState) -> bool,
    ) -> Result<bool, StoreError> {
        let id = id.to_string();
        let conn = self.conn();
        let raw: Option<String> = conn
            .query_row(
                "SELECT state FROM hosted_approvals WHERE host_id = ?1",
                [&id],
                |row| row.get(0),
            )
            .optional()?;
        let mut state = raw
            .as_deref()
            .map(from_json)
            .transpose()?
            .unwrap_or_default();
        if !edit(&mut state) {
            return Ok(false);
        }
        if state.is_empty() {
            conn.execute("DELETE FROM hosted_approvals WHERE host_id = ?1", [&id])?;
        } else {
            let raw = serde_json::to_string(&state).map_err(json_error)?;
            conn.execute(
                "INSERT INTO hosted_approvals (host_id, state) VALUES (?1, ?2)
                 ON CONFLICT(host_id) DO UPDATE SET state = excluded.state",
                params![id, raw],
            )?;
        }
        Ok(true)
    }
}

fn from_json(raw: &str) -> Result<HostApprovalState, StoreError> {
    serde_json::from_str(raw).map_err(json_error)
}

fn json_error(error: serde_json::Error) -> StoreError {
    StoreError::Corrupt(format!("invalid hosted approval state: {error}"))
}
