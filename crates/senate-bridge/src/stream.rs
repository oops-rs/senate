//! One agent's stream worker on one committee: the SSE consumer and the
//! reporter.
//!
//! Each of this bridge's agents holds its own stream on each committee it
//! is joined to (`?senator=<agent>`): the server counts that stream for the
//! agent's presence, and the agent's cursor, queue and heartbeat are its
//! own. Two agents of one person on one committee are two workers.
//!
//! Before connecting with no local cursor (never joined here, or reset)
//! the consumer seeds one ([`Shared::ensure_seeded`]). It then opens
//! `GET .../stream` with `Last-Event-ID` = the agent's durable `delivered`
//! cursor, checks the log epoch the response names, hands every event to
//! [`Shared::take_in`] (which persists before the cursor moves), and
//! reconnects with exponential backoff when the stream ends, errors, or goes
//! quiet past [`Timing::stall_timeout`](crate::Timing) (the server pings
//! every 15 s).
//!
//! An event that cannot be decoded is passed by seq, never retried: the
//! cursor moves past it with a warning. A changed epoch or a 409
//! `cursor_ahead` means the server's log was reset; the worker hands that to
//! [`Shared::resync`] and reconnects. When another worker resyncs every
//! committee, this one drops its stream and reconnects too.
//!
//! The reporter `PUT`s the agent's `delivered` cursor shortly after it
//! moves and, every heartbeat period, the cursor again plus the agent's
//! heartbeat.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::anyhow;
use chrono::Utc;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use senate_core::api;
use senate_core::{CommitteeId, Event, SenatorPath, Seq};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::client;
use crate::daemon::{Incoming, Intake, Shared, Timing};
use crate::ipc::StreamState;

pub(crate) struct CommitteeWorker {
    state: Arc<Mutex<StreamState>>,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn set(state: &Mutex<StreamState>, value: StreamState) {
    *lock(state) = value;
}

impl CommitteeWorker {
    pub(crate) fn state(&self) -> StreamState {
        lock(&self.state).clone()
    }

    pub(crate) fn stop(&self) {
        // An error means both tasks already exited.
        let _ = self.stop.send(true);
    }

    pub(crate) fn into_tasks(self) -> Vec<JoinHandle<()>> {
        self.tasks
    }
}

pub(crate) fn spawn(
    shared: Arc<Shared>,
    agent: SenatorPath,
    committee: CommitteeId,
) -> CommitteeWorker {
    let (stop, rx) = watch::channel(false);
    let state = Arc::new(Mutex::new(StreamState::Connecting { attempt: 0 }));
    let dirty = Arc::new(Notify::new());
    let resyncs = shared.resyncs();
    let tasks = vec![
        tokio::spawn(run_stream(
            shared.clone(),
            agent.clone(),
            committee.clone(),
            state.clone(),
            dirty.clone(),
            rx.clone(),
            resyncs,
        )),
        tokio::spawn(run_reporter(shared, agent, committee, dirty, rx)),
    ];
    CommitteeWorker { state, stop, tasks }
}

/// `base · 2^(attempt-1)`, capped, with ±20% jitter so laptops waking
/// together do not reconnect in step.
fn backoff(timing: &Timing, attempt: u32) -> Duration {
    let exp = attempt.saturating_sub(1).min(16);
    let raw = timing.backoff_base.saturating_mul(1u32 << exp);
    let capped = raw.min(timing.backoff_max);
    let noise = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() % 400)
        .unwrap_or(200);
    capped.mul_f64(0.8 + f64::from(noise) / 1000.0)
}

/// How one connection ended.
enum Ended {
    /// Asked to stop.
    Stop,
    /// Lost or refused; reconnect after a backoff.
    Lost(anyhow::Error),
    /// The server's log was reset (or another worker resynced); resync,
    /// then reconnect. Carries the resync generation the connection began in.
    Resync(crate::daemon::Resync, u64),
}

async fn run_stream(
    shared: Arc<Shared>,
    agent: SenatorPath,
    committee: CommitteeId,
    state: Arc<Mutex<StreamState>>,
    dirty: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
    mut resyncs: watch::Receiver<u64>,
) {
    let mut attempt: u32 = 0;
    while !*stop.borrow() {
        set(&state, StreamState::Connecting { attempt });
        let ended = consume(
            &shared,
            (&agent, &committee),
            &state,
            &dirty,
            &mut stop,
            &mut resyncs,
            &mut attempt,
        )
        .await;
        let err = match ended {
            Ended::Stop => break,
            Ended::Lost(e) => e,
            Ended::Resync(resync, generation) => {
                let err = anyhow!("{resync}");
                shared.resync(&committee, resync, generation).await;
                err
            }
        };
        attempt = attempt.saturating_add(1);
        let delay = backoff(&shared.timing, attempt);
        tracing::warn!(%agent, %committee, attempt, "stream lost: {err:#}; retrying in {delay:?}");
        let retry_at = Utc::now()
            + chrono::Duration::from_std(delay).unwrap_or_else(|_| chrono::Duration::seconds(60));
        set(
            &state,
            StreamState::Backoff {
                attempt,
                retry_at,
                error: format!("{err:#}"),
            },
        );
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop.changed() => break,
        }
    }
    set(&state, StreamState::Stopped);
}

/// One connection, from seeding to the stream's end.
async fn consume(
    shared: &Shared,
    (agent, committee): (&SenatorPath, &CommitteeId),
    state: &Mutex<StreamState>,
    dirty: &Notify,
    stop: &mut watch::Receiver<bool>,
    resyncs: &mut watch::Receiver<u64>,
    attempt: &mut u32,
) -> Ended {
    let generation = *resyncs.borrow_and_update();
    let ended = |intake: Intake| match intake {
        Intake::Resync(r) => Ended::Resync(r, generation),
        Intake::Failed(e) => Ended::Lost(e),
    };
    let seeded = tokio::select! {
        r = shared.ensure_seeded(agent, committee, generation) => r,
        _ = stop.changed() => return Ended::Stop,
    };
    if let Err(e) = seeded {
        return ended(e);
    }
    let cursor = match shared.store.delivered_cursor(agent, committee) {
        Ok(c) => c,
        Err(e) => return Ended::Lost(e.into()),
    };
    let opened = tokio::select! {
        r = shared.api.open_stream(committee, cursor, agent) => r,
        _ = stop.changed() => return Ended::Stop,
    };
    let response = match opened {
        Ok(r) => r,
        Err(e) => return ended(shared.api_failure(e)),
    };
    if let Err(e) = shared.check_epoch(client::epoch_of(response.headers()).as_deref()) {
        return ended(e);
    }
    *attempt = 0;
    // Whose work is mine, as of now; the stream's presence events keep it.
    shared.seed_covering(agent, committee).await;
    set(state, StreamState::Connected { since: Utc::now() });
    tracing::info!(%agent, %committee, after = %cursor, "stream open");
    let mut messages = response.bytes_stream().eventsource();
    let stall = shared.timing.stall_timeout;
    loop {
        let next = tokio::select! {
            n = tokio::time::timeout(stall, messages.next()) => n,
            _ = stop.changed() => return Ended::Stop,
            Ok(()) = resyncs.changed() => {
                return Ended::Lost(anyhow!("the log was resynced; reconnecting"));
            }
        };
        let message = match next {
            Err(_) => return Ended::Lost(anyhow!("no data for {stall:?} (keepalive missed)")),
            Ok(None) => return Ended::Lost(anyhow!("the server closed the stream")),
            Ok(Some(Err(e))) => return Ended::Lost(anyhow!("reading the stream: {e}")),
            Ok(Some(Ok(m))) => m,
        };
        if message.event != api::SSE_EVENT {
            continue; // `ping` and anything newer only prove liveness.
        }
        let incoming = match serde_json::from_str::<Event>(&message.data) {
            Ok(e) => Incoming::Event(Box::new(e)),
            Err(error) => {
                // The SSE id is the seq; the data may say so too.
                let seq = message.id.trim().parse().ok().map(Seq).or_else(|| {
                    serde_json::from_str::<serde_json::Value>(&message.data)
                        .ok()
                        .as_ref()
                        .and_then(client::seq_of)
                });
                let Some(seq) = seq else {
                    tracing::warn!(%agent, %committee, "skipped an undecodable stream event with no seq: {error}");
                    continue;
                };
                Incoming::Undecodable {
                    seq,
                    error: error.to_string(),
                }
            }
        };
        if let Err(e) = shared
            .take_in(agent, committee, incoming, generation, dirty)
            .await
        {
            return ended(e);
        }
    }
}

async fn run_reporter(
    shared: Arc<Shared>,
    agent: SenatorPath,
    committee: CommitteeId,
    dirty: Arc<Notify>,
    mut stop: watch::Receiver<bool>,
) {
    let mut next_beat = Instant::now();
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next_beat) => {
                shared.report_cursor(&agent, &committee).await;
                shared.heartbeat(&agent, &committee).await;
                next_beat = Instant::now() + shared.timing.heartbeat;
            }
            _ = dirty.notified() => {
                tokio::select! {
                    _ = tokio::time::sleep(shared.timing.cursor_debounce) => {}
                    _ = stop.changed() => break,
                }
                shared.report_cursor(&agent, &committee).await;
            }
            _ = stop.changed() => break,
        }
    }
    // Leave the server with the latest cursor.
    if tokio::time::timeout(
        Duration::from_secs(2),
        shared.report_cursor(&agent, &committee),
    )
    .await
    .is_err()
    {
        tracing::debug!(%agent, %committee, "final cursor report timed out");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let t = Timing {
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            ..Timing::default()
        };
        let d1 = backoff(&t, 1);
        assert!(d1 >= Duration::from_millis(800) && d1 <= Duration::from_millis(1200));
        let d3 = backoff(&t, 3);
        assert!(d3 >= Duration::from_millis(3200) && d3 <= Duration::from_millis(4800));
        let big = backoff(&t, 40);
        assert!(big <= Duration::from_secs(36));
    }
}
