//! Committee log previews for the local app. The server remains the source of
//! truth for permissions and for replay. A watcher attaches to the existing
//! intake bus before reading a head, so a write racing replay is seen either
//! in the replay or on the bus (and deduplicated by committee and seq).

use std::sync::Weak;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast;

use super::*;
use crate::ipc::{ActivityBatch, ActivityEntry, CommitteeThread, CommitteeThreadPage, StreamState};

const BACKLOG: u64 = 4096;
const BATCH_SIZE: usize = 32;
const COALESCE: Duration = Duration::from_millis(40);
const FOLLOW_RETRY: Duration = Duration::from_millis(250);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const LIFECYCLE: &str = "hosted_lifecycle";

pub(super) struct ActivityHub {
    live: broadcast::Sender<ObservedActivity>,
    feeds: Mutex<BTreeMap<CommitteeId, Weak<ReadFeed>>>,
    reset: watch::Sender<u64>,
}

#[derive(Clone)]
struct ObservedActivity {
    entry: Arc<ActivityEntry>,
    seated: bool,
}

struct ReadFeed(tokio::task::JoinHandle<()>);

impl Drop for ReadFeed {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Default for ActivityHub {
    fn default() -> Self {
        Self {
            live: broadcast::channel(256).0,
            feeds: Mutex::new(BTreeMap::new()),
            reset: watch::channel(0).0,
        }
    }
}

impl ActivityHub {
    pub(super) fn publish(&self, event: &Event, seated: bool) {
        if self.live.receiver_count() > 0 {
            let _ = self.live.send(ObservedActivity {
                entry: Arc::new(preview(event)),
                seated,
            });
        }
    }

    fn follow(shared: &Arc<Shared>, committee: CommitteeId, head: Seq) -> Arc<ReadFeed> {
        let mut feeds = lock(&shared.activity.feeds);
        feeds.retain(|_, feed| feed.strong_count() > 0);
        if let Some(feed) = feeds.get(&committee).and_then(Weak::upgrade) {
            return feed;
        }
        let feed = Arc::new(ReadFeed(tokio::spawn(follow(
            shared.clone(),
            committee.clone(),
            head,
        ))));
        feeds.insert(committee, Arc::downgrade(&feed));
        feed
    }
}

impl Shared {
    pub(super) async fn on_committee_threads(
        &self,
        committee: CommitteeId,
        include_archived: bool,
        limit: Option<usize>,
        cursor: Option<String>,
    ) -> Result<CommitteeThreadPage, IpcError> {
        let me = self.api.me().await.map_err(server_error)?;
        if !me.committees.contains(&committee)
            && !(include_archived && me.archived.contains(&committee))
        {
            return Err(IpcError::bad_request("committee is not available"));
        }
        let before = cursor
            .map(|cursor| {
                cursor
                    .parse::<u64>()
                    .ok()
                    .filter(|seq| *seq > 0)
                    .map(Seq)
                    .ok_or_else(|| IpcError::bad_request("invalid committee thread cursor"))
            })
            .transpose()?;
        let limit = limit.unwrap_or(40).clamp(1, 100);
        let page = self
            .api
            .thread_page(
                &committee,
                api::ThreadsQuery {
                    before,
                    limit: Some(limit + 1),
                    include_discussions: true,
                    include_archived,
                    ..Default::default()
                },
            )
            .await
            .map_err(server_error)?;
        let mut rows: Vec<_> = page
            .threads
            .into_iter()
            .filter(|t| include_archived || t.state != senate_core::StatusState::Archived)
            .filter(|t| before.is_none_or(|before| t.updated < before))
            .collect();
        rows.sort_unstable_by_key(|t| std::cmp::Reverse(t.updated));
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = rows
            .last()
            .filter(|_| has_more)
            .map(|t| t.updated.to_string());
        Ok(CommitteeThreadPage {
            rows: rows
                .into_iter()
                .map(|t| thread_row(&committee, t))
                .collect(),
            next_cursor,
        })
    }
}

fn thread_row(committee: &CommitteeId, t: senate_core::task::ThreadSummary) -> CommitteeThread {
    CommitteeThread {
        committee: committee.clone(),
        root: t.root,
        title: t.title.chars().take(200).collect(),
        kind: t.kind.to_string(),
        opener: t
            .activity
            .as_ref()
            .and_then(|a| a.opener.clone())
            .unwrap_or_else(|| t.on_behalf_of.as_ref().unwrap_or(&t.from).to_string()),
        addressees: t
            .to
            .iter()
            .map(ToString::to_string)
            .chain(t.activity.as_ref().and_then(|a| a.addressee.clone()))
            .collect(),
        owner: t
            .owner
            .as_ref()
            .map(|o| Address::senator(&o.agent).to_string()),
        state: t.state.to_string(),
        waits_on: t.waits().map(|waits| match waits {
            senate_core::task::WaitsOn::Agents => "agents".into(),
            senate_core::task::WaitsOn::Person => "person".into(),
        }),
        escalated_to: t.escalated_to.as_ref().map(ToString::to_string),
        takeover: t.takeover.as_ref().map(ToString::to_string),
        last_seq: t.updated,
        last_at: t.activity.as_ref().map(|activity| activity.last_at),
        last_author: t
            .activity
            .as_ref()
            .map(|activity| activity.last_author.to_string()),
        message_count: t.activity.as_ref().map(|activity| activity.message_count),
        other_committee: t.activity.and_then(|activity| activity.other_committee),
    }
}

fn preview(event: &Event) -> ActivityEntry {
    use senate_core::{
        CommitteeSeatAction, CommitteeSeatChange, PresenceState, task::StatusPayload,
    };

    let mut from = event.from.to_string();
    let mut to = event.to.iter().map(ToString::to_string).collect::<Vec<_>>();
    if to.is_empty()
        && let Some(intended) = senate_core::locator::intended(event)
    {
        to.push(intended.to_string());
    }
    let mut kind = match event.kind {
        Kind::Other(_) => "unknown",
        _ => event.kind.as_str(),
    };
    let mut state = None;
    if let Some(membership) = CommitteeSeatChange::of(event) {
        from = Address::senator(&membership.senator).to_string();
        (kind, state) = match membership.action {
            CommitteeSeatAction::Join => ("joined", Some("unknown".into())),
            CommitteeSeatAction::Unseat => ("left", Some("away".into())),
        };
    } else if let Some(presence) = senate_core::continuity::PresencePayload::of(event) {
        from = Address::senator(&presence.agent).to_string();
        (kind, state) = if presence.paused {
            ("state", Some("paused".into()))
        } else if presence.state == PresenceState::Online {
            ("joined", Some("unknown".into()))
        } else {
            ("left", Some("away".into()))
        };
    } else if let Some(chair) = senate_core::ChairPayload::of(event) {
        kind = "chair_changed";
        to = vec![Address::senator(&chair.senator).to_string()];
    } else if let Some(status) = StatusPayload::of(event) {
        if status.escalated_to.is_some() {
            kind = "escalation";
        }
        state = Some(status.state.to_string());
    } else if event.kind == Kind::Note
        && let Some((lifecycle, senator)) = event
            .meta
            .get(LIFECYCLE)
            .and_then(|v| serde_json::from_value::<(String, SenatorName)>(v.clone()).ok())
    {
        let (new_kind, seat_state) = match lifecycle.as_str() {
            "started" => ("session_started", "unknown"),
            "stopped" => ("session_stopped", "stopped"),
            "paused" => ("state", "paused"),
            _ => ("note", "unknown"),
        };
        if new_kind != "note" {
            kind = new_kind;
            from = Address::senator(&senator).to_string();
            state = Some(seat_state.into());
        }
    }
    let thread = event.thread.or_else(|| {
        (event.is_chair_discussion() || matches!(event.kind, Kind::Question | Kind::Request))
            .then_some(event.seq)
    });
    ActivityEntry {
        committee: event.committee.clone(),
        seq: event.seq,
        at: event.ts,
        kind: kind.into(),
        from,
        to,
        on_behalf_of: event
            .is_chair_discussion()
            .then(|| senate_core::Origin::of(event))
            .flatten()
            .map(|origin| origin.locator.to_string())
            .or_else(|| event.on_behalf_of.as_ref().map(ToString::to_string)),
        thread,
        line: event
            .body
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(80)
            .collect(),
        state,
    }
}

async fn follow(shared: Arc<Shared>, committee: CommitteeId, mut cursor: Seq) {
    let mut bus = shared.activity.live.subscribe();
    loop {
        let connected = lock(&shared.streams).iter().any(|((_, c), worker)| {
            c == &committee && matches!(worker.state(), StreamState::Connected { .. })
        });
        if connected {
            // Remember how far the existing local feed got before opening
            // a passive stream. A lag here resumes from the older cursor.
            if let Ok(Ok(event)) = tokio::time::timeout(FOLLOW_RETRY, bus.recv()).await
                && event.entry.committee == committee
            {
                cursor = cursor.max(event.entry.seq);
            }
            continue;
        }
        let opened = tokio::time::timeout(
            shared.timing.stall_timeout,
            shared.api.open_event_stream(&committee, cursor, None),
        )
        .await;
        if let Ok(Err(error)) = &opened
            && (error.is_cursor_ahead() || matches!(error.status(), Some(401 | 403 | 404)))
        {
            shared
                .activity
                .reset
                .send_modify(|generation| *generation = generation.wrapping_add(1));
            return;
        }
        if let Ok(Ok(response)) = opened {
            if let Err(Intake::Resync(Resync::Epoch(new))) =
                shared.check_epoch(client::epoch_of(response.headers()).as_deref())
            {
                shared.resync_epoch(&new).await;
                return;
            }
            let mut messages = response.bytes_stream().eventsource();
            'passive: loop {
                let read = tokio::time::timeout(shared.timing.stall_timeout, messages.next());
                tokio::pin!(read);
                let message = loop {
                    tokio::select! {
                        message = &mut read => break message,
                        Ok(activity) = bus.recv() => {
                            // Hand back only when a seated stream has proved it has
                            // caught up. Simply seeing it connect could miss a commit
                            // made between the old stream's last frame and the handoff.
                            if activity.seated && activity.entry.committee == committee
                                && activity.entry.seq >= cursor
                            {
                                cursor = activity.entry.seq;
                                break 'passive;
                            }
                        }
                    }
                };
                let Ok(Some(Ok(message))) = message else {
                    break;
                };
                if message.event == api::SSE_EVENT
                    && let Ok(event) = serde_json::from_str::<Event>(&message.data)
                    && event.committee == committee
                {
                    cursor = cursor.max(event.seq);
                    shared.activity.publish(&event, false);
                }
            }
        }
        tokio::time::sleep(FOLLOW_RETRY).await;
    }
}

fn selected(
    readable: Vec<CommitteeId>,
    requested: Option<Vec<CommitteeId>>,
    after: &BTreeMap<CommitteeId, Seq>,
) -> Result<Vec<CommitteeId>, IpcError> {
    let mut committees = requested.unwrap_or_else(|| readable.clone());
    committees.sort_unstable();
    committees.dedup();
    if committees.iter().any(|c| !readable.contains(c))
        || after.keys().any(|c| !committees.contains(c))
    {
        return Err(IpcError::bad_request("committee is not available"));
    }
    Ok(committees)
}

pub(super) async fn serve(
    shared: &Arc<Shared>,
    committees: Option<Vec<CommitteeId>>,
    after: BTreeMap<CommitteeId, Seq>,
    mut reader: BufReader<OwnedReadHalf>,
    mut write: OwnedWriteHalf,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut epoch = shared.resyncs();
    let mut invalidated = shared.activity.reset.subscribe();
    tokio::select! {
        () = stream(shared, committees, after, &mut write) => {}
        _ = reader.fill_buf() => {}
        _ = shutdown.changed() => {}
        _ = epoch.changed() => { reset(&mut write).await; }
        _ = invalidated.changed() => { reset(&mut write).await; }
    }
}

async fn stream(
    shared: &Arc<Shared>,
    committees: Option<Vec<CommitteeId>>,
    after: BTreeMap<CommitteeId, Seq>,
    write: &mut OwnedWriteHalf,
) {
    let mut bus = shared.activity.live.subscribe();
    let committees = match shared.api.me().await.map_err(server_error).and_then(|me| {
        let mut readable = me.committees;
        if committees.is_some() {
            readable.extend(me.archived);
        }
        selected(readable, committees, &after)
    }) {
        Ok(committees) => committees,
        Err(error) => {
            send_ipc_line(write, &Reply::err(error)).await;
            return;
        }
    };
    let mut cursors = BTreeMap::new();
    let mut feeds = Vec::new();
    let mut heads = futures::stream::iter(committees)
        .map(|committee| async {
            let page = shared.api.presence(&committee).await;
            (committee, page)
        })
        .buffer_unordered(8);
    while let Some((committee, page)) = heads.next().await {
        let head = match page {
            Ok(page) => page.head,
            Err(error) => {
                send_ipc_line(write, &Reply::err(server_error(error))).await;
                return;
            }
        };
        feeds.push(ActivityHub::follow(shared, committee.clone(), head));
        let cursor = after.get(&committee).copied().unwrap_or(head);
        cursors.insert(committee, (cursor, head));
    }
    // An empty batch acknowledges a quiet connection.
    if !send_batch(
        write,
        ActivityBatch {
            events: vec![],
            cursors: cursors
                .iter()
                .map(|(c, (seq, _))| (c.clone(), *seq))
                .collect(),
            reset: false,
        },
    )
    .await
    {
        return;
    }
    for (committee, (cursor, head)) in &mut cursors {
        if !replay(shared, committee, cursor, *head, write).await {
            reset(write).await;
            return;
        }
    }
    loop {
        let first = bus.recv().await;
        let mut events = Vec::with_capacity(BATCH_SIZE);
        let mut lagged = false;
        let mut received = first;
        let deadline = tokio::time::Instant::now() + COALESCE;
        loop {
            match received {
                Ok(observed) => {
                    let event = observed.entry;
                    if let Some((cursor, _)) = cursors.get_mut(&event.committee)
                        && event.seq > *cursor
                    {
                        if event.seq.0 > cursor.0.saturating_add(1) {
                            if !flush(write, &mut events).await {
                                return;
                            }
                            if !replay(
                                shared,
                                &event.committee,
                                cursor,
                                Seq(event.seq.0 - 1),
                                write,
                            )
                            .await
                            {
                                reset(write).await;
                                return;
                            }
                        }
                        *cursor = event.seq;
                        events.push((*event).clone());
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    lagged = true;
                    break;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
            if events.len() == BATCH_SIZE {
                break;
            }
            match tokio::time::timeout_at(deadline, bus.recv()).await {
                Ok(event) => received = event,
                Err(_) => break,
            }
        }
        if !flush(write, &mut events).await {
            return;
        }
        if lagged {
            for (committee, (cursor, _)) in &mut cursors {
                let Ok(page) = shared.api.presence(committee).await else {
                    reset(write).await;
                    return;
                };
                if !replay(shared, committee, cursor, page.head, write).await {
                    reset(write).await;
                    return;
                }
            }
        }
    }
}

async fn replay(
    shared: &Shared,
    committee: &CommitteeId,
    cursor: &mut Seq,
    through: Seq,
    write: &mut OwnedWriteHalf,
) -> bool {
    if *cursor > through || through.0.saturating_sub(cursor.0) > BACKLOG {
        return false;
    }
    while *cursor < through {
        let Ok(page) = shared
            .api
            .events(committee, *cursor, Some(BATCH_SIZE))
            .await
        else {
            return false;
        };
        if shared.check_epoch(page.epoch.as_deref()).is_err() {
            return false;
        }
        let mut events = Vec::with_capacity(BATCH_SIZE);
        let previous = *cursor;
        for raw in page.events {
            let Ok(event) = serde_json::from_value::<Event>(raw) else {
                return false;
            };
            if event.seq > through {
                break;
            }
            if event.committee != *committee || event.seq.0 != cursor.0 + 1 {
                return false;
            }
            *cursor = event.seq;
            events.push(preview(&event));
        }
        if *cursor == previous || !flush(write, &mut events).await {
            return false;
        }
    }
    true
}

async fn reset(write: &mut OwnedWriteHalf) {
    send_batch(
        write,
        ActivityBatch {
            events: vec![],
            cursors: Default::default(),
            reset: true,
        },
    )
    .await;
}

async fn flush(write: &mut OwnedWriteHalf, events: &mut Vec<ActivityEntry>) -> bool {
    events.is_empty()
        || send_batch(
            write,
            ActivityBatch {
                events: std::mem::take(events),
                cursors: Default::default(),
                reset: false,
            },
        )
        .await
}

async fn send_batch(write: &mut OwnedWriteHalf, batch: ActivityBatch) -> bool {
    tokio::time::timeout(WRITE_TIMEOUT, send_ipc_line(write, &batch))
        .await
        .unwrap_or(false)
}
