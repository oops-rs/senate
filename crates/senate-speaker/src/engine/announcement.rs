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
//! - **Startup seats.** Before the first send, read the hub's child reports.
//!   Every registered child reports across committees, even when it has no
//!   seats. A child may be backing off after a failed request during the
//!   parent restart. Wait for fresh reports up to the maximum reconnect
//!   backoff plus one poll from Speaker startup. The first send starts
//!   the hourly throttle.
//! - **Coalesced and throttled.** One refresh is pending per group at a
//!   time: `card_debounce_ms` after the first change, and never sooner than
//!   [`ANNOUNCEMENT_INTERVAL`] after the last section sent there. Whatever
//!   changed meanwhile lands with it.
//! - **Unsupported once.** A surface that cannot set this group's
//!   announcement (no announcements, or no right to edit this one) gets one
//!   notice in the group, and no more tries until the group is bound again
//!   or the Speaker restarts. Both retry the update. A receipt under the
//!   Speaker's data directory suppresses repeat posts for the same log,
//!   committee, platform and group until an update succeeds. An interrupted
//!   post retries with its saved operation id.
//!
//! What was last sent is kept in memory only (like the pinned cards): a
//! restart sends one refresh. Its `operation_id` names the committee's head
//! when it was rendered, so a replay is a gateway duplicate and new content
//! is always a new operation.

use std::{fs, io, path::Path};

use senate_core::{AnnouncementSection, BriefBuilder, EventId, StatusState};
use serde::{Deserialize, Serialize};
use tokio::time::Instant;

use super::{
    CommitteeId, CommitteeState, Duration, Engine, Outcome, PostTarget, STALL_RETRY, Seq, Stall,
    Timer, warn,
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

/// Saved atomically for one logical binding. A repeat bind has a new log
/// sequence but addresses the same group on the same platform.
#[derive(Serialize, Deserialize)]
enum Notice {
    /// An update succeeded after an outage. The next outage needs a fresh key.
    Recovered(EventId),
    /// Written before posting, so even a rebind can retry an ambiguous delivery.
    Posting(String),
    Posted,
}

impl Notice {
    fn load(path: &Path) -> io::Result<Option<Self>> {
        match fs::File::open(path) {
            Ok(file) => Ok(Some(serde_json::from_reader(file)?)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().expect("notice path has a parent");
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(temp.as_file_mut(), self)?;
        temp.as_file().sync_all()?;
        temp.persist(path)?;
        Ok(())
    }
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
        if entry.sent_at.is_none() && !entry.unsupported {
            // Seat reports do not append events. Read them even on a quiet
            // committee, including on the last attempt at the deadline.
            let ready = match self.hub.children().await {
                Ok(children) => {
                    self.children = children;
                    self.children
                        .iter()
                        .all(|child| child.last_seen.is_some_and(|seen| seen >= self.started_at))
                }
                Err(error) => {
                    tracing::debug!(%error, "cannot read startup seat reports");
                    false
                }
            };
            let remaining = self
                .seat_report_deadline
                .saturating_duration_since(Instant::now());
            if !ready && !remaining.is_zero() {
                let delay = if self.card_debounce.is_zero() {
                    remaining.min(senate_core::api::UPLINK_POLL)
                } else {
                    remaining.min(self.card_debounce)
                };
                entry.timer =
                    Some(
                        self.myself
                            .send_after(delay, move || SpeakerMsg::AnnouncementDue {
                                committee,
                                group,
                            }),
                    );
                return;
            }
        }
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
        let platform = self.surface.platform();
        let key = crate::ids::derived_event_id(&[
            self.epoch.as_deref().expect("checked by content_op_id"),
            committee.as_str(),
            platform,
            group,
        ]);
        let path = self
            .config
            .speaker_dir()
            .join("announcement-notices")
            .join(key.to_string());
        if let Outcome::Unsupported = outcome {
            self.post_announcement_notice(committee, binding, group, l, &path)
                .await?;
        }
        let Some(entry) = self
            .committees
            .get_mut(committee)
            .and_then(|s| s.announcements.get_mut(group))
        else {
            return Ok(());
        };
        match outcome {
            Outcome::Done(()) => {
                if matches!(
                    Notice::load(&path)?,
                    Some(Notice::Posted | Notice::Posting(_))
                ) {
                    Notice::Recovered(EventId::new()).save(&path)?;
                }
                entry.last = Some(section);
                entry.sent_at = Some(Instant::now());
            }
            Outcome::Refused => entry.sent_at = Some(Instant::now()),
            Outcome::Unsupported => entry.unsupported = true,
        }
        Ok(())
    }

    async fn post_announcement_notice(
        &self,
        committee: &CommitteeId,
        binding: Seq,
        group: &str,
        locale: senate_core::Locale,
        path: &Path,
    ) -> Result<(), Stall> {
        let operation = match Notice::load(path)? {
            Some(Notice::Posted) => return Ok(()),
            Some(Notice::Posting(id)) => id,
            state => {
                let mut target = format!("{group}#announcement-unsupported");
                if let Some(Notice::Recovered(generation)) = state {
                    target.push_str(&format!(":{generation}"));
                }
                let id = self.op_id(committee, binding, &target)?;
                Notice::Posting(id.clone()).save(path)?;
                id
            }
        };
        let target = PostTarget::NewRoot {
            group_id: group.to_owned(),
        };
        if self
            .post(
                target,
                &render::announcement_unsupported(locale),
                &operation,
            )
            .await?
            .is_some()
        {
            Notice::Posted.save(path)?;
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
                m.facts()
                    .rev()
                    .map(|p| render::announcement_point(&p.text))
                    .collect()
            })
            .unwrap_or_default(),
    };
    render::announcement(state.locale, &parts)
}
