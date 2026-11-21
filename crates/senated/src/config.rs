//! `senated.toml`: the server's keys at the top level, then the sections the
//! assembly reads. Every key is documented in `config/senated.example.toml`.
//!
//! - The top level and `[roster]` / `[[tokens]]` are the server's
//!   ([`ServerConfig`]).
//! - `[model]` is how the built-in session reaches its model
//!   ([`ModelSettings`]): provider, model id, the NAME of the variable that
//!   holds the key, timeouts. The built-in session runs when a complete
//!   model is configured and something refers to it (see
//!   [`Assembly`](crate::Assembly)).
//! - The built-in session's behaviour is in `secretary.toml` beside this
//!   file, and `<committee>.secretary.toml` there for one committee
//!   ([`senate_speaker::secretary`] has the layers and the reloading).
//! - `[speaker]` (or `[secretary]`), read for compatibility, is read as if
//!   it were `secretary.toml` ([`SpeakerConfig`]; a one-time note says so,
//!   and that `secretary.toml` wins when both exist); model keys found in
//!   it are still read, with a note pointing at `[model]`. Its `data_dir`
//!   and `locale` default to the top-level ones, so a deployment names its
//!   language once.
//! - `[work]` gives the built-in session hands ([`WorkSection`]): a runner
//!   and a forge. It needs a model (`[model]`, or `[speaker]`). The rules
//!   for work (`[work]` in `secretary.toml`, `[speaker.work]`) default to
//!   the runner's `allowed_repos`.
//!
//! ```toml
//! [model]
//! provider = "anthropic"
//! model = "claude-sonnet-4-5"
//! api_key_env = "ANTHROPIC_API_KEY"   # the variable's NAME, never the key
//! deadline_secs = 45
//! triage_deadline_secs = 30
//! token_budget = 80000
//! ```
//!
//! An OpenAI-compatible gateway is reached by `base_url`, spoken to in
//! Chat Completions unless `wire = "responses"` says it serves OpenAI's
//! `v1/responses` (as some OpenAI-compatible gateways do):
//!
//! ```toml
//! [model]
//! base_url = "https://gateway.example.com/v1"
//! model = "gpt-6.1"
//! api_key_env = "GATEWAY_API_KEY"
//! wire = "responses"
//! ```
//!
//! Any other top-level section is left for a private layer to read from
//! [`ServerConfig::extra`]; the `senated` binary warns about it.
//!
//! # The built-in session's configuration on a machine
//!
//! `senated --machine` keeps it in the Senate home: `secretary.toml` and
//! `<committee>.secretary.toml` in `~/.senate/`, `[model]` (and, for
//! compatibility, `[secretary]` / `[speaker]`) in `senate.toml`. Load it
//! with [`load_secretary_home`] and hand it to
//! [`Assembly::secretary`](crate::Assembly::secretary):
//!
//! ```no_run
//! # fn f(home: &std::path::Path, data_dir: &std::path::Path) -> Result<(), senated::SenatedConfigError> {
//! let secretary = std::sync::Arc::new(senated::config::load_secretary_home(home, data_dir)?);
//! // Assembly::new(config).secretary(secretary) …
//! # let _ = secretary; Ok(()) }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use senate_core::Locale;
use senate_server::{ConfigError, ServerConfig};
use senate_speaker::secretary::{COMPAT_SECTIONS, SECRETARY_KEYS, table_of_config};
use senate_speaker::{BaseLayer, ModelSettings, SecretaryConfigs, SpeakerConfig, WorkConfig};
use senate_work::{DockerRunner, DockerRunnerConfig, Forge, GhForge, GhForgeConfig, Runner};
use serde::Deserialize;

/// The sections this crate reads; anything else in [`ServerConfig::extra`]
/// belongs to someone else.
pub const KNOWN_SECTIONS: [&str; 5] = ["model", "secretary", "speaker", "work", "chat_gateway"];

#[derive(Debug, thiserror::Error)]
pub enum SenatedConfigError {
    #[error(transparent)]
    Server(#[from] ConfigError),
    #[error("[{section}]: {source}")]
    Section {
        section: &'static str,
        #[source]
        source: toml::de::Error,
    },
    #[error("[{0}] must be a table")]
    NotATable(&'static str),
    #[error(
        "[work] needs the built-in session: configure its model in [model] (only the built-in session runs work)"
    )]
    WorkWithoutSpeaker,
    #[error("{0}")]
    Secretary(String),
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// `[work]`: what does the Speaker's coding work.
///
/// ```toml
/// [work.runner]
/// image = "senate-runner:latest"
/// work_root = "/var/lib/senate/runs"
/// allowed_repos = ["example/project"]
///
/// [work.forge]
/// kind = "github"
/// token_file = "/run/secrets/github-token"
/// repos = ["example/project"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkSection {
    pub runner: DockerRunnerConfig,
    #[serde(default)]
    pub forge: Option<ForgeSection>,
}

/// `[work.forge]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ForgeSection {
    /// GitHub through `gh`.
    Github(GhForgeConfig),
    /// No forge: the pushed branch is the deliverable.
    None,
}

impl WorkSection {
    /// The runner and the forge this section names. `None` for the forge:
    /// the branch is the deliverable.
    pub fn hands(&self) -> (Arc<dyn Runner>, Option<Arc<dyn Forge>>) {
        let runner: Arc<dyn Runner> = Arc::new(DockerRunner::new(self.runner.clone()));
        let forge: Option<Arc<dyn Forge>> = match &self.forge {
            Some(ForgeSection::Github(config)) => Some(Arc::new(GhForge::new(config.clone()))),
            Some(ForgeSection::None) | None => None,
        };
        (runner, forge)
    }
}

/// Everything `senated.toml` says.
///
/// `[model]` stays in [`ServerConfig::extra`] (checked at load,
/// [`SenatedConfig::model`]); the built-in session's own files are read
/// beside the configuration file ([`SenatedConfig::secretary_base`],
/// [`secretary_dir`]).
#[derive(Clone)]
pub struct SenatedConfig {
    pub server: ServerConfig,
    /// `[speaker]` (or `[secretary]`), read for compatibility as if it were
    /// `secretary.toml`; `None` when neither is there. It also configures
    /// a model (the environment's, unless it names one), as it always did.
    pub speaker: Option<SpeakerConfig>,
    /// `[work]`; `None`: the built-in session does no coding work.
    pub work: Option<WorkSection>,
}

impl SenatedConfig {
    /// Read, parse and validate a config file.
    pub fn load(path: &Path) -> Result<Self, SenatedConfigError> {
        Self::from_server(ServerConfig::load(path)?)
    }

    /// Parse and validate config text.
    pub fn from_toml_str(text: &str) -> Result<Self, SenatedConfigError> {
        Self::from_server(ServerConfig::from_toml_str(text)?)
    }

    /// Read `[speaker]` and `[work]` out of a server config's other
    /// sections.
    pub fn from_server(server: ServerConfig) -> Result<Self, SenatedConfigError> {
        let work: Option<WorkSection> = server
            .extra
            .get("work")
            .cloned()
            .map(|value| {
                value
                    .try_into()
                    .map_err(|source| SenatedConfigError::Section {
                        section: "work",
                        source,
                    })
            })
            .transpose()?;
        let section = compat_section(&server.extra);
        let speaker = match section.and_then(|s| server.extra.get(s)).cloned() {
            None => None,
            Some(toml::Value::Table(mut table)) => {
                for key in SECRETARY_KEYS {
                    table.remove(key);
                }
                // What the Speaker does not say is the deployment's.
                table.entry("data_dir").or_insert_with(|| {
                    toml::Value::String(server.data_dir.to_string_lossy().into_owned())
                });
                table
                    .entry("locale")
                    .or_insert_with(|| toml::Value::String(server.locale.as_str().to_owned()));
                let mut speaker: SpeakerConfig =
                    toml::Value::Table(table).try_into().map_err(|source| {
                        SenatedConfigError::Section {
                            section: section.unwrap_or("speaker"),
                            source,
                        }
                    })?;
                if let Some(work) = &work
                    && speaker.work.is_none()
                {
                    speaker.work = Some(WorkConfig::new(work.runner.allowed_repos.clone()));
                }
                Some(speaker)
            }
            Some(_) => {
                return Err(SenatedConfigError::NotATable(section.unwrap_or("speaker")));
            }
        };
        // `[model]` and the compatibility table say only what they may.
        BaseLayer::of_main_table(&server.extra, &server.data_dir, server.locale)
            .map_err(SenatedConfigError::Secretary)?;
        if work.is_some() && speaker.is_none() && !server.extra.contains_key("model") {
            return Err(SenatedConfigError::WorkWithoutSpeaker);
        }
        Ok(Self {
            server,
            speaker,
            work,
        })
    }

    /// `[model]`, when the configuration has one (checked at load).
    pub fn model(&self) -> Option<ModelSettings> {
        self.server
            .extra
            .get("model")
            .and_then(|m| m.clone().try_into().ok())
    }

    /// The compatibility table the configuration holds: `secretary` or
    /// `speaker`.
    pub fn compat_section(&self) -> Option<&'static str> {
        compat_section(&self.server.extra)
    }

    /// What this configuration says for the built-in session: the layer
    /// under its files ([`BaseLayer`]). `main_file` names this
    /// configuration in what is shown.
    pub fn secretary_base(&self, main_file: Option<&Path>) -> BaseLayer {
        let (data_dir, locale) = match &self.speaker {
            Some(speaker) => (speaker.data_dir.clone(), speaker.locale),
            None => (self.server.data_dir.clone(), self.server.locale),
        };
        let mut base = BaseLayer::new(data_dir, locale);
        if let Some(model) = self.model() {
            base = base.with_model(model);
        }
        if let Some(speaker) = &self.speaker {
            let section = self.compat_section().unwrap_or("speaker");
            let mut table = table_of_config(speaker);
            // What only a secretary file says, as written.
            if let Some(toml::Value::Table(raw)) = self.server.extra.get(section) {
                for key in SECRETARY_KEYS {
                    if let Some(value) = raw.get(key) {
                        table.insert(key.to_owned(), value.clone());
                    }
                }
            }
            base = base.with_compat(section, table);
        }
        if let Some(work) = &self.work {
            // The rules for work default to the runner's repositories.
            let rules = WorkConfig::new(work.runner.allowed_repos.clone());
            if let Ok(table) = toml::Table::try_from(&rules) {
                base.deployment
                    .insert("work".into(), toml::Value::Table(table));
            }
        }
        if let Some(file) = main_file {
            base = base.with_main_file(file);
        }
        base
    }

    /// Top-level sections neither the server nor this crate reads: a private
    /// layer's, or a typo.
    pub fn other_sections(&self) -> Vec<&str> {
        self.server
            .extra
            .keys()
            .map(String::as_str)
            .filter(|key| !KNOWN_SECTIONS.contains(key))
            .collect()
    }
}

/// The table of the main configuration read for compatibility: `[secretary]`
/// before `[speaker]` (both at once is refused at load).
fn compat_section(extra: &toml::Table) -> Option<&'static str> {
    COMPAT_SECTIONS.into_iter().find(|s| extra.contains_key(*s))
}

/// Where `secretary.toml` sits for `senated --config <file>`: beside it.
pub fn secretary_dir(config_file: &Path) -> PathBuf {
    match config_file.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// The built-in session's configuration for `senated --config <file>`:
/// `secretary.toml` beside `file`, over what `config` (read from `file`)
/// says.
pub fn load_secretary(config: &SenatedConfig, file: &Path) -> SecretaryConfigs {
    SecretaryConfigs::new(Some(secretary_dir(file)), config.secretary_base(Some(file)))
}

/// The built-in session's configuration on a developer machine
/// (`senated --machine`): `secretary.toml` and `<committee>.secretary.toml`
/// in the Senate home `home`, over `senate.toml`'s `[model]` (and, read
/// for compatibility, its `[secretary]` or `[speaker]` table) and its
/// `locale`; the model's workspaces under `data_dir` (the hosted Senate's).
/// No `senate.toml`, or none of these in it, is valid: the defaults.
pub fn load_secretary_home(
    home: &Path,
    data_dir: &Path,
) -> Result<SecretaryConfigs, SenatedConfigError> {
    let main = home.join("senate.toml");
    let table: toml::Table = match std::fs::read_to_string(&main) {
        Ok(text) => toml::from_str(&text)
            .map_err(|e| SenatedConfigError::Secretary(format!("{}: {e}", main.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(source) => return Err(SenatedConfigError::Read { path: main, source }),
    };
    let locale = table
        .get("locale")
        .and_then(|v| v.as_str())
        .and_then(|l| l.parse::<Locale>().ok())
        .unwrap_or_default();
    let base = BaseLayer::of_main_table(&table, data_dir, locale)
        .map_err(|e| SenatedConfigError::Secretary(format!("{}: {e}", main.display())))?
        .with_main_file(&main);
    Ok(SecretaryConfigs::new(Some(home.to_path_buf()), base))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use senate_core::Locale;

    use super::*;

    fn config(extra: &str) -> Result<SenatedConfig, SenatedConfigError> {
        SenatedConfig::from_toml_str(&format!(
            r#"
listen = "127.0.0.1:0"
database_url = "memory:"
data_dir = "/var/lib/senate"
locale = "zh"

[[roster.principals]]
handle = "alice"

[[tokens]]
person = "alice"
token = "t"
{extra}
"#
        ))
    }

    /// The example config's commented-out sections, uncommented: `# key =`
    /// and `# [section]` lines become settings, `# # …` lines (comments
    /// inside the comment) are dropped.
    fn uncommented(example: &str, from: &str) -> String {
        let start = example.find(from).expect("the section in the example");
        example[start..]
            .lines()
            .filter(|l| !l.starts_with("# #"))
            .map(|l| l.strip_prefix("# ").unwrap_or(l.trim_start_matches('#')))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_example_config_and_its_optional_sections_parse() {
        const EXAMPLE: &str = include_str!("../../../config/senated.example.toml");
        let plain = SenatedConfig::from_toml_str(EXAMPLE).expect("the example as shipped");
        assert!(
            plain.speaker.is_none(),
            "no Speaker unless [speaker] is set"
        );
        assert!(plain.other_sections().is_empty());

        let start = EXAMPLE.find("# [speaker]").expect("a [speaker] example");
        let full = format!(
            "{}\n{}",
            &EXAMPLE[..start],
            uncommented(EXAMPLE, "# [speaker]")
        );
        let s = SenatedConfig::from_toml_str(&full).expect("the example, uncommented");
        let speaker = s.speaker.expect("[speaker]");
        let rules = speaker.work.expect("[speaker.work]");
        assert!(rules.allowed_repos.allows("example/project"));
        assert!(rules.lists_developer("refund-v2", "bob"));
        assert_eq!(rules.budget.max_turns, 200);
        let work = s.work.expect("[work]");
        assert_eq!(work.runner.image, "senate-runner:latest");
        assert!(matches!(work.forge, Some(ForgeSection::Github(_))));
        assert!(work.hands().1.is_some());

        // `[model]` alone, uncommented, with the roster's optional keys.
        let model_start = EXAMPLE.find("# [model]").expect("a [model] example");
        let model_end = model_start
            + EXAMPLE[model_start..]
                .find("\n# ----")
                .expect("the section ends");
        let roster_start = EXAMPLE.find("# [roster]\n").expect("a [roster] example");
        let roster_end = roster_start
            + EXAMPLE[roster_start..]
                .find("\n\n")
                .expect("the block ends");
        let with_model = format!(
            "{}\n{}\n{}\n{}",
            &EXAMPLE[..roster_start],
            uncommented(&EXAMPLE[..roster_end], "# [roster]"),
            &EXAMPLE[roster_end..model_start],
            uncommented(&EXAMPLE[..model_end], "# [model]"),
        );
        let m = SenatedConfig::from_toml_str(&with_model).expect("[model], uncommented");
        let model = m.model().expect("[model]");
        assert_eq!(model.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        let roster = m.server.roster().unwrap();
        assert!(roster.speaker().is_some());
    }

    #[test]
    fn no_sections_serve_the_log_alone() {
        let s = config("").unwrap();
        assert!(s.speaker.is_none());
        assert!(s.work.is_none());
    }

    #[test]
    fn the_speaker_takes_the_deployments_data_dir_and_locale() {
        let s = config("[speaker]\nanswer_fallback_secs = 30\n").unwrap();
        let speaker = s.speaker.unwrap();
        assert_eq!(speaker.data_dir, PathBuf::from("/var/lib/senate"));
        assert_eq!(speaker.locale, Locale::Zh);
        assert_eq!(speaker.answer_fallback_secs, 30);

        let own = config("[speaker]\ndata_dir = \"/srv/speaker\"\nlocale = \"en\"\n").unwrap();
        let speaker = own.speaker.unwrap();
        assert_eq!(speaker.data_dir, PathBuf::from("/srv/speaker"));
        assert_eq!(speaker.locale, Locale::En);
    }

    #[test]
    fn a_work_section_gives_the_speaker_hands() {
        let s = config(
            "[speaker]\n\
             [work.runner]\nimage = \"senate-runner:latest\"\nwork_root = \"/runs\"\n\
             allowed_repos = [\"o/r\"]\n\
             [work.forge]\nkind = \"github\"\ntoken_file = \"/run/secrets/gh\"\nrepos = [\"o/r\"]\n",
        )
        .unwrap();
        let work = s.work.clone().unwrap();
        assert!(
            matches!(&work.forge, Some(ForgeSection::Github(gh)) if gh.allowed_repos.allows("o/r"))
        );
        // The Speaker's rules default to the runner's repos.
        assert!(s.speaker.unwrap().work.unwrap().allowed_repos.allows("o/r"));

        let none = config(
            "[speaker]\n[speaker.work]\nallowed_repos = [\"a/b\"]\n\
             [work.runner]\nimage = \"i\"\nwork_root = \"/runs\"\n[work.forge]\nkind = \"none\"\n",
        )
        .unwrap();
        assert!(
            none.speaker
                .unwrap()
                .work
                .unwrap()
                .allowed_repos
                .allows("a/b")
        );
        assert!(none.work.unwrap().hands().1.is_none());

        assert!(matches!(
            config("[work.runner]\nimage = \"i\"\nwork_root = \"/r\"\n"),
            Err(SenatedConfigError::WorkWithoutSpeaker)
        ));
    }

    #[test]
    fn a_private_layers_section_is_kept_for_it() {
        let s = config("[speaker]\n[chat]\nendpoint = \"unix:///run/chat.sock\"\n").unwrap();
        assert_eq!(s.other_sections(), ["chat"]);
        assert!(s.server.extra["chat"].get("endpoint").is_some());
        assert!(matches!(
            SenatedConfig::from_toml_str(
                "speaker = 3\nlisten = \"127.0.0.1:0\"\ndatabase_url = \"memory:\"\n"
            ),
            Err(SenatedConfigError::NotATable("speaker"))
        ));
        assert!(matches!(
            config("[speaker]\nmode = \"loud\"\n"),
            Err(SenatedConfigError::Section {
                section: "speaker",
                ..
            })
        ));
    }
}
