//! Rewrite runtime-derived identities at the wire boundary. Source reads and
//! deltas still use the authoritative seat keys internally.

use crate::{GrantInfo, Hint, ProjectionError, Registry};
use senate_observe_protocol::{
    Address, Approval, CardSubject, Committee, Event, RetiredSenator, Senate, Senator, Snapshot,
    Thread,
    link::{AppLink, Target},
};

pub struct WireIdentities<'a> {
    pub registry: &'a Registry,
    pub grant: &'a GrantInfo,
}

impl WireIdentities<'_> {
    pub fn snapshot(&self, snapshot: &mut Snapshot) -> Result<(), ProjectionError> {
        self.senate(&mut snapshot.senate)?;
        for row in &mut snapshot.committees {
            self.committee(row)?;
        }
        for row in &mut snapshot.senators {
            self.senator(row)?;
        }
        for row in &mut snapshot.retired {
            self.retired(row)?;
        }
        for row in &mut snapshot.threads {
            self.thread(row)?;
        }
        for row in &mut snapshot.approvals {
            self.approval(row)?;
        }
        Ok(())
    }

    pub fn hint(&self, hint: &mut Hint) -> Result<(), ProjectionError> {
        self.link(&mut hint.link)?;
        match &mut hint.event {
            Event::StatePatch(patch) => {
                if let Some(upsert) = &mut patch.upsert {
                    for row in &mut upsert.committees {
                        self.committee(row)?;
                    }
                    for row in &mut upsert.senators {
                        self.senator(row)?;
                    }
                    for row in &mut upsert.retired {
                        self.retired(row)?;
                    }
                    for row in &mut upsert.threads {
                        self.thread(row)?;
                    }
                    for row in &mut upsert.approvals {
                        self.approval(row)?;
                    }
                }
                if let Some(remove) = &mut patch.remove {
                    for row in remove.senators.iter_mut().chain(&mut remove.retired) {
                        self.id(&mut row.id)?;
                    }
                }
                if let Some(senate) = patch
                    .replace
                    .as_mut()
                    .and_then(|replace| replace.senate.as_mut())
                {
                    self.senate(senate)?;
                }
            }
            Event::MessagePosted(message) => {
                self.address(&mut message.from)?;
                for address in &mut message.to {
                    self.address(address)?;
                }
            }
            Event::MessageRouted(route) => {
                if let Some(target) = &mut route.target {
                    self.address(target)?;
                }
            }
            Event::SenatorStateChanged(row) => self.id(&mut row.senator_id)?,
            Event::SenatorJoined(row)
            | Event::SenatorLeft(row)
            | Event::SenatorStarted(row)
            | Event::SenatorStopped(row)
            | Event::SenatorPaused(row)
            | Event::SenatorResumed(row)
            | Event::ViewerAttached(row)
            | Event::ViewerDetached(row) => self.id(&mut row.senator_id)?,
            Event::CommitteeChairChanged(row) => {
                for id in [&mut row.previous_chair_id, &mut row.chair_id]
                    .into_iter()
                    .flatten()
                {
                    self.id(id)?;
                }
            }
            Event::ApprovalAsked(row) => self.id(&mut row.senator_id)?,
            Event::ToolStarted(row) | Event::ToolEnded(row) => self.id(&mut row.senator_id)?,
            Event::CardChanged(row) => match &mut row.subject {
                CardSubject::Senator { senator_id, .. } => self.id(senator_id)?,
                CardSubject::Person { person_id } => self.id(person_id)?,
                _ => {}
            },
            Event::MessageLinePublished(_)
            | Event::ThreadStateChanged(_)
            | Event::CommitteeCreated(_)
            | Event::CommitteeArchived(_)
            | Event::ApprovalDecided(_)
            | Event::LinkChanged(_)
            | Event::ProjectReleaseSeen(_) => {}
        }
        Ok(())
    }

    fn senate(&self, row: &mut Senate) -> Result<(), ProjectionError> {
        if let Some(owner) = &mut row.desk_owner {
            self.id(owner)?;
        }
        for worker in &mut row.desk_workers {
            self.id(worker)?;
        }
        if let Some(person) = &mut row.person {
            self.id(&mut person.id)?;
        }
        Ok(())
    }

    fn committee(&self, row: &mut Committee) -> Result<(), ProjectionError> {
        if let Some(chair) = &mut row.chair_id {
            self.id(chair)?;
        }
        Ok(())
    }

    fn senator(&self, row: &mut Senator) -> Result<(), ProjectionError> {
        self.seat(&mut row.id, &mut row.alias, &mut row.link)
    }

    fn retired(&self, row: &mut RetiredSenator) -> Result<(), ProjectionError> {
        self.seat(&mut row.id, &mut row.alias, &mut row.link)
    }

    fn seat(
        &self,
        id: &mut String,
        alias: &mut Option<String>,
        link: &mut Option<String>,
    ) -> Result<(), ProjectionError> {
        if alias.as_ref().is_some_and(|alias| {
            alias == id || (native(id) && id.split_once('-').is_some_and(|(_, key)| key == alias))
        }) {
            *alias = None;
        }
        self.id(id)?;
        self.link(link)
    }

    fn thread(&self, row: &mut Thread) -> Result<(), ProjectionError> {
        self.address(&mut row.opener)?;
        for address in &mut row.addressees {
            self.address(address)?;
        }
        Ok(())
    }

    fn approval(&self, row: &mut Approval) -> Result<(), ProjectionError> {
        self.id(&mut row.senator_id)
    }

    fn address(&self, address: &mut Address) -> Result<(), ProjectionError> {
        if let Address::Senator { id } = address {
            self.id(id)?;
        }
        Ok(())
    }

    fn id(&self, value: &mut String) -> Result<(), ProjectionError> {
        if native(value) {
            *value = self
                .registry
                .seat_id(self.grant, value)
                .map_err(|_| ProjectionError::NotReady)?;
        }
        Ok(())
    }

    fn link(&self, link: &mut Option<String>) -> Result<(), ProjectionError> {
        let Some(url) = link
            .as_deref()
            .and_then(|value| value.parse::<AppLink>().ok())
        else {
            *link = None;
            return Ok(());
        };
        if let Target::Senator(name) = url.target() {
            let (committee, raw) = name
                .split_once("::")
                .map_or((None, name.as_str()), |(committee, name)| {
                    (Some(committee), name)
                });
            if native(raw) {
                let id = self
                    .registry
                    .seat_id(self.grant, raw)
                    .map_err(|_| ProjectionError::NotReady)?;
                let target = Target::Senator(
                    committee.map_or_else(|| id.clone(), |committee| format!("{committee}::{id}")),
                );
                *link = Some(
                    url.with_target(target)
                        .map_err(|_| ProjectionError::NotReady)?
                        .to_string(),
                );
            }
        }
        Ok(())
    }
}

fn native(value: &str) -> bool {
    ["codex-", "claude-", "agy-", "acp-"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CreateGrant, Scope};
    use senate_observe_protocol::{Removals, SeatKey, StatePatch};

    #[test]
    fn snapshot_and_removal_use_the_same_opaque_grant_scoped_id()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let registry = Registry::open(directory.path(), "127.0.0.1:45501".parse()?)?;
        let grant = || {
            registry.create(
                CreateGrant {
                    name: "fixture".into(),
                    committees: None,
                    all_committees: true,
                    scope: Scope::Metadata,
                    lines: None,
                    origins: vec![],
                    topology: false,
                    expires_in_days: None,
                },
                Default::default(),
            )
        };
        let first = grant()?.grant;
        let second = grant()?.grant;
        let raw = "claude-01234567-89ab-cdef-0123-456789abcdef";
        let identities = WireIdentities {
            registry: &registry,
            grant: &first,
        };
        let mut snapshot: Snapshot =
            serde_json::from_str(include_str!("../../../docs/observe/v1/fixtures/state.json"))?;
        snapshot.senators[0].id = raw.into();
        snapshot.senators[0].alias = Some(raw[7..].into());
        snapshot.senators[0].link = Some(
            AppLink::for_senate(Target::Senator(format!("chair::{raw}")), "fixture")?.to_string(),
        );
        identities
            .snapshot(&mut snapshot)
            .map_err(|_| "identity projection failed")?;
        let id = registry.seat_id(&first, raw)?;
        assert_eq!(snapshot.senators[0].id, id);
        assert_eq!(snapshot.senators[0].alias, None);
        let snapshot_json = serde_json::to_string(&snapshot)?;
        assert!(!snapshot_json.contains(&raw[7..]));
        assert_ne!(registry.seat_id(&second, raw)?, id);
        assert_ne!(
            registry.approval_id(&first, raw)?,
            registry.approval_id(&second, raw)?
        );
        assert_ne!(
            registry.history_id(&first, raw)?,
            registry.history_id(&second, raw)?
        );
        let mut hint = Hint {
            at: None,
            committee_id: None,
            link: None,
            event: Event::StatePatch(Box::new(StatePatch {
                remove: Some(Removals {
                    senators: vec![SeatKey {
                        id: raw.into(),
                        committee_id: "chair".into(),
                    }],
                    ..Removals::default()
                }),
                ..StatePatch::default()
            })),
        };
        identities
            .hint(&mut hint)
            .map_err(|_| "hint projection failed")?;
        assert!(serde_json::to_string(&hint.event)?.contains(&id));
        assert!(!serde_json::to_string(&hint.event)?.contains(raw));
        Ok(())
    }
}
