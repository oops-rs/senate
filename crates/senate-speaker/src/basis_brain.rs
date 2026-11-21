//! The [`Brain`] on basis.
//!
//! One `basis::Workspace` per committee, at `<data_dir>/speaker/<committee>/`
//! (plus `_unbound/` for groups bound to none; `_` cannot start a committee
//! id, so the names never collide). Each is opened with:
//!
//! - `without_discovery()`: nothing in the directory or the home config
//!   reaches the prompt or the roster; the Speaker's posture is all stated
//!   here.
//! - `SystemPrompt::Replace(<persona>)`: concise, never invents decisions,
//!   says when unsure, cites seq numbers, tells people how to address it
//!   (`@<bot_name>`), and writes what people read in the committee's
//!   language. Prompts are English; every run replaces the persona through
//!   its [`RunProfile`] with one for the input's [`Locale`], so the language
//!   follows the call, not the workspace.
//! - `ShellAccess::Denied`, no file tools on its runtime
//!   (`FileToolProfile::None`), no delegation, and
//!   `ToolRoster::only(["log_read", "log_search"])`: the model can read the
//!   committee's log and nothing else.
//! - Two native tools over the [`Hub`], bound to that one committee.
//!
//! - Five more read-only native tools for agent mode (`committee_brief`,
//!   `committee_status`, `presence`, `tasks`, `checkpoint`), registered on
//!   every workspace but outside its roster: only a `converse` run is
//!   offered them, through its [`RunProfile`] (roster and persona), so the
//!   other runs never see them. They answer per agent: a person may
//!   have several agents, each with its own presence, checkpoints and tasks.
//!
//! `converse` (agent mode) is a typed turn like `interpret`
//! (`speaker_turn`: `reply`, `actions[]`, `awaiting_reply`, each action read
//! by the interpret rules) with all seven read-only tools and a larger tool
//! budget. It plans; it never acts: every action goes back to the engine.
//!
//! `triage` is a typed turn with no tools at all (`speaker_triage`: three
//! probabilities and a reason), bounded by `triage_deadline_secs`; the
//! threshold is applied to its probabilities, not taken from the model.
//!
//! `pick_agent` is a typed turn of the same kind (`speaker_pick`: an agent
//! path or null, and a reason), under the same deadline: given the task, who
//! asked, and per candidate its presence, its latest checkpoint and its open
//! task threads, it names one candidate or asks. Null, or anything that is not
//! a candidate, is [`Pick::Ask`].
//!
//! `interpret` is a typed turn (`run.output::<IntentWire>()` with the
//! hand-written schema in `prompt`) that keeps its two log tools, so the
//! model can look something up before it decides; `answer_on_behalf` is an
//! ordinary tool-using turn whose final message is the answer (in a
//! takeover, from that agent's own checkpoint files, the brief and the log
//! only, with a first line of [`NOT_COVERED`](prompt::NOT_COVERED) when they
//! do not cover the question), and `brief_top` an ordinary turn over the
//! brief alone (the prompt holds all it needs; the log tools stay in reach
//! but are not asked for). Every run is bounded by a deadline, a tool
//! budget, and a token budget, and every basis failure becomes a
//! [`BrainError`].
//!
//! **Tiers.** Every call runs on the committee's model tiers in order
//! ([`ModelSettings::tiers`]): the model, the model once more, then the
//! `[model] fallback` when one is configured; the first answer wins, each
//! failed attempt and the tier that answered are logged. A quick call
//! (triage, pick, crossing, offline) is bounded by the triage deadline as a
//! whole, so a timeout leaves no room for another tier but a fast failure
//! does. When every tier fails the Speaker falls back to its fixed syntax
//! (or, for a pick, to [`rule_pick`](crate::rule_pick)); with no model at
//! all the error is [`BrainError::NoModel`].
//!
//! **Self-check.** [`BasisBrain::check_model`] probes each model on the
//! same code paths ([`probe`]).
//!
//! **Configuration per committee.** Everything a run is bounded by, the
//! persona's bot name and language, the model, and this Senate's extra
//! instructions (appended after the persona's rules, never replacing them)
//! come from the built-in session's configuration for the run's committee
//! ([`SecretaryConfigs`]). Each committee's workspace is opened for the model
//! in force there and opened again, for that committee alone, when its model
//! changes.
//!
//! `decide_crossing` is a typed turn without tools (`speaker_crossing`:
//! forward, refuse with a reason, or leave), under the triage deadline. So
//! is `decide_offline` (`speaker_offline`: answer with its text, or hold),
//! for a question put to a child Senate that is offline.
//!
//! **History is durable**, under `<data_dir>/speaker/history`
//! (`RuntimeBuilder::with_store_dir`). No run is ever resumed — each call is a
//! fresh run built from the log, which stays the only memory — so the store
//! is a record, not state: it is the audit trail for "why did the Speaker
//! relay that", and unlike the ephemeral store it does not hold every run's
//! transcript in the server's memory for the life of the process.

mod agent_tools;
pub(crate) mod probe;
pub(crate) mod prompt;
mod tools;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use basis::runtime::FileToolProfile;
use basis::{
    AllowAll, Config, ModelSelector, NullSink, RunOutcome, RunProfile, RunReport, RunSpec, Runtime,
    RuntimeBuilder, ShellAccess, SystemPrompt, ToolRoster, Workspace,
};
use senate_core::{Brief, CommitteeId, Hub, Locale, TriagePayload};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::brain::{
    AgentTurn, Agreement, AgreementInput, Answer, Brain, BrainError, Conversation, Crossing,
    CrossingInput, DigestInput, DigestOut, Intent, Offline, OfflineInput, Pick, PickInput,
    Question, TriageInput, Utterance,
};
use crate::config::SpeakerConfig;
use crate::secretary::{Effective, ModelSettings, SecretaryConfigs, Tier};
use agent_tools::{Checkpoint, CommitteeBrief, CommitteeStatus, Presence, Tasks};
pub use probe::{CheckResult, ModelReport, PROBE_DEADLINE, TierReport};
use prompt::{
    AgentTurnWire, AgreementWire, CrossingWire, DigestWire, IntentWire, OfflineWire, PickWire,
    TriageWire,
};
use tools::{LogRead, LogSearch, MemorySearch};

/// The two tool names the Speaker's model is offered.
pub const LOG_READ: &str = "log_read";
pub const LOG_SEARCH: &str = "log_search";
/// The committee's memory ([`senate_core::memory`]), searched before the log.
pub const MEMORY_SEARCH: &str = "committee_memory";
/// The read-only tools an agent-mode (`converse`) run holds besides the log.
pub const COMMITTEE_BRIEF: &str = "committee_brief";
pub const COMMITTEE_STATUS: &str = "committee_status";
pub const PRESENCE: &str = "presence";
pub const TASKS: &str = "tasks";
pub const CHECKPOINT: &str = "checkpoint";
/// Every tool a `converse` run is offered (and the answering tool).
pub const AGENT_TOOLS: [&str; 8] = [
    MEMORY_SEARCH,
    LOG_READ,
    LOG_SEARCH,
    COMMITTEE_BRIEF,
    COMMITTEE_STATUS,
    PRESENCE,
    TASKS,
    CHECKPOINT,
];

const UNBOUND_WORKSPACE: &str = "_unbound";
/// Memory points looked up for a question or a conversation turn.
const KNOWN_POINTS: usize = 5;
/// Tool calls one run may make before it must answer.
const TOOL_BUDGET: usize = 8;
/// Tool calls one agent-mode turn may make: it may need the brief, a
/// checkpoint and a couple of log reads before it plans.
const AGENT_TOOL_BUDGET: usize = 16;

/// Makes the private runtime each workspace runs on: provider, credential,
/// history. The Brain adds its own posture on top (no file tools, no
/// delegation), whatever the recipe says.
pub type RuntimeRecipe = Arc<dyn Fn() -> RuntimeBuilder + Send + Sync>;

/// [`RuntimeRecipe`] for one model's settings: the provider, endpoint and
/// key `[model]` names. An error makes the committee's calls fail (the
/// Speaker falls back to its fixed syntax) until its model changes.
pub type ModelRecipe = Arc<dyn Fn(&ModelSettings) -> Result<RuntimeBuilder, String> + Send + Sync>;

/// A committee's workspaces, one per model it runs on (its tiers'
/// connections), by [`ModelSettings::connection_key`].
type Opened = HashMap<String, Arc<Workspace>>;

pub struct BasisBrain {
    hub: Arc<dyn Hub>,
    /// The configuration per committee, re-read by the engine.
    secretary: Arc<SecretaryConfigs>,
    config: Config,
    recipe: ModelRecipe,
    workspaces: Mutex<HashMap<String, Opened>>,
}

impl BasisBrain {
    /// The production Brain on one fixed configuration: provider and
    /// credential from `config`'s model keys, else basis's usual
    /// environment and global config (`ANTHROPIC_API_KEY`, `BASIS_BASE_URL`,
    /// …), durable history under `<data_dir>/speaker/history`.
    pub fn new(config: &SpeakerConfig, hub: Arc<dyn Hub>) -> Self {
        Self::on_secretary(Arc::new(SecretaryConfigs::fixed(config.clone())), hub)
    }

    /// The production Brain on the built-in session's configuration, per
    /// committee: each committee's model is built from its `[model]`
    /// ([`basis_runtime`]), with durable history under
    /// `<data_dir>/speaker/history`.
    pub fn on_secretary(secretary: Arc<SecretaryConfigs>, hub: Arc<dyn Hub>) -> Self {
        Self::on_secretary_with(secretary, hub, None)
    }

    /// The production Brain for [`check_model`](Self::check_model) alone:
    /// no provider retries, so a check fails with the provider's own reason
    /// (a refused connection, a rejected request) rather than a deadline
    /// spent retrying it.
    pub fn for_probe(secretary: Arc<SecretaryConfigs>, hub: Arc<dyn Hub>) -> Self {
        Self::on_secretary_with(secretary, hub, Some(0))
    }

    fn on_secretary_with(
        secretary: Arc<SecretaryConfigs>,
        hub: Arc<dyn Hub>,
        retries: Option<usize>,
    ) -> Self {
        let speaker_dir = secretary.effective(None).speaker.speaker_dir();
        let history = speaker_dir.join("history");
        // The global config may name a provider or endpoint; a workspace
        // opened without discovery would not read it, so it is read here.
        let discovered = Config::discover_default(&speaker_dir).unwrap_or_default();
        let runtime_config = discovered.clone();
        let mut brain = Self::with_models(
            secretary,
            hub,
            Arc::new(move |settings: &ModelSettings| {
                let runtime = basis_runtime(settings, &runtime_config, history.clone())?;
                Ok(match retries {
                    Some(budget) => runtime.with_provider_retry_budget(budget),
                    None => runtime,
                })
            }),
        );
        brain.config = discovered;
        brain
    }

    /// A Brain on runtimes `recipe` makes (tests, or a host with its own
    /// provider), on one fixed configuration. Opens nothing until the first
    /// call.
    pub fn with_runtime(config: &SpeakerConfig, hub: Arc<dyn Hub>, recipe: RuntimeRecipe) -> Self {
        Self::with_models(
            Arc::new(SecretaryConfigs::fixed(config.clone())),
            hub,
            Arc::new(move |_: &ModelSettings| Ok(recipe())),
        )
    }

    /// A Brain on the built-in session's configuration whose runtimes
    /// `recipe` makes from each committee's model.
    pub fn with_models(
        secretary: Arc<SecretaryConfigs>,
        hub: Arc<dyn Hub>,
        recipe: ModelRecipe,
    ) -> Self {
        Self {
            hub,
            secretary,
            config: Config::default(),
            recipe,
            workspaces: Mutex::new(HashMap::new()),
        }
    }

    /// The configuration in force for `committee`, as last refreshed.
    fn settings(&self, committee: Option<&CommitteeId>) -> Arc<Effective> {
        self.secretary.effective(committee)
    }

    /// `committee`'s workspace on `model` (one of its tiers), opened on
    /// first use. Workspaces on a model no tier names any more are closed.
    async fn workspace(
        &self,
        committee: Option<&CommitteeId>,
        model: &ModelSettings,
    ) -> Result<Arc<Workspace>, BrainError> {
        let settings = self.settings(committee);
        let connection = model.connection_key();
        let key = committee.map_or_else(|| UNBOUND_WORKSPACE.to_owned(), ToString::to_string);
        let mut workspaces = self.workspaces.lock().await;
        let opened = workspaces.entry(key.clone()).or_default();
        if let Some(workspace) = opened.get(&connection) {
            return Ok(Arc::clone(workspace));
        }
        // New, or its model changed: drop what no tier runs on, open this.
        let live: Vec<String> = settings
            .model
            .iter()
            .flat_map(ModelSettings::tiers)
            .map(|tier| tier.model.connection_key())
            .collect();
        opened.retain(|c, _| live.contains(c));
        let path = settings.speaker.speaker_dir().join(&key);
        tokio::fs::create_dir_all(&path).await.map_err(|e| {
            BrainError::Unavailable(format!("cannot create {}: {e}", path.display()))
        })?;
        let runtime = (self.recipe)(model)
            .map_err(BrainError::Unavailable)?
            .with_file_tools(FileToolProfile::None)
            .with_delegation_depth(0);
        let mut builder = Workspace::builder(&path)
            .without_discovery()
            .with_config(self.config.clone())
            .with_runtime_builder(runtime)
            .with_system_prompt(SystemPrompt::Replace(
                self.persona(committee, settings.speaker.locale_of_opt(committee)),
            ))
            .with_shell(ShellAccess::Denied)
            .with_tool_roster(ToolRoster::only([MEMORY_SEARCH, LOG_READ, LOG_SEARCH]))
            .with_tool(MemorySearch::new(Arc::clone(&self.hub), committee.cloned()))
            .with_tool(LogRead::new(Arc::clone(&self.hub), committee.cloned()))
            .with_tool(LogSearch::new(Arc::clone(&self.hub), committee.cloned()))
            .with_tool(CommitteeBrief::new(
                Arc::clone(&self.hub),
                committee.cloned(),
            ))
            .with_tool(CommitteeStatus::new(
                Arc::clone(&self.hub),
                committee.cloned(),
            ))
            .with_tool(Presence::new(Arc::clone(&self.hub), committee.cloned()))
            .with_tool(Tasks::new(Arc::clone(&self.hub), committee.cloned()))
            .with_tool(Checkpoint::new(Arc::clone(&self.hub), committee.cloned()))
            .with_tool(probe::ProbeDigest);
        if let Some(model) = &model.model {
            builder = builder.with_model(ModelSelector::Id(model.clone()));
        }
        let workspace = Arc::new(
            builder
                .open()
                .await
                .map_err(|e| BrainError::Unavailable(format!("cannot open the workspace: {e}")))?,
        );
        workspaces
            .entry(key)
            .or_default()
            .insert(connection, Arc::clone(&workspace));
        Ok(workspace)
    }

    /// What the committee knows that bears on `text`, looked up before the
    /// run so memory always comes first; empty when unbound or unreadable.
    async fn known(&self, committee: Option<&CommitteeId>, text: &str) -> String {
        let Some(committee) = committee else {
            return String::new();
        };
        match self
            .hub
            .memory_search(committee, text, KNOWN_POINTS, false)
            .await
        {
            Ok(hits) if !hits.is_empty() => tools::render_memory(&hits),
            Ok(_) => String::new(),
            Err(error) => {
                tracing::debug!(%committee, %error, "no memory for this run");
                String::new()
            }
        }
    }

    /// The persona of every run but `converse`, with this committee's extra
    /// instructions after its rules.
    fn persona(&self, committee: Option<&CommitteeId>, locale: Locale) -> String {
        let settings = self.settings(committee);
        prompt::with_instructions(
            prompt::persona(committee, &settings.speaker.bot_name, locale),
            settings.instructions.as_deref(),
        )
    }

    /// The persona for one run, in the language of its input.
    fn speaking(&self, committee: Option<&CommitteeId>, locale: Locale) -> RunProfile {
        RunProfile::new().with_system_prompt(SystemPrompt::Replace(self.persona(committee, locale)))
    }

    fn bounded(
        &self,
        spec: RunSpec,
        name: &str,
        committee: Option<&CommitteeId>,
        model: &ModelSettings,
    ) -> RunSpec {
        let bounds = self.bounds(committee, model);
        spec.with_session_name(name)
            .with_deadline(bounds.deadline)
            .with_tool_budget(TOOL_BUDGET)
            .with_token_budget(bounds.tokens)
    }

    /// A typed turn with no tools but its answer, bounded by the triage
    /// deadline.
    fn quick(
        &self,
        name: &str,
        committee: &CommitteeId,
        locale: Locale,
        model: &ModelSettings,
    ) -> RunSpec {
        let triage = self.bounds(Some(committee), model).triage;
        self.quick_within_deadline(name, committee, locale, model, triage)
    }

    /// [`quick`](Self::quick) bounded by `deadline` instead of the triage
    /// deadline: the model check measures how long a quick call takes.
    fn quick_within_deadline(
        &self,
        name: &str,
        committee: &CommitteeId,
        locale: Locale,
        model: &ModelSettings,
        deadline: Duration,
    ) -> RunSpec {
        let bounds = self.bounds(Some(committee), model);
        RunSpec::default()
            .with_session_name(name)
            .with_deadline(deadline)
            .with_tool_budget(1)
            .with_token_budget(bounds.tokens)
            .with_profile(self.speaking(Some(committee), locale))
    }

    /// What one run on `model` is bounded by: its own keys, else the
    /// committee's configuration.
    fn bounds(&self, committee: Option<&CommitteeId>, model: &ModelSettings) -> Bounds {
        let speaker = &self.settings(committee).speaker;
        Bounds {
            deadline: model
                .deadline_secs
                .map_or_else(|| speaker.brain_deadline(), Duration::from_secs),
            triage: model
                .triage_deadline_secs
                .map_or_else(|| speaker.triage_deadline(), Duration::from_secs),
            tokens: model.token_budget.unwrap_or(speaker.brain_token_budget),
        }
    }

    /// `attempt` on each of `committee`'s tiers in order
    /// ([`ModelSettings::tiers`]: the model, one retry, the fallback) until
    /// one answers. A quick call (`within`: the triage deadline) is bounded
    /// by it as a whole, so it never holds its committee longer; a fast
    /// failure still leaves room for the next tier. Every failed attempt is
    /// logged, and so is the tier that answered.
    async fn tiered<T, F, Fut>(
        &self,
        call: &'static str,
        committee: Option<&CommitteeId>,
        within: Option<Duration>,
        attempt: F,
    ) -> Result<T, BrainError>
    where
        F: Fn(Tier) -> Fut,
        Fut: Future<Output = Result<T, BrainError>>,
    {
        let Some(model) = self.settings(committee).model.clone() else {
            return Err(BrainError::NoModel);
        };
        let started = Instant::now();
        let tiers = model.tiers();
        let mut failure = None;
        for tier in tiers {
            let label = tier.label;
            let model_id = tier.model.model.clone().unwrap_or_default();
            let result = match within.map(|w| w.saturating_sub(started.elapsed())) {
                Some(left) if left.is_zero() => break,
                Some(left) => tokio::time::timeout(left, attempt(tier))
                    .await
                    .unwrap_or_else(|_| {
                        Err(BrainError::Unavailable(format!(
                            "no result within {} s",
                            within.unwrap_or_default().as_secs()
                        )))
                    }),
                None => attempt(tier).await,
            };
            match result {
                Ok(value) => {
                    info!(call, tier = label, model = %model_id, committee = ?committee.map(CommitteeId::as_str), "the brain answered");
                    return Ok(value);
                }
                Err(error) => {
                    warn!(call, tier = label, model = %model_id, committee = ?committee.map(CommitteeId::as_str), %error, "a brain attempt failed");
                    failure = Some(error);
                }
            }
        }
        Err(failure.unwrap_or_else(|| {
            BrainError::Unavailable(format!(
                "no result within {} s",
                within.unwrap_or_default().as_secs()
            ))
        }))
    }
}

/// What one run is bounded by.
struct Bounds {
    deadline: Duration,
    triage: Duration,
    tokens: u64,
}

/// The runtime for `settings` on basis: the provider, endpoint and key it
/// names (the key read from the variable `api_key_env` names), the rest
/// from `config` and the environment as basis resolves them, history in
/// `history`.
pub fn basis_runtime(
    settings: &ModelSettings,
    config: &Config,
    history: PathBuf,
) -> Result<RuntimeBuilder, String> {
    let mut builder = Runtime::builder()
        .with_config(config)
        .with_store_dir(history);
    if let Some(provider) = &settings.provider {
        builder =
            builder.with_provider(basis::provider::parse(provider).map_err(|e| e.to_string())?);
    }
    if let Some(url) = &settings.base_url {
        builder = builder.with_base_url(url.clone());
        if let Some(wire) = &settings.wire {
            builder = builder.with_wire(crate::secretary::wire_of(wire)?);
        }
    }
    if let Some(var) = &settings.api_key_env {
        let key = std::env::var(var)
            .ok()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| format!("the environment variable {var} is not set"))?;
        builder = builder.with_api_key(key);
    }
    Ok(builder)
}

/// One attempt of each call, on one tier's model. The probe
/// ([`BasisBrain::check_model`]) runs these same functions.
impl BasisBrain {
    pub(crate) async fn interpret_on(
        &self,
        model: &ModelSettings,
        utterance: &Utterance,
    ) -> Result<Intent, BrainError> {
        let work = utterance.work;
        let committee = utterance.committee.as_ref();
        let workspace = self.workspace(committee, model).await?;
        let spec = self
            .bounded(RunSpec::default(), "speaker-interpret", committee, model)
            .with_profile(self.speaking(committee, utterance.locale));
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<IntentWire, _, _>(
                prompt::interpret(utterance),
                prompt::intent_spec(utterance.work),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(output_error)?;
        output.value.into_intent(work)
    }

    async fn answer_on(
        &self,
        model: &ModelSettings,
        question: &Question,
    ) -> Result<Answer, BrainError> {
        let takeover = question.takeover.is_some();
        let committee = Some(&question.committee);
        let workspace = self.workspace(committee, model).await?;
        let known = self.known(committee, &question.event.body).await;
        let spec = self
            .bounded(
                RunSpec::new(prompt::answer(question, &known)),
                "speaker-answer",
                committee,
                model,
            )
            .with_profile(self.speaking(committee, question.locale));
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let report = run
            .execute_with_approver(NullSink, AllowAll)
            .await
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let text = final_text(report)?;
        Ok(if takeover {
            prompt::takeover_answer(&text)
        } else {
            Answer::covered(text)
        })
    }

    async fn agreement_on(
        &self,
        model: &ModelSettings,
        input: &AgreementInput,
    ) -> Result<Option<Agreement>, BrainError> {
        let committee = Some(&input.committee);
        let workspace = self.workspace(committee, model).await?;
        let spec = self
            .bounded(RunSpec::default(), "speaker-agreement", committee, model)
            .with_tool_budget(1)
            .with_profile(self.speaking(committee, input.locale));
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<AgreementWire, _, _>(
                prompt::agreement(input),
                prompt::agreement_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(output_error)?;
        Ok(output.value.into_agreement())
    }

    async fn digest_on(
        &self,
        model: &ModelSettings,
        input: &DigestInput,
    ) -> Result<DigestOut, BrainError> {
        let committee = Some(&input.committee);
        let workspace = self.workspace(committee, model).await?;
        let spec = self
            .bounded(RunSpec::default(), "speaker-digest", committee, model)
            .with_tool_budget(1)
            .with_profile(self.speaking(committee, input.locale));
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<DigestWire, _, _>(
                prompt::digest(input),
                prompt::digest_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(output_error)?;
        Ok(output.value.into_out())
    }

    async fn brief_top_on(
        &self,
        model: &ModelSettings,
        brief: &Brief,
        locale: Locale,
    ) -> Result<String, BrainError> {
        let committee = Some(&brief.committee);
        let workspace = self.workspace(committee, model).await?;
        let spec = self
            .bounded(
                RunSpec::new(prompt::brief_top(brief, locale)),
                "speaker-brief-top",
                committee,
                model,
            )
            .with_profile(self.speaking(committee, locale));
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let report = run
            .execute_with_approver(NullSink, AllowAll)
            .await
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        final_text(report)
    }

    pub(crate) async fn triage_on(
        &self,
        model: &ModelSettings,
        input: &TriageInput,
    ) -> Result<TriagePayload, BrainError> {
        let triage = self.bounds(Some(&input.committee), model).triage;
        self.triage_within(model, input, triage).await
    }

    /// [`triage_on`](Self::triage_on) bounded by `deadline`.
    pub(crate) async fn triage_within(
        &self,
        model: &ModelSettings,
        input: &TriageInput,
        deadline: Duration,
    ) -> Result<TriagePayload, BrainError> {
        let workspace = self.workspace(Some(&input.committee), model).await?;
        let spec = self.quick_within_deadline(
            "speaker-triage",
            &input.committee,
            input.locale,
            model,
            deadline,
        );
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<TriageWire, _, _>(
                prompt::triage(input),
                prompt::triage_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(output_error)?;
        output.value.into_payload()
    }

    async fn converse_on(
        &self,
        model: &ModelSettings,
        conversation: &Conversation,
    ) -> Result<AgentTurn, BrainError> {
        let committee = conversation.utterance.committee.clone();
        let settings = self.settings(committee.as_ref());
        let max_actions = settings.speaker.agent_max_actions;
        let workspace = self.workspace(committee.as_ref(), model).await?;
        let known = self
            .known(committee.as_ref(), &conversation.utterance.text)
            .await;
        let profile = RunProfile::new()
            .with_tool_roster(ToolRoster::only(AGENT_TOOLS))
            .with_system_prompt(SystemPrompt::Replace(prompt::with_instructions(
                prompt::agent_persona(
                    committee.as_ref(),
                    &settings.speaker.bot_name,
                    conversation.utterance.locale,
                    max_actions,
                ),
                settings.instructions.as_deref(),
            )));
        let spec = self
            .bounded(
                RunSpec::default(),
                "speaker-converse",
                committee.as_ref(),
                model,
            )
            .with_tool_budget(AGENT_TOOL_BUDGET)
            .with_profile(profile);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<AgentTurnWire, _, _>(
                prompt::converse(conversation, max_actions, &known),
                prompt::agent_turn_spec(max_actions, conversation.utterance.work),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(output_error)?;
        output.value.into_turn(conversation.utterance.work)
    }

    pub(crate) async fn pick_on(
        &self,
        model: &ModelSettings,
        input: &PickInput,
    ) -> Result<Pick, BrainError> {
        let triage = self.bounds(Some(&input.committee), model).triage;
        self.pick_within(model, input, triage).await
    }

    /// [`pick_on`](Self::pick_on) bounded by `deadline`.
    pub(crate) async fn pick_within(
        &self,
        model: &ModelSettings,
        input: &PickInput,
        deadline: Duration,
    ) -> Result<Pick, BrainError> {
        let workspace = self.workspace(Some(&input.committee), model).await?;
        let spec = self.quick_within_deadline(
            "speaker-pick",
            &input.committee,
            input.locale,
            model,
            deadline,
        );
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<PickWire, _, _>(
                prompt::pick(input, chrono::Utc::now()),
                prompt::pick_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(output_error)?;
        Ok(output.value.into_pick(&input.candidates))
    }

    async fn crossing_on(
        &self,
        model: &ModelSettings,
        input: &CrossingInput,
    ) -> Result<Crossing, BrainError> {
        let workspace = self.workspace(Some(&input.committee), model).await?;
        let spec = self.quick("speaker-crossing", &input.committee, input.locale, model);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<CrossingWire, _, _>(
                prompt::crossing(input),
                prompt::crossing_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(output_error)?;
        output.value.into_crossing()
    }

    async fn offline_on(
        &self,
        model: &ModelSettings,
        input: &OfflineInput,
    ) -> Result<Offline, BrainError> {
        let workspace = self.workspace(Some(&input.committee), model).await?;
        let spec = self.quick("speaker-offline", &input.committee, input.locale, model);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<OfflineWire, _, _>(
                prompt::offline(input, chrono::Utc::now()),
                prompt::offline_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(output_error)?;
        output.value.into_offline()
    }

    /// The triage deadline of `committee`: what a quick call's tiers share.
    fn quick_within(&self, committee: &CommitteeId) -> Option<Duration> {
        Some(self.settings(Some(committee)).speaker.triage_deadline())
    }
}

#[async_trait]
impl Brain for BasisBrain {
    async fn interpret(&self, utterance: Utterance) -> Result<Intent, BrainError> {
        let committee = utterance.committee.clone();
        self.tiered("interpret", committee.as_ref(), None, |tier| {
            let utterance = &utterance;
            async move { self.interpret_on(&tier.model, utterance).await }
        })
        .await
    }

    async fn answer_on_behalf(&self, question: Question) -> Result<Answer, BrainError> {
        self.tiered("answer", Some(&question.committee), None, |tier| {
            let question = &question;
            async move { self.answer_on(&tier.model, question).await }
        })
        .await
    }

    async fn brief_top(&self, brief: &Brief, locale: Locale) -> Result<String, BrainError> {
        self.tiered(
            "brief_top",
            Some(&brief.committee),
            None,
            |tier| async move { self.brief_top_on(&tier.model, brief, locale).await },
        )
        .await
    }

    async fn triage(&self, input: TriageInput) -> Result<TriagePayload, BrainError> {
        let within = self.quick_within(&input.committee);
        self.tiered("triage", Some(&input.committee), within, |tier| {
            let input = &input;
            async move { self.triage_on(&tier.model, input).await }
        })
        .await
    }

    async fn converse(&self, conversation: Conversation) -> Result<AgentTurn, BrainError> {
        let committee = conversation.utterance.committee.clone();
        self.tiered("converse", committee.as_ref(), None, |tier| {
            let conversation = &conversation;
            async move { self.converse_on(&tier.model, conversation).await }
        })
        .await
    }

    async fn pick_agent(&self, input: PickInput) -> Result<Pick, BrainError> {
        if input.candidates.is_empty() {
            return Ok(Pick::Ask);
        }
        let within = self.quick_within(&input.committee);
        self.tiered("pick", Some(&input.committee), within, |tier| {
            let input = &input;
            async move { self.pick_on(&tier.model, input).await }
        })
        .await
    }

    async fn decide_crossing(&self, input: CrossingInput) -> Result<Crossing, BrainError> {
        let within = self.quick_within(&input.committee);
        self.tiered("crossing", Some(&input.committee), within, |tier| {
            let input = &input;
            async move { self.crossing_on(&tier.model, input).await }
        })
        .await
    }

    async fn spot_agreement(&self, input: AgreementInput) -> Result<Option<Agreement>, BrainError> {
        self.tiered("agreement", Some(&input.committee), None, |tier| {
            let input = &input;
            async move { self.agreement_on(&tier.model, input).await }
        })
        .await
    }

    async fn digest(&self, input: DigestInput) -> Result<DigestOut, BrainError> {
        self.tiered("digest", Some(&input.committee), None, |tier| {
            let input = &input;
            async move { self.digest_on(&tier.model, input).await }
        })
        .await
    }

    async fn decide_offline(&self, input: OfflineInput) -> Result<Offline, BrainError> {
        let within = self.quick_within(&input.committee);
        self.tiered("offline", Some(&input.committee), within, |tier| {
            let input = &input;
            async move { self.offline_on(&tier.model, input).await }
        })
        .await
    }
}

/// Why a typed turn has no value: the run's own reason, with its cause
/// chain (a refused connection, the gateway's words), when it has one.
pub(crate) fn output_error<S>(failure: basis::OutputFailure<S>) -> BrainError {
    match failure.report.map(|report| report.outcome) {
        Some(RunOutcome::Error { message }) => BrainError::Unavailable(message),
        _ => BrainError::Unavailable(failure.error.to_string()),
    }
}

/// The final message of an ordinary turn, or why there is none.
fn final_text<S>(report: RunReport<S>) -> Result<String, BrainError> {
    if let RunOutcome::Error { message } = &report.outcome {
        return Err(BrainError::Unavailable(message.clone()));
    }
    report
        .final_message
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
        .ok_or_else(|| BrainError::Invalid("the run ended without an answer".into()))
}
