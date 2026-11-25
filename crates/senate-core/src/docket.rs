//! What awaits the Speaker of one Senate on a committee: the docket.
//!
//! A Speaker held by a senator ([`SpeakerSeat::Senator`]) is a coding-agent
//! session, not a process that watches the log. Its duties are few, and
//! each one starts from an item on the docket:
//!
//! - **Pick.** A person (or anyone who is not an agent) wrote to
//!   `senator::*` and several agents are on the committee. The
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
//!   another agent's, or escalates it to the person. Not an item while an
//!   agent that is the away agent's secretary has a live connection:
//!   that one covers it first ([`DocketPage::covered`]).
//! - **A child Senate that is offline.** A root question addressed to a
//!   child Senate's desk while that child is offline, with nothing said of
//!   it yet ([`Awaiting::ChildOffline`]). The office answers it on the
//!   child's behalf from what this Senate sees, or tells the one who asked
//!   that it is held ([`child_offline_answer`], [`child_offline_held`]);
//!   both lines carry [`meta::CHILD_OFFLINE`]. The question itself reaches
//!   the child when it reconnects either way. Not an item once such a line
//!   is in the thread, the child has replied there, or the child is back.
//!
//! An item leaves the docket when the Speaker writes in its thread, relays
//! it, or the thread is archived; and an item that came from the Senate
//! above, when an answer from there lands under it ([`answered_above`]):
//! the office above answered in this Senate's name while it was offline. Folding is pure ([`SpeakerDocket`]); which
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
use crate::locale::Locale;
use crate::locator::Locator;
use crate::roster::SpeakerSeat;
use crate::senator::SenatorName;
use crate::task::StatusPayload;

/// `meta["pick"]` on a message the server addressed to the Speaker because
/// its sender wrote `senator::<owner>/*` and `owner` has several agents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickPayload {
    /// The person whose agents these were, in a log written before names
    /// were flat; absent since: `senator::*` names no person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<Name>,
    pub candidates: Vec<SenatorName>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        owner: Option<Name>,
        candidates: Vec<SenatorName>,
    },
    /// It is addressed to the Speaker: answer it.
    Addressed,
    /// A question to `agents`, none of which is connected, with no answer:
    /// cover for one of them from its own checkpoint.
    Away { agents: Vec<SenatorName> },
    /// A question to the child Senate `child`, which is offline, with
    /// nothing said of it yet: answer it on the child's behalf, or tell
    /// the one who asked that it is held.
    ChildOffline { child: Name },
}

/// The office's note in the thread of a message for a child Senate that
/// is offline: it is held. `away`: how long the child has not been seen.
pub fn child_offline_held(locale: Locale, child: &Name, away: chrono::Duration) -> String {
    let minutes = away.num_minutes().max(0);
    let (n, zh, en) = if minutes < 60 {
        (minutes, "分钟", "minute")
    } else if minutes < 48 * 60 {
        (minutes / 60, "小时", "hour")
    } else {
        (minutes / (24 * 60), "天", "day")
    };
    match locale {
        Locale::Zh => format!(
            "[{child}] 目前离线（最后一次连接在 {n} {zh}前）。这条消息已保留，它重新连接后会收到。"
        ),
        Locale::En => format!(
            "[{child}] is offline (last seen {n} {en}{s} ago). This is held for it and reaches \
             it when it reconnects.",
            s = if n == 1 { "" } else { "s" },
        ),
    }
}

/// The office's answer for a child Senate that is offline: `text`, under a
/// line saying whose answer it is.
pub fn child_offline_answer(locale: Locale, child: &Name, text: &str) -> String {
    let text = text.trim();
    match locale {
        Locale::Zh => format!(
            "[{child}] 目前离线，以下由 Speaker 的办公室代答；它重新连接后仍会收到这个问题。\n{text}"
        ),
        Locale::En => format!(
            "[{child}] is offline; the Speaker's office answers for it below. It still gets \
             the question when it reconnects.\n{text}"
        ),
    }
}

/// The child Senate a root question is put to, when it is put to one.
fn asked_of_child(event: &Event) -> Option<&Name> {
    if event.thread.is_some() || event.kind != Kind::Question {
        return None;
    }
    event.to.iter().find_map(|address| match address {
        Address::Child { senate, .. } => Some(senate),
        _ => None,
    })
}

/// One item awaiting the Speaker: the root event and why.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocketItem {
    pub event: Event,
    #[serde(flatten)]
    pub awaiting: Awaiting,
    /// The seat outside this committee the message was meant for
    /// ([`meta::INTENDED`]), when it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intended: Option<Locator>,
}

/// The facts a Speaker picks and covers by, for one agent seated here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatFacts {
    pub agent: SenatorName,
    /// A live connection now: an open stream with a fresh heartbeat.
    pub connected: bool,
    /// Its presence on the committee, when it ever had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence: Option<PresenceState>,
    /// When the server last heard from its bridge on the committee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    /// Its own latest checkpoint on the committee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointFacts>,
    /// Task threads addressed to it that are not archived.
    pub open_threads: usize,
    /// Its duty on the committee ([`crate::duty`]), the latest one set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duty: Option<String>,
    /// Paused on purpose (its presence is `absent` with `paused`): it is
    /// expected back, and sees what waits for it then.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
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
    /// The Speaker's office: who the Speaker is and who works the desk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub office: Option<crate::roster::Office>,
    pub items: Vec<DocketItem>,
    /// Questions to an away agent that one of its secretaries, an agent
    /// with a live session, covers (root seq → that secretary: the first
    /// present one in the agent's order, an agent of this Senate). They
    /// are not in `items`: the chain goes on to the chair and the desk only
    /// when no secretary is present. The secretary's own bridge brings
    /// them to its session.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub covered: BTreeMap<Seq, SenatorName>,
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
    /// Root questions put to a child Senate that nothing in their thread
    /// settles yet, with the child. Whether the child is offline is the
    /// server's to say when the docket is served.
    of_children: BTreeMap<Seq, (Event, Name)>,
    people: crate::People,
}

impl SpeakerDocket {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_people(people: crate::People) -> Self {
        Self {
            people,
            ..Self::default()
        }
    }

    pub fn head(&self) -> Seq {
        self.head
    }

    pub fn observe(&mut self, event: &Event) {
        if event.seq <= self.head {
            return;
        }
        self.head = event.seq;
        if let Some(child) = asked_of_child(event) {
            self.of_children
                .insert(event.seq, (event.clone(), child.clone()));
        } else if let Some(root) = event.thread
            && let Some((_, child)) = self.of_children.get(&root)
        {
            // The office said it is held or answered for the child, the
            // child replied, or the thread is closed.
            let settled = event.meta.contains_key(meta::CHILD_OFFLINE)
                || matches!(&event.from, Address::Child { senate, .. } if senate == child)
                || StatusPayload::of(event).is_some_and(|s| s.state == StatusState::Archived);
            if settled {
                self.of_children.remove(&root);
            }
        }
        if awaits_speaker(event)
            || (event.to.contains(&Address::Speaker) && event.is_hosted_approval(&self.people))
        {
            self.open.insert(event.seq, event.clone());
            return;
        }
        if let Some(root) = event.thread
            && self
                .open
                .get(&root)
                .is_some_and(|item| answered_above(item, event))
        {
            self.open.remove(&root);
            return;
        }
        close_docket_items(
            &mut self.open,
            event,
            event.from == Address::Speaker,
            &self.people,
        );
    }

    /// The root questions put to a child Senate that nothing settles yet,
    /// oldest first, each with the child: items while that child is
    /// offline ([`Awaiting::ChildOffline`]).
    pub fn of_children(&self) -> impl Iterator<Item = (&Event, &Name)> {
        self.of_children
            .values()
            .map(|(event, child)| (event, child))
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
                intended: crate::locator::intended(event),
                event: event.clone(),
            })
            .collect()
    }
}

/// Whether `reply` answers the desk item `root` from where the item came
/// from: both were brought down from the same thread of the Senate above
/// ([`meta::ORIGIN`]), and `reply` is an `answer`. The Speaker's office
/// above answered in this Senate's name while it was offline, so the item
/// is answered and nothing is left for this desk to do with it.
pub fn answered_above(root: &Event, reply: &Event) -> bool {
    if reply.kind != Kind::Answer || reply.thread != Some(root.seq) {
        return false;
    }
    match (
        crate::locator::Origin::of(root),
        crate::locator::Origin::of(reply),
    ) {
        (Some(asked), Some(answered)) => {
            asked.thread == answered.thread
                && asked.locator.senate() == answered.locator.senate()
                && asked.locator.committee() == answered.locator.committee()
        }
        _ => false,
    }
}

/// Both desks close a hosted approval notice on its archive. Discussing or
/// forwarding it does not mean anyone attended to the terminal.
pub fn close_docket_items(
    open: &mut BTreeMap<Seq, Event>,
    event: &Event,
    by_desk: bool,
    people: &crate::People,
) {
    if let Some(root) = event.thread
        && StatusPayload::of(event).is_some_and(|s| s.state == StatusState::Archived)
    {
        open.remove(&root);
    }
    if by_desk {
        let relayed = event
            .meta
            .get(meta::RELAYED_FROM)
            .and_then(Value::as_u64)
            .map(Seq);
        for root in event.thread.into_iter().chain(relayed) {
            if open.get(&root).is_some_and(|item| {
                !item.is_hosted_approval(people)
                    && !(crate::worker::is_question(item) && crate::is_provisional(event))
            }) {
                open.remove(&root);
            }
        }
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
            owner: Some(Name::new("alice").unwrap()),
            candidates: vec!["claude".parse().unwrap(), "codex".parse().unwrap()],
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

    /// A root question put to a child Senate is the office's to settle
    /// while the child is offline, until a line says so, the child
    /// replies, or the thread is archived.
    #[test]
    fn a_question_for_a_child_senate_is_tracked_until_something_settles_it() {
        let child = Name::new("north-desk").unwrap();
        let desk = Address::child(child.clone(), c());
        let asked = |seq: u64| {
            at(
                seq,
                NewEvent::new(c(), Kind::Question, Address::Speaker, "is the cap 3?")
                    .to(desk.clone())
                    .on_behalf_of(addr("senator::alice")),
            )
        };
        let mut docket = SpeakerDocket::new();
        docket.observe(&asked(1));
        docket.observe(&asked(2));
        docket.observe(&asked(3));
        docket.observe(&asked(4));
        // A note or a request is not answered for anyone: never tracked.
        docket.observe(&at(
            5,
            NewEvent::new(c(), Kind::Note, addr("senator::alice"), "fyi").to(desk.clone()),
        ));
        let open = |docket: &SpeakerDocket| -> Vec<u64> {
            docket.of_children().map(|(event, _)| event.seq.0).collect()
        };
        assert_eq!(open(&docket), [1, 2, 3, 4]);
        assert!(docket.items().is_empty(), "not addressed to the Speaker");
        assert_eq!(docket.of_children().next().unwrap().1, &child);

        // The office's line, held or answered.
        docket.observe(&at(
            6,
            NewEvent::new(c(), Kind::Note, Address::Speaker, "held")
                .in_thread(Seq(1))
                .with_meta(meta::CHILD_OFFLINE, serde_json::json!("north-desk")),
        ));
        // The child's own reply.
        docket.observe(&at(
            7,
            NewEvent::new(c(), Kind::Answer, desk.clone(), "yes").in_thread(Seq(2)),
        ));
        // Anything else in the thread settles nothing.
        docket.observe(&at(
            8,
            NewEvent::new(c(), Kind::Note, addr("senator::alice"), "ping").in_thread(Seq(3)),
        ));
        assert_eq!(open(&docket), [3, 4]);
        let archived = StatusPayload::new(StatusState::Archived);
        docket.observe(&at(
            9,
            NewEvent::new(c(), Kind::Status, Address::Speaker, "")
                .in_thread(Seq(4))
                .with_meta(meta::STATUS, archived.value()),
        ));
        assert_eq!(open(&docket), [3]);
    }

    #[test]
    fn the_lines_for_an_offline_child_senate_read_in_both_languages() {
        let child = Name::new("north-desk").unwrap();
        assert_eq!(
            child_offline_held(Locale::Zh, &child, chrono::Duration::minutes(5)),
            "[north-desk] 目前离线（最后一次连接在 5 分钟前）。这条消息已保留，它重新连接后会收到。"
        );
        assert_eq!(
            child_offline_held(Locale::En, &child, chrono::Duration::minutes(90)),
            "[north-desk] is offline (last seen 1 hour ago). This is held for it and reaches \
             it when it reconnects."
        );
        assert_eq!(
            child_offline_answer(Locale::En, &child, " 30 days. "),
            "[north-desk] is offline; the Speaker's office answers for it below. It still gets \
             the question when it reconnects.\n30 days."
        );
    }

    /// A desk item brought down from the Senate above leaves the docket
    /// when an answer from the same thread above lands under it; an answer
    /// from another thread, or a note, leaves it.
    #[test]
    fn an_item_from_above_leaves_when_it_was_answered_there() {
        let origin = |thread: u64| {
            crate::locator::Origin {
                locator: "/acme::refund-v2::senator::carol".parse().unwrap(),
                thread: Seq(thread),
            }
            .value()
        };
        let item = |seq: u64, thread: u64| {
            at(
                seq,
                NewEvent::new(c(), Kind::Question, Address::Committee, "is the cap 3?")
                    .to(Address::Speaker)
                    .with_meta(meta::ORIGIN, origin(thread)),
            )
        };
        let reply = |seq: u64, kind: Kind, root: u64, thread: u64| {
            at(
                seq,
                NewEvent::new(c(), kind, Address::Committee, "3, per the brief")
                    .to(Address::Speaker)
                    .in_thread(Seq(root))
                    .with_meta(meta::ORIGIN, origin(thread)),
            )
        };
        let mut docket = SpeakerDocket::new();
        docket.observe(&item(1, 7));
        docket.observe(&item(2, 9));
        docket.observe(&item(3, 11));
        // Answered above, in the thread the item came from.
        docket.observe(&reply(4, Kind::Answer, 1, 7));
        // A later note from above on the same thread is not an answer.
        docket.observe(&reply(5, Kind::Note, 2, 9));
        // An answer that names another thread above is not this item's.
        docket.observe(&reply(6, Kind::Answer, 3, 12));
        let open: Vec<u64> = docket.items().iter().map(|i| i.event.seq.0).collect();
        assert_eq!(open, [2, 3]);
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
            NewEvent::new(c(), Kind::Note, addr("senator::codex"), "hm").in_thread(Seq(1)),
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
            .to(addr("senator::codex"))
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
                NewEvent::new(c(), Kind::Question, addr("senator::codex"), "status?")
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
                .to(addr("senator::codex"))
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
            speaker: Some(SpeakerSeat::Senator("claude".parse().unwrap())),
            office: None,
            items: vec![DocketItem {
                event: pick(1),
                awaiting: Awaiting::Away {
                    agents: vec!["codex".parse().unwrap()],
                },
                intended: Some("ledger::senator::codex".parse().unwrap()),
            }],
            covered: [(Seq(7), "claude".parse().unwrap())].into_iter().collect(),
            seats: vec![SeatFacts {
                paused: false,
                agent: "codex".parse().unwrap(),
                connected: false,
                presence: Some(PresenceState::Absent),
                last_seen: None,
                checkpoint: None,
                open_threads: 1,
                duty: Some("reviews".into()),
            }],
            head: Seq(1),
        };
        let value = serde_json::to_value(&page).unwrap();
        assert_eq!(value["items"][0]["why"], "away");
        assert_eq!(value["covered"]["7"], "claude");
        assert_eq!(serde_json::from_value::<DocketPage>(value).unwrap(), page);
    }
}
