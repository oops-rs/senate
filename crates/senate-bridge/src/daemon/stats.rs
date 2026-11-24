use super::*;
use crate::stats::{self, Range, StatsReply};
use crate::timeline::Index;

#[derive(Default)]
pub(super) struct Background {
    requested: Mutex<Vec<Range>>,
    wake: Notify,
    logs_current: AtomicBool,
}

impl Shared {
    pub(super) async fn on_stats(
        &self,
        from: chrono::NaiveDate,
        to: chrono::NaiveDate,
        zone: &str,
    ) -> Result<StatsReply, IpcError> {
        let range = Range::new(from, to, zone)?;
        {
            let mut requested = lock(&self.stats.requested);
            if !requested.contains(&range) {
                if requested.len() == 4 {
                    requested.remove(0);
                }
                requested.push(range);
            }
        }
        self.stats.wake.notify_one();
        let home = self.home.clone();
        let progress = self.history_background.progress();
        let logs_current = self.stats.logs_current.load(Ordering::Acquire);
        tokio::task::spawn_blocking(move || {
            let index = Index::open_read(&home).ok();
            stats::report(index.as_ref().map(|i| &i.0), range, progress, logs_current)
        })
        .await
        .map_err(IpcError::internal)?
        .map_err(IpcError::internal)
    }
}

pub(super) async fn warm(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let mut ticks = tokio::time::interval(Duration::from_secs(15));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut active = Vec::new();
    let mut synced_at: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            _ = ticks.tick() => {},
            _ = shared.stats.wake.notified() => {},
        }
        for request in std::mem::take(&mut *lock(&shared.stats.requested)) {
            if !active.contains(&request) {
                if active.len() == 3 {
                    active.remove(0);
                }
                active.push(request);
            }
        }
        if active.is_empty() {
            continue;
        }
        // Reads of a warm range do not trigger a network poll every time.
        if synced_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(10)) {
            let synced = tokio::select! {
                _ = shutdown.changed() => return,
                result = tokio::time::timeout(Duration::from_secs(30),sync_logs(&shared)) => result,
            };
            synced_at = Some(Instant::now());
            shared
                .stats
                .logs_current
                .store(matches!(synced, Ok(Ok(true))), Ordering::Release);
        }
        let home = shared.home.clone();
        let ranges = active.clone();
        let stop = shutdown.clone();
        let ready = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let mut index = Index::open(&home)?;
            for range in ranges {
                stats::warm_days(&mut index.0, range, || *stop.borrow())?;
            }
            Ok(())
        })
        .await;
        if let Ok(Err(error)) = ready {
            tracing::debug!(%error,"refreshing local daily statistics");
        }
    }
}

async fn sync_logs(shared: &Shared) -> anyhow::Result<bool> {
    let me = shared.api.me().await?;
    anyhow::ensure!(
        me.person == shared.config.person,
        "statistics server identity changed"
    );
    let mut committees = me.committees.clone();
    committees.extend(me.archived);
    committees.sort_unstable();
    committees.dedup();
    let person = shared.person();
    let local: BTreeSet<_> = shared
        .store
        .all_sessions()?
        .into_iter()
        .map(|s| s.agent)
        .collect();
    let mut complete = true;
    for committee in committees {
        let home = shared.home.clone();
        let name = committee.to_string();
        let epoch = me.epoch.clone();
        let mut cursor = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            stats::log_progress(&Index::open(&home)?.0, &name, &epoch)
        })
        .await??;
        let mut more = true;
        for _ in 0..4 {
            let page = shared.api.events(&committee, cursor, Some(200)).await?;
            let page_epoch = page.epoch.unwrap_or_else(|| me.epoch.clone());
            anyhow::ensure!(page_epoch == me.epoch, "statistics log epoch changed");
            more = page.has_more;
            let home = shared.home.clone();
            let name = committee.to_string();
            let person = person.clone();
            let local = local.clone();
            cursor = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                stats::log_page(
                    &mut Index::open(&home)?.0,
                    &name,
                    &page_epoch,
                    cursor,
                    page.events,
                    &person,
                    &local,
                )
            })
            .await??;
            if !more {
                break;
            }
        }
        complete &= !more;
    }
    Ok(complete)
}
