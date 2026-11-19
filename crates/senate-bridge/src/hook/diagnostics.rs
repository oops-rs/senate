use std::fs::File;
use std::io::{self, Write as _};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use chrono::Utc;
use rustix::fs::{FlockOperation, Mode, OFlags, flock, open};

use super::HookKind;
use crate::ipc::{self, HookDropReport, Request};
use crate::paths::Paths;
use crate::session::Runtime;

pub const STDIN_BUDGET: Duration = Duration::from_secs(1);
pub const DROP_REPORT_BUDGET: Duration = Duration::from_millis(200);
const IPC_BUDGET: Duration = Duration::from_millis(100);
const LOG_CAP: u64 = 64 * 1024;

/// A timeout has no hook payload. Leave one diagnostic within a separate
/// budget, including if the daemon or the fallback file stalls.
pub fn report_stdin_timeout(kind: HookKind, runtime_hint: Option<Runtime>) {
    within_report_budget(move || report(kind, runtime_hint));
}

fn within_report_budget(work: impl FnOnce() + Send + 'static) {
    let began = Instant::now();
    let (done, finished) = mpsc::channel();
    if std::thread::Builder::new()
        .spawn(move || {
            work();
            let _ = done.send(());
        })
        .is_ok()
    {
        let _ = finished.recv_timeout(DROP_REPORT_BUDGET.saturating_sub(began.elapsed()));
    }
}

fn report(kind: HookKind, runtime_hint: Option<Runtime>) {
    if std::env::var_os(crate::companion::ENV_COMPANION).is_some() {
        return;
    }
    let Ok(paths) = Paths::from_env() else {
        return;
    };
    let socket = paths.socket();
    if !crate::testenv::confined(&socket) {
        return;
    }
    // A Codex app-server can inherit unrelated Claude environment hints.
    let host_id = if runtime_hint.is_none_or(|runtime| runtime == Runtime::Claude) {
        std::env::var(crate::host::HOSTED_CLAUDE_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
    } else {
        None
    };
    let report = HookDropReport {
        kind,
        runtime_hint: runtime_hint.or_else(|| {
            (kind == HookKind::HostedApproval || host_id.is_some()).then_some(Runtime::Claude)
        }),
        host_id,
        parent_pid: std::os::unix::process::parent_id(),
    };
    let delivered = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .is_ok_and(|rt| {
            rt.block_on(async {
                tokio::time::timeout(
                    IPC_BUDGET,
                    ipc::request::<()>(
                        &socket,
                        &Request::HookDropped(report.clone()),
                        IPC_BUDGET,
                        IPC_BUDGET,
                    ),
                )
                .await
                .is_ok_and(|reply| reply.is_ok())
            })
        });
    if !delivered {
        let _ = fallback(&paths, &report);
    }
}

fn fallback(paths: &Paths, report: &HookDropReport) -> io::Result<()> {
    let path = paths.hook_drops_log();
    if !crate::testenv::confined(&path) {
        return Ok(());
    }
    let mut line = serde_json::to_vec(&serde_json::json!({
        "at": Utc::now(),
        "reason": "stdin_timeout",
        "report": report,
    }))?;
    line.push(b'\n');

    let create =
        OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let mode = Mode::RUSR | Mode::WUSR;
    // Hold one lock across append and replacement. Opening it fails if the
    // Senate home does not exist.
    let lock = open(path.with_extension("lock"), create, mode)?;
    flock(&lock, FlockOperation::LockExclusive)?;
    let mut file = File::from(open(&path, create | OFlags::APPEND, mode)?);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(());
    }
    if metadata.len().saturating_add(line.len() as u64) <= LOG_CAP {
        return file.write_all(&line);
    }

    // A timed-out worker may be killed before it finishes writing. Replace
    // the log only once the complete line is ready, using one staging path.
    let staging = path.with_extension("tmp");
    match std::fs::remove_file(&staging) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err),
    }
    let mut replacement = File::from(open(&staging, create | OFlags::EXCL, mode)?);
    replacement.write_all(&line)?;
    std::fs::rename(staging, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_replacement_preserves_the_full_log() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        let log = paths.hook_drops_log();
        let old = vec![b'x'; LOG_CAP as usize];
        std::fs::write(&log, &old).unwrap();
        std::fs::create_dir(log.with_extension("tmp")).unwrap();
        let report = HookDropReport {
            kind: HookKind::SessionEnd,
            runtime_hint: None,
            host_id: None,
            parent_pid: 1,
        };
        assert!(fallback(&paths, &report).is_err());
        assert_eq!(std::fs::read(log).unwrap(), old);
    }

    #[test]
    fn the_hook_releases_a_held_diagnostic_worker() {
        assert_eq!(STDIN_BUDGET, Duration::from_secs(1));
        assert_eq!(DROP_REPORT_BUDGET, Duration::from_millis(200));
        let (release, held) = mpsc::channel::<()>();
        let (returned, observed) = mpsc::channel();
        let caller = std::thread::spawn(move || {
            let began = Instant::now();
            within_report_budget(move || {
                let _ = held.recv();
            });
            let _ = returned.send(began.elapsed());
        });
        // A watchdog protects the test itself. The worker stays held until
        // after the caller returns, regardless of process scheduling.
        let elapsed = observed.recv_timeout(Duration::from_secs(5));
        drop(release);
        assert!(caller.join().is_ok());
        assert!(elapsed.is_ok_and(|elapsed| elapsed >= DROP_REPORT_BUDGET));
    }
}
