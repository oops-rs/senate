//! Questions put to a seat that is no longer on the committee.
//!
//! A seat goes in one of two ways: an agent of this Senate is unseated
//! (a [`CommitteeSeatChange`](senate_core::CommitteeSeatChange) in the
//! log), or a child Senate's latest seat report no longer declares the
//! seat a relay named ([`meta::INTENDED`]). A question whose every
//! recipient is such a seat can never be answered. The Speaker's office
//! closes it: one note in its thread, to the one who asked, saying the seat
//! is gone, carrying [`meta::SEAT_REMOVED`] so the brief (and the pinned
//! "today" card) stop listing it as open. It is not re-routed: the asker
//! decides whether to ask again.
//!
//! The same test tells the Brain which earlier relays in its context went
//! to a seat that is gone ([`Utterance::gone_relays`](crate::Utterance)),
//! so a message sent again is not taken for a duplicate of one of them.

use senate_core::{Locator, brief::BriefQuestion};

use super::desk::DeskTurn;
use super::{
    Address, CommitteeId, Engine, Event, Kind, NewEvent, Seq, Stall, debug, derived_event_id, json,
    meta,
};

impl Engine {
    /// The seat a message to `to` (meant for `intended`) was put to, as a
    /// qualified name, when every recipient is a seat that is gone: an
    /// agent here the log unseated, or a seat a child Senate named in
    /// `intended` no longer declares on `committee` in its latest report.
    /// A person, a desk with no seat named, or a child Senate not known to
    /// have reported is never gone.
    pub(super) fn gone_seat(
        &self,
        committee: &CommitteeId,
        to: &[Address],
        intended: Option<&Locator>,
    ) -> Option<String> {
        let brief = self.committees.get(committee)?.brief.as_ref()?;
        let mut gone = None;
        for address in to {
            let seat = match address {
                Address::Senator(agent)
                    if !self.roster.is_person(agent) && brief.is_unseated(agent) =>
                {
                    address.in_text()
                }
                Address::Child { senate, committee } => {
                    let intended = intended?;
                    let seat = intended.address().as_senator()?;
                    if intended.senate()?.name() != senate {
                        return None;
                    }
                    let child = self.children.iter().find(|c| c.name == *senate)?;
                    // Nothing reported since this server started: nothing
                    // is known of its seats.
                    child.last_seen?;
                    let declared = child
                        .seats
                        .iter()
                        .any(|s| s.committee == *committee && s.senator == *seat);
                    if declared {
                        return None;
                    }
                    intended.to_string()
                }
                _ => return None,
            };
            gone.get_or_insert(seat);
        }
        gone
    }

    /// The seqs of `context`'s relays put to a seat that is gone
    /// ([`Self::gone_seat`]), for the Brain: a message sent again is not a
    /// duplicate of one of them.
    pub(super) fn gone_relays(&self, context: &[Event]) -> Vec<Seq> {
        context
            .iter()
            .filter(|e| matches!(e.kind, Kind::Question | Kind::Request | Kind::Note))
            .filter(|e| {
                let intended = senate_core::locator::intended(e);
                self.gone_seat(&e.committee, &e.to, intended.as_ref())
                    .is_some()
            })
            .map(|e| e.seq)
            .collect()
    }

    /// Close every open question on `committee` whose seat is gone: one
    /// note in each thread, to the asker. Only the desk's acting worker
    /// writes them; each note's id is derived from its question, so a
    /// replay or a second worker writes nothing new.
    pub(super) async fn close_gone_seats(&mut self, committee: &CommitteeId) -> Result<(), Stall> {
        let gone: Vec<(BriefQuestion, String)> = {
            let Some(state) = self.committees.get(committee) else {
                return Ok(());
            };
            let Some(brief) = state.brief.as_ref() else {
                return Ok(());
            };
            brief
                .open_questions()
                .filter(|q| !state.seat_removed.contains(&q.seq))
                .filter_map(|q| {
                    self.gone_seat(committee, &q.to, q.intended.as_ref())
                        .map(|seat| (q.clone(), seat))
                })
                .collect()
        };
        if gone.is_empty() {
            return Ok(());
        }
        match self.desk_turn(committee).await {
            DeskTurn::Acting => {}
            DeskTurn::Waiting(worker) => {
                debug!(%committee, %worker, "questions to seats that are gone; left to the desk worker before the built-in session");
                return Ok(());
            }
            DeskTurn::Off => return Ok(()),
        }
        let l = self.locale(committee);
        for (question, seat) in gone {
            let shown = self.seat_shown(l, committee, &seat);
            let mut note = NewEvent::new(
                committee.clone(),
                Kind::Note,
                Address::Speaker,
                crate::render::seat_removed(l, &shown),
            )
            .with_id(derived_event_id(&[
                "seat-removed",
                committee.as_str(),
                &question.seq.to_string(),
            ]))
            .in_thread(question.seq)
            .with_meta(meta::SEAT_REMOVED, json!(seat));
            if question.from != Address::Speaker {
                note = note.to(question.from.clone());
            }
            self.append(note).await?;
            if let Some(state) = self.committees.get_mut(committee) {
                state.seat_removed.insert(question.seq);
            }
        }
        Ok(())
    }

    /// A gone seat as people are shown it: its alias here when it still
    /// has one, else its qualified name as a mention.
    fn seat_shown(&self, l: senate_core::Locale, committee: &CommitteeId, seat: &str) -> String {
        match seat.parse::<Address>() {
            Ok(address @ Address::Senator(_)) => self.seat_names().local(committee, &address),
            _ => seat
                .parse::<Locator>()
                .ok()
                .and_then(|locator| self.seat_names().child(l, &locator))
                .unwrap_or_else(|| format!("[{seat}]")),
        }
    }
}
