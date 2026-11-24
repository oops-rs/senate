//! Rebuildable person-only index. The runtime's transcript is authoritative.
//! This database is local, separate from bridge state and server traffic.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, ensure};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{SearchCheckpoint, TimelineItem, Transcript, agy};
use crate::ipc::{
    SessionSearchHit, SessionSearchKind, SessionSearchReply, TimelineKind, TimelineToolDetail,
};
use crate::session::{Runtime, Session};

const DB: &str = "session-search.sqlite";
const TOOL_OUTPUT: usize = 512;
const SNIPPET: usize = 320;

pub(crate) struct Index(Connection);

#[derive(Serialize, Deserialize, PartialEq, Eq)]
struct Stamp {
    path: PathBuf,
    runtime: Runtime,
    size: u64,
    modified: Option<SystemTime>,
    device: u64,
    inode: u64,
    #[serde(default)]
    incomplete: bool,
}

impl Stamp {
    fn read(path: PathBuf, runtime: Runtime) -> io::Result<Self> {
        let meta = fs::symlink_metadata(&path)?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid transcript file",
            ));
        }
        Ok(Self {
            path,
            runtime,
            size: meta.len(),
            modified: meta.modified().ok(),
            device: meta.dev(),
            inode: meta.ino(),
            incomplete: false,
        })
    }

    fn same_file(&self, other: &Self) -> bool {
        self.path == other.path
            && self.runtime == other.runtime
            && self.device == other.device
            && self.inode == other.inode
    }
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    query_key: String,
    generation: i64,
    offset: u64,
}

impl Index {
    pub(crate) fn exists(home: &Path) -> bool {
        home.join("history").join(DB).is_file()
    }

    pub(crate) fn open(home: &Path) -> anyhow::Result<Self> {
        let dir = home.join("history");
        match fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let meta = fs::symlink_metadata(&dir)?;
        ensure!(
            meta.is_dir() && meta.mode() & 0o077 == 0,
            "session search directory must be private"
        );
        let path = dir.canonicalize()?.join(DB);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.mode() & 0o077 == 0,
            "session search index must be private"
        );
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA secure_delete = ON;
            BEGIN;
            CREATE TABLE IF NOT EXISTS revision (v INTEGER NOT NULL);
            INSERT INTO revision SELECT abs(random() % 1000000000000000000) WHERE NOT EXISTS (SELECT 1 FROM revision);
            CREATE TABLE IF NOT EXISTS sources (session_id TEXT PRIMARY KEY, stamp BLOB, checkpoint BLOB);
            CREATE TABLE IF NOT EXISTS documents (
                id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, seq INTEGER NOT NULL,
                kind INTEGER NOT NULL, ts INTEGER, body TEXT NOT NULL, UNIQUE(session_id, seq)
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS terms USING fts5(body, content='documents', content_rowid='id', tokenize='trigram');
            CREATE TRIGGER IF NOT EXISTS document_insert AFTER INSERT ON documents BEGIN
                INSERT INTO terms(rowid, body) VALUES (new.id, new.body);
            END;
            CREATE TRIGGER IF NOT EXISTS document_delete AFTER DELETE ON documents BEGIN
                INSERT INTO terms(terms, rowid, body) VALUES ('delete', old.id, old.body);
            END;
            CREATE TRIGGER IF NOT EXISTS document_update AFTER UPDATE OF body ON documents
                WHEN old.body IS NOT new.body BEGIN
                INSERT INTO terms(terms, rowid, body) VALUES ('delete', old.id, old.body);
                INSERT INTO terms(rowid, body) VALUES (new.id, new.body);
            END;
            COMMIT;
        ")?;
        if meta.len() == 0 {
            conn.execute(
                "INSERT INTO terms(terms, rank) VALUES ('secure-delete', 1)",
                [],
            )?;
        }
        Ok(Self(conn))
    }

    pub(crate) fn open_read(home: &Path) -> anyhow::Result<Self> {
        let dir = home.join("history");
        let meta = fs::symlink_metadata(&dir)?;
        ensure!(
            meta.is_dir() && meta.mode() & 0o077 == 0,
            "session search directory must be private"
        );
        let path = dir.canonicalize()?.join(DB);
        let meta = fs::symlink_metadata(&path)?;
        ensure!(
            meta.is_file() && meta.mode() & 0o077 == 0,
            "session search index must be private"
        );
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        conn.busy_timeout(std::time::Duration::from_millis(750))?;
        conn.execute_batch("PRAGMA temp_store=MEMORY")?;
        Ok(Self(conn))
    }

    pub(crate) fn remove(&mut self, ids: &[String]) -> anyhow::Result<()> {
        let tx = self.0.transaction()?;
        let mut changed = false;
        for id in ids {
            changed |= clear(&tx, id)?;
        }
        if changed {
            bump(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn retain(&mut self, ids: &HashSet<&str>) -> anyhow::Result<()> {
        let removed = {
            let mut stmt = self.0.prepare("SELECT session_id FROM sources")?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|id| !ids.contains(id.as_str()))
                .collect::<Vec<_>>()
        };
        self.remove(&removed)
    }

    pub(crate) fn refresh_one(&mut self, session: &Session) -> anyhow::Result<(bool, bool)> {
        let Some(path) = super::path(session) else {
            self.remove(std::slice::from_ref(&session.session_id))?;
            return Ok((false, false));
        };
        let mut stamp = match Stamp::read(path, session.runtime) {
            Ok(stamp) => stamp,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
                ) =>
            {
                self.remove(std::slice::from_ref(&session.session_id))?;
                return Ok((false, false));
            }
            Err(_) => return Ok((false, false)),
        };
        let tx = self.0.transaction()?;
        let (changed, more) = index_source(&tx, &session.session_id, &mut stamp)?;
        if changed {
            bump(&tx)?;
        }
        tx.commit()?;
        Ok((changed, more))
    }

    pub(crate) fn search(
        &self,
        query: &str,
        committee: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        sessions: &HashMap<&str, bool>,
    ) -> anyhow::Result<SessionSearchReply> {
        let query = query.trim();
        ensure!(
            !query.is_empty() && query.len() <= 512 && !query.contains('\0'),
            "query must have 1–512 bytes"
        );
        // Read ranking and revision from the same committed snapshot.
        let conn = self.0.unchecked_transaction()?;
        let generation = conn.query_row("SELECT v FROM revision", [], |row| row.get(0))?;
        let mut members: Vec<_> = sessions.keys().copied().collect();
        members.sort_unstable();
        let query_key = hex::encode(Sha256::digest(serde_json::to_vec(&(
            query, committee, members,
        ))?));
        let offset = match cursor {
            Some(cursor) => {
                let cursor: Cursor =
                    serde_json::from_str(cursor).context("invalid session search cursor")?;
                ensure!(
                    cursor.generation == generation && cursor.query_key == query_key,
                    "the session search changed; start at the first page"
                );
                cursor.offset
            }
            None => 0,
        };
        // The caller supplies current committee membership and liveness. A
        // temp table keeps filtered pagination and totals inside SQLite.
        conn.execute_batch("CREATE TEMP TABLE IF NOT EXISTS visible(session_id TEXT PRIMARY KEY, live INTEGER); DELETE FROM visible;")?;
        {
            let mut insert = conn.prepare("INSERT INTO visible VALUES (?1, ?2)")?;
            for (id, live) in sessions {
                insert.execute(params![id, live])?;
            }
        }
        let trigram = query.chars().count() >= 3;
        let (sql, needle) = if trigram {
            (
                "SELECT d.session_id, d.seq, d.kind, d.ts, d.body, v.live
              FROM terms JOIN documents d ON d.id = terms.rowid JOIN visible v USING(session_id)
              WHERE terms MATCH ?1 ORDER BY bm25(terms), d.ts DESC, d.id DESC LIMIT ?2 OFFSET ?3",
                format!("\"{}\"", query.replace('"', "\"\"")),
            )
        } else {
            // FTS5's trigram MATCH cannot represent one or two characters.
            // Scan the derived text for those queries, including short CJK.
            (
                "SELECT d.session_id, d.seq, d.kind, d.ts, d.body, v.live
              FROM documents d JOIN visible v USING(session_id)
              WHERE instr(lower(d.body), lower(?1)) > 0
              ORDER BY d.ts DESC, d.id DESC LIMIT ?2 OFFSET ?3",
                query.to_owned(),
            )
        };
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(params![needle, (limit + 1) as i64, i64::try_from(offset)?])?;
        let mut hits = Vec::new();
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if hits.len() == limit {
                has_more = true;
                break;
            }
            let body: String = row.get(4)?;
            let ts: Option<i64> = row.get(3)?;
            hits.push(SessionSearchHit {
                session_id: row.get(0)?,
                seq: u64::try_from(row.get::<_, i64>(1)?)?,
                kind: match row.get::<_, u8>(2)? {
                    0 => SessionSearchKind::UserMessage,
                    1 => SessionSearchKind::AssistantMessage,
                    _ => SessionSearchKind::ToolCall,
                },
                snippet: snippet(&body, query),
                ts: ts.and_then(DateTime::<Utc>::from_timestamp_millis),
                live: row.get(5)?,
            });
        }
        let next_cursor = has_more
            .then(|| {
                serde_json::to_string(&Cursor {
                    query_key,
                    generation,
                    offset: offset + hits.len() as u64,
                })
            })
            .transpose()?;
        Ok(SessionSearchReply {
            hits,
            next_cursor,
            indexing: false,
            progress: None,
        })
    }
}

fn bump(tx: &Transaction<'_>) -> anyhow::Result<()> {
    tx.execute("UPDATE revision SET v=v+1", [])?;
    Ok(())
}

fn clear(tx: &Transaction<'_>, id: &str) -> anyhow::Result<bool> {
    let deleted = tx.execute("DELETE FROM documents WHERE session_id=?1", [id])?;
    let source = tx.execute("DELETE FROM sources WHERE session_id=?1", [id])?;
    Ok(deleted != 0 || source != 0)
}

fn remember(
    tx: &Transaction<'_>,
    id: &str,
    stamp: &Stamp,
    checkpoint: &impl Serialize,
) -> anyhow::Result<()> {
    tx.execute(
        "INSERT OR REPLACE INTO sources VALUES (?1, ?2, ?3)",
        params![
            id,
            serde_json::to_vec(stamp)?,
            serde_json::to_vec(checkpoint)?
        ],
    )?;
    Ok(())
}

fn index_source(tx: &Transaction<'_>, id: &str, stamp: &mut Stamp) -> anyhow::Result<(bool, bool)> {
    let previous: Option<(Vec<u8>, Vec<u8>)> = tx
        .query_row(
            "SELECT stamp, checkpoint FROM sources WHERE session_id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let prior = previous.as_ref().and_then(|(meta, checkpoint)| {
        serde_json::from_slice::<Stamp>(meta)
            .ok()
            .map(|meta| (meta, checkpoint.as_slice()))
    });
    if stamp.runtime != Runtime::Agy && prior.as_ref().is_some_and(|(old, _)| old == stamp) {
        return Ok((false, false));
    }
    let mut insert = tx.prepare_cached("INSERT INTO documents(session_id,seq,kind,ts,body) VALUES (?1,?2,?3,?4,?5)
        ON CONFLICT(session_id,seq) DO UPDATE SET body=excluded.body, ts=excluded.ts, kind=excluded.kind
        WHERE body IS NOT excluded.body OR ts IS NOT excluded.ts OR kind IS NOT excluded.kind")?;
    let mut changed = false;
    let mut put = |item: TimelineItem| -> anyhow::Result<()> {
        if let Some((kind, body)) = text(&item.kind) {
            changed |= insert.execute(params![
                id,
                i64::try_from(item.seq)?,
                kind,
                item.ts.map(|t| t.timestamp_millis()),
                body
            ])? != 0;
        }
        Ok(())
    };
    let mut reset = false;
    let mut more = false;
    if stamp.runtime == Runtime::Agy {
        let mut watch = match prior
            .filter(|(old, _)| old.same_file(stamp))
            .and_then(|(_, checkpoint)| serde_json::from_slice::<agy::Watch>(checkpoint).ok())
        {
            Some(watch) => watch,
            None => {
                let Ok(watch) = agy::Watch::open(stamp.path.clone(), 0) else {
                    return Ok((clear(tx, id)?, false));
                };
                reset = clear(tx, id)?;
                watch
            }
        };
        let mut retry_reset = true;
        loop {
            let (items, continuing) = match watch.index_batch(&stamp.path) {
                Ok(batch) => batch,
                Err(agy::Error::Reset) if retry_reset => {
                    retry_reset = false;
                    reset |= clear(tx, id)?;
                    let Ok(new) = agy::Watch::open(stamp.path.clone(), 0) else {
                        return Ok((reset, false));
                    };
                    watch = new;
                    continue;
                }
                Err(agy::Error::Busy) => break,
                Err(_) => return Ok((clear(tx, id)?, false)),
            };
            for item in items {
                put(item)?;
            }
            more = continuing;
            break;
        }
        let checkpoint = serde_json::to_vec(&watch)?;
        if previous.as_ref().is_none_or(|(_, old)| *old != checkpoint) {
            tx.execute(
                "INSERT OR REPLACE INTO sources VALUES (?1,?2,?3)",
                params![id, serde_json::to_vec(stamp)?, checkpoint],
            )?;
        }
    } else {
        let Ok(mut transcript) = Transcript::open(stamp.path.clone(), stamp.runtime) else {
            return Ok((clear(tx, id)?, false));
        };
        let cursor = prior
            .filter(|(old, _)| {
                old.same_file(stamp)
                    && (old.size < stamp.size
                        || (old.incomplete
                            && old.size == stamp.size
                            && old.modified == stamp.modified))
            })
            .and_then(|(_, checkpoint)| {
                serde_json::from_slice::<SearchCheckpoint>(checkpoint).ok()
            });
        if cursor.is_none_or(|cursor| !transcript.resume_search(cursor).unwrap_or(false)) {
            reset = clear(tx, id)?;
        }
        // Publish a giant JSONL in chunks. If a physical record spans the
        // boundary, extend just far enough to make progress on that record.
        const CHUNK: u64 = 4 * 1024 * 1024;
        let length = stamp.size.min(transcript.snapshot_len()?);
        let start = transcript.offset;
        let mut end = length.min(start + CHUNK);
        loop {
            while let Some(change) = transcript.next(end)? {
                put(change.item)?;
            }
            if end == length || transcript.offset - transcript.record.len() as u64 > start {
                break;
            }
            end = length.min(end + CHUNK);
        }
        more = end < length;
        stamp.incomplete = more;
        remember(tx, id, stamp, &transcript.search_checkpoint()?)?;
    }
    Ok((changed || reset, more))
}

fn text(kind: &TimelineKind) -> Option<(u8, String)> {
    match kind {
        TimelineKind::UserMessage { text } => Some((0, text.clone())),
        TimelineKind::AssistantMessage { text } => Some((1, text.clone())),
        TimelineKind::ToolCall {
            name,
            detail,
            output,
            ..
        } => {
            let mut parts = vec![name.as_str()];
            let mut selected_output = output.as_deref();
            match detail {
                TimelineToolDetail::Shell {
                    command,
                    cwd,
                    output,
                    ..
                } => {
                    parts.push(command);
                    parts.extend(cwd.as_deref());
                    selected_output = output.as_deref().or(selected_output);
                }
                TimelineToolDetail::Read { path }
                | TimelineToolDetail::Write { path }
                | TimelineToolDetail::Edit { path, .. } => parts.push(path),
                TimelineToolDetail::Search { query } => parts.push(query),
                TimelineToolDetail::Fetch { url } => parts.push(url),
                TimelineToolDetail::SubAgent { description } => parts.push(description),
                TimelineToolDetail::Other { summary } => parts.push(summary),
            }
            parts.extend(selected_output.map(|text| prefix(text, TOOL_OUTPUT)));
            Some((2, parts.join("\n")))
        }
        _ => None,
    }
}

fn prefix(text: &str, max: usize) -> &str {
    &text[..text.floor_char_boundary(max.min(text.len()))]
}

fn snippet(body: &str, needle: &str) -> String {
    let lower = body.to_lowercase();
    let position = lower.find(&needle.to_lowercase()).unwrap_or(0);
    let original = body
        .char_indices()
        .scan(0, |at, (original, ch)| {
            let first = *at;
            *at += ch.to_lowercase().map(char::len_utf8).sum::<usize>();
            Some((first, original))
        })
        .take_while(|(lowered, _)| *lowered <= position)
        .last()
        .map_or(0, |(_, at)| at);
    let start = body.floor_char_boundary(original.saturating_sub(70));
    let part = prefix(&body[start..], SNIPPET);
    let clean: String = part
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    format!(
        "{}{clean}{}",
        if start == 0 { "" } else { "…" },
        if start + part.len() == body.len() {
            ""
        } else {
            "…"
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn a_reader_sees_committed_hits_before_one_large_transcript_finishes() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let file = home.path().join("source.jsonl");
        let mut transcript = fs::File::create(&file)?;
        writeln!(
            transcript,
            "{}",
            serde_json::json!({"type":"user","message":{"content":"first indexed prompt"}})
        )?;
        let ignored = serde_json::json!({"type":"user","isMeta":true,"message":{"content":"x".repeat(128 * 1024)}}).to_string();
        for _ in 0..40 {
            writeln!(transcript, "{ignored}")?;
        }
        writeln!(
            transcript,
            "{}",
            serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"last indexed reply"}]}})
        )?;
        let session = Session {
            session_id: "archive".into(),
            runtime: Runtime::Claude,
            agent: "operator".parse()?,
            agent_source: crate::agent::AgentSource::Session,
            cwd: home.path().into(),
            git_root: None,
            branch: None,
            inferred_committee: None,
            committee_source: None,
            joined_committee: None,
            socket: None,
            token: None,
            claude_pid: None,
            transcript_path: Some(file.to_string_lossy().into_owned()),
            state: crate::session::SessionState::Ended,
            started_at: Utc::now(),
            last_seen: Utc::now(),
            socket_ok: None,
        };
        let mut writer = Index::open(home.path())?;
        assert_eq!(writer.refresh_one(&session)?, (true, true));
        let reader = Index::open_read(home.path())?;
        let visible = HashMap::from([("archive", false)]);
        assert_eq!(
            reader
                .search("first indexed", None, None, 5, &visible)?
                .hits
                .len(),
            1
        );
        assert!(
            reader
                .search("last indexed", None, None, 5, &visible)?
                .hits
                .is_empty()
        );
        assert_eq!(writer.refresh_one(&session)?, (true, false));
        assert_eq!(
            reader
                .search("last indexed", None, None, 5, &visible)?
                .hits
                .len(),
            1
        );
        assert_eq!(writer.refresh_one(&session)?, (false, false));
        Ok(())
    }
}
