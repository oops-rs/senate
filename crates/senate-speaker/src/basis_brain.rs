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
//!   other runs never see them. They answer per agent path: a person may
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
//! **History is durable**, under `<data_dir>/speaker/history`
//! (`RuntimeBuilder::with_store_dir`). No run is ever resumed — each call is a
//! fresh run built from the log, which stays the only memory — so the store
//! is a record, not state: it is the audit trail for "why did the Speaker
//! relay that", and unlike the ephemeral store it does not hold every run's
//! transcript in the server's memory for the life of the process.

mod agent_tools;
pub(crate) mod prompt;
mod tools;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use basis::runtime::FileToolProfile;
use basis::{
    AllowAll, Config, ModelSelector, NullSink, RunOutcome, RunProfile, RunReport, RunSpec, Runtime,
    RuntimeBuilder, ShellAccess, SystemPrompt, ToolRoster, Workspace,
};
use senate_core::{Brief, CommitteeId, Hub, Locale, TriagePayload};
use tokio::sync::Mutex;

use crate::brain::{
    AgentTurn, Answer, Brain, BrainError, Conversation, Intent, Pick, PickInput, Question,
    TriageInput, Utterance,
};
use crate::config::SpeakerConfig;
use agent_tools::{Checkpoint, CommitteeBrief, CommitteeStatus, Presence, Tasks};
use prompt::{AgentTurnWire, IntentWire, PickWire, TriageWire};
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

pub struct BasisBrain {
    hub: Arc<dyn Hub>,
    root: PathBuf,
    model: Option<String>,
    config: Config,
    recipe: RuntimeRecipe,
    deadline: Duration,
    triage_deadline: Duration,
    token_budget: u64,
    max_actions: usize,
    /// How people address the bot in chat (`@<bot_name>`).
    bot_name: String,
    /// The language of a workspace's own persona: the deployment's, unless
    /// `locales` names the committee's. Every run states its input's locale
    /// over it.
    locale: Locale,
    locales: BTreeMap<CommitteeId, Locale>,
    workspaces: Mutex<HashMap<String, Arc<Workspace>>>,
}

impl BasisBrain {
    /// The production Brain: provider and credential from basis's usual
    /// environment and global config (`ANTHROPIC_API_KEY`, `BASIS_BASE_URL`,
    /// …), the model from `config.model` when set, durable history under
    /// `<data_dir>/speaker/history`.
    pub fn new(config: &SpeakerConfig, hub: Arc<dyn Hub>) -> Self {
        let speaker_dir = config.speaker_dir();
        let history = speaker_dir.join("history");
        // The global config may name a provider or endpoint; a workspace
        // opened without discovery would not read it, so it is read here.
        let discovered = Config::discover_default(&speaker_dir).unwrap_or_default();
        let runtime_config = discovered.clone();
        let mut brain = Self::with_runtime(
            config,
            hub,
            Arc::new(move || {
                Runtime::builder()
                    .with_config(&runtime_config)
                    .with_store_dir(history.clone())
            }),
        );
        brain.config = discovered;
        brain
    }

    /// A Brain on runtimes `recipe` makes (tests, or a host with its own
    /// provider). Opens nothing until the first call.
    pub fn with_runtime(config: &SpeakerConfig, hub: Arc<dyn Hub>, recipe: RuntimeRecipe) -> Self {
        Self {
            hub,
            root: config.speaker_dir(),
            model: config.model.clone(),
            config: Config::default(),
            recipe,
            deadline: config.brain_deadline(),
            triage_deadline: config.triage_deadline(),
            token_budget: config.brain_token_budget,
            max_actions: config.agent_max_actions,
            bot_name: config.bot_name.clone(),
            locale: config.locale,
            locales: config.locales.clone(),
            workspaces: Mutex::new(HashMap::new()),
        }
    }

    async fn workspace(
        &self,
        committee: Option<&CommitteeId>,
    ) -> Result<Arc<Workspace>, BrainError> {
        let key = committee.map_or_else(|| UNBOUND_WORKSPACE.to_owned(), ToString::to_string);
        let mut workspaces = self.workspaces.lock().await;
        if let Some(workspace) = workspaces.get(&key) {
            return Ok(Arc::clone(workspace));
        }
        let path = self.root.join(&key);
        tokio::fs::create_dir_all(&path).await.map_err(|e| {
            BrainError::Unavailable(format!("cannot create {}: {e}", path.display()))
        })?;
        let runtime = (self.recipe)()
            .with_file_tools(FileToolProfile::None)
            .with_delegation_depth(0);
        let mut builder = Workspace::builder(&path)
            .without_discovery()
            .with_config(self.config.clone())
            .with_runtime_builder(runtime)
            .with_system_prompt(SystemPrompt::Replace(prompt::persona(
                committee,
                &self.bot_name,
                self.locale_of(committee),
            )))
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
        if let Some(model) = &self.model {
            builder = builder.with_model(ModelSelector::Id(model.clone()));
        }
        let workspace = Arc::new(
            builder
                .open()
                .await
                .map_err(|e| BrainError::Unavailable(format!("cannot open the workspace: {e}")))?,
        );
        workspaces.insert(key, Arc::clone(&workspace));
        Ok(workspace)
    }

    fn locale_of(&self, committee: Option<&CommitteeId>) -> Locale {
        committee
            .and_then(|c| self.locales.get(c).copied())
            .unwrap_or(self.locale)
    }

    /// The persona for one run, in the language of its input.
    fn speaking(&self, committee: Option<&CommitteeId>, locale: Locale) -> RunProfile {
        RunProfile::new().with_system_prompt(SystemPrompt::Replace(prompt::persona(
            committee,
            &self.bot_name,
            locale,
        )))
    }

    fn bounded(&self, spec: RunSpec, name: &str) -> RunSpec {
        spec.with_session_name(name)
            .with_deadline(self.deadline)
            .with_tool_budget(TOOL_BUDGET)
            .with_token_budget(self.token_budget)
    }

    /// A typed turn with no tools but its answer, bounded by the triage
    /// deadline.
    fn quick(&self, name: &str, committee: &CommitteeId, locale: Locale) -> RunSpec {
        RunSpec::default()
            .with_session_name(name)
            .with_deadline(self.triage_deadline)
            .with_tool_budget(1)
            .with_token_budget(self.token_budget)
            .with_profile(self.speaking(Some(committee), locale))
    }
}

#[async_trait]
impl Brain for BasisBrain {
    async fn interpret(&self, utterance: Utterance) -> Result<Intent, BrainError> {
        let workspace = self.workspace(utterance.committee.as_ref()).await?;
        let spec = self
            .bounded(RunSpec::default(), "speaker-interpret")
            .with_profile(self.speaking(utterance.committee.as_ref(), utterance.locale));
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
        let workspace = self.workspace(Some(&question.committee)).await?;
        let spec = self
            .bounded(RunSpec::new(prompt::answer(&question)), "speaker-answer")
            .with_profile(self.speaking(Some(&question.committee), question.locale));
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
        let workspace = self.workspace(Some(&brief.committee)).await?;
        let spec = self
            .bounded(
                RunSpec::new(prompt::brief_top(brief, locale)),
                "speaker-brief-top",
            )
            .with_profile(self.speaking(Some(&brief.committee), locale));
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
        let workspace = self.workspace(committee.as_ref()).await?;
        let profile = RunProfile::new()
            .with_tool_roster(ToolRoster::only(AGENT_TOOLS))
            .with_system_prompt(SystemPrompt::Replace(prompt::agent_persona(
                committee.as_ref(),
                &self.bot_name,
                conversation.utterance.locale,
                self.max_actions,
            )));
        let spec = self
            .bounded(RunSpec::default(), "speaker-converse")
            .with_tool_budget(AGENT_TOOL_BUDGET)
            .with_profile(profile);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<AgentTurnWire, _, _>(
                prompt::converse(&conversation, self.max_actions),
                prompt::agent_turn_spec(self.max_actions),
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
