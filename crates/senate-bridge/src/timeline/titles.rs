use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{Event, agy, claude, codex, identity, open_file};
use crate::ipc::TimelineKind;
use crate::session::Runtime;

#[derive(Default)]
pub(crate) struct TitleCache {
    entries: HashMap<PathBuf, TitleEntry>,
    agy: HashMap<PathBuf, agy::Title>,
}

pub(crate) struct SidebarTranscript {
    pub modified: Option<DateTime<Utc>>,
    pub title: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Stamp {
    identity: (u64, u64),
    len: u64,
    modified: Option<SystemTime>,
}

struct TitleEntry {
    runtime: Runtime,
    stamp: Option<Stamp>,
    codex_context: codex::Context,
    offset: u64,
    pending_line: Vec<u8>,
    title: Option<String>,
    #[cfg(test)]
    bytes_scanned: u64,
}

impl TitleCache {
    pub(crate) fn refresh(
        &mut self,
        sessions: Vec<(String, Runtime, PathBuf)>,
    ) -> HashMap<String, SidebarTranscript> {
        let visible: HashSet<_> = sessions.iter().map(|(_, _, path)| path.as_path()).collect();
        self.entries
            .retain(|path, _| visible.contains(path.as_path()));
        self.agy.retain(|path, _| visible.contains(path.as_path()));
        sessions
            .into_iter()
            .filter_map(|(id, runtime, path)| {
                self.metadata(&path, runtime).map(|metadata| (id, metadata))
            })
            .collect()
    }

    fn metadata(&mut self, path: &Path, runtime: Runtime) -> Option<SidebarTranscript> {
        if runtime == Runtime::Agy {
            let metadata = self.agy.entry(path.to_owned()).or_default().read(path);
            if metadata.is_none() {
                self.agy.remove(path);
            }
            return metadata;
        }
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            self.entries.remove(path);
            return None;
        };
        if !metadata.is_file() {
            self.entries.remove(path);
            return None;
        }
        let stamp = Stamp {
            identity: identity(&metadata),
            len: metadata.len(),
            modified: metadata.modified().ok(),
        };
        let entry = self
            .entries
            .entry(path.to_owned())
            .or_insert_with(|| TitleEntry::new(runtime));
        // A changed inode or a non-append modification invalidates even a
        // previously found title. For appends the first prompt is immutable.
        if entry.runtime != runtime
            || entry.stamp.is_some_and(|old| {
                old.identity != stamp.identity
                    || stamp.len < old.len
                    || (stamp.len == old.len && stamp.modified != old.modified)
            })
        {
            *entry = TitleEntry::new(runtime);
        }
        if entry.stamp != Some(stamp)
            && (entry.title.is_some() || entry.continue_search(path, stamp).is_ok())
        {
            entry.stamp = Some(stamp);
        }
        Some(SidebarTranscript {
            modified: stamp.modified.map(DateTime::<Utc>::from),
            title: entry.title.clone(),
        })
    }
}

impl TitleEntry {
    fn new(runtime: Runtime) -> Self {
        Self {
            runtime,
            stamp: None,
            codex_context: codex::Context::default(),
            offset: 0,
            pending_line: Vec::new(),
            title: None,
            #[cfg(test)]
            bytes_scanned: 0,
        }
    }

    fn continue_search(&mut self, path: &Path, stamp: Stamp) -> io::Result<()> {
        if self.offset == stamp.len {
            return Ok(());
        }
        let mut file = open_file(path)?;
        if identity(&file.metadata()?) != stamp.identity {
            return Err(io::Error::other("transcript replaced during title lookup"));
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut reader = BufReader::new(file).take(stamp.len - self.offset);
        loop {
            let pending = self.pending_line.len();
            let read = match reader.read_until(b'\n', &mut self.pending_line) {
                Ok(read) => read,
                Err(err) => {
                    self.pending_line.truncate(pending);
                    return Err(err);
                }
            };
            self.offset += read as u64;
            #[cfg(test)]
            {
                self.bytes_scanned += read as u64;
            }
            if read == 0 || self.pending_line.last() != Some(&b'\n') {
                return Ok(());
            }
            let record = serde_json::from_slice::<Value>(&self.pending_line);
            // Keep only an incomplete line between calls. Complete records
            // can reuse a small buffer while this scan continues.
            self.pending_line.clear();
            self.pending_line.shrink_to(64 * 1024);
            if let Ok(record) = record
                && let Some(title) = self.title_from(&record)
            {
                self.title = Some(title);
                self.pending_line = Vec::new();
                return Ok(());
            }
        }
    }

    fn title_from(&mut self, record: &Value) -> Option<String> {
        match self.runtime {
            Runtime::Claude => claude::user_events(record, false),
            Runtime::Codex
                if record["type"] == "session_meta"
                    || (record["type"] == "response_item"
                        && record["payload"]["role"] == "user") =>
            {
                codex::events(record, &mut self.codex_context)
            }
            _ => Vec::new(),
        }
        .into_iter()
        .find_map(|event| match event {
            Event::Item(TimelineKind::UserMessage { text }) => text
                .lines()
                .next()
                .map(|first| first.chars().take(60).collect()),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::io::Write;

    use super::*;

    #[test]
    fn cached_title_search_reads_only_appends_and_drops_removed_sessions() {
        let mut cache = TitleCache::default();
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let record = r#"{"type":"user","isMeta":true,"message":{"content":"injected"}}"#;
        writeln!(file, "{record}").unwrap();
        let path = file.path().to_owned();
        let session = || vec![("test".to_string(), Runtime::Claude, path.clone())];

        assert!(cache.refresh(session())["test"].title.is_none());
        let first = cache.entries[&path].bytes_scanned;
        assert_eq!(first, (record.len() + 1) as u64);
        assert!(cache.refresh(session())["test"].title.is_none());
        assert_eq!(cache.entries[&path].bytes_scanned, first);

        let prompt = r#"{"type":"user","message":{"content":"First prompt\ncontinued"}}"#;
        let split = prompt.len() / 2;
        write!(file, "{}", &prompt[..split]).unwrap();
        assert!(cache.refresh(session())["test"].title.is_none());
        assert_eq!(cache.entries[&path].offset, first + split as u64);
        assert_eq!(cache.entries[&path].bytes_scanned, first + split as u64);
        assert!(cache.refresh(session())["test"].title.is_none());
        assert_eq!(cache.entries[&path].bytes_scanned, first + split as u64);
        writeln!(file, "{}", &prompt[split..]).unwrap();
        assert_eq!(
            cache.refresh(session())["test"].title.as_deref(),
            Some("First prompt")
        );
        assert_eq!(
            cache.entries[&path].offset,
            first + (prompt.len() + 1) as u64
        );
        assert_eq!(
            cache.entries[&path].bytes_scanned,
            first + (prompt.len() + 1) as u64
        );
        let titled = cache.entries[&path].bytes_scanned;

        writeln!(
            file,
            r#"{{"type":"user","message":{{"content":"Second prompt"}}}}"#
        )
        .unwrap();
        assert_eq!(
            cache.refresh(session())["test"].title.as_deref(),
            Some("First prompt")
        );
        assert_eq!(cache.entries[&path].bytes_scanned, titled);

        let mut replacement = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
        writeln!(
            replacement,
            r#"{{"type":"user","message":{{"content":"New conversation"}}}}"#
        )
        .unwrap();
        replacement.persist(&path).unwrap();
        assert_eq!(
            cache.refresh(session())["test"].title.as_deref(),
            Some("New conversation")
        );

        assert!(cache.refresh(Vec::new()).is_empty());
        assert!(cache.entries.is_empty());
    }
}
