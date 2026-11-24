use std::sync::Weak;

use senate_observe::{
    LocalApproval, LocalHistory, LocalSeat, MachineFacts, MachineSource, ProjectionError,
};
use senate_observe_protocol::{ApprovalDecision, ApprovalTool, LinkState, SenatorState};

use super::*;
use crate::ipc::UplinkStatus;

pub(super) struct Source {
    pub(super) shared: Weak<Shared>,
}

impl MachineSource for Source {
    fn read(&self) -> Result<MachineFacts, ProjectionError> {
        let shared = self.shared.upgrade().ok_or(ProjectionError::NotReady)?;
        let paths = Paths::new(&shared.home);
        let mut sessions = shared.store.all_sessions().map_err(unavailable)?;
        sessions.sort_by(|a, b| {
            b.last_seen
                .cmp(&a.last_seen)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        let active: Vec<_> = sessions
            .iter()
            .filter(|s| s.is_live() && !shared.hosts.unpublished(&s.session_id))
            .cloned()
            .collect();
        let host_facts =
            crate::host::status(&paths, &shared.store, &active).map_err(unavailable)?;
        let approvals = approval_facts(&shared, &active, &host_facts)?;
        let mut projects = BTreeMap::new();
        let mut seats: BTreeMap<(CommitteeId, SenatorName), LocalSeat> = BTreeMap::new();
        for session in sessions {
            let Some(committee) = session.committee() else {
                continue;
            };
            if shared.hosts.unpublished(&session.session_id) {
                continue;
            }
            if let Some(root) = &session.git_root {
                projects
                    .entry(committee.clone())
                    .or_insert_with(|| root.clone());
            }
            let key = (committee.clone(), session.agent.clone());
            let hosted = host_facts.hosted.contains_key(&session.session_id);
            let viewer_attached = if hosted {
                shared
                    .store
                    .session_host(&session.session_id)
                    .map_err(unavailable)?
                    .and_then(|id| crate::host::viewer_activity(&paths, &id))
                    .map(|viewer| viewer.attached)
            } else {
                None
            };
            let state = match session.state {
                SessionState::Live if shared.session_working(&session) => SenatorState::Working,
                SessionState::Live => SenatorState::Idle,
                SessionState::Ended => SenatorState::Stopped,
                SessionState::Stale => SenatorState::Away,
            };
            let history = if !seats.contains_key(&key)
                && matches!(session.state, SessionState::Ended | SessionState::Stale)
            {
                Some(LocalHistory {
                    native_id: session.session_id.clone(),
                    alias: shared
                        .store
                        .history_name(&session.session_id)
                        .map_err(unavailable)?
                        .and_then(|(alias, _)| alias)
                        .map(|alias| alias.to_string()),
                })
            } else {
                None
            };
            seats
                .entry(key)
                .and_modify(|seat| {
                    if rank(state) > rank(seat.state) {
                        seat.state = state;
                        seat.runtime = Some(session.runtime.as_str().to_owned());
                    }
                    seat.hosted |= hosted;
                    seat.viewer_attached = seat.viewer_attached.max(viewer_attached);
                })
                .or_insert_with(|| LocalSeat {
                    runtime: Some(session.runtime.as_str().to_owned()),
                    state,
                    last_active_at: Some(session.last_seen),
                    hosted,
                    viewer_attached,
                    history,
                });
        }
        for pause in shared.store.all_paused().map_err(unavailable)? {
            let seat = seats
                .entry((pause.committee, pause.agent))
                .or_insert(LocalSeat {
                    runtime: None,
                    state: SenatorState::Paused,
                    last_active_at: None,
                    hosted: false,
                    viewer_attached: None,
                    history: None,
                });
            seat.state = SenatorState::Paused;
        }
        let parent_state = std::fs::read(paths.hosted("top").join(UplinkStatus::FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<UplinkStatus>(&bytes).ok())
            .filter(|status| chrono::Utc::now() - status.at < chrono::Duration::seconds(60))
            .map_or(LinkState::Unknown, |status| match status.registration {
                crate::ipc::Registration::Pending => LinkState::Unknown,
                crate::ipc::Registration::Registered => LinkState::Up,
                crate::ipc::Registration::Refused { .. }
                | crate::ipc::Registration::Unreachable { .. } => LinkState::Down,
            });
        Ok(MachineFacts {
            person: shared.config.person.to_string(),
            seats,
            projects,
            paired_machines: crate::remote::names(&paths)
                .into_iter()
                .map(|name| name.to_string())
                .collect(),
            parent_state,
            approvals,
        })
    }
}

fn approval_facts(
    shared: &Shared,
    active: &[Session],
    hosts: &crate::host::HostStatus,
) -> Result<Vec<LocalApproval>, ProjectionError> {
    let bindings: BTreeMap<_, _> = active
        .iter()
        .map(|session| (session.session_id.as_str(), session))
        .collect();
    let mut approvals = Vec::new();
    for (id, session_id, kind, asked_at) in shared.relay.observe_inputs() {
        if let Some(session) = bindings.get(session_id.as_str())
            && let Some(committee) = session.committee()
        {
            approvals.push(LocalApproval {
                native_id: id,
                committee: committee.clone(),
                senator: session.agent.clone(),
                kind: approval_tool(kind),
                asked_at,
            });
        }
    }
    for (host_id, state) in shared.store.hosted_approval_states().map_err(unavailable)? {
        if let Some(candidate) = state.claude
            && let Some(session) = bindings.get(state.session_id.as_str())
            && let Some(committee) = session.committee()
            && hosts.hosted.contains_key(&state.session_id)
            && let Ok(at) = i64::try_from(candidate.at)
        {
            approvals.push(LocalApproval {
                native_id: claude_id(&host_id, &candidate),
                committee: committee.clone(),
                senator: session.agent.clone(),
                kind: claude_tool(&candidate.tool),
                asked_at: chrono::DateTime::from_timestamp_nanos(at),
            });
        }
    }
    Ok(approvals)
}

fn rank(state: SenatorState) -> u8 {
    match state {
        SenatorState::Working => 4,
        SenatorState::Idle => 3,
        SenatorState::Stopped => 2,
        SenatorState::Away => 1,
        _ => 0,
    }
}

fn unavailable(_: StoreError) -> ProjectionError {
    ProjectionError::NotReady
}

impl Front {
    pub(super) async fn set_observer_card(
        &self,
        selector: &SessionSelector,
        target: crate::ipc::CardTarget,
        document: &str,
    ) -> Result<(), IpcError> {
        use crate::ipc::CardTarget;
        use senate_core::alias::KnownAliases;
        use senate_observe::CardKey;

        let registry = self
            .observe
            .as_ref()
            .ok_or_else(|| IpcError::bad_request("Cards require a machine-hosted Senate"))?;
        let shared = self.primary();
        let local_person = selector == &SessionSelector::default();
        let session = if local_person {
            None
        } else {
            Some(shared.resolve(selector)?)
        };
        let writer = match &session {
            Some(session) => session.agent.clone(),
            None => SenatorName::new(shared.config.person.clone())
                .map_err(|error| IpcError::bad_request(error.to_string()))?,
        };
        let committee_for_session = || {
            session
                .as_ref()
                .and_then(Session::committee)
                .cloned()
                .ok_or_else(|| IpcError::bad_request("Join a committee before writing a seat card"))
        };
        let key = match target {
            CardTarget::Own if local_person => CardKey::Person {
                senator_id: writer.clone(),
            },
            CardTarget::Own => {
                let committee_id = committee_for_session()?;
                check_seated(shared, &committee_id, &writer).await?;
                CardKey::Seat {
                    committee_id,
                    senator_id: writer.clone(),
                }
            }
            CardTarget::Committee { committee } => {
                let committee_id = match (&session, committee) {
                    (Some(_), Some(committee)) if committee_for_session()? != committee => {
                        return Err(IpcError::bad_request(
                            "Only the bound committee can be changed",
                        ));
                    }
                    (_, Some(committee)) => committee,
                    (_, None) => committee_for_session()?,
                };
                let info = shared
                    .api
                    .chair(&committee_id)
                    .await
                    .map_err(server_error)?;
                if info.chair.as_ref() != Some(&writer) {
                    return Err(IpcError::bad_request(
                        "Only the current chair can write this committee card",
                    ));
                }
                CardKey::Committee { committee_id }
            }
            CardTarget::Member { senator } if local_person => {
                let committee_id = senator
                    .committee
                    .ok_or_else(|| IpcError::bad_request("Qualify the seat as committee::alias"))?;
                let aliases = shared.api.aliases().await.map_err(server_error)?;
                let senator_id = match aliases {
                    KnownAliases::Scoped(aliases) => aliases
                        .get(&committee_id, &senator.alias)
                        .cloned()
                        .unwrap_or(senator.alias),
                    _ => senator.alias,
                };
                let me = shared.api.me().await.map_err(server_error)?;
                if !me.agents.contains(&senator_id) {
                    return Err(IpcError::bad_request("That seat belongs to another person"));
                }
                check_seated(shared, &committee_id, &senator_id).await?;
                CardKey::Seat {
                    committee_id,
                    senator_id,
                }
            }
            CardTarget::Member { .. } => {
                return Err(IpcError::bad_request(
                    "Only the person can write another seat's card",
                ));
            }
        };
        registry
            .set_card(key, &writer, document)
            .map_err(|error| IpcError::bad_request(error.to_string()))
    }
}

async fn check_seated(
    shared: &Shared,
    committee: &CommitteeId,
    senator: &SenatorName,
) -> Result<(), IpcError> {
    let (presence, seat) = tokio::try_join!(
        shared.api.presence(committee),
        shared.api.committee_seat_status(committee, senator)
    )
    .map_err(server_error)?;
    if seat.unseated || !presence.agents.iter().any(|row| &row.agent == senator) {
        return Err(IpcError::bad_request(
            "The senator is not seated on this committee",
        ));
    }
    Ok(())
}

impl Shared {
    pub(super) fn observe_route(
        &self,
        committee: &CommitteeId,
        seq: Seq,
        senator: Option<&SenatorName>,
        phase: senate_observe_protocol::RoutePhase,
        reason: Option<senate_observe_protocol::HoldReason>,
    ) {
        if let Some(registry) = self
            .observe
            .as_ref()
            .filter(|registry| registry.is_watched())
        {
            registry.notice(senate_observe::LocalNotice::Route {
                committee: committee.clone(),
                seq,
                senator: senator.cloned(),
                phase,
                reason,
            });
        }
    }

    pub(super) fn observe_routes(
        &self,
        items: &[ItemKey],
        phase: senate_observe_protocol::RoutePhase,
        reason: Option<senate_observe_protocol::HoldReason>,
    ) {
        if self
            .observe
            .as_ref()
            .is_some_and(|registry| registry.is_watched())
        {
            for (senator, committee, seq) in items {
                self.observe_route(committee, *seq, Some(senator), phase, reason);
            }
        }
    }
}

pub(super) fn approval_tool(kind: crate::approvals::ApprovalKind) -> ApprovalTool {
    match kind {
        crate::approvals::ApprovalKind::Command | crate::approvals::ApprovalKind::Exec => {
            ApprovalTool::Command
        }
        crate::approvals::ApprovalKind::FileChange | crate::approvals::ApprovalKind::Patch => {
            ApprovalTool::Edit
        }
    }
}

pub(super) fn claude_tool(name: &str) -> ApprovalTool {
    match name.split_ascii_whitespace().next() {
        Some("Bash") => ApprovalTool::Command,
        Some("Edit" | "Write" | "MultiEdit" | "NotebookEdit") => ApprovalTool::Edit,
        _ => ApprovalTool::Other,
    }
}

pub(super) fn claude_id(
    host: &EventId,
    candidate: &crate::host_approval::ClaudeCandidate,
) -> String {
    format!("claude:{host}:{}:{}", candidate.at, candidate.fingerprint)
}

impl Shared {
    pub(super) fn observe_approval_decided(
        &self,
        native_id: &str,
        session: &Session,
        tool: ApprovalTool,
        decision: crate::approvals::Decision,
    ) {
        if let Some(registry) = self
            .observe
            .as_ref()
            .filter(|registry| registry.is_watched())
            && let Some(committee) = session.committee()
        {
            registry.notice(senate_observe::LocalNotice::ApprovalDecided {
                native_id: native_id.into(),
                committee: committee.clone(),
                senator: session.agent.clone(),
                tool,
                decision: match decision {
                    crate::approvals::Decision::Allow
                    | crate::approvals::Decision::AllowForSession => ApprovalDecision::Allowed,
                    crate::approvals::Decision::Deny | crate::approvals::Decision::Cancel => {
                        ApprovalDecision::Denied
                    }
                },
            });
        }
    }
}
