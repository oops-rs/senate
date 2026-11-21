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
//!   `notify-send` is on `PATH`;
//! - WSL ([`crate::wsl`]): a Windows toast, the only notifier there (a
//!   distro has no notification daemon of its own), through
//!   `powershell.exe -NoProfile -NonInteractive -EncodedCommand <script>`
//!   ([`toast_script`]).
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

/// `text` as a PowerShell single-quoted string literal. PowerShell takes
/// the typographic single quotes as quotes too, so each is doubled like
/// `'`; a control character becomes a space.
pub fn powershell_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('\'');
    for c in text.chars() {
        match c {
            '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => {
                out.push(c);
                out.push(c);
            }
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// The application a toast from WSL is shown as: Windows PowerShell's own
/// id, which every Windows has registered.
pub const TOAST_APP: &str =
    r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe";

/// The PowerShell that shows `title` and `body` as a Windows toast through
/// WinRT's `ToastNotificationManager`. The text goes in as text nodes, never
/// as markup.
pub fn toast_script(title: &str, body: &str) -> String {
    [
        "$ErrorActionPreference = 'Stop'".to_owned(),
        // Else PowerShell reports loading the WinRT types as progress, as
        // CLIXML on stderr, around any error.
        "$ProgressPreference = 'SilentlyContinue'".to_owned(),
        // A failure is one readable line on stderr, for the daemon's log
        // (without a desktop, as from an SSH session: "the notification
        // platform is unavailable").
        "[Console]::OutputEncoding = [Text.Encoding]::UTF8".to_owned(),
        "trap { [Console]::Error.WriteLine($_.Exception.Message); exit 1 }".to_owned(),
        "[void][Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]".to_owned(),
        "[void][Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime]".to_owned(),
        "$x = New-Object Windows.Data.Xml.Dom.XmlDocument".to_owned(),
        r#"$x.LoadXml('<toast><visual><binding template="ToastGeneric"><text/><text/></binding></visual></toast>')"#.to_owned(),
        "$t = $x.GetElementsByTagName('text')".to_owned(),
        format!("[void]$t.Item(0).AppendChild($x.CreateTextNode({}))", powershell_literal(title)),
        format!("[void]$t.Item(1).AppendChild($x.CreateTextNode({}))", powershell_literal(body)),
        format!(
            "[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier({}).Show([Windows.UI.Notifications.ToastNotification]::new($x))",
            powershell_literal(TOAST_APP)
        ),
    ]
    .join("\n")
}

/// `script` as `-EncodedCommand` takes it: UTF-16LE, in base64. No shell
/// or argument quoting can then change it.
pub fn encoded_command(script: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Where Windows PowerShell is, seen from WSL: on `PATH` when WSL put the
/// Windows directories there (the daemon's service leaves them out, see
/// [`crate::wsl`]), else where Windows keeps it on drive C.
fn wsl_powershell() -> String {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("powershell.exe"))
                .find(|p| p.is_file())
        })
        .map_or_else(
            || "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe".to_owned(),
            |p| p.to_string_lossy().into_owned(),
        )
}

/// The systems [`command`] posts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum System {
    Macos,
    Linux,
    /// Linux in WSL, with Windows PowerShell at this path.
    Wsl {
        powershell: String,
    },
}

impl System {
    /// This one, or `None` where there is no way to post.
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(target_os = "linux") {
            Some(match crate::wsl::distro() {
                Some(_) => Self::Wsl {
                    powershell: wsl_powershell(),
                },
                None => Self::Linux,
            })
        } else {
            None
        }
    }
}

/// The program and arguments that post `title` and `body` on `system`.
pub fn command(system: &System, title: &str, body: &str) -> (String, Vec<String>) {
    match system {
        System::Macos => (
            "osascript".into(),
            vec![
                "-e".into(),
                format!(
                    "display notification \"{}\" with title \"{}\"",
                    applescript(body),
                    applescript(title)
                ),
            ],
        ),
        System::Linux => (
            "notify-send".into(),
            vec![
                "--app-name".into(),
                "Senate".into(),
                "--".into(),
                title.replace('\n', " "),
                body.replace('\n', " "),
            ],
        ),
        System::Wsl { powershell } => (
            powershell.clone(),
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-EncodedCommand".into(),
                encoded_command(&toast_script(title, body)),
            ],
        ),
    }
}

/// The system's notifier: `osascript` on macOS, `notify-send` on Linux, a
/// Windows toast in WSL.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemNotifier;

impl Notifier for SystemNotifier {
    fn post(&self, title: &str, body: &str) -> Result<(), String> {
        let system = System::current()
            .ok_or_else(|| "desktop notifications are not supported on this system".to_string())?;
        let (program, args) = command(&system, title, body);
        let out = std::process::Command::new(&program)
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
    /// `[senator::claude] answered: the first line`, in the daemon's
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
        let (program, args) = command(
            &System::Macos,
            "Senate · refund-v2",
            "[senator::a/claude]: \"x\"",
        );
        assert_eq!(program, "osascript");
        assert_eq!(
            args,
            [
                "-e",
                "display notification \"[senator::a/claude]: \\\"x\\\"\" with title \"Senate · refund-v2\""
            ]
        );
        let (program, args) = command(&System::Linux, "T", "b\nc");
        assert_eq!(program, "notify-send");
        assert_eq!(args, ["--app-name", "Senate", "--", "T", "b c"]);
    }

    #[test]
    fn in_wsl_a_windows_toast_is_posted_through_an_encoded_powershell_script() {
        assert_eq!(powershell_literal("it's"), "'it''s'");
        assert_eq!(
            powershell_literal("a\u{2019}b\nc"),
            "'a\u{2019}\u{2019}b c'"
        );
        assert_eq!(encoded_command("ab"), "YQBiAA==");
        assert_eq!(encoded_command("abc"), "YQBiAGMA");
        let ps = "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe";
        let body = "[senator::bob]: it's '); Remove-Item x; ('";
        let (program, args) = command(
            &System::Wsl {
                powershell: ps.into(),
            },
            "Senate · wsl",
            body,
        );
        assert_eq!(program, ps);
        assert_eq!(
            args[..3],
            ["-NoProfile", "-NonInteractive", "-EncodedCommand"]
        );
        let script = toast_script("Senate · wsl", body);
        assert_eq!(args[3], encoded_command(&script));
        assert!(
            script.contains(r"CreateTextNode('[senator::bob]: it''s ''); Remove-Item x; (''')"),
            "{script}"
        );
        assert!(
            script.contains("CreateTextNode('Senate · wsl')"),
            "{script}"
        );
        assert!(script.contains(
            r"CreateToastNotifier('{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\WindowsPowerShell\v1.0\powershell.exe')"
        ));
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
