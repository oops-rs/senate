//! Launch hosted TUIs with a native identity and a committed Senate binding.
//! The terminal owner outlives the daemon that launched it.

use std::io::Read as _;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

use super::*;
use crate::host;
use crate::ipc::{HostedRuntime, SpawnReply};

const HOST_NOTICE_FAILURE_LIMIT: u32 = 3;

#[derive(Default)]
pub(super) struct Launches(Mutex<BTreeMap<EventId, Pending>>);

struct Pending {
    session_id: Option<String>,
    published: watch::Sender<bool>,
    codex_restore: bool,
}

impl Launches {
    pub(super) fn hook_gate(&self, session_id: &str) -> Option<watch::Receiver<bool>> {
        lock(&self.0)
            .values()
            .find(|pending| {
                !pending.codex_restore && pending.session_id.as_deref() == Some(session_id)
            })
            .map(|pending| pending.published.subscribe())
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
    runtime: HostedRuntime,
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
    pub(super) async fn on_spawn_with<Fut>(
        &self,
        runtime: HostedRuntime,
        committee: CommitteeId,
        alias: Option<SenatorName>,
        cwd: PathBuf,
        limit: Option<usize>,
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
        self.launch_host(runtime, committee, alias, cwd, None, limit, audit)
            .await
    }

    /// Aliases are Senate-wide. The runtime, path and committee must come
    /// from the local registry, including for a session started by hand.
    async fn find_local_senator(&self, senator: &SenatorName) -> Result<Session, IpcError> {
        let primary = if senator.is_session_name() {
            senator.clone()
        } else {
            tokio::time::timeout(Duration::from_secs(5), self.api.aliases())
                .await
                .map_err(|_| {
                    IpcError::new(codes::SERVER, "could not resolve the senator alias in time")
                })?
                .map_err(server_error)?
                .remove(senator)
                .ok_or_else(|| {
                    IpcError::new(codes::NO_SESSION, format!("unknown alias {senator}"))
                })?
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

    pub(super) async fn flush_host_switch_notices(&self) {
        let notices = match self.store.hosted_switch_notices() {
            Ok(notices) => notices,
            Err(error) => {
                tracing::warn!(%error, "reading hosted identity switch notices");
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
                        tracing::warn!(%error, "marking hosted identity switch as reported");
                    }
                    continue;
                }
                Ok(Err(error)) if definitive_notice_rejection(&error) => error,
                Ok(Err(error)) => {
                    tracing::warn!(%id, %error, "posting hosted identity switch, will retry");
                    continue;
                }
                Err(_) => {
                    tracing::warn!(%id, "posting hosted identity switch timed out, will retry");
                    continue;
                }
            };
            match self
                .store
                .fail_hosted_switch_notice(&id, HOST_NOTICE_FAILURE_LIMIT)
            {
                Ok(true) => {
                    tracing::warn!(%id, %rejection, "dropping hosted identity switch notice after repeated rejections")
                }
                Ok(false) => {
                    tracing::warn!(%id, %rejection, "posting hosted identity switch, will retry")
                }
                Err(error) => {
                    tracing::warn!(%id, %error, %rejection, "recording hosted identity switch notice rejection")
                }
            }
        }
    }

    pub(super) async fn resolve_restore(
        &self,
        senator: &SenatorName,
    ) -> Result<RestoreTarget, IpcError> {
        let session = self.find_local_senator(senator).await?;
        let runtime = match session.runtime {
            Runtime::Codex => HostedRuntime::Codex,
            Runtime::Claude => HostedRuntime::Claude,
            _ => {
                return Err(IpcError::bad_request(
                    "only Codex and Claude Code can be restored",
                ));
            }
        };
        let committee = session.committee().cloned().ok_or_else(|| {
            IpcError::bad_request(format!("no committee is recorded for {senator}"))
        })?;
        Ok(RestoreTarget {
            runtime,
            committee,
            session,
        })
    }

    pub(super) async fn on_restore_with<Fut>(
        &self,
        target: RestoreTarget,
        limit: Option<usize>,
        audit: impl Fn(LaunchAudit, SpawnReply, HostedRuntime) -> Fut + Send,
    ) -> Result<HostedLaunch, IpcError>
    where
        Fut: std::future::Future<Output = Result<(), IpcError>> + Send,
    {
        let session = target.session;
        self.refuse_running_host(&Paths::new(&self.home), &session.session_id)?;
        if session.runtime == Runtime::Claude && claude_reachable(&session).await {
            return Err(IpcError::bad_request(
                "the original session is still running; exit it before restoring",
            ));
        }
        self.launch_host(
            target.runtime,
            target.committee,
            None,
            session.cwd,
            Some((session.session_id, session.agent)),
            limit,
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

    pub(super) async fn on_attach(&self, senator: SenatorName) -> Result<PathBuf, IpcError> {
        let session = self.find_local_senator(&senator).await?;
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
            format!("no running terminal host for {senator}; use senate restore to resume it"),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    async fn launch_host<Fut>(
        &self,
        runtime: HostedRuntime,
        committee: CommitteeId,
        alias: Option<SenatorName>,
        cwd: PathBuf,
        resume: Option<(String, SenatorName)>,
        limit: Option<usize>,
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
        let (resume, seat) = match resume {
            Some((id, agent)) => (Some(id), Some(agent)),
            None => (None, None),
        };
        let id = EventId::new();
        let (published, _) = watch::channel(false);
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

        // Keep the creating connection until the host has started. Delivery
        // and the TUI use the same shared server for the life of the session.
        let mut app_server = None;
        let restoring = resume.is_some();
        let session_id = match runtime {
            HostedRuntime::Claude => resume.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            HostedRuntime::Codex => {
                let connect = || crate::delivery::codex::AppServer::connect(&self.codex_transport);
                let mut server = tokio::time::timeout(CODEX_PUSH_TIMEOUT, connect())
                    .await
                    .map_err(|_| {
                        IpcError::new(
                            codes::SERVER,
                            "timed out connecting to the Codex app-server",
                        )
                    })?
                    .map_err(IpcError::internal)?;
                let id = if let Some(id) = resume {
                    id
                } else {
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
                    id
                };
                app_server = Some(server);
                id
            }
        };
        if let Some(pending) = lock(&self.hosts.0).get_mut(&id) {
            pending.session_id = Some(session_id.clone());
        }
        let remote = (runtime == HostedRuntime::Codex).then(|| self.codex_transport.remote());
        let launch = host::SessionCommand {
            session_id: &session_id,
            program: &program,
            resume: restoring,
            codex_remote: remote.as_deref(),
        };
        let session_runtime = Runtime::from(runtime);
        let reply = SpawnReply {
            senator: seat.unwrap_or_else(|| agent::session_name(session_runtime, &session_id)),
            session_id: session_id.clone(),
            committee,
        };
        let facts = session::gather_git_facts(&cwd, Duration::from_secs(2)).await;
        let inferred = session::infer_committee(
            facts.branch.as_deref(),
            facts.committee_file.as_deref(),
            self.convention.as_ref(),
        );
        let mut process = None;
        let mut publication = None;
        let mut audit_written = false;
        let mut alias_may_have_landed = false;
        let mut staged = None;
        let result: Result<(), IpcError> = async {
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
            let (child, errors) =
                host::start(&self.host_binary, &paths, &id, runtime, &cwd, &launch)
                    .map_err(IpcError::internal)?;
            let (ready, ack) = await_ready(child, errors).await?;
            process = Some(ready);
            publication = Some(ack);
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
                    self.api.set_alias(&reply.senator, alias, false),
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
                    self.api.remove_alias(&reply.senator, alias),
                )
                .await;
            }
            if !restoring && let Some(server) = &mut app_server {
                record_rollback(
                    &mut rollback,
                    "archive Codex thread",
                    server.archive_thread(&session_id),
                )
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
            if restoring {
                self.relay.observe_pending_on_restore(&session_id);
            }
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

fn definitive_notice_rejection(error: &ApiClientError) -> bool {
    // A generic HTTP response may come from an intermediary. Only a
    // Senate rejection consumes the notice's drop budget.
    matches!(error, ApiClientError::Api { status, code, .. }
        if code != "http" && (400..500).contains(status)
            && !matches!(status, 408 | 421 | 425 | 429))
}

fn owns_claude_hook(tui: u32, reported: Option<u32>, ancestors: &[u32]) -> bool {
    // Claude sets CLAUDE_PID to the runtime that called the hook, even
    // for a nested headless Claude. Mere descent under the TUI is not
    // evidence that the TUI itself changed its native identity.
    reported == Some(tui) && ancestors.contains(&tui)
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
    use super::owns_claude_hook;

    #[test]
    fn clear_from_the_tui_matches_but_a_nested_claude_does_not() {
        assert!(owns_claude_hook(10, Some(10), &[11, 10]));
        assert!(!owns_claude_hook(10, Some(12), &[12, 11, 10]));
        assert!(!owns_claude_hook(10, Some(10), &[12, 11]));
        assert!(!owns_claude_hook(10, None, &[10]));
    }
}
