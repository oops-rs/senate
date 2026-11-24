use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::grants::lock;

const MAX_AUDIT_BYTES: u64 = 5 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

#[derive(Default)]
struct Denials {
    count: u64,
    timer_armed: bool,
}

pub(crate) struct Audit {
    path: PathBuf,
    denied: Mutex<Denials>,
}

impl Audit {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            denied: Mutex::default(),
        }
    }

    pub(crate) fn denied(self: &Arc<Self>) {
        let mut state = lock(&self.denied);
        state.count = state.count.saturating_add(1);
        if !state.timer_armed
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            state.timer_armed = true;
            let audit = Arc::downgrade(self);
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            runtime.spawn(async move {
                tokio::time::sleep_until(deadline).await;
                if let Some(audit) = audit.upgrade() {
                    let mut state = lock(&audit.denied);
                    state.timer_armed = false;
                    audit.flush(&mut state);
                }
            });
        }
    }

    pub(crate) fn flush_denied(&self) {
        self.flush(&mut lock(&self.denied));
    }

    fn flush(&self, state: &mut Denials) {
        let count = std::mem::take(&mut state.count);
        if count != 0 {
            self.write(
                serde_json::json!({"at": chrono::Utc::now(), "decision": "denied", "count": count}),
            );
        }
    }

    pub(crate) fn record(&self, route: &str, decision: &str, id: Option<&str>) {
        let mut denied = lock(&self.denied);
        self.flush(&mut denied);
        self.write(serde_json::json!({"at": chrono::Utc::now(), "route": route, "decision": decision, "grant_id": id}));
    }

    pub(crate) fn stream_closed(&self, id: &str, frames: u64, bytes: u64) {
        let mut denied = lock(&self.denied);
        self.flush(&mut denied);
        self.write(serde_json::json!({
            "at": chrono::Utc::now(), "route": "events", "decision": "closed", "grant_id": id,
            "frames": frames, "bytes": bytes,
        }));
    }

    fn write(&self, entry: serde_json::Value) {
        let old = self.path.with_extension("previous.jsonl");
        for path in [&old, &self.path] {
            // Use the creation time where available so continuous writes do
            // not keep the oldest rotation indefinitely.
            let expired = fs::metadata(path)
                .ok()
                .and_then(|meta| meta.created().or_else(|_| meta.modified()).ok())
                .and_then(|at| SystemTime::now().duration_since(at).ok())
                .is_some_and(|age| age >= MAX_AGE);
            if expired {
                let _ = fs::remove_file(path);
            }
        }
        let line = format!("{entry}\n");
        if fs::metadata(&self.path)
            .is_ok_and(|meta| meta.len() + line.len() as u64 >= MAX_AUDIT_BYTES)
            && fs::rename(&self.path, old).is_err()
        {
            return;
        }
        if let Ok(mut file) = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&self.path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

impl Drop for Audit {
    fn drop(&mut self) {
        self.flush_denied();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_denial_burst_is_recorded_without_more_traffic()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("audit.jsonl");
        let audit = Arc::new(Audit::new(path.clone()));
        for _ in 0..10_000 {
            audit.denied();
        }
        assert!(!path.exists());
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let text = fs::read_to_string(path)?;
        assert!(text.contains("\"count\":10000"));
        assert_eq!(text.lines().count(), 1);
        Ok(())
    }

    #[test]
    fn drop_flushes_the_last_burst() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("audit.jsonl");
        let audit = Arc::new(Audit::new(path.clone()));
        audit.denied();
        drop(audit);
        assert!(fs::read_to_string(path)?.contains("\"count\":1"));
        Ok(())
    }
}
