//! History is available to the local person, without changing the session list.

use super::*;
use crate::ipc::{
    SessionHistoryReply, SessionHistoryRow, SessionIndexProgress, SessionSearchReply,
};
use crate::timeline::{self, Index};
use serde::{Deserialize, Serialize};

struct Status {
    progress: Option<SessionIndexProgress>,
    error: Option<String>,
}

pub(super) struct Background {
    status: Mutex<Status>,
    wake: tokio::sync::Notify,
    ready: watch::Sender<u64>,
    reader: Arc<tokio::sync::Mutex<()>>,
}

impl Default for Background {
    fn default() -> Self {
        Self {
            status: Mutex::new(Status {
                progress: Some(SessionIndexProgress {
                    indexed: 0,
                    total: 0,
                }),
                error: None,
            }),
            wake: tokio::sync::Notify::new(),
            ready: watch::channel(0).0,
            reader: Arc::default(),
        }
    }
}

impl Background {
    pub(super) fn progress(&self) -> Option<SessionIndexProgress> {
        lock(&self.status).progress
    }
}

pub(super) async fn warm(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let mut ticks = tokio::time::interval(Duration::from_secs(5));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = ticks.tick() => {}
            _ = shared.history_background.wake.notified() => {}
        }
        let build = shared.clone();
        let stop = shutdown.clone();
        let (done, result) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("senate-history".into())
            .spawn(move || {
                lower_index_priority();
                let _ = done.send(refresh_index(&build, &stop));
            });
        let error = match worker {
            Ok(_) => match result.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            },
            Err(error) => Some(error.to_string()),
        };
        {
            let mut status = lock(&shared.history_background.status);
            if status.error != error
                && let Some(error) = &error
            {
                tracing::warn!(%error, "refreshing the private conversation index");
            }
            *status = Status {
                progress: None,
                error,
            };
        }
        shared
            .history_background
            .ready
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        if *shutdown.borrow() {
            break;
        }
    }
}

#[allow(unsafe_code)]
fn lower_index_priority() {
    // SAFETY: These selectors affect only the dedicated current thread on
    // their respective platforms. Neither call reads any caller memory.
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(
                class: libc::c_uint,
                relative_priority: libc::c_int,
            ) -> libc::c_int;
        }
        const QOS_CLASS_UTILITY: libc::c_uint = 0x11;
        pthread_set_qos_class_self_np(QOS_CLASS_UTILITY, 0);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

fn refresh_index(shared: &Shared, shutdown: &watch::Receiver<bool>) -> anyhow::Result<()> {
    let mut sessions = shared.store.all_sessions()?;
    sessions.retain(|session| !shared.hosts.unpublished(&session.session_id));
    if sessions.is_empty() && !Index::exists(&shared.home) {
        return Ok(());
    }
    sessions
        .sort_unstable_by(|a, b| (b.last_seen, &b.session_id).cmp(&(a.last_seen, &a.session_id)));
    let total = sessions.len();
    lock(&shared.history_background.status).progress =
        Some(SessionIndexProgress { indexed: 0, total });
    let mut index = Index::open(&shared.home)?;
    {
        let _guard = lock(&shared.history_index);
        index.retain(&sessions.iter().map(|s| s.session_id.as_str()).collect())?;
    }
    for (position, old) in sessions.into_iter().enumerate() {
        if *shutdown.borrow() {
            break;
        }
        loop {
            let (changed, more) = {
                let _guard = lock(&shared.history_index);
                // Recheck the live store under the same lock used by forget.
                match shared.store.session(&old.session_id)? {
                    Some(session) => index.refresh_one(&session)?,
                    None => (false, false),
                }
            };
            if changed || more {
                std::thread::sleep(Duration::from_millis(4));
            }
            if !more || *shutdown.borrow() {
                break;
            }
        }
        lock(&shared.history_background.status).progress = Some(SessionIndexProgress {
            indexed: position + 1,
            total,
        });
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Before {
    last: chrono::DateTime<chrono::Utc>,
    session_id: String,
    committee: Option<CommitteeId>,
}

impl Shared {
    // Preserve the name seen on an explicit end hook, or when History first
    // encounters a legacy row. Older alias changes cannot be reconstructed.
    pub(super) fn remember_history_name(&self, session: &Session) -> Result<(), StoreError> {
        let cache = lock(&self.display_aliases);
        let aliases = cache.as_ref().map(|(_, aliases)| aliases);
        let committee = session.committee();
        let alias = committee.and_then(|c| aliases?.alias_of(c, &session.agent).cloned());
        let shown = senate_core::alias::display_name(aliases, committee, &session.agent);
        self.store
            .remember_history_name(&session.session_id, alias.as_ref(), &shown)
    }

    fn history_sessions(&self) -> Result<Vec<(Session, Option<EventId>, bool)>, IpcError> {
        let paths = Paths::new(&self.home);
        let running: BTreeSet<_> = crate::host::running_ids(&paths)
            .map_err(IpcError::internal)?
            .into_iter()
            .collect();
        let hosted: HashMap<_, _> = self
            .store
            .hosted_sessions()
            .map_err(internal)?
            .into_iter()
            .map(|(session, id)| (session.session_id, id))
            .collect();
        Ok(self
            .store
            .all_sessions()
            .map_err(internal)?
            .into_iter()
            .filter(|session| !self.hosts.unpublished(&session.session_id))
            .map(|session| {
                let host = hosted.get(&session.session_id).cloned();
                let live = host
                    .as_ref()
                    .map_or_else(|| session.is_live(), |host| running.contains(host));
                (session, host, live)
            })
            .collect())
    }

    pub(super) async fn on_history(
        self: &Arc<Self>,
        committee: Option<CommitteeId>,
        before: Option<String>,
        limit: Option<usize>,
    ) -> Result<SessionHistoryReply, IpcError> {
        let before = before
            .map(|before| serde_json::from_str::<Before>(&before))
            .transpose()
            .map_err(|_| IpcError::bad_request("invalid session history cursor"))?;
        if before
            .as_ref()
            .is_some_and(|before| before.committee != committee)
        {
            return Err(IpcError::bad_request(
                "history cursor belongs to a different committee",
            ));
        }
        self.refresh_display_aliases().await;
        let shared = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut sessions = shared.history_sessions()?;
            sessions.retain(|(session, host, live)| {
                !live
                    && committee
                        .as_ref()
                        .is_none_or(|committee| session.committee() == Some(committee))
                    && before.as_ref().is_none_or(|before| {
                        (session.last_seen, &session.session_id) < (before.last, &before.session_id)
                    })
                    && (host.is_some()
                        || timeline::path(session).is_some_and(|path| {
                            std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
                        }))
            });
            sessions.sort_unstable_by(|(a, _, _), (b, _, _)| {
                (b.last_seen, &b.session_id).cmp(&(a.last_seen, &a.session_id))
            });
            let limit = limit.unwrap_or(60).clamp(1, 300);
            let more = sessions.len() > limit;
            sessions.truncate(limit);
            let next_before = if more {
                sessions
                    .last()
                    .map(|(session, _, _)| {
                        serde_json::to_string(&Before {
                            last: session.last_seen,
                            session_id: session.session_id.clone(),
                            committee,
                        })
                    })
                    .transpose()
                    .map_err(IpcError::internal)?
            } else {
                None
            };
            let mut rows = Vec::with_capacity(sessions.len());
            for (session, host, _) in sessions {
                let metadata = timeline::path(&session)
                    .and_then(|path| lock(&shared.history_titles).metadata(&path, session.runtime));
                // Availability of a native resume is a local indication. The
                // runtime can still reject credentials, directory, or thread.
                let readable = metadata.is_some();
                let restorable = host.is_some() && readable && session.committee().is_some();
                if shared
                    .store
                    .history_name(&session.session_id)
                    .map_err(internal)?
                    .is_none()
                {
                    shared.remember_history_name(&session).map_err(internal)?;
                }
                let (alias, shown_name) = shared
                    .store
                    .history_name(&session.session_id)
                    .map_err(internal)?
                    .unwrap_or((None, senate_core::alias::short_name(&session.agent)));
                let model = shared
                    .store
                    .hosted_model(&session.session_id)
                    .map_err(internal)?;
                let title = metadata.and_then(|meta| meta.title);
                rows.push(SessionHistoryRow {
                    session_id: session.session_id.clone(),
                    primary: session.agent.clone(),
                    alias,
                    shown_name: Some(shown_name),
                    committee: session.committee().cloned(),
                    runtime: session.runtime,
                    model,
                    dir: session.cwd,
                    branch: session.branch,
                    title,
                    started_at: session.started_at,
                    last_activity: session.last_seen,
                    readable,
                    restorable,
                    item_count: None,
                });
            }
            Ok(SessionHistoryReply { rows, next_before })
        })
        .await
        .map_err(IpcError::internal)?
    }

    pub(super) async fn on_history_search(
        self: &Arc<Self>,
        query: String,
        committee: Option<CommitteeId>,
        limit: Option<usize>,
        cursor: Option<String>,
    ) -> Result<SessionSearchReply, IpcError> {
        if query.trim().is_empty()
            || query.len() > 512
            || query.contains('\0')
            || cursor.as_ref().is_some_and(|cursor| cursor.len() > 512)
        {
            return Err(IpcError::bad_request(
                "invalid session search query or cursor",
            ));
        }
        if cursor.is_none() && lock(&self.history_background.status).progress.is_none() {
            let mut ready = self.history_background.ready.subscribe();
            self.history_background.wake.notify_one();
            // Complete tiny updates immediately when possible. Large builds
            // keep running and callers get the committed partial results.
            let _ = tokio::time::timeout(Duration::from_millis(100), ready.changed()).await;
        }
        let reader = self.history_background.reader.clone().lock_owned().await;
        let shared = self.clone();
        tokio::task::spawn_blocking(move || {
            let _reader = reader;
            let progress_before = {
                let status = lock(&shared.history_background.status);
                if let Some(error) = &status.error {
                    return Err(IpcError::internal(error));
                }
                status.progress
            };
            let sessions = shared.history_sessions()?;
            if !Index::exists(&shared.home) {
                // The index may have been removed during the preceding pass.
                // Ask for a fresh pass against the new path, rather than
                // declaring an empty index ready while sources remain.
                let total = sessions
                    .iter()
                    .filter(|(session, _, _)| {
                        timeline::path(session).is_some_and(|path| {
                            std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
                        })
                    })
                    .count();
                if total != 0 {
                    shared.history_background.wake.notify_one();
                }
                let progress = progress_before
                    .or((total != 0).then_some(SessionIndexProgress { indexed: 0, total }));
                return Ok(SessionSearchReply {
                    hits: Vec::new(),
                    next_cursor: None,
                    indexing: progress.is_some(),
                    progress,
                });
            }
            let visible = sessions
                .iter()
                .filter(|(session, _, _)| {
                    committee
                        .as_ref()
                        .is_none_or(|committee| session.committee() == Some(committee))
                })
                .map(|(session, _, live)| (session.session_id.as_str(), *live))
                .collect();
            let result = Index::open_read(&shared.home).and_then(|index| {
                index.search(
                    &query,
                    committee.as_ref().map(CommitteeId::as_ref),
                    cursor.as_deref(),
                    limit.unwrap_or(40).clamp(1, 100),
                    &visible,
                )
            });
            let mut reply = match result {
                Ok(reply) => reply,
                // A reader can see the new database file before the initial
                // schema transaction commits. The worker reports persistent
                // errors once the pass ends.
                Err(_) if progress_before.is_some() && cursor.is_none() => SessionSearchReply {
                    hits: Vec::new(),
                    next_cursor: None,
                    indexing: true,
                    progress: progress_before,
                },
                Err(error) if cursor.is_some() => {
                    return Err(IpcError::bad_request(error.to_string()));
                }
                Err(error) => return Err(IpcError::internal(error)),
            };
            let progress = progress_before.or(lock(&shared.history_background.status).progress);
            if progress.is_some() {
                reply.indexing = true;
                reply.progress = progress;
                reply.next_cursor = None;
            }
            Ok(reply)
        })
        .await
        .map_err(IpcError::internal)?
    }

    pub(super) async fn on_history_forget(self: &Arc<Self>, id: String) -> Result<(), IpcError> {
        let _binding = self.session_bindings.write().await;
        let shared = self.clone();
        tokio::task::spawn_blocking(move || {
            let _guard = shared.history_index.try_lock().map_err(|_| {
                IpcError::bad_request(
                    "the search index is updating; try forgetting the session again",
                )
            })?;
            let paths = Paths::new(&shared.home);
            let session = shared.timeline_session(&id)?;
            let host = shared.store.session_host(&id).map_err(internal)?;
            let running = match host.as_ref() {
                Some(host) => crate::host::running(&paths, host).map_err(IpcError::internal)?,
                None => session.is_live(),
            };
            if running
                || shared
                    .store
                    .paused(&session.agent)
                    .map_err(internal)?
                    .is_some()
            {
                return Err(IpcError::bad_request(
                    "stop the session before forgetting it",
                ));
            }
            if Index::exists(&shared.home) {
                Index::open(&shared.home)
                    .and_then(|mut index| index.remove(std::slice::from_ref(&id)))
                    .map_err(IpcError::internal)?;
            }
            if shared
                .store
                .delete_sessions(std::slice::from_ref(&id), &paths)
                .map_err(internal)?
                == 0
            {
                return Err(IpcError::bad_request("the session is no longer stopped"));
            }
            shared.no_longer_working(&id);
            shared.publish(WatchEvent::Sessions);
            Ok(())
        })
        .await
        .map_err(IpcError::internal)?
    }
}
