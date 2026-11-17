//! `senate init --local`: Senate alone on one machine, in one command.
//!
//! It writes two files in the Senate home (`~/.senate`, or `$SENATE_HOME`):
//! `senate.toml`, and beside it a `secretary.toml` whose every key is
//! commented out; nothing else: no service, no Claude Code or Codex
//! configuration (that is `senate install`). The profile is a root Senate
//! hosted by `senated --machine` on loopback, with no Senate above, the
//! person alone on its roster and its Speaker (`senator::<person>`). No
//! agent is named Speaker: an agent the person names in `secretaries`
//! works the Speaker's desk, and with none present the desk waits for the
//! person (`senate inbox`).
//!
//! It never overwrites: a home that already has a `senate.toml` (or the old
//! `bridge.toml`) is left as it is and the command fails; a `secretary.toml`
//! already there is kept.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use senate_core::{Locale, Name, SenatorId, SenatorName};

use crate::paths::Paths;

/// The built-in secretary's file, beside `senate.toml`.
pub const SECRETARY_FILE: &str = "secretary.toml";

/// Where the local Senate listens unless told otherwise.
pub const DEFAULT_LOCAL_LISTEN: &str = "127.0.0.1:7979";

/// What `senate init --local` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalProfile {
    pub person: Name,
    /// The Speaker, when not the person (`senator::claude`: an agent
    /// presides over an unattended root).
    pub speaker: Option<SenatorId>,
    pub locale: Locale,
    /// A loopback address.
    pub listen: String,
}

impl LocalProfile {
    /// The Speaker this profile names: the person unless told otherwise.
    pub fn speaker_id(&self) -> SenatorId {
        self.speaker.clone().unwrap_or_else(|| {
            SenatorId::here(
                SenatorName::new(self.person.clone())
                    .unwrap_or_else(|_| unreachable!("a handle is a senator name")),
            )
        })
    }

    /// `senate.toml`'s text: every key it sets, explained in English (the
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
# In each repository you work in (everything Senate keeps there is the
# untracked .senate/ directory; it follows the branch):
#   senate join <committee>  # bind this worktree, on this branch, to a committee
#   senate where             # what a session here would get, and from where
#   senate list / senate free  # the saved per-branch copies; forget what expired
#
# To keep the daemon running without a terminal (a launchd agent on macOS, a
# systemd user unit on Linux; each is a dry run until you add --yes):
#   senate service install --yes
#   senate service status
#   senate service uninstall --yes
#
# Everything senated keeps for this Senate is under senates/top/ here: its
# log (SQLite), the token only this machine uses, the address it serves.
# The built-in secretary (a model working your desk) is configured in
# secretary.toml beside this file.

# You. Every agent on this machine is yours: each is one flat name
# (claude, codex) in this machine's Senate.
person = "{person}"

# The language of everything written for you and your agents: "en" or "zh".
locale = "{locale}"

# This machine's Senate, with no Senate above. (Under a team's Senate, add
# server_url and token at the top: this Senate is then registered there as
# a child, and its Speaker is you as a senator of that Senate.)
[local]
# Its name: not your bare handle, so it is never taken for you in a Senate
# above.
name = "{name}"
# The Speaker presides over this Senate: everything addressed to `speaker`
# lands on the Speaker's desk. It is you, a senator id. You are never
# assumed present: what waits for you shows in `senate inbox`.
speaker = "{speaker}"
# Who works your desk while you are away, in order (the first present
# acts): agents of this Senate, or the built-in secretary by its name
# (secretary.toml). They pick which agent gets a message you did not
# address to one, forward and answer what they can, and leave the rest
# for you.
# secretaries = ["claude"]
# Loopback only.
listen = "{listen}"
"#,
            name = crate::config::default_desk_name(&self.person),
            person = self.person,
            locale = self.locale,
            speaker = self.speaker_id(),
            listen = self.listen,
        )
    }
}

/// `secretary.toml`'s text: the built-in secretary's settings, every key
/// commented out (no file, or an empty one, is valid: no built-in
/// secretary until a model is configured).
pub fn secretary_toml() -> String {
    r#"# The built-in secretary: a session on a model that works the Speaker's
# desk (and, when named, any senator's) while they are away. Written by
# `senate init --local`; every key is optional, and with no model
# configured there is no built-in secretary. A change takes effect without
# a restart, before it next acts (a changed `name` at the next start).
# A file <committee>.secretary.toml beside this one overrides any key but
# `name` for that committee only.

# Its senator name in this Senate. List it in `secretaries` under
# senate.toml's [local] for it to work your desk.
# name = "secretary"

# Extra instructions for the model, inline and/or in a file (relative to
# this one). Appended to its own rules, never replacing them.
# instructions = "Answer in short sentences."
# instructions_file = "secretary.md"

# What it may do without leaving it for you: route (pick the agent for a
# message several could get), forward (pass a question to another
# committee), refuse (send one back with a reason), cover (answer for an
# agent that is away, from its own notes), triage (yours to answer / needs
# you). Default: all of them.
# duties = ["route", "forward", "refuse", "cover", "triage"]

# The model. Overrides senate.toml's top-level [model], key by key. The
# key is never written here: name the environment variable that holds it.
# [model]
# provider = "anthropic"
# model = "claude-sonnet-4-5"
# api_key_env = "ANTHROPIC_API_KEY"
"#
    .to_owned()
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
/// 0700 home), refusing to replace any configuration already there, and a
/// commented `secretary.toml` beside it unless one is there already.
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
    let secretary = paths.home().join(SECRETARY_FILE);
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&secretary)
    {
        Ok(mut file) => file
            .write_all(secretary_toml().as_bytes())
            .map_err(|source| InitError::Write {
                path: secretary.clone(),
                source,
            })?,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(InitError::Write {
                path: secretary,
                source,
            });
        }
    }
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
            speaker: None,
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
        // Nothing else is written but the secretary's file, all comments.
        let mut entries: Vec<_> = std::fs::read_dir(paths.home())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        entries.sort();
        assert_eq!(entries, ["secretary.toml", "senate.toml"]);
        let secretary = std::fs::read_to_string(paths.home().join(SECRETARY_FILE)).unwrap();
        assert!(
            toml::from_str::<toml::Table>(&secretary)
                .unwrap()
                .is_empty(),
            "every key commented out"
        );
        for key in [
            "# name = ",
            "# instructions = ",
            "# instructions_file = ",
            "# duties = ",
            "# [model]",
            "# api_key_env = ",
        ] {
            assert!(secretary.contains(key), "{key}");
        }
        // Uncommented, it is a valid table with what each key says.
        let uncommented: String = secretary
            .lines()
            .map(|l| match l.strip_prefix("# ") {
                Some(rest) if rest.starts_with('[') || rest.contains(" = ") => rest,
                _ => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let full: toml::Table = toml::from_str(&uncommented).unwrap();
        assert_eq!(full["name"].as_str(), Some("secretary"));
        assert_eq!(
            full["model"]["api_key_env"].as_str(),
            Some("ANTHROPIC_API_KEY")
        );

        let text = std::fs::read_to_string(&path).unwrap();
        let config: BridgeConfig = toml::from_str(&text).unwrap();
        assert_eq!(config.person.as_str(), "alice");
        assert_eq!(config.locale, Locale::Zh);
        let hosted = Hosted::load(&path).unwrap();
        assert_eq!(hosted.validate(&config), Ok(()));
        let local = hosted.local.clone().unwrap();
        // The person presides; no agent is named Speaker.
        assert_eq!(local.speaker.unwrap().to_string(), "senator::alice");
        assert!(local.secretaries.is_empty(), "commented out");
        assert!(text.contains("# secretaries = [\"claude\"]"));
        assert_eq!(local.listen.as_deref(), Some(DEFAULT_LOCAL_LISTEN));
        assert!(hosted.preside.is_empty());
        let (speaker, secretaries, note) = hosted.office_config(&config.person, None).unwrap();
        assert_eq!(speaker.to_string(), "senator::alice");
        assert!(secretaries.is_empty() && note.is_none());

        // An unattended root names its agent, in full.
        let mut agent = profile();
        agent.speaker = Some("senator::claude".parse().unwrap());
        let hosted: Hosted = toml::from_str(&agent.to_toml()).unwrap();
        assert_eq!(
            hosted.local.unwrap().speaker.unwrap().to_string(),
            "senator::claude"
        );
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

        // A secretary.toml already there is kept as it is.
        let kept = tempfile::tempdir().unwrap();
        std::fs::write(kept.path().join(SECRETARY_FILE), "name = \"aide\"\n").unwrap();
        init_local(&Paths::new(kept.path()), &profile()).unwrap();
        assert_eq!(
            std::fs::read_to_string(kept.path().join(SECRETARY_FILE)).unwrap(),
            "name = \"aide\"\n"
        );
    }
}
