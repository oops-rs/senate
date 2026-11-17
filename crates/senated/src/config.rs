//! `senated.toml`: the server's keys at the top level, then the sections the
//! assembly reads. Every key is documented in `config/senated.example.toml`.
//!
//! - The top level and `[roster]` / `[[tokens]]` are the server's
//!   ([`ServerConfig`]).
//! - `[speaker]` turns the built-in Speaker on ([`SpeakerConfig`]). Its
//!   `data_dir` and `locale` default to the top-level ones, so a deployment
//!   names its language once.
//! - `[work]` gives the Speaker hands ([`WorkSection`]): a runner and a
//!   forge. It needs `[speaker]`. The Speaker's own rules for work
//!   (`[speaker.work]`) default to the runner's `allowed_repos`.
//!
//! Any other top-level section is left for a private layer to read from
//! [`ServerConfig::extra`]; the `senated` binary warns about it.

use std::path::Path;
use std::sync::Arc;

use senate_server::{ConfigError, ServerConfig};
use senate_speaker::{SpeakerConfig, WorkConfig};
use senate_work::{DockerRunner, DockerRunnerConfig, Forge, GhForge, GhForgeConfig, Runner};
use serde::Deserialize;

/// The sections this crate reads; anything else in [`ServerConfig::extra`]
/// belongs to someone else.
pub const KNOWN_SECTIONS: [&str; 2] = ["speaker", "work"];

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
    #[error("[work] needs [speaker]: only the Speaker runs work")]
    WorkWithoutSpeaker,
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
#[derive(Clone)]
pub struct SenatedConfig {
    pub server: ServerConfig,
    /// `[speaker]`; `None`: no built-in Speaker.
    pub speaker: Option<SpeakerConfig>,
    /// `[work]`; `None`: the Speaker does no coding work.
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
        let speaker = match server.extra.get("speaker").cloned() {
            None => None,
            Some(toml::Value::Table(mut table)) => {
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
                            section: "speaker",
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
            Some(_) => return Err(SenatedConfigError::NotATable("speaker")),
        };
        if work.is_some() && speaker.is_none() {
            return Err(SenatedConfigError::WorkWithoutSpeaker);
        }
        Ok(Self {
            server,
            speaker,
            work,
        })
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
