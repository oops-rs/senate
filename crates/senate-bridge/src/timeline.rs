//! Session transcripts belong to the person on this machine. Timeline items
//! and titles go only over the local bridge.sock to the person's app. The
//! bridge never sends transcript contents to a Senate server or committee log.
//!
//! Working state is based on observed hooks and liveness. A non-hosted
//! Codex process without a known PID can exit without reporting Stop or
//! SessionEnd. The daemon cannot reliably clear its working state then.

mod claude;
mod codex;
mod parse;
mod titles;

pub(crate) use titles::{SidebarTranscript, TitleCache};

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, BufRead, BufReader};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::ipc::{
    MAX_LINE, TimelineDirection, TimelineItem, TimelineKind, TimelineToolDetail, TimelineToolStatus,
};
use crate::session::{Runtime, Session};

const FRAME_BUDGET: usize = MAX_LINE - 8192;
const RETAINED_RECORD_CAPACITY: usize = 1024 * 1024;
const MAX_ITEMS: usize = 300;

pub(crate) fn path(session: &Session) -> Option<PathBuf> {
    if !matches!(session.runtime, Runtime::Claude | Runtime::Codex) {
        return None;
    }
    let path = PathBuf::from(session.transcript_path.as_ref()?);
    (path.is_absolute() && path.extension().is_some_and(|ext| ext == "jsonl")).then_some(path)
}

fn open_file(path: &Path) -> io::Result<File> {
    // An agent supplies this path in its hook. Never wait on a FIFO or read
    // from a device presented as a transcript.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a transcript file",
        ));
    }
    Ok(file)
}

pub(crate) struct Transcript {
    file: BufReader<File>,
    path: PathBuf,
    runtime: Runtime,
    codex_context: codex::Context,
    identity: (u64, u64),
    offset: u64,
    anchor: Vec<u8>,
    record: Vec<u8>,
    reduced: Reducer,
    // A physical line can yield several items. Keep its unconsumed changes
    // when a watch batch hits the wire limit.
    changes: VecDeque<Change>,
}

fn identity(meta: &Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

impl Transcript {
    pub(crate) fn open(path: PathBuf, runtime: Runtime) -> io::Result<Self> {
        let file = open_file(&path)?;
        Ok(Self {
            identity: identity(&file.metadata()?),
            file: BufReader::new(file),
            path,
            runtime,
            codex_context: codex::Context::default(),
            offset: 0,
            anchor: Vec::new(),
            record: Vec::new(),
            reduced: Reducer::default(),
            changes: VecDeque::new(),
        })
    }

    pub(crate) fn snapshot_len(&self) -> io::Result<u64> {
        Ok(self.file.get_ref().metadata()?.len())
    }

    /// An append preserves both inode identity and the bytes immediately
    /// preceding our offset. Checking both catches replacements and a
    /// truncate followed by a write past the old offset between polls.
    pub(crate) fn refreshed_len(&self, path: &Path, runtime: Runtime) -> io::Result<Option<u64>> {
        if path != self.path || runtime != self.runtime {
            return Ok(None);
        }
        let meta = std::fs::symlink_metadata(path)?;
        if !meta.is_file() || self.identity != identity(&meta) || meta.len() < self.offset {
            return Ok(None);
        }
        let mut anchor = [0; 64];
        let bytes = &mut anchor[..self.anchor.len()];
        if !bytes.is_empty() {
            self.file
                .get_ref()
                .read_exact_at(bytes, self.offset - bytes.len() as u64)?;
        }
        Ok((bytes == self.anchor).then_some(meta.len()))
    }

    pub(crate) fn remember_offset(&mut self) -> io::Result<()> {
        let len = self.offset.min(64) as usize;
        self.anchor.resize(len, 0);
        self.file
            .get_ref()
            .read_exact_at(&mut self.anchor, self.offset - len as u64)
    }

    fn read_record(&mut self, until: u64) -> io::Result<bool> {
        while self.offset < until {
            let buffered = self.file.fill_buf()?;
            let remaining = usize::try_from(until - self.offset).unwrap_or(usize::MAX);
            let buffered = &buffered[..buffered.len().min(remaining)];
            if buffered.is_empty() {
                break;
            }
            let newline = buffered.iter().position(|byte| *byte == b'\n');
            let consumed = newline.map_or(buffered.len(), |at| at + 1);
            self.record.extend_from_slice(&buffered[..consumed]);
            self.file.consume(consumed);
            self.offset += consumed as u64;
            if newline.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn next(&mut self, until: u64) -> io::Result<Option<Change>> {
        loop {
            if let Some(change) = self.changes.pop_front() {
                return Ok(Some(change));
            }
            if !self.read_record(until)? {
                return Ok(None);
            }
            if let Ok(record) = serde_json::from_slice::<Value>(&self.record) {
                let ts = record["timestamp"]
                    .as_str()
                    .and_then(|value| value.parse().ok());
                let events = match self.runtime {
                    Runtime::Claude => claude::events(&record),
                    Runtime::Codex => codex::events(&record, &mut self.codex_context),
                    _ => Vec::new(),
                };
                for event in events {
                    if let Some(change) = self.reduced.apply(event, ts) {
                        self.changes.push_back(change);
                    }
                }
            }
            self.record.clear();
            // A large valid result still completes its original call. Once
            // parsed, keep at most a modest buffer for the next record.
            self.record.shrink_to(RETAINED_RECORD_CAPACITY);
        }
    }

    pub(crate) fn page(
        &mut self,
        direction: TimelineDirection,
        cursor: Option<u64>,
        limit: usize,
    ) -> io::Result<Page> {
        let mut selected: VecDeque<TimelineItem> = VecDeque::new();
        let end = self.snapshot_len()?;
        let keep_latest = direction != TimelineDirection::After;
        let mut oldest = None;
        let mut newest = None;
        while let Some(Change {
            item, replacement, ..
        }) = self.next(end)?
        {
            let seq = item.seq;
            if replacement {
                if let Ok(index) = selected.binary_search_by_key(&seq, |item| item.seq) {
                    selected[index] = item;
                }
                continue;
            }
            oldest.get_or_insert(seq);
            newest = Some(seq);
            let eligible = match direction {
                TimelineDirection::Tail => true,
                TimelineDirection::Before => cursor.is_none_or(|cursor| seq < cursor),
                TimelineDirection::After => cursor.is_none_or(|cursor| seq > cursor),
            };
            if eligible && (selected.len() < limit || keep_latest) {
                if selected.len() == limit {
                    selected.pop_front();
                }
                selected.push_back(item);
            }
        }
        // Preserve contiguity when the socket's frame limit is reached.
        let mut bytes = 0;
        let with_size = |item: &TimelineItem| {
            bytes += wire_size(item);
            bytes <= FRAME_BUDGET
        };
        let items: Vec<_> = if keep_latest {
            let mut items: Vec<_> = selected.into_iter().rev().take_while(with_size).collect();
            items.reverse();
            items
        } else {
            selected.into_iter().take_while(with_size).collect()
        };
        Ok(Page {
            has_older: oldest
                .zip(items.first().map(|item| item.seq).or(cursor))
                .is_some_and(|(oldest, first)| oldest < first),
            has_newer: newest
                .zip(items.last().map(|item| item.seq).or(cursor))
                .is_some_and(|(newest, last)| newest > last),
            items,
        })
    }

    pub(crate) fn updates(&mut self, until: u64, after: u64) -> io::Result<Vec<TimelineItem>> {
        let mut batch = BTreeMap::new();
        let mut bytes = 0;
        while let Some(change) = self.next(until)? {
            // A result following the cursor may have arrived between the
            // page and watch requests, including for a much older call.
            if change.item.seq <= after && !(change.replacement && change.position >= after) {
                continue;
            }
            let size = wire_size(&change.item);
            if bytes + size > FRAME_BUDGET || batch.len() >= MAX_ITEMS {
                self.changes.push_front(change);
                break;
            }
            bytes += size;
            batch.insert(change.item.seq, change.item);
        }
        self.remember_offset()?;
        Ok(batch.into_values().collect())
    }
}

fn wire_size(item: &TimelineItem) -> usize {
    serde_json::to_vec(item).map_or(FRAME_BUDGET, |bytes| bytes.len() + 1)
}

#[derive(Default)]
pub(crate) struct Page {
    pub items: Vec<TimelineItem>,
    pub has_older: bool,
    pub has_newer: bool,
}

struct Change {
    item: TimelineItem,
    replacement: bool,
    // Ordinal of the latest new item as of this change's physical record.
    position: u64,
}

enum Event {
    Item(TimelineKind),
    Call {
        id: String,
        kind: TimelineKind,
        result: Option<ToolResult>,
    },
    Result {
        id: String,
        result: ToolResult,
    },
}

struct ToolResult {
    failed: bool,
    output: Option<String>,
    exit_code: Option<i64>,
}

impl ToolResult {
    fn fold_into(self, item: &mut TimelineItem) {
        if let TimelineKind::ToolCall { status, detail, .. } = &mut item.kind {
            *status = if self.failed || self.exit_code.is_some_and(|code| code != 0) {
                TimelineToolStatus::Failed
            } else {
                TimelineToolStatus::Completed
            };
            if let TimelineToolDetail::Shell {
                output, exit_code, ..
            } = detail
            {
                *output = self.output;
                *exit_code = self.exit_code;
            }
        }
        parse::cap(item);
    }
}

#[derive(Default)]
struct Reducer {
    seq: u64,
    pending: HashMap<String, TimelineItem>,
}

impl Reducer {
    fn apply(&mut self, event: Event, ts: Option<DateTime<Utc>>) -> Option<Change> {
        let mut replacement = false;
        let item = match event {
            Event::Item(kind) => self.new_item(kind, ts),
            Event::Call {
                id,
                kind,
                result: None,
            } => {
                let item = self.new_item(kind, ts);
                self.pending.insert(id, item.clone());
                item
            }
            Event::Call {
                id,
                kind,
                result: Some(result),
            } => {
                let previous = self.pending.remove(&id);
                replacement = previous.is_some();
                let mut item = previous.unwrap_or_else(|| self.new_item(kind, ts));
                result.fold_into(&mut item);
                item
            }
            Event::Result { id, result } => {
                replacement = true;
                let mut item = self.pending.remove(&id)?;
                result.fold_into(&mut item);
                item
            }
        };
        Some(Change {
            item,
            replacement,
            position: self.seq,
        })
    }

    fn new_item(&mut self, kind: TimelineKind, ts: Option<DateTime<Utc>>) -> TimelineItem {
        self.seq += 1;
        let mut item = TimelineItem {
            seq: self.seq,
            ts,
            truncated: false,
            kind,
        };
        parse::cap(&mut item);
        item
    }
}

#[cfg(test)]
mod tests;
