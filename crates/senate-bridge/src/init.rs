//! `senate init --local`: Senate alone on one machine, in one command.
//!
//! It writes one file, `senate.toml` in the Senate home (`~/.senate`, or
//! `$SENATE_HOME`), and nothing else: no service, no Claude Code or Codex
//! configuration (that is `senate install`). The profile is a top Senate
//! hosted by `senated --machine` on loopback, with no Senate above, the
//! person alone on its roster, and one of the person's agents named its
//! Speaker: that agent's session presides, and the person's other agents
//! (Codex next to Claude Code, say) sit beside it.
//!
//! It never overwrites: a home that already has a `senate.toml` (or the old
//! `bridge.toml`) is left as it is and the command fails.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use senate_core::{Locale, Name};

use crate::agent::RelPath;
use crate::paths::Paths;

/// Where the local Senate listens unless told otherwise.
pub const DEFAULT_LOCAL_LISTEN: &str = "127.0.0.1:7979";

/// What `senate init --local` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProfile {
    pub person: Name,
    /// The agent named Speaker, relative to the person.
    pub speaker: RelPath,
    pub locale: Locale,
    /// A loopback address.
    pub listen: String,
}

impl LocalProfile {
    /// The file's text: every key it sets, explained in English (the
    /// configuration's comments are for whoever edits it, like the
    /// examples in `config/`).
    pub fn to_toml(&self) -> String {
        format!(
            r#"# Senate alone on this machine, written by `senate init --local`.
#
#   senated --machine        # the daemon: this Senate, and your sessions' side
#   senate install --yes     # once: the MCP server and hooks for Claude Code / Codex
#   senate status
#
# Everything senated keeps for this Senate is under senates/top/ here: its
# log (SQLite), the token only this machine uses, the address it serves.

# You. Every agent on this machine is named under you.
person = "{person}"

# The language of everything written for you and your agents: "en" or "zh".
locale = "{locale}"

# A top Senate on this machine, with no Senate above.
[local]
# The agent that presides over it as its Speaker: its sessions pick which
# of your agents gets a message you did not address to one, answer for an
# agent that is away from that agent's own checkpoint, and bring you what
# they cannot settle. Your other agents (codex, ...) sit beside it.
speaker = "{speaker}"
# Loopback only.
listen = "{listen}"

# A Senate one level down for an agent's own subagents (sessions pinned to
# SENATE_AGENT=claude/reviewer, say), presided over by that agent:
# [[preside]]
# agent = "claude"
"#,
            person = self.person,
            locale = self.locale,
            speaker = self.speaker,
            listen = self.listen,
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("{0} already exists; senate init writes a new configuration only")]
    Exists(PathBuf),
    #[error("{0}")]
    Invalid(String),
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Write `profile` as the Senate home's `senate.toml` (mode 0600, in a
/// 0700 home), refusing to replace any configuration already there.
pub fn init_local(paths: &Paths, profile: &LocalProfile) -> Result<PathBuf, InitError> {
    crate::config::check_loopback(&profile.listen).map_err(InitError::Invalid)?;
    for existing in [paths.senate_toml(), paths.home().join("bridge.toml")] {
        if existing.exists() {
            return Err(InitError::Exists(existing));
        }
    }
    let path = paths.senate_toml();
    let write = |source| InitError::Write {
        path: path.clone(),
        source,
    };
    paths.ensure_home().map_err(write)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(write)?;
    file.write_all(profile.to_toml().as_bytes())
        .map_err(write)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::BridgeConfig;
    use crate::config::Hosted;

    fn profile() -> LocalProfile {
        LocalProfile {
            person: Name::new("alice").unwrap(),
            speaker: "claude".parse().unwrap(),
            locale: Locale::Zh,
            listen: DEFAULT_LOCAL_LISTEN.into(),
        }
    }

    #[test]
    fn the_local_profile_is_one_private_file_that_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path().join("senate"));
        let path = init_local(&paths, &profile()).unwrap();
        assert_eq!(path, paths.senate_toml());
        assert_eq!(paths.config(), path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Nothing else is written.
        let entries: Vec<_> = std::fs::read_dir(paths.home()).unwrap().collect();
        assert_eq!(entries.len(), 1);

        let text = std::fs::read_to_string(&path).unwrap();
        let config: BridgeConfig = toml::from_str(&text).unwrap();
        assert_eq!(config.person.as_str(), "alice");
        assert_eq!(config.locale, Locale::Zh);
        let hosted = Hosted::load(&path).unwrap();
        assert_eq!(hosted.validate(&config), Ok(()));
        let local = hosted.local.unwrap();
        assert_eq!(local.speaker.unwrap().to_string(), "claude");
        assert_eq!(local.listen.as_deref(), Some(DEFAULT_LOCAL_LISTEN));
        assert!(hosted.preside.is_empty());
    }

    #[test]
    fn it_never_overwrites_and_never_leaves_loopback() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(dir.path());
        std::fs::write(dir.path().join("bridge.toml"), "person = \"bob\"\n").unwrap();
        assert!(matches!(
            init_local(&paths, &profile()),
            Err(InitError::Exists(_))
        ));
        let fresh = tempfile::tempdir().unwrap();
        let mut open = profile();
        open.listen = "0.0.0.0:7979".into();
        assert!(matches!(
            init_local(&Paths::new(fresh.path()), &open),
            Err(InitError::Invalid(_))
        ));
        assert!(!fresh.path().join("senate.toml").exists());
    }
}
