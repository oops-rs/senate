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
//! # Optional: the first segment of `<prefix>/<committee>/<person>` branches.
//! branch_prefix = "feature"
//!
//! # Optional: the agent each kind of coding session speaks as, under the
//! # person (`alice/claude`, `alice/codex`). These are the defaults.
//! [agents]
//! claude = "claude"
//! codex = "codex"
//! ```
//!
//! # A machine is a person running agents
//!
//! The token names a **person**. The bridge never speaks as "the person's
//! agent" in the singular: each coding session is one junior senator,
//! chosen per session ([`crate::agent`]). By default a Claude Code session
//! is `<person>/claude` and a Codex session `<person>/codex`; `[agents]`
//! renames them, and a session can pin its own name with `SENATE_AGENT` or a
//! `.senate/agent` file.
//!
//! # Several Senates
//!
//! One machine may serve several Senates at once: the Senate above (the
//! top-level `server_url` and `token`) and any number of others, each a
//! `[[senates]]` entry ([`SenateLink`]) naming the agent that presides over
//! it, as a path relative to the person:
//!
//! ```toml
//! server_url = "https://senate.example.com"
//! token = "<the person's token>"
//! person = "alice"
//!
//! [[senates]]
//! presider = "claude"                 # alice/claude presides here
//! server_url = "http://127.0.0.1:7979"
//! token = "<a token for this Senate>"
//! ```
//!
//! Each session is attached to the one Senate its agent's path selects
//! ([`crate::agent`]): `claude` sits in the Senate above, `claude/reviewer`
//! in the one `alice/claude` presides over. With a single Senate (the
//! top-level keys alone) everything is as it always was. `senated` writes
//! the entries for the Senates it hosts itself; nobody needs to.
//!
//! Unknown keys are ignored so a newer file still loads.

use std::fmt;
use std::path::{Path, PathBuf};

use senate_core::{BranchNaming, CommitteeId, DEFAULT_BRANCH_PREFIX, Locale, Name};
use serde::{Deserialize, Serialize};

use crate::agent::RelPath;
use crate::session::Runtime;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// Base URL of the Senate server, e.g. `https://senate.example.com`.
    /// May be empty when `senates` lists every Senate.
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
    /// First segment of a developer branch, `<prefix>/<committee>/<person>`.
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
    /// The agent name each kind of session speaks as.
    #[serde(default)]
    pub agents: AgentNames,
    /// Other Senates this machine serves, besides the one at `server_url`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub senates: Vec<SenateLink>,
}

/// One Senate a machine serves (`[[senates]]`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenateLink {
    /// Base URL of the Senate's server.
    pub server_url: String,
    /// The person's token on that server.
    pub token: String,
    /// The agent presiding over this Senate, relative to the person
    /// (`claude` for `alice/claude`). Absent: the Senate in which the
    /// person's agents sit directly (the top Senate, or the one above).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presider: Option<RelPath>,
}

impl fmt::Debug for SenateLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SenateLink")
            .field("server_url", &self.server_url)
            .field("token", &"<redacted>")
            .field("presider", &self.presider)
            .finish()
    }
}

/// `[agents]`: the agent a session of each harness speaks as, unless the
/// session pins another ([`crate::agent`]): a name, or a path relative to
/// the person (`claude/codex`), which also selects the Senate it sits in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentNames {
    #[serde(default = "default_claude")]
    pub claude: RelPath,
    #[serde(default = "default_codex")]
    pub codex: RelPath,
}

fn default_claude() -> RelPath {
    RelPath::name(
        Name::new(Runtime::Claude.as_str()).unwrap_or_else(|_| unreachable!("a valid name")),
    )
}

fn default_codex() -> RelPath {
    RelPath::name(
        Name::new(Runtime::Codex.as_str()).unwrap_or_else(|_| unreachable!("a valid name")),
    )
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
    /// The agent for a session of `runtime`, relative to the person.
    pub fn for_runtime(&self, runtime: Runtime) -> &RelPath {
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
            .field("branch_prefix", &self.branch_prefix)
            .field("agents", &self.agents)
            .field("senates", &self.senates)
            .finish()
    }
}

/// What `senated --machine` hosts on this machine, read from the same
/// `senate.toml`: a local top Senate (`[local]`, for using Senate alone),
/// and a Senate one level down for each presiding agent (`[[preside]]`).
///
/// ```toml
/// [local]                 # alone: no Senate above, no server_url
/// speaker = "claude"      # alice/claude presides over it
/// listen = "127.0.0.1:7979"
///
/// [[preside]]
/// agent = "claude"        # alice/claude/* sit in the Senate it presides over
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Hosted {
    #[serde(default)]
    pub local: Option<LocalSenate>,
    #[serde(default)]
    pub preside: Vec<PresideSenate>,
}

/// `[local]`: a top Senate on this machine, with no Senate above.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct LocalSenate {
    /// The agent named its Speaker, relative to the person. Absent: nobody
    /// presides, and the Senate only keeps the log.
    #[serde(default)]
    pub speaker: Option<RelPath>,
    /// A loopback address to serve it on. Default: a free port, kept for
    /// the next start.
    #[serde(default)]
    pub listen: Option<String>,
}

/// `[[preside]]`: a Senate one level down, presided over by `agent`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PresideSenate {
    /// The presiding agent, relative to the person (`claude`).
    pub agent: RelPath,
    /// A loopback address to serve it on. Default: a free port, kept.
    #[serde(default)]
    pub listen: Option<String>,
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

    /// The rules: a local top Senate has no Senate above (`server_url` is
    /// empty); without one, the Senate above is configured; each presiding
    /// agent hosts one Senate; every address is loopback.
    pub fn validate(&self, config: &BridgeConfig) -> Result<(), String> {
        match (&self.local, config.server_url.is_empty()) {
            (Some(_), false) => {
                return Err(
                    "[local] is a top Senate on this machine, with no Senate above: drop \
                     server_url and token, or [local]"
                        .into(),
                );
            }
            (None, true) if config.senates.is_empty() => {
                return Err("set server_url and token (the Senate above), or [local]".into());
            }
            _ => {}
        }
        for (i, p) in self.preside.iter().enumerate() {
            if self.preside[..i].iter().any(|q| q.agent == p.agent) {
                return Err(format!("[[preside]] lists {} twice", p.agent));
            }
            if config
                .senates
                .iter()
                .any(|l| l.presider.as_ref() == Some(&p.agent))
            {
                return Err(format!(
                    "{} presides over a Senate in [[senates]] and in [[preside]]",
                    p.agent
                ));
            }
        }
        let listens = self
            .local
            .iter()
            .filter_map(|l| l.listen.as_deref())
            .chain(self.preside.iter().filter_map(|p| p.listen.as_deref()));
        for listen in listens {
            check_loopback(listen)?;
        }
        Ok(())
    }

    /// Whether this machine hosts any Senate.
    pub fn is_empty(&self) -> bool {
        self.local.is_none() && self.preside.is_empty()
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
            branch_prefix: default_branch_prefix(),
            agents: AgentNames::default(),
            senates: Vec::new(),
        }
    }

    /// Every Senate this machine serves: the one at the top-level
    /// `server_url` first (when set), then each `[[senates]]` entry.
    pub fn links(&self) -> Vec<SenateLink> {
        let mut links = Vec::with_capacity(1 + self.senates.len());
        if !self.server_url.is_empty() {
            links.push(SenateLink {
                server_url: self.server_url.clone(),
                token: self.token.clone(),
                presider: None,
            });
        }
        links.extend(self.senates.iter().cloned());
        links
    }

    /// This configuration narrowed to one of its Senates: `link`'s server
    /// and token at the top level, no other Senates.
    pub fn for_link(&self, link: &SenateLink) -> Self {
        Self {
            server_url: link.server_url.clone(),
            token: link.token.clone(),
            senates: Vec::new(),
            ..self.clone()
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
    /// --machine`, which fills in the Senates it hosts itself before the
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

    /// At least one Senate; each with an http(s) URL and a token; no two
    /// presided over by the same agent (the path of an agent selects one
    /// Senate).
    pub fn validate(&self) -> Result<(), String> {
        if self.server_url.is_empty() && self.senates.is_empty() {
            return Err("server_url is empty and no [[senates]] are listed".into());
        }
        let links = self.links();
        for link in &links {
            if !(link.server_url.starts_with("http://") || link.server_url.starts_with("https://"))
            {
                return Err(format!(
                    "server_url must start with http:// or https://, got {:?}",
                    link.server_url
                ));
            }
            if link.token.trim().is_empty() {
                return Err(format!("the token for {} is empty", link.server_url));
            }
        }
        for (i, link) in links.iter().enumerate() {
            if links[..i].iter().any(|l| l.presider == link.presider) {
                return Err(match &link.presider {
                    Some(p) => format!("two Senates are presided over by {p}"),
                    None => "two Senates seat the person's agents directly; give each \
                             [[senates]] entry its presider"
                        .into(),
                });
            }
        }
        Ok(())
    }

    /// How developer branches are named on this machine.
    pub fn branches(&self) -> BranchNaming {
        BranchNaming::new(&self.branch_prefix)
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
        let deep: BridgeConfig = toml::from_str(
            "server_url = \"http://x\"\ntoken = \"t\"\nperson = \"alice\"\n[agents]\ncodex = \"claude/codex\"\n",
        )
        .unwrap();
        assert_eq!(
            deep.agents.for_runtime(Runtime::Codex).to_string(),
            "claude/codex"
        );
        // An agent name is a senator name: validated where it is written.
        for bad in [
            "[agents]\nclaude = \"Claude\"",
            "[agents]\ncodex = \"a//b\"",
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
    fn several_senates_are_listed_with_their_presiders() {
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
        let links = c.links();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].presider, None);
        assert_eq!(
            links[1]
                .presider
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("claude")
        );
        let nested = c.for_link(&links[1]);
        assert_eq!(nested.server_url, "http://127.0.0.1:7979");
        assert!(nested.senates.is_empty());
        assert!(!format!("{c:?}").contains("nested-secret"));

        // Only [[senates]], no Senate above.
        let alone: BridgeConfig = toml::from_str(
            "person = \"alice\"\n[[senates]]\nserver_url = \"http://127.0.0.1:1\"\ntoken = \"x\"\n",
        )
        .unwrap();
        assert!(alone.validate().is_ok());
        assert_eq!(alone.links().len(), 1);

        for bad in [
            "person = \"a\"\n",
            "server_url = \"http://x\"\ntoken = \"t\"\nperson = \"a\"\n[[senates]]\nserver_url = \"http://y\"\ntoken = \"u\"\n",
            "person = \"a\"\n[[senates]]\npresider = \"c\"\nserver_url = \"http://y\"\ntoken = \"u\"\n[[senates]]\npresider = \"c\"\nserver_url = \"http://z\"\ntoken = \"u\"\n",
            "person = \"a\"\n[[senates]]\nserver_url = \"ftp://y\"\ntoken = \"u\"\n",
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
        assert_eq!(hosted.local.unwrap().speaker.unwrap().to_string(), "claude");
        let (config, hosted) = parse(
            "person = \"alice\"\nserver_url = \"https://s\"\ntoken = \"t\"\n[[preside]]\nagent = \"claude\"\n",
        );
        assert_eq!(hosted.validate(&config), Ok(()));
        assert_eq!(hosted.preside[0].agent.to_string(), "claude");

        for bad in [
            // Alone, yet a Senate above.
            "person = \"a\"\nserver_url = \"https://s\"\ntoken = \"t\"\n[local]\n",
            // Neither.
            "person = \"a\"\n[[preside]]\nagent = \"claude\"\n",
            // Twice.
            "person = \"a\"\n[local]\n[[preside]]\nagent = \"c\"\n[[preside]]\nagent = \"c\"\n",
            // Off the machine.
            "person = \"a\"\n[local]\nlisten = \"0.0.0.0:7979\"\n",
        ] {
            let (config, hosted) = parse(bad);
            assert!(hosted.validate(&config).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_bad_url() {
        let c: BridgeConfig =
            toml::from_str("server_url = \"ftp://x\"\ntoken = \"t\"\nperson = \"a\"\n").unwrap();
        assert!(c.validate().is_err());
    }
}
