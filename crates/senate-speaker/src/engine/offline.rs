//! A message for a child Senate that is offline.
//!
//! A child Senate says it is there by declaring its seats, every pass of
//! its uplink ([`ChildSenate::is_online`]). What is addressed to it while it
//! is offline stays in the log and reaches it when it reconnects, because
//! its uplink reads on from where it stopped. Meanwhile the one who asked
//! is waiting, so the Speaker's office does one of two things with a root
//! addressed to an offline child's desk:
//!
//! - **answers it on the child's behalf**, when it is a `question` and the
//!   Brain can answer from what this Senate sees
//!   ([`Brain::decide_offline`](crate::Brain::decide_offline): the brief
//!   and its checkpoints, the seats and duties the child declared, the
//!   thread): an `answer` in the question's thread, `from: speaker`,
//!   `on_behalf_of` the child's desk;
//! - **holds it and says so**: a `note` in the thread to the one who asked,
//!   saying the child is offline and since when. This is what the rules
//!   decide, what a Brain that fails or is late comes to, and all a
//!   `request` or a `note` gets: nobody is committed, and nothing is done,
//!   in an absent Senate's name.
//!
//! Both carry [`meta::CHILD_OFFLINE`], and there is one of them per
//! message, whichever it is. Neither changes the message itself: the child
//! still gets it, and what it answers lands in the same thread.
//!
//! # Who decides
//!
//! The built-in session, when it is the first present worker of the
//! Speaker's desk ([`DeskTurn::Acting`]), as for a crossing: with a person
//! presiding, a secretary before it that is present comes first, and the
//! built-in session writes nothing. It asks the Brain only when answering
//! for an absent senator is among its duties ([`Duty::Cover`]); without
//! that duty it holds.
//!
//! A child that has never declared its seats since this Senate started (or
//! runs a build from before seats were declared) is not known to be
//! offline, and nothing is said of it.

use std::sync::Arc;

use chrono::Utc;
use senate_core::{ChildSenate, docket};

use super::desk::DeskTurn;
use super::{
    Address, BrainError, CommitteeId, Engine, Event, Flow, Kind, NewEvent, Seq, Stall, ThoughtKind,
    debug, derived_event_id, json, meta, warn,
};
use crate::brain::{Offline, OfflineInput};
use crate::secretary::Duty;

impl Engine {
    /// The child Senate the root `event` is put to and that is offline now.
    fn offline_child(&self, event: &Event) -> Option<(ChildSenate, Address)> {
        if event.thread.is_some()
            || !matches!(event.kind, Kind::Question | Kind::Request | Kind::Note)
        {
            return None;
        }
        let now = Utc::now();
        event.to.iter().find_map(|address| {
            let Address::Child { senate, .. } = address else {
                return None;
            };
            self.children
                .iter()
                .find(|c| c.name == *senate && c.is_offline(now))
                .map(|child| (child.clone(), address.clone()))
        })
    }

    /// `event` is settled otherwise; when it is put to a child Senate that
    /// is offline, answer it for the child or say it is held.
    pub(super) async fn on_child_offline(&mut self, event: &Event) -> Result<Flow, Stall> {
        let Some((child, desk)) = self.offline_child(event) else {
            return Ok(Flow::Done);
        };
        let committee = event.committee.clone();
        let decided = self
            .committees
            .get(&committee)
            .and_then(|state| state.offline.get(&event.seq))
            .cloned();
        let decision = match decided {
            Some(decision) => decision,
            None => {
                match self.desk_turn(&committee).await {
                    DeskTurn::Acting => {}
                    DeskTurn::Waiting(worker) => {
                        debug!(%committee, seq = %event.seq, %worker, child = %child.name, "for an offline child Senate; a desk worker before the built-in session is present, left to it");
                        return Ok(Flow::Done);
                    }
                    DeskTurn::Off => return Ok(Flow::Done),
                }
                // A replay: said or answered already, or the child has.
                let dealt = self
                    .hub
                    .thread(&committee, event.seq)
                    .await?
                    .iter()
                    .filter(|e| e.thread == Some(event.seq))
                    .any(|e| e.meta.contains_key(meta::CHILD_OFFLINE) || e.from == desk);
                if dealt {
                    return Ok(Flow::Done);
                }
                if event.kind != Kind::Question || !self.may(&committee, Duty::Cover) {
                    Offline::Hold
                } else {
                    let input = OfflineInput {
                        committee: committee.clone(),
                        event: event.clone(),
                        child,
                        intended: senate_core::locator::intended(event),
                        brief: self.hub.brief(&committee).await.ok(),
                        context: self.context(&committee, event.seq).await?,
                        locale: self.locale(&committee),
                    };
                    let brain = Arc::clone(&self.brain);
                    let seq = event.seq;
                    let limit = self.triage_deadline;
                    self.think_within(
                        limit,
                        async move { brain.decide_offline(input).await },
                        move |decision| ThoughtKind::OfflineDecided {
                            committee,
                            seq,
                            decision,
                        },
                    );
                    return Ok(Flow::Await);
                }
            }
        };
        let l = self.locale(&committee);
        let (kind, body) = match &decision {
            Offline::Answer(text) if !text.trim().is_empty() => (
                Kind::Answer,
                docket::child_offline_answer(l, &child.name, text),
            ),
            Offline::Answer(_) | Offline::Hold => {
                let away = child
                    .last_seen
                    .map_or_else(chrono::Duration::zero, |seen| Utc::now() - seen);
                (Kind::Note, docket::child_offline_held(l, &child.name, away))
            }
        };
        let answered = kind == Kind::Answer;
        let mut line = NewEvent::new(committee.clone(), kind, Address::Speaker, body)
            // One line per message, whichever it is and however often it
            // is decided.
            .with_id(derived_event_id(&[
                "child-offline",
                committee.as_str(),
                &event.seq.to_string(),
            ]))
            .in_thread(event.seq)
            .with_meta(meta::CHILD_OFFLINE, json!(child.name.as_str()));
        if answered {
            line = line.on_behalf_of(desk);
        }
        // The one who asked: the person a relay was written for, else the
        // sender. The office's own forward has nobody here to tell.
        let asker = event.on_behalf_of.as_ref().unwrap_or(&event.from);
        if *asker != Address::Speaker {
            line = line.to(asker.clone());
        }
        self.append(line).await?;
        Ok(Flow::Done)
    }

    pub(super) async fn on_offline_decided(
        &mut self,
        committee: CommitteeId,
        seq: Seq,
        decision: Result<Offline, BrainError>,
    ) {
        let decision = decision.unwrap_or_else(|error| {
            warn!(%committee, %seq, %error, "no decision for an offline child Senate; holding");
            Offline::Hold
        });
        if let Some(state) = self.committees.get_mut(&committee) {
            state.offline.insert(seq, decision);
            if state.awaiting == Some(seq) {
                state.awaiting = None;
            }
        }
        self.catch_up(&committee).await;
    }
}
