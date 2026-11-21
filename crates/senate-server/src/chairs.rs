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
//! [`senate_core::meta::PICK`]), whoever chaired when they came: when the
//! chair moves, the new chair has them, all but its own. An item leaves
//! when the chair (whoever chairs when it writes) or the Speaker's office
//! writes in its thread, relays it, or the thread is archived. Hosted
//! approvals stay until their archive. Covering for away members is added
//! when the docket is served, as for the Speaker's desk.

use std::collections::BTreeMap;

use senate_core::{
    Address, Awaiting, ChairFold, ChairSource, DocketItem, Event, Kind, People, PickPayload,
    SenatorName, Seq,
};

/// Whether `event` is a root that waits on the chair.
fn awaits_chair(event: &Event) -> bool {
    event.thread.is_none()
        && matches!(event.kind, Kind::Question | Kind::Request | Kind::Note)
        && event.to.contains(&Address::Chair)
}

/// A committee's chair, folded from its log (see the module docs).
#[derive(Debug, Clone)]
pub(crate) struct ChairView {
    fold: ChairFold,
    people: People,
}

impl ChairView {
    pub(crate) fn new(people: People) -> Self {
        Self {
            fold: ChairFold::new(),
            people,
        }
    }

    pub(crate) fn head(&self) -> Seq {
        self.fold.head()
    }

    pub(crate) fn observe(&mut self, event: &Event) {
        if event.seq <= self.fold.head() {
            return;
        }
        self.fold.observe(event);
        if let Some(agent) = self.people.as_agent(&event.from) {
            self.fold.observe_agent(agent, event.seq);
        }
    }

    /// The chair and how it was found; `None` for a committee with no
    /// event yet.
    pub(crate) fn chair(&self) -> Option<(&SenatorName, ChairSource)> {
        self.fold.chair()
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
        // Kept whoever wrote it: the chair's own root waits on the next
        // chair once the chair moves (left out while its author chairs).
        if awaits_chair(event) {
            self.open.insert(event.seq, event.clone());
            return;
        }
        senate_core::docket::close_docket_items(
            &mut self.open,
            event,
            by_chair || event.from == Address::Speaker,
            &self.chair.people,
        );
    }

    /// What awaits the chair now. Who chairs is read when asked, not when
    /// each root came: an item waits on whoever chairs, the chair it came
    /// to or a later one, except the chair's own roots.
    pub(crate) fn items(&self) -> Vec<DocketItem> {
        let chair = self.chair.chair().map(|(chair, _)| Address::senator(chair));
        self.open
            .values()
            .filter(|event| chair.as_ref() != Some(&event.from))
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

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use senate_core::{ChairPayload, CommitteeId, Name, NewEvent, meta};

    use super::*;

    fn committee() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn agent(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn docket() -> ChairDocket {
        ChairDocket::new(People::new(["alice"].map(|h| Name::new(h).unwrap())))
    }

    struct Log {
        docket: ChairDocket,
        seq: u64,
    }

    impl Log {
        fn new() -> Self {
            Self {
                docket: docket(),
                seq: 0,
            }
        }

        fn add(&mut self, event: NewEvent) -> Seq {
            self.seq += 1;
            let event = event.into_event(Seq(self.seq), Utc::now());
            self.docket.observe(&event);
            event.seq
        }

        fn appoint(&mut self, chair: &str) {
            self.add(
                NewEvent::new(committee(), Kind::Note, Address::Committee, "chair set").with_meta(
                    meta::CHAIR,
                    ChairPayload {
                        senator: agent(chair),
                    }
                    .value(),
                ),
            );
        }

        fn ask_chair(&mut self, from: Address, body: &str) -> Seq {
            self.add(NewEvent::new(committee(), Kind::Question, from, body).to(Address::Chair))
        }

        fn open(&self) -> Vec<Seq> {
            self.docket.items().iter().map(|i| i.event.seq).collect()
        }
    }

    #[test]
    fn an_open_item_waits_on_whoever_chairs_now() {
        let mut log = Log::new();
        log.appoint("alpha");
        let asked = log.ask_chair(Address::senator(&agent("alice")), "who reviews?");
        assert_eq!(log.open(), [asked]);
        log.appoint("beta");
        assert_eq!(log.open(), [asked]);
        // beta answers: it leaves, and a reappointment does not bring it back.
        log.add(
            NewEvent::new(
                committee(),
                Kind::Answer,
                Address::senator(&agent("beta")),
                "me",
            )
            .in_thread(asked),
        );
        assert!(log.open().is_empty());
        log.appoint("alpha");
        assert!(log.open().is_empty());
    }

    #[test]
    fn what_the_old_chair_answered_stays_answered() {
        let mut log = Log::new();
        log.appoint("alpha");
        let asked = log.ask_chair(Address::senator(&agent("alice")), "settled?");
        log.add(
            NewEvent::new(
                committee(),
                Kind::Answer,
                Address::senator(&agent("alpha")),
                "yes",
            )
            .in_thread(asked),
        );
        log.appoint("beta");
        assert!(log.open().is_empty());
    }

    /// The chair's own message to `chair` is not on its own docket, and is
    /// on the next chair's: it was decided once, when it came, before.
    #[test]
    fn the_chairs_own_message_to_the_chair_waits_on_the_next_chair() {
        let mut log = Log::new();
        log.appoint("alpha");
        let own = log.ask_chair(Address::senator(&agent("alpha")), "for whoever chairs next");
        assert!(log.open().is_empty());
        log.appoint("beta");
        assert_eq!(log.open(), [own]);
        let betas = log.ask_chair(Address::senator(&agent("beta")), "beta's own");
        assert_eq!(log.open(), [own]);
        log.appoint("alpha");
        assert_eq!(log.open(), [betas]);
    }
}
