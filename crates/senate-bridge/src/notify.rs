//! Desktop notifications for what comes back to the person, so nothing needs
//! to be left running in a terminal.
//!
//! When an event addressed to the person arrives ([`senate_core::attention`])
//! and no client that shows notifications itself is connected (the desktop
//! app, through the app core's `watch` stream), the daemon posts one
//! notification: the title names Senate and the committee, the body says who
//! wrote and the first line. Bursts are coalesced: events within
//! [`Timing::notify_window`](crate::Timing) become one notification saying how
//! many. It is best effort: a missing program or a failure is logged once and
//! never affects delivery.
//!
//! Every call goes through [`Notifier`], the one seam; tests replace it and
//! never post a real notification. Only `senated --machine` installs the
//! system one ([`SystemNotifier`]); a daemon built with no notifier posts
//! nothing.
//!
//! The commands are exactly:
//!
//! - macOS: `osascript -e 'display notification "<body>" with title "<title>"'`,
//!   the text escaped for an AppleScript string ([`applescript`]);
//! - Linux: `notify-send --app-name Senate -- <title> <body>`, when
//!   `notify-send` is on `PATH`.
//!
//! Config, in `senate.toml`: `[notify] desktop = true` (default); `false`
//! turns it off ([`NotifyConfig`]).

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use senate_core::{CommitteeId, Locale};
use serde::Deserialize;
use tokio::sync::mpsc;

/// Where a notification goes. The only seam: tests replace it.
pub trait Notifier: Send + Sync + fmt::Debug {
    /// Post one notification. An error is logged once and ignored.
    fn post(&self, title: &str, body: &str) -> Result<(), String>;
}

/// `[notify]` in `senate.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct NotifyConfig {
    /// Post desktop notifications (default `true`).
    #[serde(default = "yes")]
    pub desktop: bool,
}

fn yes() -> bool {
    true
}

impl Default for NotifyConfig {
    fn default() -> Self {
        Self { desktop: true }
    }
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    notify: NotifyConfig,
}

impl NotifyConfig {
    /// The `[notify]` table of the config at `path`; the defaults (on) when
    /// the file or the table is missing.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|e| format!("parsing [notify] in {}: {e}", path.display()))?;
        Ok(file.notify)
    }
}

/// `text` as the inside of an AppleScript string literal.
pub fn applescript(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// The program and arguments that post `title` and `body` on this system,
/// or `None` where there is no way to.
pub fn command(title: &str, body: &str) -> Option<(&'static str, Vec<String>)> {
    if cfg!(target_os = "macos") {
        Some((
            "osascript",
            vec![
                "-e".into(),
                format!(
                    "display notification \"{}\" with title \"{}\"",
                    applescript(body),
                    applescript(title)
                ),
            ],
        ))
    } else if cfg!(target_os = "linux") {
        Some((
            "notify-send",
            vec![
                "--app-name".into(),
                "Senate".into(),
                "--".into(),
                title.replace('\n', " "),
                body.replace('\n', " "),
            ],
        ))
    } else {
        None
    }
}

/// The system's notifier: `osascript` on macOS, `notify-send` on Linux.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemNotifier;

impl Notifier for SystemNotifier {
    fn post(&self, title: &str, body: &str) -> Result<(), String> {
        let (program, args) = command(title, body)
            .ok_or_else(|| "desktop notifications are not supported on this system".to_string())?;
        let out = std::process::Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{program}: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{program} exited with {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

/// Default time a burst is gathered for before one notification is posted.
pub const COALESCE: Duration = Duration::from_secs(3);

/// A notifier and how long it gathers a burst: what a daemon is given to
/// post desktop notifications with (`DaemonOptions::desktop`).
#[derive(Debug, Clone)]
pub struct Desktop {
    pub notifier: Arc<dyn Notifier>,
    pub window: Duration,
}

impl Desktop {
    pub fn new(notifier: Arc<dyn Notifier>) -> Self {
        Self {
            notifier,
            window: COALESCE,
        }
    }

    /// The system's own notifier, as `senated --machine` runs it.
    pub fn system() -> Self {
        Self::new(Arc::new(SystemNotifier))
    }
}

/// One thing that came back to the person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct News {
    pub committee: CommitteeId,
    /// `[senator::alice/claude] answered: the first line`, in the daemon's
    /// language.
    pub line: String,
}

/// The coalescing queue in front of a [`Notifier`].
#[derive(Debug, Clone)]
pub struct Notifications {
    tx: mpsc::UnboundedSender<News>,
}

impl Notifications {
    /// Start the task that posts: after the first piece of news it waits
    /// `window` for more, then posts once, unless an app client is
    /// connected (`app_clients` above zero) which shows notifications
    /// itself. Must run inside a tokio runtime; the task ends when the last
    /// handle is dropped.
    pub fn spawn(
        notifier: Arc<dyn Notifier>,
        window: Duration,
        app_clients: Arc<AtomicUsize>,
        locale: Locale,
    ) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<News>();
        let reported = Arc::new(AtomicBool::new(false));
        tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut burst = vec![first];
                let deadline = tokio::time::Instant::now() + window;
                while let Ok(Some(more)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                    burst.push(more);
                }
                if app_clients.load(Ordering::SeqCst) > 0 {
                    continue;
                }
                let (title, body) = compose(&burst, locale);
                let notifier = notifier.clone();
                let result =
                    tokio::task::spawn_blocking(move || notifier.post(&title, &body)).await;
                let error = match result {
                    Ok(Ok(())) => continue,
                    Ok(Err(e)) => e,
                    Err(e) => e.to_string(),
                };
                if !reported.swap(true, Ordering::SeqCst) {
                    tracing::warn!("desktop notification failed (not reported again): {error}");
                } else {
                    tracing::debug!("desktop notification failed: {error}");
                }
            }
        });
        Self { tx }
    }

    pub fn push(&self, news: News) {
        // The task only ends with the daemon.
        let _ = self.tx.send(news);
    }
}

/// The title and body of the notification for a burst.
pub fn compose(burst: &[News], locale: Locale) -> (String, String) {
    let committees: BTreeSet<&str> = burst.iter().map(|n| n.committee.as_str()).collect();
    let title = format!(
        "Senate · {}",
        committees.into_iter().collect::<Vec<_>>().join(", ")
    );
    let last = burst.last().map(|n| n.line.as_str()).unwrap_or_default();
    let body = if burst.len() == 1 {
        last.to_string()
    } else {
        match locale {
            Locale::Zh => format!("{} 条新消息，最新：{last}", burst.len()),
            Locale::En => format!("{} new for you, the latest: {last}", burst.len()),
        }
    };
    (title, body)
}

/// A notifier that remembers what it was asked to post, for tests.
#[derive(Debug, Default)]
pub struct Recording {
    posted: Mutex<Vec<(String, String)>>,
    fail: AtomicBool,
}

impl Recording {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Make every `post` fail from now on.
    pub fn failing(self: &Arc<Self>) -> Arc<Self> {
        self.fail.store(true, Ordering::SeqCst);
        self.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<(String, String)>> {
        self.posted.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every (title, body) asked for, whether or not it failed.
    pub fn posted(&self) -> Vec<(String, String)> {
        self.lock().clone()
    }
}

impl Notifier for Recording {
    fn post(&self, title: &str, body: &str) -> Result<(), String> {
        self.lock().push((title.to_string(), body.to_string()));
        if self.fail.load(Ordering::SeqCst) {
            Err("no notification daemon".into())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn news(c: &str, line: &str) -> News {
        News {
            committee: CommitteeId::new(c).unwrap(),
            line: line.into(),
        }
    }

    #[test]
    fn the_text_is_escaped_for_applescript_and_the_command_is_exact() {
        assert_eq!(
            applescript("say \"hi\" \\ there\nnext"),
            "say \\\"hi\\\" \\\\ there next"
        );
        if cfg!(target_os = "macos") {
            let (program, args) =
                command("Senate · refund-v2", "[senator::a/claude]: \"x\"").unwrap();
            assert_eq!(program, "osascript");
            assert_eq!(
                args,
                [
                    "-e",
                    "display notification \"[senator::a/claude]: \\\"x\\\"\" with title \"Senate · refund-v2\""
                ]
            );
        }
        if cfg!(target_os = "linux") {
            let (program, args) = command("T", "b\nc").unwrap();
            assert_eq!(program, "notify-send");
            assert_eq!(args, ["--app-name", "Senate", "--", "T", "b c"]);
        }
    }

    #[test]
    fn a_burst_says_how_many_in_either_language() {
        let one = [news("a", "[x]: hi")];
        assert_eq!(
            compose(&one, Locale::En),
            ("Senate · a".into(), "[x]: hi".into())
        );
        let many = [news("b", "one"), news("a", "two"), news("b", "three")];
        assert_eq!(
            compose(&many, Locale::En),
            (
                "Senate · a, b".into(),
                "3 new for you, the latest: three".into()
            )
        );
        assert_eq!(compose(&many, Locale::Zh).1, "3 条新消息，最新：three");
    }

    #[test]
    fn notify_defaults_on_and_can_be_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("senate.toml");
        assert!(NotifyConfig::load(&p).unwrap().desktop, "no file");
        std::fs::write(&p, "person = \"a\"\n").unwrap();
        assert!(NotifyConfig::load(&p).unwrap().desktop, "no table");
        std::fs::write(&p, "[notify]\ndesktop = false\n").unwrap();
        assert!(!NotifyConfig::load(&p).unwrap().desktop);
        std::fs::write(&p, "[notify]\n").unwrap();
        assert!(NotifyConfig::load(&p).unwrap().desktop);
    }

    #[tokio::test]
    async fn a_burst_is_one_post_and_a_connected_app_posts_nothing() {
        let rec = Recording::new();
        let app = Arc::new(AtomicUsize::new(0));
        let n = Notifications::spawn(
            rec.clone(),
            Duration::from_millis(80),
            app.clone(),
            Locale::En,
        );
        for i in 0..3 {
            n.push(news("a", &format!("m{i}")));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(rec.posted().len(), 1, "{:?}", rec.posted());
        assert!(rec.posted()[0].1.starts_with("3 new for you"));
        // The app is connected: quiet.
        app.store(1, Ordering::SeqCst);
        n.push(news("a", "x"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(rec.posted().len(), 1);
        // And a failing notifier is only logged; the next burst still tries.
        app.store(0, Ordering::SeqCst);
        let bad = Recording::new().failing();
        let n = Notifications::spawn(bad.clone(), Duration::from_millis(20), app, Locale::En);
        n.push(news("a", "x"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        n.push(news("a", "y"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(bad.posted().len(), 2);
    }
}
