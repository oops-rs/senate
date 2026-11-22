//! Digests of a committee's discussion, and its memory points.
//!
//! # What counts as discussion
//!
//! What people said in the committee's groups (`chat` with a platform
//! source), and what crossed into this log from another committee or Senate
//! ([`meta::ORIGIN`]) or came up from a child Senate (written as it). A simulated message ([`Event::is_simulated`]) never
//! counts. Only events in this committee's own log are read, so a parent
//! Senate's digests see a child's discussion only as it crossed up, and a
//! child's own Speaker digests the rest into the child's memory.
//!
//! # When
//!
//! - **Daily**, at `[speaker.memory] digest_at` in the committee's timezone
//!   ([`SpeakerConfig::zone_of`](crate::SpeakerConfig::zone_of)).
//! - **When a busy stretch goes quiet**: at least `busy_messages` messages
//!   since the last digest, from two speakers or more, then no new one for
//!   `quiet_mins`. The "topic" is that stretch; the Brain separates topics
//!   within its points.
//!
//! Either is skipped when nothing was said since the last digest's
//! `through`. One digest is drafted at a time per committee.
//!
//! # What is written
//!
//! A Speaker `note` with [`meta::DIGEST`], then one with [`meta::MEMORY`]
//! per memory point. Ids are derived from the committee and the digest's
//! `through`, so a replay writes nothing twice. None of them is mirrored.
//!
//! The Brain's output is checked before anything is written
//! ([`checked`]): every cited seq must be in the input it was given (which
//! is also what keeps another Senate's content out), a digest has at most
//! [`MAX_DIGEST_POINTS`] points, a point supersedes only points in force,
//! a point that would replace one a person wrote (a correction) is dropped
//! (only a person replaces it), and a point citing a seq that a person's
//! deletion covered is dropped, so a deletion stands.
//!
//! # Restart
//!
//! What was said since the last digest is folded from the log
//! ([`Discussion::observe`]). On load the daily timer is armed from the
//! clock and the quiet one from the last message's `ts`; timers are not
//! stored.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate_core::memory::{MAX_DIGEST_POINTS, MemoryFold, one_line};
use senate_core::{DigestPayload, DigestPoint, DigestTrigger, MemoryPayload};

use super::{
    Address, CommitteeId, Engine, Event, Kind, NewEvent, PAGE, STALL_RETRY, Seq, ThoughtKind,
    Timer, debug, derived_event_id, meta, warn,
};
use crate::actor::SpeakerMsg;
use crate::brain::{BrainError, DigestInput, DigestOut, MemoryDraft};
use crate::render;

/// Most discussion events one digest is drafted from (the latest ones).
pub(super) const DIGEST_WINDOW: usize = 200;
/// Most memory points one digest adds.
const MAX_NEW_POINTS: usize = 7;

/// One committee's discussion since its last digest, and the timers that
/// digest it.
#[derive(Default)]
pub(super) struct Discussion {
    /// What was said since the last digest: seq → (who, when).
    since: BTreeMap<Seq, (Address, DateTime<Utc>)>,
    /// The last digest's `through`.
    through: Seq,
    daily: Option<Timer>,
    /// The quiet timer, armed after the message at this seq.
    quiet: Option<(Seq, Timer)>,
    drafting: bool,
}

impl Discussion {
    /// Idempotent, like everything a committee state observes.
    pub(super) fn observe(&mut self, event: &Event) {
        if let Some(digest) = DigestPayload::of(event) {
            self.through = self.through.max(digest.through);
            let through = self.through;
            self.since.retain(|seq, _| *seq > through);
            return;
        }
        if is_discussion(event) && event.seq > self.through {
            self.since.insert(event.seq, (principal(event), event.ts));
        }
    }

    fn busy(&self, busy_messages: usize) -> bool {
        let speakers: BTreeSet<&Address> = self.since.values().map(|(who, _)| who).collect();
        self.since.len() >= busy_messages && speakers.len() >= 2
    }

    fn last(&self) -> Option<(Seq, DateTime<Utc>)> {
        self.since
            .iter()
            .next_back()
            .map(|(seq, (_, ts))| (*seq, *ts))
    }

    pub(super) fn shutdown(&mut self) {
        if let Some(timer) = self.daily.take() {
            timer.abort();
        }
        if let Some((_, timer)) = self.quiet.take() {
            timer.abort();
        }
    }
}

/// People's messages in the committee's groups, and what crossed in from
/// elsewhere; never a simulated one.
pub(super) fn is_discussion(event: &Event) -> bool {
    if event.is_simulated() {
        return false;
    }
    match event.kind {
        Kind::Chat => event.source.is_some(),
        // What crossed in: from another committee or Senate, or a child
        // Senate's answer coming up.
        Kind::Question | Kind::Answer | Kind::Note | Kind::Request => {
            event.meta.contains_key(meta::ORIGIN) || matches!(event.from, Address::Child { .. })
        }
        _ => false,
    }
}

fn principal(event: &Event) -> Address {
    event
        .on_behalf_of
        .clone()
        .unwrap_or_else(|| event.from.clone())
}

/// What may be written of `out`, given the seqs the Brain was shown and the
/// memory as it stands. See the module docs.
pub(super) fn checked(
    out: DigestOut,
    shown: &BTreeSet<Seq>,
    memory: &MemoryFold,
) -> (Vec<DigestPoint>, Vec<MemoryDraft>) {
    let cite = |refs: Vec<Seq>| -> Vec<Seq> {
        let mut kept: Vec<Seq> = refs.into_iter().filter(|r| shown.contains(r)).collect();
        kept.sort();
        kept.dedup();
        kept
    };
    let points: Vec<DigestPoint> = out
        .points
        .into_iter()
        .filter_map(|p| {
            let text = one_line(&p.text);
            let refs = cite(p.refs);
            (!text.is_empty() && !refs.is_empty()).then_some(DigestPoint { text, refs })
        })
        .take(MAX_DIGEST_POINTS)
        .collect();
    let known: BTreeSet<String> = memory.in_force().map(|p| p.text.to_lowercase()).collect();
    let drafts: Vec<MemoryDraft> = out
        .memory
        .into_iter()
        .filter_map(|draft| {
            let text = one_line(&draft.text);
            let refs = cite(draft.refs);
            if text.is_empty()
                || refs.is_empty()
                || known.contains(&text.to_lowercase())
                || refs.iter().any(|r| memory.is_deleted_ref(*r))
            {
                return None;
            }
            // A person's word stands: a point that would replace one a
            // person wrote is not written at all.
            let by_person = |seq: &Seq| {
                memory.is_in_force(*seq)
                    && memory.get(*seq).is_some_and(|hit| hit.point.by_person())
            };
            if draft.supersedes.iter().any(by_person) {
                return None;
            }
            let mut supersedes: Vec<Seq> = draft
                .supersedes
                .into_iter()
                .filter(|seq| memory.is_in_force(*seq))
                .collect();
            supersedes.sort();
            supersedes.dedup();
            Some(MemoryDraft {
                text,
                refs,
                supersedes,
            })
        })
        .take(MAX_NEW_POINTS)
        .collect();
    (points, drafts)
}

impl Engine {
    /// After loading a committee: the daily digest from the clock, the
    /// quiet one from the last message.
    pub(super) fn arm_digests(&mut self, committee: &CommitteeId) {
        self.arm_daily(committee);
        self.arm_quiet(committee);
    }

    fn arm_daily(&mut self, committee: &CommitteeId) {
        let zone = self.config.zone_of(committee);
        let delay = zone.until(Utc::now(), self.config.memory.digest_time());
        let Some(state) = self.committees.get_mut(committee) else {
            return;
        };
        if let Some(old) = state.discussion.daily.take() {
            old.abort();
        }
        let message = committee.clone();
        state.discussion.daily =
            Some(
                self.myself
                    .send_after(delay, move || SpeakerMsg::DigestDue {
                        committee: message,
                        trigger: DigestTrigger::Daily,
                    }),
            );
    }

    /// After a message of discussion: when the stretch since the last
    /// digest is busy, digest it once it has been quiet for `quiet_mins`.
    pub(super) fn arm_quiet(&mut self, committee: &CommitteeId) {
        let settings = self.config.memory.clone();
        let Some(state) = self.committees.get_mut(committee) else {
            return;
        };
        let discussion = &mut state.discussion;
        if !discussion.busy(settings.busy_messages) {
            return;
        }
        let Some((seq, ts)) = discussion.last() else {
            return;
        };
        if discussion
            .quiet
            .as_ref()
            .is_some_and(|(armed, _)| *armed == seq)
        {
            return;
        }
        let quiet = settings.quiet();
        let since = (Utc::now() - ts).to_std().unwrap_or(Duration::ZERO);
        let delay = quiet.saturating_sub(since);
        if let Some((_, old)) = discussion.quiet.take() {
            old.abort();
        }
        let message = committee.clone();
        let timer = self
            .myself
            .send_after(delay, move || SpeakerMsg::DigestDue {
                committee: message,
                trigger: DigestTrigger::Quiet,
            });
        discussion.quiet = Some((seq, timer));
    }

    /// A digest is due: draft it when there is something new to digest.
    pub(crate) async fn on_digest_due(&mut self, committee: CommitteeId, trigger: DigestTrigger) {
        self.catch_up(&committee).await;
        if trigger == DigestTrigger::Daily {
            self.arm_daily(&committee); // tomorrow's
        }
        let busy_messages = self.config.memory.busy_messages;
        let Some(state) = self.committees.get_mut(&committee) else {
            return;
        };
        if trigger == DigestTrigger::Quiet {
            let armed = state.discussion.quiet.take().map(|(seq, _)| seq);
            if armed != state.discussion.last().map(|(seq, _)| seq) {
                // Something was said since: its own timer is armed.
                return;
            }
            if !state.discussion.busy(busy_messages) {
                return;
            }
        }
        if state.discussion.since.is_empty() || state.discussion.drafting {
            debug!(%committee, ?trigger, "no digest: nothing new, or one is being drafted");
            return;
        }
        if state.paused() {
            let message = committee.clone();
            self.myself
                .send_after(STALL_RETRY, move || SpeakerMsg::DigestDue {
                    committee: message,
                    trigger,
                });
            return;
        }
        let through = state.discussion.through;
        let (memory, decisions) = match &state.brief {
            Some(brief) => (
                brief.memory().in_force().cloned().collect(),
                brief.brief().decisions,
            ),
            None => (Vec::new(), Vec::new()),
        };
        let events = match self.discussion_after(&committee, through).await {
            Ok(events) => events,
            Err(error) => {
                warn!(%committee, %error, "cannot read the discussion; no digest this time");
                return;
            }
        };
        let (Some(first), Some(last)) = (events.first(), events.last()) else {
            return;
        };
        let (from, through) = (first.seq, last.seq);
        let shown: BTreeSet<Seq> = events.iter().map(|e| e.seq).collect();
        let input = DigestInput {
            committee: committee.clone(),
            locale: self.locale(&committee),
            trigger,
            events,
            memory,
            decisions,
        };
        if let Some(state) = self.committees.get_mut(&committee) {
            state.discussion.drafting = true;
        }
        let brain = std::sync::Arc::clone(&self.brain);
        self.think(async move { brain.digest(input).await }, move |out| {
            ThoughtKind::Digested {
                committee,
                trigger,
                from,
                through,
                shown,
                out,
            }
        });
    }

    /// The discussion after `through` in `committee`'s log, the latest
    /// [`DIGEST_WINDOW`] of it, oldest first.
    async fn discussion_after(
        &self,
        committee: &CommitteeId,
        through: Seq,
    ) -> Result<Vec<Event>, senate_core::HubError> {
        let mut events = Vec::new();
        let mut after = through;
        loop {
            let page = self.hub.read(committee, after, PAGE).await?;
            let Some(last) = page.last() else { break };
            after = last.seq;
            events.extend(page.into_iter().filter(is_discussion));
            if events.len() > DIGEST_WINDOW {
                events.drain(..events.len() - DIGEST_WINDOW);
            }
        }
        Ok(events)
    }

    /// A drafted digest: check it, then write it and its memory points.
    pub(super) async fn on_digested(
        &mut self,
        committee: CommitteeId,
        trigger: DigestTrigger,
        (from, through): (Seq, Seq),
        shown: BTreeSet<Seq>,
        out: Result<DigestOut, BrainError>,
    ) {
        let Some(state) = self.committees.get_mut(&committee) else {
            return;
        };
        state.discussion.drafting = false;
        let out = match out {
            Ok(out) => out,
            Err(error) => {
                warn!(%committee, %error, "the brain could not digest the discussion; next time");
                return;
            }
        };
        let empty = MemoryFold::new();
        let memory = state.brief.as_ref().map_or(&empty, |b| b.memory());
        let (points, drafts) = checked(out, &shown, memory);
        if points.is_empty() {
            warn!(%committee, "the digest had no usable point; next time");
            return;
        }
        let payload = DigestPayload {
            trigger,
            from,
            through,
            points,
        };
        let key = through.to_string();
        let digest = NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::Speaker,
            render::digest_body(&payload.points),
        )
        .with_id(derived_event_id(&["digest", committee.as_str(), &key]))
        .with_meta(meta::DIGEST, payload.to_value());
        let digest_seq = match self.append(digest).await {
            Ok(Some(event)) => event.seq,
            Ok(None) => return,
            Err(stall) => {
                warn!(%committee, error = %stall, "cannot write the digest; next time");
                return;
            }
        };
        for (i, draft) in drafts.into_iter().enumerate() {
            let point = NewEvent::new(committee.clone(), Kind::Note, Address::Speaker, draft.text)
                .with_id(derived_event_id(&[
                    "memory",
                    committee.as_str(),
                    &key,
                    &i.to_string(),
                ]))
                .with_meta(
                    meta::MEMORY,
                    MemoryPayload::Point {
                        refs: draft.refs,
                        supersedes: draft.supersedes,
                        digest: Some(digest_seq),
                    }
                    .to_value(),
                );
            if let Err(stall) = self.append(point).await {
                warn!(%committee, error = %stall, "cannot write a memory point; skipping it");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use senate_core::{CommitteeId, EventId};

    fn ev(seq: u64, from: &str, body: &str, key: &str, value: serde_json::Value) -> Event {
        let mut meta = serde_json::Map::new();
        meta.insert(key.into(), value);
        Event {
            seq: Seq(seq),
            committee: CommitteeId::new("orchard").unwrap(),
            id: EventId::new(),
            ts: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            kind: Kind::Note,
            from: from.parse().unwrap(),
            to: Vec::new(),
            thread: None,
            on_behalf_of: None,
            source: None,
            body: body.into(),
            refs: None,
            mirror: None,
            meta,
        }
    }

    fn point(seq: u64, body: &str, refs: &[u64]) -> Event {
        ev(
            seq,
            "speaker",
            body,
            meta::MEMORY,
            MemoryPayload::Point {
                refs: refs.iter().copied().map(Seq).collect(),
                supersedes: vec![],
                digest: None,
            }
            .to_value(),
        )
    }

    fn draft(text: &str, refs: &[u64], supersedes: &[u64]) -> MemoryDraft {
        MemoryDraft {
            text: text.into(),
            refs: refs.iter().copied().map(Seq).collect(),
            supersedes: supersedes.iter().copied().map(Seq).collect(),
        }
    }

    #[test]
    fn only_what_holds_is_written() {
        let mut memory = MemoryFold::new();
        memory.observe(&point(20, "Deploys at noon", &[1]));
        memory.observe(&point(21, "Old staging host", &[2]));
        let corrected = ev(
            22,
            "senator::ana",
            "Reviews need two approvals",
            meta::MEMORY,
            MemoryPayload::Correct {
                target: Seq(21),
                refs: vec![],
            }
            .to_value(),
        );
        memory.observe(&corrected);
        memory.observe(&point(23, "The cache is shared", &[4]));
        let delete = ev(
            24,
            "senator::ana",
            "",
            meta::MEMORY,
            MemoryPayload::Delete {
                targets: vec![Seq(23)],
            }
            .to_value(),
        );
        memory.observe(&delete);

        let shown: BTreeSet<Seq> = [5, 6, 7, 4].into_iter().map(Seq).collect();
        let mut points: Vec<DigestPoint> = (0..9)
            .map(|i| DigestPoint {
                text: format!("point {i}"),
                refs: vec![Seq(5)],
            })
            .collect();
        points.insert(
            0,
            DigestPoint {
                text: "cites only what it was not shown".into(),
                refs: vec![Seq(99)],
            },
        );
        let out = DigestOut {
            points,
            memory: vec![
                draft("Deploys at night", &[6, 99], &[20]),
                draft("Reviews need one approval", &[7], &[22]),
                draft("A seq from another Senate", &[1234], &[]),
                draft("The cache is shared again", &[4], &[]),
                draft("deploys at noon", &[5], &[]),
            ],
        };
        let (points, drafts) = checked(out, &shown, &memory);
        assert_eq!(points.len(), MAX_DIGEST_POINTS);
        assert_eq!(points[0].text, "point 0");
        assert_eq!(
            drafts,
            // A person's correction is never replaced by the Speaker.
            [draft("Deploys at night", &[6], &[20])]
        );
    }

    #[test]
    fn simulated_chat_is_not_discussion() {
        let mut chat = ev(1, "senator::ana", "hi", "x", serde_json::Value::Null);
        chat.kind = Kind::Chat;
        chat.meta.clear();
        assert!(!is_discussion(&chat), "no platform source");
        chat.source = Some(senate_core::Source {
            platform: "chat".into(),
            group_id: "g".into(),
            group_name: None,
            simulated: false,
            simulated_by: None,
        });
        assert!(is_discussion(&chat));
        chat.source.as_mut().unwrap().simulated = true;
        assert!(!is_discussion(&chat));
    }
}
