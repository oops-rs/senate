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
