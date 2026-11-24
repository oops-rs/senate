//! Person-only IPC over the local daemon socket.

use super::*;
use crate::ipc::{TimelineDirection, TimelineReply, TimelineUpdate};
use crate::timeline::{self, SidebarTranscript, Transcript};
use chrono::{DateTime, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

impl Shared {
    pub(super) fn timeline_session(&self, id: &str) -> Result<Session, IpcError> {
        self.store
            .session(id)
            .map_err(internal)?
            .filter(|_| !self.hosts.unpublished(id))
            .ok_or_else(|| IpcError::new(codes::NO_SESSION, "no registered session with that ID"))
    }

    pub(super) fn session_working(&self, session: &Session) -> bool {
        session.is_live() && lock(&self.working_sessions).contains_key(&session.session_id)
    }

    pub(super) fn no_longer_working(&self, id: &str) {
        if lock(&self.working_sessions).remove(id).is_some() {
            self.publish(WatchEvent::Sessions);
        }
    }

    /// Only called by the person-authorized sessions request. Run one
    /// blocking task for the whole list and prune entries for missing rows.
    pub(super) async fn sidebar_transcripts<'a>(
        &self,
        sessions: impl Iterator<Item = &'a Session>,
    ) -> HashMap<String, SidebarTranscript> {
        let visible: Vec<_> = sessions
            .filter_map(|session| {
                Some((
                    session.session_id.clone(),
                    session.runtime,
                    timeline::path(session)?,
                ))
            })
            .collect();
        let mut cache = self.session_titles.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || cache.refresh(visible))
            .await
            .unwrap_or_default()
    }

    pub(super) async fn on_timeline(
        &self,
        session_id: String,
        direction: TimelineDirection,
        cursor: Option<u64>,
        limit: Option<usize>,
    ) -> Result<TimelineReply, IpcError> {
        if direction != TimelineDirection::Tail && cursor.is_none() {
            return Err(IpcError::bad_request(
                "before and after require a timeline cursor",
            ));
        }
        let session = self.timeline_session(&session_id)?;
        let runtime = session.runtime;
        let path = timeline::path(&session);
        let limit = limit.unwrap_or(60).clamp(1, 300);
        let (page, unavailable_reason) = tokio::task::spawn_blocking(move || {
            let Some(path) = path else {
                return Ok((None, None));
            };
            if runtime == Runtime::Agy {
                return Ok(match timeline::agy::page(&path, direction, cursor, limit) {
                    Ok(page) => (Some(page), None),
                    Err(error) => (None, error.reason()),
                });
            }
            let Ok(mut transcript) = Transcript::open(path, runtime) else {
                return Ok((None, None));
            };
            transcript
                .page(direction, cursor, limit)
                .map(|page| (Some(page), None))
        })
        .await
        .map_err(IpcError::internal)?
        .map_err(IpcError::internal)?;
        let available = page.is_some();
        let page = page.unwrap_or_default();
        let working = self
            .timeline_session(&session_id)
            .is_ok_and(|session| self.session_working(&session));
        Ok(TimelineReply {
            session_id,
            runtime,
            available,
            unavailable_reason,
            working,
            first_seq: page.items.first().map(|item| item.seq),
            last_seq: page.items.last().map(|item| item.seq),
            has_older: page.has_older,
            has_newer: page.has_newer,
            items: page.items,
        })
    }
}

/// Ended rows keep their last recorded activity even when their file is gone.
pub(super) fn sidebar_metadata(
    session: &Session,
    host_started_at: Option<DateTime<Utc>>,
    transcript: Option<SidebarTranscript>,
) -> (DateTime<Utc>, Option<String>) {
    let last_activity =
        host_started_at.map_or(session.last_seen, |start| start.max(session.last_seen));
    match transcript {
        Some(SidebarTranscript { modified, title }) => (
            modified.map_or(last_activity, |time| time.max(last_activity)),
            title,
        ),
        None => (last_activity, None),
    }
}

pub(super) async fn serve(
    shared: &Shared,
    session_id: String,
    after: u64,
    mut reader: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
    mut shutdown: watch::Receiver<bool>,
) {
    tokio::select! {
        _ = stream(shared, &session_id, after, &mut write) => {},
        // A dedicated stream accepts no further input. A close also wakes
        // this branch while the client is idle or has stopped reading.
        _ = reader.read_u8() => {},
        _ = shutdown.changed() => {},
    }
}

pub(super) async fn send<T: Serialize>(write: &mut OwnedWriteHalf, value: &T) -> bool {
    let Ok(mut line) = serde_json::to_vec(value) else {
        return false;
    };
    line.push(b'\n');
    write.write_all(&line).await.is_ok()
}

enum Source {
    Jsonl(Box<Transcript>),
    Agy(Box<timeline::agy::Watch>),
}

impl Source {
    fn open(path: PathBuf, runtime: Runtime, after: u64) -> Result<Self, IpcError> {
        if runtime == Runtime::Agy {
            return timeline::agy::Watch::open(path, after)
                .map(Box::new)
                .map(Self::Agy)
                .map_err(|error| IpcError::bad_request(error.message()));
        }
        Transcript::open(path, runtime)
            .map(Box::new)
            .map(Self::Jsonl)
            .map_err(|_| IpcError::bad_request("the session has no readable timeline transcript"))
    }

    fn updates(
        &mut self,
        path: &Path,
        runtime: Runtime,
        after: u64,
    ) -> Option<Vec<crate::ipc::TimelineItem>> {
        match self {
            Self::Agy(watch) if runtime == Runtime::Agy => watch.updates(path).ok(),
            Self::Agy(_) => None,
            Self::Jsonl(transcript) => {
                let end = transcript.refreshed_len(path, runtime).ok()??;
                transcript.updates(end, after).ok()
            }
        }
    }
}

async fn stream(shared: &Shared, id: &str, after: u64, write: &mut OwnedWriteHalf) {
    let start = async {
        let session = shared.timeline_session(id)?;
        let runtime = session.runtime;
        let path = timeline::path(&session).ok_or_else(|| {
            IpcError::bad_request("the session has no readable timeline transcript")
        })?;
        tokio::task::spawn_blocking(move || Source::open(path, runtime, after))
            .await
            .map_err(IpcError::internal)?
    };
    let mut transcript = match start.await {
        Ok(transcript) => transcript,
        Err(error) => {
            send(write, &Reply::err(error)).await;
            return;
        }
    };
    let mut previous_working = None;
    loop {
        let session = shared.timeline_session(id).ok();
        let working = session
            .as_ref()
            .is_some_and(|session| shared.session_working(session));
        let state = session.and_then(|session| Some((timeline::path(&session)?, session.runtime)));
        let read = tokio::task::spawn_blocking(move || {
            let (path, runtime) = state?;
            let items = transcript.updates(&path, runtime, after)?;
            Some((transcript, items))
        })
        .await
        .ok()
        .flatten();
        let Some((next, items)) = read else {
            send(
                write,
                &TimelineUpdate {
                    items: Vec::new(),
                    working,
                    reset: true,
                },
            )
            .await;
            return;
        };
        transcript = next;
        if (!items.is_empty() || previous_working != Some(working))
            && !send(
                write,
                &TimelineUpdate {
                    items,
                    working,
                    reset: false,
                },
            )
            .await
        {
            return;
        }
        previous_working = Some(working);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
