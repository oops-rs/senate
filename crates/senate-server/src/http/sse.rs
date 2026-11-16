//! The SSE stream: replay, then live, with no gap and no duplicate.
//!
//! The handler registers the live stream *before* this stream reads the
//! backlog, so every event is either in the backlog read or arrives live (or
//! both). The stream tracks the highest seq it has sent and skips any live
//! event at or below it. Should a live event ever arrive ahead of the next
//! expected seq, the stream goes back to the store for the missing range
//! instead of skipping it.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::response::sse;
use chrono::{SecondsFormat, Utc};
use futures::Stream;
use senate_core::api::{MAX_PAGE, SSE_EVENT, SSE_PING};
use senate_core::{CommitteeId, Event, Hub, Seq};
use tokio::time::{Instant, Interval, MissedTickBehavior};

use crate::hub::{LiveStream, ServerHub};

struct Feed {
    hub: Arc<ServerHub>,
    committee: CommitteeId,
    live: LiveStream,
    /// Highest seq sent, or the client's starting point.
    sent: Seq,
    backlog: VecDeque<Event>,
    /// Still reading stored events before going live.
    replaying: bool,
    ping: Interval,
}

fn event_frame(event: &Event) -> Option<sse::Event> {
    match serde_json::to_string(event) {
        Ok(json) => Some(
            sse::Event::default()
                .event(SSE_EVENT)
                .id(event.seq.to_string())
                .data(json),
        ),
        Err(error) => {
            tracing::error!(%error, seq = %event.seq, "event does not serialize; closing stream");
            None
        }
    }
}

fn ping_frame() -> sse::Event {
    sse::Event::default()
        .event(SSE_PING)
        .data(Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true))
}

impl Feed {
    /// The next frame, or `None` to end the response (the client reconnects
    /// with `Last-Event-ID`).
    async fn next(&mut self) -> Option<sse::Event> {
        loop {
            if let Some(event) = self.backlog.pop_front() {
                if event.seq <= self.sent {
                    continue;
                }
                self.sent = event.seq;
                return event_frame(&event);
            }
            if self.replaying {
                match self.hub.read(&self.committee, self.sent, MAX_PAGE).await {
                    Ok(page) => {
                        self.replaying = page.len() == MAX_PAGE;
                        self.backlog.extend(page);
                    }
                    Err(error) => {
                        tracing::warn!(committee = %self.committee, %error, "replay failed; closing stream");
                        return None;
                    }
                }
                continue;
            }
            tokio::select! {
                biased;
                received = self.live.events.recv() => {
                    let event = received?;
                    if event.seq <= self.sent {
                        continue;
                    }
                    if event.seq != self.sent.next() {
                        // Missed something: fetch it (and this event) from the store.
                        self.replaying = true;
                        continue;
                    }
                    self.sent = event.seq;
                    return event_frame(&event);
                }
                _ = self.ping.tick() => return Some(ping_frame()),
            }
        }
    }
}

/// Every stored event with `seq > after`, then live commits, with a ping
/// every `ping_every`.
pub(crate) fn feed(
    hub: Arc<ServerHub>,
    committee: CommitteeId,
    live: LiveStream,
    after: Seq,
    ping_every: Duration,
) -> impl Stream<Item = Result<sse::Event, Infallible>> + Send + 'static {
    let mut ping = tokio::time::interval_at(Instant::now() + ping_every, ping_every);
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let feed = Feed {
        hub,
        committee,
        live,
        sent: after,
        backlog: VecDeque::new(),
        replaying: true,
        ping,
    };
    futures::stream::unfold(feed, |mut feed| async move {
        let frame = feed.next().await?;
        Some((Ok(frame), feed))
    })
}
