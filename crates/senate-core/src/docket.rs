//! What awaits the Speaker of one Senate on a committee: the docket.
//!
//! A Speaker held by a senator ([`SpeakerSeat::Senator`]) is a coding-agent
//! session, not a process that watches the log. Its duties are few, and
//! each one starts from an item on the docket:
//!
//! - **Pick.** A person (or anyone who is not an agent) wrote to
//!   `senator::<person>/*` and that person has several agents here. The
//!   server stores the message addressed to `speaker` with
//!   [`meta::PICK`] = a [`PickPayload`] naming the candidates
//!   ([`Awaiting::Pick`]). The Speaker routes it to one of them as a relay
//!   (`from: speaker`, `on_behalf_of` the sender, [`meta::RELAYED_FROM`] =
//!   the item), or escalates it to the person.
//! - **Addressed.** A root question, request or note addressed to `speaker`
//!   ([`Awaiting::Addressed`]). The Speaker answers it in its thread.
//! - **Cover.** A root question to an agent of this Senate that has no live
//!   connection and no answer yet ([`Awaiting::Away`]). The Speaker answers
//!   on that agent's behalf from that agent's own latest checkpoint, never
//!   another agent's, or escalates it to the person.
//!
//! An item leaves the docket when the Speaker writes in its thread, relays
//! it, or the thread is archived. Folding is pure ([`SpeakerDocket`]); which
//! agents are away is the server's knowledge, added when the docket is
//! served ([`DocketPage`]).
//!
//! [`SpeakerSeat::Senator`]: crate::SpeakerSeat::Senator

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::address::Address;
use crate::continuity::PresenceState;
use crate::event::{Event, Kind, StatusState, meta};
use crate::ids::{CommitteeId, Name, Seq};
use crate::roster::SpeakerSeat;
use crate::senator::SenatorPath;
use crate::task::StatusPayload;

/// `meta["pick"]` on a message the server addressed to the Speaker because
/// its sender wrote `senator::<owner>/*` and `owner` has several agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickPayload {
    pub owner: Name,
    pub candidates: Vec<SenatorPath>,
}

impl PickPayload {
    pub const META_KEY: &'static str = meta::PICK;

    pub fn of(event: &Event) -> Option<Self> {
        serde_json::from_value(event.meta.get(meta::PICK)?.clone()).ok()
    }

    pub fn value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Why an item awaits the Speaker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "why", rename_all = "snake_case")]
pub enum Awaiting {
    /// Route it to one of `candidates` ([`PickPayload`]).
    Pick {
        owner: Name,
        candidates: Vec<SenatorPath>,
    },
    /// It is addressed to the Speaker: answer it.
    Addressed,
    /// A question to `agents`, none of which is connected, with no answer:
    /// cover for one of them from its own checkpoint.
    Away { agents: Vec<SenatorPath> },
}

/// One item awaiting the Speaker: the root event and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocketItem {
    pub event: Event,
    #[serde(flatten)]
    pub awaiting: Awaiting,
}

/// The facts a Speaker picks and covers by, for one agent seated here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatFacts {
    pub agent: SenatorPath,
    /// A live connection now: an open stream with a fresh heartbeat.
    pub connected: bool,
    /// Its presence on the committee, when it ever had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence: Option<PresenceState>,
    /// Its own latest checkpoint on the committee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointFacts>,
    /// Task threads addressed to it that are not archived.
    pub open_threads: usize,
}

/// An agent's latest checkpoint, as far as picking needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointFacts {
    pub seq: Seq,
    pub ts: DateTime<Utc>,
    /// Its summary line.
    pub summary: String,
    pub branch: String,
}

/// `GET .../docket`: what awaits the Speaker on one committee, oldest first,
/// with the facts about every agent seated in this Senate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocketPage {
    pub committee: CommitteeId,
    /// Who holds the Speaker's seat here; `None`: nobody presides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<SpeakerSeat>,
    pub items: Vec<DocketItem>,
    pub seats: Vec<SeatFacts>,
    /// The last seq folded in.
    pub head: Seq,
}

/// Whether `event` is a root that waits on the Speaker: a question, request
/// or note addressed to `speaker` that the Speaker did not write itself.
pub fn awaits_speaker(event: &Event) -> bool {
    event.thread.is_none()
        && event.from != Address::Speaker
        && matches!(event.kind, Kind::Question | Kind::Request | Kind::Note)
        && event.to.contains(&Address::Speaker)
}

/// Folds a committee's log into the items addressed to the Speaker that it
/// has not dealt with. Like the task board, an event at or below the last
/// one seen is ignored.
#[derive(Debug, Clone, Default)]
pub struct SpeakerDocket {
    head: Seq,
    open: BTreeMap<Seq, Event>,
}

impl SpeakerDocket {
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
        if awaits_speaker(event) {
            self.open.insert(event.seq, event.clone());
            return;
        }
        if event.from == Address::Speaker {
            // Dealt with: an answer, a question to the person, or a status
            // in its thread; or a relay of it.
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

    /// The open items, oldest first, each with why it waits.
    pub fn items(&self) -> Vec<DocketItem> {
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
                event: event.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::NewEvent;
    use crate::ids::CommitteeId;

    fn c() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    fn at(seq: u64, event: NewEvent) -> Event {
        event.into_event(Seq(seq), Utc::now())
    }

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    fn pick(seq: u64) -> Event {
        let payload = PickPayload {
            owner: Name::new("alice").unwrap(),
            candidates: vec![
                "alice/claude".parse().unwrap(),
                "alice/codex".parse().unwrap(),
            ],
        };
        at(
            seq,
            NewEvent::new(
                c(),
                Kind::Question,
                addr("senator::alice"),
                "who looks at refunds?",
            )
            .to(Address::Speaker)
            .with_meta(meta::PICK, payload.value()),
        )
    }

    #[test]
    fn a_pick_waits_until_the_speaker_relays_it() {
        let mut docket = SpeakerDocket::new();
        docket.observe(&pick(1));
        let items = docket.items();
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0].awaiting,
            Awaiting::Pick { candidates, .. } if candidates.len() == 2
        ));
        // Someone else talking about it does not deal with it.
        docket.observe(&at(
            2,
            NewEvent::new(c(), Kind::Note, addr("senator::alice/codex"), "hm").in_thread(Seq(1)),
        ));
        assert_eq!(docket.items().len(), 1);
        docket.observe(&at(
            3,
            NewEvent::new(
                c(),
                Kind::Question,
                Address::Speaker,
                "who looks at refunds?",
            )
            .to(addr("senator::alice/codex"))
            .on_behalf_of(addr("senator::alice"))
            .with_meta(meta::RELAYED_FROM, serde_json::json!(1)),
        ));
        assert!(docket.items().is_empty());
        // Seen once only.
        docket.observe(&pick(1));
        assert!(docket.items().is_empty());
    }

    #[test]
    fn what_is_addressed_to_the_speaker_waits_for_its_reply_or_archive() {
        let mut docket = SpeakerDocket::new();
        let ask = |seq| {
            at(
                seq,
                NewEvent::new(c(), Kind::Question, addr("senator::alice/codex"), "status?")
                    .to(Address::Speaker),
            )
        };
        docket.observe(&ask(1));
        docket.observe(&ask(2));
        // The Speaker's own messages never wait on it.
        docket.observe(&at(
            3,
            NewEvent::new(c(), Kind::Note, Address::Speaker, "x").to(Address::Speaker),
        ));
        assert_eq!(docket.items().len(), 2);
        assert_eq!(docket.items()[0].awaiting, Awaiting::Addressed);
        docket.observe(&at(
            4,
            NewEvent::new(c(), Kind::Answer, Address::Speaker, "fine")
                .to(addr("senator::alice/codex"))
                .in_thread(Seq(1)),
        ));
        let mut archive = StatusPayload::new(StatusState::Archived);
        archive.reason = Some("done".into());
        docket.observe(&at(
            5,
            NewEvent::new(c(), Kind::Status, addr("senator::alice"), "closed")
                .in_thread(Seq(2))
                .with_meta(meta::STATUS, archive.value()),
        ));
        assert!(docket.items().is_empty());
    }

    #[test]
    fn a_docket_page_reads_back() {
        let page = DocketPage {
            committee: c(),
            speaker: Some(SpeakerSeat::Senator("alice/claude".parse().unwrap())),
            items: vec![DocketItem {
                event: pick(1),
                awaiting: Awaiting::Away {
                    agents: vec!["alice/codex".parse().unwrap()],
                },
            }],
            seats: vec![SeatFacts {
                agent: "alice/codex".parse().unwrap(),
                connected: false,
                presence: Some(PresenceState::Absent),
                checkpoint: None,
                open_threads: 1,
            }],
            head: Seq(1),
        };
        let value = serde_json::to_value(&page).unwrap();
        assert_eq!(value["items"][0]["why"], "away");
        assert_eq!(serde_json::from_value::<DocketPage>(value).unwrap(), page);
    }
}
