//! `catch_up`: what an agent reads when it asks what it missed.
//!
//! The `read` cursor is the agent's own (`?senator=<agent>`): two agents of
//! one person catch up separately. The daemon fetches up to `limit` events
//! after it
//! (in `seq` order, so nothing is skipped when `read` advances to the last
//! one). Within that page, events addressed to the agent come first and in
//! full; decisions stay in full; everything else is cut to
//! [`TRUNCATE_AT`] characters.

use senate_core::{Address, Event, Kind};
use serde::{Deserialize, Serialize};

use crate::format;

pub const DEFAULT_LIMIT: usize = 50;
pub const TRUNCATE_AT: usize = 300;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatchUpItem {
    /// The event, with `body` already cut when `truncated`.
    pub event: Event,
    /// Addressed to this agent.
    pub directed: bool,
    pub truncated: bool,
}

/// Order and trim one page of events for `me`.
pub fn arrange(events: Vec<Event>, me: &Address) -> Vec<CatchUpItem> {
    let (mut directed, rest): (Vec<Event>, Vec<Event>) =
        events.into_iter().partition(|e| e.is_directed_to(me));
    directed.sort_by_key(|e| e.seq);
    let mut out: Vec<CatchUpItem> = directed
        .into_iter()
        .map(|event| CatchUpItem {
            event,
            directed: true,
            truncated: false,
        })
        .collect();
    let mut rest = rest;
    rest.sort_by_key(|e| e.seq);
    out.extend(rest.into_iter().map(|mut event| {
        let truncated = if event.kind == Kind::Decision {
            false
        } else {
            let (body, cut) = format::truncate_chars(&event.body, TRUNCATE_AT);
            event.body = body;
            cut
        };
        CatchUpItem {
            event,
            directed: false,
            truncated,
        }
    }));
    out
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use senate_core::{CommitteeId, NewEvent, Seq};

    use super::*;

    #[test]
    fn directed_first_decisions_whole_rest_cut() {
        let f = CommitteeId::new("refund-v2").unwrap();
        let me: Address = "senator::claude".parse().unwrap();
        let sibling: Address = "senator::codex".parse().unwrap();
        let long = "y".repeat(1000);
        let ev = |seq, kind, to: Option<&Address>| {
            let mut e = NewEvent::new(f.clone(), kind, Address::Speaker, long.clone());
            if let Some(a) = to {
                e = e.to(a.clone());
            }
            e.into_event(Seq(seq), Utc::now())
        };
        let page = vec![
            ev(1, Kind::Chat, None),
            ev(2, Kind::Question, Some(&me)),
            ev(3, Kind::Decision, None),
            ev(4, Kind::Note, Some(&me)),
            ev(5, Kind::Answer, None),
            ev(6, Kind::Question, Some(&sibling)),
        ];
        let out = arrange(page, &me);
        let seqs: Vec<u64> = out.iter().map(|i| i.event.seq.0).collect();
        // What the person's other agent was sent is not addressed to me.
        assert_eq!(seqs, [2, 4, 1, 3, 5, 6]);
        assert!(!out[5].directed && out[5].truncated);
        assert!(out[0].directed && !out[0].truncated && out[0].event.body.len() == 1000);
        assert!(out[1].directed && !out[1].truncated);
        assert!(!out[2].directed && out[2].truncated);
        assert_eq!(out[2].event.body.chars().count(), TRUNCATE_AT + 1);
        assert!(!out[3].truncated && out[3].event.body.len() == 1000);
        assert!(out[4].truncated);
    }
}
