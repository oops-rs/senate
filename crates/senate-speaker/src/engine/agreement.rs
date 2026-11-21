//! Decisions proposed from discussion.
//!
//! When people in a group agree on something concrete, the Speaker asks
//! there whether to record it: 「要记成决定吗？」/ "Record this as a
//! decision?". A person's yes records it as an ordinary decision
//! (`confirm_proposal`, as for an agent's proposal), with the messages it
//! came from in [`meta::SOURCES`]. The Speaker only proposes; people decide.
//!
//! # When
//!
//! A burst is a group's run of messages (never simulated ones) with gaps
//! under [`BURST_GAP`]. Once a burst has been quiet for `[speaker.memory]
//! agreement_quiet_secs`, has something new since it was last checked, and
//! two people or more of the roster spoke in it, [`Brain::spot_agreement`]
//! is asked. Its answer is written only when it holds:
//!
//! - every cited seq is a message of the burst;
//! - two people or more of the roster agreed, each the author of a cited
//!   message;
//! - its topic was not proposed before on the committee, and none of its
//!   messages was cited by an earlier proposal: at most once per topic.
//!
//! # What is written
//!
//! A Speaker root `note` with [`meta::PROPOSAL`] = `decision`,
//! [`meta::SOURCES`] and [`meta::TOPIC`], carrying the group's source. It is
//! posted as a new root in that group, so a reply in its thread is the
//! answer. Earlier proposals are folded from the log; the timers and what
//! was checked are not kept across a restart (a burst is checked again at
//! most once after one, and the topic rule still holds).
//!
//! [`Brain::spot_agreement`]: crate::Brain::spot_agreement

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate_core::memory::one_line;
use senate_core::{PROPOSAL_DECISION, People, Source};
use serde_json::{Value, json};

use super::{
    Address, CommitteeId, Engine, Event, Kind, NewEvent, PAGE, Seq, ThoughtKind, Timer, debug,
    derived_event_id, meta, warn,
};
use crate::actor::SpeakerMsg;
use crate::brain::{Agreement, AgreementInput, BrainError};

/// Messages further apart than this are different bursts.
pub(super) const BURST_GAP: Duration = Duration::from_secs(30 * 60);
/// Most messages of a burst the Brain is shown (the latest).
const BURST_MESSAGES: usize = 60;
/// How far back in the log a burst is looked for.
const BURST_LOOKBACK: u64 = 400;

/// One group's latest message of discussion.
struct GroupBurst {
    last: Seq,
    ts: DateTime<Utc>,
    /// The latest message a check has covered.
    checked: Seq,
    timer: Option<(Seq, Timer)>,
}

/// The committee's bursts by group, and what was proposed before.
#[derive(Default)]
pub(super) struct Bursts {
    groups: HashMap<String, GroupBurst>,
    topics: BTreeSet<String>,
    cited: BTreeSet<Seq>,
}

impl Bursts {
    pub(super) fn observe(&mut self, event: &Event) {
        if is_discussion_proposal(event) {
            if let Some(topic) = event.meta.get(meta::TOPIC).and_then(Value::as_str) {
                self.topics.insert(normalized(topic));
            }
            self.cited.extend(sources(event));
            return;
        }
        if let Some(group) = burst_group(event) {
            let entry = self.groups.entry(group.to_owned()).or_insert(GroupBurst {
                last: Seq::ZERO,
                ts: event.ts,
                checked: Seq::ZERO,
                timer: None,
            });
            if event.seq > entry.last {
                entry.last = event.seq;
                entry.ts = event.ts;
            }
        }
    }

    pub(super) fn shutdown(&mut self) {
        for burst in self.groups.values_mut() {
            if let Some((_, timer)) = burst.timer.take() {
                timer.abort();
            }
        }
    }
}

/// The group a message of discussion was said in: a person's `chat` with a
/// platform source, never a simulated one.
fn burst_group(event: &Event) -> Option<&str> {
    if event.kind != Kind::Chat || event.is_simulated() {
        return None;
    }
    event.source.as_ref().map(|s| s.group_id.as_str())
}

/// The Speaker's proposal of a decision people agreed on.
pub(super) fn is_discussion_proposal(event: &Event) -> bool {
    event.kind == Kind::Note
        && event.from == Address::Speaker
        && event.thread.is_none()
        && event.meta.get(meta::PROPOSAL).and_then(Value::as_str) == Some(PROPOSAL_DECISION)
        && event.meta.contains_key(meta::SOURCES)
}

/// [`meta::SOURCES`] of an event.
pub(super) fn sources(event: &Event) -> Vec<Seq> {
    event
        .meta
        .get(meta::SOURCES)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_u64).map(Seq).collect())
        .unwrap_or_default()
}

fn normalized(topic: &str) -> String {
    one_line(topic).to_lowercase()
}

/// `agreement` as it may be written, given the burst it came from, who is
/// a person, and what was proposed before; `None` when it does not hold.
/// See the module docs.
fn checked(
    agreement: Agreement,
    burst: &[Event],
    people: &People,
    bursts: &Bursts,
) -> Option<Agreement> {
    let statement = one_line(&agreement.statement);
    let topic = one_line(&agreement.topic);
    if statement.is_empty() || topic.is_empty() || bursts.topics.contains(&normalized(&topic)) {
        return None;
    }
    let mut refs: Vec<Seq> = agreement
        .refs
        .into_iter()
        .filter(|r| burst.iter().any(|e| e.seq == *r))
        .collect();
    refs.sort();
    refs.dedup();
    if refs.is_empty() || refs.iter().any(|r| bursts.cited.contains(r)) {
        return None;
    }
    let authors: BTreeSet<&Address> = burst
        .iter()
        .filter(|e| refs.contains(&e.seq))
        .map(|e| &e.from)
        .collect();
    let mut agreeing: Vec<Address> = agreement
        .agreeing
        .into_iter()
        .filter(|who| people.is_person_address(who) && authors.contains(who))
        .collect();
    agreeing.sort_by_key(Address::to_string);
    agreeing.dedup();
    (agreeing.len() >= 2).then_some(Agreement {
        statement,
        topic,
        refs,
        agreeing,
    })
}

impl Engine {
    /// After a message in a group: check its burst once it has been quiet
    /// for `agreement_quiet_secs`.
    pub(super) fn arm_agreement(&mut self, committee: &CommitteeId, event: &Event) {
        let Some(group) = burst_group(event).map(str::to_owned) else {
            return;
        };
        let quiet = self.config.memory.agreement_quiet();
        let Some(state) = self.committees.get_mut(committee) else {
            return;
        };
        let Some(burst) = state.bursts.groups.get_mut(&group) else {
            return;
        };
        let (seq, ts) = (burst.last, burst.ts);
        if burst.timer.as_ref().is_some_and(|(armed, _)| *armed == seq) {
            return;
        }
        if let Some((_, old)) = burst.timer.take() {
            old.abort();
        }
        let since = (Utc::now() - ts).to_std().unwrap_or(Duration::ZERO);
        let message = committee.clone();
        let timer = self
            .myself
            .send_after(quiet.saturating_sub(since), move || {
                SpeakerMsg::AgreementDue {
                    committee: message,
                    group,
                    seq,
                }
            });
        burst.timer = Some((seq, timer));
    }

    /// A burst has gone quiet: ask whether it holds an agreement.
    pub(crate) async fn on_agreement_due(
        &mut self,
        committee: CommitteeId,
        group: String,
        seq: Seq,
    ) {
        self.catch_up(&committee).await;
        let Some(state) = self.committees.get_mut(&committee) else {
            return;
        };
        let Some(burst) = state.bursts.groups.get_mut(&group) else {
            return;
        };
        if burst.last != seq || burst.checked >= seq {
            return; // said more since (its own timer is armed), or checked
        }
        burst.timer = None;
        burst.checked = seq;
        let proposed: Vec<String> = state.bursts.topics.iter().cloned().collect();
        let decisions = state
            .brief
            .as_ref()
            .map(|b| b.brief().decisions)
            .unwrap_or_default();
        let events = match self.burst(&committee, &group, seq).await {
            Ok(events) => events,
            Err(error) => {
                warn!(%committee, %error, "cannot read the burst; not checking it");
                return;
            }
        };
        let people = self.roster.people();
        let speakers: BTreeSet<&Address> = events
            .iter()
            .map(|e| &e.from)
            .filter(|who| people.is_person_address(who))
            .collect();
        if speakers.len() < 2 {
            debug!(%committee, %group, "one person alone agrees with nobody");
            return;
        }
        let input = AgreementInput {
            committee: committee.clone(),
            locale: self.locale(&committee),
            events: events.clone(),
            proposed,
            decisions,
        };
        let brain = std::sync::Arc::clone(&self.brain);
        self.think(
            async move { brain.spot_agreement(input).await },
            move |agreement| ThoughtKind::Spotted {
                committee,
                burst: events,
                agreement,
            },
        );
    }

    /// The burst in `group` ending at `last`, oldest first.
    async fn burst(
        &self,
        committee: &CommitteeId,
        group: &str,
        last: Seq,
    ) -> Result<Vec<Event>, senate_core::HubError> {
        let mut said = Vec::new();
        let mut after = Seq(last.0.saturating_sub(BURST_LOOKBACK));
        while after < last {
            let page = self.hub.read(committee, after, PAGE).await?;
            let Some(tail) = page.last() else { break };
            after = tail.seq;
            said.extend(
                page.into_iter()
                    .filter(|e| e.seq <= last && burst_group(e) == Some(group)),
            );
        }
        let gap = chrono::Duration::from_std(BURST_GAP).unwrap_or_default();
        let mut start = said.len();
        while start > 0
            && said.len() - start < BURST_MESSAGES
            && (start == said.len() || said[start].ts - said[start - 1].ts < gap)
        {
            start -= 1;
        }
        Ok(said.split_off(start))
    }

    /// What the Brain found in a burst: propose it when it holds.
    pub(super) async fn on_spotted(
        &mut self,
        committee: CommitteeId,
        burst: Vec<Event>,
        agreement: Result<Option<Agreement>, BrainError>,
    ) {
        let agreement = match agreement {
            Ok(Some(agreement)) => agreement,
            Ok(None) => return,
            Err(error) => {
                debug!(%committee, %error, "no agreement check this time");
                return;
            }
        };
        let people = self.roster.people();
        let Some(state) = self.committees.get(&committee) else {
            return;
        };
        let Some(agreement) = checked(agreement, &burst, &people, &state.bursts) else {
            debug!(%committee, "the agreement found does not hold; not proposing it");
            return;
        };
        let Some(last) = burst.last() else { return };
        let source: Option<Source> = last.source.clone();
        let mut note = NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::Speaker,
            agreement.statement,
        )
        .with_id(derived_event_id(&[
            "discussion-proposal",
            committee.as_str(),
            &last.seq.to_string(),
        ]))
        .with_meta(meta::PROPOSAL, json!(PROPOSAL_DECISION))
        .with_meta(
            meta::SOURCES,
            json!(agreement.refs.iter().map(|s| s.0).collect::<Vec<_>>()),
        )
        .with_meta(meta::TOPIC, json!(agreement.topic));
        if let Some(source) = source {
            note = note.with_source(source);
        }
        if let Err(stall) = self.append(note).await {
            warn!(%committee, error = %stall, "cannot write the proposal; not proposing it");
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use senate_core::{EventId, Name, Principal, Roster};

    use super::*;

    fn said(seq: u64, from: &str) -> Event {
        Event {
            seq: Seq(seq),
            committee: CommitteeId::new("orchard").unwrap(),
            id: EventId::new(),
            ts: Utc.timestamp_opt(1_700_000_000 + seq as i64, 0).unwrap(),
            kind: Kind::Chat,
            from: from.parse().unwrap(),
            to: Vec::new(),
            thread: None,
            on_behalf_of: None,
            source: None,
            body: String::new(),
            refs: None,
            mirror: None,
            meta: serde_json::Map::new(),
        }
    }

    fn people() -> People {
        Roster::new(
            ["ana", "ben", "cy"]
                .into_iter()
                .map(|h| Principal::new(Name::new(h).unwrap()))
                .collect(),
            Vec::new(),
        )
        .unwrap()
        .people()
    }

    fn agreement(topic: &str, refs: &[u64], agreeing: &[&str]) -> Agreement {
        Agreement {
            statement: "  Ship on   Monday ".into(),
            topic: topic.into(),
            refs: refs.iter().copied().map(Seq).collect(),
            agreeing: agreeing.iter().map(|a| a.parse().unwrap()).collect(),
        }
    }

    #[test]
    fn only_an_agreement_of_two_people_in_the_burst_holds_once_per_topic() {
        let burst = [
            said(1, "senator::ana"),
            said(2, "senator::ben"),
            said(3, "senator::bot"),
        ];
        let mut bursts = Bursts::default();
        let ok = checked(
            agreement(
                "Release day",
                &[1, 2, 99],
                &["senator::ana", "senator::ben"],
            ),
            &burst,
            &people(),
            &bursts,
        )
        .expect("holds");
        assert_eq!(ok.statement, "Ship on Monday");
        assert_eq!(ok.refs, [Seq(1), Seq(2)]);

        // An agent's agreement does not count; neither does someone who
        // wrote none of the cited messages.
        for agreeing in [
            &["senator::ana", "senator::bot"][..],
            &["senator::ana", "senator::cy"][..],
            &["senator::ana"][..],
        ] {
            assert!(
                checked(
                    agreement("Release day", &[1, 2, 3], agreeing),
                    &burst,
                    &people(),
                    &bursts
                )
                .is_none(),
                "{agreeing:?}"
            );
        }

        // Proposed once: the same topic, or a message already cited, is not
        // proposed again.
        bursts.topics.insert(normalized("release DAY"));
        assert!(
            checked(
                agreement("Release day", &[1, 2], &["senator::ana", "senator::ben"]),
                &burst,
                &people(),
                &bursts
            )
            .is_none()
        );
        bursts.topics.clear();
        bursts.cited.insert(Seq(2));
        assert!(
            checked(
                agreement("Another topic", &[1, 2], &["senator::ana", "senator::ben"]),
                &burst,
                &people(),
                &bursts
            )
            .is_none()
        );
    }
}
