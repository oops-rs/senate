//! The Speaker's section of each people group's announcement: the
//! committee, how to reach the Speaker, the seats, what waits on people and
//! recent decisions ([`render::announcement`]).
//!
//! - **People groups only.** The Agent group has its pinned cards and never
//!   gets an announcement.
//! - **Only when it changed.** Any event but chat schedules a refresh; the
//!   refresh renders the section and sends it only if it differs from the
//!   one last sent. Nothing in it is relative to the time, so an unchanged
//!   committee sends nothing.
//! - **Coalesced and throttled.** One refresh is pending per group at a
//!   time: `card_debounce_ms` after the first change, and never sooner than
//!   [`ANNOUNCEMENT_INTERVAL`] after the last section sent there. Whatever
//!   changed meanwhile lands with it.
//! - **Unsupported once.** A surface that cannot set this group's
//!   announcement (no announcements, or no right to edit this one) gets one
//!   notice in the group, and no more tries until the group is bound again
//!   or the Speaker restarts. The notice's `operation_id` names the binding,
//!   so a restart that finds it still unsupported does not post it twice.
//!
//! What was last sent is kept in memory only (like the pinned cards): a
//! restart sends one refresh. Its `operation_id` names the committee's head
//! when it was rendered, so a replay is a gateway duplicate and new content
//! is always a new operation.

use senate_core::{AnnouncementSection, BriefBuilder, StatusState};
use tokio::time::Instant;

use super::{
    CommitteeId, CommitteeState, Duration, Engine, Outcome, PostTarget, STALL_RETRY, Seq, Stall,
    Timer, debug, warn,
};
use crate::actor::SpeakerMsg;
use crate::brain::CommitteeSeat;
use crate::render;

/// At most one announcement update per group per this long.
pub(crate) const ANNOUNCEMENT_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// One people group's announcement, for the binding that bound it.
#[derive(Default)]
pub(super) struct Announced {
    /// The `binding` this is for: a new binding starts over.
    binding: Seq,
    /// The section last set, and when.
    last: Option<AnnouncementSection>,
    sent_at: Option<Instant>,
    timer: Option<Timer>,
    /// The surface cannot set it here: no more tries for this binding.
    unsupported: bool,
}

impl Engine {
    /// Refresh every people group's announcement, coalesced and throttled.
    pub(super) fn schedule_announcements(&mut self, committee: &CommitteeId) {
        let debounce = self.card_debounce;
        let Some(state) = self.committees.get_mut(committee) else {
            return;
        };
        let bound: Vec<(String, Seq)> = state
            .people_groups()
            .map(|g| (g.group.group_id.clone(), g.seq))
            .collect();
        forget_unbound(state, &bound);
        for (group, binding) in bound {
            let entry = state.announcements.entry(group.clone()).or_default();
            if entry.binding != binding {
                if let Some(timer) = entry.timer.take() {
                    timer.abort();
                }
                *entry = Announced {
                    binding,
                    ..Announced::default()
                };
            }
            if entry.unsupported || entry.timer.is_some() {
                continue;
            }
            let delay = entry
                .sent_at
                .map(|at| (at + ANNOUNCEMENT_INTERVAL).saturating_duration_since(Instant::now()))
                .unwrap_or_default()
                .max(debounce);
            let message = committee.clone();
            entry.timer =
                Some(
                    self.myself
                        .send_after(delay, move || SpeakerMsg::AnnouncementDue {
                            committee: message,
                            group,
                        }),
                );
        }
    }

    pub(crate) async fn on_announcement_due(&mut self, committee: CommitteeId, group: String) {
        self.catch_up(&committee).await;
        let Some(entry) = self
            .committees
            .get_mut(&committee)
            .and_then(|state| state.announcements.get_mut(&group))
        else {
            return;
        };
        entry.timer = None;
        if let Err(stall) = self.refresh_announcement(&committee, &group).await {
            warn!(%committee, %group, error = %stall, "announcement deferred");
            let (message, again) = (committee.clone(), group.clone());
            let timer = self
                .myself
                .send_after(STALL_RETRY, move || SpeakerMsg::AnnouncementDue {
                    committee: message,
                    group: again,
                });
            match self
                .committees
                .get_mut(&committee)
                .and_then(|state| state.announcements.get_mut(&group))
            {
                Some(entry) => entry.timer = Some(timer),
                None => timer.abort(),
            }
        }
    }

    async fn refresh_announcement(
        &mut self,
        committee: &CommitteeId,
        group: &str,
    ) -> Result<(), Stall> {
        let seats = self.committee_seats(committee).await;
        let Some(state) = self.committees.get(committee) else {
            return Ok(());
        };
        let Some(binding) = state
            .people_groups()
            .find(|g| g.group.group_id == group)
            .map(|g| g.seq)
        else {
            return Ok(()); // unbound, or bound as the Agent group now
        };
        let Some(entry) = state.announcements.get(group) else {
            return Ok(());
        };
        if entry.unsupported || entry.binding != binding {
            return Ok(());
        }
        let section = announcement_section(committee, state, &seats);
        if entry.last.as_ref() == Some(&section) {
            return Ok(()); // nothing people would see changed
        }
        let l = state.locale;
        let head = state.board.head();
        let operation =
            self.content_op_id(committee, head, &format!("{group}#announcement"), &section)?;
        let outcome = self
            .call(self.surface.set_announcement(group, &section, &operation))
            .await?;
        let Some(entry) = self
            .committees
            .get_mut(committee)
            .and_then(|state| state.announcements.get_mut(group))
        else {
            return Ok(());
        };
        match outcome {
            Outcome::Done(()) => {
                entry.last = Some(section);
                entry.sent_at = Some(Instant::now());
            }
            // Logged by `call`; the next change tries again, an hour on.
            Outcome::Refused => entry.sent_at = Some(Instant::now()),
            Outcome::Unsupported => {
                entry.unsupported = true;
                let operation = self.op_id(
                    committee,
                    binding,
                    &format!("{group}#announcement-unsupported"),
                )?;
                let notice = render::announcement_unsupported(l);
                let target = PostTarget::NewRoot {
                    group_id: group.to_owned(),
                };
                if let Err(stall) = self.post(target, &notice, &operation).await {
                    // Try it all again: the notice's id makes a second
                    // attempt one notice.
                    if let Some(entry) = self
                        .committees
                        .get_mut(committee)
                        .and_then(|state| state.announcements.get_mut(group))
                    {
                        entry.unsupported = false;
                    }
                    return Err(stall);
                }
                debug!(%committee, %group, "the group's announcement cannot be set; said so");
            }
        }
        Ok(())
    }
}

/// Drop what is kept for groups no longer bound for people.
fn forget_unbound(state: &mut CommitteeState, bound: &[(String, Seq)]) {
    state.announcements.retain(|group, entry| {
        let keep = bound.iter().any(|(g, _)| g == group);
        if !keep && let Some(timer) = entry.timer.take() {
            timer.abort();
        }
        keep
    });
}

/// The section as the committee's state has it now, the seats as
/// [`Engine::committee_seats`] lists them.
fn announcement_section(
    committee: &CommitteeId,
    state: &CommitteeState,
    seats: &[CommitteeSeat],
) -> AnnouncementSection {
    let brief = state.brief.as_ref().map(BriefBuilder::brief);
    let memory = state.brief.as_ref().map(BriefBuilder::memory);
    let parts = render::AnnouncementParts {
        committee,
        seats,
        waiting: state.board.threads(Some(StatusState::Suspended)),
        decisions: brief
            .as_ref()
            .map(|b| b.decisions.iter().rev().collect())
            .unwrap_or_default(),
        lately: memory
            .and_then(|m| m.latest_digest())
            .map(|d| {
                d.payload
                    .points
                    .iter()
                    .map(|p| render::announcement_point(&p.text))
                    .collect()
            })
            .unwrap_or_default(),
        // Newest first: the section shows the latest few.
        known: memory
            .map(|m| {
                m.in_force()
                    .rev()
                    .map(|p| render::announcement_point(&p.text))
                    .collect()
            })
            .unwrap_or_default(),
    };
    render::announcement(state.locale, &parts)
}
