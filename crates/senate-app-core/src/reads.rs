//! Reads the app makes on demand and shows as they come back, outside the
//! [`crate::state::AppState`] snapshot: one thread's events. Like the
//! snapshot, every sentence is written in the app's language and every id
//! as a bracketed mention.

use std::time::SystemTime;

use senate_bridge::ipc::{DeskItemDetail, DeskOutcome, ThreadReply};
use senate_core::{Address, Event, Locale, Name};

use crate::text;

/// One thread, root first.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ThreadView {
    pub committee: String,
    /// The root's seq.
    pub root: u64,
    /// Its title, as its task board folds it (else the root's first line):
    /// what the page heads with when the thread is not in the inbox.
    pub title: String,
    /// Its state (`processing`, `suspended`, `active`, `archived`) and
    /// that in words, when it is a task thread.
    pub state: Option<String>,
    pub state_label: Option<String>,
    /// What kind its root is, in words.
    pub kind_label: String,
    pub events: Vec<ThreadEvent>,
}

/// One event in a thread.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ThreadEvent {
    pub seq: u64,
    /// The wire spelling (`question`, `answer`, `status`).
    pub kind: String,
    pub kind_label: String,
    /// Who wrote it, as shown to people (`[speaker::claude] takeover`).
    pub by: String,
    /// Who it was addressed to.
    pub to: Vec<String>,
    /// What was said, as written.
    pub body: String,
    pub at: SystemTime,
    /// The person wrote it (`senator::<person>`): the app's own side.
    pub mine: bool,
}

#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct DeskItemView {
    pub event_id: String,
    pub item: ThreadEvent,
    pub thread: ThreadView,
    pub origin: Option<String>,
    pub destination: Option<String>,
    pub destination_label: Option<String>,
    pub blocked: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct DeskOutcomeView {
    pub delivered: bool,
    pub destination: String,
    pub destination_label: String,
    pub committee: String,
    pub seq: u64,
}

impl DeskOutcomeView {
    pub(crate) fn new(value: DeskOutcome, locale: Locale) -> Self {
        let delivered = matches!(value, DeskOutcome::Delivered { .. });
        let (destination, committee, seq) = match value {
            DeskOutcome::Delivered {
                destination,
                committee,
                seq,
            }
            | DeskOutcome::Refused {
                destination,
                committee,
                seq,
            } => (destination, committee, seq),
        };
        Self {
            destination_label: crate::names::current().desk_destination(&destination, locale),
            delivered,
            destination,
            committee: committee.to_string(),
            seq: seq.0,
        }
    }
}

pub(crate) fn desk_item_view(
    detail: DeskItemDetail,
    person: Option<&Name>,
    locale: Locale,
) -> DeskItemView {
    let me = person.map(Address::senior);
    DeskItemView {
        event_id: detail.item.id.to_string(),
        item: thread_event(&detail.item, me.as_ref(), locale),
        thread: thread_view(
            ThreadReply {
                committee: detail.item.committee,
                root: detail.item.thread.unwrap_or(detail.item.seq),
                events: detail.events,
            },
            person,
            locale,
        ),
        origin: detail.origin,
        destination_label: detail
            .destination
            .as_deref()
            .map(|raw| crate::names::current().desk_destination(raw, locale)),
        destination: detail.destination,
        blocked: detail.blocked,
    }
}

pub(crate) fn thread_view(reply: ThreadReply, person: Option<&Name>, locale: Locale) -> ThreadView {
    let me = person.map(Address::senior);
    let mut board = senate_core::TaskBoard::new(crate::names::current().people.clone());
    for e in &reply.events {
        board.observe(e);
    }
    let summary = board.get(reply.root);
    let root = reply.events.iter().find(|e| e.seq == reply.root);
    ThreadView {
        title: summary.map_or_else(
            || {
                root.map(|e| e.body.lines().next().unwrap_or_default().trim().to_string())
                    .unwrap_or_default()
            },
            |t| t.title.clone(),
        ),
        state: summary.map(|t| crate::state::wire(&t.state)),
        state_label: summary.map(|t| text::state(locale, t.state).to_string()),
        kind_label: root.map_or_else(String::new, |e| text::kind(locale, &e.kind)),
        committee: reply.committee.to_string(),
        root: reply.root.0,
        events: reply
            .events
            .iter()
            .map(|e| thread_event(e, me.as_ref(), locale))
            .collect(),
    }
}

fn thread_event(e: &Event, me: Option<&Address>, locale: Locale) -> ThreadEvent {
    ThreadEvent {
        seq: e.seq.0,
        kind: e.kind.as_str().to_string(),
        kind_label: text::kind(locale, &e.kind),
        by: e.byline(locale).unwrap_or_else(|| e.from.in_text()),
        to: e.to.iter().map(Address::in_text).collect(),
        body: e.body.clone(),
        at: e.ts.into(),
        mine: me.is_some_and(|m| *m == e.from && e.on_behalf_of.is_none()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Utc;
    use senate_core::{CommitteeId, EventId, Kind, SenatorName, Seq};

    fn event(seq: u64, kind: Kind, from: Address, body: &str) -> Event {
        Event {
            seq: Seq(seq),
            committee: CommitteeId::new("refund-v2").unwrap(),
            id: EventId::new(),
            ts: Utc::now(),
            kind,
            from,
            to: Vec::new(),
            thread: (seq != 1).then_some(Seq(1)),
            on_behalf_of: None,
            source: None,
            body: body.into(),
            refs: None,
            mirror: None,
            meta: Default::default(),
        }
    }

    #[test]
    fn a_thread_is_its_events_with_bylines_and_the_persons_own_marked() {
        let alice = Name::new("alice").unwrap();
        let claude: SenatorName = "claude".parse().unwrap();
        let reply = ThreadReply {
            committee: CommitteeId::new("refund-v2").unwrap(),
            root: Seq(1),
            events: vec![
                event(1, Kind::Question, Address::senator(&claude), "which env?"),
                event(2, Kind::Answer, Address::senior(&alice), "staging"),
            ],
        };
        let view = thread_view(reply, Some(&alice), Locale::En);
        assert_eq!(view.root, 1);
        assert_eq!(view.events.len(), 2);
        assert_eq!(view.events[0].by, "[senator::claude]");
        assert_eq!(view.events[0].kind_label, "question");
        assert!(!view.events[0].mine);
        assert!(view.events[1].mine);
        assert_eq!(view.events[1].body, "staging");

        let zh = thread_view(
            ThreadReply {
                committee: CommitteeId::new("refund-v2").unwrap(),
                root: Seq(1),
                events: vec![event(1, Kind::Question, Address::senator(&claude), "?")],
            },
            None,
            Locale::Zh,
        );
        assert_eq!(zh.events[0].kind_label, "问题");
    }
}
