//! The model self-check and the tiers, against scripted providers: each
//! probe check passes on a model that does what the Brain asks and fails,
//! with the provider's own reason, on one that does not (a gateway that
//! rejects tool call ids, a model that skips the terminal tool, a model
//! that is down); a failing model is retried once and then the fallback
//! answers; with no model at all the error is [`BrainError::NoModel`].

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use basis::runtime::{
    ContentBlock, ModelInfo, ProviderCapabilities, ProviderDescriptor, ProviderError,
    ProviderEventStream, Request, Response, Role, provider_event_stream_from_response,
};
use basis::{Provider, Runtime, async_trait};
use senate_core::{Locale, Roster};
use serde_json::{Value, json};

use super::hub::MemoryHub;
use super::rig::{GROUP, refund, user};
use crate::basis_brain::probe::{PROBE_DESCRIPTION, PROBE_DIGEST};
use crate::basis_brain::prompt::{
    AGENT_TURN_DESCRIPTION, INTENT_DESCRIPTION, PICK_DESCRIPTION, TRIAGE_DESCRIPTION,
};
use crate::secretary::{BaseLayer, ModelSettings, SecretaryConfigs};
use crate::{BasisBrain, Brain, BrainError, GroupRef, Intent, Utterance};

/// How a scripted model behaves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Behaviour {
    /// Does everything the Brain asks.
    Good,
    /// Refuses any request that carries a tool result back, as a gateway
    /// that rejects the client's tool call ids.
    RejectsToolIds,
    /// Answers every typed turn in prose instead of calling its tool.
    SkipsTerminal,
    /// Every request fails.
    Down,
}

const REJECTED: &str = "Invalid 'messages[2].tool_call_id': expected an ID that begins with 'fc'";
const DOWN: &str = "upstream connect error";

struct Scripted {
    name: &'static str,
    behaviour: Behaviour,
    calls: Arc<AtomicUsize>,
}

fn debug_text(request: &Request<'_>) -> String {
    format!("{:?}", request.messages)
}

/// The text between `start` and the next `end` in `text`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = text.find(start)? + start.len();
    let to = text[from..].find(end)? + from;
    Some(&text[from..to])
}

#[async_trait]
impl Provider for Scripted {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new("scripted")
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_model_listing: true,
            supports_streaming: true,
            supports_tool_calls: true,
            ..Default::default()
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![ModelInfo::new(self.name, "scripted")])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let messages = debug_text(&request);
        let carries_result = messages.contains("ToolResult");
        match self.behaviour {
            Behaviour::Down => return Err(ProviderError::InvalidRequest(DOWN.into())),
            Behaviour::RejectsToolIds if carries_result => {
                return Err(ProviderError::InvalidRequest(REJECTED.into()));
            }
            _ => {}
        }
        let terminal = request.tools.iter().find_map(|tool| {
            let payload = match tool.description.as_deref()? {
                INTENT_DESCRIPTION => json!({ "action": "status" }),
                TRIAGE_DESCRIPTION => json!({
                    "p_needs_human": 0.1, "p_agent": 0.8, "p_unsure": 0.1, "reason": "a file"
                }),
                PICK_DESCRIPTION => json!({ "agent": "claude", "reason": "backend" }),
                AGENT_TURN_DESCRIPTION => return None,
                PROBE_DESCRIPTION => json!({ "ready": true }),
                _ => return None,
            };
            Some((tool.name.clone(), payload))
        });
        let offers_digest = request.tools.iter().any(|t| t.name == PROBE_DIGEST);
        let content = match (terminal, self.behaviour) {
            (Some(_), Behaviour::SkipsTerminal) => vec![ContentBlock::text("Done.")],
            (Some((name, payload)), _) => vec![ContentBlock::ToolUse {
                id: format!("call_{call}"),
                name,
                input: payload,
            }],
            (None, _) if offers_digest && !carries_result => {
                let word = between(&messages, "word \\\"", "\\\"").unwrap_or("none");
                vec![ContentBlock::ToolUse {
                    id: format!("call_{call}"),
                    name: PROBE_DIGEST.to_owned(),
                    input: json!({ "word": word }),
                }]
            }
            (None, _) if carries_result => {
                let digest = between(&messages, "digest-", "\"").unwrap_or("");
                vec![ContentBlock::text(format!(
                    "digest-{}",
                    digest.trim_end_matches('\\')
                ))]
            }
            (None, _) => vec![ContentBlock::text("ok")],
        };
        let calls_a_tool = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
        Ok(provider_event_stream_from_response(Response {
            id: format!("scripted-{call}"),
            model: self.name.to_owned(),
            role: Role::Assistant,
            content,
            stop_reason: calls_a_tool.then(|| "tool_use".to_owned()),
            usage: None,
        }))
    }
}

/// A Brain whose primary model behaves as `primary` and, when given, whose
/// fallback behaves as `fallback`; the calls each model received.
struct Rig {
    brain: BasisBrain,
    primary_calls: Arc<AtomicUsize>,
    fallback_calls: Arc<AtomicUsize>,
    _dir: tempfile::TempDir,
}

fn rig(primary: Behaviour, fallback: Option<Behaviour>) -> Rig {
    let dir = tempfile::tempdir().expect("data dir");
    let mut model = ModelSettings {
        model: Some("primary-model".into()),
        triage_deadline_secs: Some(10),
        ..ModelSettings::default()
    };
    if fallback.is_some() {
        model.fallback = Some(Box::new(ModelSettings {
            model: Some("fallback-model".into()),
            ..ModelSettings::default()
        }));
    }
    let base = BaseLayer::new(dir.path(), Locale::En).with_model(model);
    let secretary = Arc::new(SecretaryConfigs::with_check(
        None,
        base,
        Arc::new(|_: &ModelSettings| Ok(())),
    ));
    let primary_calls = Arc::new(AtomicUsize::new(0));
    let fallback_calls = Arc::new(AtomicUsize::new(0));
    let (p, f) = (Arc::clone(&primary_calls), Arc::clone(&fallback_calls));
    let brain = BasisBrain::with_models(
        secretary,
        MemoryHub::new(),
        Arc::new(move |settings: &ModelSettings| {
            let builder = Runtime::builder()
                .with_ephemeral_history()
                .with_provider_retry_budget(0);
            Ok(match settings.model.as_deref() {
                Some("fallback-model") => builder.with_provider_instance(Scripted {
                    name: "fallback-model",
                    behaviour: fallback.unwrap_or(Behaviour::Down),
                    calls: Arc::clone(&f),
                }),
                _ => builder.with_provider_instance(Scripted {
                    name: "primary-model",
                    behaviour: primary,
                    calls: Arc::clone(&p),
                }),
            })
        }),
    );
    Rig {
        brain,
        primary_calls,
        fallback_calls,
        _dir: dir,
    }
}

fn utterance() -> Utterance {
    Utterance {
        text: "what is bound here?".into(),
        speaker: user("pm"),
        committee: Some(refund()),
        group: GroupRef {
            platform: "chat".into(),
            group_id: GROUP.into(),
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

fn check<'a>(report: &'a crate::ModelReport, tier: &str, name: &str) -> &'a crate::CheckResult {
    report
        .tiers
        .iter()
        .find(|t| t.tier == tier)
        .and_then(|t| t.checks.iter().find(|c| c.check == name))
        .unwrap_or_else(|| panic!("no {tier}/{name} in {report:?}"))
}

#[tokio::test]
async fn every_check_passes_on_a_model_that_does_what_the_brain_asks() {
    let rig = rig(Behaviour::Good, None);
    let report = rig.brain.check_model(None).await;
    assert!(report.passed(), "{}", report.render());
    assert_eq!(report.tiers.len(), 1, "the retry is not probed twice");
    let names: Vec<&str> = report.tiers[0]
        .checks
        .iter()
        .map(|c| c.check.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "tool_round_trip",
            "terminal_tool",
            "intent",
            "triage",
            "pick"
        ]
    );
    assert!(
        check(&report, "primary", "tool_round_trip")
            .reason
            .contains(PROBE_DIGEST)
    );
    assert_eq!(check(&report, "primary", "intent").reason, "Status");
    assert!(report.failures().is_empty());
    // It serializes as doctor reads it.
    let value: Value = serde_json::to_value(&report).expect("json");
    assert_eq!(value["tiers"][0]["checks"][0]["passed"], json!(true));
}

#[tokio::test]
async fn a_gateway_that_rejects_tool_call_ids_fails_the_round_trip_with_its_reason() {
    let rig = rig(Behaviour::RejectsToolIds, None);
    let report = rig.brain.check_model(None).await;
    assert!(!report.passed());
    let round_trip = check(&report, "primary", "tool_round_trip");
    assert!(!round_trip.passed);
    assert!(
        round_trip.reason.contains(REJECTED),
        "{}",
        round_trip.reason
    );
    // The typed turns carry no tool result back: they pass.
    assert!(check(&report, "primary", "terminal_tool").passed);
    assert!(check(&report, "primary", "triage").passed);
    assert_eq!(report.failures().len(), 1);
    assert!(report.render().contains("FAIL  tool_round_trip"));
}

#[tokio::test]
async fn a_model_that_skips_the_terminal_tool_fails_the_typed_checks() {
    let rig = rig(Behaviour::SkipsTerminal, None);
    let report = rig.brain.check_model(None).await;
    for name in ["terminal_tool", "intent", "triage", "pick"] {
        let c = check(&report, "primary", name);
        assert!(!c.passed, "{name} passed");
        assert!(c.reason.contains("terminal tool"), "{name}: {}", c.reason);
    }
    assert!(check(&report, "primary", "tool_round_trip").passed);
}

#[tokio::test]
async fn a_down_primary_and_a_good_fallback_are_reported_apart() {
    let rig = rig(Behaviour::Down, Some(Behaviour::Good));
    let report = rig.brain.check_model(None).await;
    assert_eq!(report.tiers.len(), 2);
    assert!(!report.tiers[0].passed());
    assert!(check(&report, "primary", "intent").reason.contains(DOWN));
    assert!(report.tiers[1].passed(), "{}", report.render());
    assert_eq!(report.tiers[1].model.as_deref(), Some("fallback-model"));
    assert!(!report.passed());
}

#[tokio::test]
async fn no_model_is_a_problem_not_a_check() {
    let dir = tempfile::tempdir().expect("dir");
    let secretary = Arc::new(SecretaryConfigs::new(
        None,
        BaseLayer::new(dir.path(), Locale::En),
    ));
    let brain = BasisBrain::with_models(
        secretary,
        MemoryHub::new(),
        Arc::new(|_: &ModelSettings| Err("unused".into())),
    );
    let report = brain.check_model(None).await;
    assert!(!report.passed());
    assert!(report.tiers.is_empty());
    assert!(report.problem.is_some());
    assert!(matches!(
        brain.interpret(utterance()).await,
        Err(BrainError::NoModel)
    ));
}

// ---- the tiers at runtime -------------------------------------------------

#[tokio::test]
async fn a_good_primary_answers_alone() {
    let rig = rig(Behaviour::Good, Some(Behaviour::Good));
    let intent = rig.brain.interpret(utterance()).await.expect("intent");
    assert_eq!(intent, Intent::Status);
    assert_eq!(rig.primary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.fallback_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_failing_primary_is_retried_once_then_the_fallback_answers() {
    let rig = rig(Behaviour::Down, Some(Behaviour::Good));
    let intent = rig.brain.interpret(utterance()).await.expect("intent");
    assert_eq!(intent, Intent::Status);
    assert_eq!(rig.primary_calls.load(Ordering::SeqCst), 2, "one retry");
    assert_eq!(rig.fallback_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_primary_that_skips_the_terminal_tool_falls_back_too() {
    let rig = rig(Behaviour::SkipsTerminal, Some(Behaviour::Good));
    let triage = rig
        .brain
        .triage(crate::TriageInput {
            committee: refund(),
            kind: senate_core::Kind::Question,
            asker: user("pm"),
            to: vec![user("alice")],
            body: "which file?".into(),
            seq: None,
            context: Vec::new(),
            locale: Locale::En,
        })
        .await
        .expect("triage");
    assert_eq!(triage.verdict, senate_core::TriageVerdict::Agent);
    assert_eq!(rig.fallback_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn when_every_tier_fails_the_last_error_comes_back() {
    let rig = rig(Behaviour::Down, Some(Behaviour::SkipsTerminal));
    let error = rig.brain.interpret(utterance()).await.expect_err("fails");
    assert!(error.to_string().contains("terminal tool"), "{error}");
    assert_eq!(rig.primary_calls.load(Ordering::SeqCst), 2);
    // Without a fallback: the model and its one retry.
    let alone = rig_alone();
    assert!(alone.brain.interpret(utterance()).await.is_err());
    assert_eq!(alone.primary_calls.load(Ordering::SeqCst), 2);
}

fn rig_alone() -> Rig {
    rig(Behaviour::Down, None)
}
