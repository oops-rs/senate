//! Which agent a person means.
//!
//! A person may have any number of agents, so "alice's agent" is never
//! derived from the handle. What a name said in chat resolves to
//! ([`resolve_name`]):
//!
//! - one agent named outright (`alice/codex`, `[senator::alice/codex]`,
//!   「alice 的 codex」, "alice's codex"): that agent, if it sits here;
//! - a person with one agent here: that agent;
//! - a person with none: the person;
//! - a person with several: the one the Speaker settles on.
//!
//! Settling ([`Engine::settle`]) asks the [`Brain`](crate::Brain) to pick
//! ([`Brain::pick_agent`](crate::Brain::pick_agent)) with what the Speaker
//! knows about each candidate: whether *it* is connected, its own latest
//! checkpoint, the task threads addressed to it, and whether the request was
//! made in one of its threads. A pick is used as is; a state-changing action
//! is restated naming the agent before it runs, so a wrong pick is caught at
//! the yes. When the Brain cannot tell, the Speaker asks in the same thread
//! which one ([`Choice`]); the person who asked (or the person whose agents
//! they are) answers there with the agent's name, its number, or its path,
//! and the request carries on as if that agent had been named. A choice
//! lives in memory for `confirmation_ttl_secs`, like a restatement, and is
//! lost on restart.
//!
//! The pick call is bounded by `triage_deadline_secs` and runs on the
//! mailbox: the rule-based default answers at once, and a model-backed one
//! is cut off at the deadline and falls back to the rules.

use senate_core::{AgentOf, Mention, Name, Roster, SenatorPath, StatusState};

use super::{
    Address, Engine, Event, Flow, GroupRef, Instant, Intent, Origin, Seq, Stall, Value, meta, warn,
};
use crate::brain::{AgentCandidate, Pick, PickInput, rule_pick};
use crate::render;
use crate::syntax;

/// What a name said in chat is, against the roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Resolved {
    /// One agent: named outright, or its person's only one here.
    Agent(SenatorPath),
    /// A person with no agent here: whatever was meant for their agent goes
    /// to them.
    Person(SenatorPath),
    /// A person with several agents here, none of them named.
    Several {
        owner: Name,
        candidates: Vec<SenatorPath>,
    },
    /// Nobody this Senate knows, or an agent that does not sit here.
    Unknown,
}

/// Resolve `raw` (a handle, a name, a mention, or an agent named outright)
/// against `roster`.
pub(super) fn resolve_name(roster: &Roster, raw: &str) -> Resolved {
    let raw = raw.trim().trim_start_matches('@');
    // A full path, at whatever level this Senate seats agents.
    let bare = match raw.parse::<Mention>() {
        Ok(Mention::Senator(path)) => Some(path),
        _ => raw
            .strip_prefix("senator::")
            .unwrap_or(raw)
            .parse::<SenatorPath>()
            .ok(),
    };
    if let Some(path) = bare.filter(SenatorPath::is_junior) {
        return if roster.knows(&path) {
            Resolved::Agent(path)
        } else {
            Resolved::Unknown
        };
    }
    if let Some((person, agent)) = syntax::agent_named(raw) {
        let Some(senior) = roster.resolve(&person) else {
            return Resolved::Unknown;
        };
        return roster
            .agents_of(&senior.handle)
            .into_iter()
            .find(|a| a.name().as_str() == agent)
            .map_or(Resolved::Unknown, |a| Resolved::Agent(a.clone()));
    }
    let person = match raw.parse::<Mention>() {
        Ok(Mention::Senator(path)) | Ok(Mention::SpeakerFor(path)) => path.owner().to_string(),
        Ok(Mention::AgentOf(owner)) => owner.to_string(),
        _ => raw.strip_prefix("senator::").unwrap_or(raw).to_owned(),
    };
    let Some(senior) = roster.resolve(&person) else {
        return Resolved::Unknown;
    };
    match roster.agent_of(&senior.handle) {
        Ok(AgentOf::One(agent)) => Resolved::Agent(agent),
        Ok(AgentOf::None(person)) => Resolved::Person(person),
        Ok(AgentOf::Several(candidates)) => Resolved::Several {
            owner: senior.handle.clone(),
            candidates,
        },
        Err(_) => Resolved::Unknown,
    }
}

/// What [`Engine::settle`] came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Settled {
    /// This senator: an agent, or a person with none.
    To(SenatorPath),
    /// Nobody here by that name.
    Unknown,
    /// The Speaker asked in the thread which agent; the request waits for
    /// the answer.
    Asked,
}

/// What a [`Choice`] carries on with once it is answered.
#[derive(Debug, Clone)]
pub(super) enum Waiting {
    /// An intent with `said` among its names; the answer replaces it with
    /// the agent's path, and the intent runs as if that had been said.
    Intent {
        intent: Intent,
        said: String,
        confirm: bool,
    },
    /// A parked task thread its person hands to one of their agents.
    Handoff { root: Seq },
}

/// The Speaker asked which of a person's agents was meant.
#[derive(Debug, Clone)]
pub(super) struct Choice {
    pub(super) waiting: Waiting,
    pub(super) candidates: Vec<SenatorPath>,
    /// The question the Speaker posted, and the thread it is in.
    pub(super) ask_ref: String,
    pub(super) thread_ref: Option<String>,
    /// The person's message that asked, as a platform message.
    pub(super) origin_ref: String,
    pub(super) origin: Origin,
    pub(super) group: GroupRef,
    /// Who may answer: the one who asked, and the person whose agents
    /// these are.
    pub(super) answerers: Vec<Address>,
    pub(super) expires: Instant,
}

impl Engine {
    /// The candidates as the Speaker sees them now on the chat's committee.
    async fn candidates(
        &self,
        chat: &Event,
        candidates: &[SenatorPath],
    ) -> Result<Vec<AgentCandidate>, Stall> {
        let committee = self.acting_committee(chat);
        let board = self.committees.get(&committee).map(|state| &state.board);
        let mut out = Vec::with_capacity(candidates.len());
        for agent in candidates {
            let open_threads = board
                .map(|board| {
                    board
                        .threads_for(agent, None)
                        .into_iter()
                        .filter(|t| t.state != StatusState::Archived)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let owns_thread = chat.thread.is_some_and(|root| {
                board.and_then(|b| b.get(root)).is_some_and(|t| {
                    t.is_for(agent) || t.owner.as_ref().is_some_and(|o| &o.agent == agent)
                })
            });
            out.push(AgentCandidate {
                agent: agent.clone(),
                present: self.hub.is_agent_connected(&committee, agent).await,
                checkpoint: self.hub.latest_checkpoint(&committee, agent).await?,
                open_threads,
                owns_thread,
            });
        }
        Ok(out)
    }

    /// The Brain's pick among `candidates`, within the triage deadline; the
    /// rules when it fails or answers with someone else.
    async fn pick(&self, input: PickInput) -> Pick {
        let fallback = rule_pick(&input);
        let picked =
            tokio::time::timeout(self.triage_deadline, self.brain.pick_agent(input.clone())).await;
        match picked {
            Ok(Ok(Pick::Agent(agent))) if input.candidates.iter().any(|c| c.agent == agent) => {
                Pick::Agent(agent)
            }
            Ok(Ok(Pick::Ask)) => Pick::Ask,
            Ok(Ok(Pick::Agent(agent))) => {
                warn!(%agent, "the brain picked an agent that is not a candidate; asking");
                Pick::Ask
            }
            Ok(Err(error)) => {
                warn!(%error, "the brain could not pick an agent; using the rules");
                fallback
            }
            Err(_) => {
                warn!("no pick within the deadline; using the rules");
                fallback
            }
        }
    }

    /// Settle `raw` (as said in `chat`) to one senator for `waiting`, which
    /// is what carries on once a person answers if the Speaker has to ask.
    /// `task` is what the agent would be asked to do.
    pub(super) async fn settle(
        &mut self,
        chat: &Event,
        raw: &str,
        task: &str,
        waiting: Waiting,
    ) -> Result<Settled, Stall> {
        match resolve_name(&self.roster, raw) {
            Resolved::Agent(path) | Resolved::Person(path) => Ok(Settled::To(path)),
            Resolved::Unknown => Ok(Settled::Unknown),
            Resolved::Several { owner, candidates } => {
                self.settle_among(chat, owner, candidates, task, waiting)
                    .await
            }
        }
    }

    /// Pick one of `owner`'s `candidates`, or ask which.
    pub(super) async fn settle_among(
        &mut self,
        chat: &Event,
        owner: Name,
        candidates: Vec<SenatorPath>,
        task: &str,
        waiting: Waiting,
    ) -> Result<Settled, Stall> {
        let committee = self.acting_committee(chat);
        let key = (chat.seq, super::agent::event_key(chat), owner.clone());
        if let Some(agent) = self
            .committees
            .get(&chat.committee)
            .and_then(|state| state.picks.get(&key))
        {
            return Ok(Settled::To(agent.clone()));
        }
        let info = self.candidates(chat, &candidates).await?;
        let input = PickInput {
            committee: committee.clone(),
            owner: owner.clone(),
            asker: chat.from.clone(),
            task: task.to_owned(),
            thread: chat.thread,
            candidates: info.clone(),
            locale: self.locale(&committee),
        };
        if let Pick::Agent(agent) = self.pick(input).await {
            if let Some(state) = self.committees.get_mut(&chat.committee) {
                state.picks.insert(key, agent.clone());
            }
            return Ok(Settled::To(agent));
        }
        let l = self.locale(&chat.committee);
        if self.acting.is_some() {
            // A step after a Bind in an agent plan acts on another committee
            // than the thread's: no question can carry it there, so the
            // person names one and asks again.
            let text = render::name_one_of(l, &owner, &candidates);
            self.reply_to_chat(chat, &text).await?;
            return Ok(Settled::Asked);
        }
        let Some(group) = chat.source.as_ref().map(super::group_ref) else {
            return Ok(Settled::Unknown);
        };
        let text = render::which_agent(l, &owner, &info);
        let Some(posted) = self.reply_to_chat(chat, &text).await? else {
            return Ok(Settled::Asked);
        };
        let mut answerers = vec![chat.from.clone()];
        let owner_address = Address::senior(&owner);
        if !answerers.contains(&owner_address) {
            answerers.push(owner_address);
        }
        self.choices.retain(|c| c.ask_ref != posted.message_ref);
        self.choices.push(Choice {
            waiting,
            candidates,
            ask_ref: posted.message_ref,
            thread_ref: posted.thread_ref,
            origin_ref: chat
                .mirror
                .as_ref()
                .map(|m| m.message_ref.clone())
                .unwrap_or_default(),
            origin: Origin::Bound {
                committee: chat.committee.clone(),
                seq: chat.seq,
            },
            group,
            answerers,
            expires: Instant::now() + self.confirmation_ttl,
        });
        Ok(Settled::Asked)
    }

    /// The open choice `chat` answers: in its thread (or replying to the
    /// question), from someone who may answer it.
    pub(super) fn choice_for_chat(&mut self, chat: &Event) -> Option<usize> {
        let now = Instant::now();
        self.choices.retain(|c| c.expires > now);
        let group_id = chat.source.as_ref()?.group_id.as_str();
        let reply_to = chat.meta.get(meta::REPLY_TO).and_then(Value::as_str);
        let thread_root = chat.meta.get(meta::THREAD_ROOT).and_then(Value::as_str);
        let thread_ref = chat.mirror.as_ref().and_then(|m| m.thread_ref.as_deref());
        self.choices.iter().position(|c| {
            c.group.group_id == group_id
                && c.answerers.contains(&chat.from)
                && (reply_to == Some(c.ask_ref.as_str())
                    || (!c.origin_ref.is_empty() && thread_root == Some(c.origin_ref.as_str()))
                    || (thread_ref.is_some() && thread_ref == c.thread_ref.as_deref())
                    || matches!(&c.origin, Origin::Bound { committee, seq }
                        if *committee == chat.committee && chat.thread == Some(*seq)))
        })
    }

    /// `chat` answers choice `index`: carry on with the agent it names, or
    /// say again what the choices are.
    pub(super) async fn answer_choice(
        &mut self,
        index: usize,
        chat: &Event,
    ) -> Result<Flow, Stall> {
        let Some(choice) = self.choices.get(index).cloned() else {
            return Ok(Flow::Done);
        };
        let l = self.locale(&chat.committee);
        let Some(agent) = syntax::choice(&chat.body, &choice.candidates) else {
            let text = render::choice_not_understood(l, &choice.candidates);
            return self.say(chat, &text).await;
        };
        self.choices.retain(|c| c.ask_ref != choice.ask_ref);
        match choice.waiting {
            Waiting::Intent {
                intent,
                said,
                confirm,
            } => {
                let intent = named(intent, &said, &agent);
                self.run_bound(chat, super::Decision::Run { intent, confirm })
                    .await
            }
            Waiting::Handoff { root } => {
                let Some(thread) = self
                    .committees
                    .get(&chat.committee)
                    .and_then(|state| state.board.get(root))
                    .cloned()
                else {
                    return Ok(Flow::Done);
                };
                self.approve(chat, &thread, &agent).await?;
                Ok(Flow::Done)
            }
        }
    }
}

/// `intent` with the name `said` replaced by `agent`'s path wherever it
/// stands for a senator, so running it again settles at once.
fn named(intent: Intent, said: &str, agent: &SenatorPath) -> Intent {
    let path = agent.to_string();
    let swap = |name: String| if name == said { path.clone() } else { name };
    match intent {
        Intent::Relay {
            targets,
            kind,
            body,
        } => Intent::Relay {
            targets: targets.into_iter().map(swap).collect(),
            kind,
            body,
        },
        Intent::Work {
            target,
            instruction,
        } => Intent::Work {
            target: swap(target),
            instruction,
        },
        Intent::Reassign { from, to } => Intent::Reassign {
            from: swap(from),
            to: swap(to),
        },
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use senate_core::Senior;

    fn path(s: &str) -> SenatorPath {
        s.parse().expect("path")
    }

    fn roster() -> Roster {
        let mut alice = Senior::new(Name::new("alice").expect("name"));
        alice.names = vec!["Alice".into(), "爱丽丝".into()];
        Roster::top(
            vec![
                alice,
                Senior::new(Name::new("bob").expect("name")),
                Senior::new(Name::new("carol").expect("name")),
            ],
            vec![
                path("alice/claude"),
                path("alice/codex"),
                path("bob/claude"),
            ],
        )
        .expect("roster")
    }

    #[test]
    fn a_name_resolves_by_how_many_agents_its_person_has() {
        let roster = roster();
        assert_eq!(
            resolve_name(&roster, "bob"),
            Resolved::Agent(path("bob/claude"))
        );
        assert_eq!(
            resolve_name(&roster, "@carol"),
            Resolved::Person(path("carol"))
        );
        assert_eq!(
            resolve_name(&roster, "爱丽丝"),
            Resolved::Several {
                owner: Name::new("alice").expect("name"),
                candidates: vec![path("alice/claude"), path("alice/codex")],
            }
        );
        assert_eq!(resolve_name(&roster, "dave"), Resolved::Unknown);
    }

    #[test]
    fn an_agent_named_outright_is_that_agent_or_nobody() {
        let roster = roster();
        for raw in [
            "alice/codex",
            "[senator::alice/codex]",
            "senator::alice/codex",
            "alice 的 codex",
            "Alice's codex",
        ] {
            assert_eq!(
                resolve_name(&roster, raw),
                Resolved::Agent(path("alice/codex")),
                "{raw}"
            );
        }
        // Never another agent of the same person.
        assert_eq!(resolve_name(&roster, "alice/gemini"), Resolved::Unknown);
        assert_eq!(resolve_name(&roster, "bob 的 codex"), Resolved::Unknown);
        // The person's mention is the person.
        assert_eq!(
            resolve_name(&roster, "[senator::bob]"),
            Resolved::Agent(path("bob/claude"))
        );
    }

    #[test]
    fn a_choice_answered_names_the_agent_in_the_intent() {
        let agent = path("alice/codex");
        let relay = Intent::Relay {
            targets: vec!["alice".into(), "bob".into()],
            kind: crate::brain::RelayKind::Note,
            body: "x".into(),
        };
        assert_eq!(
            named(relay, "alice", &agent),
            Intent::Relay {
                targets: vec!["alice/codex".into(), "bob".into()],
                kind: crate::brain::RelayKind::Note,
                body: "x".into(),
            }
        );
        assert_eq!(
            named(
                Intent::Reassign {
                    from: "alice".into(),
                    to: "alice".into()
                },
                "alice",
                &agent
            ),
            Intent::Reassign {
                from: "alice/codex".into(),
                to: "alice/codex".into()
            }
        );
    }
}
