//! What the snapshots pushed to the app cost, counted here and, while
//! `SENATE_APP_PERF` is set (the app's own switch for its measures), printed
//! to stderr once a second:
//!
//! ```text
//! perf: core at_ms=1795700000000 changed=412 pushed=37 unchanged=375 clone_us=910 clone_worst_us=41 build_us=5210 build_worst_us=380 compare_us=120 compare_worst_us=9 deliver_us=2950 deliver_worst_us=212
//! ```
//!
//! `changed`: the driver was told the model changed. `pushed`: snapshots
//! handed to the listener. `unchanged`: snapshots built and not pushed,
//! being what the listener already has. `clone`: copying the model out of
//! its lock; `build`: [`crate::AppState::from_model`]; `compare`: telling
//! whether the listener has it already, and keeping a copy of it when it
//! has not; `deliver`: the listener's call (UniFFI writing the snapshot
//! out and the app reading it in). Counts and `*_us` sums are since the
//! core started, so two lines give a window; each `*_worst_us` is the worst
//! since the line before.
//!
//! With it, `SENATE_APP_PERF_EVERY_CHANGE=1` makes the core hand the
//! listener a snapshot on every change, at once and whether or not it is
//! the one it has: what the core did before it shared snapshots between
//! changes, kept so one build can be measured both ways on one machine.
//!
//! Unset, nothing is timed and nothing is printed.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The app's switch.
pub const ENV: &str = "SENATE_APP_PERF";
/// The baseline to measure against.
pub const EVERY_CHANGE_ENV: &str = "SENATE_APP_PERF_EVERY_CHANGE";
const EVERY: Duration = Duration::from_secs(1);

/// The clock the first online snapshot is timed from: set when the first
/// core is made.
static STARTED: OnceLock<Option<Instant>> = OnceLock::new();

pub(crate) static CHANGED: AtomicU64 = AtomicU64::new(0);
pub(crate) static UNCHANGED: AtomicU64 = AtomicU64::new(0);
static ONLINE: AtomicU64 = AtomicU64::new(0);
pub(crate) static CLONE: Timed = Timed::new();
pub(crate) static BUILD: Timed = Timed::new();
pub(crate) static COMPARE: Timed = Timed::new();
/// One per snapshot pushed.
pub(crate) static DELIVER: Timed = Timed::new();

/// How many times something ran, for how long in all, and the longest.
pub(crate) struct Timed {
    count: AtomicU64,
    nanos: AtomicU64,
    worst: AtomicU64,
}

impl Timed {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
            worst: AtomicU64::new(0),
        }
    }

    /// Run `work`, timed while measuring.
    pub(crate) fn time<T>(&self, work: impl FnOnce() -> T) -> T {
        if !on() {
            return work();
        }
        let start = Instant::now();
        let out = work();
        let nanos = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
        self.worst.fetch_max(nanos, Ordering::Relaxed);
        out
    }

    pub(crate) fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// `(sum, worst since the last call)`, in microseconds.
    fn micros(&self) -> (u64, u64) {
        (
            self.nanos.load(Ordering::Relaxed) / 1000,
            self.worst.swap(0, Ordering::Relaxed) / 1000,
        )
    }
}

/// Measuring: `SENATE_APP_PERF` was set when the first core was made.
pub(crate) fn on() -> bool {
    matches!(STARTED.get(), Some(Some(_)))
}

/// Measuring, and asked for a snapshot on every change (see the module).
pub(crate) fn every_change() -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    on() && *ASKED.get_or_init(|| std::env::var_os(EVERY_CHANGE_ENV).is_some_and(|v| v == "1"))
}

/// Called when a core is made: with the switch set, start the clock and
/// the printing (once per process).
pub(crate) fn start() {
    let mut first = false;
    STARTED.get_or_init(|| {
        first = true;
        std::env::var_os(ENV)
            .is_some_and(|v| !v.is_empty())
            .then(Instant::now)
    });
    if !first || !on() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("senate-core-perf".into())
        .spawn(|| {
            loop {
                std::thread::sleep(EVERY);
                eprintln!("{}", line());
            }
        });
    if let Err(e) = spawned {
        tracing::debug!("no perf thread: {e}");
    }
}

/// A snapshot went to the listener; the first one online says how long
/// that took from the core being made.
pub(crate) fn pushed(online: bool) {
    if !online || ONLINE.swap(1, Ordering::Relaxed) == 1 {
        return;
    }
    if let Some(Some(started)) = STARTED.get() {
        eprintln!(
            "perf: core first online snapshot at_ms={} after_ms={}",
            epoch_ms(),
            started.elapsed().as_millis()
        );
    }
}

fn line() -> String {
    let (clone, clone_worst) = CLONE.micros();
    let (build, build_worst) = BUILD.micros();
    let (compare, compare_worst) = COMPARE.micros();
    let (deliver, deliver_worst) = DELIVER.micros();
    format!(
        "perf: core at_ms={} changed={} pushed={} unchanged={} clone_us={clone} clone_worst_us={clone_worst} build_us={build} build_worst_us={build_worst} compare_us={compare} compare_worst_us={compare_worst} deliver_us={deliver} deliver_worst_us={deliver_worst}",
        epoch_ms(),
        CHANGED.load(Ordering::Relaxed),
        DELIVER.count(),
        UNCHANGED.load(Ordering::Relaxed),
    )
}

fn epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}
