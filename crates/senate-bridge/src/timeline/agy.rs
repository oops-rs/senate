//! Local read-only Antigravity CLI 1.5.15 conversation reader. Only committed
//! steps are visible, at the cadence Antigravity persists them. The root
//! database is the only source. SQLite may coordinate through WAL sidecars.
//! Reads use a short busy timeout, release their snapshots before waiting,
//! and disable checkpoints on close. No transcript leaves the bridge socket.

mod proto;

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use rusqlite::{
    Connection, ErrorCode, OpenFlags, OptionalExtension, Row, config::DbConfig, params,
};

use super::{FRAME_BUDGET, MAX_ITEMS, Page, identity, parse, wire_size};
use crate::ipc::{TimelineDirection as Direction, TimelineItem, TimelineKind, TimelineUnavailable};
use crate::session::{Runtime, Session, agy_session_key};

// A step can be split into reasoning/reply or multiple Senate messages.
// Reserve stable positions without renumbering a record when its text changes.
const SLOTS: u64 = 65_536;
const ROWS_PER_POLL: i64 = 64;
const MAX_STEP_BYTES: i64 = 8 * 1024 * 1024;
const BUSY_TIMEOUT: Duration = Duration::from_millis(25);

// Fingerprint of the version-1 table and column layout observed on 1.5.15.
const SCHEMA: &str = "battle_mode_infos/idx:INTEGER:1,data:BLOB:0\n\
executor_metadata/idx:INTEGER:1,data:BLOB:0\n\
gen_metadata/idx:INTEGER:1,data:BLOB:0,size:INTEGER:0\n\
parent_references/idx:INTEGER:1,data:BLOB:0\n\
steps/idx:INTEGER:1,step_type:INTEGER:0,status:INTEGER:0,has_subtrajectory:NUMERIC:0,metadata:BLOB:0,error_details:BLOB:0,permissions:BLOB:0,task_details:BLOB:0,render_info:BLOB:0,step_payload:BLOB:0,step_format:INTEGER:0\n\
trajectory_meta/trajectory_id:TEXT:1,cascade_id:TEXT:0,trajectory_type:INTEGER:0,source:INTEGER:0\n\
trajectory_metadata_blob/id:TEXT:1,data:BLOB:0";

pub(super) fn path(session: &Session) -> Option<PathBuf> {
    let id = Runtime::Agy.native_id(&session.session_id)?;
    if agy_session_key(id).as_deref() != Some(session.session_id.as_str()) {
        return None;
    }
    let filename = format!("{id}.db");
    if let Some(hint) = session.transcript_path.as_deref().map(Path::new)
        && hint.is_absolute()
        && hint
            .file_name()
            .is_some_and(|name| name == filename.as_str())
    {
        return Some(hint.to_owned());
    }
    Some(
        dirs::home_dir()?
            .join(".gemini/antigravity-cli/conversations")
            .join(filename),
    )
}

#[derive(Debug)]
pub(crate) enum Error {
    Missing,
    Busy,
    Unsupported,
    Unreadable,
    Reset,
}

impl Error {
    pub(crate) fn reason(&self) -> Option<TimelineUnavailable> {
        match self {
            Self::Missing => None,
            Self::Unsupported => Some(TimelineUnavailable::UnsupportedSchema),
            _ => Some(TimelineUnavailable::TemporarilyUnreadable),
        }
    }

    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::Missing => "the Antigravity conversation database is missing",
            Self::Unsupported => "the Antigravity conversation format is unsupported",
            _ => "the Antigravity conversation database could not be read",
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        match error.sqlite_error_code() {
            Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => Self::Busy,
            _ => Self::Unreadable,
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    id: (u64, u64),
    modified: Option<SystemTime>,
    len: u64,
}

impl FileStamp {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::Missing
            } else {
                Error::Unreadable
            }
        })?;
        if !metadata.is_file() {
            return Err(Error::Unreadable);
        }
        Ok(Self {
            id: identity(&metadata),
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    main: FileStamp,
    wal: Option<FileStamp>,
}

impl Stamp {
    fn read(path: &Path) -> Result<Self> {
        let main = FileStamp::read(path)?;
        let wal = match FileStamp::read(&path.with_extension("db-wal")) {
            Ok(wal) => Some(wal),
            Err(Error::Missing) => None,
            Err(error) => return Err(error),
        };
        Ok(Self { main, wal })
    }

    fn modified(self) -> Option<DateTime<Utc>> {
        self.main
            .modified
            .max(
                self.wal
                    .filter(|wal| wal.len > 32)
                    .and_then(|wal| wal.modified),
            )
            .map(Into::into)
    }
}

fn open(path: &Path, stamp: Stamp) -> Result<Connection> {
    // SQLite's NOFOLLOW checks every path component. Resolve directory
    // aliases (including macOS /var) without following a database symlink.
    let parent = path.parent().ok_or(Error::Unreadable)?;
    let file = path.file_name().ok_or(Error::Unreadable)?;
    let canonical = fs::canonicalize(parent)
        .map_err(|_| Error::Unreadable)?
        .join(file);
    let db = Connection::open_with_flags(
        canonical,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(BUSY_TIMEOUT)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    db.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    if FileStamp::read(path)?.id != stamp.main.id {
        return Err(Error::Reset);
    }
    Ok(db)
}

/// Must be called in the same snapshot as the page or watch read.
fn verify(db: &Connection, path: &Path, all_formats: bool) -> Result<String> {
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != 1 {
        return Err(Error::Unsupported);
    }
    let layout: String = db.query_row(
        "SELECT coalesce(group_concat(layout, char(10)), '') FROM (
             SELECT s.name || '/' || (
                 SELECT group_concat(col, ',') FROM (
                     SELECT name || ':' || upper(type) || ':' || pk AS col
                     FROM pragma_table_xinfo(s.name) ORDER BY cid
                 )
             ) AS layout
             FROM sqlite_schema s
             WHERE s.type = 'table' AND substr(s.name, 1, 7) != 'sqlite_'
             ORDER BY s.name
         )",
        [],
        |row| row.get(0),
    )?;
    if layout != SCHEMA
        || db.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))? != "wal"
    {
        return Err(Error::Unsupported);
    }
    if all_formats
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM steps WHERE step_format IS NOT 0 OR idx < 0)",
            [],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Err(Error::Unsupported);
    }
    let root_id = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or(Error::Unsupported)?;
    let (trajectory, root, only, has_references): (String, String, bool, bool) = db
        .query_row(
            "SELECT trajectory_id, cascade_id, (SELECT count(*) = 1 FROM trajectory_meta),
                EXISTS(SELECT 1 FROM parent_references)
         FROM trajectory_meta",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or(Error::Unsupported)?;
    let meta: Option<Vec<u8>> = db
        .query_row(
            "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if !only
        || root != root_id
        || has_references
        || proto::has_parent(meta.as_deref().unwrap_or_default(), root_id)
            .map_err(|_| Error::Unsupported)?
    {
        return Err(Error::Unsupported);
    }
    Ok(trajectory)
}

const STEP_FIELDS: &str = "idx, step_type, status, step_format, length(metadata), length(step_payload), metadata, step_payload";

struct Step {
    idx: i64,
    kind: i64,
    status: i64,
    ts: Option<DateTime<Utc>>,
    items: Vec<TimelineItem>,
}

impl Step {
    fn read(row: &Row<'_>) -> Result<Self> {
        let idx: i64 = row.get(0)?;
        let kind = row.get(1)?;
        let status = row.get(2)?;
        let format: i64 = row.get(3)?;
        if format != 0 || idx < 0 {
            return Err(Error::Unsupported);
        }
        let base = (idx as u64 + 1)
            .checked_mul(SLOTS)
            .ok_or(Error::Unsupported)?;
        let size = |field| {
            row.get::<_, Option<i64>>(field)
                .map(|value| value.unwrap_or_default())
        };
        let (ts, decoded) = if size(4)?.saturating_add(size(5)?) > MAX_STEP_BYTES {
            (None, Err(proto::Unsupported))
        } else {
            let metadata = bytes(row, 6)?;
            let payload = bytes(row, 7)?;
            match proto::timestamp(metadata) {
                Ok(ts) => (ts, proto::decode(idx, kind, status, metadata, payload)),
                Err(error) => (None, Err(error)),
            }
        };
        let decoded = match decoded {
            Ok(items) if items.len() < SLOTS as usize => items,
            _ => vec![(
                0,
                proto::notice("warning", "Unsupported Antigravity record"),
            )],
        };
        let items = decoded
            .into_iter()
            .map(|(slot, kind)| {
                let mut item = TimelineItem {
                    seq: base + slot,
                    ts,
                    truncated: false,
                    kind,
                };
                parse::cap(&mut item);
                item
            })
            .collect();
        Ok(Self {
            idx,
            kind,
            status,
            ts,
            items,
        })
    }

    fn anchor(&self) -> Anchor {
        Anchor {
            idx: self.idx,
            kind: self.kind,
            ts: self.ts,
        }
    }
}

fn bytes<'a>(row: &'a Row<'_>, field: usize) -> Result<&'a [u8]> {
    use rusqlite::types::ValueRef;
    match row.get_ref(field)? {
        ValueRef::Blob(bytes) => Ok(bytes),
        ValueRef::Null => Ok(&[]),
        _ => Err(Error::Unsupported),
    }
}

fn index(seq: u64) -> i64 {
    (seq / SLOTS) as i64 - 1
}

pub(crate) fn page(
    path: &Path,
    direction: Direction,
    cursor: Option<u64>,
    limit: usize,
) -> Result<Page> {
    let stamp = Stamp::read(path)?;
    let db = open(path, stamp)?;
    let tx = db.unchecked_transaction()?;
    verify(&tx, path, true)?;

    let last_first = direction != Direction::After;
    let pivot = cursor.unwrap_or_default();
    let (lo, hi) = match direction {
        Direction::Tail => (0, i64::MAX),
        Direction::Before => (0, index(pivot)),
        Direction::After => (index(pivot).max(0), i64::MAX),
    };
    let keep = |item: &TimelineItem| match direction {
        Direction::Tail => true,
        Direction::Before => item.seq < pivot,
        Direction::After => item.seq > pivot,
    };
    let (mut items, more) = collect(&tx, lo, hi, last_first, limit, keep)?;
    if last_first {
        items.reverse();
    }
    let opposite = match direction {
        Direction::Tail => false,
        Direction::Before => {
            let last = items.last().map_or(pivot, |i| i.seq);
            !collect(&tx, index(last).max(0), i64::MAX, false, 1, |i| {
                i.seq > last
            })?
            .0
            .is_empty()
        }
        Direction::After => {
            let first = items.first().map_or(pivot, |i| i.seq);
            !collect(&tx, 0, index(first), true, 1, |i| i.seq < first)?
                .0
                .is_empty()
        }
    };
    Ok(Page {
        items,
        has_older: if last_first { more } else { opposite },
        has_newer: if last_first { opposite } else { more },
    })
}

fn collect(
    db: &Connection,
    lo: i64,
    hi: i64,
    descending: bool,
    limit: usize,
    keep: impl Fn(&TimelineItem) -> bool,
) -> Result<(Vec<TimelineItem>, bool)> {
    let order = if descending { "DESC" } else { "ASC" };
    let mut query = db.prepare(&format!(
        "SELECT {STEP_FIELDS} FROM steps WHERE idx BETWEEN ?1 AND ?2 ORDER BY idx {order}"
    ))?;
    let mut rows = query.query(params![lo, hi])?;
    let mut items = Vec::new();
    let mut size = 0;
    while let Some(row) = rows.next()? {
        let mut step = Step::read(row)?;
        if descending {
            step.items.reverse();
        }
        for item in step.items.into_iter().filter(&keep) {
            size += wire_size(&item);
            if items.len() == limit || size > FRAME_BUDGET {
                return Ok((items, true));
            }
            items.push(item);
        }
    }
    Ok((items, false))
}

#[derive(Clone, PartialEq, Eq)]
struct Anchor {
    idx: i64,
    kind: i64,
    ts: Option<DateTime<Utc>>,
}

pub(crate) struct Watch {
    path: PathBuf,
    id: (u64, u64),
    trajectory: String,
    stamp: Option<Stamp>,
    after: u64,
    high: i64,
    anchor: Option<Anchor>,
    pending: BTreeMap<i64, Vec<TimelineItem>>,
    queued: VecDeque<TimelineItem>,
}

impl Watch {
    pub(crate) fn open(path: PathBuf, after: u64) -> Result<Self> {
        let stamp = Stamp::read(&path)?;
        let db = open(&path, stamp)?;
        let tx = db.unchecked_transaction()?;
        let trajectory = verify(&tx, &path, true)?;
        // A status-index lookup finds calls already in flight before the
        // cursor. Refresh them by primary key until they settle.
        let high = index(after);
        let mut query =
            tx.prepare("SELECT idx FROM steps WHERE status IN (1,2,8,9,11) AND idx < ?1")?;
        let pending = query
            .query_map([high], |row| row.get(0))?
            .map(|row| row.map(|idx| (idx, Vec::new())))
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self {
            path,
            id: stamp.main.id,
            trajectory,
            stamp: None,
            after,
            high,
            anchor: None,
            pending,
            queued: VecDeque::new(),
        })
    }

    pub(crate) fn updates(&mut self, path: &Path) -> Result<Vec<TimelineItem>> {
        if path != self.path {
            return Err(Error::Reset);
        }
        let stamp = Stamp::read(path)?;
        if stamp.main.id != self.id {
            return Err(Error::Reset);
        }
        if self.queued.is_empty() && self.stamp != Some(stamp) {
            match self.read_changes(stamp) {
                Ok(()) => {}
                // Do not reset a watch on transient writer contention.
                Err(Error::Busy) => return Ok(Vec::new()),
                Err(error) => return Err(error),
            }
        }
        let mut size = 0;
        let mut output = Vec::new();
        while let Some(item) = self.queued.front() {
            size += wire_size(item);
            if size > FRAME_BUDGET || output.len() == MAX_ITEMS {
                break;
            }
            output.extend(self.queued.pop_front());
        }
        Ok(output)
    }

    fn read_changes(&mut self, stamp: Stamp) -> Result<()> {
        let db = open(&self.path, stamp)?;
        let tx = db.unchecked_transaction()?;
        if verify(&tx, &self.path, false)? != self.trajectory {
            return Err(Error::Reset);
        }
        let mut point = tx.prepare(&format!("SELECT {STEP_FIELDS} FROM steps WHERE idx = ?1"))?;
        let mut changed = BTreeMap::new();
        let mut pending = self.pending.clone();
        let mut anchor = self.anchor.clone();
        if self.high >= 0 {
            let mut rows = point.query([self.high])?;
            let last = Step::read(rows.next()?.ok_or(Error::Reset)?)?;
            if anchor
                .as_ref()
                .is_some_and(|anchor| *anchor != last.anchor())
            {
                return Err(Error::Reset);
            }
            if anchor.is_none() {
                // Re-send the cursor row in case it changed between the page
                // and subscription. The consumer merges by seq.
                changed.extend(last.items.iter().cloned().map(|item| (item.seq, item)));
            }
            anchor = Some(last.anchor());
        }
        for (&idx, old) in &self.pending {
            let mut rows = point.query([idx])?;
            let step = Step::read(rows.next()?.ok_or(Error::Reset)?)?;
            if old
                .iter()
                .any(|old| !step.items.iter().any(|item| old.seq == item.seq))
            {
                return Err(Error::Reset);
            }
            if step.items != *old {
                changed.extend(step.items.iter().cloned().map(|item| (item.seq, item)));
            }
            if proto::running(step.status) {
                pending.insert(idx, step.items);
            } else {
                pending.remove(&idx);
            }
        }

        let mut query = tx.prepare(&format!(
            "SELECT {STEP_FIELDS} FROM steps WHERE idx > ?1 ORDER BY idx LIMIT ?2"
        ))?;
        let mut rows = query.query(params![self.high, ROWS_PER_POLL])?;
        let mut scanned = 0;
        let mut high = self.high;
        while let Some(row) = rows.next()? {
            let step = Step::read(row)?;
            scanned += 1;
            high = step.idx;
            anchor = Some(step.anchor());
            if proto::running(step.status) {
                pending.insert(step.idx, step.items.clone());
            }
            changed.extend(
                step.items
                    .into_iter()
                    .filter(|item| item.seq > self.after)
                    .map(|item| (item.seq, item)),
            );
        }
        self.stamp = (scanned < ROWS_PER_POLL).then_some(stamp);
        self.high = high;
        self.anchor = anchor;
        self.pending = pending;
        self.queued.extend(changed.into_values());
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Title {
    stamp: Option<Stamp>,
    title: Option<String>,
    last_checked: Option<i64>,
}

impl Title {
    pub(super) fn read(&mut self, path: &Path) -> Option<super::SidebarTranscript> {
        let stamp = Stamp::read(path).ok()?;
        if self
            .stamp
            .is_some_and(|old| old.main.id != stamp.main.id || old.main.len > stamp.main.len)
        {
            *self = Self::default();
        }
        if self.stamp != Some(stamp) && (self.title.is_some() || self.find(path, stamp).is_ok()) {
            self.stamp = Some(stamp);
        }
        Some(super::SidebarTranscript {
            modified: stamp.modified(),
            title: self.title.clone(),
        })
    }

    fn find(&mut self, path: &Path, stamp: Stamp) -> Result<()> {
        let db = open(path, stamp)?;
        let tx = db.unchecked_transaction()?;
        verify(&tx, path, false)?;
        let mut query = tx.prepare(&format!(
            "SELECT {STEP_FIELDS} FROM steps WHERE step_type = ?1 AND idx > ?2 ORDER BY idx"
        ))?;
        let mut rows = query.query(params![proto::USER, self.last_checked.unwrap_or(-1)])?;
        while let Some(row) = rows.next()? {
            let step = Step::read(row)?;
            if proto::running(step.status) {
                break;
            }
            self.last_checked = Some(step.idx);
            if let Some(title) = step.items.iter().find_map(|item| {
                let TimelineKind::UserMessage { text } = &item.kind else {
                    return None;
                };
                text.lines()
                    .next()
                    .map(|first| first.chars().take(60).collect())
            }) {
                self.title = Some(title);
                break;
            }
        }
        Ok(())
    }
}
