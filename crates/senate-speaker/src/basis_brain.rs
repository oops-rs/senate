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
//! [`BrainError`], on which the Speaker falls back to its fixed syntax (or,
//! for a pick, to [`rule_pick`](crate::rule_pick)).
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
//! forward, refuse with a reason, or leave), under the triage deadline.
//!
//! **History is durable**, under `<data_dir>/speaker/history`
//! (`RuntimeBuilder::with_store_dir`). No run is ever resumed — each call is a
//! fresh run built from the log, which stays the only memory — so the store
//! is a record, not state: it is the audit trail for "why did the Speaker
//! relay that", and unlike the ephemeral store it does not hold every run's
//! transcript in the server's memory for the life of the process.

mod agent_tools;
pub(crate) mod prompt;
mod tools;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use basis::runtime::FileToolProfile;
use basis::{
    AllowAll, Config, ModelSelector, NullSink, RunOutcome, RunProfile, RunReport, RunSpec, Runtime,
    RuntimeBuilder, ShellAccess, SystemPrompt, ToolRoster, Workspace,
};
use senate_core::{Brief, CommitteeId, Hub, Locale, TriagePayload};
use tokio::sync::Mutex;

use crate::brain::{
    AgentTurn, Answer, Brain, BrainError, Conversation, Crossing, CrossingInput, Intent, Pick,
    PickInput, Question, TriageInput, Utterance,
};
use crate::config::SpeakerConfig;
use crate::secretary::{Effective, ModelSettings, SecretaryConfigs};
use agent_tools::{Checkpoint, CommitteeBrief, CommitteeStatus, Presence, Tasks};
use prompt::{AgentTurnWire, CrossingWire, IntentWire, PickWire, TriageWire};
use tools::{LogRead, LogSearch};

/// The two tool names the Speaker's model is offered.
pub const LOG_READ: &str = "log_read";
pub const LOG_SEARCH: &str = "log_search";
/// The read-only tools an agent-mode (`converse`) run holds besides the log.
pub const COMMITTEE_BRIEF: &str = "committee_brief";
pub const COMMITTEE_STATUS: &str = "committee_status";
pub const PRESENCE: &str = "presence";
pub const TASKS: &str = "tasks";
pub const CHECKPOINT: &str = "checkpoint";
/// Every tool a `converse` run is offered (and the answering tool).
pub const AGENT_TOOLS: [&str; 7] = [
    LOG_READ,
    LOG_SEARCH,
    COMMITTEE_BRIEF,
    COMMITTEE_STATUS,
    PRESENCE,
    TASKS,
    CHECKPOINT,
];

const UNBOUND_WORKSPACE: &str = "_unbound";
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

/// One committee's open workspace, and the model it was opened for.
struct Opened {
    connection: String,
    workspace: Arc<Workspace>,
}

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
                basis_runtime(settings, &runtime_config, history.clone())
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

    async fn workspace(
        &self,
        committee: Option<&CommitteeId>,
    ) -> Result<Arc<Workspace>, BrainError> {
        let settings = self.settings(committee);
        let Some(model) = settings.model.clone() else {
            return Err(BrainError::Unavailable(
                "no usable model is configured for this committee".into(),
            ));
        };
        let connection = model.connection_key();
        let key = committee.map_or_else(|| UNBOUND_WORKSPACE.to_owned(), ToString::to_string);
        let mut workspaces = self.workspaces.lock().await;
        if let Some(opened) = workspaces.get(&key)
            && opened.connection == connection
        {
            return Ok(Arc::clone(&opened.workspace));
        }
        // New, or its model changed: (re)open this committee's alone.
        let path = settings.speaker.speaker_dir().join(&key);
        tokio::fs::create_dir_all(&path).await.map_err(|e| {
            BrainError::Unavailable(format!("cannot create {}: {e}", path.display()))
        })?;
        let runtime = (self.recipe)(&model)
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
            .with_tool_roster(ToolRoster::only([LOG_READ, LOG_SEARCH]))
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
            .with_tool(Checkpoint::new(Arc::clone(&self.hub), committee.cloned()));
        if let Some(model) = &model.model {
            builder = builder.with_model(ModelSelector::Id(model.clone()));
        }
        let workspace = Arc::new(
            builder
                .open()
                .await
                .map_err(|e| BrainError::Unavailable(format!("cannot open the workspace: {e}")))?,
        );
        workspaces.insert(
            key,
            Opened {
                connection,
                workspace: Arc::clone(&workspace),
            },
        );
        Ok(workspace)
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

    fn bounded(&self, spec: RunSpec, name: &str, committee: Option<&CommitteeId>) -> RunSpec {
        let settings = self.settings(committee);
        spec.with_session_name(name)
            .with_deadline(settings.speaker.brain_deadline())
            .with_tool_budget(TOOL_BUDGET)
            .with_token_budget(settings.speaker.brain_token_budget)
    }

    /// A typed turn with no tools but its answer, bounded by the triage
    /// deadline.
    fn quick(&self, name: &str, committee: &CommitteeId, locale: Locale) -> RunSpec {
        let settings = self.settings(Some(committee));
        RunSpec::default()
            .with_session_name(name)
            .with_deadline(settings.speaker.triage_deadline())
            .with_tool_budget(1)
            .with_token_budget(settings.speaker.brain_token_budget)
            .with_profile(self.speaking(Some(committee), locale))
    }
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

#[async_trait]
impl Brain for BasisBrain {
    async fn interpret(&self, utterance: Utterance) -> Result<Intent, BrainError> {
        let committee = utterance.committee.as_ref();
        let workspace = self.workspace(committee).await?;
        let spec = self
            .bounded(RunSpec::default(), "speaker-interpret", committee)
            .with_profile(self.speaking(committee, utterance.locale));
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<IntentWire, _, _>(
                prompt::interpret(&utterance),
                prompt::intent_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(|failure| BrainError::Unavailable(failure.error.to_string()))?;
        output.value.into_intent()
    }

    async fn answer_on_behalf(&self, question: Question) -> Result<Answer, BrainError> {
        let takeover = question.takeover.is_some();
        let committee = Some(&question.committee);
        let workspace = self.workspace(committee).await?;
        let spec = self
            .bounded(
                RunSpec::new(prompt::answer(&question)),
                "speaker-answer",
                committee,
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

    async fn brief_top(&self, brief: &Brief, locale: Locale) -> Result<String, BrainError> {
        let committee = Some(&brief.committee);
        let workspace = self.workspace(committee).await?;
        let spec = self
            .bounded(
                RunSpec::new(prompt::brief_top(brief, locale)),
                "speaker-brief-top",
                committee,
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

    async fn triage(&self, input: TriageInput) -> Result<TriagePayload, BrainError> {
        let workspace = self.workspace(Some(&input.committee)).await?;
        let spec = self.quick("speaker-triage", &input.committee, input.locale);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<TriageWire, _, _>(
                prompt::triage(&input),
                prompt::triage_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(|failure| BrainError::Unavailable(failure.error.to_string()))?;
        output.value.into_payload()
    }

    async fn converse(&self, conversation: Conversation) -> Result<AgentTurn, BrainError> {
        let committee = conversation.utterance.committee.clone();
        let settings = self.settings(committee.as_ref());
        let max_actions = settings.speaker.agent_max_actions;
        let workspace = self.workspace(committee.as_ref()).await?;
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
            .bounded(RunSpec::default(), "speaker-converse", committee.as_ref())
            .with_tool_budget(AGENT_TOOL_BUDGET)
            .with_profile(profile);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<AgentTurnWire, _, _>(
                prompt::converse(&conversation, max_actions),
                prompt::agent_turn_spec(max_actions),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(|failure| BrainError::Unavailable(failure.error.to_string()))?;
        output.value.into_turn()
    }

    async fn pick_agent(&self, input: PickInput) -> Result<Pick, BrainError> {
        if input.candidates.is_empty() {
            return Ok(Pick::Ask);
        }
        let workspace = self.workspace(Some(&input.committee)).await?;
        let spec = self.quick("speaker-pick", &input.committee, input.locale);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<PickWire, _, _>(
                prompt::pick(&input, chrono::Utc::now()),
                prompt::pick_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(|failure| BrainError::Unavailable(failure.error.to_string()))?;
        Ok(output.value.into_pick(&input.candidates))
    }

    async fn decide_crossing(&self, input: CrossingInput) -> Result<Crossing, BrainError> {
        let workspace = self.workspace(Some(&input.committee)).await?;
        let spec = self.quick("speaker-crossing", &input.committee, input.locale);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<CrossingWire, _, _>(
                prompt::crossing(&input),
                prompt::crossing_spec(),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(|failure| BrainError::Unavailable(failure.error.to_string()))?;
        output.value.into_crossing()
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
