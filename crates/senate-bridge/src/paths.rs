//! Where the bridge keeps its files: `~/.senate`, or `$SENATE_HOME`.
//!
//! | File | Owner | Purpose |
//! | --- | --- | --- |
//! | `senate.toml` | the user | the developer machine: the person, the Senate above (server URL, token), locale, agent names ([`crate::config`]), and the Senates `senated --machine` hosts here |
//! | `bridge.toml` | the user | the same file under its old name, read when there is no `senate.toml` |
//! | `senates/<senate>/` | `senated` | each Senate hosted on this machine: its log, its local token, its address |
//! | `state.db` | the daemon | cursors, session registry, delivery queue ([`crate::state`]) |
//! | `state.<presider>.db` | the daemon | the same, for each further Senate the machine serves |
//! | `branches/<repository>/<branch>/` | `senate` and the daemon | the saved copy of a worktree's `.senate/` files for each branch ([`crate::follow`]) |
//! | `bridge.sock` | the daemon | local IPC ([`crate::ipc`]), mode 0600 |
//!
//! The directory itself is created with mode 0700: it holds the person's token
//! and every session's inbox token.

use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use senate_core::Locale;

/// The longest Unix socket path, in bytes, the platform can bind:
/// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux, one of
/// them the terminating NUL.
pub const SOCKET_PATH_LIMIT: usize = if cfg!(target_os = "macos") { 103 } else { 107 };

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

/// The daemon's socket path does not fit in a Unix socket address. Binding
/// it would fail, and a daemon under a supervisor would be started again
/// forever, so this is found once at startup and refused. The message is in
/// the language of `senate.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SocketPathTooLong {
    pub path: PathBuf,
    /// The path's length in bytes.
    pub len: usize,
    pub limit: usize,
    pub locale: Locale,
}

impl std::fmt::Display for SocketPathTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (path, len, limit) = (self.path.display(), self.len, self.limit);
        match self.locale {
            Locale::Zh => write!(
                f,
                "本机守护进程的 socket 路径 {path} 有 {len} 字节，超过系统上限 {limit} 字节，\
                 无法监听。请把 {HOME_ENV} 设为更短的目录（例如 ~/.senate）后重试；\
                 senate.toml 和 hosted Senate 的数据会留在原目录，需要自行移动。"
            ),
            Locale::En => write!(
                f,
                "the daemon's socket path {path} is {len} bytes, over the {limit}-byte limit \
                 for a Unix socket, so it cannot listen. Set {HOME_ENV} to a shorter directory \
                 (for example ~/.senate) and start again; senate.toml and any hosted Senate's \
                 data stay in the old directory and must be moved by hand."
            ),
        }
    }
}

impl std::error::Error for SocketPathTooLong {}

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

    /// The saved copies of each repository's branch-bound `.senate/` files
    /// ([`crate::follow`]).
    pub fn branches(&self) -> PathBuf {
        self.home.join("branches")
    }

    pub fn state_db(&self) -> PathBuf {
        self.home.join("state.db")
    }

    pub fn socket(&self) -> PathBuf {
        self.home.join("bridge.sock")
    }

    /// Whether [`Paths::socket`] fits in a Unix socket address. The socket
    /// is never moved somewhere else on its own: the person decides where
    /// their home is.
    pub fn check_socket(&self, locale: Locale) -> Result<(), SocketPathTooLong> {
        let path = self.socket();
        let len = path.as_os_str().len();
        if len > SOCKET_PATH_LIMIT {
            return Err(SocketPathTooLong {
                path,
                len,
                limit: SOCKET_PATH_LIMIT,
                locale,
            });
        }
        Ok(())
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
    }

    #[test]
    fn a_socket_path_over_the_limit_is_refused_in_the_configured_language() {
        let short = Paths::new("/x/.senate");
        assert_eq!(short.check_socket(Locale::En), Ok(()));
        // Exactly at the limit still binds; one byte more does not.
        let name = "/bridge.sock".len();
        let at = Paths::new("h".repeat(SOCKET_PATH_LIMIT - name));
        assert_eq!(at.socket().as_os_str().len(), SOCKET_PATH_LIMIT);
        assert_eq!(at.check_socket(Locale::En), Ok(()));
        let long = Paths::new("h".repeat(SOCKET_PATH_LIMIT - name + 1));
        let en = long.check_socket(Locale::En).unwrap_err();
        assert_eq!(en.len, SOCKET_PATH_LIMIT + 1);
        let text = en.to_string();
        assert!(text.contains(&en.path.display().to_string()), "{text}");
        assert!(text.contains(&SOCKET_PATH_LIMIT.to_string()), "{text}");
        assert!(
            text.contains("SENATE_HOME") && text.contains("shorter"),
            "{text}"
        );
        let zh = long.check_socket(Locale::Zh).unwrap_err().to_string();
        assert!(zh.contains("SENATE_HOME") && zh.contains("更短"), "{zh}");
        assert!(zh.contains(&en.path.display().to_string()), "{zh}");
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
