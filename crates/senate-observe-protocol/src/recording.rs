//! Portable replay records. A recorder renumbers cursors before writing and
//! omits navigation and free text unless the chosen recording scope permits it.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Event, EventFrame, Snapshot, StatePatch};

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
    snapshot.senate.link = None;
    if let Some(person) = &mut snapshot.senate.person {
        person.link = None;
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
        Event::MessagePosted(message) if !lines => message.line = None,
        Event::MessageLinePublished(_) if !lines => return false,
        Event::StatePatch(patch) => {
            if let Some(replace) = &mut patch.replace {
                replace.navigation = None;
                if let Some(senate) = &mut replace.senate {
                    senate.link = None;
                    if let Some(person) = &mut senate.person {
                        person.link = None;
                    }
                }
            }
            if let Some(upsert) = &mut patch.upsert {
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
        _ => {}
    }
    true
}

fn line_capability(capability: &str) -> bool {
    matches!(capability, "lines_auto" | "published_lines")
}
