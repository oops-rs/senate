//! Reads the app makes on demand and shows as they come back, outside the
//! [`crate::state::AppState`] snapshot: one thread's events. Like the
//! snapshot, every sentence is written in the app's language and every id
//! as a bracketed mention.

use std::time::SystemTime;

use senate_bridge::ipc::ThreadReply;
use senate_core::{Address, Event, Locale, Name};

use crate::text;

/// One thread, root first.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct ThreadView {
    pub committee: String,
    /// The root's seq.
    pub root: u64,
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

pub(crate) fn thread_view(reply: ThreadReply, person: Option<&Name>, locale: Locale) -> ThreadView {
    let me = person.map(Address::senior);
    ThreadView {
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
