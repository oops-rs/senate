//! Cards updated in place: each task thread's card, and the Agent
//! group's two pinned cards, "等人处理" and "今天的摘要".
//!
//! # Where their refs live
//!
//! Everything a restarted Speaker needs to find a card again is a
//! [`MirrorRef`](senate_core::MirrorRef):
//!
//! - **A task card** is the root's copy in the group it was posted to
//!   (target `<group>`, so replies under it map to the thread), plus a
//!   marker copy `<group>#card` on the same seq saying that copy is a card.
//!   An update that landed is recorded as `<group>#card` on the thread's
//!   last seq at the time (the revision it shows), so after a restart a
//!   thread whose last seq has no such marker gets one more update; the
//!   final state always lands.
//! - **The pinned cards** hang on the Agent group's `binding` event:
//!   `<agents>#pinned-waiting` and `<agents>#pinned-today` on that seq. A
//!   newly adopted Agent group (a new binding) gets its own pair. What each
//!   card last showed is kept in memory only, so a restart sends one
//!   refresh.
//!
//! Update `operation_id`s name the seq the content was rendered at
//! (`<group>#card` on the thread's last seq; `<agents>#waiting` and
//! `<agents>#today-<date>` on the committee's head), so a replay is a gateway
//! duplicate and new content is always a new operation.

use chrono::{NaiveDate, NaiveTime};
use senate_core::{Card, MirrorRef, StatusState};

use super::{
    CommitteeId, CommitteeState, Duration, Engine, Event, Kind, Outcome, OutputLevel, PostTarget,
    STALL_RETRY, Seq, Stall, card_target, debug, warn,
};
use crate::actor::SpeakerMsg;
use crate::clock::Zone;
use crate::render;

fn waiting_target(group: &str) -> String {
    format!("{group}#pinned-waiting")
}

fn today_target(group: &str) -> String {
    format!("{group}#pinned-today")
}

fn is_today(ts: &chrono::DateTime<chrono::Utc>, day: NaiveDate, zone: Zone) -> bool {
    zone.date_of(*ts) == day
}

/// Time until a little after the next midnight in the committee's
/// timezone: "今天的摘要" starts fresh each day there.
fn until_midnight(zone: Zone) -> Duration {
    zone.until(chrono::Utc::now(), NaiveTime::MIN) + Duration::from_secs(5)
}

impl Engine {
    /// After an event's effects landed: refresh the cards it changes, and
    /// invite an active agent's person into the Agent group.
    pub(super) async fn after(&mut self, event: &Event) {
        if event.kind == Kind::Chat {
            return;
        }
        let Some(state) = self.committees.get(&event.committee) else {
            return;
        };
        let card = event.thread.filter(|root| {
            state.board.get(*root).is_some_and(|thread| {
                matches!(
                    event.kind,
                    Kind::Answer | Kind::Status | Kind::Note | Kind::Question | Kind::Request
                ) && card_wanted(state.output_level(), thread.state)
            })
        });
        let pinned = state.agents_group().is_some();
        if let Some(root) = card {
            self.schedule_card(&event.committee, root);
        }
        if pinned {
            self.schedule_pinned(&event.committee);
        }
        self.schedule_announcements(&event.committee);
        self.invite_on_activity(event).await;
    }

    // ----- task cards --------------------------------------------------------

    /// Update thread `root`'s card `card_debounce_ms` from now, unless an
    /// update is already due: it renders whatever the thread is then, so the
    /// last change before it lands with it, and any change after it
    /// schedules the next.
    pub(super) fn schedule_card(&mut self, committee: &CommitteeId, root: Seq) {
        let delay = self.card_debounce;
        let Some(state) = self.committees.get_mut(committee) else {
            return;
        };
        if state.cards.contains_key(&root) {
            return;
        }
        let message = committee.clone();
        let timer = self.myself.send_after(delay, move || SpeakerMsg::CardDue {
            committee: message,
            root,
        });
        state.cards.insert(root, timer);
    }

    pub(crate) async fn on_card_due(&mut self, committee: CommitteeId, root: Seq) {
        self.catch_up(&committee).await;
        let Some(state) = self.committees.get_mut(&committee) else {
            return;
        };
        state.cards.remove(&root);
        let Some(thread) = state.board.get(root).cloned() else {
            return;
        };
        if !card_wanted(state.output_level(), thread.state) {
            return;
        }
        let answer = state.answers.get(&root).cloned();
        match self
            .update_task_card(&committee, &thread, answer.as_deref())
            .await
        {
            Ok(()) => {}
            Err(stall) => {
                warn!(%committee, %root, error = %stall, "card update deferred");
                let message = committee.clone();
                let timer = self
                    .myself
                    .send_after(STALL_RETRY, move || SpeakerMsg::CardDue {
                        committee: message,
                        root,
                    });
                if let Some(state) = self.committees.get_mut(&committee) {
                    state.cards.insert(root, timer);
                }
            }
        }
    }

    /// The card's copy of `root`, if it was posted as a card:
    /// `(group, message_ref)`.
    async fn card_of(
        &self,
        committee: &CommitteeId,
        root: Seq,
    ) -> Result<Option<(String, String)>, Stall> {
        Ok(self
            .hub
            .mirrors_of_seq(committee, root, self.surface.platform())
            .await?
            .into_iter()
            .find_map(|copy| {
                let group = copy.target.strip_suffix("#card")?.to_owned();
                (!group.contains('#')).then_some((group, copy.message_ref))
            }))
    }

    async fn update_task_card(
        &self,
        committee: &CommitteeId,
        thread: &senate_core::ThreadSummary,
        answer: Option<&str>,
    ) -> Result<(), Stall> {
        let l = self.locale(committee);
        let Some((group, message_ref)) = self.card_of(committee, thread.root).await? else {
            return Ok(()); // a plain post (no cards), or not posted
        };
        let marker = card_target(&group);
        if self.has_copy(committee, thread.updated, &marker).await? {
            return Ok(()); // this revision already landed
        }
        let card = render::task_card(l, thread, None, answer);
        let operation = self.op_id(committee, thread.updated, &marker)?;
        match self
            .call(self.surface.update_card(&message_ref, &card, &operation))
            .await?
        {
            Outcome::Done(()) => {
                self.hub
                    .put_mirror(MirrorRef {
                        committee: committee.clone(),
                        seq: thread.updated,
                        platform: self.surface.platform().to_owned(),
                        target: marker,
                        message_ref,
                        thread_ref: None,
                    })
                    .await?;
            }
            Outcome::Unsupported => self.cards_unsupported_once(),
            Outcome::Refused => {}
        }
        Ok(())
    }

    /// After a restart: every card whose thread moved on since its last
    /// landed update gets one, and the pinned cards one refresh.
    pub(super) async fn rearm_cards(&mut self, committee: &CommitteeId) {
        let Some(state) = self.committees.get(committee) else {
            return;
        };
        let level = state.output_level();
        let moved: Vec<(Seq, Seq)> = state
            .board
            .threads(None)
            .into_iter()
            .filter(|t| t.updated > t.root && card_wanted(level, t.state))
            .map(|t| (t.root, t.updated))
            .collect();
        let pinned = state.agents_group().is_some();
        for (root, updated) in moved {
            let stale = match self.card_of(committee, root).await {
                Ok(Some((group, _))) => !self
                    .has_copy(committee, updated, &card_target(&group))
                    .await
                    .unwrap_or(false),
                _ => false,
            };
            if stale {
                self.schedule_card(committee, root);
            }
        }
        if pinned {
            self.schedule_pinned(committee);
        }
        self.schedule_announcements(committee);
    }

    // ----- the pinned cards ----------------------------------------------------

    /// Post the Agent group's two pinned cards, once per Agent group
    /// binding, and pin them. Without cards there are none (logged once);
    /// without pins they stay unpinned.
    pub(super) async fn post_pinned_cards(&mut self, committee: &CommitteeId) -> Result<(), Stall> {
        let seats = self.committee_seats(committee).await;
        let zone = self.config.zone_of(committee);
        let Some(state) = self.committees.get(committee) else {
            return Ok(());
        };
        let Some(agents) = state.agents_group().cloned() else {
            return Ok(());
        };
        let (waiting, today_card) = pinned_cards(state, &seats, zone);
        let group = agents.group.group_id.clone();
        for (target, card) in [
            (waiting_target(&group), waiting),
            (today_target(&group), today_card),
        ] {
            if self.has_copy(committee, agents.seq, &target).await? {
                continue;
            }
            let operation = self.op_id(committee, agents.seq, &target)?;
            let posted = match self
                .call(self.surface.post_card(
                    PostTarget::NewRoot {
                        group_id: group.clone(),
                    },
                    &card,
                    &operation,
                ))
                .await?
            {
                Outcome::Done(posted) => posted,
                Outcome::Unsupported => {
                    self.cards_unsupported_once();
                    return Ok(());
                }
                Outcome::Refused => continue,
            };
            let pin_op = self.op_id(committee, agents.seq, &format!("{target}:pin"))?;
            if !matches!(
                self.call(self.surface.pin(&posted.message_ref, &pin_op))
                    .await?,
                Outcome::Done(())
            ) {
                debug!(%committee, %target, "the pinned card stays unpinned");
            }
            self.hub
                .put_mirror(MirrorRef {
                    committee: committee.clone(),
                    seq: agents.seq,
                    platform: self.surface.platform().to_owned(),
                    target: target.clone(),
                    message_ref: posted.message_ref,
                    thread_ref: posted.thread_ref,
                })
                .await?;
            if let Some(state) = self.committees.get_mut(committee) {
                if target.ends_with("#pinned-waiting") {
                    state.pinned.waiting = Some(card);
                } else {
                    state.pinned.today = Some(card);
                }
            }
        }
        Ok(())
    }

    /// Refresh the pinned cards `card_debounce_ms` from now, unless a
    /// refresh is already due.
    pub(super) fn schedule_pinned(&mut self, committee: &CommitteeId) {
        let delay = self.card_debounce;
        let Some(state) = self.committees.get_mut(committee) else {
            return;
        };
        if state.pinned.timer.is_some() {
            return;
        }
        let message = committee.clone();
        state.pinned.timer = Some(
            self.myself
                .send_after(delay, move || SpeakerMsg::PinnedDue(message)),
        );
    }

    pub(crate) async fn on_pinned_due(&mut self, committee: CommitteeId) {
        self.catch_up(&committee).await;
        let zone = self.config.zone_of(&committee);
        let day = zone.today();
        let Some(state) = self.committees.get_mut(&committee) else {
            return;
        };
        state.pinned.timer = None;
        if state.pinned.midnight_for != Some(day) {
            // "今天的摘要" starts over at midnight with nothing else changing.
            if let Some(old) = state.pinned.midnight.take() {
                old.abort();
            }
            let message = committee.clone();
            state.pinned.midnight = Some(
                self.myself
                    .send_after(until_midnight(zone), move || SpeakerMsg::PinnedDue(message)),
            );
            state.pinned.midnight_for = Some(day);
        }
        if let Err(stall) = self.refresh_pinned(&committee, day).await {
            warn!(%committee, error = %stall, "pinned cards deferred");
            if let Some(state) = self.committees.get_mut(&committee) {
                let message = committee.clone();
                state.pinned.timer = Some(
                    self.myself
                        .send_after(STALL_RETRY, move || SpeakerMsg::PinnedDue(message)),
                );
            }
        }
    }

    async fn refresh_pinned(
        &mut self,
        committee: &CommitteeId,
        day: NaiveDate,
    ) -> Result<(), Stall> {
        let seats = self.committee_seats(committee).await;
        let zone = self.config.zone_of(committee);
        let Some(state) = self.committees.get(committee) else {
            return Ok(());
        };
        let Some(agents) = state.agents_group().cloned() else {
            return Ok(());
        };
        let head = state.board.head();
        let (waiting, today_card) = pinned_cards(state, &seats, zone);
        let group = agents.group.group_id.clone();
        let copies = self
            .hub
            .mirrors_of_seq(committee, agents.seq, self.surface.platform())
            .await?;
        let message_of = |target: &str| {
            copies
                .iter()
                .find(|c| c.target == target)
                .map(|c| c.message_ref.clone())
        };
        let jobs = [
            (
                message_of(&waiting_target(&group)),
                format!("{group}#waiting"),
                waiting,
                true,
            ),
            (
                message_of(&today_target(&group)),
                format!("{group}#today-{}", day.format("%Y%m%d")),
                today_card,
                false,
            ),
        ];
        for (message_ref, key, card, is_waiting) in jobs {
            let Some(message_ref) = message_ref else {
                continue;
            };
            let unchanged = self.committees.get(committee).is_some_and(|s| {
                let last = if is_waiting {
                    &s.pinned.waiting
                } else {
                    &s.pinned.today
                };
                last.as_ref() == Some(&card)
            });
            if unchanged {
                continue;
            }
            let operation = self.op_id(committee, head, &key)?;
            match self
                .call(self.surface.update_card(&message_ref, &card, &operation))
                .await?
            {
                Outcome::Done(()) => {
                    if let Some(state) = self.committees.get_mut(committee) {
                        if is_waiting {
                            state.pinned.waiting = Some(card);
                        } else {
                            state.pinned.today = Some(card);
                        }
                    }
                }
                Outcome::Unsupported => self.cards_unsupported_once(),
                Outcome::Refused => {}
            }
        }
        Ok(())
    }
}

/// Whether a card update is wanted at this output level: at "answers only"
/// a card changes only when it is answered or closed.
fn card_wanted(level: OutputLevel, state: StatusState) -> bool {
    level != OutputLevel::Answers || matches!(state, StatusState::Active | StatusState::Archived)
}

/// "等人处理" and "今天的摘要" as the committee's state has them now, the
/// latter listing `seats` ([`Engine::committee_seats`]).
fn pinned_cards(
    state: &CommitteeState,
    seats: &[crate::brain::CommitteeSeat],
    zone: Zone,
) -> (Card, Card) {
    let l = state.locale;
    let waiting = render::waiting_card(l, &state.board.threads(Some(StatusState::Suspended)));
    let day = zone.today();
    let is_today = |ts: &chrono::DateTime<chrono::Utc>, day: NaiveDate| is_today(ts, day, zone);
    // The latest digest, when it was written today.
    let discussion = state
        .brief
        .as_ref()
        .and_then(|b| b.memory().latest_digest())
        .filter(|d| is_today(&d.ts, day))
        .map(|d| d.payload.points.iter().map(|p| p.text.clone()).collect())
        .unwrap_or_default();
    let brief = state.brief.as_ref().map(senate_core::BriefBuilder::brief);
    let decisions = brief
        .as_ref()
        .map(|b| {
            b.decisions
                .iter()
                .filter(|d| is_today(&d.ts, day))
                .collect()
        })
        .unwrap_or_default();
    let questions = brief
        .as_ref()
        .map(|b| b.open_questions.iter().collect())
        .unwrap_or_default();
    let mut threads: Vec<_> = state
        .board
        .threads(None)
        .into_iter()
        .filter(|t| {
            state
                .root_ts
                .get(&t.root)
                .is_some_and(|ts| is_today(ts, day))
        })
        .collect();
    threads.sort_by_key(|t| t.root);
    // Agents here active today, by the name people see them by; then
    // every seat in a child Senate, from its report, with what the log
    // here last has of it today (the relays to it, its answers).
    let mut active: Vec<(Seq, String, String)> = state
        .activity
        .iter()
        .filter(|(_, (_, ts, _))| is_today(ts, day))
        .map(|(agent, (seq, _, what))| {
            let who = seats
                .iter()
                .find(|s| s.target == agent.as_str())
                .map_or_else(
                    || senate_core::Address::senator(agent).in_text(),
                    crate::brain::CommitteeSeat::people_name,
                );
            (*seq, who, what.clone())
        })
        .collect();
    let mut quiet = Vec::new();
    for seat in seats.iter().filter(|s| s.target.contains("::")) {
        let who = render::seat_line(l, seat);
        match state
            .seat_activity
            .get(&seat.target)
            .filter(|(_, ts, _)| is_today(ts, day))
        {
            Some((seq, _, what)) => active.push((*seq, who, what.clone())),
            None => quiet.push((who, None)),
        }
    }
    active.sort_by_key(|a| std::cmp::Reverse(a.0));
    let activity = active
        .into_iter()
        .map(|(seq, who, what)| (who, Some((seq, what))))
        .chain(quiet)
        .collect();
    let today_card = render::today_card(
        l,
        &render::Today {
            date: day,
            decisions,
            discussion,
            questions,
            threads,
            activity,
            seats,
        },
    );
    (waiting, today_card)
}
