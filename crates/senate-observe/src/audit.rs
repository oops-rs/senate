use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use crate::grants::lock;

const MAX_AUDIT_BYTES: u64 = 5 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

pub(crate) struct Audit {
    path: PathBuf,
    denied: Mutex<(Instant, u64)>,
}

impl Audit {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            denied: Mutex::new((Instant::now(), 0)),
        }
    }

    pub(crate) fn denied(&self) {
        let mut denied = lock(&self.denied);
        denied.1 = denied.1.saturating_add(1);
        if denied.0.elapsed() >= Duration::from_secs(60) {
            self.write(serde_json::json!({"at": chrono::Utc::now(), "decision": "denied", "count": denied.1}));
            *denied = (Instant::now(), 0);
        }
    }

    pub(crate) fn record(&self, route: &str, decision: &str, id: Option<&str>) {
        let _guard = lock(&self.denied);
        self.write(serde_json::json!({"at": chrono::Utc::now(), "route": route, "decision": decision, "grant_id": id}));
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
