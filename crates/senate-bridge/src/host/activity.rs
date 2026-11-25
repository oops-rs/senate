//! When a hosted TUI last wrote to its terminal.
//!
//! A turn that is running keeps repainting its spinner or its timer, in
//! Claude Code, Codex and agy alike; a TUI sitting at its prompt writes
//! nothing. So the time of the last write tells a busy session from an
//! idle one whatever the runtime, and without trusting its hooks: a turn
//! Senate pushed into Claude Code fires no prompt hook, and an interrupted
//! turn fires no stop hook.
//!
//! The host keeps that time ([`Screen::last_output`]) and uses it before it
//! types anything ([`super::control`]). For the daemon, which only lists
//! sessions, it is written to a marker file, `output`, at most once every
//! [`EVERY`] while the TUI writes and never while it is quiet. No screen
//! text is written anywhere.

use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{directory, observed_at, watch::Screen};
use crate::paths::Paths;
use senate_core::EventId;

/// How often the marker follows the screen.
const EVERY: Duration = Duration::from_secs(1);
const MARKER: &str = "output";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Output {
    /// When the TUI last wrote, [`observed_at`]'s clock.
    at: u64,
}

/// How long ago host `id`'s TUI last wrote to its terminal, to within
/// [`EVERY`]; `None` when the host never said (an older host).
pub(crate) fn quiet_for(paths: &Paths, id: &EventId) -> Option<Duration> {
    let data = std::fs::read(directory(paths, id).join(MARKER)).ok()?;
    let output: Output = serde_json::from_slice(&data).ok()?;
    Some(Duration::from_nanos(
        observed_at().saturating_sub(output.at),
    ))
}

/// Keep the marker at the screen's last write.
pub(super) async fn watch(dir: &Path, screen: &Screen) -> anyhow::Result<()> {
    let marker = dir.join(MARKER);
    let mut written = None;
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let at = screen.last_output();
        if written == Some(at) {
            continue;
        }
        let recorded = (|| -> std::io::Result<()> {
            let staged = marker.with_extension("tmp");
            std::fs::write(&staged, serde_json::to_vec(&Output { at })?)?;
            std::fs::rename(staged, &marker)
        })();
        match recorded {
            Ok(()) => written = Some(at),
            Err(error) => tracing::debug!(%error, "recording when the hosted TUI last wrote"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_marker_follows_the_screen_and_rests_while_it_is_quiet() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let paths = Paths::new(home.path());
        let id = EventId::new();
        let dir = directory(&paths, &id);
        std::fs::create_dir_all(&dir)?;
        assert_eq!(quiet_for(&paths, &id), None, "an older host says nothing");

        let screen = Screen::new(24, 80);
        let wrote = observed_at().saturating_sub(30_000_000_000);
        screen.set_last_output(wrote);
        let watching = watch(&dir, &screen);
        tokio::pin!(watching);
        let _ = tokio::time::timeout(Duration::from_millis(200), &mut watching).await;
        let quiet = quiet_for(&paths, &id).expect("the marker is written");
        assert!(
            (30..40).contains(&quiet.as_secs()),
            "quiet for {quiet:?}, the screen wrote 30s ago"
        );
        let first = std::fs::metadata(dir.join(MARKER))?.modified()?;

        // Quiet: nothing is written again.
        let _ = tokio::time::timeout(Duration::from_millis(1300), &mut watching).await;
        assert_eq!(std::fs::metadata(dir.join(MARKER))?.modified()?, first);

        screen.set_last_output(observed_at());
        let _ = tokio::time::timeout(Duration::from_millis(1300), &mut watching).await;
        assert!(quiet_for(&paths, &id).expect("the marker").as_secs() < 5);
        Ok(())
    }
}
