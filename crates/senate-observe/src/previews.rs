//! The only free-text projection of a message. Eligibility is checked by
//! the server before it calls this policy.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use senate_core::{CommitteeId, EventId};
use serde::{Deserialize, Serialize};

use crate::grants::{GrantError, lock};

const MAX_PUBLISHED: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinePolicy {
    Auto,
    Published,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preview {
    pub committee: CommitteeId,
    pub event_id: EventId,
    pub line: String,
}

pub(crate) struct Previews {
    path: PathBuf,
    entries: Mutex<BTreeMap<EventId, Preview>>,
}

impl Previews {
    pub(crate) fn open(directory: &Path) -> Result<Self, GrantError> {
        let path = directory.join("observe-previews.json");
        let stored: Vec<Preview> = match File::open(&path) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        if stored.len() > MAX_PUBLISHED {
            return Err(GrantError::Invalid("too many published previews"));
        }
        Ok(Self {
            path,
            entries: Mutex::new(
                stored
                    .into_iter()
                    .map(|preview| (preview.event_id.clone(), preview))
                    .collect(),
            ),
        })
    }

    pub(crate) fn line(&self, policy: LinePolicy, event: &senate_core::Event) -> Option<String> {
        match policy {
            LinePolicy::Auto => first_line(&event.body),
            LinePolicy::Published => lock(&self.entries)
                .get(&event.id)
                .filter(|preview| preview.committee == event.committee)
                .map(|preview| preview.line.clone()),
        }
    }

    pub(crate) fn publish(&self, preview: Preview) -> Result<bool, GrantError> {
        let mut entries = lock(&self.entries);
        if entries
            .get(&preview.event_id)
            .is_some_and(|entry| entry.line == preview.line && entry.committee == preview.committee)
        {
            return Ok(false);
        }
        if entries.len() >= MAX_PUBLISHED && !entries.contains_key(&preview.event_id) {
            return Err(GrantError::Invalid("published preview limit reached"));
        }
        self.save(
            entries
                .values()
                .filter(|old| old.event_id != preview.event_id)
                .chain([&preview]),
        )?;
        entries.insert(preview.event_id.clone(), preview);
        Ok(true)
    }

    pub(crate) fn withdraw(
        &self,
        committee: &CommitteeId,
        event_id: &EventId,
    ) -> Result<bool, GrantError> {
        let mut entries = lock(&self.entries);
        if entries
            .get(event_id)
            .is_none_or(|preview| &preview.committee != committee)
        {
            return Ok(false);
        }
        self.save(entries.values().filter(|old| &old.event_id != event_id))?;
        entries.remove(event_id);
        Ok(true)
    }

    fn save<'a>(&self, values: impl Iterator<Item = &'a Preview>) -> Result<(), GrantError> {
        let directory = self
            .path
            .parent()
            .ok_or(GrantError::Invalid("missing preview directory"))?;
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temp, &values.collect::<Vec<_>>())?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

pub fn first_line(body: &str) -> Option<String> {
    let line = body.lines().next()?;
    let clean: String = line.chars().filter(|character| {
        !character.is_control() && !matches!(character,
            '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        )
    }).collect();
    let preview: String = clean.trim().chars().take(80).collect();
    (!preview.is_empty()).then_some(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use senate_core::{Address, Kind, NewEvent, Seq};

    #[test]
    fn preview_is_literal_first_line_bounded_and_stripped() {
        let long = format!("  \u{202e}{}\nsecond line", "🛠".repeat(100));
        let result = first_line(&long).unwrap_or_default();
        assert_eq!(result.chars().count(), 80);
        assert_eq!(result.len(), 320);
        assert!(!result.contains('\u{202e}'));
        assert_eq!(first_line("\nprivate second line"), None);
    }

    #[test]
    fn reviewed_previews_are_per_event_and_withdrawable() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let store = Previews::open(dir.path())?;
        let event = NewEvent::new(
            "committee-one".parse()?,
            Kind::Note,
            Address::Speaker,
            "preview\nprivate",
        )
        .into_event(Seq(1), chrono::Utc::now());
        assert_eq!(
            store.line(LinePolicy::Auto, &event).as_deref(),
            Some("preview")
        );
        assert_eq!(store.line(LinePolicy::Published, &event), None);
        let reviewed = Preview {
            committee: event.committee.clone(),
            event_id: event.id.clone(),
            line: "preview".into(),
        };
        assert!(store.publish(reviewed)?);
        assert_eq!(
            store.line(LinePolicy::Published, &event).as_deref(),
            Some("preview")
        );
        assert!(store.withdraw(&event.committee, &event.id)?);
        assert_eq!(store.line(LinePolicy::Published, &event), None);
        assert_eq!(
            store.line(LinePolicy::Auto, &event).as_deref(),
            Some("preview")
        );
        Ok(())
    }
}
