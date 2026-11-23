//! Launch hosted TUIs with a native identity and a committed Senate binding.
//! The terminal owner outlives the daemon that launched it.

use std::io::Read as _;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

use super::spawn_attempt::SpawnAttempt;
use super::*;
use crate::host;
use crate::ipc::{HostedModel, HostedRuntime, SpawnReply};

const HOST_NOTICE_FAILURE_LIMIT: u32 = 3;

#[derive(Default)]
pub(super) struct Launches(Mutex<BTreeMap<EventId, Pending>>);

struct Pending {
    session_id: Option<String>,
    published: watch::Sender<bool>,
    codex_restore: bool,
    /// A new hosted agy: its conversation ID comes from its first hook.
    unnamed: bool,
    named: watch::Sender<Option<String>>,
}

/// How long a new hosted agy has to name its conversation in a hook.
const AGY_NAMING_TIMEOUT: Duration = Duration::from_secs(15);

impl Launches {
    pub(super) fn hook_gate(&self, session_id: &str) -> Option<watch::Receiver<bool>> {
        lock(&self.0)
            .values()
            .find(|pending| {
                !pending.codex_restore && pending.session_id.as_deref() == Some(session_id)
            })
            .map(|pending| pending.published.subscribe())
    }

    /// Whether host `id` is still starting: not an orphan, whatever the
    /// session registry says yet.
    pub(super) fn launching(&self, id: &EventId) -> bool {
        lock(&self.0).contains_key(id)
    }

    /// A hosted agy's first hook names `session_id` as the conversation of
    /// launch `id`. True when that is what the launch now waits on (named
    /// now, or by this same ID before).
    pub(super) fn name_agy(&self, id: &EventId, session_id: &str) -> bool {
        let mut launches = lock(&self.0);
        let Some(pending) = launches.get_mut(id) else {
            return false;
        };
        if !pending.unnamed {
            return pending.session_id.as_deref() == Some(session_id);
        }
        pending.unnamed = false;
        pending.session_id = Some(session_id.to_owned());
        pending.named.send_replace(Some(session_id.to_owned()));
        true
    }

    pub(super) fn unpublished(&self, session_id: &str) -> bool {
        self.hook_gate(session_id)
            .is_some_and(|publication| !*publication.borrow())
    }
}

pub(super) enum LaunchAudit {
    Ready,
    RolledBack { reason: String },
}

struct PendingGuard<'a> {
    launches: &'a Launches,
    id: EventId,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        lock(&self.launches.0).remove(&self.id);
    }
}

pub(super) struct RestoreTarget {
    pub(super) committee: CommitteeId,
    pub(super) session: Session,
    pub(super) runtime: HostedRuntime,
    /// The model and effort it was last started with.
    pub(super) model: HostedModel,
}

pub(super) struct StopTarget {
    pub(super) committee: CommitteeId,
    pub(super) senator: SenatorName,
    runtime: Runtime,
    host_id: EventId,
}

pub(super) struct StopAuditGuard<'a> {
    shared: &'a Shared,
    notice: EventId,
}

impl Drop for StopAuditGuard<'_> {
    fn drop(&mut self) {
        lock(&self.shared.active_stop_notices).remove(&self.notice);
        self.shared.host_notices_ready.notify_one();
        self.shared.work.notify_one();
    }
}

struct HostResume {
    recorded_model: HostedModel,
    session_id: String,
    senator: SenatorName,
    start_empty: bool,
}

/// A registered launch that the caller can still roll back if a subsequent
/// Senate operation fails. The receipt sent to IPC clients has no process
/// handle, and the wait task survives the caller dropping its handle.
pub(super) struct HostedLaunch {
    pub(super) reply: SpawnReply,
    pub(super) process: HostProcess,
}

impl HostedLaunch {
    pub(super) fn finish(self) -> SpawnReply {
        drop(self.process);
        self.reply
    }
}

pub(super) struct HostProcess {
    stop: oneshot::Sender<oneshot::Sender<Result<(), IpcError>>>,
}

impl HostProcess {
    fn monitor(mut child: Child) -> Self {
        let (stop, requested) = oneshot::channel::<oneshot::Sender<_>>();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {}
                result = requested => match result {
                    Ok(ack) => { let _ = ack.send(stop_child(&mut child).await); }
                    Err(_) => { let _ = child.wait().await; }
                }
            }
        });
        Self { stop }
    }

    pub(super) async fn stop(self) -> Result<(), IpcError> {
        let (ack, result) = oneshot::channel();
        if self.stop.send(ack).is_err() {
            return Ok(()); // Already reaped by the monitor.
        }
        result.await.map_err(IpcError::internal)?
    }
}

async fn stop_child(child: &mut Child) -> Result<(), IpcError> {
    if let Some(pid) = child
        .id()
        .and_then(|pid| i32::try_from(pid).ok())
        .and_then(rustix::process::Pid::from_raw)
    {
        match rustix::process::kill_process(pid, rustix::process::Signal::TERM) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(IpcError::internal(error)),
        }
    }
    match tokio::time::timeout(Duration::from_secs(4), child.wait()).await {
        Ok(status) => status.map(|_| ()).map_err(IpcError::internal),
        Err(_) => child.kill().await.map_err(IpcError::internal),
    }
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn on_spawn_with<Fut>(
        &self,
        runtime: HostedRuntime,
        committee: CommitteeId,
        alias: Option<SenatorName>,
        cwd: PathBuf,
        model: HostedModel,
        limit: Option<usize>,
        attempt: Option<&mut SpawnAttempt>,
        audit: impl Fn(LaunchAudit, SpawnReply, HostedRuntime) -> Fut + Send,
    ) -> Result<HostedLaunch, IpcError>
    where
        Fut: std::future::Future<Output = Result<(), IpcError>> + Send,
    {
        // Validate before starting a child, and never let spawn create a
        // committee merely by turning on a stream for an unknown name.
        let me = tokio::time::timeout(Duration::from_secs(5), self.api.me())
            .await
            .map_err(|_| IpcError::new(codes::SERVER, "could not confirm the committee in time"))?
            .map_err(server_error)?;
        if !me.committees.contains(&committee) {
            return Err(IpcError::bad_request(format!(
                "unknown committee {committee}"
            )));
        }
        self.launch_host(
            runtime, committee, alias, cwd, None, model, limit, attempt, audit,
        )
        .await
    }

    /// The local session `name` stands for: its primary ID, or an alias
    /// resolved from `writer` (the caller's own committee, when it has one;
    /// an alias several agents have is refused with its candidates there).
    /// The runtime, path and committee must come from the local registry,
    /// including for a session started by hand.
    pub(super) async fn find_local_senator(
        &self,
        name: &AliasName,
        writer: Option<&CommitteeId>,
    ) -> Result<Session, IpcError> {
        let senator = &name.alias;
        let primary = if name.committee.is_none() && senator.is_session_name() {
            senator.clone()
        } else {
            let resolved = tokio::time::timeout(Duration::from_secs(5), self.api.aliases())
                .await
                .map_err(|_| {
                    IpcError::new(codes::SERVER, "could not resolve the senator alias in time")
                })?
                .map_err(server_error)?
                .resolve(name, writer);
            if resolved == AliasMatch::NotAlias {
                return Err(IpcError::new(
                    codes::NO_SESSION,
                    format!("unknown alias {name}"),
                ));
            }
            resolved
                .non_interactive(senator)
                .map_err(|e| IpcError::bad_request(e.to_string()))?
        };
        for session in self.store.current_sessions().map_err(internal)? {
            if session.agent == primary && session.agent_source == AgentSource::Session {
                if self.hosts.unpublished(&session.session_id) {
                    return Err(IpcError::bad_request(
                        "this senator is still being restored; try again after it publishes",
                    ));
                }
                return Ok(session);
            }
        }
        Err(IpcError::new(
            codes::NO_SESSION,
            format!("no local session record for {senator}; its directory is unknown"),
        ))
    }

    /// An ordinary hook can share the binding read lock with delivery.
    /// Recheck ownership under the write lock before changing the native ID.
    pub(super) async fn bind_claude_hook(
        &self,
        report: &HookReport,
        next: &mut SessionUpsert,
    ) -> Result<(tokio::sync::RwLockReadGuard<'_, ()>, Option<Session>), IpcError> {
        let paths = Paths::new(&self.home);
        let binding = self.session_bindings.read().await;
        let candidates = self.hosted_claude_reports(&paths, report)?;
        if candidates
            .iter()
            .any(|(native, _, _)| *native != report.session_id)
        {
            drop(binding);
            let write = self.session_bindings.write().await;
            let candidates = self.hosted_claude_reports(&paths, report)?;
            let switched = self.reconcile_hosted_claude_hook(&paths, report, next, &candidates)?;
            return Ok((tokio::sync::RwLockWriteGuard::downgrade(write), switched));
        }
        self.reconcile_hosted_claude_hook(&paths, report, next, &candidates)?;
        Ok((binding, None))
    }

    fn hosted_claude_reports(
        &self,
        paths: &Paths,
        report: &HookReport,
    ) -> Result<Vec<(String, EventId, bool)>, IpcError> {
        if report.claude_pid.is_none() && report.ancestor_pids.is_empty() {
            return Ok(Vec::new());
        }
        let mut candidates = Vec::new();
        for (native, id) in self.store.hosted_claude_bindings().map_err(internal)? {
            let Some(pid) = host::child_pid(paths, &id) else {
                continue;
            };
            let ancestors = &report.ancestor_pids;
            // A nearer, independently reported Claude process can have its own
            // session. An inherited or absent PID cannot establish that.
            let below_host = ancestors
                .iter()
                .take_while(|&&ancestor| ancestor != pid)
                .any(|&ancestor| report.claude_pid == Some(ancestor));
            if (report.claude_pid == Some(pid) || (ancestors.contains(&pid) && !below_host))
                && session::pid_alive(pid)
                && host::running(paths, &id).map_err(IpcError::internal)?
            {
                candidates.push((
                    native,
                    id,
                    owns_claude_hook(pid, report.claude_pid, ancestors),
                ));
            }
        }
        Ok(candidates)
    }

    fn reconcile_hosted_claude_hook(
        &self,
        paths: &Paths,
        report: &HookReport,
        next: &mut SessionUpsert,
        candidates: &[(String, EventId, bool)],
    ) -> Result<Option<Session>, IpcError> {
        let (native, id) = match candidates {
            [] => return Ok(None),
            [(native, id, verified)] if *native == report.session_id => {
                if *verified {
                    host::clear_context_reports(paths, id);
                }
                return Ok(None);
            }
            [(native, id, true)] => (native, id),
            _ => {
                let reason = "could not attribute the Claude context change to one hosted TUI";
                for (native, id, _) in candidates {
                    if *native != report.session_id {
                        report_unverified_switch(paths, id, &report.session_id, reason);
                    }
                }
                return Err(IpcError::bad_request(reason));
            }
        };
        if self.hosts.unpublished(native) {
            return Err(IpcError::bad_request(
                "the hosted launch is still being published",
            ));
        }
        let first = self
            .store
            .session(native)
            .map_err(internal)?
            .ok_or_else(|| IpcError::internal("the hosted seat has no session record"))?;
        let target = self.store.session(&report.session_id).map_err(internal)?;
        if let Some(target) = &target
            && target.agent == first.agent
        {
            // An out-of-order prompt from a superseded context is not an
            // explicit request to resume that context.
            if report.event != HookEvent::SessionStart || report.source.as_deref() != Some("resume")
            {
                let reason = "a previously registered native ID requires a verified resume hook";
                report_unverified_switch(paths, id, &report.session_id, reason);
                return Err(IpcError::bad_request(reason));
            }
            if target
                .claude_pid
                .is_some_and(|pid| Some(pid) != report.claude_pid && session::pid_alive(pid))
                || (target.state == SessionState::Stale
                    && target.socket.as_ref().is_some_and(|socket| socket.exists()))
            {
                return Err(refuse_verified_conflict(paths, id, report));
            }
        }
        let committee = first
            .committee()
            .ok_or_else(|| IpcError::internal("the hosted seat has no committee"))?;
        let chair = self.chair_of(committee);
        let headed_by_seat = chair.as_ref() == Some(&first.agent);
        let seat = Address::senator(&first.agent).in_text();
        let old_native = &first.session_id;
        let new_native = &report.session_id;
        let mut detail = match self.locale {
            Locale::En => format!(
                "Hosted Claude seat {seat} in `{committee}` changed session context from native ID `{old_native}` to `{new_native}`."
            ),
            Locale::Zh => format!(
                "委员会 `{committee}` 的托管 Claude 席位 {seat} 已更换会话上下文，原生 ID 从 `{old_native}` 切换到 `{new_native}`。"
            ),
        };
        if headed_by_seat {
            detail.push_str(self.locale.pick(
                " 该委员会主席的会话上下文已变更。",
                " The committee chair's session context changed.",
            ));
        }
        let mut notice = NewEvent::new(
            committee.clone(),
            Kind::Note,
            Address::senior(&self.config.person),
            detail,
        );
        // The stream normally knows the chair. If it has not caught up,
        // tell the desk about the context change without claiming a role.
        if headed_by_seat || chair.is_none() {
            notice.to.push(Address::Speaker);
        }
        next.agent = first.agent.clone();
        match self.store.switch_hosted_session(&first, next, id, &notice) {
            Ok(session) => {
                host::clear_context_reports(paths, id);
                self.host_notices_ready.notify_one();
                Ok(Some(session))
            }
            Err(StoreError::HostedNativeConflict) => {
                Err(refuse_verified_conflict(paths, id, report))
            }
            Err(error) => Err(internal(error)),
        }
    }

    pub(super) async fn flush_host_notices(&self) {
        if let Err(error) = self.recover_stop_notices() {
            tracing::warn!(%error, "recovering unconfirmed terminal stops");
        }
        let notices = match self.store.hosted_switch_notices() {
            Ok(notices) => notices,
            Err(error) => {
                tracing::warn!(%error, "reading hosted session notices");
                return;
            }
        };
        for notice in notices {
            let id = notice.id.clone();
            let rejection = match tokio::time::timeout(
                Duration::from_secs(1),
                self.api.append(&notice),
            )
            .await
            {
                Ok(Ok(_)) => {
                    if let Err(error) = self.store.remove_hosted_switch_notice(&id) {
                        tracing::warn!(%error, "marking hosted session notice as reported");
                    }
                    continue;
                }
                Ok(Err(error)) if error.definitive_notice_rejection() => error,
                Ok(Err(error)) => {
                    tracing::warn!(%id, %error, "posting hosted session notice, will retry");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(%id, "posting hosted session notice timed out, will retry");
                    continue;
                }
            };
            match self
                .store
                .fail_hosted_switch_notice(&id, HOST_NOTICE_FAILURE_LIMIT)
            {
                Ok(true) => {
                    tracing::warn!(%id, %rejection, "dropping hosted session notice notice after repeated rejections")
                }
                Ok(false) => {
                    tracing::warn!(%id, %rejection, "posting hosted session notice, will retry")
                }
                Err(error) => {
                    tracing::warn!(%id, %error, %rejection, "recording hosted session notice notice rejection")
                }
            }
        }
    }

    fn recover_stop_notices(&self) -> Result<(), StoreError> {
        let paths = Paths::new(&self.home);
        for (notice, host) in self.store.awaiting_stop_notices()? {
            if lock(&self.active_stop_notices).contains_key(&notice) {
                continue;
            }
            match host::running(&paths, &host) {
                Ok(false) => self.store.publish_stop_notice(&notice, None)?,
                Ok(true) => {}
                Err(error) => {
                    tracing::warn!(%error, %host, "checking a host with an unconfirmed stop")
                }
            }
        }
        Ok(())
    }

    pub(super) fn track_stop_notice(
        &self,
        notice: EventId,
        senator: &SenatorName,
    ) -> StopAuditGuard<'_> {
        lock(&self.active_stop_notices).insert(notice.clone(), senator.clone());
        StopAuditGuard {
            shared: self,
            notice,
        }
    }

    pub(super) async fn resolve_restore(
        &self,
        senator: &AliasName,
        writer: Option<&CommitteeId>,
    ) -> Result<RestoreTarget, IpcError> {
        let session = self.find_local_senator(senator, writer).await?;
        let runtime = match session.runtime {
            Runtime::Codex => HostedRuntime::Codex,
            Runtime::Claude => HostedRuntime::Claude,
            Runtime::Agy => HostedRuntime::Agy,
            Runtime::Acp => {
                return Err(IpcError::bad_request(
                    "only Codex, Claude Code and agy can be restored",
                ));
            }
        };
        let committee = session.committee().cloned().ok_or_else(|| {
            IpcError::bad_request(format!("no committee is recorded for {senator}"))
        })?;
        let model = self
            .store
            .hosted_model(&session.session_id)
            .map_err(internal)?;
        Ok(RestoreTarget {
            runtime,
            committee,
            session,
            model,
        })
    }

    pub(super) async fn resolve_stop(
        &self,
        senator: &AliasName,
        writer: Option<&CommitteeId>,
    ) -> Result<StopTarget, IpcError> {
        let session = self.find_local_senator(senator, writer).await?;
        let host_id = self
            .store
            .session_host(&session.session_id)
            .map_err(internal)?
            .ok_or_else(|| {
                IpcError::bad_request(format!(
                    "{senator} has no Senate-owned terminal. Exit it from its own terminal",
                ))
            })?;
        let paths = Paths::new(&self.home);
        if !host::running(&paths, &host_id).map_err(IpcError::internal)? {
            return Err(IpcError::new(
                codes::NO_SESSION,
                format!(
                    "no running terminal host for {senator}. Use senate session restore to resume it"
                ),
            ));
        }
        let committee = session.committee().cloned().ok_or_else(|| {
            IpcError::bad_request(format!("no committee is recorded for {senator}"))
        })?;
        Ok(StopTarget {
            committee,
            senator: session.agent,
            runtime: session.runtime,
            host_id,
        })
    }

    pub(super) async fn stop_host(
        &self,
        target: &StopTarget,
        force: bool,
        unconfirmed_audit: NewEvent,
    ) -> Result<(String, bool), IpcError> {
        let binding = if target.runtime == Runtime::Codex {
            // Drain a push that started before this stop was marked.
            tokio::sync::RwLockWriteGuard::downgrade(self.session_bindings.write().await)
        } else {
            self.session_bindings.read().await
        };
        let current = self
            .store
            .session_for_host(&target.host_id)
            .map_err(internal)?;
        let session = match current {
            Some(id) => self.store.session(&id).map_err(internal)?,
            None => None,
        };
        let Some(session) = session.filter(|session| {
            session.agent == target.senator
                && session.committee() == Some(&target.committee)
                && !self.hosts.unpublished(&session.session_id)
        }) else {
            return Err(IpcError::bad_request(
                "the hosted session's binding changed. Retry stop",
            ));
        };
        let paths = Paths::new(self.home.canonicalize().map_err(IpcError::internal)?);
        if host::awaiting_publication(&paths, &target.host_id) {
            return Err(IpcError::bad_request(
                "this terminal host is still waiting for publication",
            ));
        }
        let stop = host::control::begin_stop(
            &paths,
            &target.host_id,
            force,
            unconfirmed_audit,
            (session.runtime == Runtime::Codex)
                .then_some((&self.codex_transport, session.session_id.as_str())),
        )
        .await?;
        drop(binding);
        let recorded = host::control::wait_stopped(stop).await?;
        Ok((session.session_id, recorded))
    }

    pub(super) async fn on_restore_with<Fut>(
        &self,
        target: RestoreTarget,
        model: HostedModel,
        limit: Option<usize>,
        audit: impl Fn(LaunchAudit, SpawnReply, HostedRuntime) -> Fut + Send,
    ) -> Result<HostedLaunch, IpcError>
    where
        Fut: std::future::Future<Output = Result<(), IpcError>> + Send,
    {
        let session = target.session;
        self.refuse_running_host(&Paths::new(&self.home), &session.session_id)?;
        if (session.runtime == Runtime::Claude && claude_reachable(&session).await)
            || (session.runtime == Runtime::Agy
                && session::agy_conversation_open(&session.session_id) == Some(true))
        {
            return Err(IpcError::bad_request(
                "the original session is still running; exit it before restoring",
            ));
        }
        let start_empty = session.runtime == Runtime::Claude
            && self
                .store
                .session_host(&session.session_id)
                .map_err(internal)?
                .is_some()
            && !claude_has_transcript(&session)?;
        self.launch_host(
            target.runtime,
            target.committee,
            None,
            session.cwd,
            Some(HostResume {
                recorded_model: target.model,
                session_id: session.session_id,
                senator: session.agent,
                start_empty,
            }),
            model,
            limit,
            None,
            audit,
        )
        .await
    }

    fn refuse_running_host(&self, paths: &Paths, session_id: &str) -> Result<(), IpcError> {
        if let Some(id) = self.store.session_host(session_id).map_err(internal)?
            && host::running(paths, &id).map_err(IpcError::internal)?
        {
            if host::awaiting_publication(paths, &id) {
                return Err(IpcError::bad_request(
                    "this terminal host is still waiting for publication",
                ));
            }
            return Err(IpcError::bad_request(
                "this senator already has a running terminal host; use senate attach",
            ));
        }
        Ok(())
    }

    pub(super) async fn on_attach(&self, senator: AliasName) -> Result<PathBuf, IpcError> {
        let session = self.find_local_senator(&senator, None).await?;
        let paths = Paths::new(self.home.canonicalize().map_err(IpcError::internal)?);
        if let Some(id) = self
            .store
            .session_host(&session.session_id)
            .map_err(internal)?
            && host::running(&paths, &id).map_err(IpcError::internal)?
        {
            if host::awaiting_publication(&paths, &id) {
                // The acknowledgement is already in the pipe after a
                // successful launch. Allow the host to consume it.
                for _ in 0..20 {
                    if !host::awaiting_publication(&paths, &id) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                if host::awaiting_publication(&paths, &id) {
                    return Err(IpcError::bad_request(
                        "this terminal host is still waiting for publication",
                    ));
                }
            }
            return Ok(host::socket(&paths, &id));
        }
        Err(IpcError::new(
            codes::NO_SESSION,
            format!(
                "no running terminal host for {senator}; use senate session restore to resume it"
            ),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn launch_host<Fut>(
        &self,
        runtime: HostedRuntime,
        committee: CommitteeId,
        alias: Option<SenatorName>,
        cwd: PathBuf,
        resume: Option<HostResume>,
        model: HostedModel,
        limit: Option<usize>,
        mut attempt: Option<&mut SpawnAttempt>,
        audit: impl Fn(LaunchAudit, SpawnReply, HostedRuntime) -> Fut + Send,
    ) -> Result<HostedLaunch, IpcError>
    where
        Fut: std::future::Future<Output = Result<(), IpcError>> + Send,
    {
        if !cwd.is_absolute() {
            return Err(IpcError::bad_request(
                "the project directory must be absolute",
            ));
        }
        let cwd = cwd.canonicalize().map_err(|error| {
            IpcError::bad_request(format!(
                "cannot open project directory {}: {error}",
                cwd.display()
            ))
        })?;
        if !cwd.is_dir() {
            return Err(IpcError::bad_request(format!(
                "{} is not a directory",
                cwd.display()
            )));
        }
        let paths = Paths::new(self.home.canonicalize().map_err(IpcError::internal)?);
        let requested_model = model.model.clone();
        let mut model = resume
            .as_ref()
            .map(|r| r.recorded_model.overridden_by(&model))
            .unwrap_or(model);
        model.verification_warning = None;
        let (resume, seat, start_empty) = match resume {
            Some(resume) => (
                Some(resume.session_id),
                Some(resume.senator),
                resume.start_empty,
            ),
            None => (None, None, false),
        };
        let id = EventId::new();
        let (published, _) = watch::channel(false);
        let unnamed = runtime == HostedRuntime::Agy && resume.is_none();
        let (named, mut naming) = watch::channel(None);
        {
            let mut launches = lock(&self.hosts.0);
            if let Some(limit) = limit {
                Self::check_host_capacity(&paths, &launches, limit)?;
            }
            if let Some(id) = &resume {
                if launches
                    .values()
                    .any(|pending| pending.session_id.as_ref() == Some(id))
                {
                    return Err(IpcError::bad_request(
                        "this senator is already being restored",
                    ));
                }
                self.refuse_running_host(&paths, id)?;
            }
            launches.insert(
                id.clone(),
                Pending {
                    session_id: resume.clone(),
                    published: published.clone(),
                    codex_restore: resume.is_some() && runtime == HostedRuntime::Codex,
                    unnamed,
                    named,
                },
            );
        }
        let _pending = PendingGuard {
            launches: &self.hosts,
            id: id.clone(),
        };

        let executable = match runtime {
            HostedRuntime::Claude => self.companion_config.claude_bin.clone(),
            HostedRuntime::Codex => crate::delivery::codex::CodexConfig::load(&paths.config())
                .map_err(IpcError::internal)?
                .bin
                .unwrap_or_else(|| PathBuf::from("codex")),
            HostedRuntime::Agy => crate::config::AgyConfig::load(&paths.config())
                .map_err(IpcError::internal)?
                .bin(),
        };
        if !self.host_binary.is_file()
            || rustix::fs::access(&self.host_binary, rustix::fs::Access::EXEC_OK).is_err()
        {
            return Err(IpcError::internal(format!(
                "Senate CLI executable not found: {}",
                self.host_binary.display()
            )));
        }
        let program = host::preflight(&paths, &id, &executable).map_err(IpcError::internal)?;

        // The TUI shows the senator's alias as its own name. A restored
        // seat is named by its alias on this committee; not finding one
        // only leaves the session unnamed.
        let name = match (&alias, &seat) {
            (Some(alias), _) => Some(alias.to_string()),
            (None, Some(seat)) => tokio::time::timeout(Duration::from_secs(2), self.api.aliases())
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|aliases| aliases.alias_of(&committee, seat).map(ToString::to_string)),
            (None, None) => None,
        };
        // Keep the creating connection until the host has started. Delivery
        // and the TUI use the same shared server for the life of the session.
        let mut app_server = None;
        let restoring = resume.is_some();
        let mut session_id = match runtime {
            HostedRuntime::Claude => resume.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            // Until its first hook names it, a new agy's session is only
            // the runtime's prefix.
            HostedRuntime::Agy => {
                if let Some(request) = &requested_model {
                    let catalog = super::model_catalog::agy_models(&program, &cwd).await?;
                    model.model = Some(super::model_catalog::resolve(request, catalog)?);
                }
                resume.unwrap_or_else(|| Runtime::Agy.session_key(""))
            }
            HostedRuntime::Codex => {
                let connect = || crate::delivery::codex::AppServer::connect(&self.codex_transport);
                let mut server = tokio::time::timeout(
                    CODEX_PUSH_TIMEOUT,
                    crate::delivery::codex::AppServer::connect_with_settings(
                        &self.codex_transport,
                        !model.is_default(),
                    ),
                )
                .await
                .map_err(|_| {
                    IpcError::new(
                        codes::SERVER,
                        "timed out connecting to the Codex app-server",
                    )
                })?
                .map_err(IpcError::internal)?;
                if let Some(request) = &requested_model {
                    let catalog = tokio::time::timeout(CODEX_PUSH_TIMEOUT, server.model_ids())
                        .await
                        .map_err(|_| IpcError::internal("Codex model/list timed out"))?
                        .map_err(IpcError::internal)?;
                    model.model = Some(super::model_catalog::resolve(request, catalog)?);
                }
                let id = if let Some(id) = resume {
                    id
                } else {
                    if let Some(attempt) = attempt.as_deref_mut() {
                        attempt.begin_launch(&self.store)?;
                    }
                    let id = tokio::time::timeout(CODEX_PUSH_TIMEOUT, server.create_thread(&cwd))
                        .await
                        .map_err(|_| {
                            IpcError::new(codes::SERVER, "timed out creating the Codex thread")
                        })?
                        .map_err(IpcError::internal)?;
                    let seeded =
                        tokio::time::timeout(CODEX_PUSH_TIMEOUT, server.seed_hosted_thread(&id))
                            .await;
                    if !matches!(seeded, Ok(Ok(()))) {
                        // The thread ID is known even if the injection response
                        // was lost. Do not reuse a cancelled WebSocket read.
                        let mut rollback_errors = Vec::new();
                        record_rollback(&mut rollback_errors, "archive Codex thread", async {
                            connect().await?.archive_thread(&id).await
                        })
                        .await;
                        let error = match seeded {
                            Ok(Err(error)) => IpcError::internal(error),
                            _ => IpcError::new(codes::SERVER, "timed out seeding the Codex thread"),
                        };
                        return if rollback_errors.is_empty() {
                            Err(error)
                        } else {
                            Err(IpcError::new(
                                &error.code,
                                format!("{error}. Rollback: {}", rollback_errors.join(", ")),
                            ))
                        };
                    }
                    if let Some(name) = &name {
                        // The name is for people; a thread without one works.
                        let named =
                            tokio::time::timeout(CODEX_PUSH_TIMEOUT, server.name_thread(&id, name))
                                .await;
                        if !matches!(named, Ok(Ok(()))) {
                            tracing::debug!(%id, name, "the Codex thread could not be named");
                        }
                    }
                    id
                };
                app_server = Some(server);
                id
            }
        };
        if !unnamed && let Some(pending) = lock(&self.hosts.0).get_mut(&id) {
            pending.session_id = Some(session_id.clone());
        }
        let remote = (runtime == HostedRuntime::Codex).then(|| self.codex_transport.remote());
        let launch_session = session_id.clone();
        let launch = host::SessionCommand {
            session_id: &launch_session,
            program: &program,
            resume: restoring && !start_empty,
            codex_remote: remote.as_deref(),
            name: name.as_deref(),
            model: &model,
            prompt_quiet: self.timing.screen_prompt_quiet,
        };
        let session_runtime = Runtime::from(runtime);
        let mut reply = SpawnReply {
            display_name: name.clone(),
            model_warning: None,
            resolved_model: model.model.clone().filter(|id| {
                requested_model
                    .as_ref()
                    .is_some_and(|request| request != id)
            }),
            senator: seat.unwrap_or_else(|| agent::session_name(session_runtime, &session_id)),
            session_id: session_id.clone(),
            committee: committee.clone(),
            request: None,
        };
        let facts = session::gather_git_facts(&cwd, Duration::from_secs(2)).await;
        let inferred = session::infer_committee(
            facts.branch.as_deref(),
            facts.committee_file.as_deref(),
            self.convention.as_ref(),
        );
        let mut start_terminal = |cwd: &Path| {
            host::start(
                &self.host_binary,
                &paths,
                &id,
                runtime,
                cwd,
                &launch,
                || {
                    // Codex crosses the creation boundary at thread/start.
                    if runtime != HostedRuntime::Codex
                        && let Some(attempt) = attempt.as_deref_mut()
                    {
                        attempt.begin_launch(&self.store)?;
                    }
                    Ok(())
                },
            )
            .map_err(|error| {
                error
                    .downcast::<IpcError>()
                    .unwrap_or_else(IpcError::internal)
            })
        };
        let mut process = None;
        let mut publication = None;
        let mut audit_written = false;
        let mut alias_may_have_landed = false;
        let mut staged = None;
        let mut recorded_model = model.clone();
        let result: Result<(), IpcError> = async {
            if unnamed {
                // A new agy names its conversation only once it runs: start
                // it, then take the ID its first hook reports.
                let (child, errors) = start_terminal(&cwd)?;
                let (ready, ack) = await_ready(child, errors).await?;
                process = Some(ready);
                publication = Some(ack);
                let named = tokio::time::timeout(AGY_NAMING_TIMEOUT, naming.wait_for(Option::is_some))
                    .await
                    .map_err(|_| IpcError::new(codes::SERVER, "agy did not name its conversation in time; are Senate's agy hooks installed (senate setup install)?"))?
                    .map_err(IpcError::internal)?
                    .clone()
                    .ok_or_else(|| IpcError::internal("agy named no conversation"))?;
                session_id = named;
                reply.session_id = session_id.clone();
                reply.senator = agent::session_name(session_runtime, &session_id);
            }
            let cursor = if self.store.has_cursor(&reply.senator, &reply.committee).map_err(internal)? {
                None
            } else {
                let generation = *self.generation.read().await;
                let (delivered, reported) = tokio::time::timeout(Duration::from_secs(5), async {
                    if restoring {
                        let (delivered, reported, _) = self.seed_point(&reply.senator, &reply.committee, true).await?;
                        Ok((delivered, reported))
                    } else {
                        Ok((self.head(&reply.committee).await?, Seq::ZERO))
                    }
                }).await.map_err(|_| IpcError::new(codes::SERVER, "timed out preparing the senator's inbox"))?
                    .map_err(intake_error)?;
                Some((generation, (delivered, reported)))
            };
            if !unnamed {
                let (child, errors) = start_terminal(&cwd)?;
                let (ready, ack) = await_ready(child, errors).await?;
                process = Some(ready);
                publication = Some(ack);
            }
            if let Some(server) = &mut app_server
                && !model.is_default()
            {
                tokio::time::timeout(CODEX_PUSH_TIMEOUT, async {
                    if restoring {
                        server.hosted_settings(&session_id).await?;
                    }
                    server.update_hosted_settings(&session_id, &model).await
                }).await
                    .map_err(|_| IpcError::internal("timed out applying hosted Codex model settings"))?
                    .map_err(IpcError::internal)?;
                reply.model_warning = match tokio::time::timeout(
                    CODEX_PUSH_TIMEOUT, server.hosted_settings(&session_id)
                ).await {
                    Ok(Ok(effective)) => crate::delivery::codex::AppServer::hosted_settings_warning(&effective, &model),
                    _ => Some("Codex: could not verify the requested model settings after launch".into()),
                };
                recorded_model.verification_warning = reply.model_warning.clone();
            }
            // The hosted Claude hook can wait for this bounded phase within
            // its own IPC deadline. Existing Codex hooks do not wait here.
            tokio::time::timeout(Duration::from_secs(7), async {
            audit(LaunchAudit::Ready, reply.clone(), runtime).await?;
            audit_written = true;
            if !host::running(&paths, &id).map_err(IpcError::internal)?
                || !host::child_pid(&paths, &id).is_some_and(session::pid_alive)
            {
                return Err(IpcError::internal(
                    "terminal host exited before publication",
                ));
            }
            // This launch's hook is waiting on its own publication
            // channel; unrelated hooks and delivery can keep running.
            let binding = self.session_bindings.write().await;
            if !restoring && self.store.session(&session_id).map_err(internal)?.is_some() {
                return Err(IpcError::internal(
                    "the newly allocated native session already exists",
                ));
            }
            // Native SessionEnd may have run while the restore was waiting
            // for the new terminal. Inspect the prior registration under
            // the same lock used by SessionEnd.
            let shared_codex = restoring
                && runtime == HostedRuntime::Codex
                && self
                    .store
                    .session(&session_id)
                    .map_err(internal)?
                    .is_some_and(|session| session.is_live())
                && self
                    .store
                    .session_host(&session_id)
                    .map_err(internal)?
                    .is_none();
            if shared_codex {
                host::mark_shared_codex_thread(&paths, &id).map_err(IpcError::internal)?;
            }
            let epoch = match cursor {
                Some((generation, _)) => Some(self.intake_guard(generation).await.map_err(intake_error)?),
                None => None,
            };
            self.store
                .register_hosted_session(
                    &SessionUpsert {
                        session_id: session_id.clone(),
                        cwd,
                        git_root: facts.toplevel,
                        branch: facts.branch,
                        inferred_committee: inferred,
                        socket: None,
                        token: None,
                        claude_pid: None,
                        transcript_path: None,
                        ancestor_pids: vec![],
                        runtime: session_runtime,
                        agent: reply.senator.clone(),
                        agent_source: AgentSource::Session,
                    },
                    &id,
                    &reply.committee,
                    cursor.map(|(_, seqs)| seqs),
                    &recorded_model,
                )
                .map_err(internal)?;
            if !restoring {
                staged = Some(cursor.is_some());
            }
            drop(epoch);
            drop(binding);

            // The server's alias operation also records its owner. Make it
            // the last fallible publication step, after the local transaction.
            // During the call, this launch stays hidden behind its gate.
            if let Some(alias) = &alias {
                alias_may_have_landed = true;
                match tokio::time::timeout(
                    Duration::from_secs(5),
                    self.api.set_alias(&reply.senator, alias, Some(&committee), false),
                )
                .await
                {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        alias_may_have_landed = error.status().is_none_or(|status| status >= 500);
                        return Err(server_error(error));
                    }
                    Err(_) => {
                        alias_may_have_landed = true;
                        return Err(self.join_name_timeout());
                    }
                }
            }
                Ok::<_, IpcError>(())
            }).await.map_err(|_| IpcError::new(codes::SERVER, "timed out publishing the hosted session"))??;
            // The publication is committed. Losing the host at this point
            // is a liveness failure, not grounds to undo an acknowledged alias.
            if restoring && runtime == HostedRuntime::Agy {
                // --conversation opens at the input prompt without a seed turn or Stop hook.
                self.mark_agy_idle(&session_id);
            }
            published.send_replace(true);
            if let Some(mut ack) = publication.take()
                && !tokio::time::timeout(Duration::from_secs(1), ack.write_all(&[host::PUBLISHED]))
                    .await.is_ok_and(|result| result.is_ok())
            {
                tracing::warn!(session = %session_id, "the published terminal host closed its acknowledgement pipe");
            }
            // Do not race the first stream reconciliation against the
            // host consuming the acknowledgement.
            for _ in 0..20 {
                if !host::awaiting_publication(&paths, &id) { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            drop(publication);
            let mut rollback = Vec::new();
            if let Some(process) = process
                && let Err(stop) = process.stop().await
            {
                rollback.push(format!("could not stop terminal host: {stop}"));
            }
            if let Some(seed_cursor) = staged
                && let Err(error) = self.store.discard_hosted_launch(&reply, &id, seed_cursor)
            {
                rollback.push(format!(
                    "could not discard the provisional binding: {error}"
                ));
            }
            if alias_may_have_landed && let Some(alias) = &alias {
                record_rollback(
                    &mut rollback,
                    "remove alias",
                    self.api
                        .remove_alias(&reply.senator, alias, Some(&committee)),
                )
                .await;
            }
            if !restoring && app_server.is_some() {
                record_rollback(&mut rollback, "archive Codex thread", async {
                    crate::delivery::codex::AppServer::connect(&self.codex_transport)
                        .await?
                        .archive_thread(&session_id)
                        .await
                })
                .await;
            }
            if audit_written {
                record_rollback(
                    &mut rollback,
                    "record launch rollback",
                    audit(
                        LaunchAudit::RolledBack {
                            reason: error.message.clone(),
                        },
                        reply,
                        runtime,
                    ),
                )
                .await;
            }
            return if rollback.is_empty() {
                Err(error)
            } else {
                Err(IpcError::new(
                    &error.code,
                    format!("{error}. Rollback: {}", rollback.join(", ")),
                ))
            };
        }
        self.committees_changed.notify_one();
        self.work.notify_one();
        if runtime == HostedRuntime::Codex {
            self.reconcile_approvals();
        }
        Ok(HostedLaunch {
            reply,
            process: process.ok_or_else(|| IpcError::internal("missing terminal host"))?,
        })
    }

    /// Count pending launches and running hosts once, under the same lock
    /// used to reserve a launch. A host can hold its owner lock while the
    /// reservation is still pending, including across daemon restarts.
    fn check_host_capacity(
        paths: &Paths,
        launches: &BTreeMap<EventId, Pending>,
        limit: usize,
    ) -> Result<(), IpcError> {
        let mut used: BTreeSet<EventId> = launches.keys().cloned().collect();
        used.extend(host::running_ids(paths).map_err(IpcError::internal)?);
        if used.len() >= limit {
            return Err(IpcError::bad_request(format!(
                "the machine is at its hosted session limit ({limit})"
            )));
        }
        Ok(())
    }
}

fn intake_error(error: Intake) -> IpcError {
    match error {
        Intake::Resync(reason) => IpcError::new(codes::SERVER, reason.to_string()),
        Intake::Failed(error) => IpcError::internal(error),
    }
}

fn report_unverified_switch(paths: &Paths, host: &EventId, native: &str, reason: &str) {
    if let Err(error) = host::note_unverified_switch(paths, host, native) {
        tracing::warn!(%error, %host, "recording a refused hosted Claude context change");
    }
    tracing::warn!(%host, %native, %reason, "keeping the existing hosted Claude binding");
}

fn refuse_verified_conflict(paths: &Paths, host: &EventId, report: &HookReport) -> IpcError {
    let reason = StoreError::HostedNativeConflict.to_string();
    report_unverified_switch(paths, host, &report.session_id, &reason);
    // A verified context transition can leave the inbox pointing at the
    // rejected conversation. A prompt alone can be a delayed report.
    if report.event == HookEvent::SessionStart
        && matches!(report.source.as_deref(), Some("clear" | "resume"))
        && let Err(error) = host::note_unbound_context(paths, host, &report.session_id)
    {
        tracing::warn!(%error, %host, "recording an unbound Claude context");
    }
    IpcError::bad_request(reason)
}

fn owns_claude_hook(tui: u32, reported: Option<u32>, ancestors: &[u32]) -> bool {
    // Claude sets CLAUDE_PID to the runtime that called the hook, even
    // for a nested headless Claude. Mere descent under the TUI is not
    // evidence that the TUI itself changed its native identity.
    reported == Some(tui) && ancestors.contains(&tui)
}

fn claude_has_transcript(session: &Session) -> Result<bool, IpcError> {
    let recorded = session
        .transcript_path
        .as_ref()
        .map(|path| session.cwd.join(path));
    if let Some(path) = &recorded
        && path.try_exists().map_err(IpcError::internal)?
    {
        return Ok(true);
    }
    let native = uuid::Uuid::parse_str(&session.session_id)
        .map_err(|_| IpcError::bad_request("the hosted Claude session has an invalid native ID"))?;
    let profile = std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
        .ok_or_else(|| IpcError::internal("cannot locate Claude Code's profile"))?;
    let projects = session.cwd.join(profile).join("projects");
    let recorded_projects = recorded
        .as_deref()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .filter(|path| path.ends_with("projects") && *path != projects);
    // Claude 2.1.327 accepts a duplicate UUID in another project. Search
    // the profile before opening a fresh conversation with a recorded ID.
    for root in std::iter::once(projects.as_path()).chain(recorded_projects) {
        if profile_has_transcript(root, native).map_err(IpcError::internal)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn profile_has_transcript(projects: &Path, native: uuid::Uuid) -> std::io::Result<bool> {
    let dirs = match std::fs::read_dir(projects) {
        Ok(dirs) => dirs,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let name = format!("{native}.jsonl");
    for dir in dirs {
        match dir?.path().join(&name).try_exists() {
            Ok(true) => return Ok(true),
            Ok(false) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

async fn record_rollback<E: std::fmt::Display>(
    errors: &mut Vec<String>,
    action: &str,
    operation: impl std::future::Future<Output = Result<(), E>>,
) {
    match tokio::time::timeout(Duration::from_secs(5), operation).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => errors.push(format!("could not {action}: {error}")),
        Err(_) => errors.push(format!("timed out attempting to {action}")),
    }
}

async fn claude_reachable(session: &Session) -> bool {
    if session.is_live() && session.claude_pid.is_some_and(session::pid_alive) {
        return true;
    }
    let Some(socket) = &session.socket else {
        return false;
    };
    crate::testenv::confined(socket)
        && tokio::time::timeout(Duration::from_millis(300), UnixStream::connect(socket))
            .await
            .is_ok_and(|result| result.is_ok())
}

async fn await_ready(
    mut child: Child,
    errors: std::fs::File,
) -> Result<(HostProcess, ChildStdin), IpcError> {
    let Some(ack) = child.stdin.take() else {
        let _ = stop_child(&mut child).await;
        return Err(IpcError::internal(
            "the terminal host has no publication pipe",
        ));
    };
    let Some(mut ready) = child.stdout.take() else {
        let _ = stop_child(&mut child).await;
        return Err(IpcError::internal(
            "the terminal host has no readiness pipe",
        ));
    };
    let result = tokio::select! {
        status = child.wait() => status.map_err(IpcError::internal).and_then(|status| {
            Err(IpcError::internal(format!("the terminal host exited during startup ({status})")))
        }),
        marker = tokio::time::timeout(host::START_TIMEOUT, ready.read_u8()) => match marker {
            Ok(Ok(host::READY)) => Ok(()),
            Ok(_) => Err(IpcError::internal("the terminal host closed its startup pipe")),
            Err(_) => Err(IpcError::internal("timed out starting the terminal host")),
        },
    };
    let process = HostProcess::monitor(child);
    if let Err(error) = result {
        if let Err(stop_error) = process.stop().await {
            tracing::warn!(%stop_error, "stopping a failed terminal launch");
        }
        let mut details = String::new();
        let _ = errors.take(4096).read_to_string(&mut details);
        return if details.trim().is_empty() {
            Err(error)
        } else {
            Err(IpcError::new(
                &error.code,
                format!("{error}: {}", details.trim()),
            ))
        };
    }
    Ok((process, ack))
}

#[cfg(test)]
mod tests {
    use super::{owns_claude_hook, profile_has_transcript};

    #[test]
    fn clear_from_the_tui_matches_but_a_nested_claude_does_not() {
        assert!(owns_claude_hook(10, Some(10), &[11, 10]));
        assert!(!owns_claude_hook(10, Some(12), &[12, 11, 10]));
        assert!(!owns_claude_hook(10, Some(10), &[12, 11]));
        assert!(!owns_claude_hook(10, None, &[10]));
    }

    #[test]
    fn a_saved_claude_id_is_found_across_project_directories() {
        let profile = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4();
        let root = profile.path().join("projects");
        let a = root.join("project-a");
        let b = root.join("project-b");
        assert!(!profile_has_transcript(&root, id).unwrap());
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(root.join("index.json"), "{}").unwrap();
        assert!(!profile_has_transcript(&root, id).unwrap());
        std::fs::write(a.join(format!("{id}.jsonl")), "{}\n").unwrap();
        assert!(!b.join(format!("{id}.jsonl")).exists());
        assert!(profile_has_transcript(&root, id).unwrap());
    }
}
