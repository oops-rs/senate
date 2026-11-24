//! Resume is scoped to one grant projection. Positions are never disclosed.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use senate_observe_protocol::{
    Cursor, Event, EventFrame, MAX_FRAME_BYTES, MAX_STATE_BYTES, Snapshot, V1,
};

const MAX_FRAMES: usize = 512;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(60);

pub(crate) fn new_cursor() -> Result<Cursor, JournalError> {
    use base64::Engine as _;

    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|_| JournalError::Unavailable)?;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(bytes)
        .parse()
        .map_err(|_| JournalError::Unavailable)
}

pub struct Hint {
    pub at: Option<chrono::DateTime<Utc>>,
    pub committee_id: Option<String>,
    pub link: Option<String>,
    pub event: Event,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum JournalError {
    Unavailable,
    TooLarge,
    Lost,
}

#[derive(Clone)]
pub(crate) struct Frame {
    pub cursor: Cursor,
    pub data: Arc<str>,
    pub(crate) position: u64,
    at: Instant,
}

pub(crate) struct Journal {
    state: Arc<str>,
    state_size: usize,
    cursor: Cursor,
    first_cursor: Cursor,
    head: u64,
    frames: VecDeque<Frame>,
    bytes: usize,
}

impl Journal {
    pub(crate) fn new(mut snapshot: Snapshot) -> Result<Self, JournalError> {
        let cursor = new_cursor()?;
        snapshot.cursor = cursor.clone();
        let data = encode(&snapshot, MAX_STATE_BYTES)?;
        Ok(Self {
            state_size: data.len(),
            state: data,
            cursor: cursor.clone(),
            first_cursor: cursor,
            head: 0,
            frames: VecDeque::new(),
            bytes: 0,
        })
    }

    pub(crate) fn snapshot(&self) -> Arc<str> {
        self.state.clone()
    }

    pub(crate) fn state_bytes(&self) -> usize {
        self.state_size
    }

    pub(crate) fn boundary(&self, cursor: Option<&Cursor>) -> Option<u64> {
        match cursor {
            None => Some(self.head),
            Some(cursor) if cursor == &self.cursor => Some(self.head),
            Some(cursor)
                if self.head == self.frames.len() as u64 && cursor == &self.first_cursor =>
            {
                Some(0)
            }
            Some(cursor) => self
                .frames
                .iter()
                .find(|frame| &frame.cursor == cursor)
                .map(|frame| frame.position),
        }
    }

    pub(crate) fn next(&self, after: u64) -> Result<Option<Frame>, JournalError> {
        if self.head == after {
            return Ok(None);
        }
        let first = self.frames.front().ok_or(JournalError::Lost)?;
        let offset = after
            .checked_add(1)
            .and_then(|next| next.checked_sub(first.position))
            .and_then(|delta| usize::try_from(delta).ok())
            .ok_or(JournalError::Lost)?;
        self.frames
            .get(offset)
            .cloned()
            .map(Some)
            .ok_or(JournalError::Lost)
    }

    /// Caller holds the view lock for the entire update. Serialize first, so
    /// a bad or oversized projection cannot publish half an update.
    pub(crate) fn prepare(
        &self,
        mut snapshot: Snapshot,
        hints: Vec<Hint>,
    ) -> Result<Publication, JournalError> {
        let at = Utc::now();
        let now = Instant::now();
        if hints.len() > MAX_FRAMES {
            return Err(JournalError::Lost);
        }
        let mut pending = Vec::with_capacity(hints.len());
        let mut encoded_bytes = 0;
        let mut cursor = self.cursor.clone();
        let mut position = self.head;
        for hint in hints {
            cursor = new_cursor()?;
            position = position.checked_add(1).ok_or(JournalError::Unavailable)?;
            let frame = EventFrame {
                schema_version: V1,
                cursor: cursor.clone(),
                at: hint.at.unwrap_or(at),
                committee_id: hint.committee_id,
                link: hint.link,
                event: hint.event,
            };
            let data = encode(&frame, MAX_FRAME_BYTES)?;
            encoded_bytes += data.len();
            if encoded_bytes > MAX_BYTES {
                return Err(JournalError::Lost);
            }
            pending.push(Frame {
                cursor: cursor.clone(),
                data,
                position,
                at: now,
            });
        }
        snapshot.cursor = cursor.clone();
        let state = encode(&snapshot, MAX_STATE_BYTES)?;
        Ok(Publication {
            state,
            cursor,
            position,
            pending,
            now,
        })
    }

    pub(crate) fn apply(&mut self, publication: Publication) {
        let Publication {
            state,
            cursor,
            position,
            pending,
            now,
        } = publication;
        self.state_size = state.len();
        self.state = state;
        self.head = position;
        self.cursor = cursor;
        for frame in pending {
            self.bytes += frame.data.len();
            self.frames.push_back(frame);
        }
        self.trim(now);
    }

    pub(crate) fn prune(&mut self) {
        self.trim(Instant::now());
    }

    fn trim(&mut self, now: Instant) {
        while self.frames.len() > MAX_FRAMES
            || self.bytes > MAX_BYTES
            || self
                .frames
                .front()
                .is_some_and(|frame| now.duration_since(frame.at) >= MAX_AGE)
        {
            if let Some(frame) = self.frames.pop_front() {
                self.bytes -= frame.data.len();
            }
        }
    }
}

pub(crate) struct Publication {
    state: Arc<str>,
    cursor: Cursor,
    position: u64,
    pending: Vec<Frame>,
    now: Instant,
}

impl Publication {
    pub(crate) fn state_bytes(&self) -> usize {
        self.state.len()
    }
}

fn encode(value: &impl serde::Serialize, limit: usize) -> Result<Arc<str>, JournalError> {
    let text = serde_json::to_string(value).map_err(|_| JournalError::Unavailable)?;
    if text.len() > limit {
        return Err(JournalError::TooLarge);
    }
    Ok(Arc::from(text))
}
