//! `senate-server.toml`. Every key is documented in
//! `config/senate-server.example.toml`.
//!
//! Two separate things are configured about people. The **roster**
//! (`[roster]`, a [`RosterConfig`]) says who exists: handles, names,
//! accounts on outside platforms, optionally agents seated from the start
//! and the presider of a nested Senate. The **tokens** (`[[tokens]]`) say
//! how a bearer token maps to one of those people. The first is the default
//! roster source, the second the default [`Authenticate`](crate::Authenticate);
//! a private layer replaces either without the other.
//!
//! Unknown top-level keys are not errors: sections other crates own (a
//! gateway's, the Speaker's) are kept in [`ServerConfig::extra`] for the
//! binary to hand on.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use senate_core::api::SSE_PING_SECS;
use senate_core::{Locale, Name, Roster, RosterConfig};
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parsing config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Clone, Deserialize)]
pub struct ServerConfig {
    /// Address the HTTP API binds, e.g. `"0.0.0.0:7878"` or `"127.0.0.1:0"`.
    pub listen: String,
    /// `"memory:"`, a `postgres://` URL, or `"sqlite:<path>"`.
    pub database_url: String,
    /// The language text for people is rendered in where the server renders
    /// any (see [`ServerHub::locale`](crate::ServerHub::locale)). Defaults to
    /// [`Locale::default`].
    #[serde(default)]
    pub locale: Locale,
    /// Where server-side components keep local state (the Speaker's
    /// workspaces live under `<data_dir>/speaker/`).
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// SSE keepalive interval. Defaults to [`SSE_PING_SECS`]; tests shorten it.
    #[serde(default = "default_ping_secs")]
    pub sse_ping_secs: u64,
    /// How long an open stream counts as a connected agent without a fresh
    /// heartbeat. Defaults to [`DEFAULT_PRESENCE_GRACE_SECS`]; tests shorten it.
    #[serde(default = "default_presence_grace_secs")]
    pub presence_grace_secs: u64,
    /// Seconds without a heartbeat before an `online` agent is
    /// `disconnected`. Defaults to
    /// [`PRESENCE_DISCONNECTED_SECS`](senate_core::continuity::PRESENCE_DISCONNECTED_SECS)
    /// (120); tests shorten it.
    #[serde(default = "default_presence_disconnected_secs")]
    pub presence_disconnected_secs: u64,
    /// Seconds without a heartbeat before an agent's lease expires and it is
    /// `absent`. Defaults to [`LEASE_SECS`](senate_core::continuity::LEASE_SECS)
    /// (900); must exceed `presence_disconnected_secs`.
    #[serde(default = "default_lease_secs")]
    pub lease_secs: u64,
    /// The people this Senate knows, optionally the agents already seated,
    /// and the presider of a nested Senate (absent for the top Senate).
    #[serde(default)]
    pub roster: RosterConfig,
    /// Bearer tokens, each naming a person in the roster.
    #[serde(default)]
    pub tokens: Vec<TokenConfig>,
    /// Every other top-level key, e.g. a gateway's or the Speaker's section.
    #[serde(flatten)]
    pub extra: toml::Table,
}

/// One `[[tokens]]` entry: a bearer token and the person it names. Exactly
/// one of `token` / `token_sha256` is set.
#[derive(Clone, Deserialize)]
pub struct TokenConfig {
    pub person: Name,
    /// Plaintext bearer token. Dev only: prefer `token_sha256` anywhere real.
    #[serde(default)]
    pub token: Option<String>,
    /// Lowercase or uppercase hex SHA-256 of the bearer token.
    #[serde(default)]
    pub token_sha256: Option<String>,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

fn default_ping_secs() -> u64 {
    SSE_PING_SECS
}

/// The default `presence_grace_secs`: two and a half of the bridge's 60 s
/// heartbeat intervals, so one late or lost heartbeat does not flap presence.
pub const DEFAULT_PRESENCE_GRACE_SECS: u64 = 150;

fn default_presence_grace_secs() -> u64 {
    DEFAULT_PRESENCE_GRACE_SECS
}

fn default_presence_disconnected_secs() -> u64 {
    senate_core::continuity::PRESENCE_DISCONNECTED_SECS
}

fn default_lease_secs() -> u64 {
    senate_core::continuity::LEASE_SECS
}

impl ServerConfig {
    /// Read, parse and validate a config file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// Parse and validate config text.
    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// A config for tests and embedding: `memory:` store, ephemeral port,
    /// the given roster and tokens.
    pub fn new(roster: RosterConfig, tokens: Vec<TokenConfig>) -> Self {
        Self {
            listen: "127.0.0.1:0".into(),
            database_url: senate_log::MEMORY_URL.into(),
            locale: Locale::default(),
            data_dir: default_data_dir(),
            sse_ping_secs: SSE_PING_SECS,
            presence_grace_secs: DEFAULT_PRESENCE_GRACE_SECS,
            presence_disconnected_secs: default_presence_disconnected_secs(),
            lease_secs: default_lease_secs(),
            roster,
            tokens,
            extra: toml::Table::new(),
        }
    }

    /// The checks on the timing keys alone: what a server built with its own
    /// authentication and roster still reads from the configuration.
    pub fn validate_intervals(&self) -> Result<(), ConfigError> {
        if self.sse_ping_secs == 0 {
            return Err(ConfigError::Invalid(
                "sse_ping_secs must be at least 1".into(),
            ));
        }
        if self.presence_grace_secs == 0 {
            return Err(ConfigError::Invalid(
                "presence_grace_secs must be at least 1".into(),
            ));
        }
        if self.presence_disconnected_secs == 0 {
            return Err(ConfigError::Invalid(
                "presence_disconnected_secs must be at least 1".into(),
            ));
        }
        if self.lease_secs <= self.presence_disconnected_secs {
            return Err(ConfigError::Invalid(format!(
                "lease_secs ({}) must exceed presence_disconnected_secs ({})",
                self.lease_secs, self.presence_disconnected_secs
            )));
        }
        Ok(())
    }

    /// Every check: the timing keys, the roster's rules, and that each token
    /// names a person in the roster.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_intervals()?;
        let roster = self.roster()?;
        for token in &self.tokens {
            token.digest()?;
            if roster.senior(&token.person).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "a token names {}, who is not in the roster",
                    token.person
                )));
            }
        }
        Ok(())
    }

    /// The configured roster, with the roster's own rules applied (people
    /// listed once, agents under a listed person at this Senate's level).
    pub fn roster(&self) -> Result<Roster, ConfigError> {
        Roster::try_from(self.roster.clone())
            .map_err(|e| ConfigError::Invalid(format!("roster: {e}")))
    }

    pub fn sse_ping_interval(&self) -> Duration {
        Duration::from_secs(self.sse_ping_secs)
    }

    pub fn presence_grace(&self) -> Duration {
        Duration::from_secs(self.presence_grace_secs)
    }

    pub(crate) fn lease_timing(&self) -> crate::actors::lease::LeaseTiming {
        crate::actors::lease::LeaseTiming {
            disconnected: Duration::from_secs(self.presence_disconnected_secs),
            lease: Duration::from_secs(self.lease_secs),
        }
    }
}

impl TokenConfig {
    /// A plaintext token naming `person`.
    pub fn with_token(person: Name, token: impl Into<String>) -> Self {
        Self {
            person,
            token: Some(token.into()),
            token_sha256: None,
        }
    }

    /// The SHA-256 of this token, from whichever form is configured.
    pub(crate) fn digest(&self) -> Result<[u8; 32], ConfigError> {
        match (&self.token, &self.token_sha256) {
            (Some(token), None) if !token.is_empty() => Ok(crate::auth::sha256(token)),
            (None, Some(hex_digest)) => {
                let bytes = hex::decode(hex_digest.trim()).map_err(|e| {
                    ConfigError::Invalid(format!(
                        "token for {}: token_sha256 is not hex: {e}",
                        self.person
                    ))
                })?;
                bytes.try_into().map_err(|_| {
                    ConfigError::Invalid(format!(
                        "token for {}: token_sha256 must be 32 bytes (64 hex chars)",
                        self.person
                    ))
                })
            }
            (Some(_), None) => Err(ConfigError::Invalid(format!(
                "token for {}: token is empty",
                self.person
            ))),
            _ => Err(ConfigError::Invalid(format!(
                "token for {}: set exactly one of token or token_sha256",
                self.person
            ))),
        }
    }
}

impl fmt::Debug for ServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerConfig")
            .field("listen", &self.listen)
            .field("database_url", &redact_url(&self.database_url))
            .field("data_dir", &self.data_dir)
            .field("sse_ping_secs", &self.sse_ping_secs)
            .field("presence_grace_secs", &self.presence_grace_secs)
            .field(
                "presence_disconnected_secs",
                &self.presence_disconnected_secs,
            )
            .field("lease_secs", &self.lease_secs)
            .field("locale", &self.locale)
            .field("roster", &self.roster)
            .field("tokens", &self.tokens)
            .field("extra", &self.extra.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl fmt::Debug for TokenConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenConfig")
            .field("person", &self.person)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field(
                "token_sha256",
                &self.token_sha256.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// `postgres://user:secret@host/db` → `postgres://user:***@host/db`.
pub(crate) fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return url.to_string();
    };
    match userinfo.split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***@{host}"),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const EXAMPLE: &str = include_str!("../config/senate-server.example.toml");

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn example_config_parses_and_keeps_extra_sections() {
        let config = ServerConfig::from_toml_str(EXAMPLE).unwrap();
        assert_eq!(config.tokens.len(), 2);
        let roster = config.roster().unwrap();
        let alice = roster.resolve("Alice").unwrap();
        assert_eq!(alice.handle.as_str(), "alice");
        assert_eq!(alice.account_ids("chat").collect::<Vec<_>>(), ["u_alice"]);
        assert!(roster.senior(&name("bob")).is_some());
        assert_eq!(
            roster.agents().len(),
            1,
            "alice/claude is seated from the start"
        );
        assert_eq!(roster.presider(), None);
        assert_eq!(config.sse_ping_secs, SSE_PING_SECS);
        assert_eq!(config.presence_grace(), Duration::from_secs(150));
        assert_eq!(config.locale, Locale::default());
        assert!(config.extra.contains_key("gateway"));
    }

    #[test]
    fn the_locale_and_the_presider_come_from_config() {
        let config = ServerConfig::from_toml_str(
            r#"
            listen = "127.0.0.1:0"
            database_url = "memory:"
            locale = "en"

            [roster]
            presider = "alice/claude"
            agents = ["alice/claude/reviewer"]

            [[roster.seniors]]
            handle = "alice"
            "#,
        )
        .unwrap();
        assert_eq!(config.locale, Locale::En);
        let roster = config.roster().unwrap();
        assert_eq!(
            roster.presider().map(ToString::to_string).as_deref(),
            Some("alice/claude")
        );
        let bad = "listen = 'x'\ndatabase_url = 'memory:'\nlocale = 'fr'";
        assert!(matches!(
            ServerConfig::from_toml_str(bad),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn intervals_must_be_positive() {
        for key in [
            "sse_ping_secs",
            "presence_grace_secs",
            "presence_disconnected_secs",
        ] {
            let text = format!("listen = 'x'\ndatabase_url = 'memory:'\n{key} = 0");
            assert!(
                matches!(
                    ServerConfig::from_toml_str(&text),
                    Err(ConfigError::Invalid(_))
                ),
                "accepted {key} = 0"
            );
        }
        let text = "listen = 'x'\ndatabase_url = 'memory:'\npresence_grace_secs = 2";
        let config = ServerConfig::from_toml_str(text).unwrap();
        assert_eq!(config.presence_grace(), Duration::from_secs(2));
        assert_eq!(config.lease_secs, 900);
        let text = "listen = 'x'\ndatabase_url = 'memory:'\nlease_secs = 120";
        assert!(
            matches!(
                ServerConfig::from_toml_str(text),
                Err(ConfigError::Invalid(_))
            ),
            "the lease must outlast the disconnect"
        );
    }

    #[test]
    fn unknown_sections_are_kept_not_rejected() {
        let config = ServerConfig::from_toml_str(
            r#"
            listen = "127.0.0.1:0"
            database_url = "memory:"

            [gateway]
            addr = "http://127.0.0.1:50051"

            [speaker]
            answer_fallback_secs = 60
            "#,
        )
        .unwrap();
        let gateway = config.extra["gateway"].as_table().unwrap();
        assert_eq!(gateway["addr"].as_str(), Some("http://127.0.0.1:50051"));
        let speaker = config.extra["speaker"].as_table().unwrap();
        assert_eq!(speaker["answer_fallback_secs"].as_integer(), Some(60));
        assert!(!config.extra.contains_key("listen"));
        assert_eq!(config.data_dir, PathBuf::from("data"));
    }

    #[test]
    fn tokens_are_validated() {
        let roster = "[roster]\n[[roster.seniors]]\nhandle = \"alice\"\n";
        let bad = [
            // neither
            "[[tokens]]\nperson = \"alice\"",
            // both
            "[[tokens]]\nperson = \"alice\"\ntoken = \"t\"\ntoken_sha256 = \"00\"",
            // not hex
            "[[tokens]]\nperson = \"alice\"\ntoken_sha256 = \"zz\"",
            // wrong length
            "[[tokens]]\nperson = \"alice\"\ntoken_sha256 = \"abcd\"",
            // empty
            "[[tokens]]\nperson = \"alice\"\ntoken = \"\"",
            // a person the roster does not list
            "[[tokens]]\nperson = \"mallory\"\ntoken = \"t\"",
        ];
        for tokens in bad {
            let text =
                format!("listen = \"127.0.0.1:0\"\ndatabase_url = \"memory:\"\n{tokens}\n{roster}");
            assert!(
                matches!(
                    ServerConfig::from_toml_str(&text),
                    Err(ConfigError::Invalid(_))
                ),
                "accepted: {tokens}"
            );
        }
        let bad_name = "listen = \"x\"\ndatabase_url = \"memory:\"\n[[tokens]]\nperson = \"Al/ice\"\ntoken = \"t\"";
        assert!(matches!(
            ServerConfig::from_toml_str(bad_name),
            Err(ConfigError::Parse(_))
        ));
    }

    #[test]
    fn a_roster_that_breaks_its_rules_is_refused() {
        // An agent two levels down does not sit in the top Senate; a person
        // listed twice; an agent whose person is not listed.
        for roster in [
            "agents = [\"alice/claude/reviewer\"]\n[[roster.seniors]]\nhandle = \"alice\"",
            "[[roster.seniors]]\nhandle = \"alice\"\n[[roster.seniors]]\nhandle = \"alice\"",
            "agents = [\"bob/claude\"]\n[[roster.seniors]]\nhandle = \"alice\"",
        ] {
            let text = format!("listen = 'x'\ndatabase_url = 'memory:'\n[roster]\n{roster}");
            assert!(
                matches!(
                    ServerConfig::from_toml_str(&text),
                    Err(ConfigError::Invalid(_))
                ),
                "accepted: {roster}"
            );
        }
    }

    #[test]
    fn debug_redacts_secrets() {
        let mut config = ServerConfig::new(
            RosterConfig::default(),
            vec![TokenConfig::with_token(name("alice"), "s3cret-token")],
        );
        config.database_url = "postgres://senate:pw@db:5432/senate".into();
        let debug = format!("{config:?}");
        assert!(!debug.contains("s3cret-token"), "{debug}");
        assert!(!debug.contains(":pw@"), "{debug}");
        assert!(debug.contains("postgres://senate:***@db:5432/senate"));
    }
}
