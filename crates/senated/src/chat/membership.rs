//! Bind = join: keeping the bot's group memberships in line with the log.
//!
//! The gateway decides *which* groups the bot should be in (joined while the
//! log binds the group, left once it binds it to nothing); this module
//! makes it so, without ever holding anything up:
//!
//! - every change runs in a task of its own, one task per group at a time,
//!   so a slow or hanging platform never blocks the Speaker, an append, or
//!   another group;
//! - a release waits [`MembershipPolicy::release_grace`] first, and a bind within
//!   it cancels it, so a group moved from one committee to another (an
//!   unbind, then a bind) stays joined throughout and is never adopted
//!   twice;
//! - a failed adopt or release is retried with a doubling pause (from
//!   [`MembershipPolicy::retry_initial`] up to [`MembershipPolicy::retry_max`], with no
//!   end) until it succeeds or the log's bindings make it unwanted;
//! - but one the platform refuses outright ([`SurfaceError::Rejected`]: it
//!   does not know the message the group was named by, say) is *parked*:
//!   not tried again until something changes (a new real message from the
//!   group, a rebind, the log wanting the opposite, or a restart), logged
//!   once, and listed by [`ChatGateway::parked_groups`](super::ChatGateway::parked_groups);
//! - calls for one group are made one at a time and in the order wanted, and
//!   each is bounded by [`MembershipPolicy::call_timeout`], so a call that never
//!   returns is a failure like any other;
//! - a surface that answers `Unsupported` is asked no more, for that kind of
//!   call: nothing retries, nothing spins.
//!
//! What the gateway believes the platform holds (`confirmed`) is a cache of
//! the last answer, never stored: after a restart it starts empty and the
//! log's bindings are replayed, so every bound group is adopted again
//! (idempotent) and every group the log ever bound that is bound to nothing
//! now is released again (idempotent).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use senate_core::{AdoptedGroup, HubError, Seq, SurfaceError};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use super::{Inner, lock};

/// How the gateway paces joining and leaving groups. See the [module
/// docs](self) and [`ChatGateway::with_membership_policy`](super::ChatGateway::with_membership_policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MembershipPolicy {
    /// How long the bot stays in a group bound to nothing before it is
    /// released. A bind in that time keeps it.
    pub release_grace: Duration,
    /// The pause after a failed adopt or release; it doubles per failure.
    pub retry_initial: Duration,
    /// The longest pause between two tries.
    pub retry_max: Duration,
    /// How long one call to the platform may take before it counts as
    /// failed.
    pub call_timeout: Duration,
}

impl Default for MembershipPolicy {
    fn default() -> Self {
        Self {
            release_grace: Duration::from_secs(3),
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            call_timeout: Duration::from_secs(30),
        }
    }
}

impl MembershipPolicy {
    fn next_pause(&self, pause: Duration) -> Duration {
        pause.saturating_mul(2).min(self.retry_max)
    }
}

/// What the log wants of the bot's membership of a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Want {
    Held,
    Released,
}

/// Why a bound group's membership is stuck until someone acts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParkedReason {
    /// The platform refused the call outright; it is not tried again until
    /// something changes.
    Rejected {
        /// What the platform said.
        error: String,
    },
    /// No real message names the group yet (the log holds only simulated
    /// ones, or none): the adopt waits for one.
    NoRealMessage,
}

/// A group the gateway cannot bring in line with the log by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkedGroup {
    pub group_id: String,
    pub reason: ParkedReason,
    pub since: chrono::DateTime<chrono::Utc>,
}

struct GroupMembership {
    /// What the log wants now; `epoch` changes whenever this does.
    want: Option<Want>,
    epoch: u64,
    /// What the platform last confirmed, if it has since this run began.
    confirmed: Option<Want>,
    /// The epoch of the task working towards `want`, if one is alive.
    driver: Option<u64>,
    /// Held across each call for this group, so calls are made in turn.
    gate: Arc<AsyncMutex<()>>,
    /// Why nothing is being tried for `want`, when that is so.
    parked: Option<(ParkedReason, chrono::DateTime<chrono::Utc>)>,
}

impl GroupMembership {
    fn new() -> Self {
        Self {
            want: None,
            epoch: 0,
            confirmed: None,
            driver: None,
            gate: Arc::new(AsyncMutex::new(())),
            parked: None,
        }
    }
}

#[derive(Default)]
pub(super) struct Memberships {
    groups: HashMap<String, GroupMembership>,
    adopt_unsupported: bool,
    release_unsupported: bool,
}

impl Memberships {
    fn unsupported(&self, want: Want) -> bool {
        match want {
            Want::Held => self.adopt_unsupported,
            Want::Released => self.release_unsupported,
        }
    }

    /// The log wants `want` of `group`. The epoch of the task to start,
    /// when one is needed: not when the platform already confirmed it, a
    /// task already works on it, or the surface cannot.
    fn set(&mut self, group: &str, want: Want) -> Option<u64> {
        let unsupported = self.unsupported(want);
        let g = self
            .groups
            .entry(group.to_owned())
            .or_insert_with(GroupMembership::new);
        if g.want != Some(want) {
            g.want = Some(want);
            g.epoch += 1;
            g.parked = None;
        }
        let refused = matches!(g.parked, Some((ParkedReason::Rejected { .. }, _)));
        if unsupported || refused || g.confirmed == Some(want) || g.driver == Some(g.epoch) {
            return None;
        }
        g.driver = Some(g.epoch);
        Some(g.epoch)
    }

    fn is_current(&self, group: &str, epoch: u64) -> bool {
        self.groups.get(group).is_some_and(|g| g.epoch == epoch)
    }

    /// Something changed for `group` (a real message from it, a rebind):
    /// what was parked may be tried again.
    pub(super) fn unpark(&mut self, group: &str) {
        if let Some(g) = self.groups.get_mut(group) {
            g.parked = None;
        }
    }

    fn refused(&self, group: &str) -> bool {
        self.groups
            .get(group)
            .is_some_and(|g| matches!(g.parked, Some((ParkedReason::Rejected { .. }, _))))
    }

    /// The groups parked now, by id.
    pub(super) fn parked(&self) -> Vec<ParkedGroup> {
        let mut parked: Vec<ParkedGroup> = self
            .groups
            .iter()
            .filter_map(|(group_id, g)| {
                let (reason, since) = g.parked.clone()?;
                Some(ParkedGroup {
                    group_id: group_id.clone(),
                    reason,
                    since,
                })
            })
            .collect();
        parked.sort_by(|a, b| a.group_id.cmp(&b.group_id));
        parked
    }
}

enum Attempt {
    /// Settled, or no longer wanted, or cannot be done: stop.
    Done,
    /// Failed: try again after a pause.
    Retry,
}

/// A call's outcome, flattened.
enum Called {
    Adopted(AdoptedGroup),
    Released(bool),
    Failed(SurfaceError),
    TimedOut,
    Stopped,
}

impl Inner {
    /// The log wants `want` of `group`: start the task that makes it so,
    /// unless nothing needs doing. Called with the state locked.
    pub(super) fn want_membership(
        self: &Arc<Self>,
        memberships: &mut Memberships,
        group: &str,
        want: Want,
    ) {
        let Some(epoch) = memberships.set(group, want) else {
            return;
        };
        let delay = match want {
            Want::Held => Duration::ZERO,
            Want::Released => lock(&self.policy).release_grace,
        };
        tokio::spawn(self.clone().drive(group.to_owned(), want, epoch, delay));
    }

    async fn drive(self: Arc<Self>, group: String, want: Want, epoch: u64, delay: Duration) {
        let stop = lock(&self.stop).clone();
        let policy = *lock(&self.policy);
        if delay.is_zero() || !sleep_or_stop(delay, &stop).await {
            let mut pause = policy.retry_initial;
            loop {
                if stop.is_cancelled() {
                    break;
                }
                match self.attempt(&group, want, epoch, &policy, &stop).await {
                    Attempt::Done => break,
                    Attempt::Retry => {
                        if sleep_or_stop(pause, &stop).await {
                            break;
                        }
                        pause = policy.next_pause(pause);
                    }
                }
            }
        }
        let mut state = lock(&self.state);
        if let Some(g) = state.memberships.groups.get_mut(&group)
            && g.driver == Some(epoch)
        {
            g.driver = None;
        }
    }

    async fn attempt(
        &self,
        group: &str,
        want: Want,
        epoch: u64,
        policy: &MembershipPolicy,
        stop: &CancellationToken,
    ) -> Attempt {
        let gate = {
            let state = lock(&self.state);
            if !state.memberships.is_current(group, epoch) {
                return Attempt::Done;
            }
            state.memberships.groups[group].gate.clone()
        };
        let _turn = gate.lock_owned().await;
        // Decided only now, once it is this call's turn: what was wanted
        // may have changed, or been done, while waiting.
        let by = {
            let state = lock(&self.state);
            if !state.memberships.is_current(group, epoch)
                || state.memberships.groups[group].confirmed == Some(want)
                || state.memberships.unsupported(want)
            {
                return Attempt::Done;
            }
            state.latest_ref.get(group).cloned()
        };
        let by = match want {
            Want::Released => None,
            Want::Held => match by {
                Some(by) => Some(by),
                None => match self.derive_ref(group).await {
                    Ok(Some(by)) => Some(by),
                    Ok(None) => {
                        // Nothing names the group yet: its first real message will.
                        self.park(group, epoch, ParkedReason::NoRealMessage);
                        return Attempt::Done;
                    }
                    Err(error) => {
                        tracing::warn!(%group, %error, "cannot look a bound group's messages up; retrying");
                        return Attempt::Retry;
                    }
                },
            },
        };
        let called = match &by {
            Some(by) => self.call(stop, policy, self.surface.adopt_group(by)).await,
            None => {
                self.call_release(stop, policy, self.surface.release_group(group))
                    .await
            }
        };
        match called {
            Called::Adopted(AdoptedGroup {
                group_id,
                already_joined,
            }) => {
                tracing::debug!(%group_id, already_joined, "group adopted");
                self.confirm(group, Want::Held);
                Attempt::Done
            }
            Called::Released(removed) => {
                tracing::debug!(%group, removed, "group released");
                self.confirm(group, Want::Released);
                Attempt::Done
            }
            Called::Failed(SurfaceError::Unsupported(reason)) => {
                self.unsupported(want, &reason);
                Attempt::Done
            }
            Called::Failed(SurfaceError::Rejected(error)) => {
                // Asking again the same way would be refused the same way.
                self.park(group, epoch, ParkedReason::Rejected { error });
                Attempt::Done
            }
            Called::Failed(error) => {
                tracing::warn!(%group, %error, ?want, "cannot join or leave a group; will retry");
                Attempt::Retry
            }
            Called::TimedOut => {
                tracing::warn!(%group, ?want, timeout = ?policy.call_timeout, "the platform did not answer a join or leave call; will retry");
                Attempt::Retry
            }
            Called::Stopped => Attempt::Done,
        }
    }

    async fn call<F>(&self, stop: &CancellationToken, policy: &MembershipPolicy, call: F) -> Called
    where
        F: Future<Output = Result<AdoptedGroup, SurfaceError>>,
    {
        tokio::select! {
            () = stop.cancelled() => Called::Stopped,
            result = tokio::time::timeout(policy.call_timeout, call) => match result {
                Ok(Ok(adopted)) => Called::Adopted(adopted),
                Ok(Err(error)) => Called::Failed(error),
                Err(_) => Called::TimedOut,
            },
        }
    }

    async fn call_release<F>(
        &self,
        stop: &CancellationToken,
        policy: &MembershipPolicy,
        call: F,
    ) -> Called
    where
        F: Future<Output = Result<bool, SurfaceError>>,
    {
        tokio::select! {
            () = stop.cancelled() => Called::Stopped,
            result = tokio::time::timeout(policy.call_timeout, call) => match result {
                Ok(Ok(removed)) => Called::Released(removed),
                Ok(Err(error)) => Called::Failed(error),
                Err(_) => Called::TimedOut,
            },
        }
    }

    /// The platform said so: a fact, whatever has been wanted since.
    fn confirm(&self, group: &str, state: Want) {
        if let Some(g) = lock(&self.state).memberships.groups.get_mut(group) {
            g.confirmed = Some(state);
        }
    }

    /// Nothing more is tried for `group` until it is unparked; logged once.
    fn park(&self, group: &str, epoch: u64, reason: ParkedReason) {
        let mut state = lock(&self.state);
        let Some(g) = state.memberships.groups.get_mut(group) else {
            return;
        };
        if g.epoch != epoch || g.parked.as_ref().is_some_and(|(r, _)| *r == reason) {
            return;
        }
        let want = g.want;
        match &reason {
            ParkedReason::Rejected { error } => tracing::warn!(
                %group,
                %error,
                ?want,
                "the platform refused to join or leave the group; not retrying until a new message from it, a rebind, or a restart"
            ),
            ParkedReason::NoRealMessage => tracing::info!(
                %group,
                "no real message names the bound group yet (simulated ones cannot); joining on its first"
            ),
        }
        g.parked = Some((reason, chrono::Utc::now()));
    }

    fn unsupported(&self, want: Want, reason: &str) {
        let mut state = lock(&self.state);
        let flag = match want {
            Want::Held => &mut state.memberships.adopt_unsupported,
            Want::Released => &mut state.memberships.release_unsupported,
        };
        if !std::mem::replace(flag, true) {
            tracing::info!(
                reason,
                ?want,
                "this chat surface cannot join or leave groups; not asking again"
            );
        }
    }

    /// A message the platform delivered from `group`, found in the log: the
    /// latest real (never simulated) chat from it, the way a bound group is named for an adopt
    /// after a restart, before anyone has spoken there.
    async fn derive_ref(&self, group: &str) -> Result<Option<String>, HubError> {
        let Some(hub) = self.hub.borrow().clone() else {
            return Ok(None);
        };
        let committee = {
            let state = lock(&self.state);
            match state.table.committee_of(group) {
                Some(committee) => committee.clone(),
                None => return Ok(None),
            }
        };
        let mut found = None;
        let mut after = Seq::ZERO;
        loop {
            let page = hub.read(&committee, after, 500).await?;
            let Some(last) = page.last() else { break };
            after = last.seq;
            for event in &page {
                // A simulated message's ref is not the platform's to know.
                if let (Some(source), Some(mirror)) = (&event.source, &event.mirror)
                    && !source.simulated
                    && source.platform == self.platform
                    && source.group_id == group
                    && mirror.platform == self.platform
                {
                    found = Some(mirror.message_ref.clone());
                }
            }
        }
        Ok(found)
    }

    /// A post into `group` was refused: when the group is bound and the
    /// platform has not confirmed the bot's membership, adopt it now (waiting for an
    /// adopt already under way) and say the post may be tried once more.
    pub(super) async fn heal(self: &Arc<Self>, group: &str) -> bool {
        self.drain_fresh();
        let stop = lock(&self.stop).clone();
        let policy = *lock(&self.policy);
        let (gate, by) = {
            let mut state = lock(&self.state);
            if state.table.committee_of(group).is_none()
                || state.memberships.adopt_unsupported
                || state.memberships.refused(group)
            {
                return false;
            }
            let Some(by) = state.latest_ref.get(group).cloned() else {
                return false;
            };
            let g = state
                .memberships
                .groups
                .entry(group.to_owned())
                .or_insert_with(GroupMembership::new);
            if g.confirmed == Some(Want::Held) {
                // Joined: this refusal is about something else.
                return false;
            }
            (g.gate.clone(), by)
        };
        let _turn = gate.lock_owned().await;
        if lock(&self.state)
            .memberships
            .groups
            .get(group)
            .is_some_and(|g| g.confirmed == Some(Want::Held))
        {
            return true;
        }
        match self
            .call(&stop, &policy, self.surface.adopt_group(&by))
            .await
        {
            Called::Adopted(_) => {
                self.confirm(group, Want::Held);
                true
            }
            Called::Failed(SurfaceError::Rejected(error)) => {
                let epoch = lock(&self.state).memberships.groups[group].epoch;
                self.park(group, epoch, ParkedReason::Rejected { error });
                false
            }
            _ => false,
        }
    }
}

/// Sleep, or until `stop`; true when stopped.
async fn sleep_or_stop(pause: Duration, stop: &CancellationToken) -> bool {
    tokio::select! {
        () = stop.cancelled() => true,
        () = tokio::time::sleep(pause) => false,
    }
}
