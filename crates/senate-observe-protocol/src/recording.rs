//! Portable replay records. A recorder renumbers cursors before writing and
//! omits navigation and free text unless the chosen recording scope permits it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Address, CardSubject, Event, EventFrame, Senate, Snapshot, StatePatch, Thread};

/// Maximum NDJSON row size including its terminating newline.
pub const MAX_ROW_BYTES: usize = crate::MAX_STATE_BYTES + 1024;

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct Header {
    pub format: Format,
    pub version: crate::V1,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub enum Format {
    #[serde(rename = "senate-observe-recording")]
    SenateObserveRecording,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
pub struct Row {
    pub offset_ms: u64,
    #[serde(flatten)]
    pub row: Content,
}

#[derive(Debug, Clone, PartialEq, JsonSchema, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Content {
    Snapshot(Box<Snapshot>),
    Frame(Box<EventFrame>),
    Discontinuity,
    End,
}

pub fn apply_frame(snapshot: &mut Snapshot, frame: &EventFrame) {
    snapshot.cursor = frame.cursor.clone();
    if let Event::StatePatch(patch) = &frame.event {
        snapshot.as_of = frame.at;
        apply_patch(snapshot, patch);
    }
}

fn apply_patch(snapshot: &mut Snapshot, patch: &StatePatch) {
    if let Some(replace) = &patch.replace {
        if let Some(senate) = &replace.senate {
            snapshot.senate = senate.clone();
        }
        if let Some(windows) = &replace.windows {
            snapshot.windows = windows.clone();
        }
        if let Some(navigation) = &replace.navigation {
            snapshot.navigation = Some(navigation.clone());
        }
    }
    macro_rules! collection {
        ($field:ident, $id:expr, $remove_id:expr) => {{
            let upsert = patch
                .upsert
                .as_ref()
                .map_or(&[][..], |rows| rows.$field.as_slice());
            let remove = patch
                .remove
                .as_ref()
                .map_or(&[][..], |rows| rows.$field.as_slice());
            if !upsert.is_empty() || !remove.is_empty() {
                let identify = $id;
                let remove_id = $remove_id;
                let mut rows: BTreeMap<_, _> = snapshot
                    .$field
                    .drain(..)
                    .map(|row| (identify(&row), row))
                    .collect();
                for row in remove {
                    rows.remove(&remove_id(row));
                }
                for row in upsert {
                    rows.insert(identify(row), row.clone());
                }
                snapshot.$field = rows.into_values().collect();
            }
        }};
    }
    let id = |row: &crate::IdKey| row.id.clone();
    collection!(links, |row: &crate::SenateLink| row.id.clone(), id);
    collection!(committees, |row: &crate::Committee| row.id.clone(), id);
    collection!(projects, |row: &crate::Project| row.id.clone(), id);
    collection!(approvals, |row: &crate::Approval| row.id.clone(), id);
    let seat = |row: &crate::SeatKey| (row.committee_id.clone(), row.id.clone());
    collection!(
        senators,
        |row: &crate::Senator| (row.committee_id.clone(), row.id.clone()),
        seat
    );
    collection!(
        retired,
        |row: &crate::RetiredSenator| (row.committee_id.clone(), row.id.clone()),
        seat
    );
    let thread = |row: &crate::ThreadKey| (row.committee_id.clone(), row.root_seq);
    collection!(
        threads,
        |row: &crate::Thread| (row.committee_id.clone(), row.root_seq),
        thread
    );
    collection!(
        desk_items,
        |row: &crate::DeskItem| (row.committee_id.clone(), row.root_seq),
        thread
    );
}

pub fn sanitize_snapshot(snapshot: &mut Snapshot, lines: bool) {
    snapshot
        .capabilities
        .retain(|item| item != "app_links" && (lines || !line_capability(item)));
    snapshot.navigation = None;
    sanitize_senate(&mut snapshot.senate);
    for chair in snapshot
        .committees
        .iter_mut()
        .filter_map(|row| row.chair_id.as_mut())
    {
        strip_grant_hint(chair);
    }
    for id in snapshot
        .senators
        .iter_mut()
        .map(|row| &mut row.id)
        .chain(snapshot.retired.iter_mut().map(|row| &mut row.id))
        .chain(snapshot.approvals.iter_mut().map(|row| &mut row.senator_id))
    {
        strip_grant_hint(id);
    }
    for thread in &mut snapshot.threads {
        sanitize_thread(thread);
    }
    macro_rules! strip {
        ($($field:ident),+) => {$(
            for row in &mut snapshot.$field { row.link = None; }
        )+ };
    }
    strip!(
        links, committees, projects, senators, retired, threads, desk_items, approvals
    );
}

/// Returns false for a frame that must be dropped from the record.
pub fn sanitize_frame(frame: &mut EventFrame, lines: bool) -> bool {
    frame.link = None;
    match &mut frame.event {
        Event::MessagePosted(message) => {
            if !lines {
                message.line = None;
            }
            sanitize_address(&mut message.from);
            for address in &mut message.to {
                sanitize_address(address);
            }
        }
        Event::MessageLinePublished(_) if !lines => return false,
        Event::MessageRouted(route) => {
            if let Some(target) = &mut route.target {
                sanitize_address(target);
            }
        }
        Event::StatePatch(patch) => sanitize_patch(patch),
        Event::SenatorStateChanged(row) => strip_grant_hint(&mut row.senator_id),
        Event::SenatorJoined(row)
        | Event::SenatorLeft(row)
        | Event::SenatorStarted(row)
        | Event::SenatorStopped(row)
        | Event::SenatorPaused(row)
        | Event::SenatorResumed(row)
        | Event::ViewerAttached(row)
        | Event::ViewerDetached(row) => strip_grant_hint(&mut row.senator_id),
        Event::CommitteeChairChanged(row) => {
            for id in [&mut row.previous_chair_id, &mut row.chair_id]
                .into_iter()
                .flatten()
            {
                strip_grant_hint(id);
            }
        }
        Event::ApprovalAsked(row) => strip_grant_hint(&mut row.senator_id),
        Event::ToolStarted(row) | Event::ToolEnded(row) => strip_grant_hint(&mut row.senator_id),
        Event::CardChanged(row) => match &mut row.subject {
            CardSubject::Senator { senator_id, .. } => strip_grant_hint(senator_id),
            CardSubject::Person { person_id } => strip_grant_hint(person_id),
            _ => {}
        },
        Event::MessageLinePublished(_)
        | Event::ThreadStateChanged(_)
        | Event::CommitteeCreated(_)
        | Event::CommitteeArchived(_)
        | Event::ApprovalDecided(_)
        | Event::LinkChanged(_)
        | Event::ProjectReleaseSeen(_) => {}
    }
    true
}

fn sanitize_patch(patch: &mut StatePatch) {
    if let Some(replace) = &mut patch.replace {
        replace.navigation = None;
        if let Some(senate) = &mut replace.senate {
            sanitize_senate(senate);
        }
    }
    if let Some(remove) = &mut patch.remove {
        for row in remove.senators.iter_mut().chain(&mut remove.retired) {
            strip_grant_hint(&mut row.id);
        }
    }
    if let Some(upsert) = &mut patch.upsert {
        for chair in upsert
            .committees
            .iter_mut()
            .filter_map(|row| row.chair_id.as_mut())
        {
            strip_grant_hint(chair);
        }
        for id in upsert
            .senators
            .iter_mut()
            .map(|row| &mut row.id)
            .chain(upsert.retired.iter_mut().map(|row| &mut row.id))
            .chain(upsert.approvals.iter_mut().map(|row| &mut row.senator_id))
        {
            strip_grant_hint(id);
        }
        for thread in &mut upsert.threads {
            sanitize_thread(thread);
        }
        macro_rules! strip {
            ($($field:ident),+) => {$(
                for row in &mut upsert.$field { row.link = None; }
            )+ };
        }
        strip!(
            links, committees, projects, senators, retired, threads, desk_items, approvals
        );
    }
}

fn sanitize_senate(senate: &mut Senate) {
    senate.link = None;
    for id in senate.desk_owner.iter_mut().chain(&mut senate.desk_workers) {
        strip_grant_hint(id);
    }
    if let Some(person) = &mut senate.person {
        person.link = None;
        strip_grant_hint(&mut person.id);
    }
}

fn sanitize_thread(thread: &mut Thread) {
    sanitize_address(&mut thread.opener);
    for address in &mut thread.addressees {
        sanitize_address(address);
    }
}

fn sanitize_address(address: &mut Address) {
    if let Address::Senator { id } = address {
        strip_grant_hint(id);
    }
}

fn strip_grant_hint(id: &mut String) {
    let Some((hint, digest)) = id
        .strip_prefix("obs-")
        .and_then(|value| value.split_once('-'))
    else {
        return;
    };
    if hint.len() == 12
        && digest.len() == 32
        && hint
            .bytes()
            .chain(digest.bytes())
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        // The digest already commits to both the grant and the native ID.
        *id = format!("recorded:{digest}");
    }
}

fn line_capability(capability: &str) -> bool {
    matches!(capability, "lines_auto" | "published_lines")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cursor, Removals, SeatKey, V1};

    #[test]
    fn recorded_ids_drop_grant_hints_and_still_match_removals()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut state: Snapshot =
            serde_json::from_str(include_str!("../../../docs/observe/v1/fixtures/state.json"))?;
        let id = "obs-0123456789ab-0123456789abcdef0123456789abcdef".to_owned();
        state.senators[0].id = id.clone();
        state.committees[0].chair_id = Some(id.clone());
        state.threads[0].opener = Address::Senator { id: id.clone() };
        let mut frame = EventFrame {
            schema_version: V1,
            cursor: "AAAAAAAAAAAAAAAAAAAAAA".parse::<Cursor>()?,
            at: state.as_of,
            committee_id: None,
            link: None,
            event: Event::StatePatch(Box::new(StatePatch {
                remove: Some(Removals {
                    senators: vec![SeatKey {
                        id,
                        committee_id: state.senators[0].committee_id.clone(),
                    }],
                    ..Removals::default()
                }),
                ..StatePatch::default()
            })),
        };
        let count = state.senators.len();
        sanitize_snapshot(&mut state, false);
        assert!(sanitize_frame(&mut frame, false));
        for data in [
            serde_json::to_string(&state)?,
            serde_json::to_string(&frame)?,
        ] {
            assert!(!data.contains("0123456789ab-"));
            assert!(data.contains("recorded:0123456789abcdef0123456789abcdef"));
        }
        apply_frame(&mut state, &frame);
        assert_eq!(state.senators.len(), count - 1);
        Ok(())
    }
}
