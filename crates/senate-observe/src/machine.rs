//! Local bridge facts that augment authoritative seat records without
//! reintroducing unseated sessions or exposing any native session key.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use futures::StreamExt;
use senate_core::{CommitteeId, SenatorName};
use senate_observe_protocol::{
    Hosting, LinkKind, LinkState, Person, Project, SenateLink, SenatorState, Snapshot,
    link::{AppLink, Target},
};

use crate::{GrantInfo, ProjectionError, Registry};

/// The embedded bridge supplies local facts. The server determines which
/// committees and seats actually exist in a grant.
pub trait MachineSource: Send + Sync {
    fn read(&self) -> Result<MachineFacts, ProjectionError>;
    fn idle(&self) {}
}

pub struct MachineFacts {
    pub person: String,
    pub seats: BTreeMap<(CommitteeId, SenatorName), LocalSeat>,
    pub projects: BTreeMap<CommitteeId, PathBuf>,
    pub paired_machines: Vec<String>,
    pub parent_state: LinkState,
    pub approvals: Vec<LocalApproval>,
}

pub struct LocalSeat {
    pub runtime: Option<String>,
    pub state: SenatorState,
    pub last_active_at: Option<DateTime<Utc>>,
    pub hosted: bool,
    pub viewer_attached: Option<bool>,
    pub history: Option<LocalHistory>,
}

pub struct LocalApproval {
    pub native_id: String,
    pub committee: CommitteeId,
    pub senator: SenatorName,
    pub kind: senate_observe_protocol::ApprovalTool,
    pub asked_at: DateTime<Utc>,
}

pub struct LocalHistory {
    pub native_id: String,
    pub alias: Option<String>,
}

pub(crate) async fn merge(
    registry: &Registry,
    grant: &GrantInfo,
    public: &mut Snapshot,
) -> Result<(), ProjectionError> {
    let facts = registry.machine_facts()?;
    let id = public.senate.id.as_str();
    for seat in &mut public.senators {
        let key = (
            seat.committee_id
                .parse()
                .map_err(|_| ProjectionError::NotReady)?,
            seat.id.parse().map_err(|_| ProjectionError::NotReady)?,
        );
        if let Some(local) = facts.seats.get(&key) {
            seat.runtime.clone_from(&local.runtime);
            if local.state != SenatorState::Unknown {
                seat.state = local.state;
            }
            seat.hosted = if local.hosted {
                Hosting::Here
            } else {
                Hosting::Unknown
            };
            seat.viewer_attached = local.viewer_attached;
            seat.last_active_at = seat.last_active_at.max(local.last_active_at);
        }
    }
    for retired in &mut public.retired {
        let key = (
            retired
                .committee_id
                .parse()
                .map_err(|_| ProjectionError::NotReady)?,
            retired.id.parse().map_err(|_| ProjectionError::NotReady)?,
        );
        if let Some(local) = facts.seats.get(&key) {
            retired.runtime.clone_from(&local.runtime);
            retired.last_active_at = local.last_active_at;
            if let Some(history) = &local.history {
                retired.alias = history.alias.clone().or_else(|| retired.alias.clone());
                let history_id = registry
                    .history_id(&history.native_id)
                    .map_err(|_| ProjectionError::NotReady)?;
                retired.link = AppLink::for_senate(Target::HistoryEntry(history_id), id)
                    .ok()
                    .map(|link| link.to_string());
            }
        }
    }
    for approval in facts.approvals {
        let Some(seat) = public.senators.iter_mut().find(|seat| {
            seat.committee_id == approval.committee.as_str() && seat.id == approval.senator.as_str()
        }) else {
            continue;
        };
        if matches!(seat.state, SenatorState::Idle | SenatorState::Unknown) {
            seat.state = SenatorState::WaitingOnPerson;
        }
        public.approvals.push(senate_observe_protocol::Approval {
            id: registry
                .approval_id(&approval.native_id)
                .map_err(|_| ProjectionError::NotReady)?,
            committee_id: approval.committee.to_string(),
            senator_id: approval.senator.to_string(),
            tool_kind: approval.kind,
            asked_at: approval.asked_at,
            link: AppLink::for_senate(Target::Approvals, id)
                .ok()
                .map(|link| link.to_string()),
        });
    }
    public.approvals.sort_by(|a, b| a.id.cmp(&b.id));
    public.approvals.dedup_by(|a, b| a.id == b.id);
    let mut projects = BTreeMap::new();
    let mut roots = BTreeMap::new();
    for committee in &mut public.committees {
        let key = committee
            .id
            .parse()
            .map_err(|_| ProjectionError::NotReady)?;
        let Some(path) = facts.projects.get(&key) else {
            continue;
        };
        let Some(path) = path.canonicalize().ok() else {
            continue;
        };
        let project_id = registry
            .project_id(&path)
            .map_err(|_| ProjectionError::NotReady)?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| {
                !name.is_empty() && name.len() <= 128 && !name.chars().any(char::is_control)
            })
            .unwrap_or("Project")
            .to_owned();
        roots.entry(project_id.clone()).or_insert(path);
        committee.project_id = Some(project_id.clone());
        projects
            .entry(project_id.clone())
            .or_insert_with(|| Project {
                id: project_id,
                name,
                link: committee.link.clone(),
                releases: None,
            });
    }
    // Large views keep their ordinary project rows while individual
    // release requests time out. A cached result remains usable.
    let results = futures::stream::iter(roots)
        .map(|(id, root)| async move { (id, registry.releases.get(&root).await) })
        .buffer_unordered(4);
    tokio::pin!(results);
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(7));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            result = results.next() => match result {
                Some((id, releases)) => {
                    if let Some(project) = projects.get_mut(&id) {
                        project.releases = releases;
                    }
                }
                None => break,
            },
        }
    }
    public.projects = projects.into_values().collect();
    if grant.topology {
        let link = AppLink::for_senate(Target::Profile, id)
            .ok()
            .map(|link| link.to_string());
        public.senate.person = Some(Person {
            id: facts.person.clone(),
            name: facts.person.clone(),
            link,
            card: None,
        });
        for existing in &mut public.links {
            if existing.kind == LinkKind::Parent {
                existing.state = facts.parent_state;
            }
        }
        public
            .links
            .extend(facts.paired_machines.iter().map(|name| SenateLink {
                id: format!("paired:{name}"),
                name: name.clone(),
                kind: LinkKind::PairedMachine,
                state: LinkState::Unknown,
                link: None,
            }));
        public.links.sort_by(|a, b| a.id.cmp(&b.id));
    }
    Ok(())
}
