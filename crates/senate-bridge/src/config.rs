//! `senate.toml` (formerly `bridge.toml`): who this machine is on the Senate server.
//!
//! ```toml
//! server_url = "https://senate.example.com"
//! token = "<the person's token>"
//! person = "alice"
//! # Optional: the language of everything the bridge writes for people and
//! # agents (`zh` or `en`). Defaults to the deployment default (`en`).
//! locale = "zh"
//! # Optional: keep these committee streams open for this machine's agents
//! # even with no session bound.
//! committees = ["refund-v2"]
//! # Optional: characters of the committee brief a session start injects.
//! brief_chars = 6000
//! # Optional: also bind a session to the committee a branch is named for,
//! # `<prefix>/<committee>/<person>`. Off by default; `branch_prefix` alone
//! # does not turn it on. When on, it is read before `.senate/committee`.
//! committee_from_branch = true
//! # Optional: the first segment of `<prefix>/<committee>/<person>` branches
//! # (the convention above, and the Speaker's work branches).
//! branch_prefix = "feature"
//! # Optional: `senate free` forgets the queued items and cursors of an
//! # agent with no session and no contact for this many days.
//! free_after_days = 30
//!
//! # Optional: the agent each kind of coding session speaks as. These are
//! # the defaults.
//! [agents]
//! claude = "claude"
//! codex = "codex"
//! ```
//!
//! # A machine is a person running agents
//!
//! The token names a **person**. The bridge never speaks as "the person's
//! agent" in the singular: each coding session is one senator, chosen per
//! session ([`crate::agent`]), a flat name the person runs. By default a
//! Claude Code session is `claude` and a Codex session `codex`; `[agents]`
//! renames them, and a session can pin its own name with `SENATE_AGENT` or
//! a `.senate/agent` file.
//!
//! # One Senate
//!
//! A machine serves one Senate: the one at `server_url` (a team's), or the
//! one `senated --machine` hosts for it (`[local]`, which fills in the
//! address and token itself). Senates nested under an agent
//! (`[[preside]]`) are no longer hosted: an agent's own team is a child
//! Senate.
//!
//! Unknown keys are ignored so a newer file still loads.

use std::fmt;
use std::path::{Path, PathBuf};

use senate_core::{BranchNaming, CommitteeId, DEFAULT_BRANCH_PREFIX, Locale, Name};
use serde::{Deserialize, Serialize};

use senate_core::SenatorName;

use crate::session::Runtime;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// Base URL of the Senate server, e.g. `https://senate.example.com`.
    /// Empty in a file for a machine used alone: `senated --machine` fills
    /// it in for the Senate it hosts.
    #[serde(default)]
    pub server_url: String,
    /// The person's bearer token. It names the person to the server.
    #[serde(default)]
    pub token: String,
    /// The person's handle. Every agent this bridge seats is under it.
    pub person: Name,
    /// The language of everything the bridge writes for people and agents.
    #[serde(default)]
    pub locale: Locale,
    /// Committees whose streams stay open with no session bound.
    #[serde(default)]
    pub committees: Vec<CommitteeId>,
    /// Most characters of the committee brief injected at session start; 0
    /// injects none.
    #[serde(default = "default_brief_chars")]
    pub brief_chars: usize,
    /// Whether a `<prefix>/<committee>/<person>` branch name binds a
    /// session to that committee ([`crate::session`]), ahead of
    /// `.senate/committee`. Off unless set: `branch_prefix` alone does not
    /// enable it.
    #[serde(default)]
    pub committee_from_branch: bool,
    /// `senate free` forgets what an agent with no session and no contact
    /// for this many days left in the daemon's state.
    #[serde(default = "default_free_after_days")]
    pub free_after_days: u64,
    /// First segment of a developer branch, `<prefix>/<committee>/<person>`.
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
    /// The agent name each kind of session speaks as.
    #[serde(default)]
    pub agents: AgentNames,
}

/// `[agents]`: the agent a session of each harness speaks as, unless the
/// session pins another ([`crate::agent`]): one flat senator name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNames {
    #[serde(default = "default_claude")]
    pub claude: SenatorName,
    #[serde(default = "default_codex")]
    pub codex: SenatorName,
}

fn default_claude() -> SenatorName {
    Runtime::Claude
        .as_str()
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid name"))
}

fn default_codex() -> SenatorName {
    Runtime::Codex
        .as_str()
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid name"))
}

impl Default for AgentNames {
    fn default() -> Self {
        Self {
            claude: default_claude(),
            codex: default_codex(),
        }
    }
}

impl AgentNames {
    /// The agent for a session of `runtime`.
    pub fn for_runtime(&self, runtime: Runtime) -> &SenatorName {
        match runtime {
            Runtime::Claude => &self.claude,
            Runtime::Codex => &self.codex,
        }
    }
}

/// Default of [`BridgeConfig::brief_chars`].
pub const DEFAULT_BRIEF_CHARS: usize = 6_000;

fn default_brief_chars() -> usize {
    DEFAULT_BRIEF_CHARS
}

fn default_free_after_days() -> u64 {
    crate::binding::DEFAULT_FREE_AFTER_DAYS
}

fn default_branch_prefix() -> String {
    DEFAULT_BRANCH_PREFIX.to_string()
}

impl fmt::Debug for BridgeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeConfig")
            .field("server_url", &self.server_url)
            .field("token", &"<redacted>")
            .field("person", &self.person)
            .field("locale", &self.locale)
            .field("committees", &self.committees)
            .field("brief_chars", &self.brief_chars)
            .field("committee_from_branch", &self.committee_from_branch)
            .field("free_after_days", &self.free_after_days)
            .field("branch_prefix", &self.branch_prefix)
            .field("agents", &self.agents)
            .finish()
    }
}

/// What `senated --machine` hosts on this machine, read from the same
/// `senate.toml`: the machine's one Senate (`[local]`, for using Senate
/// alone).
///
/// ```toml
/// [local]                 # alone: no Senate above, no server_url
/// name = "alice-desk"     # default: <person>-desk
/// speaker = "claude"      # the agent named its Speaker
/// listen = "127.0.0.1:7979"
/// ```
///
/// `[[preside]]` (a Senate nested under an agent) is still read, only so
/// the machine can say once that such Senates are no longer hosted and
/// where their logs stay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Hosted {
    #[serde(default)]
    pub local: Option<LocalSenate>,
    #[serde(default)]
    pub preside: Vec<PresideSenate>,
}

/// `[local]`: this machine's Senate, with no Senate above.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LocalSenate {
    /// The Senate's name. Absent: [`default_desk_name`] of the person.
    #[serde(default)]
    pub name: Option<Name>,
    /// The agent named its Speaker. Absent: nobody presides, and the Senate
    /// only keeps the log.
    #[serde(default)]
    pub speaker: Option<SenatorName>,
    /// A loopback address to serve it on. Default: a free port, kept for
    /// the next start.
    #[serde(default)]
    pub listen: Option<String>,
}

/// `[[preside]]`, as an older file wrote it: a Senate one level down,
/// presided over by `agent`. No longer hosted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PresideSenate {
    /// The presiding agent, as written (a name or a path).
    pub agent: String,
    #[serde(default)]
    pub listen: Option<String>,
}

/// The name of a Senate hosted on a person's machine when `[local]` gives
/// none: the person's handle followed by `-desk` (`alice-desk`). Not the
/// bare handle: the Senate's name must not be taken for a person's handle
/// in the Senate above. A handle too long for the suffix is cut to fit.
pub fn default_desk_name(person: &Name) -> Name {
    const SUFFIX: &str = "-desk";
    let mut stem = person.as_str().to_owned();
    stem.truncate(63 - SUFFIX.len());
    let stem = stem.trim_end_matches('-');
    Name::new(format!("{stem}{SUFFIX}"))
        .unwrap_or_else(|_| Name::new("desk").unwrap_or_else(|_| unreachable!("a valid name")))
}

impl Hosted {
    /// `[local]` and `[[preside]]` of the file at `path`; none when it has
    /// neither.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let hosted: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(hosted)
    }

    /// The rules: the machine's Senate has no Senate above (`server_url` is
    /// empty); without it, the Senate above is configured; its address is
    /// loopback; its name is not an address keyword.
    pub fn validate(&self, config: &BridgeConfig) -> Result<(), String> {
        match (&self.local, config.server_url.is_empty()) {
            (Some(_), false) => {
                return Err(
                    "[local] is this machine's own Senate, with no Senate above: drop \
                     server_url and token, or [local]"
                        .into(),
                );
            }
            (None, true) => {
                return Err("set server_url and token (the Senate above), or [local]".into());
            }
            _ => {}
        }
        if let Some(local) = &self.local {
            if let Some(listen) = &local.listen {
                check_loopback(listen)?;
            }
            senate_core::check_not_keyword(self.name(&config.person).as_str(), "a Senate")
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// The name of the Senate this machine hosts.
    pub fn name(&self, person: &Name) -> Name {
        self.local
            .as_ref()
            .and_then(|l| l.name.clone())
            .unwrap_or_else(|| default_desk_name(person))
    }

    /// Whether this machine hosts a Senate.
    pub fn is_empty(&self) -> bool {
        self.local.is_none()
    }
}

/// A hosted Senate listens on loopback only: its token never leaves the
/// machine, and neither does its traffic.
pub fn check_loopback(listen: &str) -> Result<(), String> {
    let addr: std::net::SocketAddr = listen
        .parse()
        .map_err(|e| format!("listen = {listen:?} is not an address: {e}"))?;
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(format!(
            "listen = {listen:?}: a Senate on this machine listens on loopback only"
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(
        "no configuration at {0}; write one with server_url, token and person (under a team's Senate), or run `senate init --local` (alone)"
    )]
    Missing(PathBuf),
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

impl BridgeConfig {
    /// A configuration with every optional key at its default.
    pub fn new(server_url: &str, token: &str, person: Name) -> Self {
        Self {
            server_url: server_url.to_string(),
            token: token.to_string(),
            person,
            locale: Locale::default(),
            committees: Vec::new(),
            brief_chars: DEFAULT_BRIEF_CHARS,
            committee_from_branch: false,
            free_after_days: default_free_after_days(),
            branch_prefix: default_branch_prefix(),
            agents: AgentNames::default(),
        }
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let config = Self::load_unchecked(path)?;
        config.validate().map_err(|message| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        })?;
        Ok(config)
    }

    /// Read and parse, without [`BridgeConfig::validate`]: for `senated
    /// --machine`, which fills in the Senate it hosts itself before the
    /// configuration is complete.
    pub fn load_unchecked(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(ConfigError::Missing(path.to_path_buf()));
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// The `locale` of the file at `path`, for the thin clients (the MCP
    /// shim, `install`) that need only the language. Anything unreadable,
    /// including a missing file, gives the default: these clients must
    /// still work before the bridge is configured.
    pub fn locale_at(path: &Path) -> Locale {
        #[derive(Deserialize)]
        struct OnlyLocale {
            #[serde(default)]
            locale: Locale,
        }
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<OnlyLocale>(&text).ok())
            .map(|f| f.locale)
            .unwrap_or_default()
    }

    /// The Senate's address is an http(s) URL, and the token is set.
    pub fn validate(&self) -> Result<(), String> {
        if self.server_url.is_empty() {
            return Err("server_url is empty".into());
        }
        if !(self.server_url.starts_with("http://") || self.server_url.starts_with("https://")) {
            return Err(format!(
                "server_url must start with http:// or https://, got {:?}",
                self.server_url
            ));
        }
        if self.token.trim().is_empty() {
            return Err(format!("the token for {} is empty", self.server_url));
        }
        Ok(())
    }

    /// How developer branches are named on this machine.
    pub fn branches(&self) -> BranchNaming {
        BranchNaming::new(&self.branch_prefix)
    }

    /// The naming a branch is read by to find its committee, when
    /// `committee_from_branch` is on.
    pub fn branch_convention(&self) -> Option<BranchNaming> {
        self.committee_from_branch.then(|| self.branches())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_redacts() {
        let c: BridgeConfig = toml::from_str(
            r#"
            server_url = "http://localhost:8080"
            token = "secret"
            person = "alice"
            committees = ["refund-v2"]
            future_key = 1
            "#,
        )
        .unwrap();
        assert!(c.validate().is_ok());
        assert_eq!(c.person.as_str(), "alice");
        assert_eq!(c.committees, vec![CommitteeId::new("refund-v2").unwrap()]);
        assert_eq!(c.brief_chars, DEFAULT_BRIEF_CHARS);
        assert_eq!(c.locale, Locale::default());
        assert_eq!(c.branches().prefix(), "feature");
        assert_eq!(c.agents, AgentNames::default());
        assert!(!format!("{c:?}").contains("secret"));
        let c: BridgeConfig = toml::from_str(
            "server_url = \"http://x\"\ntoken = \"t\"\nperson = \"a\"\nbrief_chars = 0\n",
        )
        .unwrap();
        assert_eq!(c.brief_chars, 0);
    }

    #[test]
    fn locale_agents_and_branch_prefix_are_configurable() {
        let c: BridgeConfig = toml::from_str(
            r#"
            server_url = "http://x"
            token = "t"
            person = "alice"
            locale = "zh-CN"
            branch_prefix = "team/work"
            [agents]
            codex = "reviewer"
            "#,
        )
        .unwrap();
        assert_eq!(c.locale, Locale::Zh);
        assert_eq!(c.branches().prefix(), "team/work");
        assert_eq!(c.agents.for_runtime(Runtime::Claude).to_string(), "claude");
        assert_eq!(c.agents.for_runtime(Runtime::Codex).to_string(), "reviewer");
        // An agent name is one flat senator name: validated where it is
        // written. A path (an owner, or a nested agent) is not one.
        for bad in [
            "[agents]\nclaude = \"Claude\"",
            "[agents]\ncodex = \"a//b\"",
            "[agents]\ncodex = \"claude/codex\"",
            "[agents]\ncodex = \"alice/codex\"",
            "locale = \"fr\"",
            "person = \"Alice\"",
        ] {
            let text = "server_url = \"http://x\"\ntoken = \"t\"\nperson = \"alice\"\n".to_string();
            let text = if bad.starts_with("person") {
                text.replace("person = \"alice\"", bad)
            } else {
                format!("{text}{bad}\n")
            };
            assert!(toml::from_str::<BridgeConfig>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_thin_clients_read_only_the_locale() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge.toml");
        assert_eq!(BridgeConfig::locale_at(&path), Locale::default());
        std::fs::write(&path, "locale = \"zh\"\n[codex]\nlisten = \"x\"\n").unwrap();
        assert_eq!(BridgeConfig::locale_at(&path), Locale::Zh);
        std::fs::write(&path, "locale = \"fr\"\n").unwrap();
        assert_eq!(BridgeConfig::locale_at(&path), Locale::default());
    }

    #[test]
    fn missing_file_is_named() {
        let err = BridgeConfig::load(Path::new("/nonexistent/bridge.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Missing(_)));
    }

    #[test]
    fn a_machine_serves_one_senate() {
        // Replaces "several Senates are listed with their presiders": no
        // Senate is nested under an agent, so `[[senates]]` is gone; an
        // older file that still has it loads, and the entries are ignored.
        let c: BridgeConfig = toml::from_str(
            r#"
            server_url = "https://senate.example.com"
            token = "t"
            person = "alice"
            [[senates]]
            presider = "claude"
            server_url = "http://127.0.0.1:7979"
            token = "nested-secret"
            "#,
        )
        .unwrap();
        assert!(c.validate().is_ok());
        assert_eq!(c.server_url, "https://senate.example.com");
        for bad in [
            "person = \"a\"\n",
            "person = \"a\"\nserver_url = \"ftp://y\"\ntoken = \"u\"\n",
            "person = \"a\"\nserver_url = \"http://y\"\ntoken = \" \"\n",
        ] {
            let c: BridgeConfig = toml::from_str(bad).unwrap();
            assert!(c.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn what_a_machine_hosts_is_checked_against_the_rest() {
        let parse = |text: &str| -> (BridgeConfig, Hosted) {
            (toml::from_str(text).unwrap(), toml::from_str(text).unwrap())
        };
        let (config, hosted) = parse(
            "person = \"alice\"\n[local]\nspeaker = \"claude\"\nlisten = \"127.0.0.1:7979\"\n",
        );
        assert_eq!(hosted.validate(&config), Ok(()));
        let local = hosted.local.clone().unwrap();
        assert_eq!(local.speaker.unwrap().to_string(), "claude");
        // The Senate's name: given, or the person's handle and `-desk`.
        assert_eq!(hosted.name(&config.person).as_str(), "alice-desk");
        let (config, hosted) = parse("person = \"alice\"\n[local]\nname = \"north-desk\"\n");
        assert_eq!(hosted.validate(&config), Ok(()));
        assert_eq!(hosted.name(&config.person).as_str(), "north-desk");
        // An older file's nested Senates are read, to be named, not hosted.
        let (config, hosted) = parse(
            "person = \"alice\"\n[local]\nspeaker = \"claude\"\n[[preside]]\nagent = \"claude\"\n",
        );
        assert_eq!(hosted.validate(&config), Ok(()));
        assert_eq!(hosted.preside[0].agent, "claude");

        for bad in [
            // Alone, yet a Senate above.
            "person = \"a\"\nserver_url = \"https://s\"\ntoken = \"t\"\n[local]\n",
            // Neither.
            "person = \"a\"\n[[preside]]\nagent = \"claude\"\n",
            // Off the machine.
            "person = \"a\"\n[local]\nlisten = \"0.0.0.0:7979\"\n",
            // A keyword is not a Senate's name.
            "person = \"a\"\n[local]\nname = \"speaker\"\n",
        ] {
            let (config, hosted) = parse(bad);
            assert!(hosted.validate(&config).is_err(), "{bad}");
        }
        // The speaker is one flat name.
        assert!(toml::from_str::<Hosted>("[local]\nspeaker = \"alice/claude\"\n").is_err());
    }

    #[test]
    fn the_default_desk_name_is_never_the_bare_handle() {
        let name = |s: &str| Name::new(s).unwrap();
        assert_eq!(default_desk_name(&name("wendell")).as_str(), "wendell-desk");
        let long = "a".repeat(63);
        let desk = default_desk_name(&name(&long));
        assert!(desk.as_str().ends_with("-desk") && desk.as_str().len() <= 63);
        assert_ne!(default_desk_name(&name("bob")).as_str(), "bob");
    }

    #[test]
    fn rejects_bad_url() {
        let c: BridgeConfig =
            toml::from_str("server_url = \"ftp://x\"\ntoken = \"t\"\nperson = \"a\"\n").unwrap();
        assert!(c.validate().is_err());
    }
}
