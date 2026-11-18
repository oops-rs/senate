//! Watches hosted sessions for a request to attach and attend to a prompt.
//! State is local, and every notice and archive is a normal committee event.

use super::*;
use crate::host;
use crate::host_approval::{
    ApprovalNotice, ClaudeCandidate, CodexApprovalSignal, HostApprovalState, observed_at,
    tool_label,
};
use crate::ipc::ClaudeApprovalSignal;
use senate_core::StatusState;
use tokio::sync::mpsc;

const RETRY: Duration = Duration::from_secs(1);
const POST_TIMEOUT: Duration = Duration::from_secs(5);
const POST_FAILURE_LIMIT: u8 = 6;

#[derive(Debug, thiserror::Error)]
enum PostError {
    #[error(transparent)]
    Server(#[from] ApiClientError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

struct PostBackoff {
    failures: u32,
    ready_at: Instant,
}

#[derive(Default)]
pub(super) struct Monitor {
    wake: Notify,
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

    fn approval_host(&self, session: &str) -> Result<Option<(EventId, Session)>, StoreError> {
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
        }
        Ok(serde_json::json!({}))
    }

    fn observe_codex_resolution(&self, signal: CodexApprovalSignal) -> Result<(), StoreError> {
        let Some((id, session)) = self.approval_host(&signal.session_id)? else {
            return Ok(());
        };
        if session.runtime == Runtime::Codex {
            self.edit_approvals(&id, |state| {
                close_notices(state, |notice| {
                    notice.at <= signal.at
                        && signal
                            .request_id
                            .as_ref()
                            .is_none_or(|key| &notice.prompt == key)
                })
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
        let changed = self.store.edit_hosted_approvals(host_id, |state| {
            let changed = edit(state);
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
        Ok(())
    }

    async fn reconcile_approval_hosts(&self) -> Result<(), StoreError> {
        let _binding = self.session_bindings.read().await;
        let paths = Paths::new(&self.home);
        for (id, state) in self.store.hosted_approval_states()? {
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
                close_notices(state, |_| true) || had_candidate
            })?;
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
            && (notice.runtime != Runtime::Codex
                || self
                    .relay
                    .hosted_prompts()
                    .iter()
                    .any(|prompt| prompt.request_id == notice.prompt))
    }

    async fn send_approval_notice(
        &self,
        host_id: &EventId,
        native: &str,
        notice: &ApprovalNotice,
    ) -> Result<(), PostError> {
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
                // While looking up the recipient the viewer or a Claude
                // result may have handled this before its first append.
                if !self.can_append_approval(host_id, native, notice) {
                    return Ok(());
                }
                let started = self.store.edit_hosted_approvals(host_id, |state| {
                    let Some(current) = state.notices.iter_mut().find(|n| n.id == notice.id) else {
                        return false;
                    };
                    if current.archive.is_some() && !current.append_attempted {
                        return false;
                    }
                    current.recipient = Some(recipient.clone());
                    current.append_attempted = true;
                    true
                })?;
                if !started {
                    return Ok(());
                }
                let event = NewEvent::new(
                    notice.committee.clone(),
                    Kind::Question,
                    sender,
                    approval_body(notice, self.locale),
                )
                .with_id(notice.id.clone())
                .to(recipient)
                .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(true));
                let posted = self.api.append(&event).await?;
                self.edit_approvals(host_id, |state| {
                    update_notice(state, &notice.id, |n| {
                        n.seq = Some(posted.event.seq);
                        n.failures = 0;
                    })
                })?;
            }
            (Some(seq), Some(id)) => {
                let event = Self::status_event(
                    notice.committee.clone(),
                    sender,
                    seq,
                    &StatusPayload::new(StatusState::Archived),
                    Some(id.clone()),
                )
                .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(notice.id));
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

fn set_context(state: &mut HostApprovalState, session_id: &str) -> bool {
    if state.session_id == session_id {
        return false;
    }
    state.session_id = session_id.to_owned();
    state.claude = None;
    close_notices(state, |_| true);
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
    close_notices(state, |notice| notice.at <= at) || candidate
}

fn open_notice(
    state: &mut HostApprovalState,
    session: &Session,
    at: u64,
    prompt: String,
    tool: Option<String>,
) -> bool {
    let Some(committee) = session.committee() else {
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
        archive: None,
        append_attempted: false,
        failures: 0,
    });
    true
}

fn close_notices(state: &mut HostApprovalState, applies: impl Fn(&ApprovalNotice) -> bool) -> bool {
    let mut changed = false;
    for notice in &mut state.notices {
        if notice.archive.is_none() && applies(notice) {
            notice.archive = Some(EventId::new());
            notice.failures = 0;
            changed = true;
        }
    }
    state
        .notices
        .retain(|n| n.archive.is_none() || n.append_attempted || n.seq.is_some());
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
                if let Err(error) = shared.observe_codex_resolution(signal) {
                    tracing::warn!(%error, "recording Codex approval resolution");
                }
                continue;
            }
            _ = retry.tick() => {
                if let Err(error) = shared.reconcile_approval_hosts().await {
                    tracing::warn!(%error, "checking hosted approvals");
                }
            }
            _ = shared.hosted_approvals.wake.notified() => {}
        }
        let states = match shared.store.hosted_approval_states() {
            Ok(states) => states,
            Err(error) => {
                tracing::warn!(%error, "reading hosted approvals");
                continue;
            }
        };
        let mut pending = BTreeSet::new();
        for (host, state) in states {
            for notice in state.notices.into_iter().filter(|n| {
                n.failures < POST_FAILURE_LIMIT && (n.seq.is_none() || n.archive.is_some())
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
