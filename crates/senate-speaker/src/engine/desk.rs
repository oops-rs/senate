//! The built-in session's role: the Speaker's desk, and secretary to
//! other senators.
//!
//! # Who it is
//!
//! The built-in session is a senator with a flat name
//! ([`SecretaryConfigs::name`](crate::SecretaryConfigs::name), `secretary`
//! by default). Its role comes from references to that name in the roster:
//!
//! - **The desk.** When the office ([`Roster::office`](senate_core::Roster::office))
//!   names it as the Speaker, or lists it among the Speaker's secretaries,
//!   it works the Speaker's desk. A Senate that names no Speaker is an
//!   unattended root, as one configured before Speakers were named: it is
//!   the Speaker there. Desk workers act in order: it acts on a desk item
//!   only when no worker before it has a live connection on the committee
//!   ([`Hub::is_agent_connected`](senate_core::Hub::is_agent_connected));
//!   otherwise the item is left to that worker (and the Speaker sees it).
//!   It judges task roots (triage) whenever it works the desk, since the
//!   server tells bridges to hold roots for its verdict then.
//! - **A senator's secretary.** When `roster.secretaries_of(agent)` lists
//!   it and that agent has no live session, it answers a *question* put to
//!   the agent from the agent's own latest checkpoint, `from` its own name,
//!   `on_behalf_of` the agent, with [`meta::TAKEOVER`]; a secretary earlier
//!   in the list that is connected goes first. It never performs a
//!   *request*: that waits for the agent. What it cannot answer from the
//!   notes goes up the chain: to the desk when it works the desk, else it
//!   waits.
//!
//! Everything it writes as the office is `from: speaker` with
//! [`meta::ACTED_BY`] = its name ([`Engine::stamped`]).
//!
//! # Crossings
//!
//! An agent's message for a seat outside its committee comes to the desk on
//! its own committee, addressed to `speaker`, with [`meta::INTENDED`]. The
//! desk asks [`Brain::decide_crossing`](crate::Brain::decide_crossing)
//! (within the triage deadline; no answer leaves it) and:
//!
//! - **forwards** it: a new root at the destination (the committee and
//!   address of the intended name, in this Senate), `from: speaker`, with
//!   [`meta::ORIGIN`] = the asker's qualified name and thread, and a note in
//!   the asker's thread with [`meta::FORWARDED_TO`];
//! - **refuses** it: an answer in the asker's thread with the reason;
//! - **leaves** it: nothing is written, and the Speaker sees it.
//!
//! A name in another Senate is left: that crossing is the machine's to
//! carry. A duty the configuration leaves out ([`Duty`]) is left too.
//!
//! # Configuration
//!
//! [`Engine::use_config`] applies the configuration in force for the
//! committee about to be acted on (re-read when its files changed) before
//! every event, timer and Brain result of that committee.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use senate_core::{CheckpointRecord, Locator, Origin as Started};

use super::{
    Address, Answer, BRAIN_SLACK, BrainError, CommitteeId, Drafted, Engine, Event, Flow, HubError,
    Kind, NewEvent, STALL_RETRY, SenatorName, Seq, Stall, TAKEOVER_META, Takeover, ThoughtKind,
    debug, derived_event_id, json, meta, warn,
};
use crate::actor::SpeakerMsg;
use crate::brain::{Crossing, CrossingInput};
use crate::render::{self, NoCover};
use crate::secretary::Duty;

/// Where the built-in session stands on the desk of one committee.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeskTurn {
    /// It is the first present worker: it acts.
    Acting,
    /// A worker before it is present: it leaves the item to that one.
    Waiting(SenatorName),
    /// It does not work this desk.
    Off,
}

/// Who answers a question put to an agent that is not answering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CoverBy {
    /// The built-in session, as the agent's own secretary.
    Secretary,
    /// The Speaker's office (the built-in session at the desk).
    Office,
    /// Somebody else, or nobody yet: it waits.
    Nobody,
}

/// Whether `event` is an answer given in an agent's name while it was away:
/// the Speaker's takeover, or the built-in session's (`me`) as its
/// secretary.
pub(super) fn is_cover(event: &Event, me: Option<&SenatorName>) -> bool {
    if render::is_takeover(event) {
        return true;
    }
    let flagged = event
        .meta
        .get(meta::TAKEOVER)
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    flagged
        && event.on_behalf_of.is_some()
        && matches!(&event.from, Address::Senator(from) if Some(from) == me)
}

/// The qualified name a desk item was meant for.
fn intended_of(event: &Event) -> Option<Locator> {
    event
        .meta
        .get(meta::INTENDED)?
        .as_str()?
        .parse::<Locator>()
        .ok()
}

/// Whether `event` is a message crossing into another committee, put on the
/// desk on the way.
pub(super) fn is_crossing(event: &Event) -> bool {
    event.thread.is_none()
        && event.from != Address::Speaker
        && event.to.contains(&Address::Speaker)
        && matches!(event.kind, Kind::Question | Kind::Request | Kind::Note)
        && intended_of(event).is_some()
}

impl Engine {
    // ----- configuration ----------------------------------------------------

    /// Apply the configuration of the committee `message` is about.
    pub(crate) fn prepare(&mut self, message: &SpeakerMsg) -> Option<CommitteeId> {
        let committee = match message {
            SpeakerMsg::Event(event) => Some(event.committee.clone()),
            SpeakerMsg::Retry(committee)
            | SpeakerMsg::TopDue(committee)
            | SpeakerMsg::PinnedDue(committee)
            | SpeakerMsg::FallbackDue { committee, .. }
            | SpeakerMsg::DigestDue { committee, .. }
            | SpeakerMsg::AgreementDue { committee, .. }
            | SpeakerMsg::CardDue { committee, .. }
            | SpeakerMsg::AnnouncementDue { committee, .. } => Some(committee.clone()),
            SpeakerMsg::Thought(thought) => thought.0.committee().cloned(),
            SpeakerMsg::Run(report) => Some(report.0.committee().clone()),
            SpeakerMsg::Unbound(_) => None,
            SpeakerMsg::Resync | SpeakerMsg::Settle(_) => return None,
        };
        self.use_config(committee.as_ref());
        committee
    }

    /// Tell the Speaker, once per change, that the built-in session's
    /// configuration files have errors: a `note` from the office on
    /// `committee` (the one being handled when the change was noticed),
    /// addressed to the Speaker's seat when it is a senator of this Senate
    /// other than the built-in session itself. A change is a different set
    /// of errors than the last told; none, then the same again, is a change
    /// too. The note's id is derived from the errors, so a restart that
    /// finds the same ones tells nothing twice.
    pub(crate) async fn tell_speaker_of_config_problems(&mut self, committee: &CommitteeId) {
        let problems = self.secretary.problems();
        let key = problems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        if problems.is_empty() {
            self.told_problems = None;
            return;
        }
        if self.told_problems.as_deref() == Some(key.as_str()) {
            return;
        }
        let speaker = self
            .roster
            .office()
            .and_then(|office| office.speaker_here().cloned())
            .filter(|speaker| *speaker != self.me);
        let Some(speaker) = speaker else {
            // Nobody to tell but the built-in session itself.
            self.told_problems = Some(key);
            return;
        };
        let note = NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::Speaker,
            crate::render::config_problems_note(self.secretary.locale(), &problems),
        )
        .with_id(derived_event_id(&[
            "config-problems",
            committee.as_str(),
            &key,
        ]))
        .to(Address::senator(&speaker));
        match self.append(note).await {
            Ok(_) => self.told_problems = Some(key),
            Err(stall) => {
                warn!(%committee, error = %stall, "cannot tell the Speaker of the configuration errors; trying again");
            }
        }
    }

    /// Re-read the configuration of `committee` (`None`: the groups bound
    /// to none) if its files changed, and apply it.
    pub(super) fn use_config(&mut self, committee: Option<&CommitteeId>) {
        let effective = self.secretary.refresh(committee);
        if let Some(committee) = committee
            && let Some(state) = self.committees.get_mut(committee)
        {
            state.locale = effective.speaker.locale_of(committee);
        }
        if self
            .applied
            .as_ref()
            .is_some_and(|applied| Arc::ptr_eq(applied, &effective))
        {
            return;
        }
        let c = &effective.speaker;
        self.fallback_after = c.answer_fallback();
        self.confirmation_ttl = c.confirmation_ttl();
        self.top_debounce = c.brief_top_debounce();
        // Every tier may take its whole deadline: the model, its retry, the
        // fallback ([`ModelSettings::tiers`](crate::ModelSettings::tiers)).
        self.brain_timeout = effective.model.as_ref().map_or_else(
            || c.brain_deadline(),
            |model| {
                model
                    .tiers()
                    .iter()
                    .map(|tier| {
                        tier.model
                            .deadline_secs
                            .map_or_else(|| c.brain_deadline(), Duration::from_secs)
                    })
                    .sum()
            },
        ) + BRAIN_SLACK;
        self.triage_deadline = c.triage_deadline();
        self.card_debounce = c.card_debounce();
        self.takeover_stale = c.takeover_stale();
        self.mode = c.mode;
        self.agent_max_actions = c.agent_max_actions;
        self.work = c.work.clone();
        self.config = c.clone();
        self.applied = Some(effective);
    }

    /// Whether the configuration lets it do `duty` on `committee` without
    /// leaving it for the Speaker.
    pub(super) fn may(&self, committee: &CommitteeId, duty: Duty) -> bool {
        self.secretary.effective(Some(committee)).may(duty)
    }

    /// `event` as the office writes it: `from: speaker` is signed with who
    /// acted, the built-in session.
    pub(super) fn stamped(&self, event: NewEvent) -> NewEvent {
        if event.from == Address::Speaker && !event.meta.contains_key(meta::ACTED_BY) {
            event.with_meta(meta::ACTED_BY, json!(self.me.as_str()))
        } else {
            event
        }
    }

    // ----- the desk -----------------------------------------------------------

    /// Whether the built-in session works the Speaker's desk at all.
    pub(super) fn works_desk(&self) -> bool {
        self.roster
            .office()
            .is_none_or(|office| office.works_desk(&self.me))
    }

    /// Where it stands on `committee`'s desk now.
    pub(super) async fn desk_turn(&self, committee: &CommitteeId) -> DeskTurn {
        let Some(office) = self.roster.office() else {
            return DeskTurn::Acting;
        };
        if !office.works_desk(&self.me) {
            return DeskTurn::Off;
        }
        for worker in &office.desk {
            if *worker == self.me {
                return DeskTurn::Acting;
            }
            if self.hub.is_agent_connected(committee, worker).await {
                return DeskTurn::Waiting(worker.clone());
            }
        }
        DeskTurn::Off
    }

    /// Whether triage runs on `committee`: the desk is its, and the
    /// configuration does not leave triage to the Speaker.
    pub(super) fn judges(&self, committee: &CommitteeId) -> bool {
        self.works_desk() && self.may(committee, Duty::Triage)
    }

    // ----- crossings ------------------------------------------------------------

    /// A message crossing into another committee, on the desk: forward,
    /// refuse, or leave it.
    pub(super) async fn on_crossing(&mut self, event: &Event) -> Result<Flow, Stall> {
        self.mirror(event).await?;
        let committee = event.committee.clone();
        let Some(intended) = intended_of(event) else {
            return Ok(Flow::Done);
        };
        let decided = self
            .committees
            .get(&committee)
            .and_then(|state| state.crossings.get(&event.seq))
            .cloned();
        let decision = match decided {
            Some(decision) => decision,
            None => {
                match self.desk_turn(&committee).await {
                    DeskTurn::Acting => {}
                    DeskTurn::Waiting(worker) => {
                        debug!(%committee, seq = %event.seq, %worker, "a desk worker before the built-in session is present; left to it");
                        return Ok(Flow::Done);
                    }
                    DeskTurn::Off => return Ok(Flow::Done),
                }
                // A replay (forwarded or refused already), or an item from
                // the Senate above that was answered there in this Senate's
                // name while it was offline: nothing is left to do with it.
                let dealt = self
                    .hub
                    .thread(&committee, event.seq)
                    .await?
                    .iter()
                    .any(|e| {
                        (e.from == Address::Speaker && e.thread == Some(event.seq))
                            || senate_core::docket::answered_above(event, e)
                    });
                if dealt {
                    return Ok(Flow::Done);
                }
                let input = CrossingInput {
                    committee: committee.clone(),
                    event: event.clone(),
                    intended,
                    context: self.context(&committee, event.seq).await?,
                    locale: self.locale(&committee),
                };
                let brain = Arc::clone(&self.brain);
                let seq = event.seq;
                let limit = self.triage_deadline;
                self.think_within(
                    limit,
                    async move { brain.decide_crossing(input).await },
                    move |crossing| ThoughtKind::Crossed {
                        committee,
                        seq,
                        crossing,
                    },
                );
                return Ok(Flow::Await);
            }
        };
        // It may have been answered above while the Brain was deciding.
        let answered = self
            .hub
            .thread(&committee, event.seq)
            .await?
            .iter()
            .any(|e| senate_core::docket::answered_above(event, e));
        if answered {
            return Ok(Flow::Done);
        }
        let decision = match decision {
            Crossing::Forward if !self.may(&committee, Duty::Forward) => Crossing::Leave,
            Crossing::Refuse(_) if !self.may(&committee, Duty::Refuse) => Crossing::Leave,
            decision => decision,
        };
        match decision {
            Crossing::Forward => self.forward(event, &intended).await?,
            Crossing::Refuse(reason) => self.refuse_crossing(event, &intended, &reason).await?,
            Crossing::Leave => {
                debug!(%committee, seq = %event.seq, "a crossing left on the desk for the Speaker");
            }
        }
        Ok(Flow::Done)
    }

    pub(super) async fn on_crossed(
        &mut self,
        committee: CommitteeId,
        seq: Seq,
        crossing: Result<Crossing, BrainError>,
    ) {
        let crossing = crossing.unwrap_or_else(|error| {
            warn!(%committee, %seq, %error, "no crossing decision; left on the desk for the Speaker");
            Crossing::Leave
        });
        if let Some(state) = self.committees.get_mut(&committee) {
            state.crossings.insert(seq, crossing);
            if state.awaiting == Some(seq) {
                state.awaiting = None;
            }
        }
        self.catch_up(&committee).await;
    }

    /// Deliver `event` where it was meant to go, and tell the asker.
    async fn forward(&mut self, event: &Event, intended: &Locator) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        let here = self
            .hub
            .senate_path()
            .or_else(|| intended.senate().cloned());
        if let (Some(there), Some(here)) = (intended.senate(), &here)
            && there != here
        {
            debug!(%intended, "a crossing into another Senate; left for the Speaker");
            return Ok(());
        }
        let Some(destination) = intended.committee().cloned() else {
            return Ok(());
        };
        let asker = match &here {
            Some(here) => Locator::at(here.clone(), event.committee.clone(), event.from.clone()),
            None => Locator::on(event.committee.clone(), event.from.clone()),
        };
        let origin = Started {
            locator: asker,
            thread: event.seq,
        };
        let seq = event.seq.to_string();
        let root = NewEvent::new(
            destination,
            event.kind.clone(),
            Address::Speaker,
            event.body.clone(),
        )
        .with_id(derived_event_id(&[
            "forward",
            event.committee.as_str(),
            &seq,
        ]))
        .to(intended.address().clone())
        .with_meta(meta::ORIGIN, origin.value());
        let mut root = self.stamped(root);
        if event.from == Address::Committee && event.source.is_some() {
            root.refs.clone_from(&event.refs);
            root.source.clone_from(&event.source);
            root.on_behalf_of = event.uplink_chat_sender().cloned();
        }
        let forwarded = if intended.address().is_wildcard() {
            self.hub.resolve_and_append(root).await
        } else {
            self.hub.append(root).await
        };
        match forwarded {
            Ok(_) => {}
            Err(error @ HubError::Unavailable(_)) => return Err(Stall::Hub(error)),
            Err(error) => {
                warn!(%intended, %error, "the forward was refused; left on the desk for the Speaker");
                return Ok(());
            }
        }
        let note = NewEvent::new(
            event.committee.clone(),
            Kind::Note,
            Address::Speaker,
            render::crossing_forwarded(l, intended),
        )
        .with_id(derived_event_id(&[
            "forwarded",
            event.committee.as_str(),
            &seq,
        ]))
        .in_thread(event.seq)
        .to(event.from.clone())
        .with_meta(meta::FORWARDED_TO, json!(intended.to_string()));
        self.append(note).await.map(drop)
    }

    /// Tell the asker it does not go there, and why.
    async fn refuse_crossing(
        &mut self,
        event: &Event,
        intended: &Locator,
        reason: &str,
    ) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        let answer = NewEvent::new(
            event.committee.clone(),
            Kind::Answer,
            Address::Speaker,
            render::crossing_refused(l, intended, reason),
        )
        .with_id(derived_event_id(&[
            "refused",
            event.committee.as_str(),
            &event.seq.to_string(),
        ]))
        .in_thread(event.seq)
        .to(event.from.clone());
        self.append(answer).await.map(drop)
    }

    // ----- covering -------------------------------------------------------------

    /// Who answers a question put to `agent` that is due: its own
    /// secretaries first (when it has no live session), then the desk.
    pub(super) async fn cover_by(&self, committee: &CommitteeId, agent: &SenatorName) -> CoverBy {
        if !self.may(committee, Duty::Cover) {
            return CoverBy::Nobody;
        }
        let secretaries = self.roster.secretaries_of(agent);
        if !secretaries.is_empty() && !self.hub.is_agent_connected(committee, agent).await {
            for secretary in secretaries {
                if *secretary == self.me {
                    return CoverBy::Secretary;
                }
                if self.hub.is_agent_connected(committee, secretary).await {
                    return CoverBy::Nobody; // its own secretary is there
                }
            }
        }
        match self.desk_turn(committee).await {
            DeskTurn::Acting => CoverBy::Office,
            DeskTurn::Waiting(_) | DeskTurn::Off => CoverBy::Nobody,
        }
    }

    /// Answer `question` for `agent` as its secretary, from the agent's own
    /// latest checkpoint; up the chain when there is none fresh enough.
    pub(super) async fn cover_as_secretary(
        &mut self,
        committee: CommitteeId,
        root: Seq,
        agent: SenatorName,
        question: Seq,
    ) {
        let checkpoint = match self.hub.latest_checkpoint(&committee, &agent).await {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                warn!(%committee, %error, "cannot read the checkpoint; the secretary's answer is deferred");
                self.arm(&committee, root, agent, STALL_RETRY);
                return;
            }
        };
        let age = |c: &CheckpointRecord| (Utc::now() - c.ts).to_std().unwrap_or(Duration::ZERO);
        let checkpoint = match checkpoint {
            None => Err(NoCover::Missing),
            Some(c) if age(&c) > self.takeover_stale => Err(NoCover::Stale),
            Some(c) => Ok(c),
        };
        let checkpoint = match checkpoint {
            Ok(checkpoint) => checkpoint,
            Err(why) => {
                self.up_the_chain(&committee, root, &agent, question, why)
                    .await;
                return;
            }
        };
        let brief = self.hub.brief(&committee).await.ok();
        let takeover = Takeover {
            age: age(&checkpoint),
            checkpoint,
            brief,
            paused: self
                .committees
                .get(&committee)
                .and_then(|state| state.presence.get(&agent))
                .is_some_and(|p| p.paused),
        };
        self.draft_answer_as(committee, root, agent, question, Some(takeover), true)
            .await;
    }

    /// What the secretary could not answer: the desk takes it when the
    /// built-in session works the desk now (escalating to people, as a
    /// takeover it could not cover); otherwise it waits.
    async fn up_the_chain(
        &mut self,
        committee: &CommitteeId,
        root: Seq,
        agent: &SenatorName,
        question: Seq,
        why: NoCover,
    ) {
        if matches!(self.desk_turn(committee).await, DeskTurn::Acting) {
            self.escalate_takeover(committee, root, agent, question, why)
                .await;
        } else {
            self.forget_open(committee, root, agent);
        }
    }

    /// A secretary's draft is back: post it in the agent's name, signed
    /// with the built-in session's own, or send it up the chain.
    pub(super) async fn on_secretary_answered(
        &mut self,
        draft: Drafted,
        answer: Result<Answer, BrainError>,
    ) {
        let Drafted {
            committee,
            root,
            agent,
            question,
            asker,
        } = draft;
        let Some(open) = self
            .committees
            .get_mut(&committee)
            .and_then(|state| state.open.get_mut(&(root, agent.clone())))
        else {
            return; // the agent answered while the Brain was drafting
        };
        if open.question != question {
            return;
        }
        open.answering = false;
        let text = match answer {
            Ok(answer) if answer.covered && !answer.text.trim().is_empty() => {
                answer.text.trim().to_owned()
            }
            Ok(_) => {
                self.up_the_chain(&committee, root, &agent, question, NoCover::Uncovered)
                    .await;
                return;
            }
            Err(error) => {
                warn!(%committee, %error, "the brain could not answer as the agent's secretary");
                self.up_the_chain(&committee, root, &agent, question, NoCover::Unavailable)
                    .await;
                return;
            }
        };
        match self.hub.mark_taken_over(&committee, &agent).await {
            Ok(_) => {}
            Err(HubError::Rejected(reason)) => {
                debug!(%committee, %agent, %reason, "not covered; the agent is back");
                let window = self.fallback_after;
                self.arm(&committee, root, agent, window);
                return;
            }
            Err(error) => {
                warn!(%committee, %error, "cannot record the cover; deferred");
                self.arm(&committee, root, agent, STALL_RETRY);
                return;
            }
        }
        let answer = NewEvent::new(
            committee.clone(),
            Kind::Answer,
            Address::Senator(self.me.clone()),
            text,
        )
        .with_id(derived_event_id(&[
            "secretary",
            committee.as_str(),
            &question.to_string(),
            &agent.to_string(),
        ]))
        .in_thread(root)
        .on_behalf_of(Address::senator(&agent))
        .to(asker)
        .with_meta(TAKEOVER_META, json!(true));
        match self.append(answer).await {
            Ok(_) => self.forget_open(&committee, root, &agent),
            Err(stall) => {
                warn!(%committee, error = %stall, "the secretary's answer is deferred");
                self.arm(&committee, root, agent, STALL_RETRY);
            }
        }
    }
}
