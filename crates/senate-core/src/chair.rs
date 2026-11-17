//! Who chairs a committee.
//!
//! Every committee has exactly one chair: any senator of the Senate, a
//! principal or an agent. Who it is, is a fact of the committee's log, so
//! every reader agrees ([`ChairFold`]):
//!
//! 1. the last chair set explicitly: a `note` the server writes from
//!    `committee` carrying [`meta::CHAIR`](crate::meta::CHAIR) =
//!    [`ChairPayload`] (`senate chair <senator>`, or the Speaker's office);
//! 2. else the first agent to take a seat on the committee: its first
//!    contact (a stream, a heartbeat, a cursor), which the server writes
//!    as the agent's first `presence` when the log shows no agent yet, or
//!    its first event of its own (the server's reading also counts it);
//! 3. else the senator who wrote the committee's first event (the principal
//!    who created it).
//!
//! `chair` is a link, like `speaker`: written as an address it reaches the
//! committee's chair. The chair picks the agent for `senator::*` when the
//! sender cannot, covers for an absent member from that member's own notes,
//! receives members' escalations, and does what the Speaker asks. A chair
//! that is absent (or concerned itself) leaves its duties to the Speaker's
//! desk.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::address::Address;
use crate::continuity::PresencePayload;
use crate::event::{Event, Kind, meta};
use crate::ids::Seq;
use crate::senator::SenatorName;

/// `meta["chair"]`: the committee's chair, set explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChairPayload {
    pub senator: SenatorName,
}

impl ChairPayload {
    pub const META_KEY: &'static str = meta::CHAIR;

    pub fn of(event: &Event) -> Option<Self> {
        if event.from != Address::Committee {
            return None;
        }
        serde_json::from_value(event.meta.get(meta::CHAIR)?.clone()).ok()
    }

    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// How the chair was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChairSource {
    /// Set explicitly, at that seq.
    Set,
    /// The first agent seated on the committee.
    FirstAgent,
    /// The author of the committee's first event.
    Creator,
}

/// The chair of one committee, folded from its log.
#[derive(Debug, Clone, Default)]
pub struct ChairFold {
    head: Seq,
    set: Option<SenatorName>,
    first_agent: Option<SenatorName>,
    creator: Option<SenatorName>,
}

impl ChairFold {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn head(&self) -> Seq {
        self.head
    }

    pub fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        if self.creator.is_none()
            && let Some(author) = event.from.as_senator()
        {
            self.creator = Some(author.clone());
        }
        if let Some(chair) = ChairPayload::of(event) {
            self.set = Some(chair.senator);
            return;
        }
        if self.first_agent.is_none()
            && event.kind == Kind::Presence
            && let Some(presence) = PresencePayload::of(event)
        {
            self.first_agent = Some(presence.agent);
        }
    }

    /// The committee's chair, and how it was found; `None` for a committee
    /// with no event yet.
    pub fn chair(&self) -> Option<(&SenatorName, ChairSource)> {
        self.set
            .as_ref()
            .map(|s| (s, ChairSource::Set))
            .or_else(|| {
                self.first_agent
                    .as_ref()
                    .map(|s| (s, ChairSource::FirstAgent))
            })
            .or_else(|| self.creator.as_ref().map(|s| (s, ChairSource::Creator)))
    }

    /// The chair from a whole log.
    pub fn of(events: &[Event]) -> Self {
        let mut fold = Self::new();
        for event in events {
            fold.observe(event);
        }
        fold
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::continuity::{PresenceState, presence_event};
    use crate::event::NewEvent;
    use crate::ids::CommitteeId;

    fn c() -> CommitteeId {
        CommitteeId::new("ibot").unwrap()
    }

    fn name(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn presence(agent: &str) -> NewEvent {
        presence_event(
            c(),
            &PresencePayload {
                agent: name(agent),
                state: PresenceState::Online,
                since: Utc::now(),
                since_seq: Seq(0),
                previous: None,
                last_seen: None,
                cursor: None,
                lag: None,
                new_owner: None,
            },
        )
    }

    fn log(events: Vec<NewEvent>) -> Vec<Event> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, e)| e.into_event(Seq(i as u64 + 1), Utc::now()))
            .collect()
    }

    #[test]
    fn the_creator_chairs_until_an_agent_sits_then_the_first_agent() {
        let events = log(vec![
            NewEvent::new(
                c(),
                Kind::Note,
                "senator::wendell".parse().unwrap(),
                "hello",
            ),
            presence("claude"),
            presence("codex"),
        ]);
        assert_eq!(
            ChairFold::of(&events[..1]).chair(),
            Some((&name("wendell"), ChairSource::Creator))
        );
        assert_eq!(
            ChairFold::of(&events).chair(),
            Some((&name("claude"), ChairSource::FirstAgent))
        );
        assert_eq!(ChairFold::of(&[]).chair(), None);
    }

    #[test]
    fn a_chair_set_explicitly_wins_and_only_the_server_sets_one() {
        let set = |who: &str, from: Address| {
            NewEvent::new(c(), Kind::Note, from, "")
                .with_meta(meta::CHAIR, ChairPayload { senator: name(who) }.value())
        };
        let events = log(vec![
            presence("claude"),
            set("codex", Address::Committee),
            // Anyone else's claim is not a chair.
            set("mallory", "senator::mallory".parse().unwrap()),
        ]);
        assert_eq!(
            ChairFold::of(&events).chair(),
            Some((&name("codex"), ChairSource::Set))
        );
    }
}
