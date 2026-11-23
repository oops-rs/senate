//! Bounded best-effort diagnostics. Disk failures cannot authorize a request
//! or prevent a revoke. Only admitted actions can enter this queue.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, SyncSender};

use serde_json::{Value, json};

use super::Device;

const FILE_LIMIT: u64 = 1024 * 1024;
const QUEUE_LIMIT: usize = 128;

pub(super) struct Audit {
    sender: SyncSender<Value>,
    queue_warning: AtomicBool,
    next_rejection: AtomicI64,
}

impl Audit {
    pub(super) fn open(dir: &Path) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_LIMIT);
        let path = dir.join("audit.jsonl");
        std::thread::Builder::new()
            .name("senate-remote-audit".into())
            .spawn(move || {
                let mut warned = false;
                for record in receiver {
                    match append(&path, &record, FILE_LIMIT) {
                        Ok(()) => warned = false,
                        Err(error) if !warned => {
                            tracing::warn!(kind = ?error.kind(), "remote audit unavailable");
                            warned = true;
                        }
                        Err(_) => {}
                    }
                }
            })?;
        Ok(Self {
            sender,
            queue_warning: AtomicBool::new(false),
            next_rejection: AtomicI64::new(0),
        })
    }

    pub(super) fn record(
        &self,
        device: &Device,
        action: &'static str,
        peer: Option<SocketAddr>,
        session: Option<&str>,
    ) {
        let session = session.filter(|value| value.len() <= 128);
        let entry = json!({
            "at": chrono::Utc::now(), "device": device.id, "name": device.name,
            "scopes": device.scopes, "expires": device.expires,
            "action": action, "peer": peer, "session": session,
        });
        self.enqueue(entry);
    }

    pub(super) fn rejected(&self, peer: SocketAddr) {
        let now = chrono::Utc::now().timestamp();
        let next = self.next_rejection.load(Ordering::Relaxed);
        if now >= next
            && self
                .next_rejection
                .compare_exchange(next, now + 30, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.enqueue(
                json!({ "at": chrono::Utc::now(), "action": "handshake_rejected", "peer": peer }),
            );
        }
    }

    fn enqueue(&self, entry: Value) {
        if self.sender.try_send(entry).is_ok() {
            self.queue_warning.store(false, Ordering::Relaxed);
        } else if !self.queue_warning.swap(true, Ordering::Relaxed) {
            tracing::warn!("remote audit queue is full or stopped");
        }
    }
}

fn open(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "remote audit must be a private file",
        ));
    }
    Ok(file)
}

fn append(path: &Path, record: &Value, limit: u64) -> io::Result<()> {
    let mut line = serde_json::to_vec(record)?;
    line.push(b'\n');
    if line.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "oversized audit record",
        ));
    }
    let mut file = open(path)?;
    if file.metadata()?.len().saturating_add(line.len() as u64) > limit {
        fs::rename(path, path.with_extension("jsonl.1"))?;
        file = open(path)?;
    }
    file.write_all(&line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unauthenticated_operational_refusals_are_sampled() -> anyhow::Result<()> {
        let (sender, receiver) = mpsc::sync_channel(2);
        let audit = Audit {
            sender,
            queue_warning: AtomicBool::new(false),
            next_rejection: AtomicI64::new(0),
        };
        let peer = "127.0.0.1:4000".parse()?;
        for _ in 0..128 {
            audit.rejected(peer);
        }
        let records: Vec<_> = receiver.try_iter().collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["action"], "handshake_rejected");
        Ok(())
    }

    #[test]
    fn audit_retains_at_most_two_bounded_files() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("audit.jsonl");
        for n in 0..20 {
            append(&path, &json!({"action": "test", "n": n}), 100)?;
        }
        let files = fs::read_dir(dir.path())?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(files.len(), 2);
        for file in files {
            assert!(file.metadata()?.len() <= 100);
        }
        assert!(fs::read_to_string(path)?.contains("\"n\":19"));
        Ok(())
    }
}
