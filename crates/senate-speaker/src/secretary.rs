//! The built-in session's configuration: the shared `[model]`,
//! `secretary.toml`, and `<committee>.secretary.toml`.
//!
//! # Where the files are
//!
//! `secretary.toml` sits in one directory: the Senate home on a machine
//! (`~/.senate/secretary.toml`), and beside the configuration file for
//! `senated --config <file>`. A committee's own file is
//! `<committee>.secretary.toml` in the same directory; a file whose prefix
//! is not a committee id is never looked at. No file at all is valid.
//!
//! # Layers
//!
//! Each value is taken from the highest layer that says it, key by key
//! (tables such as `[work]` or `[locales]` merge the same way, inside):
//!
//! 1. the built-in defaults ([`SpeakerConfig`]'s);
//! 2. the main configuration: its `[model]`, the deployment's `data_dir`
//!    and `locale`, and, read for compatibility, an old `[speaker]` (or a
//!    `[secretary]`) table as if it were `secretary.toml` ([`BaseLayer`]);
//! 3. `secretary.toml`;
//! 4. `<committee>.secretary.toml`, for that committee only. It may not set
//!    `name` (one Senate, one built-in session): such a file is refused as
//!    a whole, with a message saying why.
//!
//! When `secretary.toml` exists, the compatibility table's behaviour is not
//! read (a one-time note says so); model keys found inside it still are,
//! as the layer just below `[model]`, with a note pointing at `[model]`.
//!
//! The model ([`ModelSettings`]) is layered the same way. A layer whose
//! resulting model is incomplete or invalid (an unknown provider, a key
//! variable that is not set, a base URL that is not one) falls back to the
//! next layer down, and the error is reported against that file. An empty
//! `[model]` is complete: basis then resolves the provider and the key from
//! its usual environment (`ANTHROPIC_API_KEY`, …).
//!
//! ```toml
//! # secretary.toml
//! name = "secretary"                # its senator name; applied at the next start
//! mode = "agent"                    # every [speaker] key reads here
//! confirmation_ttl_secs = 600
//! bot_name = "Speaker"              # how people address it in chat
//! instructions = "Answer in short sentences."
//! instructions_file = "secretary.md"   # relative to this file
//! duties = ["route", "forward", "refuse", "cover", "triage"]   # default: all
//!
//! [model]                           # overrides the main configuration's [model], key by key
//! provider = "anthropic"
//! model = "claude-sonnet-4-5"
//! api_key_env = "ANTHROPIC_API_KEY" # the variable's NAME; the key is never written here
//! ```
//!
//! ```toml
//! # refund-v2.secretary.toml
//! instructions = "This committee ships on Fridays; never promise a date."
//! [model]
//! model = "claude-opus-4-1"         # this committee only
//! ```
//!
//! # Reloading
//!
//! Before the built-in session acts on a committee it calls
//! [`SecretaryConfigs::refresh`] for it, which compares the modification
//! time and size of the files that make that committee's configuration
//! (`secretary.toml`, the committee's file, and any `instructions_file`)
//! with what they were when it last read them: a few `stat` calls, no
//! watcher thread, and a change (an edit, a new file, a removed file) takes
//! effect on the next turn on that committee. A file that cannot be read or
//! understood is never half-applied: its last good content stays in force
//! (none, if it never read well), the error is logged once per change with
//! the file and the line, and it is listed by [`SecretaryConfigs::problems`]
//! until it is fixed or removed. A changed `name` is not applied until the
//! next start; the log says so once.
//!
//! # Showing it
//!
//! [`SecretaryConfigs::explain`] says, for one committee, every value in
//! force and which layer it came from ([`Source`]); [`Shown::render`] writes
//! it for people in their language. A model's key is never shown: only the
//! name of the variable that holds it, and whether it is set.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;

use senate_core::{CommitteeId, Locale, SenatorName};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::SpeakerConfig;

/// The built-in session's name when no configuration gives one.
pub const DEFAULT_NAME: &str = "secretary";
/// The Senate-wide file.
pub const SECRETARY_FILE: &str = "secretary.toml";
/// A committee's file is `<committee>` followed by this.
pub const COMMITTEE_FILE_SUFFIX: &str = ".secretary.toml";
/// The tables of a main configuration read as if they were
/// `secretary.toml`, for compatibility.
pub const COMPAT_SECTIONS: [&str; 2] = ["secretary", "speaker"];

/// What the built-in session may do on the Speaker's desk without leaving it
/// for the Speaker. A duty left out is left on the desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Duty {
    /// Route a message the server put on the desk because several agents
    /// could get it.
    Route,
    /// Forward a plain question that crosses into another committee.
    Forward,
    /// Refuse a message that crosses into another committee, with a reason.
    Refuse,
    /// Answer for an agent that is away, from its own notes.
    Cover,
    /// Judge task roots (yours to answer / needs the person).
    Triage,
}

impl Duty {
    pub const ALL: [Self; 5] = [
        Self::Route,
        Self::Forward,
        Self::Refuse,
        Self::Cover,
        Self::Triage,
    ];
}

/// `[model]`: how the built-in session reaches its model. Every key is
/// optional; what none says is basis's usual resolution from the
/// environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSettings {
    /// `anthropic`, `openai`, `gemini`, `openrouter`, `ollama`, `lmstudio`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The NAME of the environment variable that holds the key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// An OpenAI-compatible endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// The request format the endpoint at `base_url` speaks:
    /// `chat_completions` (the default) or `responses` (OpenAI's
    /// `v1/responses`, as some gateways such as LiteLLM serve it). Only
    /// with `base_url`: a provider preset carries its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire: Option<String>,
    /// Wall-clock bound on one run (`[speaker] brain_deadline_secs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_secs: Option<u64>,
    /// Wall-clock bound on one triage or pick call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_deadline_secs: Option<u64>,
    /// Token bound on one run (`[speaker] brain_token_budget`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
    /// `fallback = { … }`: a second model, with the same keys, that the
    /// Brain turns to when this one fails twice ([`ModelSettings::tiers`]).
    /// It names its own connection (provider, model, key, endpoint, wire);
    /// the bounds it leaves out are this model's. It has no fallback of its
    /// own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Box<ModelSettings>>,
}

/// One attempt in the order the Brain tries its models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    /// How the log names it: `primary`, `primary-retry`, `fallback`.
    pub label: &'static str,
    /// The model it runs on, without a fallback of its own.
    pub model: ModelSettings,
}

/// Checks that a model is complete and valid before it is used.
pub type ModelCheck = Arc<dyn Fn(&ModelSettings) -> Result<(), String> + Send + Sync>;

impl ModelSettings {
    /// `self`, with what it leaves out taken from `below`.
    pub fn over(&self, below: &Self) -> Self {
        fn pick<T: Clone>(a: &Option<T>, b: &Option<T>) -> Option<T> {
            a.clone().or_else(|| b.clone())
        }
        Self {
            provider: pick(&self.provider, &below.provider),
            model: pick(&self.model, &below.model),
            api_key_env: pick(&self.api_key_env, &below.api_key_env),
            base_url: pick(&self.base_url, &below.base_url),
            wire: pick(&self.wire, &below.wire),
            deadline_secs: pick(&self.deadline_secs, &below.deadline_secs),
            triage_deadline_secs: pick(&self.triage_deadline_secs, &below.triage_deadline_secs),
            token_budget: pick(&self.token_budget, &below.token_budget),
            fallback: pick(&self.fallback, &below.fallback),
        }
    }

    /// The order the Brain tries its models in, one entry per attempt: this
    /// model, this model again (one retry), then the fallback when there is
    /// one. The fallback's bounds default to this model's.
    pub fn tiers(&self) -> Vec<Tier> {
        let primary = Self {
            fallback: None,
            ..self.clone()
        };
        let mut tiers = vec![
            Tier {
                label: "primary",
                model: primary.clone(),
            },
            Tier {
                label: "primary-retry",
                model: primary.clone(),
            },
        ];
        if let Some(fallback) = &self.fallback {
            let bounds = Self {
                deadline_secs: primary.deadline_secs,
                triage_deadline_secs: primary.triage_deadline_secs,
                token_budget: primary.token_budget,
                ..Self::default()
            };
            tiers.push(Tier {
                label: "fallback",
                model: Self {
                    fallback: None,
                    ..fallback.over(&bounds)
                },
            });
        }
        tiers
    }

    /// What a model client is built from: two settings with the same key
    /// share one client.
    pub fn connection_key(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}|{:?}",
            self.provider, self.model, self.api_key_env, self.base_url, self.wire
        )
    }

    /// basis's rules: a provider it knows, a base URL that is one, a key
    /// variable that is set and says what it is for. Nothing is resolved
    /// beyond reading the variable.
    pub fn check_basis(&self) -> Result<(), String> {
        if let Some(fallback) = &self.fallback {
            if fallback.fallback.is_some() {
                return Err("fallback has a fallback of its own; only one is tried".into());
            }
            fallback
                .check_basis()
                .map_err(|e| format!("fallback: {e}"))?;
        }
        if self.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
            return Err("model is empty".into());
        }
        if let Some(provider) = &self.provider {
            basis::provider::parse(provider).map_err(|e| e.to_string())?;
        }
        if let Some(url) = &self.base_url {
            basis::provider::normalize_base_url(url).map_err(|e| e.to_string())?;
        }
        if let Some(wire) = &self.wire {
            wire_of(wire)?;
            if self.base_url.is_none() {
                return Err(format!(
                    "wire = {wire:?} needs a base_url: a provider preset speaks its own wire"
                ));
            }
        }
        if let Some(var) = &self.api_key_env {
            if var.trim().is_empty() {
                return Err("api_key_env is empty".into());
            }
            if self.provider.is_none() && self.base_url.is_none() {
                return Err(format!(
                    "api_key_env = {var:?} needs a provider or a base_url to say what the key is for"
                ));
            }
            if !env_is_set(var) {
                return Err(format!("the environment variable {var} is not set"));
            }
        }
        Ok(())
    }

    /// The default check: [`ModelSettings::check_basis`].
    pub fn basis_check() -> ModelCheck {
        Arc::new(Self::check_basis)
    }

    /// Put the timeouts and the model id where the engine reads them.
    fn apply_to(&self, speaker: &mut SpeakerConfig) {
        if self.model.is_some() {
            speaker.model = self.model.clone();
        }
        if let Some(secs) = self.deadline_secs {
            speaker.brain_deadline_secs = secs;
        }
        if let Some(secs) = self.triage_deadline_secs {
            speaker.triage_deadline_secs = secs;
        }
        if let Some(tokens) = self.token_budget {
            speaker.brain_token_budget = tokens;
        }
    }

    /// Each key it sets, as people read it: the key's variable by name (and
    /// whether it is set: the third field), never its value; a base URL
    /// without the credentials it may carry.
    fn shown(&self) -> Vec<(&'static str, String, Option<bool>)> {
        let mut out = Vec::new();
        let quoted = |s: &str| format!("{s:?}");
        if let Some(v) = &self.provider {
            out.push(("provider", quoted(v), None));
        }
        if let Some(v) = &self.model {
            out.push(("model", quoted(v), None));
        }
        if let Some(v) = &self.api_key_env {
            out.push(("api_key_env", v.clone(), Some(env_is_set(v))));
        }
        if let Some(v) = &self.base_url {
            out.push(("base_url", quoted(&without_credentials(v)), None));
        }
        if let Some(v) = &self.wire {
            out.push(("wire", quoted(v), None));
        }
        if let Some(v) = self.deadline_secs {
            out.push(("deadline_secs", v.to_string(), None));
        }
        if let Some(v) = self.triage_deadline_secs {
            out.push(("triage_deadline_secs", v.to_string(), None));
        }
        if let Some(v) = self.token_budget {
            out.push(("token_budget", v.to_string(), None));
        }
        if let Some(fallback) = &self.fallback {
            let keys: Vec<String> = fallback
                .shown()
                .into_iter()
                .map(|(key, value, set)| match set {
                    Some(false) => format!("{key} = {value} (not set)"),
                    _ => format!("{key} = {value}"),
                })
                .collect();
            out.push(("fallback", format!("{{ {} }}", keys.join(", ")), None));
        }
        out
    }
}

/// The basis wire `wire` names: `chat_completions` or `responses`.
pub fn wire_of(wire: &str) -> Result<basis::runtime::Wire, String> {
    match wire.trim() {
        "chat_completions" => Ok(basis::runtime::Wire::ChatCompletions),
        "responses" => Ok(basis::runtime::Wire::Responses),
        other => Err(format!(
            "wire = {other:?} is not one of \"chat_completions\", \"responses\""
        )),
    }
}

fn env_is_set(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| !v.is_empty())
}

/// `url` with any `user:password@` taken out.
fn without_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let host_end = rest.find('/').unwrap_or(rest.len());
    match rest[..host_end].rfind('@') {
        Some(at) => format!("{scheme}://***@{}", &rest[at + 1..]),
        None => url.to_owned(),
    }
}

/// The keys a `[speaker]` table held for its model, by their old names.
const OLD_MODEL_KEYS: [&str; 4] = [
    "model",
    "brain_deadline_secs",
    "triage_deadline_secs",
    "brain_token_budget",
];

/// The keys only a secretary file (or a `[secretary]` table) says, which
/// [`SpeakerConfig`] does not hold.
pub const SECRETARY_KEYS: [&str; 4] = ["name", "instructions", "instructions_file", "duties"];

/// A `[speaker]` (or `[secretary]`) table as the main configuration holds
/// it: its model keys taken out (they belong under `[model]` now), the rest
/// left as `secretary.toml` would say it. `None` for the model when the table
/// had no model key.
pub fn split_model_keys(mut table: toml::Table) -> (toml::Table, Option<ModelSettings>) {
    let mut model = ModelSettings::default();
    let mut found = false;
    for key in OLD_MODEL_KEYS {
        let Some(value) = table.remove(key) else {
            continue;
        };
        found = true;
        match (key, value) {
            ("model", toml::Value::String(id)) => model.model = Some(id),
            ("model", toml::Value::Table(inner)) => {
                // Already the new shape inside the old table.
                if let Ok(settings) = toml::Value::Table(inner).try_into::<ModelSettings>() {
                    model = settings.over(&model);
                }
            }
            ("brain_deadline_secs", toml::Value::Integer(n)) => {
                model.deadline_secs = u64::try_from(n).ok();
            }
            ("triage_deadline_secs", toml::Value::Integer(n)) => {
                model.triage_deadline_secs = u64::try_from(n).ok();
            }
            ("brain_token_budget", toml::Value::Integer(n)) => {
                model.token_budget = u64::try_from(n).ok();
            }
            _ => {}
        }
    }
    (table, found.then_some(model))
}

/// What `config` says that its defaults do not: the table an older
/// caller's whole [`SpeakerConfig`] stands for. `data_dir` and `locale`
/// are the deployment's and left out.
pub fn table_of_config(config: &SpeakerConfig) -> toml::Table {
    let mut defaults = SpeakerConfig::new(config.data_dir.clone());
    defaults.locale = config.locale;
    let defaults = toml::Table::try_from(&defaults).unwrap_or_default();
    let mut table = toml::Table::try_from(config).unwrap_or_default();
    table.remove("data_dir");
    table.remove("locale");
    table.retain(|key, value| defaults.get(key) != Some(value));
    table
}

/// Whether `table` says only what a secretary file may say (its model keys
/// already taken out: [`split_model_keys`]). The message names the first
/// key that is wrong.
pub fn check_table(table: &toml::Table) -> Result<(), String> {
    toml::Value::Table(table.clone())
        .try_into::<SecretaryFile>()
        .map(drop)
        .map_err(|e| e.to_string())
}

/// What the main configuration says for the built-in session (layer 2).
#[derive(Debug, Clone, Default)]
pub struct BaseLayer {
    /// The deployment's data directory: the model's workspaces live in
    /// `<data_dir>/speaker/`.
    pub data_dir: PathBuf,
    /// The deployment's language: what people read, and the language of
    /// every problem and note this configuration reports.
    pub locale: Locale,
    /// The main configuration file, to name it in what is shown.
    pub main_file: Option<PathBuf>,
    /// Other defaults the deployment gives, under every file (the rules
    /// for work a runner implies, say): keys as `secretary.toml` says them.
    pub deployment: toml::Table,
    /// An old `[speaker]` or a `[secretary]` table, read as if it were
    /// `secretary.toml` (model keys taken out: [`split_model_keys`]), and
    /// which it was (`"speaker"`). Its presence configures a model (the
    /// environment's, when it named none), as such a table always did.
    pub compat: Option<(String, toml::Table)>,
    /// The model keys found in the compatibility table, when it had any.
    pub compat_model: Option<ModelSettings>,
    /// `[model]`. `Some` (even empty) says a model is configured.
    pub model: Option<ModelSettings>,
}

impl BaseLayer {
    /// The deployment's data directory and language, nothing else.
    pub fn new(data_dir: impl Into<PathBuf>, locale: Locale) -> Self {
        Self {
            data_dir: data_dir.into(),
            locale,
            ..Self::default()
        }
    }

    /// With `[model]`.
    pub fn with_model(mut self, model: ModelSettings) -> Self {
        self.model = Some(model);
        self
    }

    /// With the main configuration named `file` in what is shown.
    pub fn with_main_file(mut self, file: impl Into<PathBuf>) -> Self {
        self.main_file = Some(file.into());
        self
    }

    /// With the compatibility table `section` (`speaker` or `secretary`) as
    /// written: its model keys are taken out here.
    pub fn with_compat(mut self, section: &str, table: toml::Table) -> Self {
        let (behaviour, model) = split_model_keys(table);
        self.compat = Some((section.to_owned(), behaviour));
        self.compat_model = model;
        self
    }

    /// The layer an older caller's whole [`SpeakerConfig`] makes: what it
    /// says beyond the defaults as the compatibility table `[speaker]`.
    pub fn of_config(config: &SpeakerConfig) -> Self {
        Self::new(config.data_dir.clone(), config.locale)
            .with_compat("speaker", table_of_config(config))
    }

    /// The layer a main configuration file's table makes (`senate.toml` on
    /// a machine, `senated.toml`): its `[model]`, and its `[secretary]` or
    /// old `[speaker]` table. Refused when one of them says something a
    /// secretary file may not, or both compatibility tables are there.
    pub fn of_main_table(
        table: &toml::Table,
        data_dir: impl Into<PathBuf>,
        locale: Locale,
    ) -> Result<Self, String> {
        let mut base = Self::new(data_dir, locale);
        if let Some(model) = table.get("model") {
            let model: ModelSettings = model
                .clone()
                .try_into()
                .map_err(|e: toml::de::Error| format!("[model]: {e}"))?;
            base.model = Some(model);
        }
        let found: Vec<&str> = COMPAT_SECTIONS
            .into_iter()
            .filter(|s| table.contains_key(*s))
            .collect();
        match found.as_slice() {
            [] => {}
            [section] => {
                let Some(toml::Value::Table(inner)) = table.get(*section) else {
                    return Err(format!("[{section}] must be a table"));
                };
                base = base.with_compat(section, inner.clone());
                if let Some((_, behaviour)) = &base.compat {
                    check_table(behaviour).map_err(|e| format!("[{section}]: {e}"))?;
                }
            }
            _ => {
                return Err(
                    "[secretary] and [speaker] both configure the built-in session; keep one (better: neither, and a secretary.toml)".into(),
                );
            }
        }
        Ok(base)
    }

    /// Whether a model is configured here: `[model]`, or a compatibility
    /// table.
    pub fn configures_model(&self) -> bool {
        self.model.is_some() || self.compat.is_some()
    }
}

/// Where a value in force came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The built-in default.
    Default,
    /// The main configuration (`file`, when it was read from one), at its
    /// top level (`table: None`) or in one of its tables.
    Main {
        file: Option<PathBuf>,
        table: Option<String>,
    },
    /// A secretary file.
    File(PathBuf),
}

impl Source {
    /// The source as a path, for a [`Problem`]: the file, and the table.
    fn as_path(&self) -> PathBuf {
        match self {
            Self::Default => PathBuf::from("(default)"),
            Self::File(path) => path.clone(),
            Self::Main { file, table } => {
                let file = file.as_ref().map_or_else(
                    || "main configuration".to_owned(),
                    |f| f.display().to_string(),
                );
                match table {
                    Some(table) => PathBuf::from(format!("{file} [{table}]")),
                    None => PathBuf::from(file),
                }
            }
        }
    }

    /// As people read it.
    pub fn render(&self, l: Locale) -> String {
        match self {
            Self::Default => text::default_source(l),
            Self::File(path) => path.display().to_string(),
            Self::Main { file, table } => {
                let file = file
                    .as_ref()
                    .map_or_else(|| text::main_configuration(l), |f| f.display().to_string());
                match table {
                    Some(table) => format!("{file} [{table}]"),
                    None => file,
                }
            }
        }
    }
}

/// One error in a configuration file: shown by status, logged once per
/// change. Its message is in the deployment's language.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Problem {
    pub file: PathBuf,
    /// 1-based, when it is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{line}: {}", self.file.display(), self.message),
            None => write!(f, "{}: {}", self.file.display(), self.message),
        }
    }
}

/// The configuration in force for one committee (or for the groups bound to
/// none).
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    /// The built-in session's senator name (fixed at start).
    pub name: SenatorName,
    /// Its behaviour, the model's timeouts and id folded in.
    pub speaker: SpeakerConfig,
    /// Some layer configures a model (whether or not it is usable).
    pub model_configured: bool,
    /// The model in force: the highest layer whose model is complete;
    /// `None` when no layer's is.
    pub model: Option<ModelSettings>,
    /// Extra instructions for the model, appended after the engine's own
    /// rules: `instructions`, then the text of `instructions_file`.
    pub instructions: Option<String>,
    /// Where part of them came from (watched like the files).
    pub instructions_file: Option<PathBuf>,
    /// What it may do on the desk without leaving it for the Speaker.
    pub duties: BTreeSet<Duty>,
}

impl Effective {
    pub fn may(&self, duty: Duty) -> bool {
        self.duties.contains(&duty)
    }
}

/// What one file says, as read. The behaviour keys are [`SpeakerConfig`]'s.
#[derive(Debug, Clone, Deserialize)]
struct SecretaryFile {
    /// Checked here, read from the table ([`SecretaryConfigs::name`]).
    #[serde(default, rename = "name")]
    _name: Option<String>,
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    instructions_file: Option<PathBuf>,
    #[serde(default)]
    duties: Option<Vec<Duty>>,
    /// Checked here, layered from the tables ([`ModelSettings::over`]).
    #[serde(default, rename = "model")]
    _model: Option<ModelSettings>,
    #[serde(flatten)]
    behaviour: SpeakerConfig,
}

type Stamp = Option<(Option<SystemTime>, u64)>;

fn stamp(path: &Path) -> Stamp {
    std::fs::metadata(path)
        .ok()
        .map(|m| (m.modified().ok(), m.len()))
}

/// One file as last read.
#[derive(Default)]
struct FileState {
    /// Read at least once.
    seen: bool,
    stamp: Stamp,
    /// What is in force from it: its last good content.
    table: Option<toml::Table>,
    problem: Option<Problem>,
}

#[derive(Default)]
struct State {
    files: HashMap<PathBuf, FileState>,
    effective: HashMap<Option<CommitteeId>, Arc<Effective>>,
    /// What each effective configuration was computed from.
    stamps: HashMap<Option<CommitteeId>, Vec<(PathBuf, Stamp)>>,
    /// Model fallbacks and unreadable instructions, per configuration.
    problems: HashMap<Option<CommitteeId>, Vec<Problem>>,
    /// A name that could not be used, at start.
    name_problem: Option<Problem>,
    /// Problems and notes already logged.
    logged: HashSet<String>,
}

/// The layers one configuration is computed from, as they are now.
struct Layers {
    secretary_file: Option<PathBuf>,
    committee_file: Option<PathBuf>,
    /// `secretary.toml`'s table (its last good content).
    file_table: Option<toml::Table>,
    committee_table: Option<toml::Table>,
    /// Whether the compatibility table's behaviour is read (no
    /// `secretary.toml`).
    compat_read: bool,
}

/// The built-in session's configuration, re-read as its files change. Shared
/// by the engine (which refreshes it before acting on a committee), the
/// Brain (which reads the model and the instructions per committee) and the
/// daemon (which shows its problems).
pub struct SecretaryConfigs {
    dir: Option<PathBuf>,
    base: BaseLayer,
    check: ModelCheck,
    name: SenatorName,
    /// What the start assumed, to say once.
    notes: Vec<String>,
    state: Mutex<State>,
}

impl fmt::Debug for SecretaryConfigs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretaryConfigs")
            .field("dir", &self.dir)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn default_name() -> SenatorName {
    DEFAULT_NAME
        .parse()
        .unwrap_or_else(|_| unreachable!("the default name is valid"))
}

impl SecretaryConfigs {
    /// The configuration in `dir` (`None`: no files, only `base`), over
    /// `base`. Models are checked by basis's rules
    /// ([`ModelSettings::check_basis`]).
    pub fn new(dir: Option<PathBuf>, base: BaseLayer) -> Self {
        Self::with_check(dir, base, ModelSettings::basis_check())
    }

    /// The same, with models checked by `check` (a host with providers of
    /// its own, or a test).
    pub fn with_check(dir: Option<PathBuf>, base: BaseLayer, check: ModelCheck) -> Self {
        let mut configs = Self {
            dir,
            base,
            check,
            name: default_name(),
            notes: Vec::new(),
            state: Mutex::new(State::default()),
        };
        configs.name = configs.initial_name();
        configs.notes = configs.start_notes();
        configs
    }

    /// A configuration with no files: `config` as the only word.
    pub fn fixed(config: SpeakerConfig) -> Self {
        Self::new(None, BaseLayer::of_config(&config))
    }

    /// The built-in session's senator name, as it was at start.
    pub fn name(&self) -> &SenatorName {
        &self.name
    }

    /// The directory the files are read from.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The deployment's language.
    pub fn locale(&self) -> Locale {
        self.base.locale
    }

    /// The compatibility table the main configuration holds (`speaker` or
    /// `secretary`), when it holds one.
    pub fn compat_section(&self) -> Option<&str> {
        self.base
            .compat
            .as_ref()
            .map(|(section, _)| section.as_str())
    }

    /// `<dir>/secretary.toml`.
    pub fn secretary_file(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(SECRETARY_FILE))
    }

    /// `<dir>/<committee>.secretary.toml`.
    pub fn committee_file(&self, committee: &CommitteeId) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|d| d.join(format!("{committee}{COMMITTEE_FILE_SUFFIX}")))
    }

    /// Whether some layer configures a model now.
    pub fn model_configured(&self) -> bool {
        self.refresh(None).model_configured
    }

    /// Whether a complete model is in force for the Senate as a whole
    /// (`secretary.toml` over the main configuration): what the built-in
    /// session needs to run.
    pub fn model_in_force(&self) -> bool {
        self.refresh(None).model.is_some()
    }

    /// What the start assumed (the compatibility table and its model
    /// keys), in the deployment's language. Not logged here: the host says
    /// them once (`senated` remembers in the store that it did).
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Read again whatever changed, in every file the built-in session's
    /// configuration can come from (`secretary.toml` and each
    /// `<committee>.secretary.toml` in the directory), and return every
    /// error in force. The engine re-reads a committee's files only when it
    /// acts there; this is for whoever shows the errors without waiting
    /// for that.
    pub fn check_all(&self) -> Vec<Problem> {
        self.refresh(None);
        // The committees with a file now, and those read before (their
        // file may be gone, which ends its errors).
        let mut committees: BTreeSet<CommitteeId> = self
            .lock()
            .effective
            .keys()
            .filter_map(Clone::clone)
            .collect();
        if let Some(entries) = self.dir.as_ref().and_then(|d| std::fs::read_dir(d).ok()) {
            committees.extend(entries.filter_map(Result::ok).filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                CommitteeId::new(name.strip_suffix(COMMITTEE_FILE_SUFFIX)?).ok()
            }));
        }
        for committee in committees {
            self.refresh(Some(&committee));
        }
        self.problems()
    }

    /// Every error in force: unreadable files, models that fell back,
    /// instructions that could not be read.
    pub fn problems(&self) -> Vec<Problem> {
        let state = self.lock();
        let mut out: Vec<Problem> = state
            .files
            .values()
            .filter_map(|f| f.problem.clone())
            .chain(state.problems.values().flatten().cloned())
            .chain(state.name_problem.clone())
            .collect();
        out.sort_by(|a, b| (&a.file, a.line, &a.message).cmp(&(&b.file, b.line, &b.message)));
        out.dedup();
        out
    }

    /// The configuration in force for `committee` (`None`: groups bound to
    /// none), re-read first if one of its files changed. A few `stat`
    /// calls when nothing did.
    pub fn refresh(&self, committee: Option<&CommitteeId>) -> Arc<Effective> {
        let key = committee.cloned();
        let mut state = self.lock();
        let stamps = stamps_of(self.watched(&state, committee));
        if let (Some(previous), Some(effective)) =
            (state.stamps.get(&key), state.effective.get(&key))
            && *previous == stamps
        {
            return Arc::clone(effective);
        }
        let (effective, problems) = self.compute(&mut state, committee);
        let effective = match effective {
            Some(effective) => Arc::new(effective),
            None => match state.effective.get(&key) {
                // Never half-applied: what was in force stays.
                Some(last) => Arc::clone(last),
                None => Arc::new(self.base_only()),
            },
        };
        for problem in &problems {
            self.log_once(&mut state, &format!("{problem}"), || {
                warn!(%problem, "the built-in session's configuration has an error; the last good one stays");
            });
        }
        state.problems.insert(key.clone(), problems);
        state.effective.insert(key.clone(), Arc::clone(&effective));
        // The stamps of what this configuration now reads, instructions
        // included.
        let stamps = stamps_of(self.watched(&state, committee));
        state.stamps.insert(key, stamps);
        effective
    }

    /// The configuration last computed for `committee`, without looking at
    /// the files (computed now if it never was).
    pub fn effective(&self, committee: Option<&CommitteeId>) -> Arc<Effective> {
        if let Some(effective) = self.lock().effective.get(&committee.cloned()) {
            return Arc::clone(effective);
        }
        self.refresh(committee)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn log_once(&self, state: &mut State, key: &str, log: impl FnOnce()) {
        if state.logged.insert(key.to_owned()) {
            log();
        }
    }

    /// The files `committee`'s configuration reads: `secretary.toml`, the
    /// committee's own file, and the instructions file last in force.
    fn watched(&self, state: &State, committee: Option<&CommitteeId>) -> Vec<PathBuf> {
        let mut files = self.files_of(committee);
        if let Some(path) = state
            .effective
            .get(&committee.cloned())
            .and_then(|e| e.instructions_file.clone())
        {
            files.push(path);
        }
        files
    }

    /// `secretary.toml` and the committee's own file.
    fn files_of(&self, committee: Option<&CommitteeId>) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = self.secretary_file().into_iter().collect();
        if let Some(committee) = committee {
            files.extend(self.committee_file(committee));
        }
        files
    }

    fn main_source(&self, table: Option<&str>) -> Source {
        Source::Main {
            file: self.base.main_file.clone(),
            table: table.map(str::to_owned),
        }
    }

    /// The notes of a start: how the compatibility table is read.
    fn start_notes(&self) -> Vec<String> {
        let l = self.base.locale;
        let mut notes = Vec::new();
        let Some((section, _)) = &self.base.compat else {
            return notes;
        };
        let main = self.main_source(None).render(l);
        let file = self.secretary_file();
        match file.as_ref().filter(|f| f.exists()) {
            Some(file) => notes.push(text::compat_file_wins(l, &main, section, file)),
            None => notes.push(text::compat_read(l, &main, section, file.as_deref())),
        }
        if self.base.compat_model.is_some() {
            notes.push(text::compat_model_keys(l, &main, section));
        }
        notes
    }

    /// The name at start: `secretary.toml`'s, else the compatibility
    /// table's, else [`DEFAULT_NAME`].
    fn initial_name(&self) -> SenatorName {
        let mut state = self.lock();
        let from_file = self.secretary_file().and_then(|path| {
            self.read(&mut state, &path, false);
            state
                .files
                .get(&path)
                .and_then(|f| f.table.as_ref())
                .and_then(|t| t.get("name"))
                .and_then(|v| v.as_str().map(str::to_owned))
                .map(|name| (name, path))
        });
        let from_table = self.base.compat.as_ref().and_then(|(section, table)| {
            table
                .get("name")
                .and_then(|v| v.as_str())
                .map(|name| (name.to_owned(), self.main_source(Some(section)).as_path()))
        });
        let Some((name, file)) = from_file.or(from_table) else {
            return default_name();
        };
        match name.parse::<SenatorName>() {
            Ok(name) => name,
            Err(error) => {
                let problem = Problem {
                    file,
                    line: None,
                    message: text::bad_name(self.base.locale, &name, &error.to_string()),
                };
                warn!(%problem, "the built-in session's name");
                state.name_problem = Some(problem);
                default_name()
            }
        }
    }

    /// Read `path` again if it changed since it was last read. A file that
    /// cannot be read or understood keeps its last good content in force.
    fn read(&self, state: &mut State, path: &Path, committee_file: bool) {
        let now = stamp(path);
        let entry = state.files.entry(path.to_path_buf()).or_default();
        if entry.seen && entry.stamp == now {
            return;
        }
        entry.seen = true;
        entry.stamp = now;
        if now.is_none() {
            // Removed: what it said no longer applies.
            entry.table = None;
            entry.problem = None;
            return;
        }
        match read_layer(path, committee_file, self.base.locale) {
            Ok(table) => {
                entry.table = Some(table);
                entry.problem = None;
            }
            Err(problem) => {
                entry.problem = Some(problem.clone());
                let key = format!("{problem} {now:?}");
                self.log_once(state, &key, || {
                    warn!(%problem, "cannot read the built-in session's configuration; the last good one stays");
                });
            }
        }
    }

    /// Read the files `committee`'s configuration is made of, as they are
    /// now.
    fn layers(&self, state: &mut State, committee: Option<&CommitteeId>) -> Layers {
        let secretary_file = self.secretary_file();
        if let Some(path) = &secretary_file {
            self.read(state, path, false);
        }
        let committee_file = committee.and_then(|c| self.committee_file(c));
        if let Some(path) = &committee_file {
            self.read(state, path, true);
        }
        let table_of = |state: &State, path: &Option<PathBuf>| {
            path.as_ref()
                .and_then(|p| state.files.get(p))
                .and_then(|f| f.table.clone())
        };
        let file_table = table_of(state, &secretary_file);
        let committee_table = table_of(state, &committee_file);
        let compat_read = !secretary_file
            .as_ref()
            .is_some_and(|path| state.files.get(path).is_some_and(|f| f.stamp.is_some()));
        Layers {
            secretary_file,
            committee_file,
            file_table,
            committee_table,
            compat_read,
        }
    }

    /// The model layers, lowest first, and any `[model]` table in a file
    /// that is not one.
    fn model_layers(&self, layers: &Layers) -> (Vec<(Source, ModelSettings)>, Vec<Problem>) {
        let mut out = Vec::new();
        let mut problems = Vec::new();
        if let Some((section, _)) = &self.base.compat {
            out.push((
                self.main_source(Some(section)),
                self.base.compat_model.clone().unwrap_or_default(),
            ));
        }
        if let Some(model) = &self.base.model {
            out.push((self.main_source(Some("model")), model.clone()));
        }
        for (path, table) in [
            (&layers.secretary_file, &layers.file_table),
            (&layers.committee_file, &layers.committee_table),
        ] {
            let (Some(path), Some(table)) = (path, table) else {
                continue;
            };
            if let Some(model) = table.get("model") {
                match model.clone().try_into::<ModelSettings>() {
                    Ok(model) => out.push((Source::File(path.clone()), model)),
                    Err(error) => problems.push(Problem {
                        file: path.clone(),
                        line: None,
                        message: format!("[model]: {error}"),
                    }),
                }
            }
        }
        (out, problems)
    }

    /// Compute `committee`'s configuration from the files as they are now.
    /// `None` when it cannot be (the last good one then stays).
    fn compute(
        &self,
        state: &mut State,
        committee: Option<&CommitteeId>,
    ) -> (Option<Effective>, Vec<Problem>) {
        let l = self.base.locale;
        let layers = self.layers(state, committee);

        // The name: fixed at start.
        if let (Some(table), Some(path)) = (&layers.file_table, &layers.secretary_file)
            && let Some(name) = table.get("name").and_then(|v| v.as_str())
            && name != self.name.as_str()
        {
            let note = text::name_next_start(l, path, name, &self.name);
            self.log_once(state, &note, || info!("{note}"));
        }

        // Layer 2: the deployment's defaults, then the compatibility table
        // unless secretary.toml says it all.
        let mut behaviour = self.deployment_table();
        if let Some((_, compat)) = &self.base.compat
            && layers.compat_read
        {
            merge(&mut behaviour, compat);
        }
        let (model_layers, mut problems) = self.model_layers(&layers);
        for table in [&layers.file_table, &layers.committee_table]
            .into_iter()
            .flatten()
        {
            let mut table = table.clone();
            table.remove("model");
            merge(&mut behaviour, &table);
        }
        let file: SecretaryFile = match toml::Value::Table(behaviour).try_into() {
            Ok(file) => file,
            Err(error) => {
                let file = layers
                    .committee_file
                    .clone()
                    .or_else(|| layers.secretary_file.clone())
                    .unwrap_or_default();
                problems.push(Problem {
                    file,
                    line: None,
                    message: error.to_string(),
                });
                return (None, problems);
            }
        };
        let model = self
            .model_of(&model_layers, &mut problems)
            .map(|(_, model)| model);
        let mut speaker = file.behaviour;
        if let Some(model) = &model {
            model.apply_to(&mut speaker);
        }
        let mut instructions: Vec<String> = file
            .instructions
            .into_iter()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(path) = file.instructions_file.as_ref() {
            match std::fs::read_to_string(path) {
                Ok(text) if !text.trim().is_empty() => instructions.push(text.trim().to_owned()),
                Ok(_) => {}
                Err(error) => {
                    problems.push(Problem {
                        file: path.clone(),
                        line: None,
                        message: text::instructions_unreadable(l, &error.to_string()),
                    });
                    return (None, problems);
                }
            }
        }
        let duties = file.duties.map_or_else(
            || Duty::ALL.into_iter().collect(),
            |d| d.into_iter().collect(),
        );
        let effective = Effective {
            name: self.name.clone(),
            speaker,
            model_configured: !model_layers.is_empty(),
            model,
            instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
            instructions_file: file.instructions_file,
            duties,
        };
        (Some(effective), problems)
    }

    /// `data_dir` and `locale`, as the deployment says them.
    fn deployment_table(&self) -> toml::Table {
        let mut table = toml::Table::new();
        table.insert(
            "data_dir".into(),
            toml::Value::String(self.base.data_dir.to_string_lossy().into_owned()),
        );
        table.insert(
            "locale".into(),
            toml::Value::String(self.base.locale.as_str().to_owned()),
        );
        merge(&mut table, &self.base.deployment);
        table
    }

    /// The highest layer whose model is complete (its index, and the model
    /// merged up to it); each one above it that is not is reported.
    fn model_of(
        &self,
        layers: &[(Source, ModelSettings)],
        problems: &mut Vec<Problem>,
    ) -> Option<(usize, ModelSettings)> {
        let l = self.base.locale;
        for top in (0..layers.len()).rev() {
            let merged = layers[..=top]
                .iter()
                .fold(ModelSettings::default(), |below, (_, m)| m.over(&below));
            match (self.check)(&merged) {
                Ok(()) => return Some((top, merged)),
                Err(error) => problems.push(Problem {
                    file: layers[top].0.as_path(),
                    line: None,
                    message: if top == 0 {
                        text::model_none(l, &error)
                    } else {
                        text::model_falls_back(l, &error, &layers[top - 1].0.render(l))
                    },
                }),
            }
        }
        None
    }

    /// The configuration from the main configuration alone.
    fn base_only(&self) -> Effective {
        let mut speaker = SpeakerConfig::new(self.base.data_dir.clone());
        speaker.locale = self.base.locale;
        let model = self
            .base
            .model
            .clone()
            .or_else(|| {
                self.base
                    .compat
                    .as_ref()
                    .map(|_| self.base.compat_model.clone().unwrap_or_default())
            })
            .filter(|m| (self.check)(m).is_ok());
        if let Some(model) = &model {
            model.apply_to(&mut speaker);
        }
        Effective {
            name: self.name.clone(),
            speaker,
            model_configured: self.base.configures_model(),
            model,
            instructions: None,
            instructions_file: None,
            duties: Duty::ALL.into_iter().collect(),
        }
    }

    /// Every value in force for `committee` (`None`: the Senate as a whole,
    /// `secretary.toml` over the main configuration) and where it came
    /// from, re-read first if a file changed; the model's key is never
    /// among them.
    pub fn explain(&self, committee: Option<&CommitteeId>) -> Shown {
        let effective = self.refresh(committee);
        let mut state = self.lock();
        let layers = self.layers(&mut state, committee);
        drop(state);

        // Behaviour, key by key.
        let mut values: BTreeMap<String, (toml::Value, Source)> = BTreeMap::new();
        let mut put = |table: &toml::Table, source: &Source| {
            let mut flat = BTreeMap::new();
            flatten("", table, &mut flat);
            for (key, value) in flat {
                if OLD_MODEL_KEYS.contains(&key.as_str())
                    || key == "model"
                    || key.starts_with("model.")
                {
                    continue;
                }
                values.insert(key, (value, source.clone()));
            }
        };
        let mut defaults = toml::Table::try_from(SpeakerConfig::new(self.base.data_dir.clone()))
            .unwrap_or_default();
        defaults.insert("name".into(), toml::Value::String(DEFAULT_NAME.into()));
        defaults.insert(
            "duties".into(),
            toml::Value::Array(
                Duty::ALL
                    .iter()
                    .filter_map(|d| toml::Value::try_from(d).ok())
                    .collect(),
            ),
        );
        put(&defaults, &Source::Default);
        put(&self.deployment_table(), &self.main_source(None));
        if let Some((section, compat)) = &self.base.compat
            && layers.compat_read
        {
            put(compat, &self.main_source(Some(section)));
        }
        if let (Some(path), Some(table)) = (&layers.secretary_file, &layers.file_table) {
            put(table, &Source::File(path.clone()));
        }
        if let (Some(path), Some(table)) = (&layers.committee_file, &layers.committee_table) {
            put(table, &Source::File(path.clone()));
        }
        // The name in force is the one of the start.
        if let Some((value, _)) = values.get_mut("name") {
            *value = toml::Value::String(self.name.to_string());
        }
        let values = values
            .into_iter()
            .map(|(key, (value, source))| ShownValue {
                key,
                value: shown_value(&value),
                key_set: None,
                source,
            })
            .collect();

        // The model: the layers in force, key by key.
        let (model_layers, _) = self.model_layers(&layers);
        let mut discarded = Vec::new();
        let in_force = self.model_of(&model_layers, &mut discarded);
        let upto = in_force
            .as_ref()
            .map_or(model_layers.len(), |(top, _)| top + 1);
        let mut model: BTreeMap<&'static str, (String, Option<bool>, Source)> = BTreeMap::new();
        for (source, settings) in &model_layers[..upto] {
            for (key, value, set) in settings.shown() {
                model.insert(key, (value, set, source.clone()));
            }
        }
        let model = model
            .into_iter()
            .map(|(key, (value, key_set, source))| ShownValue {
                key: key.to_owned(),
                value,
                key_set,
                source,
            })
            .collect();

        Shown {
            name: self.name.clone(),
            committee: committee.cloned(),
            values,
            model_configured: effective.model_configured,
            model_in_force: in_force.is_some(),
            model,
            problems: self.problems(),
            notes: self.notes.clone(),
        }
    }
}

fn stamps_of(files: Vec<PathBuf>) -> Vec<(PathBuf, Stamp)> {
    files
        .into_iter()
        .map(|p| {
            let s = stamp(&p);
            (p, s)
        })
        .collect()
}

/// `table`'s keys, dotted, with their values (arrays are values).
fn flatten(prefix: &str, table: &toml::Table, out: &mut BTreeMap<String, toml::Value>) {
    for (key, value) in table {
        let key = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match value {
            toml::Value::Table(inner) => flatten(&key, inner, out),
            other => {
                out.insert(key, other.clone());
            }
        }
    }
}

/// A value on one line, a long text cut short.
fn shown_value(value: &toml::Value) -> String {
    const MAX: usize = 80;
    let text = value.to_string().replace('\n', " ");
    if text.chars().count() > MAX {
        let cut: String = text.chars().take(MAX).collect();
        format!("{cut}…")
    } else {
        text
    }
}

/// One value in force and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShownValue {
    /// The key, dotted inside a table (`work.pr_base`).
    pub key: String,
    /// As written in TOML; for `api_key_env`, the variable's name.
    pub value: String,
    /// For `api_key_env`: whether the variable is set (its value is never
    /// read into what is shown).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_set: Option<bool>,
    pub source: Source,
}

/// A committee's configuration in force, value by value, with the problems
/// and the notes: what `secretary show` prints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Shown {
    pub name: SenatorName,
    pub committee: Option<CommitteeId>,
    /// The behaviour keys.
    pub values: Vec<ShownValue>,
    /// Some layer configures a model.
    pub model_configured: bool,
    /// A complete model is in force.
    pub model_in_force: bool,
    /// The model keys in force (or, when none is, as configured).
    pub model: Vec<ShownValue>,
    pub problems: Vec<Problem>,
    pub notes: Vec<String>,
}

impl Shown {
    /// The model key `key` as shown, when it is set.
    pub fn model_value(&self, key: &str) -> Option<&ShownValue> {
        self.model.iter().find(|v| v.key == key)
    }

    /// The behaviour key `key` as shown.
    pub fn value(&self, key: &str) -> Option<&ShownValue> {
        self.values.iter().find(|v| v.key == key)
    }

    /// As people read it, in `l`.
    pub fn render(&self, l: Locale) -> String {
        let mut out = String::new();
        let id = format!("[senator::{}]", self.name);
        out.push_str(&text::show_heading(l, &id, self.committee.as_ref()));
        out.push('\n');
        let width = self
            .values
            .iter()
            .chain(&self.model)
            .map(|v| v.key.len())
            .max()
            .unwrap_or(0);
        let line = |v: &ShownValue| {
            let value = match v.key_set {
                Some(set) => text::key_variable(l, &v.value, set),
                None => v.value.clone(),
            };
            format!("  {:width$} = {value}    ({})\n", v.key, v.source.render(l))
        };
        for value in &self.values {
            out.push_str(&line(value));
        }
        out.push('\n');
        out.push_str(&text::model_heading(
            l,
            self.model_configured,
            self.model_in_force,
        ));
        out.push('\n');
        for value in &self.model {
            out.push_str(&line(value));
        }
        out.push('\n');
        if self.problems.is_empty() {
            out.push_str(&text::no_problems(l));
            out.push('\n');
        } else {
            out.push_str(&text::problems_heading(l));
            out.push('\n');
            for problem in &self.problems {
                out.push_str(&format!("  {problem}\n"));
            }
        }
        if !self.notes.is_empty() {
            out.push('\n');
            out.push_str(&text::notes_heading(l));
            out.push('\n');
            for note in &self.notes {
                out.push_str(&format!("  {note}\n"));
            }
        }
        out
    }
}

/// Merge `over` into `into`, key by key; tables merge inside.
fn merge(into: &mut toml::Table, over: &toml::Table) {
    for (key, value) in over {
        match (into.get_mut(key), value) {
            (Some(toml::Value::Table(inner)), toml::Value::Table(over_inner)) => {
                merge(inner, over_inner);
            }
            _ => {
                into.insert(key.clone(), value.clone());
            }
        }
    }
}

/// The 1-based line of byte `offset` in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// The line where `key` is set at the top of `text` (`key = …`), or where
/// its table starts (`[key]`, `[key.…]`).
fn line_of_key(text: &str, key: &str) -> Option<usize> {
    text.lines()
        .position(|line| {
            let line = line.trim_start();
            line.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
                || line
                    .strip_prefix('[')
                    .and_then(|rest| rest.strip_prefix(key))
                    .is_some_and(|rest| rest.starts_with(']') || rest.starts_with('.'))
        })
        .map(|i| i + 1)
}

/// One file, read and checked: a TOML table whose keys are what a
/// secretary file may say. A relative `instructions_file` is made relative
/// to the file's directory.
fn read_layer(path: &Path, committee_file: bool, l: Locale) -> Result<toml::Table, Problem> {
    let problem = |line: Option<usize>, message: String| Problem {
        file: path.to_path_buf(),
        line,
        message,
    };
    let text = std::fs::read_to_string(path).map_err(|e| problem(None, e.to_string()))?;
    let mut table: toml::Table = toml::from_str(&text).map_err(|e: toml::de::Error| {
        let line = e.span().map(|s| line_of(&text, s.start));
        problem(line, e.message().to_owned())
    })?;
    // The keys and their types, with the line of the first that is wrong
    // (the flattened behaviour keys carry no position of their own, so it
    // is found key by key).
    if let Err(error) = toml::Value::Table(table.clone()).try_into::<SecretaryFile>() {
        let wrong = table.iter().find(|(key, value)| {
            let mut alone = toml::Table::new();
            alone.insert((*key).clone(), (*value).clone());
            toml::Value::Table(alone)
                .try_into::<SecretaryFile>()
                .is_err()
        });
        let line = wrong
            .and_then(|(key, _)| line_of_key(&text, key))
            .or_else(|| error.span().map(|s| line_of(&text, s.start)));
        return Err(problem(line, error.message().to_owned()));
    }
    if committee_file && table.contains_key("name") {
        return Err(problem(
            line_of_key(&text, "name"),
            text::name_in_committee_file(l),
        ));
    }
    if let Some(file) = table.get("instructions_file").and_then(|v| v.as_str()) {
        let file = PathBuf::from(file);
        if file.is_relative()
            && let Some(dir) = path.parent()
        {
            table.insert(
                "instructions_file".into(),
                toml::Value::String(dir.join(file).to_string_lossy().into_owned()),
            );
        }
    }
    Ok(table)
}

/// Whether `file_name` is a committee's file, and whose. A prefix that is
/// not a committee id is not one.
pub fn committee_of_file(file_name: &str) -> Option<CommitteeId> {
    let prefix = file_name.strip_suffix(COMMITTEE_FILE_SUFFIX)?;
    let id = CommitteeId::new(prefix).ok()?;
    senate_core::check_not_keyword(id.as_str(), "a committee").ok()?;
    Some(id)
}

/// Every text this module writes for people, one function per message, in
/// both languages. Senate, senator and Speaker are never translated; ids
/// are bracketed.
pub mod text {
    use std::path::Path;

    use senate_core::{CommitteeId, Locale, SenatorName};

    pub(super) fn default_source(l: Locale) -> String {
        match l {
            Locale::Zh => "内置默认值".into(),
            Locale::En => "built-in default".into(),
        }
    }

    pub(super) fn main_configuration(l: Locale) -> String {
        match l {
            Locale::Zh => "主配置".into(),
            Locale::En => "the main configuration".into(),
        }
    }

    /// `api_key_env`'s value as shown: the variable, and whether it is set.
    pub(super) fn key_variable(l: Locale, var: &str, set: bool) -> String {
        match (l, set) {
            (Locale::Zh, true) => format!("{var}（已设置）"),
            (Locale::Zh, false) => format!("{var}（未设置）"),
            (Locale::En, true) => format!("{var} (set)"),
            (Locale::En, false) => format!("{var} (not set)"),
        }
    }

    /// The compatibility table is read as `secretary.toml`.
    pub fn compat_read(l: Locale, main: &str, section: &str, file: Option<&Path>) -> String {
        let file = file.map_or_else(
            || super::SECRETARY_FILE.to_owned(),
            |f| f.display().to_string(),
        );
        match l {
            Locale::Zh => format!(
                "{main} 中的 [{section}] 表按 secretary.toml 读取（兼容旧配置）。请把它移到 {file}。"
            ),
            Locale::En => format!(
                "The [{section}] table of {main} is read as if it were secretary.toml (for compatibility). Move it to {file}."
            ),
        }
    }

    /// Both are there: the file wins.
    pub fn compat_file_wins(l: Locale, main: &str, section: &str, file: &Path) -> String {
        let file = file.display();
        match l {
            Locale::Zh => format!(
                "{file} 与 {main} 中的 [{section}] 表同时存在：以文件为准，[{section}] 表中除模型设置外的内容不再读取。请把仍需要的内容移入该文件。"
            ),
            Locale::En => format!(
                "Both {file} and the [{section}] table of {main} exist: the file wins, and the table is no longer read (except its model keys). Move what you still need into the file."
            ),
        }
    }

    /// Model keys found in the compatibility table.
    pub fn compat_model_keys(l: Locale, main: &str, section: &str) -> String {
        match l {
            Locale::Zh => format!(
                "{main} 的 [{section}] 表中有模型设置（model、brain_deadline_secs 等），仍会读取；请移到顶层 [model] 表（provider、model、api_key_env、deadline_secs、triage_deadline_secs、token_budget）。"
            ),
            Locale::En => format!(
                "The [{section}] table of {main} holds model keys (model, brain_deadline_secs, …); they are still read. Move them to the top-level [model] table (provider, model, api_key_env, deadline_secs, triage_deadline_secs, token_budget)."
            ),
        }
    }

    pub(super) fn name_next_start(l: Locale, path: &Path, new: &str, now: &SenatorName) -> String {
        let path = path.display();
        match l {
            Locale::Zh => format!(
                "{path}：name 改为 {new:?}，下次启动时生效；本次运行仍为 [senator::{now}]。"
            ),
            Locale::En => format!(
                "{path}: name {new:?} applies at the next start; this run keeps [senator::{now}]."
            ),
        }
    }

    pub(super) fn name_in_committee_file(l: Locale) -> String {
        match l {
            Locale::Zh => "委员会的配置文件不能设置 name：内置会话在整个 Senate 中只有一个名字，请在 secretary.toml 中设置。整个文件未被采用。".into(),
            Locale::En => "a committee's file may not set name: the built-in session has one name in the whole Senate; set it in secretary.toml. The file is not applied.".into(),
        }
    }

    pub(super) fn bad_name(l: Locale, name: &str, error: &str) -> String {
        match l {
            Locale::Zh => format!(
                "name {name:?} 不是有效的 senator 名字（{error}）；使用 {:?}",
                super::DEFAULT_NAME
            ),
            Locale::En => format!(
                "name {name:?} is not a senator name ({error}); using {:?}",
                super::DEFAULT_NAME
            ),
        }
    }

    pub(super) fn instructions_unreadable(l: Locale, error: &str) -> String {
        match l {
            Locale::Zh => format!("instructions_file 无法读取：{error}"),
            Locale::En => format!("instructions_file cannot be read: {error}"),
        }
    }

    pub(super) fn model_falls_back(l: Locale, error: &str, below: &str) -> String {
        match l {
            Locale::Zh => format!("模型设置不完整：{error}；改用下一层（{below}）"),
            Locale::En => {
                format!("the model is incomplete: {error}; the next layer down is used ({below})")
            }
        }
    }

    pub(super) fn model_none(l: Locale, error: &str) -> String {
        match l {
            Locale::Zh => format!("模型设置不完整：{error}；没有可用的模型"),
            Locale::En => format!("the model is incomplete: {error}; no model is in force"),
        }
    }

    pub(super) fn show_heading(l: Locale, id: &str, committee: Option<&CommitteeId>) -> String {
        match (l, committee) {
            (Locale::Zh, Some(c)) => format!("内置会话 {id} 在委员会 [{c}] 的生效配置："),
            (Locale::Zh, None) => format!("内置会话 {id} 在整个 Senate 的生效配置："),
            (Locale::En, Some(c)) => {
                format!("The built-in session {id}, configuration in force on committee [{c}]:")
            }
            (Locale::En, None) => {
                format!("The built-in session {id}, configuration in force for the whole Senate:")
            }
        }
    }

    pub(super) fn model_heading(l: Locale, configured: bool, in_force: bool) -> String {
        match (l, configured, in_force) {
            (Locale::Zh, _, true) => {
                "模型（生效中；密钥从不显示，只显示变量名及是否已设置）：".into()
            }
            (Locale::Zh, true, false) => "模型（已配置但不完整，未生效）：".into(),
            (Locale::Zh, false, false) => "模型：未配置（内置会话不会运行）".into(),
            (Locale::En, _, true) => {
                "Model (in force; a key is never shown, only its variable and whether it is set):"
                    .into()
            }
            (Locale::En, true, false) => "Model (configured but incomplete; not in force):".into(),
            (Locale::En, false, false) => {
                "Model: not configured (the built-in session does not run)".into()
            }
        }
    }

    pub(super) fn problems_heading(l: Locale) -> String {
        match l {
            Locale::Zh => "问题：".into(),
            Locale::En => "Problems:".into(),
        }
    }

    pub(super) fn no_problems(l: Locale) -> String {
        match l {
            Locale::Zh => "没有问题。".into(),
            Locale::En => "No problems.".into(),
        }
    }

    pub(super) fn notes_heading(l: Locale) -> String {
        match l {
            Locale::Zh => "说明：".into(),
            Locale::En => "Notes:".into(),
        }
    }
}
