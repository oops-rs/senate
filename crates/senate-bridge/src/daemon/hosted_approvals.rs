//! Watches hosted sessions for a request to attach and attend to a prompt.
//! State is local, and every notice and archive is a normal committee event.

use super::*;
use crate::host;
use crate::host_approval::{
    ApprovalNotice, ClaudeCandidate, CodexApprovalOutcome, CodexApprovalSignal, HostApprovalState,
    PromptDecision, observed_at, tool_label,
};
use crate::ipc::{ClaudeApprovalSignal, HostedPrompt};
use senate_core::StatusState;
use tokio::sync::mpsc;

const RETRY: Duration = Duration::from_secs(1);
const POST_TIMEOUT: Duration = Duration::from_secs(5);
const POST_FAILURE_LIMIT: u8 = 6;
// Permission hooks can arrive before Claude paints its dialog.
const CLAUDE_DIALOG_GRACE_NS: u64 = 2_000_000_000;

#[derive(Debug, thiserror::Error)]
enum PostError {
    #[error(transparent)]
    Server(#[from] ApiClientError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Thread(#[from] IpcError),
}

struct PostBackoff {
    failures: u32,
    ready_at: Instant,
}

#[derive(Default)]
pub(super) struct Monitor {
    wake: Notify,
    // Keep an archive snapshot from racing the confirmed terminal keystroke.
    pub(super) claude_answer: tokio::sync::RwLock<()>,
    missing_viewers: Mutex<BTreeSet<EventId>>,
}

impl Shared {
    pub(super) fn log_missing_host_viewers(&self, paths: &Paths) -> Result<(), StoreError> {
        let mut missing = BTreeSet::new();
        let mut reported = lock(&self.hosted_approvals.missing_viewers);
        for id in host::running_ids(paths)? {
            if !host::awaiting_publication(paths, &id)
                && host::viewer_activity(paths, &id).is_none()
                && let Some(session) = self.store.session_for_host(&id)?
            {
                if !reported.contains(&id) {
                    tracing::warn!(host = %id, %session, "viewer activity is unavailable for hosted approval notices");
                }
                missing.insert(id);
            }
        }
        *reported = missing;
        Ok(())
    }

    pub(super) fn approval_host(
        &self,
        session: &str,
    ) -> Result<Option<(EventId, Session)>, StoreError> {
        let Some(id) = self.store.session_host(session)? else {
            return Ok(None);
        };
        let paths = Paths::new(&self.home);
        if self.hosts.unpublished(session)
            || host::awaiting_publication(&paths, &id)
            || !host::running(&paths, &id)?
        {
            return Ok(None);
        }
        Ok(self
            .store
            .session(session)?
            .filter(|session| session.state != SessionState::Ended)
            .map(|session| (id, session)))
    }

    pub(super) async fn on_hosted_claude_approval(
        &self,
        session_id: &str,
        host_id: &EventId,
        at: u64,
        signal: ClaudeApprovalSignal,
    ) -> Result<serde_json::Value, IpcError> {
        let began = Instant::now();
        // The stable seat can switch its native ID between two hooks.
        // Validate and record a hook under the registry's binding lock.
        let _binding = self.session_bindings.read().await;
        if (1..=observed_at()).contains(&at)
            && let Some((id, session)) = self.approval_host(session_id).map_err(internal)?
            && id == *host_id
            && session.runtime == Runtime::Claude
        {
            let paths = Paths::new(&self.home);
            let input_at =
                host::viewer_activity(&paths, host_id).map_or(0, |viewer| viewer.input_at);
            self.edit_approvals(host_id, |state| {
                let contextualized = set_context(state, session_id);
                let attended = observe_input(state, input_at);
                let observed = match signal {
                    ClaudeApprovalSignal::Requested { tool, fingerprint } => {
                        if at > input_at && state.claude.as_ref().is_none_or(|prior| prior.at <= at)
                        {
                            state.claude = Some(ClaudeCandidate {
                                tool,
                                fingerprint,
                                at,
                            });
                            true
                        } else {
                            false
                        }
                    }
                    ClaudeApprovalSignal::Pending => {
                        let candidate = state.claude.as_ref().filter(|pending| pending.at <= at);
                        let (prompt, tool) = candidate.map_or_else(
                            || ("claude:unspecified".into(), None),
                            |candidate| {
                                (candidate.fingerprint.clone(), Some(candidate.tool.clone()))
                            },
                        );
                        at > input_at && open_notice(state, &session, at, prompt, tool)
                    }
                    ClaudeApprovalSignal::Finished { fingerprint } => {
                        let completed = state.claude.as_ref().is_some_and(|candidate| {
                            candidate.fingerprint == fingerprint && candidate.at <= at
                        });
                        if completed {
                            state.claude = None;
                        }
                        close_notices(state, |notice| {
                            notice.prompt == fingerprint && notice.at <= at
                        }) || completed
                    }
                };
                contextualized || attended || observed
            })
            .map_err(internal)?;
            self.on_host_hook_processed(host_id, began);
        }
        Ok(serde_json::json!({}))
    }

    fn observe_codex_resolution(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        outcome: CodexApprovalOutcome,
        at: u64,
    ) -> Result<(), StoreError> {
        let Some((id, session)) = self.approval_host(session_id)? else {
            return Ok(());
        };
        if session.runtime == Runtime::Codex {
            self.edit_approvals(&id, |state| {
                let applies = |n: &ApprovalNotice| {
                    !n.screen && n.at <= at && request_id.is_none_or(|key| n.prompt == key)
                };
                match outcome {
                    CodexApprovalOutcome::Answered(answer) => {
                        let Some(notice) = state.notices.iter_mut().rfind(|n| applies(n)) else {
                            return false;
                        };
                        notice.answer = Some(answer);
                        close_notices(state, applies);
                        true
                    }
                    CodexApprovalOutcome::ResolvedElsewhere => close_notices(state, applies),
                    CodexApprovalOutcome::Dismissed => dismiss_notices(state, applies),
                }
            })?;
        }
        Ok(())
    }

    fn edit_approvals(
        &self,
        host_id: &EventId,
        edit: impl FnOnce(&mut HostApprovalState) -> bool,
    ) -> Result<(), StoreError> {
        let marker = host::approval_candidate(&Paths::new(&self.home), host_id);
        // Change the marker in the same order as the serialized state edit.
        // A stray marker only costs an unnecessary IPC until reconciliation.
        let mut opened_or_closed = false;
        let changed = self.store.edit_hosted_approvals(host_id, |state| {
            let before = open_prompts(state);
            let changed = edit(state);
            opened_or_closed = open_prompts(state) != before;
            if state.claude.is_some() {
                if !marker.exists() {
                    let _ = std::fs::write(&marker, []);
                }
            } else if marker.exists() {
                let _ = std::fs::remove_file(&marker);
            }
            changed
        })?;
        if changed {
            self.hosted_approvals.wake.notify_one();
        }
        if opened_or_closed {
            self.publish(WatchEvent::Sessions);
        }
        Ok(())
    }

    /// The Claude permission request a hosted session's hook reported and
    /// no result or typing has settled: its tool and fingerprint.
    pub(super) fn claude_candidate(
        &self,
        host_id: &EventId,
        session_id: &str,
    ) -> Result<Option<ClaudeCandidate>, StoreError> {
        Ok(self
            .store
            .hosted_approval_states()?
            .into_iter()
            .find(|(id, state)| id == host_id && state.session_id == session_id)
            .and_then(|(_, state)| state.claude))
    }

    /// A person answered the prompt from outside the terminal
    /// ([`super::remote_approve`]): close its notice now, and forget the
    /// Claude request answered, since a denial brings no result hook.
    pub(super) fn settle_claude_answer(
        &self,
        host_id: &EventId,
        answered: &ClaudeCandidate,
        decision: crate::approvals::Decision,
        actor: String,
    ) -> Result<(), StoreError> {
        self.edit_approvals(host_id, |state| {
            let forgotten = state
                .claude
                .take_if(|candidate| {
                    candidate.at == answered.at && candidate.fingerprint == answered.fingerprint
                })
                .is_some();
            let next_request = state.claude.as_ref().map(|candidate| candidate.at);
            let applies = |n: &ApprovalNotice| {
                !n.screen
                    && (n.archive.is_none() || n.at >= answered.at)
                    && next_request.is_none_or(|next| n.at < next)
                    && (n.prompt == answered.fingerprint || n.prompt == "claude:unspecified")
            };
            let mut recorded = false;
            for notice in state.notices.iter_mut().filter(|n| applies(n)) {
                notice.answer = Some(PromptDecision {
                    decision,
                    actor: Some(actor.clone()),
                });
                recorded = true;
            }
            close_notices(state, applies) || recorded || forgotten
        })
    }

    /// A negative screen check or a turn boundary settles only observations
    /// that were already present when the check or hook began.
    pub(super) fn dismiss_claude(
        &self,
        id: &EventId,
        session_id: &str,
        through: u64,
    ) -> Result<(), StoreError> {
        self.edit_approvals(id, |state| {
            if state.session_id != session_id {
                return false;
            }
            let candidate = state.claude.take_if(|c| c.at <= through).is_some();
            dismiss_notices(state, |n| !n.screen && n.at <= through) || candidate
        })
    }

    /// A parallel tool can finish while a dialog is up. Stop is different:
    /// the turn itself ended, even if the tool's own result never came.
    pub(super) async fn on_claude_turn_end(&self, session_id: &str) -> Result<(), StoreError> {
        let at = observed_at();
        let _binding = self.session_bindings.read().await;
        let Some((id, session)) = self.approval_host(session_id)? else {
            return Ok(());
        };
        if session.runtime != Runtime::Claude {
            return Ok(());
        }
        self.dismiss_claude(&id, session_id, at)
    }

    /// Each host's oldest open notice, dated on the wall clock.
    pub(super) fn hosted_prompts(&self) -> BTreeMap<EventId, HostedPrompt> {
        let observed = observed_at();
        let now = chrono::Utc::now();
        self.store
            .hosted_approval_states()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|(id, state)| {
                let notice = state
                    .notices
                    .iter()
                    .filter(|n| !n.unseated && n.archive.is_none())
                    .min_by_key(|n| n.at)?;
                let ago = Duration::from_nanos(observed.saturating_sub(notice.at));
                Some((
                    id,
                    HostedPrompt {
                        tool: notice.tool.as_deref().map(tool_label),
                        since: now - chrono::Duration::from_std(ago).unwrap_or_default(),
                        screen: notice.screen,
                    },
                ))
            })
            .collect()
    }

    async fn reconcile_approval_hosts(&self) -> Result<(), StoreError> {
        let _binding = self.session_bindings.read().await;
        let paths = Paths::new(&self.home);
        let mut claude_checks = Vec::new();
        for (id, state) in self.store.hosted_approval_states()? {
            let through = observed_at().saturating_sub(CLAUDE_DIALOG_GRACE_NS);
            let check_claude = state.claude.as_ref().is_some_and(|c| c.at <= through)
                || state.notices.iter().any(|n| {
                    n.runtime == Runtime::Claude
                        && !n.screen
                        && n.archive.is_none()
                        && n.at <= through
                });
            let native = state.session_id;
            let current = match self.store.session_for_host(&id)? {
                Some(session_id)
                    if self
                        .approval_host(&session_id)?
                        .is_some_and(|(live, _)| live == id) =>
                {
                    Some(session_id)
                }
                _ => None,
            };
            let viewer = host::viewer_activity(&paths, &id);
            self.edit_approvals(&id, |state| {
                if state.session_id != native {
                    return false;
                }
                if let Some(current) = &current {
                    let moved = set_context(state, current);
                    let attended = viewer
                        .as_ref()
                        .is_some_and(|viewer| observe_input(state, viewer.input_at));
                    return moved || attended;
                }
                let had_candidate = state.claude.take().is_some();
                dismiss_notices(state, |_| true) || had_candidate
            })?;
            if check_claude && current.as_deref() == Some(native.as_str()) {
                let paths = &paths;
                claude_checks.push(async move {
                    (
                        host::control::permission_shown(paths, &id).await,
                        id,
                        native,
                        through,
                    )
                });
            }
        }
        for (shown, id, native, through) in futures::future::join_all(claude_checks).await {
            if shown == Some(false) {
                self.dismiss_claude(&id, &native, through)?;
            }
        }
        // Take the relay's live snapshot even if the native request arrived
        // before the host was published. A disconnected relay supplies no
        // positive pending evidence for an unattended notice.
        for prompt in self.relay.hosted_prompts() {
            let Some((id, session)) = self.approval_host(&prompt.session_id)? else {
                continue;
            };
            if session.runtime != Runtime::Codex {
                continue;
            }
            let Some(viewer) = host::viewer_activity(&paths, &id) else {
                continue;
            };
            let Some(unattended_since) = viewer.unattended_since(observed_at()) else {
                continue;
            };
            let at = prompt.at.max(unattended_since);
            if at > viewer.input_at {
                self.edit_approvals(&id, |state| {
                    open_notice(state, &session, at, prompt.request_id, Some(prompt.tool))
                })?;
            }
        }
        self.reconcile_screen_prompts(&paths)
    }

    /// A choice each host sees waiting on its screen ([`host::prompt`]),
    /// as a notice like an approval's, while nobody attends the terminal
    /// and no permission prompt Senate knows of explains it.
    fn reconcile_screen_prompts(&self, paths: &Paths) -> Result<(), StoreError> {
        let relayed = self.relay.hosted_prompts();
        for id in host::running_ids(paths)? {
            let Some(native) = self.store.session_for_host(&id)? else {
                continue;
            };
            let Some((live, session)) = self.approval_host(&native)? else {
                continue;
            };
            if live != id {
                continue;
            }
            let shown = host::prompt::read(paths, &id);
            let native_approval = relayed.iter().any(|prompt| prompt.session_id == native);
            let viewer = host::viewer_activity(paths, &id);
            self.edit_approvals(&id, |state| {
                let key = shown.map(|prompt| format!("screen:{}", prompt.since));
                // Senate's own approval of what is on the screen comes first.
                let approval = native_approval
                    || state.claude.is_some()
                    || state
                        .notices
                        .iter()
                        .any(|n| !n.screen && n.archive.is_none());
                let screen_ended =
                    |n: &ApprovalNotice| n.screen && (approval || key.as_ref() != Some(&n.prompt));
                let unattended = |n: &ApprovalNotice| {
                    viewer.as_ref().is_some_and(|viewer| viewer.input_at < n.at)
                };
                let dismissed =
                    dismiss_notices(state, |n| screen_ended(n) && !approval && unattended(n));
                let mut changed = close_notices(state, screen_ended) || dismissed;
                if approval {
                    return changed;
                }
                let (Some(prompt), Some(key), Some(viewer)) = (shown, key, viewer.as_ref()) else {
                    return changed;
                };
                let Some(unattended) = viewer.unattended_since(observed_at()) else {
                    return changed;
                };
                // Quiet since before someone last typed: they saw it, and
                // what they typed may not have reached the screen yet.
                let at = prompt.since.max(unattended);
                if prompt.since > viewer.input_at && open_notice(state, &session, at, key, None) {
                    if let Some(notice) = state.notices.last_mut() {
                        notice.screen = true;
                    }
                    changed = true;
                }
                changed
            })?;
        }
        Ok(())
    }

    async fn approval_recipient(&self, notice: &ApprovalNotice) -> Result<Address, ApiClientError> {
        let (chair, docket) = tokio::try_join!(
            self.api.chair(&notice.committee),
            self.api.docket(&notice.committee),
        )?;
        let connected = chair.chair.is_some_and(|chair| {
            chair != notice.agent
                && docket
                    .seats
                    .iter()
                    .any(|seat| seat.agent == chair && seat.connected)
        });
        Ok(if connected {
            Address::Chair
        } else if docket
            .office
            .as_ref()
            .is_some_and(|office| office.works_desk(&notice.agent))
            || docket.speaker.as_ref().and_then(|seat| seat.senator()) == Some(&notice.agent)
        {
            Address::senior(&self.config.person)
        } else {
            Address::Speaker
        })
    }

    fn can_append_approval(
        &self,
        host_id: &EventId,
        native: &str,
        notice: &ApprovalNotice,
    ) -> bool {
        if notice.relay_thread.is_some() && notice.runtime == Runtime::Codex {
            return true;
        }
        if notice.archive.is_some() {
            // Recover the receipt of an append that may already exist, so
            // its archive can name the real seq.
            return notice.append_attempted;
        }
        let unattended =
            host::viewer_activity(&Paths::new(&self.home), host_id).is_some_and(|viewer| {
                viewer.unattended_since(observed_at()).is_some() && viewer.input_at < notice.at
            });
        unattended
            && self
                .approval_host(native)
                .ok()
                .flatten()
                .is_some_and(|(id, _)| &id == host_id)
            && (notice.screen
                || notice.runtime != Runtime::Codex
                || self
                    .relay
                    .hosted_prompts()
                    .iter()
                    .any(|prompt| prompt.request_id == notice.prompt))
    }

    async fn opened_codex_prompt(
        &self,
        prompt: crate::host_approval::CodexPrompt,
    ) -> Result<(), PostError> {
        let Some((host, session)) = self.approval_host(&prompt.session_id)? else {
            return Ok(());
        };
        let Some(committee) = session.joined_committee.as_ref() else {
            return Ok(());
        };
        if session.runtime != Runtime::Codex {
            return Ok(());
        }
        if let Some(thread) = self
            .approval_thread(&prompt.session_id, &session.agent, committee)
            .await?
        {
            // A relay's requester needs this update even with someone at the terminal
            // or when a remote decision arrives between two snapshots.
            self.edit_approvals(&host, |state| {
                let key = prompt.request_id;
                open_notice(state, &session, prompt.at, key.clone(), Some(prompt.tool));
                if let Some(n) = state
                    .notices
                    .iter_mut()
                    .rev()
                    .find(|n| n.prompt == key && !n.unseated && !n.append_attempted)
                {
                    n.relay_thread = Some(thread);
                    return true;
                }
                false
            })?;
        } else {
            self.reconcile_approval_hosts().await?;
        }
        Ok(())
    }

    // A status in the waiting session identifies its current task. A sole
    // incoming relay is a fallback for sessions that have not reported status.
    async fn approval_thread(
        &self,
        native: &str,
        agent: &SenatorName,
        committee: &CommitteeId,
    ) -> Result<Option<Seq>, PostError> {
        let mut roots: BTreeMap<_, _> = self
            .store
            .delivered_relays(native, committee, false)?
            .into_iter()
            .map(|item| (item.event.seq, (item.event, false)))
            .collect();
        for task in self.api.threads(committee, Some(agent)).await?.threads {
            if !matches!(task.kind, Kind::Question | Kind::Request) {
                continue;
            }
            if task.waits().is_none()
                || task.takeover.is_some()
                || task
                    .owner
                    .as_ref()
                    .is_some_and(|o| o.session != native || &o.agent != agent)
            {
                roots.remove(&task.root);
                continue;
            }
            match roots.entry(task.root) {
                std::collections::btree_map::Entry::Occupied(mut slot) => slot.get_mut().1 = true,
                std::collections::btree_map::Entry::Vacant(slot) => {
                    let root = self.fetch_event(committee, task.root).await?;
                    slot.insert((root, true));
                }
            }
        }

        let mut latest_status = None;
        let mut open = Vec::new();
        for (seq, (root, listed_open)) in roots {
            let at_desk = root.to.contains(&Address::Speaker);
            let (_, events) = self.thread_events(committee, root.clone()).await?;
            if events.iter().any(|e| {
                // A route creates a new root. Status on that new root belongs
                // to its recipient, never to the original desk item.
                let routed = at_desk
                    && e.from == Address::Speaker
                    && e.thread.is_none()
                    && e.meta.get(meta::RELAYED_FROM).and_then(|v| v.as_u64()) == Some(seq.0);
                if e.thread != Some(seq) {
                    return routed;
                }
                (!listed_open && e.kind == Kind::Answer && !is_provisional(e))
                    || e.meta
                        .get(meta::FORWARDED_TO)
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<Locator>().ok())
                        .is_some_and(|target| {
                            target.address() != &Address::Speaker
                                || target.senate().is_some_and(|s| {
                                    self.senate_path().is_none_or(|here| !here.ends_with(s))
                                })
                                || (e.from == Address::Speaker
                                    && e.meta.get(meta::ACTED_BY).and_then(|v| v.as_str())
                                        == Some(agent.as_str()))
                        })
                    || StatusPayload::of(e)
                        .is_some_and(|s| s.state == StatusState::Archived || s.takeover.is_some())
            }) {
                continue;
            }
            for event in events {
                if event.thread == Some(seq)
                    && event.from.as_senator() == Some(agent)
                    && StatusPayload::of(&event)
                        .is_some_and(|s| s.session.as_deref() == Some(native))
                {
                    latest_status = latest_status.max(Some((event.seq, seq)));
                }
            }
            open.push(root);
        }
        if let Some((_, root)) = latest_status {
            return Ok(Some(root));
        }

        let mut fallback = None;
        for root in open {
            if !matches!(root.from, Address::Speaker | Address::Committee) {
                continue;
            }
            let incoming = if senate_core::Origin::of(&root).is_none()
                && root.from == Address::Speaker
                && let Some(seq) = root.meta.get(meta::RELAYED_FROM).and_then(|v| v.as_u64())
                && seq > 0
                && seq < root.seq.0
            {
                Some(self.fetch_event(committee, Seq(seq)).await?)
            } else {
                None
            };
            if senate_core::Origin::of(&root).is_some()
                || incoming.as_ref().is_some_and(|e| {
                    e.thread.is_none()
                        && e.from == Address::Committee
                        && matches!(e.kind, Kind::Question | Kind::Request)
                        && senate_core::Origin::of(e).is_some()
                })
            {
                if fallback.is_some() {
                    return Ok(None);
                }
                fallback = Some(root.seq);
            }
        }
        Ok(fallback)
    }

    async fn send_approval_notice(
        &self,
        host_id: &EventId,
        native: &str,
        notice: &ApprovalNotice,
    ) -> Result<(), PostError> {
        if self
            .api
            .committee_seat_status(&notice.committee, &notice.agent)
            .await?
            .unseated
        {
            self.edit_approvals(host_id, |state| {
                if notice.archive.is_some() {
                    state.notices.retain(|n| n.id != notice.id);
                    return true;
                }
                update_notice(state, &notice.id, |n| {
                    n.unseated = true;
                    n.relay_thread = None;
                    n.append_attempted = false;
                })
            })?;
            return Ok(());
        }
        let sender = Address::senator(&notice.agent);
        match (notice.seq, notice.archive.as_ref()) {
            (None, _) => {
                if !self.can_append_approval(host_id, native, notice) {
                    return Ok(());
                }
                let recipient = match &notice.recipient {
                    Some(address) => address.clone(),
                    None => self.approval_recipient(notice).await?,
                };
                let relay_thread = if notice.append_attempted || notice.relay_thread.is_some() {
                    notice.relay_thread
                } else {
                    self.approval_thread(native, &notice.agent, &notice.committee)
                        .await?
                };
                // While looking up the recipient the viewer or a Claude
                // result may have handled this before its first append.
                if !self.can_append_approval(host_id, native, notice) {
                    return Ok(());
                }
                let started = self.store.edit_hosted_approvals(host_id, |state| {
                    let Some(current) = state.notices.iter_mut().find(|n| n.id == notice.id) else {
                        return false;
                    };
                    if current.archive.is_some()
                        && !current.append_attempted
                        && current.relay_thread.is_none()
                    {
                        return false;
                    }
                    current.recipient = Some(recipient.clone());
                    current.relay_thread = relay_thread;
                    current.append_attempted = true;
                    true
                })?;
                if !started {
                    return Ok(());
                }
                let mut event = NewEvent::new(
                    notice.committee.clone(),
                    Kind::Question,
                    sender,
                    approval_body(notice, self.locale),
                )
                .with_id(notice.id.clone())
                .to(recipient)
                .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(true));
                if let Some(thread) = relay_thread {
                    let label =
                        notice.tool.as_deref().map(tool_label).unwrap_or_else(|| {
                            self.locale.pick("终端提示", "terminal prompt").into()
                        });
                    event = event.in_thread(thread).with_meta(
                        meta::APPROVAL_WAIT,
                        serde_json::json!({"thread": thread, "label":label}),
                    );
                }
                let posted = self.api.append(&event).await?;
                tracing::info!(notice = %notice.id, committee = %notice.committee, seq = %posted.event.seq, ?relay_thread, "posted hosted approval notice");
                self.edit_approvals(host_id, |state| {
                    update_notice(state, &notice.id, |n| {
                        n.seq = Some(posted.event.seq);
                        n.failures = 0;
                    })
                })?;
            }
            (Some(seq), Some(id)) => {
                let mut status = StatusPayload::new(StatusState::Archived);
                if notice.dismissed && notice.answer.is_none() {
                    status.reason = Some(
                        self.locale
                            .pick(
                                "终端提示已消失，未收到答复。",
                                "The terminal prompt went away unanswered.",
                            )
                            .into(),
                    );
                }
                let mut event = Self::status_event(
                    notice.committee.clone(),
                    sender,
                    seq,
                    &status,
                    Some(id.clone()),
                )
                .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(notice.id));
                if let Some(reason) = status.reason {
                    event.body = reason;
                }
                if notice.relay_thread.is_some() {
                    if let Some(answer) = &notice.answer {
                        event = event.with_meta(meta::APPROVAL_WAIT, serde_json::json!(answer));
                    } else if notice.dismissed {
                        event = event
                            .with_meta(meta::APPROVAL_WAIT, serde_json::json!({"dismissed":true}));
                    }
                }
                self.api.append(&event).await?;
                self.store.edit_hosted_approvals(host_id, |state| {
                    let count = state.notices.len();
                    state.notices.retain(|n| n.id != notice.id);
                    state.notices.len() != count
                })?;
            }
            _ => {}
        }
        Ok(())
    }

    fn reject_approval_post(&self, host_id: &EventId, notice: &ApprovalNotice) {
        let updated = self.store.edit_hosted_approvals(host_id, |state| {
            let Some(current) = state.notices.iter_mut().find(|n| n.id == notice.id) else {
                return false;
            };
            if current.seq != notice.seq || current.archive != notice.archive {
                return false;
            }
            current.failures = current.failures.saturating_add(1);
            if current.failures == POST_FAILURE_LIMIT {
                tracing::warn!(event = %notice.id, committee = %notice.committee, "hosted approval notice reached its retry limit");
            }
            true
        });
        if let Err(error) = updated {
            tracing::warn!(%error, "saving hosted approval retry progress");
        }
    }
}

fn approval_body(notice: &ApprovalNotice, locale: Locale) -> String {
    let committee = &notice.committee;
    let attach = format!("senate attach {}", notice.agent);
    let who = Address::senator(&notice.agent).in_text();
    if notice.screen {
        return match locale {
            Locale::En => format!(
                "{who} is waiting on a prompt in its terminal in committee \u{0060}{committee}\u{0060} that Senate does not answer (its screen shows a choice and has not changed for a while).\nAsk a person to run \u{0060}{attach}\u{0060} and choose in the terminal."
            ),
            Locale::Zh => format!(
                "{who} 在委员会 \u{0060}{committee}\u{0060} 的终端里停在一个 Senate 不会回答的提示上（屏幕显示一个选择，已有一段时间没有变化）。\n请让人员运行 \u{0060}{attach}\u{0060}，在终端内选择。"
            ),
        };
    }
    let tool = notice.tool.as_ref().map_or_else(String::new, |tool| {
        let quoted = serde_json::json!(tool_label(tool));
        match locale {
            Locale::En => format!(" Tool or command {quoted}."),
            Locale::Zh => format!(" 工具或命令 {quoted}。"),
        }
    });
    match locale {
        Locale::En => format!(
            "{who} has a permission prompt waiting in committee \u{0060}{committee}\u{0060}.{tool}\nAsk a person to run \u{0060}{attach}\u{0060} and decide in the terminal."
        ),
        Locale::Zh => format!(
            "{who} 在委员会 \u{0060}{committee}\u{0060} 等待工具授权。{tool}\n请让人员运行 \u{0060}{attach}\u{0060}，在终端内决定。"
        ),
    }
}

/// The notices still waiting for a person, by id.
fn open_prompts(state: &HostApprovalState) -> Vec<EventId> {
    state
        .notices
        .iter()
        .filter(|n| !n.unseated && n.archive.is_none())
        .map(|n| n.id.clone())
        .collect()
}

fn set_context(state: &mut HostApprovalState, session_id: &str) -> bool {
    if state.session_id == session_id {
        return false;
    }
    state.session_id = session_id.to_owned();
    state.claude = None;
    dismiss_notices(state, |_| true);
    true
}

fn observe_input(state: &mut HostApprovalState, at: u64) -> bool {
    let candidate = state
        .claude
        .as_ref()
        .is_some_and(|candidate| candidate.at <= at);
    if candidate {
        state.claude = None;
    }
    close_notices(state, |notice| {
        // A relayed Codex wait belongs to the task's requester. Typing
        // shows the terminal is attended but says nothing about its native
        // approval outcome.
        notice.at <= at
            && !notice.screen
            && (notice.runtime != Runtime::Codex || notice.relay_thread.is_none())
    }) || candidate
}

#[cfg(test)]
#[test]
fn typing_keeps_a_relayed_codex_wait_open_for_its_native_outcome() -> anyhow::Result<()> {
    let ordinary = ApprovalNotice {
        id: EventId::new(),
        prompt: "ordinary".into(),
        at: 100,
        tool: None,
        runtime: Runtime::Codex,
        agent: "codex-test".parse()?,
        committee: CommitteeId::new("fixture")?,
        recipient: None,
        seq: Some(Seq(1)),
        relay_thread: None,
        unseated: false,
        archive: None,
        answer: None,
        dismissed: false,
        append_attempted: true,
        failures: 0,
        screen: false,
    };
    let relayed = ApprovalNotice {
        id: EventId::new(),
        prompt: "relayed".into(),
        seq: Some(Seq(3)),
        relay_thread: Some(Seq(2)),
        ..ordinary.clone()
    };
    let screen = ApprovalNotice {
        id: EventId::new(),
        prompt: "screen".into(),
        seq: Some(Seq(4)),
        screen: true,
        ..relayed.clone()
    };
    let mut state = HostApprovalState {
        notices: vec![ordinary, relayed, screen],
        ..HostApprovalState::default()
    };

    assert!(observe_input(&mut state, 101));
    assert!(state.notices[0].archive.is_some());
    assert!(state.notices[1].archive.is_none());
    assert!(state.notices[2].archive.is_none());
    assert!(close_notices(&mut state, |notice| notice.prompt == "relayed"));
    assert!(state.notices[1].archive.is_some());
    Ok(())
}

fn open_notice(
    state: &mut HostApprovalState,
    session: &Session,
    at: u64,
    prompt: String,
    tool: Option<String>,
) -> bool {
    let Some(committee) = session.joined_committee.as_ref() else {
        return false;
    };
    if state
        .notices
        .iter()
        .any(|notice| notice.prompt == prompt && (notice.archive.is_none() || notice.at >= at))
    {
        return false;
    }
    set_context(state, &session.session_id);
    state.notices.push(ApprovalNotice {
        id: EventId::new(),
        prompt,
        at,
        tool,
        runtime: session.runtime,
        agent: session.agent.clone(),
        committee: committee.clone(),
        recipient: None,
        seq: None,
        relay_thread: None,
        unseated: false,
        archive: None,
        answer: None,
        dismissed: false,
        append_attempted: false,
        failures: 0,
        screen: false,
    });
    true
}

fn close_notices(state: &mut HostApprovalState, applies: impl Fn(&ApprovalNotice) -> bool) -> bool {
    settle_notices(state, false, applies)
}

fn dismiss_notices(
    state: &mut HostApprovalState,
    applies: impl Fn(&ApprovalNotice) -> bool,
) -> bool {
    settle_notices(state, true, applies)
}

fn settle_notices(
    state: &mut HostApprovalState,
    dismissed: bool,
    applies: impl Fn(&ApprovalNotice) -> bool,
) -> bool {
    let mut changed = false;
    for notice in &mut state.notices {
        if notice.archive.is_none() && applies(notice) {
            notice.archive = Some(EventId::new());
            notice.dismissed = dismissed;
            notice.failures = 0;
            changed = true;
        }
    }
    state.notices.retain(|n| {
        n.archive.is_none() || n.append_attempted || n.seq.is_some() || n.relay_thread.is_some()
    });
    changed
}

fn update_notice(
    state: &mut HostApprovalState,
    id: &EventId,
    change: impl FnOnce(&mut ApprovalNotice),
) -> bool {
    let Some(notice) = state.notices.iter_mut().find(|notice| &notice.id == id) else {
        return false;
    };
    change(notice);
    true
}

pub(super) async fn observe(
    shared: Arc<Shared>,
    mut signals: mpsc::UnboundedReceiver<CodexApprovalSignal>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut retry = tokio::time::interval(shared.timing.sweep.min(RETRY));
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut backoff: BTreeMap<EventId, PostBackoff> = BTreeMap::new();
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            Some(signal) = signals.recv() => {
                match signal {
                    CodexApprovalSignal::Opened(prompt) => {
                        if let Err(error) = shared.opened_codex_prompt(prompt).await {
                            tracing::warn!(%error, "recording Codex approval request");
                        }
                    }
                    CodexApprovalSignal::Resolved { session_id, request_id, outcome, at } => {
                        if let Err(error) = shared.observe_codex_resolution(&session_id, request_id.as_deref(), outcome, at) {
                            tracing::warn!(%error, "recording Codex approval resolution");
                        }
                        continue;
                    }
                }
            }
            _ = retry.tick() => {
                if let Err(error) = shared.reconcile_approval_hosts().await {
                    tracing::warn!(%error, "checking hosted approvals");
                }
            }
            _ = shared.hosted_approvals.wake.notified() => {}
        }
        let snapshot = {
            let _settled = shared.hosted_approvals.claude_answer.write().await;
            shared.store.hosted_approval_states()
        };
        let states = match snapshot {
            Ok(states) => states,
            Err(error) => {
                tracing::warn!(%error, "reading hosted approvals");
                continue;
            }
        };
        let mut pending = BTreeSet::new();
        for (host, state) in states {
            for notice in state.notices.into_iter().filter(|n| {
                !n.unseated
                    && n.failures < POST_FAILURE_LIMIT
                    && (n.seq.is_none() || n.archive.is_some())
            }) {
                let operation = notice
                    .seq
                    .and(notice.archive.as_ref())
                    .unwrap_or(&notice.id);
                pending.insert(operation.clone());
                if backoff
                    .get(operation)
                    .is_some_and(|retry| retry.ready_at > Instant::now())
                {
                    continue;
                }
                let result = tokio::select! {
                    _ = shutdown.changed() => return,
                    result = tokio::time::timeout(POST_TIMEOUT, shared.send_approval_notice(&host, &state.session_id, &notice)) => result,
                };
                match result {
                    Ok(Ok(())) => {
                        backoff.remove(operation);
                        continue;
                    }
                    Ok(Err(error)) => {
                        tracing::debug!(%error, "posting hosted approval notice");
                        if matches!(&error, PostError::Server(server) if server.definitive_notice_rejection())
                        {
                            tracing::warn!(%error, notice = %notice.id, committee = %notice.committee, "server refused hosted approval notice");
                            shared.reject_approval_post(&host, &notice);
                        }
                    }
                    Err(_) => {
                        tracing::debug!("timed out posting hosted approval notice");
                    }
                }
                let failures = backoff
                    .get(operation)
                    .map_or(1, |retry| retry.failures.saturating_add(1));
                backoff.insert(
                    operation.clone(),
                    PostBackoff {
                        failures,
                        ready_at: Instant::now() + retry_delay(&shared.timing, failures),
                    },
                );
            }
        }
        backoff.retain(|id, _| pending.contains(id));
    }
}
