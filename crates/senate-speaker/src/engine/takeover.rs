//! Continuity: presence and checkpoints in the mirror group, and the
//! Speaker taking over an away agent's directed questions.
//!
//! Everything here is per agent. A person may have several agents; one of
//! them being away says nothing about the others, and a question for one
//! agent is never answered from another's checkpoint, nor rerouted to a
//! sibling of the same person.
//!
//! # Lines in the mirror group
//!
//! Each `presence` transition (written by the server, `from: committee`) is
//! one root line in the mirror group (the Agent group when there is one),
//! e.g. `[senator::claude] 失联` / `[senator::claude] absent`,
//! `… taken over by the Speaker`, `… reassigned → [senator::copilot]`.
//! At output level "answers only" just `absent`, `taken_over` and `online`
//! are posted; otherwise every transition. Each hand-off `checkpoint`
//! (trigger `handoff`) is one line too, at "status" and "everything"; other
//! checkpoints (a commit, the interval, a session end, the tool) are not
//! posted, only logged and in the brief. Both are the event's copy in that
//! group, so a replay posts nothing twice.
//!
//! # Takeover
//!
//! When a directed `question` to an agent is due (see `fallback_on_arrival`
//! and `on_fallback_due` in the engine), *that agent's* own presence
//! decides:
//!
//! | presence of the agent | when | what |
//! | --- | --- | --- |
//! | `absent`, `taken_over` | at once | takeover (below) |
//! | `disconnected` | after `answer_fallback_secs` (from the question) | a plain answer for it, or a takeover if it is away by then |
//! | `online`, `reassigned`, none known | after the window with a live connection, at once without | a plain answer for it |
//!
//! Presence is read when the question is processed and again when it is
//! due (after the committee has caught up), so `absent` arriving during a
//! window makes the question due at once, as a takeover.
//!
//! A takeover reads the agent's own latest checkpoint. Missing, or older
//! than `takeover_stale_hours`: escalate. Otherwise the Brain answers from
//! its files, the brief and the recent log ([`Takeover`]); an answer it
//! marks not covered (or no answer at all) is escalated too. A covered
//! answer first has [`Hub::mark_taken_over`](senate_core::Hub::mark_taken_over)
//! record `taken_over` (a no-op after the first), then is appended as the
//! Speaker's answer on the agent's behalf with
//! [`TAKEOVER_META`](super::TAKEOVER_META), which is what shows it as a
//! takeover. If the agent came back meanwhile (`mark_taken_over` refuses: it
//! is online), the draft is dropped and the question gets a fresh answer
//! window. Requests are never taken over: they always go to the person.
//!
//! **Escalating** is a Speaker `status` `suspended` in the question's
//! thread, `on_behalf_of` the agent, with `meta.takeover` and the reason,
//! which closes the question. When that status is processed, the reason is
//! posted under the thread's copy (the Agent group's when it has one) with
//! a native mention of each other person one of whose agents has been
//! active on the committee, one post each, keyed `<group>#takeover-<handle>`
//! on the status.
//!
//! **Hand-back.** When the agent's presence goes `taken_over` → `online`,
//! the Speaker appends one `note` to it listing every answer with
//! `meta.takeover` given since that `taken_over` (folded from the log), and
//! posts a line in the mirror group (`<group>#handback` on the presence
//! event). Nothing when there were none.
//!
//! Every step is keyed to an event: the escalation status and the answer by
//! (committee, question, agent), the hand-back note by the `online` event, so
//! a restart anywhere in between redoes at most a Brain call and never
//! writes or posts anything twice.

use std::time::Duration;

use chrono::Utc;
use senate_core::{
    CheckpointPayload, CheckpointRecord, CheckpointTrigger, MirrorRef, Name, PresencePayload,
    PresenceState, StatusState,
};

use super::{
    Address, Answer, BrainError, CommitteeId, Drafted, Engine, Event, HubError, Kind, MirrorPlan,
    NewEvent, OutputLevel, PostTarget, STALL_RETRY, SenatorName, Seq, Stall, StatusPayload,
    TAKEOVER_META, Takeover, behalf_answer, debug, derived_event_id, json, meta, warn,
};
use crate::render::{self, HandedBack, NoCover};

impl Engine {
    /// `agent`'s own presence on the committee as the server sees it now;
    /// `None` when it has none.
    pub(super) async fn presence_state(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Result<Option<PresenceState>, Stall> {
        Ok(self
            .hub
            .presence(committee)
            .await?
            .agents
            .into_iter()
            .find(|p| p.agent == *agent)
            .map(|p| p.state))
    }

    /// `/status`'s presence lines: every agent with a presence or a
    /// checkpoint, its state, lag and the age of its latest checkpoint.
    pub(super) async fn presence_lines(&self, committee: &CommitteeId) -> Vec<String> {
        let l = self.locale(committee);
        let presence = match self.hub.presence(committee).await {
            Ok(page) => page.agents,
            Err(error) => {
                debug!(%committee, %error, "presence unavailable for /status");
                Vec::new()
            }
        };
        let checkpoints = self
            .committees
            .get(committee)
            .and_then(|s| s.brief.as_ref())
            .map(|b| b.brief().checkpoints)
            .unwrap_or_default();
        let mut agents: Vec<SenatorName> = presence.iter().map(|p| p.agent.clone()).collect();
        agents.extend(checkpoints.iter().map(|c| c.agent.clone()));
        agents.retain(|agent| *agent != self.me);
        agents.sort();
        agents.dedup();
        let now = Utc::now();
        agents
            .iter()
            .map(|agent| {
                let checkpoint = checkpoints
                    .iter()
                    .find(|c| c.agent == *agent)
                    .map(|c| (c.seq, now - c.ts));
                self.seat_names().display_bylines(
                    &render::presence_status(
                        l,
                        agent,
                        presence.iter().find(|p| p.agent == *agent),
                        checkpoint,
                    ),
                    committee,
                )
            })
            .collect()
    }

    /// A `presence` transition: its line in the mirror group; questions
    /// waiting on an agent that is now away are due; an agent back from a
    /// takeover gets its mailbox handed back.
    pub(super) async fn on_presence(&mut self, event: &Event) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        if event.from != Address::Committee {
            return Ok(()); // only the server's count
        }
        let Some(payload) = PresencePayload::of(event) else {
            return Ok(());
        };
        let Some(state) = self.committees.get(&event.committee) else {
            return Ok(());
        };
        let previous = state
            .presence
            .get(&payload.agent)
            .filter(|seen| seen.seq == event.seq)
            .and_then(|seen| seen.previous);
        let group = state.mirror_group().map(|g| g.group_id.clone());
        let wanted = state.output_level() != OutputLevel::Answers
            || matches!(
                payload.state,
                PresenceState::Absent | PresenceState::TakenOver | PresenceState::Online
            );
        if wanted && let Some(group) = &group {
            self.post_line(
                event,
                group,
                group.clone(),
                render::presence_line(l, &payload),
            )
            .await?;
        }
        match payload.state {
            PresenceState::Absent | PresenceState::TakenOver => {
                self.due_for(&event.committee, &payload.agent);
            }
            PresenceState::Online if previous == Some(PresenceState::TakenOver) => {
                self.hand_back(event, &payload.agent, group.as_deref())
                    .await?;
            }
            PresenceState::Reassigned => self.on_reassigned(event, &payload).await?,
            PresenceState::Online => self.on_reassignment_ended(event).await?,
            _ => {}
        }
        Ok(())
    }

    /// An agent's hand-off `checkpoint`: one line in the mirror group,
    /// unless the output level is "answers only". Other checkpoints (commit,
    /// interval, …) stay in the log and the brief only.
    pub(super) async fn on_checkpoint(&mut self, event: &Event) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        if !self.roster.is_agent_address(&event.from) {
            return Ok(());
        }
        let Some(payload) = CheckpointPayload::of(event) else {
            return Ok(());
        };
        if payload.trigger != CheckpointTrigger::Handoff {
            return Ok(());
        }
        let Some(state) = self.committees.get(&event.committee) else {
            return Ok(());
        };
        if state.output_level() == OutputLevel::Answers {
            return Ok(());
        }
        let Some(group) = state.mirror_group().map(|g| g.group_id.clone()) else {
            return Ok(());
        };
        let text = render::checkpoint_line(l, self.seat_names(), event, &payload);
        self.post_line(event, &group, group.clone(), text).await
    }

    /// A new root `text` in `group`, recorded as `event`'s copy `target`.
    async fn post_line(
        &self,
        event: &Event,
        group: &str,
        target: String,
        text: String,
    ) -> Result<(), Stall> {
        let plan = MirrorPlan {
            to: PostTarget::NewRoot {
                group_id: group.to_owned(),
            },
            text,
            target,
            record_as: None,
            card: None,
        };
        self.run_plan(&event.committee, event.seq, plan).await
    }

    /// Every question still waiting on `agent` is due now (it is away: the
    /// takeover decides). Only that agent's: its siblings are not touched.
    fn due_for(&mut self, committee: &CommitteeId, agent: &SenatorName) {
        let roots: Vec<Seq> = self
            .committees
            .get(committee)
            .map(|state| {
                state
                    .open
                    .iter()
                    .filter(|((_, who), open)| who == agent && !open.answering)
                    .map(|((root, _), _)| *root)
                    .collect()
            })
            .unwrap_or_default();
        for root in roots {
            self.arm_now(committee, root, agent.clone());
        }
    }

    // ----- taking over ---------------------------------------------------------

    /// `question` (thread `root`) is due and `agent` is away: answer from
    /// its own latest checkpoint, or escalate when there is none fresh
    /// enough. Another agent of the same person never stands in.
    pub(super) async fn take_over(
        &mut self,
        committee: CommitteeId,
        root: Seq,
        agent: SenatorName,
        question: Seq,
    ) {
        // The cover chain: the agent's own secretaries, then the desk.
        match self.cover_by(&committee, &agent).await {
            super::desk::CoverBy::Secretary => {
                self.cover_as_secretary(committee, root, agent, question)
                    .await;
                return;
            }
            super::desk::CoverBy::Nobody => {
                self.forget_open(&committee, root, &agent);
                return;
            }
            super::desk::CoverBy::Office => {}
        }
        let checkpoint = match self.hub.latest_checkpoint(&committee, &agent).await {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                warn!(%committee, %error, "cannot read the checkpoint; takeover deferred");
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
                self.escalate_takeover(&committee, root, &agent, question, why)
                    .await;
                return;
            }
        };
        let brief = match self.hub.brief(&committee).await {
            Ok(brief) => Some(brief),
            Err(error) => {
                debug!(%committee, %error, "no brief for the takeover; checkpoint and log only");
                None
            }
        };
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
        self.draft_answer(committee, root, agent, question, Some(takeover))
            .await;
    }

    /// A takeover draft is back: post it as the Speaker's answer for the
    /// agent (recording `taken_over` first), or escalate when it does not
    /// cover the question.
    pub(super) async fn on_takeover_answered(
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
                self.escalate_takeover(&committee, root, &agent, question, NoCover::Uncovered)
                    .await;
                return;
            }
            Err(error) => {
                warn!(%committee, %error, "the brain could not answer for an away agent");
                self.escalate_takeover(&committee, root, &agent, question, NoCover::Unavailable)
                    .await;
                return;
            }
        };
        match self.hub.mark_taken_over(&committee, &agent).await {
            Ok(_) => {}
            Err(HubError::Rejected(reason)) => {
                // It came back while the Brain was drafting: its own session
                // gets the question; a fresh window before anyone answers.
                debug!(%committee, %agent, %reason, "not taken over; the agent is back");
                let window = self.fallback_after;
                self.arm(&committee, root, agent, window);
                return;
            }
            Err(error) => {
                warn!(%committee, %error, "cannot record the takeover; deferred");
                self.arm(&committee, root, agent, STALL_RETRY);
                return;
            }
        }
        let answer = behalf_answer(&committee, root, &agent, question, asker, text)
            .with_meta(TAKEOVER_META, json!(true));
        match self.append(answer).await {
            Ok(_) => self.forget_open(&committee, root, &agent),
            Err(stall) => {
                warn!(%committee, error = %stall, "takeover answer deferred");
                self.arm(&committee, root, agent, STALL_RETRY);
            }
        }
    }

    pub(super) fn forget_open(&mut self, committee: &CommitteeId, root: Seq, agent: &SenatorName) {
        if let Some(open) = self
            .committees
            .get_mut(committee)
            .and_then(|state| state.open.remove(&(root, agent.clone())))
            && let Some(timer) = open.timer
        {
            timer.abort();
        }
    }

    /// Not answered from the checkpoint: the question waits on people
    /// (`suspended`, with the reason). The mentions are posted when the
    /// status is processed.
    pub(super) async fn escalate_takeover(
        &mut self,
        committee: &CommitteeId,
        root: Seq,
        agent: &SenatorName,
        question: Seq,
        why: NoCover,
    ) {
        let l = self.locale(committee);
        let body = match self.hub.get(committee, question).await {
            Ok(event) => event.map(|e| e.body).unwrap_or_default(),
            Err(error) => {
                warn!(%committee, %error, "takeover escalation deferred");
                self.arm(committee, root, agent.clone(), STALL_RETRY);
                return;
            }
        };
        let behalf = Address::senator(agent);
        let reason = render::takeover_escalation(l, &behalf, why, &body);
        let mut status = StatusPayload::new(StatusState::Suspended);
        status.reason = Some(reason.clone());
        let event = NewEvent::new(committee.clone(), Kind::Status, Address::Speaker, reason)
            .with_id(derived_event_id(&[
                "takeover-escalate",
                committee.as_str(),
                &question.to_string(),
                &agent.to_string(),
            ]))
            .in_thread(root)
            .on_behalf_of(behalf)
            .with_meta(meta::STATUS, status.value())
            .with_meta(TAKEOVER_META, json!(true));
        match self.append(event).await {
            Ok(_) => self.forget_open(committee, root, agent),
            Err(stall) => {
                warn!(%committee, error = %stall, "takeover escalation deferred");
                self.arm(committee, root, agent.clone(), STALL_RETRY);
            }
        }
    }

    /// The Speaker's takeover escalation `status` (in thread `root`): its
    /// reason under the thread's copy, mentioning each other person one of
    /// whose agents has been active on the committee.
    pub(super) async fn post_takeover_escalation(
        &self,
        event: &Event,
        root: Seq,
        status: &StatusPayload,
    ) -> Result<(), Stall> {
        let Some(away) = event
            .on_behalf_of
            .as_ref()
            .filter(|a| self.roster.is_agent_address(a))
            .and_then(Address::as_senator)
        else {
            return Ok(());
        };
        let away_owner = self.roster.owner_of(away);
        let platform = self.surface.platform().to_owned();
        let Some((message_ref, group)) = self
            .thread_target(&event.committee, root, &platform)
            .await?
        else {
            return Ok(());
        };
        let text = status.reason.clone().unwrap_or_else(|| event.body.clone());
        let mut people: Vec<Name> = self
            .committees
            .get(&event.committee)
            .map(|state| {
                state
                    .activity
                    .keys()
                    .filter_map(|agent| self.roster.owner_of(agent))
                    .filter(|person| Some(*person) != away_owner)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        people.sort();
        people.dedup();
        let to = PostTarget::Reply {
            message_ref,
            in_thread: true,
        };
        if people.is_empty() {
            let target = format!("{group}#takeover");
            if self.has_copy(&event.committee, event.seq, &target).await? {
                return Ok(());
            }
            let operation = self.op_id(&event.committee, event.seq, &target)?;
            if let Some(posted) = self.post(to, &text, &operation).await? {
                self.record(event, target, posted).await?;
            }
            return Ok(());
        }
        for person in people {
            let target = format!("{group}#takeover-{person}");
            self.post_mention_as(event, to.clone(), target, &person, &text)
                .await?;
        }
        Ok(())
    }

    /// Record `posted` as `event`'s copy `target`.
    pub(super) async fn record(
        &self,
        event: &Event,
        target: String,
        posted: senate_core::Posted,
    ) -> Result<(), Stall> {
        self.hub
            .put_mirror(MirrorRef {
                committee: event.committee.clone(),
                seq: event.seq,
                platform: self.surface.platform().to_owned(),
                target,
                message_ref: posted.message_ref,
                thread_ref: posted.thread_ref,
            })
            .await?;
        Ok(())
    }

    // ----- handing back ----------------------------------------------------------

    /// `agent` is back from a takeover: one note to it listing every answer
    /// given for it since, and one line in the mirror group.
    async fn hand_back(
        &mut self,
        event: &Event,
        agent: &SenatorName,
        group: Option<&str>,
    ) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        let Some((since, covered)) = self
            .committees
            .get(&event.committee)
            .and_then(|state| state.taken_over.get(agent))
            .cloned()
        else {
            return Ok(());
        };
        if covered.is_empty() {
            return Ok(());
        }
        let mut answers = Vec::with_capacity(covered.len());
        for c in &covered {
            let question = self
                .hub
                .get(&event.committee, c.question)
                .await?
                .map(|e| e.body)
                .unwrap_or_default();
            answers.push(HandedBack {
                question_seq: c.question,
                answer_seq: c.answer,
                question,
                answer: c.body.clone(),
            });
        }
        let behalf = Address::senator(agent);
        let note = NewEvent::new(
            event.committee.clone(),
            Kind::Note,
            Address::Speaker,
            render::handback_note(l, since, &answers),
        )
        .with_id(derived_event_id(&[
            "handback",
            event.committee.as_str(),
            &agent.to_string(),
            &event.seq.to_string(),
        ]))
        .to(behalf.clone());
        self.append(note).await?;
        if let Some(group) = group {
            let text = render::handback_line(l, &behalf, answers.len());
            self.post_line(event, group, format!("{group}#handback"), text)
                .await?;
        }
        Ok(())
    }
}
