//! Local bridge facts that augment authoritative seat records without
//! reintroducing unseated sessions or exposing any native session key.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
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
}

pub struct MachineFacts {
    pub person: String,
    pub seats: BTreeMap<(CommitteeId, SenatorName), LocalSeat>,
    pub projects: BTreeMap<CommitteeId, PathBuf>,
    pub paired_machines: Vec<String>,
    pub parent_state: LinkState,
}

pub struct LocalSeat {
    pub runtime: Option<String>,
    pub state: SenatorState,
    pub last_active_at: Option<DateTime<Utc>>,
    pub hosted: bool,
    pub viewer_attached: Option<bool>,
}

pub(crate) fn merge(
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
    let mut projects = BTreeMap::new();
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
            .unwrap_or("Project");
        committee.project_id = Some(project_id.clone());
        projects
            .entry(project_id.clone())
            .or_insert_with(|| Project {
                id: project_id,
                name: name.to_owned(),
                link: committee.link.clone(),
            });
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
