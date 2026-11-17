//! Task threads: triage, parking a thread on its person, approval, a person
//! stepping in, and `escalate` (see the `engine` module docs).
//!
//! A thread belongs to the agents its root names, never to "the agent of" a
//! person. Parking it waits on the person behind the agent it was put to;
//! that person's hand-off sends it back to that same agent. Only when the
//! root names none of their agents (an escalation from someone else's
//! agent) does the hand-off become a roster lookup: one agent is it, none
//! means it stays with the person, several is the one named in the hand-off
//! (「交给我的 codex」), else the one the Speaker settles on, else a
//! question in the thread.

use senate_core::{Locale, Name, SenatorName, StatusState, ThreadSummary, needs_triage};

use super::route::{Settled, Waiting};
use super::{
    Address, CommitteeId, Engine, Event, Flow, Kind, NewEvent, OutputLevel, PostTarget, Seq, Stall,
    StatusPayload, ThoughtKind, TriageInput, TriagePayload, TriageVerdict, apply_threshold,
    derived_event_id, json, meta, relayed_from, warn,
};
use crate::brain::BrainError;
use crate::render;
use crate::syntax::handoff;

/// The hard rule every `request` meets: a request always goes to the person
/// first.
pub(super) const RULE_REQUEST: &str = "request";
/// A person asking one of their own agents needs nobody's approval.
pub(super) const RULE_OWN_AGENT: &str = "own_agent";
/// The rule on the verdict a person's approval carries.
pub(super) const RULE_APPROVED: &str = "approved";

fn ruled(verdict: TriageVerdict, rule: &str, reason: String) -> TriagePayload {
    TriagePayload {
        verdict,
        p_needs_human: 0.0,
        p_agent: 0.0,
        p_unsure: 0.0,
        rule: Some(rule.to_owned()),
        reason: Some(reason),
    }
}

/// What the Speaker does when no model judged in time.
fn unsure(l: Locale) -> TriagePayload {
    TriagePayload {
        verdict: TriageVerdict::Unsure,
        p_needs_human: 0.0,
        p_agent: 0.0,
        p_unsure: 0.0,
        rule: None,
        reason: Some(render::triage_unavailable(l)),
    }
}

impl Engine {
    // ----- triage ----------------------------------------------------------

    /// A root `question` / `request` to an agent: judge it (unless the
    /// Speaker wrote it with its verdict inline), log the verdict as a
    /// `note` in its thread, post its card, and either park it on its person
    /// or watch it so someone answers if the agent does not.
    pub(super) async fn on_task_root(&mut self, event: &Event) -> Result<Flow, Stall> {
        let l = self.locale(&event.committee);
        let judged = needs_triage(event, &self.roster.people()) && self.judges(&event.committee);
        let triage = if judged {
            match self.root_verdict(event).await? {
                Some(triage) => Some(triage),
                None => return Ok(Flow::Await),
            }
        } else {
            TriagePayload::of(event)
        };
        if let (Some(triage), Some(state)) = (&triage, self.committees.get_mut(&event.committee)) {
            // For the card, which is posted before the note is folded in.
            state.triage.insert(event.seq, triage.clone());
        }
        if judged && let Some(triage) = &triage {
            let note = NewEvent::new(
                event.committee.clone(),
                Kind::Note,
                Address::Speaker,
                render::triage_note(l, triage),
            )
            .with_id(derived_event_id(&[
                "triage",
                event.committee.as_str(),
                &event.seq.to_string(),
            ]))
            .in_thread(event.seq)
            .with_meta(meta::TRIAGE, triage.value());
            self.append(note).await?;
        }
        self.mirror(event).await?;
        match triage {
            Some(triage) if triage.verdict == TriageVerdict::NeedsHuman => {
                if let Some(state) = self.committees.get_mut(&event.committee) {
                    state.close_open(event.seq);
                }
                self.park_on_owner(event, &triage).await?;
            }
            _ => self.fallback_on_arrival(event).await,
        }
        Ok(Flow::Done)
    }

    /// The verdict on a root: one already logged (a replay), a hard rule, or
    /// one back from the Brain. `None` when a Brain call was just started.
    async fn root_verdict(&mut self, event: &Event) -> Result<Option<TriagePayload>, Stall> {
        let l = self.locale(&event.committee);
        if let Some(cached) = self
            .committees
            .get(&event.committee)
            .and_then(|state| state.triage.get(&event.seq))
        {
            return Ok(Some(cached.clone()));
        }
        let logged = self
            .hub
            .thread(&event.committee, event.seq)
            .await?
            .iter()
            .filter(|e| e.from == Address::Speaker && e.thread == Some(event.seq))
            .find_map(TriagePayload::of);
        if logged.is_some() {
            return Ok(logged);
        }
        if let Some(asker) = event.from.as_senator().filter(|p| self.roster.is_person(p))
            && !event.to.is_empty()
            && event.to.iter().all(|to| {
                to.as_senator()
                    .is_some_and(|a| self.roster.owner_of(a) == Some(asker.name()))
            })
        {
            // A person's own task for their own agents: they are the person.
            return Ok(Some(ruled(
                TriageVerdict::Agent,
                RULE_OWN_AGENT,
                render::own_agent_reason(l),
            )));
        }
        if event.kind == Kind::Request {
            return Ok(Some(ruled(
                TriageVerdict::NeedsHuman,
                RULE_REQUEST,
                render::request_rule_reason(l),
            )));
        }
        let input = TriageInput {
            committee: event.committee.clone(),
            kind: event.kind.clone(),
            asker: event
                .on_behalf_of
                .clone()
                .unwrap_or_else(|| event.from.clone()),
            to: event.to.clone(),
            body: event.body.clone(),
            seq: Some(event.seq),
            context: self.context(&event.committee, event.seq).await?,
            locale: l,
        };
        self.start_triage(&event.committee, event.seq, input);
        Ok(None)
    }

    /// Ask the Brain to judge, within `triage_deadline_secs`; the verdict
    /// comes back for the event at `seq` (a root, or a relaying `chat`).
    pub(super) fn start_triage(&mut self, committee: &CommitteeId, seq: Seq, input: TriageInput) {
        let brain = std::sync::Arc::clone(&self.brain);
        let committee = committee.clone();
        let limit = self.triage_deadline;
        self.think_within(
            limit,
            async move { brain.triage(input).await },
            move |triage| ThoughtKind::Triaged {
                committee,
                seq,
                triage,
            },
        );
    }

    pub(super) async fn on_triaged(
        &mut self,
        committee: CommitteeId,
        seq: Seq,
        triage: Result<TriagePayload, BrainError>,
    ) {
        let payload = match triage {
            Ok(payload) => apply_threshold(payload),
            Err(error) => {
                warn!(%committee, %seq, %error, "no triage verdict; delivering as unsure");
                unsure(self.locale(&committee))
            }
        };
        if let Some(state) = self.committees.get_mut(&committee) {
            state.triage.insert(seq, payload);
            if state.awaiting == Some(seq) {
                state.awaiting = None;
            }
        }
        self.catch_up(&committee).await;
    }

    /// A root that needs a person: the thread waits on the person behind
    /// the (first) agent it was put to (`suspended`, `escalated_to` them, so
    /// it is in their inbox). The mention is posted when that status is
    /// processed.
    async fn park_on_owner(&mut self, root: &Event, triage: &TriagePayload) -> Result<(), Stall> {
        let l = self.locale(&root.committee);
        let people = self.roster.people();
        let Some(owner) = root
            .to
            .iter()
            .filter_map(|a| people.as_agent(a))
            .find_map(|agent| self.roster.owner_of(agent).cloned())
        else {
            return Ok(());
        };
        let reason = render::triage_reason(l, triage);
        let mut status = StatusPayload::new(StatusState::Suspended);
        status.escalated_to = Some(owner);
        status.reason = Some(reason.clone());
        let event = NewEvent::new(
            root.committee.clone(),
            Kind::Status,
            Address::Speaker,
            reason,
        )
        .with_id(derived_event_id(&[
            "needs-human",
            root.committee.as_str(),
            &root.seq.to_string(),
        ]))
        .in_thread(root.seq)
        .with_meta(meta::STATUS, status.value());
        self.append(event).await.map(drop)
    }

    // ----- status ----------------------------------------------------------

    /// A `status` in a task thread. One that hands the thread to a person
    /// (`escalated_to`, no takeover) mentions them in the thread: the
    /// Speaker's own is "needs a person", an agent's is `escalate`, which
    /// also tells the asker. At output level "everything" any other status
    /// is posted in the Agent group's thread. The card is updated by
    /// [`after`](Engine::after).
    pub(super) async fn on_status(&mut self, event: &Event) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        let (Some(root), Some(status)) = (event.thread, StatusPayload::of(event)) else {
            return Ok(());
        };
        if event.from == Address::Speaker
            && let Some(run) = senate_core::WorkRun::of(event)
        {
            // A work run's progress or end.
            return self.on_work_status(event, root, &run).await;
        }
        if event.from == Address::Speaker && render::is_takeover(event) {
            // An away agent's question the checkpoint could not answer; its
            // thread need not be a task's.
            return self.post_takeover_escalation(event, root, &status).await;
        }
        let Some(state) = self.committees.get(&event.committee) else {
            return Ok(());
        };
        if state.board.get(root).is_none() {
            return Ok(());
        }
        let level = state.output_level();
        let agents = state.agents_group().map(|g| g.group.group_id.clone());
        if let (Some(person), None) = (&status.escalated_to, &status.takeover) {
            if event.from == Address::Speaker {
                let text = render::needs_human(l, status.reason.as_deref().unwrap_or_default());
                self.post_mention(event, root, person, &text).await?;
            } else if self.roster.is_agent_address(&event.from) {
                let text = render::escalated(l, &event.from, status.reason.as_deref());
                self.post_mention(event, root, person, &text).await?;
                self.ack_escalation(event, root, person).await?;
            }
            return Ok(());
        }
        if level != OutputLevel::All {
            return Ok(());
        }
        let Some(agents) = agents else {
            return Ok(());
        };
        let Some(copy) = self
            .plain_copies(&event.committee, root)
            .await?
            .into_iter()
            .find(|c| c.target == agents)
        else {
            return Ok(());
        };
        let plan = super::MirrorPlan {
            to: PostTarget::Reply {
                message_ref: copy.message_ref,
                in_thread: true,
            },
            text: render::status_reply(l, event, &status),
            target: agents,
            record_as: None,
            card: None,
        };
        self.run_plan(&event.committee, event.seq, plan).await
    }

    /// Post `text` in thread `root` (its Agent group copy when it has one)
    /// with a native mention of `person`, or `@<handle>` written out where
    /// the surface has no mentions. Keyed to `event`, the status.
    async fn post_mention(
        &self,
        event: &Event,
        root: Seq,
        person: &Name,
        text: &str,
    ) -> Result<(), Stall> {
        let platform = self.surface.platform().to_owned();
        let Some((message_ref, group)) = self
            .thread_target(&event.committee, root, &platform)
            .await?
        else {
            return Ok(());
        };
        let to = PostTarget::Reply {
            message_ref,
            in_thread: true,
        };
        self.post_mention_as(event, to, group, person, text).await
    }

    /// Post `text` to `to` with a native mention of `person` (`@<handle>`
    /// written out where the surface has none, or where the person has no
    /// account on this platform), once, recorded as `event`'s copy `target`.
    pub(super) async fn post_mention_as(
        &self,
        event: &Event,
        to: PostTarget,
        target: String,
        person: &Name,
        text: &str,
    ) -> Result<(), Stall> {
        if self.has_copy(&event.committee, event.seq, &target).await? {
            return Ok(());
        }
        let operation = self.op_id(&event.committee, event.seq, &target)?;
        let name = person.to_string();
        let platform_id = self
            .roster
            .principal(person)
            .and_then(|s| s.account_ids(self.surface.platform()).next())
            .map(str::to_owned);
        let posted = match platform_id {
            Some(id) => match self
                .call(
                    self.surface
                        .post_mentioning(to.clone(), text, &id, &operation),
                )
                .await?
            {
                super::Outcome::Done(posted) => Some(posted),
                super::Outcome::Unsupported => {
                    self.post(to, &render::with_at(&name, text), &operation)
                        .await?
                }
                super::Outcome::Refused => None,
            },
            None => {
                self.post(to, &render::with_at(&name, text), &operation)
                    .await?
            }
        };
        if let Some(posted) = posted {
            self.record(event, target, posted).await?;
        }
        Ok(())
    }

    /// "Now with [senator::alice] in person" to whoever asked: in their own
    /// platform thread when a person asked through the Speaker, else as a
    /// `note` to the asking agent in the thread.
    async fn ack_escalation(
        &mut self,
        event: &Event,
        root: Seq,
        person: &Name,
    ) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        let Some(root_event) = self.hub.get(&event.committee, root).await? else {
            return Ok(());
        };
        let text = render::escalated_ack(l, person);
        if relayed_from(&root_event).is_some() {
            let agents = self
                .committees
                .get(&event.committee)
                .and_then(super::CommitteeState::agents_group)
                .map(|g| g.group.group_id.clone());
            let Some(copy) = self
                .plain_copies(&event.committee, root)
                .await?
                .into_iter()
                .find(|c| Some(&c.target) != agents.as_ref())
            else {
                return Ok(());
            };
            let plan = super::MirrorPlan {
                to: PostTarget::Reply {
                    message_ref: copy.message_ref,
                    in_thread: true,
                },
                text,
                target: format!("{}#escalated", copy.target),
                record_as: None,
                card: None,
            };
            return self.run_plan(&event.committee, event.seq, plan).await;
        }
        let asker = root_event
            .on_behalf_of
            .clone()
            .unwrap_or_else(|| root_event.from.clone());
        if asker.as_senator().is_none() || asker == event.from {
            return Ok(());
        }
        let note = NewEvent::new(event.committee.clone(), Kind::Note, Address::Speaker, text)
            .with_id(derived_event_id(&[
                "escalated-ack",
                event.committee.as_str(),
                &event.seq.to_string(),
            ]))
            .in_thread(root)
            .to(asker);
        self.append(note).await.map(drop)
    }

    // ----- people in a task thread -------------------------------------------

    /// A person's `chat` in a task thread. 「交给我的 agent」 from the person
    /// a thread waits on hands it to their own agent; from another person
    /// of a thread that waits on someone, it is told whom (and nothing
    /// changes). Any other reply by a
    /// person other than the asker, before an answer and before anyone took
    /// it, takes the thread over (人工处理中) so a queued delivery is
    /// cancelled. True when the chat was one of those.
    pub(super) async fn on_task_chat(&mut self, chat: &Event) -> Result<bool, Stall> {
        let l = self.locale(&chat.committee);
        let Some(root) = chat.thread else {
            return Ok(false);
        };
        let Some(thread) = self
            .committees
            .get(&chat.committee)
            .and_then(|state| state.board.get(root))
            .cloned()
        else {
            return Ok(false);
        };
        if !self.is_member(&chat.from) || thread.kind == Kind::Note {
            // A work run is the Speaker's own: nobody takes it over.
            return Ok(false);
        }
        if let Some(named) = handoff(&chat.body) {
            let Some(person) = self.person_of(&chat.from) else {
                return Ok(false);
            };
            match &thread.escalated_to {
                Some(waits_on) if *waits_on == person => {}
                Some(waits_on) => {
                    // Someone else's thread: say whose, rather than nothing.
                    self.reply_to_chat(chat, &render::handoff_not_yours(l, waits_on))
                        .await?;
                    return Ok(true);
                }
                None => return Ok(false),
            }
            if thread.state == StatusState::Suspended
                && thread.takeover.is_none()
                && !thread.answered
            {
                self.hand_off(chat, &thread, &person, named.as_deref())
                    .await?;
            } else {
                self.reply_to_chat(chat, &render::already_handed_off(l))
                    .await?;
            }
            return Ok(true);
        }
        if chat.is_directed_to(&Address::Speaker)
            || thread.answered
            || thread.state == StatusState::Archived
            || thread.takeover.is_some()
        {
            return Ok(false);
        }
        let asker = thread.on_behalf_of.as_ref().unwrap_or(&thread.from);
        if *asker == chat.from {
            return Ok(false); // the asker adding to their own question
        }
        let Some(person) = self.person_of(&chat.from) else {
            return Ok(false);
        };
        let mut status = StatusPayload::new(StatusState::Suspended);
        status.takeover = Some(person);
        status.reason = Some(render::takeover_reason(l));
        let event = NewEvent::new(
            chat.committee.clone(),
            Kind::Status,
            Address::Speaker,
            render::takeover_reason(l),
        )
        // One takeover per thread, whoever gets there first.
        .with_id(derived_event_id(&[
            "takeover",
            chat.committee.as_str(),
            &root.to_string(),
        ]))
        .in_thread(root)
        .with_meta(meta::STATUS, status.value());
        self.append(event).await?;
        Ok(true)
    }

    /// `person` hands the thread parked on them to one of their agents:
    /// the agent the root was put to when it is theirs (a thread belongs to
    /// the agent its root names), or the one named in the hand-off; when
    /// the root names none of theirs, a roster lookup settles it, asking in
    /// the thread when the Speaker cannot tell.
    async fn hand_off(
        &mut self,
        chat: &Event,
        thread: &ThreadSummary,
        person: &Name,
        named: Option<&str>,
    ) -> Result<(), Stall> {
        let l = self.locale(&chat.committee);
        let theirs: Vec<SenatorName> = self.roster.agents_of(person).into_iter().cloned().collect();
        if let Some(name) = named {
            let Some(agent) = theirs.iter().find(|a| a.name().as_str() == name) else {
                let text = render::no_agent_named(l, person, name);
                return self.reply_to_chat(chat, &text).await.map(drop);
            };
            return self.approve(chat, thread, agent).await;
        }
        let in_root: Vec<SenatorName> = thread
            .agents()
            .filter(|a| theirs.contains(a))
            .cloned()
            .collect();
        let candidates = if in_root.is_empty() { theirs } else { in_root };
        match candidates.as_slice() {
            [] => {
                let text = render::no_agent_of_yours(l);
                self.reply_to_chat(chat, &text).await.map(drop)
            }
            [agent] => {
                let agent = agent.clone();
                self.approve(chat, thread, &agent).await
            }
            _ => {
                let waiting = Waiting::Handoff { root: thread.root };
                match self
                    .settle_among(
                        chat,
                        Some(person.clone()),
                        candidates,
                        &thread.title,
                        waiting,
                        super::route::Among::Agents,
                    )
                    .await?
                {
                    Settled::To(agent) => self.approve(chat, thread, &agent).await,
                    Settled::Asked | Settled::Unknown | Settled::Child(_) => Ok(()),
                }
            }
        }
    }

    /// The person approves: the root becomes their own task, re-addressed to
    /// `agent` (one of theirs) in the same thread with an inline *agent*
    /// verdict (so it is pushed at once), and the thread is processing
    /// again.
    pub(super) async fn approve(
        &mut self,
        chat: &Event,
        thread: &ThreadSummary,
        agent: &SenatorName,
    ) -> Result<(), Stall> {
        let l = self.locale(&chat.committee);
        let Some(root) = self.hub.get(&chat.committee, thread.root).await? else {
            return Ok(());
        };
        let agent = Address::senator(agent);
        let root_seq = thread.root.to_string();
        let verdict = ruled(
            TriageVerdict::Agent,
            RULE_APPROVED,
            render::approved_reason(l, &agent),
        );
        let mut task = NewEvent::new(
            chat.committee.clone(),
            root.kind.clone(),
            chat.from.clone(),
            root.body.clone(),
        )
        .with_id(derived_event_id(&[
            "approve",
            chat.committee.as_str(),
            &root_seq,
        ]))
        .to(agent.clone())
        .in_thread(thread.root)
        .with_meta(meta::RELAYED_FROM, json!(chat.seq.0))
        .with_meta(meta::TRIAGE, verdict.value());
        if let Some(source) = &chat.source {
            task = task.with_source(source.clone());
        }
        if let Some(refs) = root.refs.clone() {
            task = task.with_refs(refs);
        }
        self.append(task).await?;
        let mut status = StatusPayload::new(StatusState::Processing);
        status.reason = Some(render::approved_reason(l, &agent));
        let processing = NewEvent::new(
            chat.committee.clone(),
            Kind::Status,
            Address::Speaker,
            render::approved_reason(l, &agent),
        )
        .with_id(derived_event_id(&[
            "approve-status",
            chat.committee.as_str(),
            &root_seq,
        ]))
        .in_thread(thread.root)
        .with_meta(meta::STATUS, status.value());
        self.append(processing).await?;
        self.reply_to_chat(chat, &render::handed_off(l, &agent))
            .await
            .map(drop)
    }
}
