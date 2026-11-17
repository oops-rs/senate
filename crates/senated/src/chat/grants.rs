//! Bind = grant: keeping the platform's grants in line with the log.
//!
//! The gateway decides *what* each group's grant should be (held while the
//! log binds the group, given back once it binds it to nothing); this module
//! makes it so, without ever holding anything up:
//!
//! - every change runs in a task of its own, one task per group at a time,
//!   so a slow or hanging platform never blocks the Speaker, an append, or
//!   another group;
//! - a release waits [`GrantPolicy::release_grace`] first, and a bind within
//!   it cancels it, so a group moved from one committee to another (an
//!   unbind, then a bind) keeps its grant throughout and is never adopted
//!   twice;
//! - a failed adopt or release is retried with a doubling pause (from
//!   [`GrantPolicy::retry_initial`] up to [`GrantPolicy::retry_max`], with no
//!   end) until it succeeds or the log's bindings make it unwanted;
//! - calls for one group are made one at a time and in the order wanted, and
//!   each is bounded by [`GrantPolicy::call_timeout`], so a call that never
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

/// How the gateway paces the platform's grants. See the [module
/// docs](self) and [`ChatGateway::with_grant_policy`](super::ChatGateway::with_grant_policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantPolicy {
    /// How long a group bound to nothing keeps its grant before it is
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

impl Default for GrantPolicy {
    fn default() -> Self {
        Self {
            release_grace: Duration::from_secs(3),
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            call_timeout: Duration::from_secs(30),
        }
    }
}

impl GrantPolicy {
    fn next_pause(&self, pause: Duration) -> Duration {
        pause.saturating_mul(2).min(self.retry_max)
    }
}

/// What the log wants of a group's grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Want {
    Held,
    Released,
}

struct GroupGrant {
    /// What the log wants now; `epoch` changes whenever this does.
    want: Option<Want>,
    epoch: u64,
    /// What the platform last confirmed, if it has since this run began.
    confirmed: Option<Want>,
    /// The epoch of the task working towards `want`, if one is alive.
    driver: Option<u64>,
    /// Held across each call for this group, so calls are made in turn.
    gate: Arc<AsyncMutex<()>>,
}

impl GroupGrant {
    fn new() -> Self {
        Self {
            want: None,
            epoch: 0,
            confirmed: None,
            driver: None,
            gate: Arc::new(AsyncMutex::new(())),
        }
    }
}

#[derive(Default)]
pub(super) struct Grants {
    groups: HashMap<String, GroupGrant>,
    adopt_unsupported: bool,
    release_unsupported: bool,
}

impl Grants {
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
            .or_insert_with(GroupGrant::new);
        if g.want != Some(want) {
            g.want = Some(want);
            g.epoch += 1;
        }
        if unsupported || g.confirmed == Some(want) || g.driver == Some(g.epoch) {
            return None;
        }
        g.driver = Some(g.epoch);
        Some(g.epoch)
    }

    fn is_current(&self, group: &str, epoch: u64) -> bool {
        self.groups.get(group).is_some_and(|g| g.epoch == epoch)
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
    pub(super) fn want_grant(self: &Arc<Self>, grants: &mut Grants, group: &str, want: Want) {
        let Some(epoch) = grants.set(group, want) else {
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
        if let Some(g) = state.grants.groups.get_mut(&group)
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
        policy: &GrantPolicy,
        stop: &CancellationToken,
    ) -> Attempt {
        let gate = {
            let state = lock(&self.state);
            if !state.grants.is_current(group, epoch) {
                return Attempt::Done;
            }
            state.grants.groups[group].gate.clone()
        };
        let _turn = gate.lock_owned().await;
        // Decided only now, once it is this call's turn: what was wanted
        // may have changed, or been done, while waiting.
        let by = {
            let state = lock(&self.state);
            if !state.grants.is_current(group, epoch)
                || state.grants.groups[group].confirmed == Some(want)
                || state.grants.unsupported(want)
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
                        // Nothing names the group yet: its first message will.
                        tracing::debug!(%group, "no message known to adopt the group by yet");
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
                already_granted,
            }) => {
                tracing::debug!(%group_id, already_granted, "group adopted");
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
            Called::Failed(error) => {
                tracing::warn!(%group, %error, ?want, "cannot change a group's grant; will retry");
                Attempt::Retry
            }
            Called::TimedOut => {
                tracing::warn!(%group, ?want, timeout = ?policy.call_timeout, "the platform did not answer a grant call; will retry");
                Attempt::Retry
            }
            Called::Stopped => Attempt::Done,
        }
    }

    async fn call<F>(&self, stop: &CancellationToken, policy: &GrantPolicy, call: F) -> Called
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
        policy: &GrantPolicy,
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
        if let Some(g) = lock(&self.state).grants.groups.get_mut(group) {
            g.confirmed = Some(state);
        }
    }

    fn unsupported(&self, want: Want, reason: &str) {
        let mut state = lock(&self.state);
        let flag = match want {
            Want::Held => &mut state.grants.adopt_unsupported,
            Want::Released => &mut state.grants.release_unsupported,
        };
        if !std::mem::replace(flag, true) {
            tracing::info!(
                reason,
                ?want,
                "this chat surface does not grant groups; not asking again"
            );
        }
    }

    /// A message the platform delivered from `group`, found in the log: the
    /// latest chat from it, the way a bound group is named for an adopt
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
                if let (Some(source), Some(mirror)) = (&event.source, &event.mirror)
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
    /// platform has not confirmed its grant, adopt it now (waiting for an
    /// adopt already under way) and say the post may be tried once more.
    pub(super) async fn heal(self: &Arc<Self>, group: &str) -> bool {
        self.drain_fresh();
        let stop = lock(&self.stop).clone();
        let policy = *lock(&self.policy);
        let (gate, by) = {
            let mut state = lock(&self.state);
            if state.table.committee_of(group).is_none() || state.grants.adopt_unsupported {
                return false;
            }
            let Some(by) = state.latest_ref.get(group).cloned() else {
                return false;
            };
            let g = state
                .grants
                .groups
                .entry(group.to_owned())
                .or_insert_with(GroupGrant::new);
            if g.confirmed == Some(Want::Held) {
                // Granted: this refusal is about something else.
                return false;
            }
            (g.gate.clone(), by)
        };
        let _turn = gate.lock_owned().await;
        if lock(&self.state)
            .grants
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
