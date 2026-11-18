//! Watches hosted sessions for a request to attach and attend to a prompt.
//! State is local, and every notice and archive is a normal committee event.

use super::*;
use crate::host;
use crate::host_approval::{
    ApprovalNotice, ClaudeCandidate, CodexApprovalSignal, HostApprovalState, observed_at,
};
use crate::ipc::ClaudeApprovalSignal;
use senate_core::StatusState;
use tokio::sync::mpsc;

const RETRY: Duration = Duration::from_secs(1);
const POST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
pub(super) struct Monitor {
    inputs: Mutex<BTreeMap<EventId, u64>>,
    wake: Notify,
}

impl Shared {
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
            .filter(Session::is_live)
            .map(|session| (id, session)))
    }

    pub(super) fn on_hosted_viewer_input(
        &self,
        session: &str,
        host_id: &EventId,
        at: u64,
    ) -> Result<serde_json::Value, IpcError> {
        if (1..=observed_at()).contains(&at)
            && self
                .approval_host(session)
                .map_err(internal)?
                .is_some_and(|(id, _)| id == *host_id)
        {
            let mut inputs = lock(&self.hosted_approvals.inputs);
            let last = inputs.entry(host_id.clone()).or_default();
            *last = (*last).max(at);
            self.edit_approvals(host_id, |state| {
                close_notices(state, |notice| notice.at <= *last)
            })
            .map_err(internal)?;
        }
        Ok(serde_json::json!({}))
    }

    pub(super) fn on_hosted_claude_approval(
        &self,
        session_id: &str,
        host_id: &EventId,
        at: u64,
        signal: ClaudeApprovalSignal,
    ) -> Result<serde_json::Value, IpcError> {
        if (1..=observed_at()).contains(&at)
            && let Some((id, session)) = self.approval_host(session_id).map_err(internal)?
            && id == *host_id
            && session.runtime == Runtime::Claude
        {
            let inputs = lock(&self.hosted_approvals.inputs);
            let last_input = inputs.get(host_id).copied().unwrap_or(0);
            self.edit_approvals(host_id, |state| match signal {
                ClaudeApprovalSignal::Requested { tool, fingerprint } => {
                    if state.claude.as_ref().is_some_and(|pending| pending.at > at) {
                        return false;
                    }
                    state.session_id = session_id.to_owned();
                    state.claude = Some(ClaudeCandidate {
                        tool,
                        fingerprint,
                        at,
                    });
                    true
                }
                ClaudeApprovalSignal::Pending => {
                    let Some(candidate) = state.claude.as_ref().filter(|pending| pending.at <= at)
                    else {
                        return false;
                    };
                    let (prompt, tool) = (candidate.fingerprint.clone(), candidate.tool.clone());
                    open_notice(state, &session, at, last_input, prompt, tool)
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
            })
            .map_err(internal)?;
        }
        Ok(serde_json::json!({}))
    }

    fn observe_codex_approval(&self, signal: CodexApprovalSignal) -> Result<(), StoreError> {
        let Some((id, session)) = self.approval_host(&signal.session_id)? else {
            return Ok(());
        };
        if session.runtime != Runtime::Codex {
            return Ok(());
        }
        let inputs = lock(&self.hosted_approvals.inputs);
        let last_input = inputs.get(&id).copied().unwrap_or(0);
        self.edit_approvals(&id, |state| match signal.tool {
            Some(tool) => open_notice(
                state,
                &session,
                signal.at,
                last_input,
                signal.request_id,
                tool,
            ),
            None => close_notices(state, |notice| {
                notice.prompt == signal.request_id && notice.at <= signal.at
            }),
        })
    }

    fn edit_approvals(
        &self,
        host_id: &EventId,
        edit: impl FnOnce(&mut HostApprovalState) -> bool,
    ) -> Result<(), StoreError> {
        if self.store.edit_hosted_approvals(host_id, edit)? {
            self.hosted_approvals.wake.notify_one();
        }
        Ok(())
    }

    fn retire_approval_hosts(&self) -> Result<(), StoreError> {
        {
            let mut inputs = lock(&self.hosted_approvals.inputs);
            if !inputs.is_empty() {
                let alive = host::running_ids(&Paths::new(&self.home))?;
                inputs.retain(|id, _| alive.contains(id));
            }
        }
        for (id, state) in self.store.hosted_approval_states()? {
            if self
                .approval_host(&state.session_id)?
                .is_some_and(|(live, _)| live == id)
            {
                continue;
            }
            self.store.edit_hosted_approvals(&id, |state| {
                let had_candidate = state.claude.take().is_some();
                close_notices(state, |_| true) || had_candidate
            })?;
        }
        Ok(())
    }

    async fn approval_recipient(&self, notice: &ApprovalNotice) -> Result<Address, IpcError> {
        let (chair, docket) = tokio::try_join!(
            self.api.chair(&notice.committee),
            self.api.docket(&notice.committee),
        )
        .map_err(server_error)?;
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

    async fn send_approval_notice(
        &self,
        host_id: &EventId,
        notice: ApprovalNotice,
    ) -> Result<(), IpcError> {
        let sender = Address::senator(&notice.agent);
        match (notice.seq, notice.archive.as_ref()) {
            (None, _) => {
                let recipient = match notice.recipient {
                    Some(address) => address,
                    None => {
                        let address = self.approval_recipient(&notice).await?;
                        let saved = self
                            .store
                            .edit_hosted_approvals(host_id, |state| {
                                update_notice(state, &notice.id, |n| {
                                    n.recipient = Some(address.clone())
                                })
                            })
                            .map_err(internal)?;
                        if !saved {
                            return Ok(());
                        }
                        address
                    }
                };
                let committee = &notice.committee;
                let tool = &notice.tool;
                let attach = format!("senate attach {}", notice.agent);
                let who = sender.in_text();
                let body = match self.locale {
                    Locale::En => format!(
                        "{who} is waiting for tool approval in committee `{committee}`. Tool or command: {tool}\nAsk a person to run `{attach}` and decide in the terminal."
                    ),
                    Locale::Zh => format!(
                        "{who} 在委员会 `{committee}` 等待工具授权。工具或命令：{tool}\n请让人员运行 `{attach}`，在终端内决定。"
                    ),
                };
                let event = NewEvent::new(notice.committee, Kind::Question, sender, body)
                    .with_id(notice.id.clone())
                    .to(recipient)
                    .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(true));
                let posted = self.post(event).await?;
                self.store
                    .edit_hosted_approvals(host_id, |state| {
                        update_notice(state, &notice.id, |n| n.seq = Some(posted.seq))
                    })
                    .map_err(internal)?;
                if notice.archive.is_some() {
                    self.hosted_approvals.wake.notify_one();
                }
            }
            (Some(seq), Some(id)) => {
                let event = Self::status_event(
                    notice.committee,
                    sender,
                    seq,
                    &StatusPayload::new(StatusState::Archived),
                    Some(id.clone()),
                )
                .with_meta(meta::HOSTED_APPROVAL, serde_json::json!(notice.id));
                self.post(event).await?;
                self.store
                    .edit_hosted_approvals(host_id, |state| {
                        state.notices.retain(|n| n.id != notice.id);
                        true
                    })
                    .map_err(internal)?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn open_notice(
    state: &mut HostApprovalState,
    session: &Session,
    at: u64,
    last_input: u64,
    prompt: String,
    tool: String,
) -> bool {
    let Some(committee) = session.committee() else {
        return false;
    };
    // An observation can cross an input report in transit. By the time
    // that fact reaches this worker, the viewer is already attending to it.
    if last_input >= at {
        return false;
    }
    if state
        .notices
        .iter()
        .any(|notice| notice.prompt == prompt && (notice.archive.is_none() || notice.at >= at))
    {
        return false;
    }
    state.session_id.clone_from(&session.session_id);
    state.notices.push(ApprovalNotice {
        id: EventId::new(),
        prompt,
        at,
        tool: crate::host_approval::tool_label(&tool),
        agent: session.agent.clone(),
        committee: committee.clone(),
        recipient: None,
        seq: None,
        archive: None,
    });
    true
}

fn close_notices(state: &mut HostApprovalState, applies: impl Fn(&ApprovalNotice) -> bool) -> bool {
    let mut changed = false;
    for notice in &mut state.notices {
        if notice.archive.is_none() && applies(notice) {
            notice.archive = Some(EventId::new());
            changed = true;
        }
    }
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
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            Some(signal) = signals.recv() => {
                if let Err(error) = shared.observe_codex_approval(signal) {
                    tracing::warn!(%error, "recording Codex approval");
                }
                continue;
            }
            _ = retry.tick() => {
                if let Err(error) = shared.retire_approval_hosts() {
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
        for (host, state) in states {
            for notice in state
                .notices
                .into_iter()
                .filter(|n| n.seq.is_none() || n.archive.is_some())
            {
                let result = tokio::select! {
                    _ = shutdown.changed() => return,
                    result = tokio::time::timeout(POST_TIMEOUT, shared.send_approval_notice(&host, notice)) => result,
                };
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::debug!(%error, "posting hosted approval notice"),
                    Err(_) => tracing::debug!("timed out posting hosted approval notice"),
                }
            }
        }
    }
}
