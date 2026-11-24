//! An explicit change to one agent's seat on a committee.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    Address, CommitteeId, Event, EventId, Kind, Name, NewEvent, PresencePayload, PresenceState,
    SenatorName, Seq, meta,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommitteeSeatAction {
    Unseat,
    Join,
}

/// A server-authored membership operation. The senator is its primary id
/// and `by` is the authenticated principal who made the change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteeSeatChange {
    pub senator: SenatorName,
    pub action: CommitteeSeatAction,
    pub by: Name,
}

impl CommitteeSeatChange {
    pub fn event(&self, committee: CommitteeId, id: EventId) -> NewEvent {
        NewEvent::new(committee, Kind::Presence, Address::Committee, "")
            .with_id(id)
            .with_meta(
                meta::COMMITTEE_SEAT,
                serde_json::to_value(self).unwrap_or_default(),
            )
    }

    pub fn of(event: &Event) -> Option<Self> {
        if event.kind != Kind::Presence || event.from != Address::Committee {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::COMMITTEE_SEAT)?.clone()).ok()
    }

    /// An explicit join grants a seat, even before the agent connects.
    pub fn joined_presence(&self, event: &Event) -> PresencePayload {
        PresencePayload {
            paused: false,
            agent: self.senator.clone(),
            state: PresenceState::Absent,
            since: event.ts,
            since_seq: event.seq,
            previous: None,
            last_seen: None,
            cursor: None,
            lag: None,
            new_owner: None,
        }
    }
}

/// The latest explicit change for each agent on one committee.
#[derive(Debug, Clone, Default)]
pub struct CommitteeSeats {
    latest: BTreeMap<SenatorName, (Seq, CommitteeSeatAction)>,
}

impl CommitteeSeats {
    pub fn apply(&mut self, change: &CommitteeSeatChange, seq: Seq) -> bool {
        if self
            .status(&change.senator)
            .is_some_and(|(last, _)| last >= seq)
        {
            return false;
        }
        self.latest
            .insert(change.senator.clone(), (seq, change.action));
        true
    }

    pub fn status(&self, senator: &SenatorName) -> Option<(Seq, CommitteeSeatAction)> {
        self.latest.get(senator).copied()
    }

    pub fn is_unseated(&self, senator: &SenatorName) -> bool {
        self.status(senator)
            .is_some_and(|(_, action)| action == CommitteeSeatAction::Unseat)
    }

    pub fn unseated(&self) -> impl Iterator<Item = &SenatorName> {
        self.latest.iter().filter_map(|(senator, (_, action))| {
            (*action == CommitteeSeatAction::Unseat).then_some(senator)
        })
    }
}

/// A committee archived (or unarchived) by the Speaker in person: an event
/// in its own log, never a deletion. While archived, the committee takes no
/// new threads, seats or agent connections; what is in it stays readable,
/// and its open threads may still be answered and closed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitteeArchive {
    pub archived: bool,
    /// The person who archived or unarchived it.
    pub by: Name,
}

impl CommitteeArchive {
    pub fn event(&self, committee: CommitteeId, id: EventId, body: String) -> NewEvent {
        NewEvent::new(committee, Kind::Note, Address::Committee, body)
            .with_id(id)
            .with_meta(
                meta::COMMITTEE_ARCHIVE,
                serde_json::to_value(self).unwrap_or_default(),
            )
    }

    pub fn of(event: &Event) -> Option<Self> {
        if event.from != Address::Committee {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::COMMITTEE_ARCHIVE)?.clone()).ok()
    }
}

/// Whether a committee is archived, folded from its log: the latest
/// [`CommitteeArchive`] says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveFold {
    archived: Option<Seq>,
}

impl ArchiveFold {
    pub fn observe(&mut self, event: &Event) {
        if let Some(change) = CommitteeArchive::of(event) {
            self.archived = change.archived.then_some(event.seq);
        }
    }

    /// The seq it was archived at, while it is.
    pub fn archived(&self) -> Option<Seq> {
        self.archived
    }
}

#[cfg(test)]
mod archive_tests {
    use super::*;

    #[test]
    fn the_latest_change_says_whether_it_is_archived() {
        let committee = CommitteeId::new("api-test").unwrap();
        let change = |archived, seq| {
            CommitteeArchive {
                archived,
                by: Name::new("wendell").unwrap(),
            }
            .event(committee.clone(), EventId::new(), String::new())
            .into_event(Seq(seq), chrono::Utc::now())
        };
        let mut fold = ArchiveFold::default();
        assert_eq!(fold.archived(), None);
        fold.observe(&change(true, 4));
        assert_eq!(fold.archived(), Some(Seq(4)));
        fold.observe(&change(false, 9));
        assert_eq!(fold.archived(), None);
        // The same meta from anyone but the server is no change.
        let mut forged = change(true, 12);
        forged.from = Address::Speaker;
        fold.observe(&forged);
        assert_eq!(fold.archived(), None);
    }
}
