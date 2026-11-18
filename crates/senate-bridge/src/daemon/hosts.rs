//! Launch hosted TUIs with a native identity and a committed Senate binding.
//! The terminal owner outlives the daemon that launched it.

use std::io::Read as _;

use tokio::io::AsyncReadExt as _;
use tokio::process::Child;
use tokio::sync::oneshot;

use super::*;
use crate::host;
use crate::ipc::{HostedRuntime, SpawnReply};

#[derive(Default)]
pub(super) struct Launches(Mutex<BTreeMap<EventId, Pending>>);

struct Pending {
    session_id: Option<String>,
    published: watch::Sender<bool>,
}

impl Launches {
    pub(super) fn pending(&self, session_id: &str) -> Option<watch::Receiver<bool>> {
        lock(&self.0)
            .values()
            .find(|pending| pending.session_id.as_deref() == Some(session_id))
            .map(|pending| pending.published.subscribe())
    }
}

pub(super) enum LaunchAudit {
    Ready,
    RolledBack { reason: String },
}

async fn no_audit(phase: LaunchAudit, _: SpawnReply, _: HostedRuntime) -> Result<(), IpcError> {
    if let LaunchAudit::RolledBack { reason } = phase {
        tracing::debug!(%reason, "rolling back an operator terminal launch");
    }
    Ok(())
}

pub(super) fn switch_message(old: &SenatorName, new: &str) -> String {
    format!(
        "hosted Claude session {old} switched to native ID {new}; attach to {old} to inspect or exit the existing terminal"
    )
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
    runtime: HostedRuntime,
    cwd: PathBuf,
    session_id: String,
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
    pub(super) async fn on_spawn(
        &self,
        runtime: HostedRuntime,
        committee: CommitteeId,
        alias: Option<SenatorName>,
        cwd: PathBuf,
        limit: Option<usize>,
    ) -> Result<HostedLaunch, IpcError> {
        self.on_spawn_with(runtime, committee, alias, cwd, limit, no_audit)
            .await
    }

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
            .map_err(|_| IpcError::bad_request("could not confirm the committee in time"))?
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
                .map_err(|_| IpcError::bad_request("could not resolve the senator alias in time"))?
                .map_err(server_error)?
                .remove(senator)
                .ok_or_else(|| {
                    IpcError::new(codes::NO_SESSION, format!("unknown alias {senator}"))
                })?
        };
        for session in self.store.all_sessions().map_err(internal)? {
            if session.agent == primary && session.agent_source == AgentSource::Session {
                return Ok(session);
            }
            let paths = Paths::new(&self.home);
            if let Some(id) = self
                .store
                .session_host(&session.session_id)
                .map_err(internal)?
                && let Some(new) = host::switched(&paths, &id)
                && agent::session_name(Runtime::Claude, &new) == primary
                && host::running(&paths, &id).map_err(IpcError::internal)?
            {
                return Err(IpcError::bad_request(switch_message(&session.agent, &new)));
            }
        }
        Err(IpcError::new(
            codes::NO_SESSION,
            format!("no local session record for {senator}; its directory is unknown"),
        ))
    }

    pub(super) fn detect_host_switch(
        &self,
        report: &HookReport,
    ) -> Result<Option<SenatorName>, IpcError> {
        let paths = Paths::new(&self.home);
        for session in self.store.live_sessions().map_err(internal)? {
            if session.runtime != Runtime::Claude || session.session_id == report.session_id {
                continue;
            }
            if let Some(id) = self
                .store
                .session_host(&session.session_id)
                .map_err(internal)?
                && let Some(pid) = host::child_pid(&paths, &id)
                && (Some(pid) == report.claude_pid || report.ancestor_pids.contains(&pid))
                && host::running(&paths, &id).map_err(IpcError::internal)?
            {
                host::note_switch(&paths, &id, &report.session_id).map_err(IpcError::internal)?;
                self.committees_changed.notify_one();
                return Ok(Some(session.agent));
            }
        }
        Ok(None)
    }

    pub(super) async fn resolve_restore(
        &self,
        senator: &SenatorName,
    ) -> Result<RestoreTarget, IpcError> {
        let session = self.find_local_senator(senator).await?;
        self.refuse_running_host(&Paths::new(&self.home), &session.session_id)?;
        if session.runtime == Runtime::Claude && claude_reachable(&session).await {
            return Err(IpcError::bad_request(
                "the original session is still running; exit it before restoring",
            ));
        }
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
            cwd: session.cwd,
            session_id: session.session_id,
        })
    }

    pub(super) async fn on_restore(
        &self,
        target: RestoreTarget,
        limit: Option<usize>,
    ) -> Result<HostedLaunch, IpcError> {
        self.on_restore_with(target, limit, no_audit).await
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
        self.launch_host(
            target.runtime,
            target.committee,
            None,
            target.cwd,
            Some(target.session_id),
            limit,
            audit,
        )
        .await
    }

    fn refuse_running_host(&self, paths: &Paths, session_id: &str) -> Result<(), IpcError> {
        if let Some(id) = self.store.session_host(session_id).map_err(internal)?
            && host::running(paths, &id).map_err(IpcError::internal)?
        {
            if let Some(new) = host::switched(paths, &id) {
                let old = agent::session_name(Runtime::Claude, session_id);
                return Err(IpcError::bad_request(switch_message(&old, &new)));
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
        resume: Option<String>,
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
                let (server, id) = tokio::time::timeout(CODEX_PUSH_TIMEOUT, async {
                    let mut server =
                        crate::delivery::codex::AppServer::connect(&self.codex_transport).await?;
                    let id = match resume {
                        Some(id) => id,
                        None => server.start_hosted_thread(&cwd).await?,
                    };
                    Ok::<_, crate::delivery::codex::CodexError>((server, id))
                })
                .await
                .map_err(|_| IpcError::new(codes::SERVER, "timed out preparing the Codex thread"))?
                .map_err(IpcError::internal)?;
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
            senator: agent::session_name(session_runtime, &session_id),
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
        let mut audit_written = false;
        let mut alias_attempted = false;
        let result = async {
            // No agent cursor GET here: that endpoint seats the senator.
            // Snapshot the head without acting as the new senator.
            let cursor = if self
                .store
                .has_cursor(&reply.senator, &reply.committee)
                .map_err(internal)?
            {
                None
            } else {
                Some(
                    tokio::time::timeout(Duration::from_secs(5), self.head(&reply.committee))
                        .await
                        .map_err(|_| {
                            IpcError::new(codes::SERVER, "timed out preparing the senator's inbox")
                        })?
                        .map_err(|error| match error {
                            Intake::Resync(reason) => {
                                IpcError::new(codes::SERVER, reason.to_string())
                            }
                            Intake::Failed(error) => IpcError::internal(error),
                        })?,
                )
            };
            let (child, errors) =
                host::start(&self.host_binary, &paths, &id, runtime, &cwd, &launch)
                    .map_err(IpcError::internal)?;
            process = Some(await_ready(child, errors).await?);
            audit(LaunchAudit::Ready, reply.clone(), runtime).await?;
            audit_written = true;
            if let Some(alias) = &alias {
                alias_attempted = true;
                tokio::time::timeout(
                    Duration::from_secs(5),
                    self.api.set_alias(&reply.senator, alias, false),
                )
                .await
                .map_err(|_| self.join_name_timeout())?
                .map_err(server_error)?;
            }
            if !host::running(&paths, &id).map_err(IpcError::internal)?
                || !host::child_pid(&paths, &id).is_some_and(session::pid_alive)
            {
                return Err(IpcError::internal(
                    "terminal host exited before publication",
                ));
            }
            // This launch's hook is waiting on its own publication
            // channel; unrelated hooks and delivery can keep running.
            let _binding = self.session_bindings.write().await;
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
                    cursor,
                )
                .map_err(internal)?;
            published.send_replace(true);
            Ok(())
        }
        .await;
        if let Err(error) = result {
            let mut rollback = Vec::new();
            if let Some(process) = process
                && let Err(stop) = process.stop().await
            {
                rollback.push(format!("could not stop terminal host: {stop}"));
            }
            if alias_attempted && let Some(alias) = &alias {
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

async fn await_ready(mut child: Child, errors: std::fs::File) -> Result<HostProcess, IpcError> {
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
            Err(IpcError::bad_request(format!(
                "{error}: {}",
                details.trim()
            )))
        };
    }
    Ok(process)
}
