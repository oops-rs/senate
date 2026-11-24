use std::sync::Weak;

use senate_observe::{LocalSeat, MachineFacts, MachineSource, ProjectionError};
use senate_observe_protocol::{LinkState, SenatorState};

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
        let active: Vec<_> = sessions.iter().filter(|s| s.is_live()).cloned().collect();
        let host_facts =
            crate::host::status(&paths, &shared.store, &active).map_err(unavailable)?;
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
        })
    }
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
