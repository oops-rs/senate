//! A [`Brain`] that answers from a script, for tests (this crate's and the
//! integrator's end-to-end test). Deterministic and offline.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;

use senate_core::{Brief, Locale, TriagePayload, TriageVerdict};

use crate::brain::{
    AgentTurn, Answer, Brain, BrainError, Conversation, Crossing, CrossingInput, Intent, Offline,
    OfflineInput, Pick, PickInput, Question, TriageInput, Utterance, rule_crossing, rule_offline,
    rule_pick,
};

/// One scripted result, and how long the "model" takes to produce it.
struct Step<T> {
    delay: Duration,
    result: Result<T, BrainError>,
}

#[derive(Default)]
struct Script {
    interpretations: VecDeque<Step<Intent>>,
    answers: VecDeque<Step<Answer>>,
    tops: VecDeque<Step<String>>,
    triages: VecDeque<Step<TriagePayload>>,
    triage_inputs: Vec<TriageInput>,
    turns: VecDeque<Step<AgentTurn>>,
    conversations: Vec<Conversation>,
    utterances: Vec<Utterance>,
    questions: Vec<Question>,
    briefs: Vec<Brief>,
    top_locales: Vec<Locale>,
    picks: VecDeque<Step<Pick>>,
    pick_inputs: Vec<PickInput>,
    crossings: VecDeque<Step<Crossing>>,
    crossing_inputs: Vec<CrossingInput>,
    offlines: VecDeque<Step<Offline>>,
    offline_inputs: Vec<OfflineInput>,
}

/// Plays queued answers in order and records every call. An unscripted call
/// is [`BrainError::Unavailable`], which is exactly the "model is down" path,
/// except [`Brain::triage`]: unscripted, it judges *agent* (p 1.0) at once,
/// so a test that does not care about triage sees deliveries unchanged;
/// [`Brain::pick_agent`], [`Brain::decide_crossing`] and
/// [`Brain::decide_offline`]: unscripted, they are the rule-based defaults.
#[derive(Default)]
pub struct ScriptedBrain {
    script: Mutex<Script>,
}

impl ScriptedBrain {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue the next [`Brain::interpret`] result.
    pub fn interprets(&self, result: Result<Intent, BrainError>) -> &Self {
        self.interprets_after(Duration::ZERO, result)
    }

    /// Queue the next [`Brain::interpret`] result, returned `delay` after
    /// the call (Tokio time, so a paused test clock controls it).
    pub fn interprets_after(&self, delay: Duration, result: Result<Intent, BrainError>) -> &Self {
        self.lock()
            .interpretations
            .push_back(Step { delay, result });
        self
    }

    /// Queue the next [`Brain::answer_on_behalf`] result.
    pub fn answers(&self, result: Result<String, BrainError>) -> &Self {
        self.answers_after(Duration::ZERO, result)
    }

    /// Queue the next [`Brain::answer_on_behalf`] result, returned `delay`
    /// after the call.
    pub fn answers_after(&self, delay: Duration, result: Result<String, BrainError>) -> &Self {
        self.answers_with_after(delay, result.map(Answer::covered))
    }

    /// Queue the next [`Brain::answer_on_behalf`] result as an answer that
    /// says the material does not cover the question.
    pub fn answers_uncovered(&self, text: &str) -> &Self {
        self.answers_with_after(Duration::ZERO, Ok(Answer::not_covered(text)))
    }

    /// Queue the next [`Brain::answer_on_behalf`] result in full, returned
    /// `delay` after the call.
    pub fn answers_with_after(&self, delay: Duration, result: Result<Answer, BrainError>) -> &Self {
        self.lock().answers.push_back(Step { delay, result });
        self
    }

    /// Queue the next [`Brain::brief_top`] result.
    pub fn writes_top(&self, result: Result<String, BrainError>) -> &Self {
        self.writes_top_after(Duration::ZERO, result)
    }

    /// Queue the next [`Brain::brief_top`] result, returned `delay` after
    /// the call.
    pub fn writes_top_after(&self, delay: Duration, result: Result<String, BrainError>) -> &Self {
        self.lock().tops.push_back(Step { delay, result });
        self
    }

    /// Queue the next [`Brain::triage`] result.
    pub fn triages(&self, result: Result<TriagePayload, BrainError>) -> &Self {
        self.triages_after(Duration::ZERO, result)
    }

    /// Queue the next [`Brain::triage`] result, returned `delay` after the
    /// call.
    pub fn triages_after(
        &self,
        delay: Duration,
        result: Result<TriagePayload, BrainError>,
    ) -> &Self {
        self.lock().triages.push_back(Step { delay, result });
        self
    }

    /// Queue a verdict with probability 1.0 on it and `reason`.
    pub fn judges(&self, verdict: TriageVerdict, reason: &str) -> &Self {
        self.triages(Ok(scripted_verdict(verdict, Some(reason))))
    }

    /// Queue the next [`Brain::converse`] result.
    pub fn converses(&self, result: Result<AgentTurn, BrainError>) -> &Self {
        self.converses_after(Duration::ZERO, result)
    }

    /// Queue the next [`Brain::converse`] result, returned `delay` after the
    /// call.
    pub fn converses_after(&self, delay: Duration, result: Result<AgentTurn, BrainError>) -> &Self {
        self.lock().turns.push_back(Step { delay, result });
        self
    }

    /// Every conversation turn asked for so far (recorded when the call
    /// starts).
    pub fn conversations(&self) -> Vec<Conversation> {
        self.lock().conversations.clone()
    }

    /// Every root triaged so far (recorded when the call starts).
    pub fn triage_inputs(&self) -> Vec<TriageInput> {
        self.lock().triage_inputs.clone()
    }

    /// Every utterance interpreted so far (recorded when the call starts).
    pub fn utterances(&self) -> Vec<Utterance> {
        self.lock().utterances.clone()
    }

    /// Every question answered on behalf so far (recorded when the call
    /// starts).
    pub fn questions(&self) -> Vec<Question> {
        self.lock().questions.clone()
    }

    /// Every brief a top was asked for so far (recorded when the call
    /// starts).
    pub fn briefs(&self) -> Vec<Brief> {
        self.lock().briefs.clone()
    }

    /// The locale each top was asked for in.
    pub fn top_locales(&self) -> Vec<Locale> {
        self.lock().top_locales.clone()
    }

    /// Queue the next [`Brain::pick_agent`] result.
    pub fn picks(&self, result: Result<Pick, BrainError>) -> &Self {
        self.lock().picks.push_back(Step {
            delay: Duration::ZERO,
            result,
        });
        self
    }

    /// Every pick asked for so far (recorded when the call starts).
    pub fn pick_inputs(&self) -> Vec<PickInput> {
        self.lock().pick_inputs.clone()
    }

    /// Queue the next [`Brain::decide_crossing`] result.
    pub fn decides_crossing(&self, result: Result<Crossing, BrainError>) -> &Self {
        self.lock().crossings.push_back(Step {
            delay: Duration::ZERO,
            result,
        });
        self
    }

    /// Every crossing decision asked for so far (recorded when the call
    /// starts).
    pub fn crossing_inputs(&self) -> Vec<CrossingInput> {
        self.lock().crossing_inputs.clone()
    }

    /// Queue the next [`Brain::decide_offline`] result.
    pub fn decides_offline(&self, result: Result<Offline, BrainError>) -> &Self {
        self.lock().offlines.push_back(Step {
            delay: Duration::ZERO,
            result,
        });
        self
    }

    /// Every decision for an offline child Senate asked for so far
    /// (recorded when the call starts).
    pub fn offline_inputs(&self) -> Vec<OfflineInput> {
        self.lock().offline_inputs.clone()
    }

    fn lock(&self) -> MutexGuard<'_, Script> {
        self.script
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A classifier result that is certain of `verdict`.
pub fn scripted_verdict(verdict: TriageVerdict, reason: Option<&str>) -> TriagePayload {
    let p = |v: TriageVerdict| if v == verdict { 1.0 } else { 0.0 };
    TriagePayload {
        verdict,
        p_needs_human: p(TriageVerdict::NeedsHuman),
        p_agent: p(TriageVerdict::Agent),
        p_unsure: p(TriageVerdict::Unsure),
        rule: None,
        reason: reason.map(str::to_owned),
    }
}

async fn play<T>(step: Option<Step<T>>, unscripted: &str) -> Result<T, BrainError> {
    let Some(step) = step else {
        return Err(BrainError::Unavailable(unscripted.into()));
    };
    if !step.delay.is_zero() {
        tokio::time::sleep(step.delay).await;
    }
    step.result
}

#[async_trait]
impl Brain for ScriptedBrain {
    async fn interpret(&self, utterance: Utterance) -> Result<Intent, BrainError> {
        let step = {
            let mut script = self.lock();
            script.utterances.push(utterance);
            script.interpretations.pop_front()
        };
        play(step, "no scripted interpretation").await
    }

    async fn answer_on_behalf(&self, question: Question) -> Result<Answer, BrainError> {
        let step = {
            let mut script = self.lock();
            script.questions.push(question);
            script.answers.pop_front()
        };
        play(step, "no scripted answer").await
    }

    async fn brief_top(&self, brief: &Brief, locale: Locale) -> Result<String, BrainError> {
        let step = {
            let mut script = self.lock();
            script.briefs.push(brief.clone());
            script.top_locales.push(locale);
            script.tops.pop_front()
        };
        play(step, "no scripted top").await
    }

    async fn pick_agent(&self, input: PickInput) -> Result<Pick, BrainError> {
        let step = {
            let mut script = self.lock();
            script.pick_inputs.push(input.clone());
            script.picks.pop_front()
        };
        match step {
            Some(step) => play(Some(step), "").await,
            None => Ok(rule_pick(&input)),
        }
    }

    async fn decide_crossing(&self, input: CrossingInput) -> Result<Crossing, BrainError> {
        let step = {
            let mut script = self.lock();
            script.crossing_inputs.push(input.clone());
            script.crossings.pop_front()
        };
        match step {
            Some(step) => play(Some(step), "").await,
            None => Ok(rule_crossing(&input)),
        }
    }

    async fn decide_offline(&self, input: OfflineInput) -> Result<Offline, BrainError> {
        let step = {
            let mut script = self.lock();
            script.offline_inputs.push(input.clone());
            script.offlines.pop_front()
        };
        match step {
            Some(step) => play(Some(step), "").await,
            None => Ok(rule_offline(&input)),
        }
    }

    async fn triage(&self, input: TriageInput) -> Result<TriagePayload, BrainError> {
        let step = {
            let mut script = self.lock();
            script.triage_inputs.push(input);
            script.triages.pop_front()
        };
        match step {
            Some(step) => play(Some(step), "").await,
            None => Ok(scripted_verdict(TriageVerdict::Agent, None)),
        }
    }

    async fn converse(&self, conversation: Conversation) -> Result<AgentTurn, BrainError> {
        let step = {
            let mut script = self.lock();
            script.conversations.push(conversation);
            script.turns.pop_front()
        };
        play(step, "no scripted turn").await
    }
}
