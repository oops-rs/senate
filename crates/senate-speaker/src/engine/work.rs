//! Work on request and reassignment: only people ask, on their own behalf.
//!
//! # Who may ask
//!
//! Only `chat` from the platform is an instruction (agents never are; an
//! agent's own `/work` or `/reassign` to the Speaker gets one note back
//! saying it cannot). Both intents change state, so a guest is refused.
//! Work is also developers-only: a person one of whose agents has been
//! active on the committee (an event of its, or a presence, or a live
//! connection), or who is listed in `[speaker.work.developers]` for it;
//! anyone else is told only the committee's developers may ask for code.
//! Both are always restated and run only after the asker's yes, the fixed
//! syntax included.
//!
//! # Whose work
//!
//! A run starts from one agent's checkpoint. The target is settled to a
//! concrete agent before anything is restated: named outright
//! (`/work codex …`), a person's only agent, or, for a person with
//! several, the one the Speaker settles on (asking in the thread when it
//! cannot tell). The restatement names the agent, so a wrong pick is caught
//! at the yes. A reassignment names an agent on both sides the same way, and
//! may hand work between two agents of one person.
//!
//! # A work run
//!
//! The request is checked before it is restated: the server has a runner and
//! `[speaker.work]`; no other run of the committee is in flight; the target
//! agent has a latest checkpoint on a branch, pushed, with a remote URL whose
//! repository is in `allowed_repos`. The restatement names the checkpoint
//! (`branch@commit`), the new branch `<prefix>/<committee>/speaker-<run_id>`
//! (`run_id` = the seq of the asking `chat`; the prefix from
//! `[speaker.work] branch_prefix`), the instruction and whom it is for.
//!
//! The yes appends the run's root: a Speaker `note` with no `to`,
//! `on_behalf_of` the asker, `meta.work` = the [`WorkRun`] `queued`
//! ([`senate_core::work`]). When the root is processed it gets its card
//! ([`TaskBoard`](senate_core::TaskBoard) treats it as a task) and, unless
//! its thread already has a run status (a replay), a first Speaker `status`
//! (`queued`, keyed by the root) is appended and the run is spawned off the
//! mailbox, at most `max_concurrent_runs` at once and one per committee. The
//! runner's [`RunProgress`] comes back as [`RunEvent::Progress`]; steps are
//! folded and written as one `status` (`running`, steps, the branch link) at
//! most every `card_debounce_ms`, so the card moves. When the run ends, the
//! PR is opened with the [`Forge`](senate_work::Forge)
//! ([`PrRequest::for_run`]) if anything was pushed, and one terminal
//! `status` (keyed by the root, so there is only ever one) carries the final
//! [`WorkRun`], the PR / branch links, and the result text as its body. When
//! that status is processed its text is posted under the card and in the
//! asker's own thread: the PR link, or the summary and the next steps as
//! numbered options. A refused order says why.
//!
//! # Restart safety
//!
//! A run is never re-run automatically. The latest `meta.work` of each run
//! is folded from the log; when a committee is loaded, every run that is not
//! over is one that was in flight when the Speaker stopped, and gets the
//! terminal status `failed` saying the Speaker restarted, with the next
//! steps (its branch keeps whatever was pushed). A root replayed after a
//! restart is not started again when its thread already has a run status.
//!
//! # Reassignment
//!
//! `/reassign alice bob` (or 「把 alice 在 refund-v2 的工作交给 bob」), restated
//! and confirmed, asks the server ([`Hub::reassign`](senate_core::Hub::reassign))
//! to write `presence: reassigned` for alice's agent with `new_owner` bob's,
//! `on_behalf_of` the asker. It stands until undone: `/reassign bob alice`
//! (or `/reassign alice alice`, 「把 refund-v2 交还给 alice」) writes alice's
//! agent `online` again. When the `reassigned` presence is processed, the
//! Agent group gets a line saying whose work went to whom, and the new
//! owner one `note` with the old agent's latest checkpoint (branch, commit,
//! the three files) and how to start from its branch (keyed by the presence
//! event). A hand-back posts its line and tells the agent that held it.
//!
//! While it stands, the new owner covers the old agent: a question to the
//! old agent gets no answer from the Speaker while the new owner is
//! connected (its bridge is pushed it), the plain answer-for-the-agent when
//! it is not, and never a takeover; the new owner's answer in a thread
//! addressed to the old agent closes its open question, and is shown under
//! the new owner's label with the agent it covers for.

use std::sync::Arc;

use senate_core::task::MAX_STEPS;
use senate_core::{
    CheckpointPayload, Link, Locale, Name, PresencePayload, PresenceState, SenatorName, Step,
    WorkRun, WorkState, is_work_root,
};
use senate_work::{
    CheckpointFiles, Delivery, PrRequest, RepoRef, RunOutcome, RunProgress, RunStatus, WorkOrder,
    default_next_steps,
};
use tokio::sync::mpsc;

use super::route::{Settled, Waiting};
use super::{
    Address, CommitteeId, Engine, Event, Flow, HubError, Kind, MirrorPlan, NewEvent, PendingAction,
    PostTarget, Seq, Stall, StatusPayload, Timer, debug, derived_event_id, meta, warn,
};
use crate::actor::{RunReport, SpeakerMsg};
use crate::brain::Intent;
use crate::render;
use crate::syntax::parse_command;

/// News of a run, back on the mailbox.
pub(crate) enum RunEvent {
    /// The run got a runner slot.
    Started { committee: CommitteeId, root: Seq },
    Progress {
        committee: CommitteeId,
        root: Seq,
        progress: RunProgress,
    },
    /// The run ended; `delivery` is the forge's answer when anything was
    /// pushed.
    Finished {
        committee: CommitteeId,
        root: Seq,
        outcome: RunOutcome,
        delivery: Option<Result<Delivery, String>>,
    },
    /// The debounced progress status is due.
    Flush { committee: CommitteeId, root: Seq },
}

/// A run in flight in this process.
impl RunEvent {
    /// The committee the run belongs to.
    pub(crate) fn committee(&self) -> &CommitteeId {
        match self {
            Self::Started { committee, .. }
            | Self::Progress { committee, .. }
            | Self::Finished { committee, .. }
            | Self::Flush { committee, .. } => committee,
        }
    }
}

pub(super) struct Run {
    run: WorkRun,
    order: WorkOrder,
    steps: Vec<Step>,
    flush: Option<Timer>,
    revision: u64,
}

impl Run {
    pub(super) fn abort(&mut self) {
        if let Some(timer) = self.flush.take() {
            timer.abort();
        }
    }
}

/// Sends a failed `Finished` if the run task ends without reporting (it
/// panicked), so the Speaker never waits on a run that is gone.
struct Report {
    myself: ractor::ActorRef<SpeakerMsg>,
    committee: CommitteeId,
    root: Seq,
    branch: String,
    locale: Locale,
    sent: bool,
}

impl Report {
    fn send(&self, event: RunEvent) {
        // Fails only when the Speaker is gone, and then nobody waits.
        let _ = self.myself.send_message(SpeakerMsg::Run(RunReport(event)));
    }

    fn finished(mut self, outcome: RunOutcome, delivery: Option<Result<Delivery, String>>) {
        self.sent = true;
        self.send(RunEvent::Finished {
            committee: self.committee.clone(),
            root: self.root,
            outcome,
            delivery,
        });
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        if !self.sent {
            self.send(RunEvent::Finished {
                committee: self.committee.clone(),
                root: self.root,
                outcome: RunOutcome {
                    status: RunStatus::Failed {
                        reason: "the run ended without a result".into(),
                    },
                    branch: self.branch.clone(),
                    pushed_commit: None,
                    summary: render::run_crashed(self.locale),
                    next_steps: default_next_steps(&self.branch, false, self.locale),
                },
                delivery: None,
            });
        }
    }
}

/// A GitHub URL for `branch` of `repo`, when the repo is on github.com.
fn branch_link(l: Locale, repo_url: Option<&str>, branch: &str) -> Option<Link> {
    let url = repo_url?;
    let repo = RepoRef::from_url(url).ok()?;
    url.contains("github.com").then(|| Link {
        label: render_label(&render::branch_label(l, branch)),
        url: format!("https://github.com/{}/tree/{branch}", repo.slug),
    })
}

fn render_label(label: &str) -> String {
    senate_work::clip_label(label)
}

/// The card state, links and so on of a run as it is now.
fn run_status(l: Locale, run: &WorkRun, steps: &[Step], pr: Option<&str>) -> StatusPayload {
    let mut status = StatusPayload::new(run.state.status_state());
    status.title = Some(render_label(&render::work_title(
        l,
        run.instruction.lines().next().unwrap_or_default().trim(),
    )));
    if !steps.is_empty() {
        status.steps = Some(steps.to_vec());
    }
    let mut links: Vec<Link> = branch_link(l, run.repo_url.as_deref(), &run.branch)
        .into_iter()
        .collect();
    if let Some(url) = pr {
        links.insert(
            0,
            Link {
                label: senate_work::pr_number_from_url(url)
                    .map_or_else(|| "PR".to_owned(), |n| format!("PR #{n}")),
                url: url.to_owned(),
            },
        );
    }
    if !links.is_empty() {
        status.links = Some(links);
    }
    if matches!(run.state, WorkState::Failed | WorkState::TimedOut) {
        status.escalated_to = Some(run.requester.clone());
    }
    status.reason = Some(render::work_reason(l, run));
    status
}

impl Engine {
    // ----- asking --------------------------------------------------------------

    /// Whether `person` is a developer of the committee (module docs): one
    /// of their agents has spoken or had presence on it, or is connected to
    /// it now (an agent's first `online` is not logged, so a bridge that has
    /// only joined leaves nothing in the log), or the config lists them.
    async fn is_developer(&self, committee: &CommitteeId, person: &Name) -> bool {
        let mine = |agent: &SenatorName| self.roster.owner_of(agent) == Some(person);
        let active = self.committees.get(committee).is_some_and(|state| {
            state.activity.keys().any(mine) || state.presence.keys().any(mine)
        });
        if active
            || self
                .work
                .as_ref()
                .is_some_and(|w| w.lists_developer(committee.as_str(), person.as_str()))
        {
            return true;
        }
        for agent in self.roster.agents_of(person) {
            if self.hub.is_agent_connected(committee, agent).await {
                return true;
            }
        }
        false
    }

    /// `/work <target> <instruction>`: settle whose checkpoint, check, then
    /// restate (always).
    pub(super) async fn request_work(
        &mut self,
        chat: &Event,
        target: &str,
        instruction: &str,
    ) -> Result<Flow, Stall> {
        let l = self.locale(&chat.committee);
        let committee = self.acting_committee(chat);
        let (Some(_), Some(work)) = (&self.runner, &self.work) else {
            return self.say(chat, &render::work_unconfigured(l)).await;
        };
        let allowed = work.allowed_repos.clone();
        let branches = work.branches.clone();
        let Some(asker) = self.person_of(&chat.from) else {
            return self.say(chat, &render::agents_cannot_work(l)).await;
        };
        if !self.is_developer(&committee, &asker).await {
            return self.say(chat, &render::not_a_developer(l)).await;
        }
        let instruction = instruction.trim();
        if instruction.is_empty() || target.trim().is_empty() {
            return self.say(chat, &render::work_whom(l)).await;
        }
        if self.preparing.as_ref().is_some_and(|actions| {
            actions
                .iter()
                .any(|a| matches!(a, PendingAction::Work { .. }))
        }) {
            return self.say(chat, &render::work_busy(l, chat.seq)).await;
        }
        if let Some(root) = self.run_in_flight(&committee) {
            return self.say(chat, &render::work_busy(l, root)).await;
        }
        let waiting = Waiting::Intent {
            intent: crate::brain::Intent::Work {
                target: target.to_owned(),
                instruction: instruction.to_owned(),
            },
            said: target.to_owned(),
            confirm: true,
        };
        let agent = match self.settle(chat, target, instruction, waiting).await? {
            Settled::To(senator) if !self.roster.is_person(&senator) => senator,
            Settled::To(person) => {
                // A person with no agent here has no checkpoint to start from.
                return self
                    .say(chat, &render::no_agent_to_work_from(l, person.name()))
                    .await;
            }
            Settled::Unknown | Settled::Child(_) => {
                let text = render::unknown_targets(l, &[target.to_owned()], &self.roster);
                return self.say(chat, &text).await;
            }
            Settled::Asked => return Ok(Flow::Done),
        };
        let Some(record) = self.hub.latest_checkpoint(&committee, &agent).await? else {
            return self.say(chat, &render::no_checkpoint(l, &agent)).await;
        };
        let c = &record.checkpoint;
        if c.branch == "HEAD" || c.branch.trim().is_empty() {
            return self
                .say(chat, &render::checkpoint_detached(l, &agent))
                .await;
        }
        let Some(url) = c.remote_url.clone() else {
            return self
                .say(chat, &render::checkpoint_no_remote(l, &agent))
                .await;
        };
        if !c.pushed {
            return self
                .say(chat, &render::checkpoint_not_pushed(l, &agent, &record))
                .await;
        }
        let repo = match RepoRef::from_url(&url) {
            Ok(repo) => repo,
            Err(e) => {
                return self
                    .say(chat, &render::repo_refused(l, &e.to_string()))
                    .await;
            }
        };
        if let Err(e) = allowed.check(&repo.slug) {
            return self
                .say(chat, &render::repo_refused(l, &e.to_string()))
                .await;
        }
        let run = WorkRun {
            run_id: chat.seq.0,
            requester: asker,
            target_agent: agent,
            instruction: instruction.to_owned(),
            branch: branches.speaker(&committee, chat.seq.0),
            state: WorkState::Queued,
            base_branch: Some(c.branch.clone()),
            base_commit: Some(c.commit.clone()),
            repo_url: Some(url),
            checkpoint_seq: Some(record.seq),
            requested_in: Some(chat.seq),
            pr_url: None,
            pushed_commit: None,
            summary: None,
            next_steps: Vec::new(),
        };
        self.restate_or_run(
            chat,
            PendingAction::Work {
                committee,
                run: Box::new(run),
            },
            true,
        )
        .await?;
        Ok(Flow::Done)
    }

    /// `/reassign <from> <to>`: settle both sides to concrete agents (two
    /// agents of one person are fine), then restate (always).
    pub(super) async fn request_reassign(
        &mut self,
        chat: &Event,
        from: &str,
        to: &str,
    ) -> Result<Flow, Stall> {
        let l = self.locale(&chat.committee);
        let committee = self.acting_committee(chat);
        let presence = self.hub.presence(&committee).await?.agents;
        let owner_of = |agent: &SenatorName| {
            presence
                .iter()
                .find(|p| p.agent == *agent && p.state == PresenceState::Reassigned)
                .and_then(|p| p.new_owner.clone())
        };
        // 「交还给 alice」 with alice's several agents: the one whose work is
        // away is meant, when only one is.
        let handing_back = match super::route::resolve_name(&self.roster, to) {
            super::route::Resolved::Several { candidates, .. } if from == to => {
                let away: Vec<&SenatorName> = candidates
                    .iter()
                    .filter(|a| owner_of(a).is_some())
                    .collect();
                match away.as_slice() {
                    [one] => Some((*one).clone()),
                    _ => None,
                }
            }
            _ => None,
        };
        let intent = crate::brain::Intent::Reassign {
            from: from.to_owned(),
            to: to.to_owned(),
        };
        let mut sides = Vec::with_capacity(2);
        for raw in [from, to] {
            if let Some(agent) = &handing_back {
                sides.push(agent.clone());
                continue;
            }
            let waiting = Waiting::Intent {
                intent: intent.clone(),
                said: raw.to_owned(),
                confirm: true,
            };
            match self.settle(chat, raw, "", waiting).await? {
                Settled::To(agent) if !self.roster.is_person(&agent) => sides.push(agent),
                Settled::To(person) => {
                    let text = render::no_agent_to_reassign(l, person.name());
                    return self.say(chat, &text).await;
                }
                Settled::Unknown | Settled::Child(_) => {
                    let text = render::unknown_targets(l, &[raw.to_owned()], &self.roster);
                    return self.say(chat, &text).await;
                }
                Settled::Asked => return Ok(Flow::Done),
            }
        }
        let (from_a, to_a) = (sides[0].clone(), sides[1].clone());
        let action = if from_a == to_a {
            // `/reassign alice alice`, 「交还给 alice」.
            let Some(owner) = owner_of(&to_a) else {
                return self.say(chat, &render::not_reassigned(l, &to_a)).await;
            };
            PendingAction::Reassign {
                committee,
                agent: to_a.clone(),
                to: to_a,
                owner: Some(owner),
            }
        } else if owner_of(&to_a).as_ref() == Some(&from_a) {
            // `/reassign bob alice` while alice's work is with bob: back.
            PendingAction::Reassign {
                committee,
                agent: to_a.clone(),
                to: to_a,
                owner: Some(from_a),
            }
        } else {
            PendingAction::Reassign {
                committee,
                agent: from_a,
                to: to_a,
                owner: None,
            }
        };
        self.restate_or_run(chat, action, true).await?;
        Ok(Flow::Done)
    }

    /// An agent's own `/work` or `/reassign` to the Speaker: one note back.
    /// Never run: only people may ask.
    pub(super) async fn refuse_agent_work(&mut self, event: &Event) -> Result<bool, Stall> {
        let l = self.locale(&event.committee);
        if !self.roster.is_agent_address(&event.from)
            || !event.to.contains(&Address::Speaker)
            || !matches!(
                parse_command(&event.body, l),
                Some(Ok(Intent::Work { .. } | Intent::Reassign { .. }))
            )
        {
            return Ok(false);
        }
        let note = NewEvent::new(
            event.committee.clone(),
            Kind::Note,
            Address::Speaker,
            render::agents_cannot_work(l),
        )
        .with_id(derived_event_id(&[
            "agent-work-refused",
            event.committee.as_str(),
            &event.seq.to_string(),
        ]))
        .in_thread(event.thread.unwrap_or(event.seq))
        .to(event.from.clone());
        self.append(note).await?;
        Ok(true)
    }

    /// What the yes to a reassignment does. Both sides are agents, settled
    /// before the restatement; `requester` is the person who asked.
    pub(super) async fn execute_reassign(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
        to: &SenatorName,
        requester: &Address,
        strict: bool,
    ) -> Result<String, Stall> {
        let l = self.locale(committee);
        let Some(by) = self.person_of(requester) else {
            return Ok(render::guest_refused(l));
        };
        match self.hub.reassign(committee, agent, to, &by).await {
            Ok(Some(_)) if agent == to => Ok(render::handed_back(l, agent)),
            Ok(Some(_)) => Ok(render::reassigned(l, agent, to)),
            Ok(None) => Ok(render::already_reassigned(l, agent, to)),
            Err(HubError::Rejected(reason) | HubError::NotFound(reason)) => {
                if strict {
                    return Err(Stall::Hub(HubError::Rejected(reason)));
                }
                Ok(render::reassign_refused(l, &reason))
            }
            Err(error) => Err(Stall::Hub(error)),
        }
    }

    /// What the yes to work does: the run's root. It starts when the root
    /// is processed.
    pub(super) async fn execute_work(
        &self,
        committee: &CommitteeId,
        run: &WorkRun,
        strict: bool,
    ) -> Result<String, Stall> {
        let l = self.locale(committee);
        let root = NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::Speaker,
            render::work_root(l, run),
        )
        .with_id(derived_event_id(&[
            "work",
            committee.as_str(),
            &run.run_id.to_string(),
        ]))
        .on_behalf_of(Address::senior(&run.requester))
        .with_meta(meta::WORK, run.value());
        Ok(match self.append_action(root, strict).await? {
            Some(event) => render::work_started(l, event.seq),
            None => String::new(),
        })
    }

    // ----- running ---------------------------------------------------------------

    /// The root of a run in flight in this process on `committee`, if any.
    fn run_in_flight(&self, committee: &CommitteeId) -> Option<Seq> {
        self.runs
            .keys()
            .find(|(f, _)| f == committee)
            .map(|(_, root)| *root)
    }

    /// A run's root: its card, then (unless its thread shows it was started
    /// before) the run itself.
    pub(super) async fn on_work_root(&mut self, event: &Event) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        let group = self
            .committees
            .get(&event.committee)
            .and_then(super::CommitteeState::mirror_group)
            .map(|g| g.group_id.clone());
        if let Some(group) = group
            && let Some(plan) = self.card_plan(event, &group)
        {
            self.run_plan(&event.committee, event.seq, plan).await?;
        }
        let Some(run) = WorkRun::of(event) else {
            return Ok(());
        };
        let key = (event.committee.clone(), event.seq);
        if self.runs.contains_key(&key) {
            return Ok(());
        }
        let thread = self.hub.thread(&event.committee, event.seq).await?;
        match WorkRun::latest(thread.iter().filter(|e| e.seq > event.seq)) {
            Some(latest) if latest.state.is_terminal() => return Ok(()),
            Some(latest) => return self.interrupt(&event.committee, event.seq, latest).await,
            None => {}
        }
        if let Some(other) = self.run_in_flight(&event.committee) {
            let mut run = run;
            run.state = WorkState::Refused;
            run.summary = Some(render::work_conflict(l, other));
            return self
                .finish(&event.committee, event.seq, &run, &[], None)
                .await;
        }
        let order = match self.order_for(&event.committee, &run).await? {
            Ok(order) => order,
            Err(reason) => {
                let mut run = run;
                run.state = WorkState::Refused;
                run.summary = Some(reason);
                return self
                    .finish(&event.committee, event.seq, &run, &[], None)
                    .await;
            }
        };
        // The marker a replay sees: this run was started once.
        let marker = NewEvent::new(
            event.committee.clone(),
            Kind::Status,
            Address::Speaker,
            render::work_reason(l, &run),
        )
        .with_id(derived_event_id(&[
            "work-start",
            event.committee.as_str(),
            &event.seq.to_string(),
        ]))
        .in_thread(event.seq)
        .with_meta(meta::STATUS, run_status(l, &run, &[], None).value())
        .with_meta(meta::WORK, run.value());
        if self.append(marker).await?.is_none() {
            return Ok(());
        }
        self.spawn_run(key, run, order);
        Ok(())
    }

    /// The order a run asks for, or why there is none.
    async fn order_for(
        &self,
        committee: &CommitteeId,
        run: &WorkRun,
    ) -> Result<Result<WorkOrder, String>, Stall> {
        let l = self.locale(committee);
        let (Some(_), Some(work)) = (&self.runner, &self.work) else {
            return Ok(Err(render::work_unconfigured(l)));
        };
        let requester = &run.requester;
        if self.roster.is_person(&run.target_agent) {
            return Ok(Err(render::agents_cannot_work(l)));
        }
        let files = match run.checkpoint_seq {
            Some(seq) => self
                .hub
                .get(committee, seq)
                .await?
                .and_then(|e| CheckpointPayload::of(&e))
                .map(|c| CheckpointFiles {
                    progress: c.files.progress,
                    decisions: c.files.decisions,
                    open: c.files.open,
                })
                .unwrap_or_default(),
            None => CheckpointFiles::default(),
        };
        let repo = match RepoRef::from_url(run.repo_url.as_deref().unwrap_or_default()) {
            Ok(repo) => repo,
            Err(e) => return Ok(Err(render::repo_refused(l, &e.to_string()))),
        };
        let on_behalf_of = self.display_name(requester);
        let order = WorkOrder {
            committee: committee.clone(),
            run_id: run.run_id,
            requester: requester.clone(),
            on_behalf_of,
            target_agent: run.target_agent.clone(),
            instruction: run.instruction.clone(),
            repo,
            base_branch: run.base_branch.clone().unwrap_or_default(),
            base_commit: run.base_commit.clone().unwrap_or_default(),
            pr_base: work.pr_base.clone(),
            checkpoint_files: files,
            budget: work.budget,
            branches: work.branches.clone(),
            locale: l,
        };
        Ok(match order.validate(&work.allowed_repos) {
            Ok(()) => Ok(order),
            Err(e) => Err(render::repo_refused(l, &e.to_string())),
        })
    }

    fn spawn_run(&mut self, key: (CommitteeId, Seq), run: WorkRun, order: WorkOrder) {
        let Some(runner) = self.runner.clone() else {
            return;
        };
        let forge = Arc::clone(&self.forge);
        let permits = Arc::clone(&self.run_permits);
        let report = Report {
            myself: self.myself.clone(),
            committee: key.0.clone(),
            root: key.1,
            branch: order.branch(),
            locale: order.locale,
            sent: false,
        };
        let task_order = order.clone();
        self.running.spawn(async move {
            let Ok(_permit) = permits.acquire_owned().await else {
                return; // the Speaker is stopping
            };
            report.send(RunEvent::Started {
                committee: report.committee.clone(),
                root: report.root,
            });
            let (tx, mut rx) = mpsc::channel::<RunProgress>(64);
            let forward = async {
                while let Some(progress) = rx.recv().await {
                    report.send(RunEvent::Progress {
                        committee: report.committee.clone(),
                        root: report.root,
                        progress,
                    });
                }
            };
            let (outcome, ()) = tokio::join!(runner.run(&task_order, tx), forward);
            let pushed = outcome.pushed_commit.is_some()
                && !matches!(outcome.status, RunStatus::Refused { .. });
            let delivery = if pushed {
                let request = PrRequest::for_run(&task_order, &outcome);
                Some(forge.open_pr(&request).await.map_err(|e| e.to_string()))
            } else {
                None
            };
            report.finished(outcome, delivery);
        });
        self.runs.insert(
            key,
            Run {
                run,
                order,
                steps: Vec::new(),
                flush: None,
                revision: 0,
            },
        );
    }

    pub(crate) async fn on_run(&mut self, event: RunEvent) {
        match event {
            RunEvent::Started { committee, root } => {
                if let Some(run) = self.runs.get_mut(&(committee.clone(), root)) {
                    run.run.state = WorkState::Running;
                }
                self.flush_run(&committee, root).await;
            }
            RunEvent::Progress {
                committee,
                root,
                progress,
            } => {
                let delay = self.card_debounce;
                let Some(run) = self.runs.get_mut(&(committee.clone(), root)) else {
                    return;
                };
                run.run.state = WorkState::Running;
                let step = progress.to_step();
                match run.steps.iter_mut().find(|s| s.id == step.id) {
                    Some(existing) => *existing = step,
                    None => {
                        run.steps.push(step);
                        if run.steps.len() > MAX_STEPS {
                            let over = run.steps.len() - MAX_STEPS;
                            run.steps.drain(..over);
                        }
                    }
                }
                if run.flush.is_none() {
                    let message = committee.clone();
                    run.flush = Some(self.myself.send_after(delay, move || {
                        SpeakerMsg::Run(RunReport(RunEvent::Flush {
                            committee: message,
                            root,
                        }))
                    }));
                }
            }
            RunEvent::Flush { committee, root } => self.flush_run(&committee, root).await,
            RunEvent::Finished {
                committee,
                root,
                outcome,
                delivery,
            } => {
                self.on_run_finished(committee, root, outcome, delivery)
                    .await
            }
        }
    }

    /// Write the run's progress as one `status` now.
    async fn flush_run(&mut self, committee: &CommitteeId, root: Seq) {
        let l = self.locale(committee);
        let Some(run) = self.runs.get_mut(&(committee.clone(), root)) else {
            return;
        };
        if let Some(timer) = run.flush.take() {
            timer.abort();
        }
        run.revision += 1;
        let status = run_status(l, &run.run, &run.steps, None);
        let body = run
            .steps
            .last()
            .map_or_else(|| render::work_reason(l, &run.run), |s| s.label.clone());
        let event = NewEvent::new(committee.clone(), Kind::Status, Address::Speaker, body)
            .with_id(derived_event_id(&[
                "work-progress",
                committee.as_str(),
                &root.to_string(),
                &run.revision.to_string(),
            ]))
            .in_thread(root)
            .with_meta(meta::STATUS, status.value())
            .with_meta(meta::WORK, run.run.value());
        if let Err(stall) = self.append(event).await {
            // Progress is best effort; the terminal status carries the steps.
            warn!(%committee, %root, error = %stall, "a run's progress was not written");
        }
    }

    async fn on_run_finished(
        &mut self,
        committee: CommitteeId,
        root: Seq,
        outcome: RunOutcome,
        delivery: Option<Result<Delivery, String>>,
    ) {
        let l = self.locale(&committee);
        let Some(mut entry) = self.runs.remove(&(committee.clone(), root)) else {
            return;
        };
        entry.abort();
        let mut run = entry.run;
        run.state = match &outcome.status {
            RunStatus::Succeeded => WorkState::Succeeded,
            RunStatus::Failed { .. } => WorkState::Failed,
            RunStatus::TimedOut => WorkState::TimedOut,
            RunStatus::Refused { .. } => WorkState::Refused,
        };
        run.pushed_commit = outcome.pushed_commit.clone();
        run.summary = Some(match &outcome.status {
            RunStatus::Failed { reason } if !outcome.summary.contains(reason.as_str()) => {
                format!("{}（{reason}）", outcome.summary.trim())
            }
            _ => outcome.summary.trim().to_owned(),
        });
        run.next_steps = if outcome.next_steps.is_empty()
            && !matches!(run.state, WorkState::Succeeded | WorkState::Refused)
        {
            default_next_steps(&outcome.branch, outcome.pushed_commit.is_some(), l)
        } else {
            outcome.next_steps.clone()
        };
        let mut note = None;
        match delivery {
            Some(Ok(Delivery::PullRequest(pr))) => run.pr_url = Some(pr.url),
            Some(Ok(Delivery::Branch { .. })) | None => {}
            Some(Err(reason)) => {
                warn!(%committee, %root, %reason, "the forge could not open the run's PR");
                note = Some(render::pr_failed(l, &reason));
            }
        }
        debug!(%committee, %root, state = %run.state, branch = %entry.order.branch(), "run finished");
        if let Err(stall) = self
            .finish(&committee, root, &run, &entry.steps, note.as_deref())
            .await
        {
            // Retried at the next restart, as an interrupted run.
            warn!(%committee, %root, error = %stall, "a run's result was not written");
        }
    }

    /// The run's one terminal status (keyed by its root).
    async fn finish(
        &self,
        committee: &CommitteeId,
        root: Seq,
        run: &WorkRun,
        steps: &[Step],
        note: Option<&str>,
    ) -> Result<(), Stall> {
        let l = self.locale(committee);
        let status = run_status(l, run, steps, run.pr_url.as_deref());
        let event = NewEvent::new(
            committee.clone(),
            Kind::Status,
            Address::Speaker,
            render::work_result(l, run, note),
        )
        .with_id(derived_event_id(&[
            "work-done",
            committee.as_str(),
            &root.to_string(),
        ]))
        .in_thread(root)
        .with_meta(meta::STATUS, status.value())
        .with_meta(meta::WORK, run.value());
        self.append(event).await.map(drop)
    }

    /// A run found unfinished in the log with nothing running it: it was in
    /// flight when the Speaker stopped. Never re-run (module docs).
    async fn interrupt(
        &self,
        committee: &CommitteeId,
        root: Seq,
        mut run: WorkRun,
    ) -> Result<(), Stall> {
        let l = self.locale(committee);
        run.state = WorkState::Failed;
        run.summary = Some(render::work_interrupted(l));
        run.next_steps = default_next_steps(&run.branch, true, l);
        self.finish(committee, root, &run, &[], None).await
    }

    /// After a committee is loaded: every run the log says is not over.
    pub(super) async fn interrupt_runs(&mut self, committee: &CommitteeId) {
        let open: Vec<(Seq, WorkRun)> = self
            .committees
            .get(committee)
            .map(|state| {
                state
                    .work
                    .iter()
                    .filter(|(_, run)| !run.state.is_terminal())
                    .map(|(root, run)| (*root, run.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for (root, run) in open {
            if self.runs.contains_key(&(committee.clone(), root)) {
                continue;
            }
            warn!(%committee, %root, "a work run was in flight at restart; marking it failed");
            if let Err(stall) = self.interrupt(committee, root, run).await {
                warn!(%committee, %root, error = %stall, "could not mark the interrupted run");
            }
        }
    }

    /// A Speaker `status` of a run: when it is the terminal one, its text
    /// under the card and in the asker's thread.
    pub(super) async fn on_work_status(
        &self,
        event: &Event,
        root: Seq,
        run: &WorkRun,
    ) -> Result<(), Stall> {
        if !run.state.is_terminal() {
            return Ok(());
        }
        let platform = self.surface.platform().to_owned();
        if let Some((message_ref, group)) = self
            .thread_target(&event.committee, root, &platform)
            .await?
        {
            self.run_plan(
                &event.committee,
                event.seq,
                MirrorPlan {
                    to: PostTarget::Reply {
                        message_ref,
                        in_thread: true,
                    },
                    text: event.body.clone(),
                    target: format!("{group}#work-result"),
                    record_as: None,
                    card: None,
                },
            )
            .await?;
        }
        let Some(chat_seq) = run.requested_in else {
            return Ok(());
        };
        let Some(chat) = self.hub.get(&event.committee, chat_seq).await? else {
            return Ok(());
        };
        let (Some(mirror), Some(source)) =
            (chat.mirror.filter(|m| m.platform == platform), chat.source)
        else {
            return Ok(());
        };
        self.run_plan(
            &event.committee,
            event.seq,
            MirrorPlan {
                to: PostTarget::Reply {
                    message_ref: mirror.message_ref,
                    in_thread: true,
                },
                text: event.body.clone(),
                target: format!("{}#work-result-asked", source.group_id),
                record_as: None,
                card: None,
            },
        )
        .await
    }

    // ----- reassignment ------------------------------------------------------------

    /// How branches are named on this deployment.
    pub(super) fn branch_naming(&self) -> senate_core::BranchNaming {
        self.work
            .as_ref()
            .map(|w| w.branches.clone())
            .unwrap_or_default()
    }

    /// The agent `agent`'s work is with now, if it is reassigned.
    pub(super) fn cover_of(
        &self,
        committee: &CommitteeId,
        agent: &SenatorName,
    ) -> Option<SenatorName> {
        self.committees
            .get(committee)
            .and_then(|state| state.reassigned.get(agent))
            .cloned()
    }

    /// The agent an answer in thread `root` covers for: one the root was
    /// addressed to, whose work is with the answer's author.
    pub(super) async fn covering_for(
        &self,
        event: &Event,
        root: Seq,
    ) -> Result<Option<SenatorName>, Stall> {
        let people = self.roster.people();
        let Some(author) = people.as_agent(&event.from) else {
            return Ok(None);
        };
        let Some(state) = self.committees.get(&event.committee) else {
            return Ok(None);
        };
        if !state.reassigned.values().any(|owner| owner == author) {
            return Ok(None);
        }
        let to = match state.board.get(root) {
            Some(thread) => thread.to.clone(),
            None => self
                .hub
                .get(&event.committee, root)
                .await?
                .map(|e| e.to)
                .unwrap_or_default(),
        };
        Ok(to
            .iter()
            .filter_map(|a| people.as_agent(a))
            .find(|old| state.reassigned.get(*old) == Some(author))
            .cloned())
    }

    /// `reassigned` was processed: the new owner's one push.
    pub(super) async fn on_reassigned(
        &mut self,
        event: &Event,
        payload: &PresencePayload,
    ) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        let Some(owner) = &payload.new_owner else {
            return Ok(());
        };
        let checkpoint = self
            .hub
            .latest_checkpoint(&event.committee, &payload.agent)
            .await?;
        let by = event
            .on_behalf_of
            .as_ref()
            .and_then(Address::as_senator)
            .and_then(|s| self.person_behind(s));
        let note = NewEvent::new(
            event.committee.clone(),
            Kind::Note,
            Address::Speaker,
            render::reassign_note(
                l,
                &event.committee,
                &payload.agent,
                self.roster.owner_of(owner).unwrap_or(owner.name()),
                by.as_ref(),
                checkpoint.as_ref(),
                &self.branch_naming(),
            ),
        )
        .with_id(derived_event_id(&[
            "reassign-note",
            event.committee.as_str(),
            &event.seq.to_string(),
        ]))
        .to(Address::senator(owner));
        self.append(note).await.map(drop)
    }

    /// A hand-back was processed: tell the agent that held the work.
    pub(super) async fn on_reassignment_ended(&mut self, event: &Event) -> Result<(), Stall> {
        let l = self.locale(&event.committee);
        let Some(payload) = PresencePayload::of(event) else {
            return Ok(());
        };
        let Some(owner) = self
            .committees
            .get(&event.committee)
            .and_then(|state| state.handbacks.get(&event.seq))
            .cloned()
        else {
            return Ok(());
        };
        let note = NewEvent::new(
            event.committee.clone(),
            Kind::Note,
            Address::Speaker,
            render::work_handback_note(l, &event.committee, &payload.agent),
        )
        .with_id(derived_event_id(&[
            "handback-owner-note",
            event.committee.as_str(),
            &event.seq.to_string(),
        ]))
        .to(Address::Senator(owner));
        self.append(note).await.map(drop)
    }
}

/// The run carried by a work thread's root, if `event` is one.
pub(super) fn work_root_run(event: &Event) -> Option<WorkRun> {
    is_work_root(event).then(|| WorkRun::of(event)).flatten()
}
