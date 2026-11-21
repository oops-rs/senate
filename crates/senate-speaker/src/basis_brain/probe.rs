//! The model self-check: does the model configured for the Speaker do what
//! the Brain asks of it?
//!
//! [`BasisBrain::check_model`] probes each model a committee runs on (the
//! primary and, when configured, the fallback: [`ModelSettings::tiers`]),
//! one short run per check, under [`PROBE_DEADLINE`]:
//!
//! - **`tool_round_trip`**: an ordinary run with one probe-only tool,
//!   `probe_digest`, whose result the model cannot make up; the run must
//!   call it and repeat what it returned. A gateway that rejects the tool
//!   call ids the client sends back (some OpenAI-compatible gateways do,
//!   over Chat Completions, refusing `call_…`) fails here.
//! - **`terminal_tool`**: a typed turn with nothing but its answering tool;
//!   a model that ends the run without calling it ("run completed without
//!   invoking the expected terminal tool") fails here.
//! - **`intent`**, **`triage`**, **`pick`**: the Brain's own typed turns
//!   ([`interpret`](crate::Brain::interpret), [`triage`](crate::Brain::triage),
//!   [`pick_agent`](crate::Brain::pick_agent)) on canned inputs, through the
//!   same code the Speaker runs, so their schemas and parsing are what is
//!   checked.
//!
//! The result is data ([`ModelReport`], serde-able): `senated check-model`
//! prints it and exits non-zero on a failure, the daemon logs it once at
//! start, and `senated doctor` reads it.

use std::time::{Duration, Instant};

use basis::async_trait;
use basis::tools::{
    ParallelToolContext, RuntimeToolDescriptor, ToolDefinition, ToolExecutor, ToolResult,
};
use basis::{AllowAll, NullSink, OutputSpec, RunProfile, RunSpec, ToolRoster};
use senate_core::{Address, CommitteeId, Kind, Locale, Name, Roster, SenatorName};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{BasisBrain, final_text, tools::descriptor};
use crate::brain::{AgentCandidate, BrainError, GroupRef, PickInput, TriageInput, Utterance};
use crate::secretary::ModelSettings;

/// The probe-only tool. Registered on every workspace but outside every
/// roster: only the probe's own run is offered it.
pub const PROBE_DIGEST: &str = "probe_digest";
/// The bound on each check's run, or the configured one when tighter.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(20);
/// The answering tool's description of the `terminal_tool` check; also how
/// a scripted test finds it.
pub(crate) const PROBE_DESCRIPTION: &str =
    "Report that you are ready. Call this exactly once, with ready = true.";
/// The committee the probe's quick turns are asked about when it checks the
/// Senate as a whole. Nothing is read from it.
const PROBE_COMMITTEE: &str = "speaker-probe";

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckResult {
    /// `tool_round_trip`, `terminal_tool`, `intent`, `triage` or `pick`.
    pub check: String,
    pub passed: bool,
    /// What it answered when it passed; the exact error when it failed.
    pub reason: String,
    pub millis: u64,
}

/// The checks on one model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierReport {
    /// `primary` or `fallback`.
    pub tier: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub wire: Option<String>,
    pub checks: Vec<CheckResult>,
}

impl TierReport {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.passed)
    }
}

/// The self-check of the model a committee (or the Senate as a whole) runs
/// on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelReport {
    pub committee: Option<String>,
    /// Why nothing was probed: no usable model is configured.
    pub problem: Option<String>,
    pub tiers: Vec<TierReport>,
}

impl ModelReport {
    /// Every check on every model passed (and there was one to check).
    pub fn passed(&self) -> bool {
        self.problem.is_none()
            && !self.tiers.is_empty()
            && self.tiers.iter().all(TierReport::passed)
    }

    /// The failed checks, as `tier/check: reason` lines.
    pub fn failures(&self) -> Vec<String> {
        let mut out: Vec<String> = self.problem.iter().cloned().collect();
        for tier in &self.tiers {
            for check in tier.checks.iter().filter(|c| !c.passed) {
                out.push(format!("{}/{}: {}", tier.tier, check.check, check.reason));
            }
        }
        out
    }

    /// The report as a person reads it in a terminal.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(problem) = &self.problem {
            out.push_str(&format!("FAIL  {problem}\n"));
        }
        for tier in &self.tiers {
            let mut model = Vec::new();
            for (key, value) in [
                ("provider", &tier.provider),
                ("model", &tier.model),
                ("base_url", &tier.base_url),
                ("wire", &tier.wire),
            ] {
                if let Some(value) = value {
                    model.push(format!("{key}={value}"));
                }
            }
            out.push_str(&format!("{} model: {}\n", tier.tier, model.join(" ")));
            for check in &tier.checks {
                out.push_str(&format!(
                    "  {}  {:<16} {} ({} ms)\n",
                    if check.passed { "pass" } else { "FAIL" },
                    check.check,
                    check.reason,
                    check.millis
                ));
            }
        }
        out
    }
}

impl BasisBrain {
    /// Probe each model `committee` runs on (`None`: the Senate as a whole).
    /// Never fails: what goes wrong is in the report.
    pub async fn check_model(&self, committee: Option<&CommitteeId>) -> ModelReport {
        let settings = self.settings(committee);
        let mut report = ModelReport {
            committee: committee.map(ToString::to_string),
            problem: None,
            tiers: Vec::new(),
        };
        let Some(model) = settings.model.clone() else {
            report.problem = Some(BrainError::NoModel.to_string());
            return report;
        };
        let mut seen = Vec::new();
        for tier in model.tiers() {
            let key = tier.model.connection_key();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            // Boxed: five basis runs at once make a deep future.
            report
                .tiers
                .push(Box::pin(self.check_tier(committee, tier.label, &tier.model)).await);
        }
        report
    }

    async fn check_tier(
        &self,
        committee: Option<&CommitteeId>,
        label: &str,
        model: &ModelSettings,
    ) -> TierReport {
        let quick = committee.cloned().unwrap_or_else(|| {
            CommitteeId::new(PROBE_COMMITTEE).unwrap_or_else(|_| unreachable!("a valid id"))
        });
        let (round_trip, terminal, intent, triage, pick) = tokio::join!(
            timed(
                "tool_round_trip",
                Box::pin(self.round_trip(committee, model))
            ),
            timed("terminal_tool", Box::pin(self.terminal(committee, model))),
            timed(
                "intent",
                Box::pin(async {
                    self.interpret_on(model, &probe_utterance(committee))
                        .await
                        .map(|intent| format!("{intent:?}"))
                })
            ),
            timed(
                "triage",
                Box::pin(async {
                    self.triage_on(model, &probe_triage(&quick))
                        .await
                        .map(|t| format!("{:?}", t.verdict))
                })
            ),
            timed(
                "pick",
                Box::pin(async {
                    self.pick_on(model, &probe_pick(&quick))
                        .await
                        .map(|p| format!("{p:?}"))
                })
            ),
        );
        TierReport {
            tier: label.replace("-retry", ""),
            provider: model.provider.clone(),
            model: model.model.clone(),
            base_url: model
                .base_url
                .clone()
                .map(|u| crate::secretary::without_credentials(&u)),
            wire: model.wire.clone(),
            checks: vec![round_trip, terminal, intent, triage, pick],
        }
    }

    /// An ordinary run that must call [`PROBE_DIGEST`] and repeat its result.
    async fn round_trip(
        &self,
        committee: Option<&CommitteeId>,
        model: &ModelSettings,
    ) -> Result<String, BrainError> {
        let word = probe_word();
        let expected = digest(&word);
        let workspace = self.workspace(committee, model).await?;
        let spec = self
            .bounded(
                RunSpec::new(format!(
                    "This is a connectivity check. Call the {PROBE_DIGEST} tool once with the \
                     word \"{word}\". Then reply with exactly the text it returned and nothing \
                     else."
                )),
                "speaker-probe-tools",
                committee,
                model,
            )
            .with_deadline(self.probe_deadline(committee, model))
            .with_tool_budget(2)
            .with_profile(RunProfile::new().with_tool_roster(ToolRoster::only([PROBE_DIGEST])));
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let report = run
            .execute_with_approver(NullSink, AllowAll)
            .await
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let text = final_text(report)?;
        if text.contains(&expected) {
            Ok(format!("called {PROBE_DIGEST} and used its result"))
        } else {
            Err(BrainError::Invalid(format!(
                "the run ended without using {PROBE_DIGEST}'s result: answered {:?}",
                text.chars().take(120).collect::<String>()
            )))
        }
    }

    /// A typed turn with nothing but its answering tool.
    async fn terminal(
        &self,
        committee: Option<&CommitteeId>,
        model: &ModelSettings,
    ) -> Result<String, BrainError> {
        let workspace = self.workspace(committee, model).await?;
        let spec = self
            .bounded(
                RunSpec::default(),
                "speaker-probe-terminal",
                committee,
                model,
            )
            .with_deadline(self.probe_deadline(committee, model))
            .with_tool_budget(1);
        let mut run = workspace
            .prepare(spec)
            .map_err(|e| BrainError::Unavailable(e.to_string()))?;
        let output = run
            .output::<ReadyWire, _, _>(
                "This is a connectivity check. Call the speaker_probe tool exactly once with \
                 ready = true."
                    .to_owned(),
                OutputSpec::new(
                    "speaker_probe",
                    PROBE_DESCRIPTION,
                    json!({
                        "type": "object",
                        "properties": { "ready": { "type": "boolean" } },
                        "required": ["ready"],
                        "additionalProperties": false
                    }),
                ),
                NullSink,
                AllowAll,
            )
            .await
            .map_err(super::output_error)?;
        Ok(format!(
            "called speaker_probe (ready = {})",
            output.value.ready
        ))
    }

    fn probe_deadline(&self, committee: Option<&CommitteeId>, model: &ModelSettings) -> Duration {
        self.bounds(committee, model).deadline.min(PROBE_DEADLINE)
    }
}

#[derive(Debug, Deserialize)]
struct ReadyWire {
    ready: bool,
}

/// `name`'s result, timed, bounded by [`PROBE_DEADLINE`] (the quick turns
/// carry their own, tighter, triage deadline).
async fn timed(name: &str, check: impl Future<Output = Result<String, BrainError>>) -> CheckResult {
    let started = Instant::now();
    let result = tokio::time::timeout(PROBE_DEADLINE, check)
        .await
        .unwrap_or_else(|_| {
            Err(BrainError::Unavailable(format!(
                "no result within {} s",
                PROBE_DEADLINE.as_secs()
            )))
        });
    let millis = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let (passed, reason) = match result {
        Ok(found) => (true, found),
        Err(error) => (false, error.to_string()),
    };
    CheckResult {
        check: name.to_owned(),
        passed,
        reason,
        millis,
    }
}

fn probe_word() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("senate{nanos:x}")
}

/// What [`PROBE_DIGEST`] returns for `word`: nothing a model can make up.
pub(crate) fn digest(word: &str) -> String {
    let hash = Sha256::digest(format!("senate-probe:{word}").as_bytes());
    format!("digest-{}", &hex::encode(hash)[..12])
}

fn senator(name: &str) -> SenatorName {
    name.parse()
        .unwrap_or_else(|_| unreachable!("{name} is a valid name"))
}

fn probe_utterance(committee: Option<&CommitteeId>) -> Utterance {
    Utterance {
        text: "please tell alice that the build is green again".into(),
        speaker: Address::senator(&senator("pm")),
        committee: committee.cloned(),
        group: GroupRef {
            platform: "probe".into(),
            group_id: "probe".into(),
            group_name: None,
            simulated: false,
        },
        context: Vec::new(),
        roster: Roster::default(),
        seats: Vec::new(),
        locale: Locale::En,
        work: false,
        gone_relays: Vec::new(),
    }
}

fn probe_triage(committee: &CommitteeId) -> TriageInput {
    TriageInput {
        committee: committee.clone(),
        kind: Kind::Question,
        asker: Address::senator(&senator("pm")),
        to: vec![Address::senator(&senator("claude"))],
        body: "Which file defines the retry limit for the payments client?".into(),
        seq: None,
        context: Vec::new(),
        locale: Locale::En,
    }
}

fn probe_pick(committee: &CommitteeId) -> PickInput {
    let candidate = |name: &str, duty: &str| AgentCandidate {
        agent: senator(name),
        present: true,
        checkpoint: None,
        open_threads: Vec::new(),
        owns_thread: false,
        duty: Some(duty.into()),
    };
    PickInput {
        committee: committee.clone(),
        owner: Name::new("alice").ok(),
        asker: Address::senator(&senator("pm")),
        task: "Fix the failing payments backend build.".into(),
        thread: None,
        candidates: vec![
            candidate("claude", "payments backend"),
            candidate("codex", "documentation"),
        ],
        locale: Locale::En,
    }
}

/// [`PROBE_DIGEST`]: the digest of a word.
pub(crate) struct ProbeDigest;

impl ToolDefinition for ProbeDigest {
    fn descriptor(&self) -> RuntimeToolDescriptor {
        descriptor(
            PROBE_DIGEST,
            "Return the probe digest of `word`. Used only by the connectivity check.",
            json!({
                "type": "object",
                "properties": { "word": { "type": "string" } },
                "required": ["word"],
                "additionalProperties": false
            }),
        )
    }
}

#[async_trait]
impl ToolExecutor for ProbeDigest {
    async fn execute(&self, _ctx: ParallelToolContext, input: Value) -> ToolResult {
        let word = input
            .get("word")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .ok_or_else(|| "word is required".to_owned())?;
        Ok(digest(word))
    }
}
