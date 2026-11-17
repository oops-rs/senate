//! Who chairs a committee, as the server reads it, and what awaits the
//! chair: the chair's docket.
//!
//! [`ChairView`] is [`ChairFold`] with one reading added. The first agent
//! to take a seat on a committee chairs it; the log shows a seat by the
//! agent's `presence`, but the leases hold an agent's first appearance
//! unwritten (only later transitions are logged). So an agent's first
//! event of its own on the committee counts as its seat too, whichever
//! comes first. Both are facts of the log: any reader that knows the
//! Senate's people reads the same chair.
//!
//! The chair's items are the roots addressed to `chair` (among them the
//! messages for `senator::*` the server put to the chair to pick, carrying
//! [`meta::PICK`]). An item leaves when the chair (whoever chairs when it
//! writes) or the Speaker's office writes in its thread, relays it, or the
//! thread is archived. Covering for away members is added when the docket
//! is served, as for the Speaker's desk.

use std::collections::BTreeMap;

use senate_core::{
    Address, Awaiting, ChairFold, ChairSource, DocketItem, Event, Kind, People, PickPayload,
    PresencePayload, SenatorName, Seq, StatusPayload, StatusState, meta,
};
use serde_json::Value;

/// Whether `event` is a root that waits on the chair.
fn awaits_chair(event: &Event) -> bool {
    event.thread.is_none()
        && matches!(event.kind, Kind::Question | Kind::Request | Kind::Note)
        && event.to.contains(&Address::Chair)
}

/// A committee's chair, folded from its log (see the module docs).
#[derive(Debug, Clone)]
pub(crate) struct ChairView {
    head: Seq,
    fold: ChairFold,
    people: People,
    first_agent: Option<SenatorName>,
}

impl ChairView {
    pub(crate) fn new(people: People) -> Self {
        Self {
            head: Seq::ZERO,
            fold: ChairFold::new(),
            people,
            first_agent: None,
        }
    }

    pub(crate) fn head(&self) -> Seq {
        self.head
    }

    pub(crate) fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        self.fold.observe(event);
        if self.first_agent.is_none() {
            self.first_agent = match event.kind {
                Kind::Presence => PresencePayload::of(event).map(|p| p.agent),
                _ => self.people.as_agent(&event.from).cloned(),
            };
        }
    }

    /// The chair and how it was found; `None` for a committee with no
    /// event yet.
    pub(crate) fn chair(&self) -> Option<(&SenatorName, ChairSource)> {
        match self.fold.chair() {
            Some((set, ChairSource::Set)) => Some((set, ChairSource::Set)),
            found => self
                .first_agent
                .as_ref()
                .map(|agent| (agent, ChairSource::FirstAgent))
                .or(found),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ChairDocket {
    head: Seq,
    chair: ChairView,
    open: BTreeMap<Seq, Event>,
}

impl ChairDocket {
    pub(crate) fn new(people: People) -> Self {
        Self {
            head: Seq::ZERO,
            chair: ChairView::new(people),
            open: BTreeMap::new(),
        }
    }

    pub(crate) fn head(&self) -> Seq {
        self.head
    }

    pub(crate) fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        self.chair.observe(event);
        let by_chair = event
            .from
            .as_senator()
            .is_some_and(|s| self.chair.chair().is_some_and(|(chair, _)| chair == s));
        if awaits_chair(event) && !by_chair {
            self.open.insert(event.seq, event.clone());
            return;
        }
        if by_chair || event.from == Address::Speaker {
            if let Some(root) = event.thread {
                self.open.remove(&root);
            }
            if let Some(relayed) = event.meta.get(meta::RELAYED_FROM).and_then(Value::as_u64) {
                self.open.remove(&Seq(relayed));
            }
            return;
        }
        if event.kind == Kind::Status
            && let Some(root) = event.thread
            && StatusPayload::of(event).is_some_and(|s| s.state == StatusState::Archived)
        {
            self.open.remove(&root);
        }
    }

    pub(crate) fn items(&self) -> Vec<DocketItem> {
        self.open
            .values()
            .map(|event| DocketItem {
                awaiting: match PickPayload::of(event) {
                    Some(pick) => Awaiting::Pick {
                        owner: pick.owner,
                        candidates: pick.candidates,
                    },
                    None => Awaiting::Addressed,
                },
                intended: senate_core::locator::intended(event),
                event: event.clone(),
            })
            .collect()
    }
}
