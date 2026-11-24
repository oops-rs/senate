use std::collections::BTreeMap;

use senate_observe_protocol::{
    IdKey, Removals, Replacements, SeatKey, Snapshot, StatePatch, ThreadKey, Upserts,
};

fn changes<T: Clone + PartialEq, K: Ord + Clone>(
    before: &[T],
    after: &[T],
    key: impl Fn(&T) -> K,
) -> (Vec<T>, Vec<K>) {
    let before: BTreeMap<_, _> = before.iter().map(|row| (key(row), row)).collect();
    let after: BTreeMap<_, _> = after.iter().map(|row| (key(row), row)).collect();
    let removals = before.keys().filter(|id| !after.contains_key(*id));
    let upserts = after
        .iter()
        .filter(|(id, row)| before.get(*id) != Some(*row));
    (
        upserts.map(|(_, row)| (*row).clone()).collect(),
        removals.cloned().collect(),
    )
}

pub(crate) fn patches(before: &Snapshot, after: &Snapshot) -> StatePatch {
    let mut upsert = Upserts::default();
    let mut remove = Removals::default();
    macro_rules! diff_ids {
        ($($field:ident),+ $(,)?) => { $(
            let (rows, keys) = changes(&before.$field, &after.$field, |row| row.id.clone());
            upsert.$field = rows;
            remove.$field = keys.into_iter().map(|id| IdKey { id }).collect();
        )+ };
    }
    diff_ids!(links, committees, projects, approvals);
    let (senators, seats) = changes(&before.senators, &after.senators, |row| {
        (row.committee_id.clone(), row.id.clone())
    });
    upsert.senators = senators;
    remove.senators = seats
        .into_iter()
        .map(|(committee_id, id)| SeatKey { committee_id, id })
        .collect();
    let (retired, retired_keys) = changes(&before.retired, &after.retired, |row| {
        (row.committee_id.clone(), row.id.clone())
    });
    upsert.retired = retired;
    remove.retired = retired_keys
        .into_iter()
        .map(|(committee_id, id)| SeatKey { committee_id, id })
        .collect();
    macro_rules! diff_threads {
        ($($field:ident),+ $(,)?) => { $(
            let (rows, keys) = changes(&before.$field, &after.$field, |row| (row.committee_id.clone(), row.root_seq));
            upsert.$field = rows;
            remove.$field = keys.into_iter().map(|(committee_id, root_seq)| ThreadKey { committee_id, root_seq }).collect();
        )+ };
    }
    diff_threads!(threads, desk_items);
    let replace = Replacements {
        senate: (before.senate != after.senate).then(|| after.senate.clone()),
        windows: (before.windows != after.windows).then(|| after.windows.clone()),
        navigation: (before.navigation != after.navigation)
            .then(|| after.navigation.clone().unwrap_or_default()),
    };
    StatePatch {
        upsert: (upsert != Upserts::default()).then_some(upsert),
        remove: (remove != Removals::default()).then_some(remove),
        replace: (replace != Replacements::default()).then_some(replace),
    }
}

pub(crate) fn change_hints(before: &Snapshot, after: &Snapshot) -> Vec<crate::Hint> {
    use senate_observe_protocol::{
        ApprovalAsked, ChairChanged, CommitteeArchived, Event, Hosting, LinkChanged, SenatorRef,
        SenatorState, SenatorStateChanged, ThreadStateChanged,
    };

    let mut hints = Vec::new();
    let previous_committees: BTreeMap<_, _> =
        before.committees.iter().map(|row| (&row.id, row)).collect();
    for row in &after.committees {
        let Some(old) = previous_committees.get(&row.id) else {
            continue;
        };
        if old.archived != row.archived {
            hints.push(crate::Hint {
                committee_id: Some(row.id.clone()),
                link: row.link.clone(),
                event: Event::CommitteeArchived(CommitteeArchived {
                    archived: row.archived,
                }),
            });
        }
        if old.chair_id != row.chair_id {
            hints.push(crate::Hint {
                committee_id: Some(row.id.clone()),
                link: row.link.clone(),
                event: Event::CommitteeChairChanged(ChairChanged {
                    previous_chair_id: old.chair_id.clone(),
                    chair_id: row.chair_id.clone(),
                }),
            });
        }
    }
    let previous_senators: BTreeMap<_, _> = before
        .senators
        .iter()
        .map(|row| ((&row.committee_id, &row.id), row))
        .collect();
    for row in &after.senators {
        let Some(old) = previous_senators.get(&(&row.committee_id, &row.id)) else {
            continue;
        };
        if old.state != row.state {
            hints.push(crate::Hint {
                committee_id: Some(row.committee_id.clone()),
                link: row.link.clone(),
                event: Event::SenatorStateChanged(SenatorStateChanged {
                    senator_id: row.id.clone(),
                    previous_state: old.state,
                    state: row.state,
                }),
            });
        }
    }
    for row in &after.senators {
        let Some(old) = previous_senators.get(&(&row.committee_id, &row.id)) else {
            continue;
        };
        let senator = || SenatorRef {
            senator_id: row.id.clone(),
        };
        let append = |hints: &mut Vec<crate::Hint>, event| {
            hints.push(crate::Hint {
                committee_id: Some(row.committee_id.clone()),
                link: row.link.clone(),
                event,
            });
        };
        match (old.state, row.state) {
            (previous, SenatorState::Paused) if previous != SenatorState::Paused => {
                append(&mut hints, Event::SenatorPaused(senator()));
            }
            (SenatorState::Paused, SenatorState::Idle | SenatorState::Working) => {
                append(&mut hints, Event::SenatorResumed(senator()));
            }
            _ => {}
        }
        if old.hosted != Hosting::Here && row.hosted == Hosting::Here {
            append(&mut hints, Event::SenatorStarted(senator()));
        }
        if old.state != SenatorState::Stopped && row.state == SenatorState::Stopped {
            append(&mut hints, Event::SenatorStopped(senator()));
        }
        match (old.viewer_attached, row.viewer_attached) {
            (Some(true), Some(false) | None) => {
                append(&mut hints, Event::ViewerDetached(senator()))
            }
            (Some(false) | None, Some(true)) => {
                append(&mut hints, Event::ViewerAttached(senator()))
            }
            _ => {}
        }
    }
    let previous_approvals: std::collections::BTreeSet<_> =
        before.approvals.iter().map(|row| &row.id).collect();
    for approval in &after.approvals {
        if !previous_approvals.contains(&approval.id) {
            hints.push(crate::Hint {
                committee_id: Some(approval.committee_id.clone()),
                link: approval.link.clone(),
                event: Event::ApprovalAsked(ApprovalAsked {
                    id: approval.id.clone(),
                    senator_id: approval.senator_id.clone(),
                    tool_kind: approval.tool_kind,
                }),
            });
        }
    }
    let previous_threads: BTreeMap<_, _> = before
        .threads
        .iter()
        .map(|row| ((&row.committee_id, row.root_seq), row))
        .collect();
    for row in &after.threads {
        let Some(old) = previous_threads.get(&(&row.committee_id, row.root_seq)) else {
            continue;
        };
        if old.state != row.state || old.waits_on != row.waits_on {
            hints.push(crate::Hint {
                committee_id: Some(row.committee_id.clone()),
                link: row.link.clone(),
                event: Event::ThreadStateChanged(ThreadStateChanged {
                    root_seq: row.root_seq,
                    previous_state: old.state,
                    state: row.state,
                    waits_on: row.waits_on,
                }),
            });
        }
    }
    let previous_links: BTreeMap<_, _> = before.links.iter().map(|row| (&row.id, row)).collect();
    for row in &after.links {
        if let Some(old) = previous_links.get(&row.id)
            && old.state != row.state
        {
            hints.push(crate::Hint {
                committee_id: None,
                link: row.link.clone(),
                event: Event::LinkChanged(LinkChanged {
                    id: row.id.clone(),
                    previous_state: old.state,
                    state: row.state,
                }),
            });
        }
    }
    let previous_projects: BTreeMap<_, _> = before
        .projects
        .iter()
        .map(|project| (&project.id, project))
        .collect();
    for project in &after.projects {
        if let Some(previous) = previous_projects
            .get(&project.id)
            .and_then(|project| project.releases.as_ref())
            && let Some(releases) = &project.releases
        {
            for release in releases {
                if !previous.iter().any(|old| old.tag == release.tag) {
                    hints.push(crate::Hint {
                        committee_id: None,
                        link: None,
                        event: Event::ProjectReleaseSeen(
                            senate_observe_protocol::ProjectReleaseSeen {
                                project_id: project.id.clone(),
                                release: release.clone(),
                            },
                        ),
                    });
                }
            }
        }
    }
    hints
}
