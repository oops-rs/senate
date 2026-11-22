//! The basis Brain against a scripted provider: the typed `interpret` output
//! parses, and the model is offered the two log tools and nothing else;
//! agent mode's `converse` gets the seven read-only tools, the turns of its
//! conversation, and a strict wire whose actions read like `interpret`'s;
//! `pick_agent` is one typed turn without tools that names a candidate or
//! asks. The prompts are English, name the bot as configured, list each
//! person's seated agents, and ask for what people read in the input's
//! language.
//!
//! Live-model tests run only with `SENATE_SPEAKER_LIVE=1` (and basis's
//! usual provider environment).

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use basis::runtime::{
    ContentBlock, ModelInfo, ProviderCapabilities, ProviderDescriptor, ProviderError,
    ProviderEventStream, Request, Response, Role, provider_event_stream_from_response,
};
use basis::{Provider, Runtime, async_trait};
use senate_core::{
    Address, CheckpointFiles, CheckpointPayload, CheckpointRecord, CheckpointTrigger, CommitteeId,
    Event, Hub, Kind, Locale, NewEvent, Roster, Seq, meta,
};
use serde_json::{Value, json};

use super::hub::MemoryHub;
use super::rig::{
    GROUP, agent, agent_path, assert_senate_vocabulary, bind_event, config, name, path, refund,
    roster, runner_of, user,
};
use crate::basis_brain::prompt::{
    AGENT_TURN_DESCRIPTION, AGREEMENT_DESCRIPTION, DIGEST_DESCRIPTION, INTENT_DESCRIPTION,
    PICK_DESCRIPTION, TRIAGE_DESCRIPTION,
};
use crate::basis_brain::{
    AGENT_TOOLS, CHECKPOINT, COMMITTEE_BRIEF, COMMITTEE_STATUS, MAX_CORRECTIONS, PRESENCE, TASKS,
};
use crate::{
    AgentCandidate, AgentTurn, BasisBrain, Brain, BrainError, Conversation, GroupRef, Intent,
    LOG_READ, LOG_SEARCH, Pick, PickInput, Question, RelayKind, SpeakerConfig, TriageInput, Turn,
    Utterance,
};

const PROVIDER: &str = "scripted";
const MODEL: &str = "scripted-model";

#[derive(Clone)]
enum Round {
    /// Call these tools by name.
    Call(Vec<(&'static str, Value)>),
    /// Call the generated answering tool with this payload.
    Answer(Value),
    Text(&'static str),
    Fail,
}

#[derive(Clone, Default)]
struct ScriptedModel {
    rounds: Arc<Mutex<VecDeque<Round>>>,
    requests: Arc<Mutex<Vec<Request<'static>>>>,
}

impl ScriptedModel {
    fn new(rounds: Vec<Round>) -> Self {
        Self {
            rounds: Arc::new(Mutex::new(rounds.into())),
            requests: Arc::default(),
        }
    }

    fn requests(&self) -> Vec<Request<'static>> {
        self.requests.lock().expect("requests").clone()
    }
}

#[async_trait]
impl Provider for ScriptedModel {
    fn descriptor(&self) -> ProviderDescriptor {
        ProviderDescriptor::new(PROVIDER)
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
        Ok(vec![ModelInfo::new(MODEL, PROVIDER)])
    }

    async fn stream(&self, request: Request<'_>) -> Result<ProviderEventStream, ProviderError> {
        let terminal = request
            .tools
            .iter()
            .find(|tool| {
                matches!(
                    tool.description.as_deref(),
                    Some(
                        INTENT_DESCRIPTION
                            | TRIAGE_DESCRIPTION
                            | AGENT_TURN_DESCRIPTION
                            | PICK_DESCRIPTION
                            | DIGEST_DESCRIPTION
                            | AGREEMENT_DESCRIPTION
                    )
                )
            })
            .map(|tool| tool.name.clone());
        let call = {
            let mut requests = self.requests.lock().expect("requests");
            requests.push(request.into_owned());
            requests.len()
        };
        let round = self
            .rounds
            .lock()
            .expect("rounds")
            .pop_front()
            .unwrap_or(Round::Text("(end of script)"));
        let content = match round {
            Round::Call(calls) => calls
                .into_iter()
                .enumerate()
                .map(|(i, (name, input))| ContentBlock::ToolUse {
                    id: format!("call-{call}-{i}"),
                    name: name.to_owned(),
                    input,
                })
                .collect(),
            Round::Answer(payload) => vec![ContentBlock::ToolUse {
                id: format!("answer-{call}"),
                name: terminal.expect("a typed turn offers its answering tool"),
                input: payload,
            }],
            Round::Text(text) => vec![ContentBlock::text(text)],
            Round::Fail => {
                return Err(ProviderError::InvalidRequest("scripted failure".into()));
            }
        };
        let calls_a_tool = content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
        Ok(provider_event_stream_from_response(Response {
            id: format!("scripted-{call}"),
            model: MODEL.to_owned(),
            role: Role::Assistant,
            content,
            stop_reason: calls_a_tool.then(|| "tool_use".to_owned()),
            usage: None,
        }))
    }
}

struct Fixture {
    hub: Arc<MemoryHub>,
    model: ScriptedModel,
    brain: BasisBrain,
    dir: tempfile::TempDir,
    log: Vec<Event>,
}

async fn fixture(rounds: Vec<Round>) -> Fixture {
    fixture_with(rounds, |_| {}).await
}

/// A fixture whose config `adjust` changes first (the bot name, the locale).
async fn fixture_with(rounds: Vec<Round>, adjust: impl FnOnce(&mut SpeakerConfig)) -> Fixture {
    let hub = MemoryHub::new();
    let mut log = vec![
        hub.append(bind_event(&refund(), GROUP))
            .await
            .expect("bind")
            .event,
    ];
    log.push(
        hub.append(NewEvent::new(
            refund(),
            Kind::Decision,
            agent("alice"),
            "退款上限定为 30 天",
        ))
        .await
        .expect("decision")
        .event,
    );
    let model = ScriptedModel::new(rounds);
    let dir = tempfile::tempdir().expect("data dir");
    let mut config = SpeakerConfig {
        data_dir: dir.path().into(),
        model: Some(MODEL.into()),
        ..config()
    };
    adjust(&mut config);
    let provider = model.clone();
    let brain = BasisBrain::with_runtime(
        &config,
        hub.clone(),
        Arc::new(move || {
            Runtime::builder()
                .with_provider_instance(provider.clone())
                .with_ephemeral_history()
                .with_provider_retry_budget(0)
        }),
    );
    Fixture {
        hub,
        model,
        brain,
        dir,
        log,
    }
}

fn group() -> GroupRef {
    GroupRef {
        platform: "chat".into(),
        group_id: GROUP.into(),
        group_name: None,
        simulated: false,
        simulated_by: None,
    }
}

fn utterance(fixture: &Fixture, text: &str) -> Utterance {
    Utterance {
        text: text.into(),
        speaker: user("pm"),
        committee: Some(refund()),
        group: group(),
        context: fixture.log.clone(),
        roster: roster(),
        seats: Vec::new(),
        locale: Locale::Zh,
        work: true,
        gone_relays: Vec::new(),
    }
}

/// The system prompt of every request the model was sent, in order.
fn systems(fixture: &Fixture) -> Vec<String> {
    fixture
        .model
        .requests()
        .iter()
        .map(|r| r.system.as_deref().unwrap_or_default().to_owned())
        .collect()
}

fn tool_names(request: &Request<'_>) -> BTreeSet<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

/// `(content, is_error)` of every tool result the model was shown.
fn tool_results(request: &Request<'_>) -> Vec<(String, bool)> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content.to_display_string(), *is_error)),
            _ => None,
        })
        .collect()
}

/// Thread 797 item 8: with no coding work configured, the answering tool
/// does not offer `work`, and a `work` answer anyway is out of shape.
#[tokio::test]
async fn interpret_offers_work_only_when_it_is_configured() {
    let fixture = fixture(vec![Round::Answer(json!({
        "action": "work",
        "target": "alice",
        "instruction": "在 README 末尾加一节"
    }))])
    .await;
    let said = Utterance {
        work: false,
        ..utterance(&fixture, "请在 README 末尾加一节")
    };
    assert!(fixture.brain.interpret(said).await.is_err());
    let requests = fixture.model.requests();
    let answering = requests[0]
        .tools
        .iter()
        .find(|tool| tool.description.as_deref() == Some(INTENT_DESCRIPTION))
        .expect("the answering tool");
    let schema = serde_json::to_string(answering).expect("tool");
    assert!(!schema.contains("\"work\""), "{schema}");
    assert!(schema.contains("There is no work action here"), "{schema}");
    assert!(schema.contains("\"request\""), "{schema}");
}

#[tokio::test]
async fn interpret_returns_a_typed_intent_with_only_the_log_tools_in_reach() {
    let fixture = fixture(vec![
        // Look something up, and try a tool it was never offered.
        Round::Call(vec![
            (LOG_SEARCH, json!({ "query": "退款", "limit": null })),
            ("shell", json!({ "command": "ls" })),
        ]),
        Round::Answer(json!({
            "action": "relay",
            "targets": ["alice"],
            "kind": "note",
            "body": "退款上限是 30 天"
        })),
    ])
    .await;

    let intent = fixture
        .brain
        .interpret(utterance(&fixture, "告诉 alice 的 agent，退款上限是 30 天"))
        .await
        .expect("a typed intent");
    assert_eq!(
        intent,
        Intent::Relay {
            targets: vec!["alice".into()],
            kind: RelayKind::Note,
            body: "退款上限是 30 天".into(),
        }
    );

    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let names = tool_names(request);
        let ordinary: BTreeSet<String> = request
            .tools
            .iter()
            .filter(|tool| tool.description.as_deref() != Some(INTENT_DESCRIPTION))
            .map(|tool| tool.name.clone())
            .collect();
        assert_eq!(
            ordinary,
            BTreeSet::from([
                crate::basis_brain::MEMORY_SEARCH.to_owned(),
                LOG_READ.to_owned(),
                LOG_SEARCH.to_owned(),
            ]),
            "offered {names:?}"
        );
        assert_eq!(
            names.len(),
            4,
            "the memory, the two log tools and the answering tool"
        );
        let system = request.system.as_deref().unwrap_or_default();
        assert!(
            system.contains("You are the Speaker"),
            "the persona replaces the prompt"
        );
        assert!(system.contains("refund-v2"));
    }
    let results = tool_results(&requests[1]);
    assert_eq!(results.len(), 2);
    let search = results
        .iter()
        .find(|(content, _)| content.contains("退款上限定为 30 天"))
        .expect("the search ran against the hub");
    assert!(!search.1);
    assert!(search.0.contains("seq 2"), "{}", search.0);
    assert!(
        search.0.contains("[senator::claude]"),
        "authors are mentions: {}",
        search.0
    );
    let shell = results
        .iter()
        .find(|(content, _)| content.contains("shell"))
        .expect("the shell call was answered");
    assert!(shell.1, "a tool outside the roster is refused: {}", shell.0);
    assert!(
        fixture.dir.path().join("speaker/refund-v2").is_dir(),
        "one workspace per committee"
    );
}

#[tokio::test]
async fn the_persona_states_the_rules_and_the_reply_language_follows_the_locale() {
    let fixture = fixture(vec![
        Round::Answer(json!({ "action": "status" })),
        Round::Answer(json!({ "action": "status" })),
    ])
    .await;
    fixture
        .brain
        .interpret(utterance(&fixture, "绑定了什么？"))
        .await
        .expect("intent");
    fixture
        .brain
        .interpret(Utterance {
            locale: Locale::En,
            ..utterance(&fixture, "what is this group bound to?")
        })
        .await
        .expect("intent");
    let systems = systems(&fixture);
    assert_eq!(systems.len(), 2);
    let (zh, en) = (&systems[0], &systems[1]);
    assert_senate_vocabulary(zh);
    assert_senate_vocabulary(en);
    assert!(
        zh.contains("Write everything people will read in Chinese."),
        "{zh}"
    );
    assert!(!zh.contains("in English."), "{zh}");
    assert!(
        en.contains("Write everything people will read in English."),
        "{en}"
    );
    // Every rule of the persona is there, in English.
    for rule in [
        "the only member of the group who speaks for the system",
        "Never invent decisions, numbers, times or commitments.",
        "say so plainly, and say what information is missing and whom to ask",
        "give the seq, e.g. (seq 118)",
        "you can only read the log",
        "add nothing, drop nothing",
        "Identifiers are never translated",
        "A person may have several agents",
        "an agent is written senator::<agent>, by its own name",
        "Never choose one of a person's agents yourself",
    ] {
        assert!(zh.contains(rule), "{rule} in {zh}");
    }
}

#[tokio::test]
async fn the_bot_name_comes_from_the_config() {
    let named = fixture_with(vec![Round::Answer(json!({ "action": "status" }))], |c| {
        c.bot_name = "Helper".into();
    })
    .await;
    named
        .brain
        .interpret(utterance(&named, "在吗"))
        .await
        .expect("intent");
    let named = systems(&named);
    let system = &named[0];
    assert!(
        system.contains("People address you in chat as @Helper."),
        "{system}"
    );

    let default = fixture(vec![Round::Answer(json!({ "action": "status" }))]).await;
    default
        .brain
        .interpret(utterance(&default, "在吗"))
        .await
        .expect("intent");
    assert!(systems(&default)[0].contains("as @Speaker."));
}

#[tokio::test]
async fn the_roster_lists_each_persons_seated_agents() {
    let fixture = fixture(vec![Round::Answer(json!({
        "action": "relay",
        "targets": ["codex"],
        "kind": "note",
        "body": "先 rebase"
    }))])
    .await;
    fixture.hub.seat(&path("codex"), "alice");
    let seated: Roster = fixture.hub.roster().await.expect("roster");
    let intent = fixture
        .brain
        .interpret(Utterance {
            roster: seated,
            seats: Vec::new(),
            ..utterance(&fixture, "告诉 alice 的 codex 先 rebase")
        })
        .await
        .expect("intent");
    assert_eq!(
        intent,
        Intent::Relay {
            targets: vec!["codex".into()],
            kind: RelayKind::Note,
            body: "先 rebase".into(),
        }
    );
    let prompt = format!("{:?}", fixture.model.requests()[0].messages);
    for needle in [
        "- alice (also called Alice, 爱丽丝): claude, codex",
        "- bob (also called Bob): copilot",
        "[senator::pm] said to you",
    ] {
        assert!(prompt.contains(needle), "{needle} in {prompt}");
    }
}

#[tokio::test]
async fn answer_on_behalf_reads_the_log_and_returns_the_final_message() {
    let fixture = fixture(vec![
        Round::Call(vec![(LOG_READ, json!({ "after": null, "limit": null }))]),
        Round::Text("退款上限是 30 天（seq 2）。"),
    ])
    .await;
    let asked = fixture
        .hub
        .append(
            NewEvent::new(refund(), Kind::Question, agent("bob"), "退款上限多少天？")
                .to(agent("alice")),
        )
        .await
        .expect("question")
        .event;

    let text = fixture
        .brain
        .answer_on_behalf(Question {
            event: asked,
            agent: agent_path("alice"),
            committee: refund(),
            context: fixture.log.clone(),
            takeover: None,
            locale: Locale::Zh,
        })
        .await
        .expect("an answer");
    assert_eq!(text, crate::Answer::covered("退款上限是 30 天（seq 2）。"));

    let requests = fixture.model.requests();
    assert_eq!(
        tool_names(&requests[0]),
        BTreeSet::from([
            crate::basis_brain::MEMORY_SEARCH.to_owned(),
            LOG_READ.to_owned(),
            LOG_SEARCH.to_owned(),
        ])
    );
    let prompt = format!("{:?}", requests[0].messages);
    assert!(prompt.contains("[speaker::claude]"), "{prompt}");
    assert!(
        requests[0]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("in Chinese.")
    );
    let results = tool_results(&requests[1]);
    assert!(
        results
            .iter()
            .any(|(content, error)| !error && content.contains("seq 3")),
        "log_read showed the latest events: {results:?}"
    );
}

fn alice_checkpoint() -> CheckpointRecord {
    CheckpointRecord {
        seq: Seq(7),
        ts: chrono::Utc::now(),
        agent: agent_path("alice"),
        body: "幂等键做完了".into(),
        checkpoint: CheckpointPayload {
            branch: "senate/refund-v2/alice".into(),
            commit: "a41f9c2e0000".into(),
            pushed: false,
            remote_url: None,
            files: CheckpointFiles {
                progress: Some("- 完成：幂等键用 Idempotency-Key".into()),
                decisions: None,
                open: Some("- seq 4 等 PM 定上限".into()),
            },
            session_transcript: false,
            trigger: CheckpointTrigger::Tool,
        },
    }
}

#[tokio::test]
async fn a_takeover_answers_from_the_checkpoint_and_says_when_it_does_not_cover() {
    let fixture = fixture(vec![
        Round::Text("据检查点 seq 7 的 PROGRESS.md，用 Idempotency-Key。"),
        Round::Text("[NOT_COVERED]\n检查点没提回调签名。"),
    ])
    .await;
    let asked = fixture
        .hub
        .append(
            NewEvent::new(
                refund(),
                Kind::Question,
                agent("bob"),
                "幂等键用哪个 header？",
            )
            .to(agent("alice")),
        )
        .await
        .expect("question")
        .event;
    let brief = fixture.hub.brief(&refund()).await.expect("brief");
    let question = Question {
        event: asked,
        agent: agent_path("alice"),
        committee: refund(),
        context: fixture.log.clone(),
        takeover: Some(crate::Takeover {
            paused: false,
            checkpoint: alice_checkpoint(),
            age: std::time::Duration::from_secs(3 * 3_600),
            brief: Some(brief),
        }),
        locale: Locale::Zh,
    };
    let covered = fixture
        .brain
        .answer_on_behalf(question.clone())
        .await
        .expect("an answer");
    assert_eq!(
        covered,
        crate::Answer::covered("据检查点 seq 7 的 PROGRESS.md，用 Idempotency-Key。")
    );
    let prompt = format!("{:?}", fixture.model.requests()[0].messages);
    for needle in [
        "seq 7",
        "3 hours ago",
        "senate/refund-v2/alice@a41f9c2",
        "not pushed",
        "- 完成：幂等键用 Idempotency-Key",
        "DECISIONS.md\\n(no such file)",
        "- seq 4 等 PM 定上限",
        "[NOT_COVERED]",
        "[speaker::claude] takeover",
        "退款上限定为 30 天",
        // Only that agent's own checkpoint, never a sibling's.
        "The checkpoint is this agent's own",
        "not even another agent of the same person",
        "do not promise anything on its behalf",
    ] {
        assert!(prompt.contains(needle), "{needle} in {prompt}");
    }

    let uncovered = fixture
        .brain
        .answer_on_behalf(question)
        .await
        .expect("an answer");
    assert_eq!(
        uncovered,
        crate::Answer::not_covered("检查点没提回调签名。")
    );
}

#[tokio::test]
async fn triage_is_one_typed_turn_without_tools_and_applies_the_threshold() {
    let judged = fixture(vec![Round::Answer(json!({
        "p_needs_human": 0.55,
        "p_agent": 0.40,
        "p_unsure": 0.05,
        "reason": "  要本人承诺上线时间  "
    }))])
    .await;
    let triage = judged
        .brain
        .triage(TriageInput {
            committee: refund(),
            kind: Kind::Question,
            asker: agent("bob"),
            to: vec![agent("alice")],
            body: "下周能上线吗？".into(),
            seq: Some(Seq(3)),
            context: judged.log.clone(),
            locale: Locale::Zh,
        })
        .await
        .expect("a verdict");
    assert_eq!(triage.verdict, senate_core::TriageVerdict::NeedsHuman);
    assert_eq!(triage.reason.as_deref(), Some("要本人承诺上线时间"));
    let requests = judged.model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].tools.len(),
        1,
        "only the answering tool: {:?}",
        tool_names(&requests[0])
    );
    let prompt = format!("{:?}", requests[0].messages);
    assert!(
        prompt.contains("下周能上线吗？"),
        "the root is in the prompt"
    );
    assert!(
        prompt.contains("lean toward needing the person"),
        "{prompt}"
    );
    assert!(prompt.contains("Write the reason in Chinese."), "{prompt}");

    // Out of range is out of shape.
    let wild = fixture(vec![Round::Answer(json!({
        "p_needs_human": 3.0, "p_agent": 0.0, "p_unsure": 0.0
    }))])
    .await;
    let input = TriageInput {
        committee: refund(),
        kind: Kind::Question,
        asker: agent("bob"),
        to: vec![agent("alice")],
        body: "?".into(),
        seq: None,
        context: Vec::new(),
        locale: Locale::En,
    };
    assert!(wild.brain.triage(input).await.is_err());
}

#[tokio::test]
async fn brief_top_answers_from_the_brief_alone() {
    let fixture = fixture(vec![
        Round::Text("  退款上限 30 天（seq 2）\n幂等键用 Idempotency-Key  "),
        Round::Text("Refunds are capped at 30 days (seq 2)."),
    ])
    .await;
    fixture
        .hub
        .append(
            NewEvent::new(refund(), Kind::Note, Address::Speaker, "旧的重点")
                .with_meta(meta::BRIEF_TOP, json!(true)),
        )
        .await
        .expect("old top");
    let brief = fixture.hub.brief(&refund()).await.expect("brief");
    assert!(brief.top.is_some());

    let top = fixture
        .brain
        .brief_top(&brief, Locale::Zh)
        .await
        .expect("a top");
    assert_eq!(top, "退款上限 30 天（seq 2）\n幂等键用 Idempotency-Key");
    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 1, "no lookups needed");
    let prompt = format!("{:?}", requests[0].messages);
    assert!(
        prompt.contains("退款上限定为 30 天"),
        "the brief is in the prompt"
    );
    assert!(prompt.contains("At most 3 lines"));
    assert!(prompt.contains("In Chinese;"), "{prompt}");
    assert!(
        !prompt.contains("旧的重点"),
        "the top being replaced is left out"
    );

    // The same brief for an English committee.
    fixture
        .brain
        .brief_top(&brief, Locale::En)
        .await
        .expect("a top");
    let requests = fixture.model.requests();
    let prompt = format!("{:?}", requests[1].messages);
    assert!(prompt.contains("In English;"), "{prompt}");
    assert!(
        requests[1]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("Write everything people will read in English.")
    );
}

#[tokio::test]
async fn a_provider_failure_is_a_brain_error() {
    let failing = fixture(vec![Round::Fail]).await;
    let error = failing
        .brain
        .interpret(utterance(&failing, "随便"))
        .await
        .expect_err("the provider failed");
    assert!(matches!(error, BrainError::Unavailable(_)), "{error:?}");
}

#[tokio::test]
async fn the_log_tools_refuse_politely_in_an_unbound_group() {
    let unbound = fixture(vec![
        Round::Call(vec![(LOG_READ, json!({ "after": null, "limit": null }))]),
        Round::Answer(json!({ "action": "status" })),
    ])
    .await;
    let said = Utterance {
        committee: None,
        context: Vec::new(),
        ..utterance(&unbound, "现在什么情况？")
    };
    assert_eq!(
        unbound.brain.interpret(said).await.expect("intent"),
        Intent::Status
    );
    let results = tool_results(&unbound.model.requests()[1]);
    assert!(
        results[0].1 && results[0].0.contains("not bound"),
        "{results:?}"
    );
    assert!(unbound.dir.path().join("speaker/_unbound").is_dir());
}

// ---- Agent mode: converse ---------------------------------------------

/// One action of the strict agent wire: every property present, null when it
/// does not apply.
fn action(fields: Value) -> Value {
    let mut all = json!({
        "action": null, "targets": null, "kind": null, "body": null, "supersedes": null,
        "committee": null, "level": null, "target": null, "instruction": null,
        "from": null, "to": null
    });
    for (key, value) in fields.as_object().expect("fields") {
        all[key] = value.clone();
    }
    all
}

fn conversation(fixture: &Fixture, text: &str, turns: Vec<Turn>) -> Conversation {
    Conversation {
        utterance: utterance(fixture, text),
        turns,
    }
}

async fn add_checkpoint(hub: &MemoryHub, who: Address, progress: &str) {
    let branch = runner_of(who.as_senator().expect("an agent")).to_string();
    let payload = CheckpointPayload {
        branch: format!("senate/refund-v2/{branch}"),
        commit: "a41f9c2e0000".into(),
        pushed: false,
        remote_url: None,
        files: CheckpointFiles {
            progress: Some(progress.into()),
            decisions: None,
            open: None,
        },
        session_transcript: false,
        trigger: CheckpointTrigger::Tool,
    };
    hub.append(
        NewEvent::new(refund(), Kind::Checkpoint, who, "重试路径做了一半")
            .with_meta(meta::CHECKPOINT, payload.value()),
    )
    .await
    .expect("checkpoint");
}

#[tokio::test]
async fn converse_plans_several_actions_with_the_read_only_tools() {
    let fixture = fixture(vec![
        Round::Call(vec![
            (COMMITTEE_BRIEF, json!({})),
            (COMMITTEE_STATUS, json!({ "agents": null })),
            (PRESENCE, json!({})),
            (TASKS, json!({ "state": null, "agent": null, "limit": null })),
            (CHECKPOINT, json!({ "agent": "alice" })),
        ]),
        Round::Answer(json!({
            "reply": "好的，下面两件事请确认。",
            "actions": [
                action(json!({ "action": "bind", "committee": "refund-v3" })),
                action(json!({ "action": "decide", "body": "重试上限 3 次" })),
                action(json!({ "action": "relay", "targets": ["alice"], "kind": "note", "body": "先别合并" })),
            ],
            "awaiting_reply": false
        })),
    ])
    .await;
    add_checkpoint(&fixture.hub, agent("alice"), "- 完成：重试路径的一半").await;

    let turn = fixture
        .brain
        .converse(conversation(
            &fixture,
            "把这个群改绑到 refund-v3，再记一下：重试上限 3 次，然后告诉 alice 的 agent 先别合并",
            Vec::new(),
        ))
        .await
        .expect("a turn");
    assert_eq!(
        turn,
        AgentTurn::act(
            "好的，下面两件事请确认。",
            vec![
                Intent::Bind {
                    committee: "refund-v3".into()
                },
                Intent::Decide {
                    body: "重试上限 3 次".into(),
                    supersedes: None
                },
                Intent::Relay {
                    targets: vec!["alice".into()],
                    kind: RelayKind::Note,
                    body: "先别合并".into()
                },
            ]
        )
    );

    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let offered: BTreeSet<String> = request
            .tools
            .iter()
            .filter(|tool| tool.description.as_deref() != Some(AGENT_TURN_DESCRIPTION))
            .map(|tool| tool.name.clone())
            .collect();
        assert_eq!(
            offered,
            AGENT_TOOLS.iter().map(|t| (*t).to_owned()).collect(),
            "the seven read-only tools and nothing else"
        );
        assert_eq!(request.tools.len(), AGENT_TOOLS.len() + 1);
        let system = request.system.as_deref().unwrap_or_default();
        assert_senate_vocabulary(system);
        assert!(
            system.contains("This is conversation mode"),
            "the agent persona: {system}"
        );
        assert!(
            system.contains("reply must never say anything has been done"),
            "{system}"
        );
        assert!(system.contains("at most 5"), "{system}");
        assert!(system.contains("@Speaker"), "{system}");
        assert!(
            system.contains("Write everything people will read in Chinese."),
            "{system}"
        );
        // A bind moves the rest of the plan; no promises for later.
        assert!(
            system.contains("every operation after a bind acts on the new committee"),
            "{system}"
        );
        assert!(
            system.contains("Never promise in reply to do something later"),
            "{system}"
        );
        // Only a clarifying question waits for an answer.
        assert!(
            system.contains(
                "awaiting_reply is true only when you are really waiting for the speaker to \
                 answer a clarifying question"
            ),
            "{system}"
        );
        assert!(
            system.contains("this is a refusal, not a question: awaiting_reply is false"),
            "{system}"
        );
    }
    let results = tool_results(&requests[1]);
    assert_eq!(results.len(), 5);
    for (content, is_error) in &results {
        assert!(!is_error, "{content}");
    }
    let all = results
        .iter()
        .map(|(c, _)| c.as_str())
        .collect::<Vec<_>>()
        .join("\n---\n");
    for needle in [
        "退款上限定为 30 天",
        "Bound groups",
        // A person with one agent: that agent's checkpoint.
        "Latest checkpoint of [senator::claude]",
        "- 完成：重试路径的一半",
    ] {
        assert!(all.contains(needle), "{needle} in {all}");
    }
}

#[tokio::test]
async fn converse_shows_the_earlier_turns_and_can_ask() {
    let fixture = fixture(vec![Round::Answer(json!({
        "reply": "要交给 bob 的是谁的工作？",
        "actions": [],
        "awaiting_reply": true
    }))])
    .await;
    let turn = fixture
        .brain
        .converse(conversation(
            &fixture,
            "那就交给 bob",
            vec![
                Turn {
                    speaker: user("pm"),
                    text: "她的工作要有人接".into(),
                },
                Turn {
                    speaker: Address::Speaker,
                    text: "你说的「她」是谁？".into(),
                },
            ],
        ))
        .await
        .expect("a turn");
    assert_eq!(turn, AgentTurn::ask("要交给 bob 的是谁的工作？"));
    let prompt = format!("{:?}", fixture.model.requests()[0].messages);
    for needle in [
        "[senator::pm]: 她的工作要有人接",
        "you (Speaker): 你说的「她」是谁？",
        "那就交给 bob",
        "退款上限定为 30 天",
    ] {
        assert!(prompt.contains(needle), "{needle} in {prompt}");
    }
}

#[tokio::test]
async fn converse_drops_none_and_refuses_a_plan_with_a_broken_step() {
    let none_only = fixture(vec![Round::Answer(json!({
        "reply": "",
        "actions": [action(json!({ "action": "status" })), { "action": "none", "reply": "在的" }],
        "awaiting_reply": false
    }))])
    .await;
    assert_eq!(
        none_only
            .brain
            .converse(conversation(&none_only, "在吗？绑定了什么", Vec::new()))
            .await
            .expect("a turn"),
        AgentTurn::act("在的", vec![Intent::Status])
    );

    // Out of shape every time: each tier (the model and its one retry) is
    // told why and may correct itself MAX_CORRECTIONS times, then gives up.
    let broken_plan = Round::Answer(json!({
        "reply": "",
        "actions": [
            action(json!({ "action": "decide", "body": "重试上限 3 次" })),
            action(json!({ "action": "relay", "targets": null, "body": "x" })),
        ],
        "awaiting_reply": false
    }));
    let per_tier = MAX_CORRECTIONS + 1;
    let broken = fixture(vec![broken_plan; 2 * per_tier]).await;
    let error = broken
        .brain
        .converse(conversation(&broken, "记一下，再转告", Vec::new()))
        .await
        .expect_err("a relay with no targets");
    assert!(matches!(error, BrainError::Invalid(_)), "{error:?}");
    let requests = broken.model.requests();
    assert_eq!(requests.len(), 2 * per_tier, "bounded per tier");
    let told = tool_results(&requests[1]);
    assert!(
        told.iter()
            .any(|(text, error)| *error && text.contains("actions[1]: relay needs targets")),
        "{told:?}"
    );

    // A turn without awaiting_reply is not a turn.
    let partial = fixture(vec![Round::Answer(
        json!({ "reply": "是谁？", "actions": [] }),
    )])
    .await;
    partial
        .brain
        .converse(conversation(&partial, "把她的工作交给 bob", Vec::new()))
        .await
        .expect_err("a turn missing a field falls back");

    let failing = fixture(vec![Round::Fail]).await;
    let error = failing
        .brain
        .converse(conversation(&failing, "随便", Vec::new()))
        .await
        .expect_err("the provider failed");
    assert!(matches!(error, BrainError::Unavailable(_)), "{error:?}");
}

#[tokio::test]
async fn the_agent_tools_are_committee_scoped_and_polite_when_unbound() {
    let fixture = fixture(vec![
        Round::Call(vec![
            (COMMITTEE_BRIEF, json!({})),
            (CHECKPOINT, json!({ "agent": "bob" })),
        ]),
        Round::Answer(json!({ "reply": "", "actions": [], "awaiting_reply": false })),
        Round::Call(vec![
            (PRESENCE, json!({})),
            (
                TASKS,
                json!({ "state": null, "agent": null, "limit": null }),
            ),
        ]),
        Round::Answer(json!({ "reply": "", "actions": [], "awaiting_reply": false })),
    ])
    .await;
    // Another committee's decision and checkpoint never reach refund-v2's tools.
    let other = CommitteeId::new("billing").expect("committee");
    fixture
        .hub
        .append(NewEvent::new(
            other.clone(),
            Kind::Decision,
            agent("bob"),
            "账单按月结",
        ))
        .await
        .expect("decision");
    let mut elsewhere = NewEvent::new(other, Kind::Checkpoint, agent("bob"), "账单做完了");
    elsewhere = elsewhere.with_meta(
        meta::CHECKPOINT,
        CheckpointPayload {
            branch: "senate/billing/bob".into(),
            commit: "b0b".into(),
            pushed: false,
            remote_url: None,
            files: CheckpointFiles::default(),
            session_transcript: false,
            trigger: CheckpointTrigger::Tool,
        }
        .value(),
    );
    fixture.hub.append(elsewhere).await.expect("checkpoint");

    fixture
        .brain
        .converse(conversation(&fixture, "现在怎么样", Vec::new()))
        .await
        .expect("a turn");
    let results = tool_results(&fixture.model.requests()[1]);
    let all = format!("{results:?}");
    assert!(!all.contains("账单"), "{all}");
    assert!(
        all.contains("[senator::copilot] has no checkpoint on refund-v2 yet"),
        "{all}"
    );

    let said = Conversation {
        utterance: Utterance {
            committee: None,
            context: Vec::new(),
            ..utterance(&fixture, "现在怎么样")
        },
        turns: Vec::new(),
    };
    fixture.brain.converse(said).await.expect("a turn");
    let results = tool_results(&fixture.model.requests()[3]);
    assert_eq!(results.len(), 2);
    for (content, is_error) in &results {
        assert!(*is_error && content.contains("not bound"), "{content}");
    }
}

#[tokio::test]
async fn the_agent_tools_answer_per_agent_when_a_person_has_several() {
    let fixture = fixture(vec![
        Round::Call(vec![
            (CHECKPOINT, json!({ "agent": "alice" })),
            (CHECKPOINT, json!({ "agent": "codex" })),
            (CHECKPOINT, json!({ "agent": "[senator::claude]" })),
            (COMMITTEE_STATUS, json!({ "agents": ["alice"] })),
            (
                TASKS,
                json!({ "state": null, "agent": "codex", "limit": null }),
            ),
        ]),
        Round::Answer(json!({ "reply": "", "actions": [], "awaiting_reply": false })),
    ])
    .await;
    let codex = path("codex");
    fixture.hub.seat(&codex, "alice");
    fixture.hub.connect(&refund(), &codex);
    add_checkpoint(&fixture.hub, agent("alice"), "- claude: 重试路径").await;
    fixture
        .hub
        .append(
            NewEvent::new(
                refund(),
                Kind::Question,
                agent("bob"),
                "codex 的迁移脚本好了吗？",
            )
            .to(Address::senator(&codex)),
        )
        .await
        .expect("question to codex");
    fixture
        .hub
        .append(
            NewEvent::new(
                refund(),
                Kind::Question,
                agent("bob"),
                "claude 的重试好了吗？",
            )
            .to(agent("alice")),
        )
        .await
        .expect("question to claude");

    fixture
        .brain
        .converse(conversation(&fixture, "alice 那边怎么样", Vec::new()))
        .await
        .expect("a turn");
    let results = tool_results(&fixture.model.requests()[1]);
    assert_eq!(results.len(), 5, "{results:?}");
    let find = |needle: &str| {
        results
            .iter()
            .find(|(content, _)| content.contains(needle))
            .unwrap_or_else(|| panic!("{needle} in {results:?}"))
            .clone()
    };
    // A person with several agents: the candidates, to call again with one.
    let (several, is_error) = find("has several agents here");
    assert!(is_error, "{several}");
    assert!(
        several.contains("[senator::claude], [senator::codex]"),
        "{several}"
    );
    // Each agent's own checkpoint, never its sibling's.
    let (none_yet, _) = find("[senator::codex] has no checkpoint");
    assert!(!none_yet.contains("重试路径"), "{none_yet}");
    let (own, _) = find("Latest checkpoint of [senator::claude]");
    assert!(own.contains("- claude: 重试路径"), "{own}");
    // A person's handle checks every agent of theirs.
    let (status, _) = find("Online agents");
    assert!(
        status.contains("Online agents: [senator::codex]")
            && status.contains("Offline agents: [senator::claude]"),
        "{status}"
    );
    // One agent's inbox, not its person's.
    let (tasks, _) = find("迁移脚本");
    assert!(!tasks.contains("重试好了吗"), "{tasks}");
}

#[tokio::test]
async fn interpret_cannot_reach_the_agent_tools() {
    let fixture = fixture(vec![
        Round::Call(vec![(COMMITTEE_BRIEF, json!({}))]),
        Round::Answer(json!({ "action": "recall" })),
    ])
    .await;
    assert_eq!(
        fixture
            .brain
            .interpret(utterance(&fixture, "你记得什么"))
            .await
            .expect("intent"),
        Intent::Recall
    );
    let requests = fixture.model.requests();
    assert!(!tool_names(&requests[0]).contains(COMMITTEE_BRIEF));
    let results = tool_results(&requests[1]);
    assert!(
        results[0].1,
        "an agent tool is refused outside converse: {results:?}"
    );
    assert!(
        !requests[0]
            .system
            .as_deref()
            .unwrap_or_default()
            .contains("This is conversation mode")
    );
}

// ---- Picking one of a person's agents ---------------------------------

fn pick_input(task: &str) -> PickInput {
    let thread = |root: u64, title: &str| {
        let mut board = senate_core::TaskBoard::new(roster().people());
        let root_event = NewEvent::new(refund(), Kind::Request, agent("bob"), title)
            .to(agent("alice"))
            .into_event(Seq(root), chrono::Utc::now());
        board.observe(&root_event);
        board.get(Seq(root)).expect("a thread").clone()
    };
    PickInput {
        committee: refund(),
        owner: Some(name("alice")),
        asker: user("pm"),
        task: task.into(),
        thread: Some(Seq(12)),
        candidates: vec![
            AgentCandidate {
                agent: agent_path("alice"),
                present: false,
                checkpoint: Some(alice_checkpoint()),
                open_threads: vec![thread(12, "幂等键的重试")],
                owns_thread: true,
                duty: None,
            },
            AgentCandidate {
                agent: path("codex"),
                present: true,
                checkpoint: None,
                open_threads: Vec::new(),
                owns_thread: false,
                duty: Some("reviews the payment gateway".into()),
            },
        ],
        locale: Locale::Zh,
    }
}

#[tokio::test]
async fn pick_agent_is_one_typed_turn_that_names_a_candidate_or_asks() {
    let fixture = fixture(vec![
        Round::Answer(json!({ "agent": "claude", "reason": "its own thread" })),
        Round::Answer(json!({ "agent": null, "reason": "either could" })),
    ])
    .await;
    let input = pick_input("把幂等键的重试做完");

    assert_eq!(
        fixture.brain.pick_agent(input.clone()).await,
        Ok(Pick::Agent(agent_path("alice")))
    );
    assert_eq!(
        fixture.brain.pick_agent(input.clone()).await,
        Ok(Pick::Ask),
        "null is asked"
    );

    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].tools.len(),
        1,
        "only the answering tool: {:?}",
        tool_names(&requests[0])
    );
    let prompt = format!("{:?}", requests[0].messages);
    for needle in [
        "把幂等键的重试做完",
        "[senator::pm]",
        "[senator::alice]",
        "seq 12",
        "- claude · away · the thread this was asked in is this agent's",
        "latest checkpoint: seq 7, 0 minutes ago, senate/refund-v2/alice@a41f9c2: 幂等键做完了",
        "open task threads: seq 12 \\\"幂等键的重试\\\"",
        "- codex · online",
        "duty: reviews the payment gateway",
        "duty: none set",
        "falls within exactly one agent's duty, pick that agent",
        "latest checkpoint: none",
        "answer null",
        "Do not pick by an agent's name alone",
    ] {
        assert!(prompt.contains(needle), "{needle} in {prompt}");
    }
}

#[tokio::test]
async fn a_pick_out_of_shape_or_unreachable_is_a_brain_error() {
    // No agent field at all: out of shape, so the Speaker falls back to its rule.
    let shapeless = fixture(vec![Round::Answer(json!({ "reason": "?" }))]).await;
    assert!(shapeless.brain.pick_agent(pick_input("x")).await.is_err());
    let failing = fixture(vec![Round::Fail]).await;
    assert!(matches!(
        failing.brain.pick_agent(pick_input("x")).await,
        Err(BrainError::Unavailable(_))
    ));
    // No candidates: nothing to ask the model.
    let idle = fixture(Vec::new()).await;
    let none = PickInput {
        candidates: Vec::new(),
        ..pick_input("x")
    };
    assert_eq!(idle.brain.pick_agent(none).await, Ok(Pick::Ask));
    assert!(idle.model.requests().is_empty());
}

/// Against a real model, only when asked for: `SENATE_SPEAKER_LIVE=1`.
#[tokio::test]
async fn live_interpret_understands_a_relay() {
    if std::env::var("SENATE_SPEAKER_LIVE").as_deref() != Ok("1") {
        return;
    }
    let hub = MemoryHub::new();
    hub.append(bind_event(&refund(), GROUP))
        .await
        .expect("bind");
    let dir = tempfile::tempdir().expect("data dir");
    let config = SpeakerConfig {
        data_dir: dir.path().into(),
        model: std::env::var("SENATE_SPEAKER_LIVE_MODEL").ok(),
        ..config()
    };
    let brain = BasisBrain::new(&config, hub.clone());
    let intent = brain
        .interpret(Utterance {
            text: "告诉 alice 的 agent，退款上限是 30 天".into(),
            speaker: user("pm"),
            committee: Some(refund()),
            group: group(),
            context: hub.events(&refund()),
            roster: roster(),
            seats: Vec::new(),
            locale: Locale::Zh,
            work: false,
            gone_relays: Vec::new(),
        })
        .await
        .expect("the live model answers");
    let Intent::Relay { targets, body, .. } = intent else {
        panic!("expected a relay, got {intent:?}");
    };
    assert_eq!(targets, vec!["alice".to_owned()]);
    assert!(body.contains("30"), "{body}");
}

// ----- the configuration per committee ---------------------------------------------

/// Models are complete when they name one of the scripted providers.
fn scripted_check() -> crate::ModelCheck {
    Arc::new(|m: &crate::ModelSettings| match m.provider.as_deref() {
        Some("alpha" | "beta") => Ok(()),
        Some(other) => Err(format!("unknown provider {other:?}")),
        None => Err("no provider".into()),
    })
}

/// A Brain on the files in `home`, whose runtimes are scripted models told
/// apart by `<provider>/<model>`; every one made is kept in `made`.
struct Configured {
    hub: Arc<MemoryHub>,
    secretary: Arc<crate::SecretaryConfigs>,
    brain: BasisBrain,
    made: Arc<Mutex<std::collections::BTreeMap<String, ScriptedModel>>>,
    home: tempfile::TempDir,
}

impl Configured {
    async fn new(files: &[(&str, &str)]) -> Self {
        let home = tempfile::tempdir().expect("home");
        for (name, text) in files {
            std::fs::write(home.path().join(name), text).expect("write");
        }
        let hub = MemoryHub::new();
        for committee in [refund(), other()] {
            hub.append(NewEvent::new(
                committee,
                Kind::Decision,
                agent("alice"),
                "退款上限定为 30 天",
            ))
            .await
            .expect("decision");
        }
        let base = crate::BaseLayer::new(home.path().join("data"), Locale::En).with_model(
            crate::ModelSettings {
                provider: Some("alpha".into()),
                model: Some("m-1".into()),
                ..crate::ModelSettings::default()
            },
        );
        let secretary = Arc::new(crate::SecretaryConfigs::with_check(
            Some(home.path().into()),
            base,
            scripted_check(),
        ));
        let made: Arc<Mutex<std::collections::BTreeMap<String, ScriptedModel>>> = Arc::default();
        let models = made.clone();
        let brain = BasisBrain::with_models(
            secretary.clone(),
            hub.clone(),
            Arc::new(move |settings: &crate::ModelSettings| {
                let key = format!(
                    "{}/{}",
                    settings.provider.as_deref().unwrap_or("-"),
                    settings.model.as_deref().unwrap_or("-")
                );
                let model = models
                    .lock()
                    .expect("models")
                    .entry(key)
                    .or_insert_with(|| ScriptedModel::new(Vec::new()))
                    .clone();
                Ok(Runtime::builder()
                    .with_provider_instance(model)
                    .with_ephemeral_history()
                    .with_provider_retry_budget(0))
            }),
        );
        Self {
            hub,
            secretary,
            brain,
            made,
            home,
        }
    }

    /// One brief-top run on `committee`, after the refresh the engine makes
    /// before it acts there.
    async fn top(&self, committee: &CommitteeId) {
        self.secretary.refresh(Some(committee));
        let brief = self.hub.brief(committee).await.expect("brief");
        self.brain
            .brief_top(&brief, Locale::En)
            .await
            .expect("a top");
    }

    /// The system prompts each model was sent, by `<provider>/<model>`.
    fn systems(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.made
            .lock()
            .expect("models")
            .iter()
            .map(|(key, model)| {
                (
                    key.clone(),
                    model
                        .requests()
                        .iter()
                        .map(|r| r.system.as_deref().unwrap_or_default().to_owned())
                        .collect(),
                )
            })
            .collect()
    }
}

fn other() -> CommitteeId {
    CommitteeId::new("billing").expect("committee")
}

#[tokio::test]
async fn extra_instructions_reach_that_committees_prompt_and_the_rules_remain() {
    let configured = Configured::new(&[
        ("secretary.toml", "instructions = \"Sign every reply.\"\n"),
        (
            "refund-v2.secretary.toml",
            "instructions = \"Never promise a ship date.\"\ninstructions_file = \"refund.md\"\n",
        ),
        ("refund.md", "Cite the brief.\n"),
    ])
    .await;
    configured.top(&refund()).await;
    configured.top(&other()).await;
    let systems = configured.systems();
    let prompts = &systems["alpha/m-1"];
    assert_eq!(prompts.len(), 2);
    let rules = |committee: &CommitteeId| {
        crate::basis_brain::prompt::persona(Some(committee), "Speaker", Locale::En)
    };
    // The engine's rules come first, whole; the instructions after them.
    assert!(prompts[0].starts_with(&rules(&refund())), "{}", prompts[0]);
    assert!(prompts[0].contains("Never promise a ship date.\n\nCite the brief."));
    assert!(
        !prompts[0].contains("Sign every reply."),
        "the committee's file overrides the key"
    );
    assert!(prompts[1].starts_with(&rules(&other())), "{}", prompts[1]);
    assert!(prompts[1].contains("Sign every reply."));
    assert!(
        !prompts[1].contains("Never promise a ship date.")
            && !prompts[1].contains("Cite the brief."),
        "that committee's only"
    );
    assert!(prompts[1].contains("never replace them"));

    // The instructions file edited: the next run on that committee reads it.
    std::fs::write(
        configured.home.path().join("refund.md"),
        "Quote the brief's seq numbers, and nothing else.\n",
    )
    .expect("edit");
    configured.top(&refund()).await;
    let systems = configured.systems();
    assert!(systems["alpha/m-1"][2].contains("Quote the brief's seq numbers"));
}

#[tokio::test]
async fn a_committee_file_switches_the_model_for_that_committee_only() {
    let configured =
        Configured::new(&[("refund-v2.secretary.toml", "[model]\nmodel = \"m-2\"\n")]).await;
    configured.top(&refund()).await;
    configured.top(&other()).await;
    let systems = configured.systems();
    assert_eq!(systems["alpha/m-2"].len(), 1, "{systems:?}");
    assert_eq!(systems["alpha/m-1"].len(), 1, "{systems:?}");

    // A provider-level override: the model id is inherited.
    std::fs::write(
        configured.home.path().join("refund-v2.secretary.toml"),
        "[model]\nprovider = \"beta\"\n",
    )
    .expect("edit");
    configured.top(&refund()).await;
    configured.top(&other()).await;
    let systems = configured.systems();
    assert_eq!(systems["beta/m-1"].len(), 1, "{systems:?}");
    assert_eq!(systems["alpha/m-1"].len(), 2, "{systems:?}");
    assert!(configured.secretary.problems().is_empty());
}

#[tokio::test]
async fn an_incomplete_committee_model_falls_back_and_is_reported() {
    let configured = Configured::new(&[(
        "refund-v2.secretary.toml",
        "[model]\nprovider = \"gamma\"\nmodel = \"m-9\"\n",
    )])
    .await;
    configured.top(&refund()).await;
    let systems = configured.systems();
    assert_eq!(
        systems.keys().collect::<Vec<_>>(),
        ["alpha/m-1"],
        "{systems:?}"
    );
    let problems = configured.secretary.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(
        problems[0].file.ends_with("refund-v2.secretary.toml"),
        "{problems:?}"
    );
}

// ----- the seats on the committee, and relaying by duty ------------------------------

const DEPLOY_DUTY: &str = "负责回答 退款服务的部署、架构和使用问题";

/// The committee's seats as the engine gives them: an agent here and two
/// seats of the child Senate `carol-desk`, one of them away.
fn seats() -> Vec<crate::CommitteeSeat> {
    vec![
        crate::CommitteeSeat {
            paused: false,
            target: "codex".into(),
            shown: None,
            duty: Some("owns the retry path".into()),
            online: true,
            presence: None,
        },
        crate::CommitteeSeat {
            paused: false,
            target: "carol-desk::responder".into(),
            shown: Some("refund-dev（carol-desk）".into()),
            duty: Some(DEPLOY_DUTY.into()),
            online: true,
            presence: None,
        },
        crate::CommitteeSeat {
            paused: false,
            target: "carol-desk::archivist".into(),
            shown: None,
            duty: None,
            online: false,
            presence: None,
        },
    ]
}

/// What both modes must show the model: each seat as a relay target with
/// its presence and duty in the prompt, and the rule in the persona.
fn assert_seats_and_rule(fixture: &Fixture) {
    let request = &fixture.model.requests()[0];
    let prompt = format!("{:?}", request.messages);
    for needle in [
        "Seats on this committee, written as a relay target",
        "- codex · online · duty: owns the retry path",
        &format!(
            "- carol-desk::responder · shown to people as refund-dev（carol-desk） · \
             online · duty: {DEPLOY_DUTY}"
        ),
        "Whenever you name a seat to people, write the name it is shown to people as, never its \
         relay target or id",
        "- carol-desk::archivist · away · duty: none set",
    ] {
        assert!(prompt.contains(needle), "{needle} in {prompt}");
    }
    let system = request.system.as_deref().unwrap_or_default();
    for needle in [
        "within exactly one seat's duty, relay it to that seat: action relay, kind question",
        "several seats' duties, or none, do not relay by duty",
        "Never route by duty when the speaker named someone",
        "that seat's relay target exactly as the seats on this committee list it",
    ] {
        assert!(system.contains(needle), "{needle} in {system}");
    }
    assert_own_state_rule(system);
}

/// The rule that keeps the Senate's own state with the Speaker, with the
/// question from the live demo as its counter-example.
fn assert_own_state_rule(system: &str) {
    for needle in [
        "Duty routing is only for the work or a subject area.",
        "A question about this Senate's own state is yours to answer",
        "is never relayed by duty",
        "who is here, the seats and their duties, presence, status, the brief, decisions, open \
         items, who owns what",
        "「@Speaker 现在有哪些 agent，他们的职责是什么」",
        "never relayed to a seat whose duty mentions committees",
    ] {
        assert!(system.contains(needle), "{needle} in {system}");
    }
}

/// The question from the live demo: the seats are listed, one seat's duty
/// names "committee", and the Speaker answers itself in both modes.
const WHO_IS_HERE: &str = "@Speaker 现在有哪些 agent，他们的职责是什么";

#[tokio::test]
async fn interpret_answers_who_is_here_itself_with_the_own_state_rule() {
    let fixture = fixture(vec![Round::Answer(json!({
        "action": "none",
        "reply": "codex 和 carol-desk::responder"
    }))])
    .await;
    let intent = fixture
        .brain
        .interpret(Utterance {
            seats: seats(),
            ..utterance(&fixture, WHO_IS_HERE)
        })
        .await
        .expect("intent");
    assert!(!matches!(intent, Intent::Relay { .. }), "{intent:?}");
    let request = &fixture.model.requests()[0];
    assert_own_state_rule(request.system.as_deref().unwrap_or_default());
}

#[tokio::test]
async fn converse_answers_who_is_here_itself_with_the_own_state_rule() {
    let fixture = fixture(vec![Round::Answer(json!({
        "reply": "codex 和 carol-desk::responder",
        "actions": [],
        "awaiting_reply": false
    }))])
    .await;
    let turn = fixture
        .brain
        .converse(Conversation {
            utterance: Utterance {
                seats: seats(),
                ..utterance(&fixture, WHO_IS_HERE)
            },
            turns: Vec::new(),
        })
        .await
        .expect("a turn");
    assert!(turn.actions.is_empty(), "{:?}", turn.actions);
    let request = &fixture.model.requests()[0];
    assert_own_state_rule(request.system.as_deref().unwrap_or_default());
}

#[tokio::test]
async fn interpret_shows_the_seats_and_their_duties_and_relays_by_duty() {
    let fixture = fixture(vec![Round::Answer(json!({
        "action": "relay",
        "targets": ["carol-desk::responder"],
        "kind": "question",
        "body": "退款服务是怎么部署的？"
    }))])
    .await;
    let intent = fixture
        .brain
        .interpret(Utterance {
            seats: seats(),
            ..utterance(&fixture, "退款服务是怎么部署的？")
        })
        .await
        .expect("intent");
    assert_eq!(
        intent,
        Intent::Relay {
            targets: vec!["carol-desk::responder".into()],
            kind: RelayKind::Question,
            body: "退款服务是怎么部署的？".into(),
        }
    );
    assert_seats_and_rule(&fixture);
}

/// Thread 797 item 9: a relay in the context put to a seat that is gone is
/// marked so, and only that one, in both modes; the system prompt says
/// such a relay is no reason to skip a message sent again.
#[tokio::test]
async fn relays_to_a_gone_seat_are_marked_in_the_context() {
    const MARK: &str = "[put to a seat that is gone";
    let fixture = fixture(vec![
        Round::Answer(json!({ "action": "none", "reply": "" })),
        Round::Answer(json!({ "reply": "", "actions": [], "awaiting_reply": false })),
    ])
    .await;
    let gone = fixture.log[0].seq;
    let said = Utterance {
        gone_relays: vec![gone],
        ..utterance(&fixture, "退款服务怎么部署？")
    };
    fixture.brain.interpret(said.clone()).await.expect("intent");
    fixture
        .brain
        .converse(crate::Conversation {
            utterance: said,
            turns: Vec::new(),
        })
        .await
        .expect("turn");
    for request in fixture.model.requests() {
        let prompt = format!("{:?}", request.messages);
        assert_eq!(prompt.matches(MARK).count(), 1, "{prompt}");
        let at = prompt.find(MARK).expect("marked");
        let line = format!("seq {gone} ·");
        let event = prompt.find(&line).expect("the gone relay is listed");
        assert!(event < at, "the mark follows its relay");
        let system = request.system.as_deref().unwrap_or_default();
        assert!(
            system.contains("marked as put to a seat that is gone"),
            "{system}"
        );
    }
}

#[tokio::test]
async fn converse_shows_the_seats_and_their_duties_as_context_and_relays_by_duty() {
    let fixture = fixture(vec![Round::Answer(json!({
        "reply": "",
        "actions": [action(json!({
            "action": "relay",
            "targets": ["carol-desk::responder"],
            "kind": "question",
            "body": "退款服务是怎么部署的？"
        }))],
        "awaiting_reply": false
    }))])
    .await;
    let turn = fixture
        .brain
        .converse(Conversation {
            utterance: Utterance {
                seats: seats(),
                ..utterance(&fixture, "退款服务是怎么部署的？")
            },
            turns: Vec::new(),
        })
        .await
        .expect("a turn");
    assert_eq!(
        turn.actions,
        vec![Intent::Relay {
            targets: vec!["carol-desk::responder".into()],
            kind: RelayKind::Question,
            body: "退款服务是怎么部署的？".into(),
        }]
    );
    assert_seats_and_rule(&fixture);
}

#[tokio::test]
async fn no_seats_known_says_so() {
    let fixture = fixture(vec![Round::Answer(
        json!({ "action": "none", "reply": "" }),
    )])
    .await;
    fixture
        .brain
        .interpret(utterance(&fixture, "在吗"))
        .await
        .expect("intent");
    let prompt = format!("{:?}", fixture.model.requests()[0].messages);
    assert!(
        prompt.contains("with presence and duty:\\n(none known)"),
        "{prompt}"
    );
}

#[tokio::test]
async fn digest_is_one_typed_turn_that_cites_seqs_and_marks_a_persons_points() {
    let digested = fixture(vec![Round::Answer(json!({
        "points": [{ "text": "Release moved to Monday", "refs": [1, 2] }],
        "memory": [{ "text": "Releases go out on Mondays", "refs": [2], "supersedes": [7] }]
    }))])
    .await;
    let mut correction = digested.log[0].clone();
    correction.seq = Seq(8);
    let person_point = senate_core::MemoryPoint {
        seq: Seq(8),
        ts: correction.ts,
        text: "Reviews need two approvals".into(),
        refs: vec![Seq(1)],
        supersedes: vec![],
        by: user("alice"),
    };
    let out = digested
        .brain
        .digest(crate::DigestInput {
            committee: refund(),
            locale: Locale::En,
            trigger: senate_core::DigestTrigger::Daily,
            events: digested.log.clone(),
            memory: vec![person_point],
            decisions: Vec::new(),
        })
        .await
        .expect("a digest");
    assert_eq!(out.points[0].refs, [Seq(1), Seq(2)]);
    assert_eq!(out.memory[0].supersedes, [Seq(7)]);
    let requests = digested.model.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tools.len(), 1, "only the answering tool");
    let prompt = format!("{:?}", requests[0].messages);
    assert!(
        prompt.contains("seq 8 (by a person): Reviews need two approvals"),
        "{prompt}"
    );
    assert!(prompt.contains("Write in English"), "{prompt}");
}

#[tokio::test]
async fn spot_agreement_reads_who_agreed_as_addresses_or_finds_none() {
    let spotted = fixture(vec![
        Round::Answer(json!({ "agreement": {
            "statement": "Retries are capped at three",
            "topic": "retry cap",
            "refs": [1, 2],
            "agreeing": ["[senator::alice]", "senator::bob"]
        }})),
        Round::Answer(json!({ "agreement": null })),
    ])
    .await;
    let input = crate::AgreementInput {
        committee: refund(),
        locale: Locale::En,
        events: spotted.log.clone(),
        proposed: vec!["release day".into()],
        decisions: Vec::new(),
    };
    let found = spotted
        .brain
        .spot_agreement(input.clone())
        .await
        .expect("an answer")
        .expect("an agreement");
    assert_eq!(found.agreeing, [user("alice"), user("bob")]);
    assert_eq!(found.refs, [Seq(1), Seq(2)]);
    assert!(
        spotted
            .brain
            .spot_agreement(input)
            .await
            .expect("an answer")
            .is_none()
    );
    let prompt = format!("{:?}", spotted.model.requests()[0].messages);
    assert!(
        prompt.contains("Topics already proposed as decisions: release day"),
        "{prompt}"
    );
}

#[tokio::test]
async fn an_answer_for_an_agent_looks_at_what_the_committee_knows_first() {
    let fixture = fixture(vec![Round::Text("The release is on Monday (seq 3).")]).await;
    fixture
        .hub
        .append(
            NewEvent::new(
                refund(),
                Kind::Note,
                Address::Speaker,
                "The release is on Monday",
            )
            .with_meta(
                meta::MEMORY,
                senate_core::MemoryPayload::Point {
                    refs: vec![Seq(2)],
                    supersedes: vec![],
                    digest: None,
                }
                .to_value(),
            ),
        )
        .await
        .expect("point");
    let mut event = fixture.log[1].clone();
    event.body = "When is the release?".into();
    fixture
        .brain
        .answer_on_behalf(Question {
            event,
            agent: agent_path("alice"),
            committee: refund(),
            context: fixture.log.clone(),
            takeover: None,
            locale: Locale::En,
        })
        .await
        .expect("an answer");
    let prompt = format!("{:?}", fixture.model.requests()[0].messages);
    assert!(
        prompt.contains("What the committee knows that may bear on it (its memory):")
            && prompt.contains("(from seq 2): The release is on Monday"),
        "{prompt}"
    );
}

// ---- Validated output: the model corrects itself in the same turn -------

/// The error text of every rejected answer the model was shown.
fn rejections(request: &Request<'_>) -> Vec<String> {
    tool_results(request)
        .into_iter()
        .filter_map(|(text, error)| error.then_some(text))
        .collect()
}

fn triage_input() -> TriageInput {
    TriageInput {
        committee: refund(),
        kind: Kind::Question,
        asker: agent("bob"),
        to: vec![agent("alice")],
        body: "下周能上线吗？".into(),
        seq: None,
        context: Vec::new(),
        locale: Locale::Zh,
    }
}

#[tokio::test]
async fn a_pick_of_a_non_candidate_is_corrected_in_the_same_turn() {
    let fixture = fixture(vec![
        Round::Answer(json!({ "agent": "copilot", "reason": "not a candidate" })),
        Round::Answer(json!({ "agent": "codex", "reason": "reviews the gateway" })),
    ])
    .await;
    assert_eq!(
        fixture.brain.pick_agent(pick_input("看一下网关")).await,
        Ok(Pick::Agent(path("codex")))
    );
    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 2, "one turn, one correction");
    let told = rejections(&requests[1]);
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].contains("\"copilot\" is not one of the listed agents")
            && told[0].contains("codex")
            && told[0].contains("again with a corrected answer"),
        "{told:?}"
    );
}

#[tokio::test]
async fn an_intent_with_a_bad_action_is_corrected_in_the_same_turn() {
    let fixture = fixture(vec![
        Round::Answer(json!({ "action": "launch", "body": "x" })),
        Round::Answer(json!({ "action": "relay", "targets": ["alice"], "body": "rebase first" })),
    ])
    .await;
    let intent = fixture
        .brain
        .interpret(utterance(&fixture, "告诉 alice 先 rebase"))
        .await
        .expect("an intent");
    assert_eq!(
        intent,
        Intent::Relay {
            targets: vec!["alice".into()],
            kind: RelayKind::Note,
            body: "rebase first".into(),
        }
    );
    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 2);
    let told = rejections(&requests[1]);
    assert!(
        told.len() == 1 && told[0].contains("action \"launch\" is not one of"),
        "{told:?}"
    );
}

#[tokio::test]
async fn a_triage_out_of_range_is_corrected_in_the_same_turn() {
    let fixture = fixture(vec![
        Round::Answer(json!({ "p_needs_human": 3.0, "p_agent": 0.0, "p_unsure": 0.0 })),
        Round::Answer(json!({ "p_needs_human": 0.8, "p_agent": 0.1, "p_unsure": 0.1 })),
    ])
    .await;
    let triage = fixture
        .brain
        .triage(triage_input())
        .await
        .expect("a verdict");
    assert_eq!(triage.verdict, senate_core::TriageVerdict::NeedsHuman);
    let requests = fixture.model.requests();
    assert_eq!(requests.len(), 2);
    let told = rejections(&requests[1]);
    assert!(
        told.len() == 1 && told[0].contains("must each be a number in 0..1"),
        "{told:?}"
    );
}

#[tokio::test]
async fn an_answer_out_of_schema_is_corrected_too() {
    // A shape error (a string where a number belongs) goes back like a rule.
    let fixture = fixture(vec![
        Round::Answer(json!({ "p_needs_human": "high", "p_agent": 0.0, "p_unsure": 0.0 })),
        Round::Answer(json!({ "p_needs_human": 0.1, "p_agent": 0.8, "p_unsure": 0.1 })),
    ])
    .await;
    let triage = fixture
        .brain
        .triage(triage_input())
        .await
        .expect("a verdict");
    assert_eq!(triage.verdict, senate_core::TriageVerdict::Agent);
    let told = rejections(&fixture.model.requests()[1]);
    assert!(
        told.len() == 1 && told[0].contains("does not fit the schema"),
        "{told:?}"
    );
}

#[tokio::test]
async fn a_stubborn_model_is_bounded_and_the_pick_falls_back() {
    // Never a candidate: each tier rejects MAX_CORRECTIONS + 1 answers and
    // gives up, so the Brain fails and the engine falls back to its rule.
    let per_tier = MAX_CORRECTIONS + 1;
    let fixture = fixture(vec![
        Round::Answer(
            json!({ "agent": "copilot", "reason": "stubborn" })
        );
        10
    ])
    .await;
    let error = fixture
        .brain
        .pick_agent(pick_input("x"))
        .await
        .expect_err("never corrected");
    assert!(matches!(error, BrainError::Invalid(_)), "{error:?}");
    assert!(error.to_string().contains("copilot"), "{error}");
    let requests = fixture.model.requests();
    assert_eq!(
        requests.len(),
        2 * per_tier,
        "the model and its retry, bounded"
    );
    assert_eq!(rejections(&requests[per_tier - 1]).len(), MAX_CORRECTIONS);
}
