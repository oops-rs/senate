//! Which agent a person means.
//!
//! A person may have any number of agents, so "alice's agent" is never
//! derived from the handle. What a name said in chat resolves to
//! ([`resolve_name`]):
//!
//! - one agent named outright (`codex`, `[senator::codex]`, 「alice 的
//!   codex」, "alice's codex"): that agent, if it sits here (and, said as
//!   a person's, is theirs);
//! - a person with one agent here: that agent;
//! - a person with none: the person;
//! - a person with several: the one the Speaker settles on;
//! - whichever agent (`[senator::*]`): the same three ways, among the
//!   agents on the committee ([`Hub::committee_agents`](senate_core::Hub::committee_agents)),
//!   the Senate's only person when there is none.
//!
//! Settling ([`Engine::settle`]) asks the [`Brain`](crate::Brain) to pick
//! ([`Brain::pick_agent`](crate::Brain::pick_agent)) with what the Speaker
//! knows about each candidate: whether *it* is connected, its own latest
//! checkpoint, the task threads addressed to it, and whether the request was
//! made in one of its threads. A pick is used as is; a state-changing action
//! is restated naming the agent before it runs, so a wrong pick is caught at
//! the yes. When the Brain cannot tell, the Speaker asks in the same thread
//! which one ([`Choice`]); the person who asked (or the person whose agents
//! they are) answers there with the agent's name, its number, or its mention,
//! and the request carries on as if that agent had been named. A choice
//! lives in memory for `confirmation_ttl_secs`, like a restatement, and is
//! lost on restart.
//!
//! The pick call is bounded by `triage_deadline_secs` and runs on the
//! mailbox: the rule-based default answers at once, and a model-backed one
//! is cut off at the deadline and falls back to the rules.
//!
//! # A person's agent, one level up
//!
//! A relay ("ask alice's agent …", 「问 alice 的 agent …」) to a person who
//! runs no agent in this Senate reaches that person's child Senates
//! ([`Engine::settle_reach`]): the Senates registered here whose Speaker
//! she is ([`Hub::children`](senate_core::Hub::children)). One: the relay
//! goes to that Senate's Speaker's desk for the same committee,
//! `<child>::<committee>::speaker`. Several: the same pick as among a
//! person's agents (the Brain, else a question in the thread naming the
//! Senates). None: the person herself, as before. A person who runs agents
//! here keeps the lookup above; `senator::alice` written out is always the
//! person.
//!
//! # A seat in a child Senate
//!
//! A child Senate declares its seats here: who sits on each committee the
//! two share, and whether it is online
//! ([`ChildSenate::seats`](senate_core::ChildSenate)). A relay that goes
//! down to a child is meant for one of them when that can be told
//! ([`Engine::seat_in`]): the only seat it declares on the committee, or,
//! among several, the one the Brain picks, as among a person's agents
//! here, weighing the task against the duty each seat declares. The relay still goes to the child's desk, and names the seat in
//! [`meta::INTENDED`]; the desk there delivers it. When the child declares
//! no seat on the committee, or the pick cannot tell, no seat is named and
//! the desk decides, as before: no question is asked up here about seats
//! only the child knows. A seat that is not online is still named: what
//! is meant for it is the child's desk's to handle.
//!
//! A seat may also be named outright ([`Engine::child_named`],
//! [`Engine::declared_seat`]), when the name is no senator here:
//!
//! | said | means |
//! | --- | --- |
//! | `<child>::<committee>::senator::responder` | that seat, declared or not |
//! | `<child>::responder` | that seat on the committee, when the child declares it |
//! | `responder` | the same, when one child alone declares it on the committee |
//! | `<child>::speaker`, `<child>::<committee>::speaker` | the desk, no seat named |
//! | `<child>::*` | whichever seat there, told as for a person's agent |
//!
//! One message carries one intended name, so a relay to several recipients
//! names no seat.

use senate_core::{
    AgentOf, Locator, Mention, Name, PresenceState, Roster, SenatePath, SenatorName, StatusState,
};

use super::{
    Address, CommitteeId, Engine, Event, Flow, GroupRef, Instant, Intent, Origin, Seq, Stall,
    Value, meta, warn,
};
use crate::brain::{AgentCandidate, CommitteeSeat, Pick, PickInput, rule_pick};
use crate::render;
use crate::syntax;

/// What a name said in chat is, against the roster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Resolved {
    /// One agent: named outright, or its person's only one here.
    Agent(SenatorName),
    /// A person with no agent here: whatever was meant for their agent goes
    /// to them.
    Person(SenatorName),
    /// A person with several agents here, none of them named.
    Several {
        owner: Name,
        candidates: Vec<SenatorName>,
    },
    /// Whichever agent on the committee (`[senator::*]`): the committee's
    /// agents settle it.
    AnyAgent,
    /// Nobody this Senate knows, or an agent that does not sit here.
    Unknown,
}

/// Resolve `raw` (a handle, a name, a mention, or an agent named outright)
/// against `roster`.
pub(super) fn resolve_name(roster: &Roster, raw: &str) -> Resolved {
    let raw = raw.trim().trim_start_matches('@');
    let mention = raw.parse::<Mention>().ok();
    if matches!(mention, Some(Mention::AnyAgent)) || raw == "senator::*" {
        return Resolved::AnyAgent;
    }
    // One senator named outright: when it is not a person, it is an agent,
    // which must sit here.
    let bare = match &mention {
        Some(Mention::Senator(name) | Mention::SpeakerFor(name)) => Some(name.clone()),
        _ => raw
            .strip_prefix("senator::")
            .unwrap_or(raw)
            .parse::<SenatorName>()
            .ok(),
    };
    if let Some(name) = bare.as_ref().filter(|n| !roster.is_person(n)) {
        if roster.is_agent(name) {
            return Resolved::Agent(name.clone());
        }
        if mention.is_some() {
            return Resolved::Unknown;
        }
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
    let person = match bare {
        Some(name) => name.to_string(),
        None => raw.strip_prefix("senator::").unwrap_or(raw).to_owned(),
    };
    let Some(senior) = roster.resolve(&person) else {
        return Resolved::Unknown;
    };
    match super::agent_of(roster, &senior.handle) {
        AgentOf::One(agent) => Resolved::Agent(agent),
        AgentOf::None(Some(person)) => Resolved::Person(person),
        AgentOf::None(None) => Resolved::Unknown,
        AgentOf::Several(candidates) => Resolved::Several {
            owner: senior.handle.clone(),
            candidates,
        },
    }
}

/// What [`Engine::settle`] came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Settled {
    /// This senator: an agent, or a person with none.
    To(SenatorName),
    /// Nobody here by that name.
    Unknown,
    /// The Speaker asked in the thread which agent; the request waits for
    /// the answer.
    Asked,
    /// A child Senate's Speaker's desk, and the seat there when one could
    /// be told: a person's agent one level down ([`Engine::settle_reach`]).
    Child(ChildTarget),
}

/// Where a message goes in a child Senate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ChildTarget {
    /// The child Senate: the message goes to its Speaker's desk.
    pub(super) senate: Name,
    /// The committee there.
    pub(super) committee: CommitteeId,
    /// The seat it is meant for; `None`: the desk there decides.
    pub(super) seat: Option<SenatorName>,
}

/// What a name said outright means in a child Senate.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seat {
    /// The Speaker's desk, no seat named.
    Desk,
    /// This seat.
    Named(SenatorName),
    /// Whichever seat there ([`Engine::seat_in`]).
    Any,
}

/// What a pick, or the question the Speaker asks, is among.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Among {
    /// Agents of this Senate.
    Agents,
    /// A person's child Senates.
    Senates,
}

/// How a child Senate is named in a request carried on after a choice:
/// whichever seat there, as when the person had one child Senate.
fn child_target(child: &str) -> String {
    format!("{child}::*")
}

/// Whether `raw` names a senator written out (`senator::alice`,
/// `[senator::alice]`): the person, never their agent one level down.
fn written_out(raw: &str) -> bool {
    let raw = raw.trim().trim_start_matches('@');
    raw.starts_with("senator::") || raw.starts_with('[')
}

/// What a [`Choice`] carries on with once it is answered.
#[derive(Debug, Clone)]
pub(super) enum Waiting {
    /// An intent with `said` among its names; the answer replaces it with
    /// the agent's name, and the intent runs as if that had been said.
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
    pub(super) candidates: Vec<SenatorName>,
    /// Agents, or child Senates (named like senators).
    pub(super) among: Among,
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
    /// Every seat on `committee` an unaddressed question could be relayed
    /// to by its duty: this Senate's agents there, then the seats child
    /// Senates declare there, each named as a relay names it. What the hub
    /// cannot say is left out, not guessed.
    pub(super) async fn committee_seats(&self, committee: &CommitteeId) -> Vec<CommitteeSeat> {
        let page = match self.hub.presence(committee).await {
            Ok(page) => page,
            Err(error) => {
                tracing::debug!(%committee, %error, "listing seats without their duties");
                senate_core::api::PresencePage {
                    agents: Vec::new(),
                    head: Seq::ZERO,
                    duties: Default::default(),
                }
            }
        };
        let agents = match self.hub.committee_agents(committee).await {
            Ok(agents) => agents,
            Err(error) => {
                tracing::debug!(%committee, %error, "listing seats without this Senate's agents");
                Vec::new()
            }
        };
        let l = self.locale(committee);
        let mut seats = Vec::new();
        for agent in agents.into_iter().filter(|a| *a != self.me) {
            seats.push(CommitteeSeat {
                shown: self
                    .aliases
                    .alias_of(committee, &agent)
                    .map(ToString::to_string),
                duty: page.duties.get(&agent).cloned(),
                online: self.hub.is_agent_connected(committee, &agent).await,
                presence: page
                    .agents
                    .iter()
                    .find(|p| p.agent == agent)
                    .map(|p| p.state),
                paused: page.agents.iter().any(|p| p.agent == agent && p.paused),
                target: agent.to_string(),
            });
        }
        let now = chrono::Utc::now();
        for child in &self.children {
            let child_online = child.is_online(now);
            for seat in child.seats.iter().filter(|s| s.committee == *committee) {
                seats.push(CommitteeSeat {
                    target: format!("{}::{}", child.name, seat.senator),
                    shown: seat
                        .alias
                        .as_ref()
                        .map(|alias| crate::render::child_seat_name(l, alias, &child.name)),
                    duty: seat.duty.clone(),
                    online: child_online && seat.presence == PresenceState::Online,
                    presence: Some(if child_online {
                        seat.presence
                    } else {
                        PresenceState::Disconnected
                    }),
                    paused: child_online && seat.paused,
                });
            }
        }
        seats
    }

    /// The candidates as the Speaker sees them now on the chat's committee.
    async fn candidates(
        &self,
        chat: &Event,
        candidates: &[SenatorName],
    ) -> Result<Vec<AgentCandidate>, Stall> {
        let committee = self.acting_committee(chat);
        let board = self.committees.get(&committee).map(|state| &state.board);
        let duties = match self.hub.presence(&committee).await {
            Ok(page) => page.duties,
            Err(error) => {
                tracing::debug!(%committee, %error, "picking without the seats' duties");
                Default::default()
            }
        };
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
                duty: duties.get(agent).cloned(),
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
                self.settle_among(chat, Some(owner), candidates, task, waiting, Among::Agents)
                    .await
            }
            Resolved::AnyAgent => {
                let committee = self.acting_committee(chat);
                let on_committee = self.hub.committee_agents(&committee).await?;
                match self.roster.any_agent_among(&on_committee) {
                    AgentOf::One(agent) => Ok(Settled::To(agent)),
                    AgentOf::None(Some(person)) => Ok(Settled::To(person)),
                    AgentOf::None(None) => Ok(Settled::Unknown),
                    AgentOf::Several(candidates) => {
                        self.settle_among(chat, None, candidates, task, waiting, Among::Agents)
                            .await
                    }
                }
            }
        }
    }

    /// The child Senates registered here whose Speaker is `person`.
    fn child_senates_of(&self, person: &SenatorName) -> Vec<Name> {
        self.children
            .iter()
            .filter(|c| c.speaker == *person)
            .map(|c| c.name.clone())
            .collect()
    }

    /// The registered child Senate a Senate path names: its name alone, or
    /// its whole path from the root.
    fn child_at(&self, path: &SenatePath) -> Option<&senate_core::ChildSenate> {
        let child = self.children.iter().find(|c| c.name == *path.name())?;
        let whole = path.depth() == 1
            || self
                .hub
                .senate_path()
                .and_then(|here| here.child(child.name.clone()).ok())
                .is_some_and(|there| there.same_names(path));
        whole.then_some(child)
    }

    /// What `raw` names in a registered child Senate, when it names the
    /// Senate (see the module docs): the Senate, the committee there
    /// (`acting` when none is said) and the desk or seat.
    fn child_named(&self, raw: &str, acting: &CommitteeId) -> Option<(Name, CommitteeId, Seat)> {
        let raw = raw.trim().trim_start_matches('@');
        let raw = raw
            .strip_prefix('[')
            .and_then(|r| r.strip_suffix(']'))
            .unwrap_or(raw);
        if let Some((child, seat)) = raw.split_once("::")
            && !seat.contains("::")
            && let Some(child) = self.children.iter().find(|c| c.name.as_str() == child)
        {
            let seat = match seat {
                "speaker" => Seat::Desk,
                "*" => Seat::Any,
                seat => {
                    let seat: SenatorName = seat.parse().ok()?;
                    let declared = child
                        .seats
                        .iter()
                        .any(|s| s.committee == *acting && s.senator == seat);
                    if !declared {
                        return None;
                    }
                    Seat::Named(seat)
                }
            };
            return Some((child.name.clone(), acting.clone(), seat));
        }
        let locator: Locator = raw.parse().ok()?;
        let child = self.child_at(locator.senate()?)?;
        let seat = match locator.address() {
            Address::Speaker => Seat::Desk,
            Address::AnyAgent => Seat::Any,
            Address::Senator(seat) => Seat::Named(seat.clone()),
            _ => return None,
        };
        let committee = locator
            .committee()
            .cloned()
            .unwrap_or_else(|| acting.clone());
        Some((child.name.clone(), committee, seat))
    }

    /// `raw` is one name, no senator here, that one child Senate alone
    /// declares a seat for on `committee`: that seat.
    fn declared_seat(&self, raw: &str, committee: &CommitteeId) -> Option<ChildTarget> {
        let raw = raw.trim().trim_start_matches('@');
        let seat: SenatorName = match raw.parse::<Mention>() {
            Ok(Mention::Senator(seat)) => seat,
            Ok(_) => return None,
            Err(_) => raw.strip_prefix("senator::").unwrap_or(raw).parse().ok()?,
        };
        let mut declaring = self.children.iter().filter(|c| {
            c.seats
                .iter()
                .any(|s| s.committee == *committee && s.senator == seat)
        });
        match (declaring.next(), declaring.next()) {
            (Some(child), None) => Some(ChildTarget {
                senate: child.name.clone(),
                committee: committee.clone(),
                seat: Some(seat),
            }),
            _ => None,
        }
    }

    /// The seat of the child Senate `senate` that a message for its
    /// Speaker's agent is meant for on `committee`: the only one it
    /// declares there, or among several the one the Brain picks
    /// ([`Brain::pick_agent`](crate::Brain::pick_agent), the same pick as
    /// among a person's agents here). `None` when it declares none there
    /// or the pick cannot tell: the message goes to the desk with no seat
    /// named. A seat that is not online is still a seat.
    ///
    /// What was told is kept for the chat step, so a step repeated after a
    /// stall or a triage wait names the same seat.
    async fn seat_in(
        &mut self,
        chat: &Event,
        senate: &Name,
        committee: &CommitteeId,
        task: &str,
    ) -> Option<SenatorName> {
        let child = self.children.iter().find(|c| c.name == *senate)?;
        let candidates = seat_candidates(child, committee);
        match candidates.as_slice() {
            [] => return None,
            [one] => return Some(one.agent.clone()),
            _ => {}
        }
        let key = (chat.seq, super::agent::event_key(chat), senate.clone());
        if let Some(told) = self
            .committees
            .get(&chat.committee)
            .and_then(|state| state.seat_picks.get(&key))
        {
            return told.clone();
        }
        let input = PickInput {
            committee: committee.clone(),
            owner: Some(child.speaker.name().clone()),
            asker: chat.from.clone(),
            task: task.to_owned(),
            thread: chat.thread,
            candidates,
            locale: self.locale(committee),
        };
        let told = match self.pick(input).await {
            Pick::Agent(seat) => Some(seat),
            Pick::Ask => None,
        };
        if let Some(state) = self.committees.get_mut(&chat.committee) {
            state.seat_picks.insert(key, told.clone());
        }
        told
    }

    /// The qualified name of `seat` in the child Senate `target` names.
    pub(super) fn seat_locator(&self, target: &ChildTarget, seat: &SenatorName) -> Option<Locator> {
        let senate = match self.hub.senate_path() {
            Some(here) => here.child(target.senate.clone()),
            None => SenatePath::new(vec![target.senate.clone()]),
        }
        .ok()?;
        Some(Locator::at(
            senate,
            target.committee.clone(),
            Address::Senator(seat.clone()),
        ))
    }

    /// [`Engine::settle`] for a message to be delivered (a relay): a person
    /// with no agent here is reached through their child Senates, one
    /// level down, when they have any, and a seat a child Senate declares
    /// may be named outright (see the module docs).
    pub(super) async fn settle_reach(
        &mut self,
        chat: &Event,
        raw: &str,
        task: &str,
        waiting: Waiting,
    ) -> Result<Settled, Stall> {
        let acting = self.acting_committee(chat);
        if let Some((senate, committee, seat)) = self.child_named(raw, &acting) {
            let seat = match seat {
                Seat::Desk => None,
                Seat::Named(seat) => Some(seat),
                Seat::Any => self.seat_in(chat, &senate, &committee, task).await,
            };
            return Ok(Settled::Child(ChildTarget {
                senate,
                committee,
                seat,
            }));
        }
        let settled = self.settle(chat, raw, task, waiting.clone()).await?;
        let person = match &settled {
            Settled::To(person) => person.clone(),
            // Nobody here: a seat one child Senate declares, named alone.
            Settled::Unknown => {
                return Ok(self
                    .declared_seat(raw, &acting)
                    .map_or(settled, Settled::Child));
            }
            Settled::Asked | Settled::Child(_) => return Ok(settled),
        };
        if !self.roster.is_person(&person) || written_out(raw) {
            return Ok(settled);
        }
        let children = self.child_senates_of(&person);
        let senate = match children.as_slice() {
            [] => return Ok(settled),
            [one] => one.clone(),
            several => {
                let named: Vec<SenatorName> = several
                    .iter()
                    .filter_map(|c| SenatorName::new(c.clone()).ok())
                    .collect();
                let owner = person.name().clone();
                match self
                    .settle_among(chat, Some(owner), named, task, waiting, Among::Senates)
                    .await?
                {
                    Settled::To(child) => child.name().clone(),
                    other => return Ok(other),
                }
            }
        };
        let seat = self.seat_in(chat, &senate, &acting, task).await;
        Ok(Settled::Child(ChildTarget {
            senate,
            committee: acting,
            seat,
        }))
    }

    /// Pick one of `candidates`, or ask which: `owner`'s agents, or (with
    /// no owner) the agents on the committee; or `owner`'s child Senates
    /// ([`Among::Senates`]), named like senators.
    pub(super) async fn settle_among(
        &mut self,
        chat: &Event,
        owner: Option<Name>,
        candidates: Vec<SenatorName>,
        task: &str,
        waiting: Waiting,
        among: Among,
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
            let text = match among {
                Among::Agents => render::name_one_of(l, owner.as_ref(), &candidates),
                Among::Senates => render::name_one_senate_of(l, owner.as_ref(), &candidates),
            };
            self.reply_to_chat(chat, &text).await?;
            return Ok(Settled::Asked);
        }
        let Some(group) = chat.source.as_ref().map(super::group_ref) else {
            return Ok(Settled::Unknown);
        };
        let text = match among {
            Among::Agents => render::which_agent(l, owner.as_ref(), &info),
            Among::Senates => render::which_senate(l, owner.as_ref(), &candidates),
        };
        let Some(posted) = self.reply_to_chat(chat, &text).await? else {
            return Ok(Settled::Asked);
        };
        let mut answerers = vec![chat.from.clone()];
        if let Some(owner) = &owner {
            let owner_address = Address::senior(owner);
            if !answerers.contains(&owner_address) {
                answerers.push(owner_address);
            }
        }
        self.choices.retain(|c| c.ask_ref != posted.message_ref);
        self.choices.push(Choice {
            waiting,
            candidates,
            among,
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
            let text = match choice.among {
                Among::Agents => render::choice_not_understood(l, &choice.candidates),
                Among::Senates => render::senate_choice_not_understood(l, &choice.candidates),
            };
            return self.say(chat, &text).await;
        };
        let flow = match choice.waiting {
            Waiting::Intent {
                intent,
                said,
                confirm,
            } => {
                let target = match choice.among {
                    Among::Agents => agent.to_string(),
                    Among::Senates => child_target(agent.as_str()),
                };
                let intent = named(intent, &said, &target);
                self.run_bound(chat, super::Decision::Run { intent, confirm })
                    .await
            }
            Waiting::Handoff { root } => {
                match self
                    .committees
                    .get(&chat.committee)
                    .and_then(|state| state.board.get(root))
                    .cloned()
                {
                    Some(thread) => self
                        .approve(chat, &thread, &agent)
                        .await
                        .map(|_| Flow::Done),
                    None => Ok(Flow::Done),
                }
            }
        };
        // The answer is processed again after a Brain call it waits on (a
        // relayed question's triage) or a stall: the choice stays until then.
        if matches!(flow, Ok(Flow::Done)) {
            self.choices.retain(|c| c.ask_ref != choice.ask_ref);
        }
        flow
    }
}

/// The seats `child` declares on `committee`, as candidates of a pick: what
/// a child Senate says of a seat is who it is, its duty there and whether
/// it is online.
fn seat_candidates(
    child: &senate_core::ChildSenate,
    committee: &CommitteeId,
) -> Vec<AgentCandidate> {
    child
        .seats
        .iter()
        .filter(|seat| seat.committee == *committee)
        .map(|seat| AgentCandidate {
            agent: seat.senator.clone(),
            present: seat.presence == PresenceState::Online,
            checkpoint: None,
            open_threads: Vec::new(),
            owns_thread: false,
            duty: seat.duty.clone(),
        })
        .collect()
}

/// `intent` with the name `said` replaced by `target` (an agent's name, or
/// a child Senate) wherever it stands for a senator, so running it again
/// settles at once.
fn named(intent: Intent, said: &str, target: &str) -> Intent {
    let swap = |name: String| {
        if name == said {
            target.to_owned()
        } else {
            name
        }
    };
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
    use senate_core::{Principal, SeatedAgent};

    fn path(s: &str) -> SenatorName {
        s.parse().expect("path")
    }

    fn roster() -> Roster {
        let mut alice = Principal::new(Name::new("alice").expect("name"));
        alice.names = vec!["Alice".into(), "爱丽丝".into()];
        let seated = |agent: &str, owner: &str| {
            SeatedAgent::new(path(agent), Name::new(owner).expect("name"))
        };
        Roster::new(
            vec![
                alice,
                Principal::new(Name::new("bob").expect("name")),
                Principal::new(Name::new("carol").expect("name")),
            ],
            vec![
                seated("claude", "alice"),
                seated("codex", "alice"),
                seated("copilot", "bob"),
            ],
        )
        .expect("roster")
    }

    #[test]
    fn a_name_resolves_by_how_many_agents_its_person_has() {
        let roster = roster();
        assert_eq!(
            resolve_name(&roster, "bob"),
            Resolved::Agent(path("copilot"))
        );
        assert_eq!(
            resolve_name(&roster, "@carol"),
            Resolved::Person(path("carol"))
        );
        assert_eq!(
            resolve_name(&roster, "爱丽丝"),
            Resolved::Several {
                owner: Name::new("alice").expect("name"),
                candidates: vec![path("claude"), path("codex")],
            }
        );
        assert_eq!(resolve_name(&roster, "dave"), Resolved::Unknown);
    }

    #[test]
    fn an_agent_named_outright_is_that_agent_or_nobody() {
        let roster = roster();
        for raw in [
            "codex",
            "[senator::codex]",
            "senator::codex",
            "alice 的 codex",
            "Alice's codex",
        ] {
            assert_eq!(
                resolve_name(&roster, raw),
                Resolved::Agent(path("codex")),
                "{raw}"
            );
        }
        // An agent that does not sit here is nobody, and a person's agent
        // is never another person's.
        assert_eq!(resolve_name(&roster, "gemini"), Resolved::Unknown);
        assert_eq!(
            resolve_name(&roster, "[senator::gemini]"),
            Resolved::Unknown
        );
        assert_eq!(resolve_name(&roster, "bob 的 codex"), Resolved::Unknown);
        // The person's mention is the person.
        assert_eq!(
            resolve_name(&roster, "[senator::bob]"),
            Resolved::Agent(path("copilot"))
        );
    }

    #[test]
    fn an_owner_prefixed_name_is_nobody() {
        let roster = roster();
        for raw in [
            "alice/codex",
            "senator::alice/codex",
            "[senator::alice/codex]",
        ] {
            assert_eq!(resolve_name(&roster, raw), Resolved::Unknown, "{raw}");
        }
    }

    #[test]
    fn whichever_agent_is_settled_among_the_committee() {
        let roster = roster();
        for raw in ["[senator::*]", "senator::*"] {
            assert_eq!(resolve_name(&roster, raw), Resolved::AnyAgent, "{raw}");
        }
    }

    #[test]
    fn a_persons_agent_is_looked_up_by_who_runs_it() {
        let roster = roster();
        let name = |s: &str| Name::new(s).expect("name");
        assert_eq!(
            super::super::agent_of(&roster, &name("bob")),
            AgentOf::One(path("copilot"))
        );
        assert_eq!(
            super::super::agent_of(&roster, &name("alice")),
            AgentOf::Several(vec![path("claude"), path("codex")])
        );
        assert_eq!(
            super::super::agent_of(&roster, &name("carol")),
            AgentOf::None(Some(path("carol")))
        );
    }

    #[test]
    fn a_choice_answered_names_the_agent_in_the_intent() {
        let agent = path("codex").to_string();
        let relay = Intent::Relay {
            targets: vec!["alice".into(), "bob".into()],
            kind: crate::brain::RelayKind::Note,
            body: "x".into(),
        };
        assert_eq!(
            named(relay, "alice", &agent),
            Intent::Relay {
                targets: vec!["codex".into(), "bob".into()],
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
                from: "codex".into(),
                to: "codex".into()
            }
        );
    }
}
