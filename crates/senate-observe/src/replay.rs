//! Disposable playback feeds the identical authenticated Observe router.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use senate_core::{CommitteeId, Event as LogEvent, Seq};
use senate_observe_protocol::recording::{self, Content, Header, Row};
use senate_observe_protocol::{Event, Snapshot};
use tokio::sync::broadcast;

use crate::grants::lock;
use crate::{Capture, GrantInfo, Hint, LocalNotice, ProjectionError, Registry, SenateSource};

const LIMIT: u64 = 100 * 1024 * 1024;
const MAX_ROW: u64 = 4 * 1024 * 1024 + 1024;

pub struct Recording {
    initial: Snapshot,
    rows: Vec<Row>,
    includes_lines: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("cannot read Observe recording")]
    Io(#[from] std::io::Error),
    #[error("invalid Observe recording")]
    Invalid,
    #[error("Observe recording exceeds its limit")]
    Limit,
}

impl Recording {
    pub fn read(path: &Path) -> Result<Self, ReplayError> {
        Self::read_with_limit(path, LIMIT)
    }

    pub fn read_with_limit(path: &Path, limit: u64) -> Result<Self, ReplayError> {
        let limit = limit.min(1024 * 1024 * 1024);
        let file = File::open(path)?;
        if file.metadata()?.len() > limit {
            return Err(ReplayError::Limit);
        }
        Self::parse(BufReader::new(file.take(limit + 1)))
    }

    pub fn demo() -> Result<Self, ReplayError> {
        Self::parse(include_bytes!("demo.ndjson").as_slice())
    }

    pub fn includes_lines(&self) -> bool {
        self.includes_lines
    }

    fn parse(mut input: impl BufRead) -> Result<Self, ReplayError> {
        let Some(header) = line(&mut input)? else {
            return Err(ReplayError::Invalid);
        };
        let _: Header = serde_json::from_slice(&header).map_err(|_| ReplayError::Invalid)?;
        let Some(first) = line(&mut input)? else {
            return Err(ReplayError::Invalid);
        };
        let Row {
            offset_ms: 0,
            row: Content::Snapshot(mut snapshot),
        } = serde_json::from_slice(&first).map_err(|_| ReplayError::Invalid)?
        else {
            return Err(ReplayError::Invalid);
        };
        let mut includes_lines = snapshot
            .capabilities
            .iter()
            .any(|capability| capability == "lines_auto" || capability == "published_lines");
        recording::sanitize_snapshot(&mut snapshot, includes_lines);
        let mut rows = Vec::new();
        let mut previous = 0;
        let mut discontinuity = false;
        let mut ended = false;
        while let Some(bytes) = line(&mut input)? {
            if ended {
                return Err(ReplayError::Invalid);
            }
            let mut row: Row = serde_json::from_slice(&bytes).map_err(|_| ReplayError::Invalid)?;
            if row.offset_ms < previous || row.offset_ms > 7 * 24 * 60 * 60 * 1000 {
                return Err(ReplayError::Invalid);
            }
            previous = row.offset_ms;
            match &mut row.row {
                Content::Snapshot(snapshot) if discontinuity => {
                    discontinuity = false;
                    let caps = &snapshot.capabilities;
                    includes_lines |= caps.iter().any(|capability| {
                        capability == "lines_auto" || capability == "published_lines"
                    });
                    recording::sanitize_snapshot(snapshot, includes_lines);
                }
                Content::Frame(event) if !discontinuity => {
                    if !recording::sanitize_frame(event, includes_lines) {
                        continue;
                    }
                }
                Content::Discontinuity if !discontinuity => discontinuity = true,
                Content::End if !discontinuity => ended = true,
                _ => return Err(ReplayError::Invalid),
            }
            rows.push(row);
            if rows.len() > 200_000 {
                return Err(ReplayError::Limit);
            }
        }
        if discontinuity {
            return Err(ReplayError::Invalid);
        }
        Ok(Self {
            initial: *snapshot,
            rows,
            includes_lines,
        })
    }

    pub fn attach(self, registry: &Arc<Registry>) {
        let replay = Arc::new(Playback {
            registry: Arc::downgrade(registry),
            inner: Arc::new(Mutex::new(self.initial)),
            rows: Mutex::new(Some(self.rows)),
            events: broadcast::channel(1).0,
        });
        registry.attach_senate(replay);
    }
}

fn line(input: &mut impl BufRead) -> Result<Option<Vec<u8>>, ReplayError> {
    let mut line = Vec::new();
    let length = input.take(MAX_ROW + 1).read_until(b'\n', &mut line)?;
    if length as u64 > MAX_ROW {
        return Err(ReplayError::Limit);
    }
    if length == 0 || !line.ends_with(b"\n") {
        return Ok(None);
    }
    Ok(Some(line))
}

struct Playback {
    registry: Weak<Registry>,
    inner: Arc<Mutex<Snapshot>>,
    rows: Mutex<Option<Vec<Row>>>,
    events: broadcast::Sender<LogEvent>,
}

#[async_trait]
impl SenateSource for Playback {
    fn subscribe(&self) -> broadcast::Receiver<LogEvent> {
        self.events.subscribe()
    }

    async fn capture(&self, _: &GrantInfo) -> Result<Capture, ProjectionError> {
        Ok(Capture {
            snapshot: lock(&self.inner).clone(),
            heads: BTreeMap::new(),
        })
    }

    async fn read(
        &self,
        _: &CommitteeId,
        _: Seq,
        _: usize,
    ) -> Result<Vec<LogEvent>, ProjectionError> {
        Ok(Vec::new())
    }

    async fn hint(
        &self,
        _: &GrantInfo,
        _: &LogEvent,
        _: &Snapshot,
    ) -> Result<Option<Hint>, ProjectionError> {
        Ok(None)
    }

    async fn notice(
        &self,
        _: &GrantInfo,
        notice: LocalNotice,
        _: &Snapshot,
    ) -> Result<Option<Hint>, ProjectionError> {
        match notice {
            LocalNotice::Playback(frame) if !matches!(frame.event, Event::StatePatch(_)) => {
                Ok(Some(Hint {
                    at: Some(frame.at),
                    committee_id: frame.committee_id,
                    link: None,
                    event: frame.event,
                }))
            }
            _ => Ok(None),
        }
    }

    fn stream_opened(&self) {
        let Some(rows) = lock(&self.rows).take() else {
            return;
        };
        let registry = self.registry.clone();
        let state = self.inner.clone();
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            for row in rows {
                tokio::time::sleep_until(started + Duration::from_millis(row.offset_ms)).await;
                let Some(registry) = registry.upgrade() else {
                    break;
                };
                match row.row {
                    Content::Snapshot(snapshot) => {
                        *lock(&state) = *snapshot;
                        registry.invalidate_views();
                    }
                    Content::Frame(frame) => {
                        recording::apply_frame(&mut lock(&state), &frame);
                        registry.notice(LocalNotice::Playback(frame));
                    }
                    Content::Discontinuity => {}
                    Content::End => break,
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateGrant, Scope};

    #[tokio::test]
    async fn demo_reduces_the_recorded_unseat_and_retains_no_live_links()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let registry = Registry::open(dir.path(), "127.0.0.1:43210".parse()?)?;
        let mut recording = Recording::demo()?;
        assert!(!recording.includes_lines());
        assert!(
            recording
                .initial
                .committees
                .iter()
                .all(|row| row.link.is_none())
        );
        assert_eq!(recording.initial.senators.len(), 2);
        for row in &mut recording.rows {
            row.offset_ms = 0;
        }
        recording.attach(&registry);
        let grant = registry.create(
            CreateGrant {
                name: "demo".into(),
                committees: None,
                all_committees: true,
                scope: Scope::Metadata,
                lines: None,

                origins: vec![],
                topology: false,
                expires_in_days: None,
            },
            Default::default(),
        )?;
        let source = registry
            .projections
            .source()
            .ok_or("missing playback source")?;
        source.stream_opened();
        let state = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let capture = source
                    .capture(&grant.grant)
                    .await
                    .map_err(|_| "playback capture failed")?;
                if capture.snapshot.senators.len() == 1 {
                    break Ok::<_, &str>(capture.snapshot);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(
            state.senators[0].state,
            senate_observe_protocol::SenatorState::Away
        );
        Ok(())
    }
}
