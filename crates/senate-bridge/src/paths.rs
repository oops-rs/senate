//! Where the bridge keeps its files: `~/.senate`, or `$SENATE_HOME`.
//!
//! | File | Owner | Purpose |
//! | --- | --- | --- |
//! | `senate.toml` | the user | the developer machine: the person, the Senate above (server URL, token), locale, agent names ([`crate::config`]), and the Senates `senated --machine` hosts here |
//! | `bridge.toml` | the user | the same file under its old name, read when there is no `senate.toml` |
//! | `senates/<senate>/` | `senated` | each Senate hosted on this machine: its log, its local token, its address |
//! | `state.db` | the daemon | cursors, session registry, delivery queue ([`crate::state`]) |
//! | `state.<presider>.db` | the daemon | the same, for each further Senate the machine serves |
//! | `bridge.sock` | the daemon | local IPC ([`crate::ipc`]), mode 0600 |
//!
//! The directory itself is created with mode 0700: it holds the person's token
//! and every session's inbox token.

use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Environment variable that replaces `~/.senate`.
pub const HOME_ENV: &str = "SENATE_HOME";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    home: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    #[error("cannot find a home directory; set {HOME_ENV}")]
    NoHome,
}

impl Paths {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `$SENATE_HOME` when set and non-empty, else `~/.senate`.
    pub fn from_env() -> Result<Self, PathsError> {
        if let Some(home) = std::env::var_os(HOME_ENV).filter(|v| !v.is_empty()) {
            return Ok(Self::new(home));
        }
        let home = dirs::home_dir().ok_or(PathsError::NoHome)?;
        Ok(Self::new(home.join(".senate")))
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The machine's configuration: `senate.toml`, or `bridge.toml` (its
    /// old name) when only that exists.
    pub fn config(&self) -> PathBuf {
        let current = self.senate_toml();
        let old = self.home.join("bridge.toml");
        if !current.exists() && old.exists() {
            old
        } else {
            current
        }
    }

    /// `senate.toml`, whether or not it exists.
    pub fn senate_toml(&self) -> PathBuf {
        self.home.join("senate.toml")
    }

    /// Where `senated` keeps a Senate it hosts on this machine.
    pub fn hosted(&self, key: &str) -> PathBuf {
        self.home.join("senates").join(key)
    }

    pub fn state_db(&self) -> PathBuf {
        self.home.join("state.db")
    }

    /// The state of a Senate presided over by `presider` (relative to the
    /// person, `claude`), when this machine serves several:
    /// `state.<presider with / as ->.db`. The Senate where the person's
    /// agents sit directly keeps `state.db`.
    pub fn state_db_for(&self, presider: &str) -> PathBuf {
        self.home
            .join(format!("state.{}.db", presider.replace('/', "-")))
    }

    pub fn socket(&self) -> PathBuf {
        self.home.join("bridge.sock")
    }

    /// Create the home directory (0700) if missing and tighten its mode.
    pub fn ensure_home(&self) -> io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.home)?;
        std::fs::set_permissions(&self.home, std::fs::Permissions::from_mode(0o700))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_live_under_home() {
        let p = Paths::new("/x/.senate");
        assert_eq!(p.config(), PathBuf::from("/x/.senate/senate.toml"));
        assert_eq!(p.hosted("top"), PathBuf::from("/x/.senate/senates/top"));
        assert_eq!(p.state_db(), PathBuf::from("/x/.senate/state.db"));
        assert_eq!(p.socket(), PathBuf::from("/x/.senate/bridge.sock"));
        assert_eq!(
            p.state_db_for("claude/reviewer"),
            PathBuf::from("/x/.senate/state.claude-reviewer.db")
        );
    }

    #[test]
    fn the_old_file_name_is_read_while_it_is_the_only_one() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path());
        assert_eq!(p.config(), dir.path().join("senate.toml"));
        std::fs::write(dir.path().join("bridge.toml"), "").unwrap();
        assert_eq!(p.config(), dir.path().join("bridge.toml"));
        std::fs::write(dir.path().join("senate.toml"), "").unwrap();
        assert_eq!(p.config(), dir.path().join("senate.toml"));
    }

    #[test]
    fn ensure_home_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths::new(dir.path().join("h"));
        p.ensure_home().unwrap();
        let mode = std::fs::metadata(p.home()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
