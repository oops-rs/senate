//! `senate session pause` and `resume`: stop a hosted session so that it
//! can be brought back where it was, and bring it back.
//!
//! # Pause
//!
//! 1. A Senate note asks the session to finish its current step and write
//!    a checkpoint.
//! 2. The daemon waits until it has: a checkpoint the session wrote itself
//!    after the note, and then [`Timing::pause_quiet`] with no hook from
//!    it. It waits no longer than the timeout ([`PAUSE_TIMEOUT_SECS`]
//!    unless given), and not at all when the note could not be posted.
//! 3. The session is recorded paused ([`crate::state::Store::mark_paused`]),
//!    so the bridge's leave tells the server it is paused: its presence is
//!    `absent` with `paused`, away rules unchanged.
//! 4. Its terminal is stopped as `senate session stop` stops one (asked to
//!    end, then killed). What waits for it stays queued.
//!
//! A session that left no checkpoint in time is still recorded paused.
//!
//! # Resume
//!
//! Only a session recorded paused: it is restored into the same
//! conversation (as `senate session restore`), and a note points it at its
//! last checkpoint. A session stopped on purpose is never resumed.
//!
//! # Shutdown
//!
//! With `pause_on_shutdown`, SIGTERM pauses every running hosted session at
//! once, all within [`SHUTDOWN_PAUSE_SECS`](crate::config::SHUTDOWN_PAUSE_SECS). With `resume_on_start`, the
//! daemon resumes the sessions recorded paused once it is up.
//!
//! [`Timing::pause_quiet`]: super::Timing::pause_quiet

use super::host_policy::HostAuthority;
use super::hosts::StopTarget;
use super::*;
use crate::ipc::{HostedModel, PAUSE_TIMEOUT_SECS, PauseReply, ResumeReply};
use chrono::Utc;

/// How often a pause looks for the checkpoint it asked for.
const PAUSE_POLL: Duration = Duration::from_millis(100);
const NOTE_TIMEOUT: Duration = Duration::from_secs(4);

impl Shared {
    /// `senate session pause <senator>` ([`Request::Pause`]).
    pub(super) async fn on_pause(
        &self,
        senator: &AliasName,
        force: bool,
        timeout_secs: Option<u64>,
    ) -> Result<PauseReply, IpcError> {
        let auth = self
            .authorize_host_action(&SessionSelector::default())
            .await?;
        let target = self.resolve_stop(senator, None).await?;
        auth.check_target(&target.committee, false)?;
        let timeout = Duration::from_secs(timeout_secs.unwrap_or(PAUSE_TIMEOUT_SECS));
        self.pause_target(&auth, &target, force, Instant::now() + timeout, false)
            .await
    }

    /// Ask `target` to checkpoint, wait for it until `deadline` at most,
    /// record it paused and stop its terminal. A failed stop leaves it
    /// not paused, unless `shutdown`: then the daemon is going, and it is
    /// paused whatever happens to its terminal.
    async fn pause_target(
        &self,
        auth: &HostAuthority,
        target: &StopTarget,
        force: bool,
        deadline: Instant,
        shutdown: bool,
    ) -> Result<PauseReply, IpcError> {
        let asked = Instant::now();
        let primary = &target.senator;
        let committee = &target.committee;
        let secs = deadline.saturating_duration_since(asked).as_secs();
        let body = self.locale.pick(
            format!(
                "你的终端即将暂停，稍后会在同一个对话中恢复。请完成当前这一步，然后用 Senate 的 checkpoint 工具写一个检查点（让 PROGRESS、DECISIONS、OPEN 保持最新）。最多 {secs} 秒后终端会停止；在此期间请不要开始新的工作。"
            ),
            format!(
                "Your terminal is about to be paused, to be resumed later in this same conversation. Finish your current step, then write a checkpoint with the Senate checkpoint tool (keep PROGRESS, DECISIONS and OPEN current). The terminal stops within {secs} s; start nothing new before then."
            ),
        );
        let note = auth
            .note(committee.clone(), body)
            .to(Address::senator(primary));
        let asked_ok = match tokio::time::timeout(NOTE_TIMEOUT, self.post(note)).await {
            Ok(Ok(_)) => true,
            Ok(Err(error)) => {
                tracing::warn!(%primary, %error, "the pause note was not posted; pausing without waiting");
                false
            }
            Err(_) => {
                tracing::warn!(%primary, "the pause note was not acknowledged; pausing without waiting");
                false
            }
        };
        let checkpoint = if asked_ok {
            self.await_checkpoint(primary, asked, deadline).await
        } else {
            None
        };
        let waited_secs = asked.elapsed().as_secs();
        // Recorded before the stop, so the bridge's leave says paused.
        self.store
            .mark_paused(primary, committee, Utc::now(), checkpoint)
            .map_err(internal)?;
        match self.stop_audited(auth, target, force, true).await {
            Ok(_) => Ok(PauseReply {
                senator: primary.clone(),
                committee: committee.clone(),
                checkpoint,
                waited_secs,
            }),
            Err(error) => {
                // Still running: not paused.
                let paths = Paths::new(&self.home);
                if !shutdown
                    && self
                        .host_of(primary)
                        .is_some_and(|id| crate::host::running(&paths, &id).unwrap_or(true))
                    && let Err(e) = self.store.clear_paused(primary)
                {
                    tracing::warn!(%primary, error = %e, "clearing a failed pause");
                }
                Err(error)
            }
        }
    }

    /// The seq of the checkpoint `agent` writes after `asked`, once it has
    /// then been quiet for [`Timing::pause_quiet`](super::Timing): `None`
    /// when `deadline` comes first or its terminal ends.
    async fn await_checkpoint(
        &self,
        agent: &SenatorName,
        asked: Instant,
        deadline: Instant,
    ) -> Option<Seq> {
        let quiet = self.timing.pause_quiet;
        let paths = Paths::new(&self.home);
        let mut written = None;
        loop {
            let now = Instant::now();
            if let Some((at, seq)) = lock(&self.tool_checkpoints).get(agent).copied()
                && at >= asked
            {
                written = Some(seq);
                let since_hook = self
                    .store
                    .current_sessions()
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter(|s| &s.agent == agent)
                    .map(|s| (Utc::now() - s.last_seen).to_std().unwrap_or_default())
                    .min()
                    .unwrap_or(quiet);
                if now.saturating_duration_since(at) >= quiet && since_hook >= quiet {
                    return written;
                }
            }
            if now >= deadline {
                return written;
            }
            // Its terminal ended by itself: nothing more is coming.
            if let Some(id) = self.host_of(agent)
                && !crate::host::running(&paths, &id).unwrap_or(true)
            {
                return written;
            }
            tokio::time::sleep(PAUSE_POLL.min(deadline - now)).await;
        }
    }

    /// The terminal host of `agent`'s hosted session, if it has one.
    fn host_of(&self, agent: &SenatorName) -> Option<EventId> {
        let sessions = self.store.current_sessions().ok()?;
        let session = sessions
            .iter()
            .find(|s| &s.agent == agent && s.agent_source == AgentSource::Session)?;
        self.store.session_host(&session.session_id).ok().flatten()
    }

    /// `senate session resume <senator>` ([`Request::Resume`]).
    pub(super) async fn on_resume(&self, senator: &AliasName) -> Result<ResumeReply, IpcError> {
        let target = self.resolve_restore(senator, None).await?;
        let primary = target.session.agent.clone();
        let Some(paused) = self.store.paused(&primary).map_err(internal)? else {
            let who = Address::senator(&primary).in_text();
            return Err(IpcError::bad_request(self.locale.pick(
                format!("{who} 没有被暂停；要重新打开它，用 senate session restore。"),
                format!("{who} is not paused; senate session restore reopens it."),
            )));
        };
        let selector = SessionSelector::default();
        let reply = self
            .on_hosted_restore(
                &selector,
                &AliasName::bare(primary.clone()),
                HostedModel::default(),
            )
            .await?;
        let checkpoint = match paused.checkpoint {
            Some(seq) => Some(seq),
            None => tokio::time::timeout(
                NOTE_TIMEOUT,
                self.api.latest_checkpoint(&reply.committee, &primary),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
            .map(|c| c.seq),
        };
        let body = match (checkpoint, self.locale) {
            (Some(seq), Locale::Zh) => format!(
                "你已从暂停中恢复，仍是同一个对话。你最后的检查点是 seq {seq}；先读它（再 catch_up 看暂停期间的消息），从你停下的地方继续。"
            ),
            (Some(seq), Locale::En) => format!(
                "You are resumed after a pause, in the same conversation. Your last checkpoint is seq {seq}: read it (and catch_up on what came while you were paused), then pick up where you left off."
            ),
            (None, Locale::Zh) => "你已从暂停中恢复，仍是同一个对话。暂停前没有检查点；先 catch_up 看暂停期间的消息，再从你停下的地方继续。".to_owned(),
            (None, Locale::En) => "You are resumed after a pause, in the same conversation. You left no checkpoint before it: catch_up on what came while you were paused, then pick up where you left off.".to_owned(),
        };
        let auth = self.authorize_host_action(&selector).await?;
        let event = auth
            .note(reply.committee.clone(), body)
            .to(Address::senator(&primary));
        let note = match tokio::time::timeout(NOTE_TIMEOUT, self.post(event)).await {
            Ok(Ok(posted)) => Some(posted.seq),
            Ok(Err(error)) => {
                tracing::warn!(%primary, %error, "the resumed note was not posted");
                None
            }
            Err(_) => {
                tracing::warn!(%primary, "the resumed note was not acknowledged");
                None
            }
        };
        Ok(ResumeReply {
            session_id: Some(reply.session_id),
            senator: primary,
            committee: reply.committee,
            checkpoint,
            note,
        })
    }

    /// SIGTERM with `pause_on_shutdown`: pause every running hosted session
    /// at once, all within `cap` ([`SHUTDOWN_PAUSE_SECS`](crate::config::SHUTDOWN_PAUSE_SECS) unless a test
    /// says). One whose pause has not finished by then is recorded paused
    /// all the same (its terminal ends with the daemon or is stopped later).
    pub(super) async fn pause_all_within(self: &Arc<Self>, cap: Duration) {
        let deadline = Instant::now() + cap;
        let rows = match self.on_sessions(false).await {
            Ok(reply) => reply.rows,
            Err(error) => {
                tracing::warn!(%error, "listing hosted sessions to pause at shutdown");
                return;
            }
        };
        let running: Vec<_> = rows.into_iter().filter(|r| r.is_running()).collect();
        if running.is_empty() {
            return;
        }
        tracing::info!(
            count = running.len(),
            "pausing hosted sessions for shutdown"
        );
        // Each is recorded paused up front: one the cap cuts off is paused
        // all the same.
        for row in &running {
            if let Some(committee) = &row.committee
                && let Err(error) =
                    self.store
                        .mark_paused(&row.primary, committee, Utc::now(), None)
            {
                tracing::warn!(senator = %row.primary, %error, "recording a shutdown pause");
            }
        }
        let auth = match self
            .authorize_host_action(&SessionSelector::default())
            .await
        {
            Ok(auth) => Arc::new(auth),
            Err(error) => {
                tracing::warn!(%error, "pausing at shutdown: not authorized; recorded paused only");
                return;
            }
        };
        // Leave the last part of the cap for the stops themselves.
        let stop_margin = Duration::from_secs(15).min(cap / 4);
        let ask_until = deadline.checked_sub(stop_margin).unwrap_or(deadline);
        let mut tasks = tokio::task::JoinSet::new();
        for row in running {
            let shared = self.clone();
            let auth = auth.clone();
            tasks.spawn(async move {
                let name = AliasName::bare(row.primary.clone());
                let target = match shared.resolve_stop(&name, None).await {
                    Ok(target) => target,
                    Err(error) => {
                        tracing::warn!(senator = %row.primary, %error, "pausing at shutdown");
                        return;
                    }
                };
                if let Err(error) = shared
                    .pause_target(&auth, &target, true, ask_until, true)
                    .await
                {
                    tracing::warn!(senator = %row.primary, %error, "pausing at shutdown");
                }
            });
        }
        let all = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout_at(deadline, all).await.is_err() {
            tracing::warn!("the shutdown pause cap passed; the rest are recorded paused");
        }
    }

    /// `resume_on_start`: resume each session recorded paused, and none
    /// stopped on purpose.
    pub(super) async fn resume_paused_on_start(&self) {
        let paused = match self.store.all_paused() {
            Ok(paused) => paused,
            Err(error) => {
                tracing::warn!(%error, "reading paused sessions to resume");
                return;
            }
        };
        for p in paused {
            match self.on_resume(&AliasName::bare(p.agent.clone())).await {
                Ok(_) => tracing::info!(senator = %p.agent, "resumed a paused hosted session"),
                Err(error) => {
                    tracing::warn!(senator = %p.agent, %error, "resuming a paused hosted session");
                }
            }
        }
    }
}
