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
//! under `[model]`.
//!
//! The model ([`ModelSettings`]) is layered the same way. A layer whose
//! resulting model is incomplete or invalid (an unknown provider, a key
//! variable that is not set, a base URL that is not one) falls back to the
//! next layer down, and the error is reported against that file.
//!
//! ```toml
//! # secretary.toml
//! name = "secretary"                # its senator name; applied at the next start
//! mode = "agent"                    # every [speaker] key reads here
//! confirmation_ttl_secs = 600
//! bot_name = "Speaker"              # how people address it in chat
//! instructions = "Answer in short sentences."
//! instructions_file = "secretary.md"   # relative to this file
//! duties = ["route", "forward", "cover", "triage"]   # default: all
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

use std::collections::{BTreeSet, HashMap, HashSet};
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
    /// Wall-clock bound on one run (`[speaker] brain_deadline_secs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_secs: Option<u64>,
    /// Wall-clock bound on one triage or pick call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triage_deadline_secs: Option<u64>,
    /// Token bound on one run (`[speaker] brain_token_budget`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u64>,
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
            deadline_secs: pick(&self.deadline_secs, &below.deadline_secs),
            triage_deadline_secs: pick(&self.triage_deadline_secs, &below.triage_deadline_secs),
            token_budget: pick(&self.token_budget, &below.token_budget),
        }
    }

    /// What a model client is built from: two settings with the same key
    /// share one client.
    pub fn connection_key(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            self.provider, self.model, self.api_key_env, self.base_url
        )
    }

    /// basis's rules: a provider it knows, a base URL that is one, a key
    /// variable that is set and says what it is for. Nothing is resolved
    /// beyond reading the variable.
    pub fn check_basis(&self) -> Result<(), String> {
        if self.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
            return Err("model is empty".into());
        }
        if let Some(provider) = &self.provider {
            basis::provider::parse(provider).map_err(|e| e.to_string())?;
        }
        if let Some(url) = &self.base_url {
            basis::provider::normalize_base_url(url).map_err(|e| e.to_string())?;
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
            if std::env::var(var).map_or(true, |v| v.is_empty()) {
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
}

/// The keys a `[speaker]` table held for its model, by their old names.
const OLD_MODEL_KEYS: [&str; 4] = [
    "model",
    "brain_deadline_secs",
    "triage_deadline_secs",
    "brain_token_budget",
];

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

/// What the main configuration says for the built-in session (layer 2).
#[derive(Debug, Clone, Default)]
pub struct BaseLayer {
    /// The deployment's data directory: the model's workspaces live in
    /// `<data_dir>/speaker/`.
    pub data_dir: PathBuf,
    /// The deployment's language.
    pub locale: Locale,
    /// An old `[speaker]` or a `[secretary]` table, read as if it were
    /// `secretary.toml` (model keys already taken out:
    /// [`split_model_keys`]), and which it was (`"speaker"`).
    pub compat: Option<(String, toml::Table)>,
    /// `[model]`, over the model keys found in the compatibility table.
    /// `Some` (even empty) says a model is configured.
    pub model: Option<ModelSettings>,
}

impl BaseLayer {
    /// The layer an older caller's whole [`SpeakerConfig`] makes: its
    /// behaviour as the compatibility table, its model keys as `[model]`.
    pub fn of_config(config: &SpeakerConfig) -> Self {
        let table = toml::Table::try_from(config).unwrap_or_default();
        let (behaviour, model) = split_model_keys(table);
        Self {
            data_dir: config.data_dir.clone(),
            locale: config.locale,
            compat: Some(("speaker".to_owned(), behaviour)),
            model: Some(model.unwrap_or_default()),
        }
    }
}

/// One error in a configuration file: shown by status, logged once per
/// change.
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

/// The built-in session's configuration, re-read as its files change. Shared
/// by the engine (which refreshes it before acting on a committee), the
/// Brain (which reads the model and the instructions per committee) and the
/// daemon (which shows its problems).
pub struct SecretaryConfigs {
    dir: Option<PathBuf>,
    base: BaseLayer,
    check: ModelCheck,
    name: SenatorName,
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
            state: Mutex::new(State::default()),
        };
        configs.name = configs.initial_name();
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
        let watched = self.watched(&state, committee);
        let stamps: Vec<(PathBuf, Stamp)> = watched
            .into_iter()
            .map(|p| {
                let s = stamp(&p);
                (p, s)
            })
            .collect();
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
        let stamps = self
            .watched(&state, committee)
            .into_iter()
            .map(|p| {
                let s = stamp(&p);
                (p, s)
            })
            .collect();
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
                .map(|name| (name.to_owned(), PathBuf::from(format!("[{section}]"))))
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
                    message: format!(
                        "name {name:?} is not a senator name ({error}); using {DEFAULT_NAME:?}"
                    ),
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
        match read_layer(path, committee_file) {
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

    /// Compute `committee`'s configuration from the files as they are now.
    /// `None` when it cannot be (the last good one then stays).
    fn compute(
        &self,
        state: &mut State,
        committee: Option<&CommitteeId>,
    ) -> (Option<Effective>, Vec<Problem>) {
        let mut problems = Vec::new();
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

        // The name: fixed at start.
        if let (Some(table), Some(path)) = (&file_table, &secretary_file)
            && let Some(name) = table.get("name").and_then(|v| v.as_str())
            && name != self.name.as_str()
        {
            let note = format!(
                "{}: name {name:?} applies at the next start; this one keeps {:?}",
                path.display(),
                self.name.as_str()
            );
            self.log_once(state, &note, || info!("{note}"));
        }

        // Layer 2: the deployment's defaults, then the compatibility table
        // unless secretary.toml says it all.
        let mut behaviour = toml::Table::new();
        behaviour.insert(
            "data_dir".into(),
            toml::Value::String(self.base.data_dir.to_string_lossy().into_owned()),
        );
        behaviour.insert(
            "locale".into(),
            toml::Value::String(self.base.locale.as_str().to_owned()),
        );
        if let Some((section, compat)) = &self.base.compat {
            match &secretary_file {
                Some(path) if state.files.get(path).is_some_and(|f| f.stamp.is_some()) => {
                    let note = format!(
                        "{} is read instead of the [{section}] table of the main configuration (the file wins); move what is still there into it",
                        path.display()
                    );
                    self.log_once(state, &note, || info!("{note}"));
                }
                _ => merge(&mut behaviour, compat),
            }
        }
        let mut model_layers: Vec<(PathBuf, ModelSettings)> = Vec::new();
        if let Some(model) = &self.base.model {
            model_layers.push((PathBuf::from("[model]"), model.clone()));
        }
        for (path, table) in [
            (&secretary_file, &file_table),
            (&committee_file, &committee_table),
        ] {
            let (Some(path), Some(table)) = (path, table) else {
                continue;
            };
            let mut table = table.clone();
            if let Some(model) = table.remove("model") {
                match toml::Value::try_into::<ModelSettings>(model) {
                    Ok(model) => model_layers.push((path.clone(), model)),
                    Err(error) => problems.push(Problem {
                        file: path.clone(),
                        line: None,
                        message: format!("[model]: {error}"),
                    }),
                }
            }
            merge(&mut behaviour, &table);
        }
        let file: SecretaryFile = match toml::Value::Table(behaviour).try_into() {
            Ok(file) => file,
            Err(error) => {
                let file = committee_file
                    .clone()
                    .or_else(|| secretary_file.clone())
                    .unwrap_or_default();
                problems.push(Problem {
                    file,
                    line: None,
                    message: error.to_string(),
                });
                return (None, problems);
            }
        };
        let model = self.model_of(&model_layers, &mut problems);
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
                        message: format!("instructions_file cannot be read: {error}"),
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

    /// The highest layer whose model is complete; each one above it that is
    /// not is reported.
    fn model_of(
        &self,
        layers: &[(PathBuf, ModelSettings)],
        problems: &mut Vec<Problem>,
    ) -> Option<ModelSettings> {
        for top in (0..layers.len()).rev() {
            let merged = layers[..=top]
                .iter()
                .fold(ModelSettings::default(), |below, (_, m)| m.over(&below));
            match (self.check)(&merged) {
                Ok(()) => return Some(merged),
                Err(error) => problems.push(Problem {
                    file: layers[top].0.clone(),
                    line: None,
                    message: if top == 0 {
                        format!("the model is incomplete: {error}; no model is in force")
                    } else {
                        format!(
                            "the model is incomplete: {error}; the next layer down is used ({})",
                            layers[top - 1].0.display()
                        )
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
        let model = self.base.model.clone().filter(|m| (self.check)(m).is_ok());
        if let Some(model) = &model {
            model.apply_to(&mut speaker);
        }
        Effective {
            name: self.name.clone(),
            speaker,
            model_configured: self.base.model.is_some(),
            model,
            instructions: None,
            instructions_file: None,
            duties: Duty::ALL.into_iter().collect(),
        }
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

/// The line where `key` is set at the top of `text`.
fn line_of_key(text: &str, key: &str) -> Option<usize> {
    text.lines()
        .position(|line| {
            line.trim_start()
                .strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
        .map(|i| i + 1)
}

/// One file, read and checked: a TOML table whose keys are what a
/// secretary file may say. A relative `instructions_file` is made relative
/// to the file's directory.
fn read_layer(path: &Path, committee_file: bool) -> Result<toml::Table, Problem> {
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
    // The keys and their types, with the line of the first that is wrong.
    if let Err(error) = toml::from_str::<SecretaryFile>(&text) {
        let line = error.span().map(|s| line_of(&text, s.start));
        return Err(problem(line, error.message().to_owned()));
    }
    if committee_file && table.contains_key("name") {
        return Err(problem(
            line_of_key(&text, "name"),
            "a committee's file may not set name: the built-in session has one name in the whole Senate; set it in secretary.toml".into(),
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
