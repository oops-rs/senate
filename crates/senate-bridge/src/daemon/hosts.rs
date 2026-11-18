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
pub(super) struct Launches(Mutex<BTreeMap<EventId, Option<String>>>);

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
        self.launch_host(runtime, committee, alias, cwd, None, limit)
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
        self.store
            .all_sessions()
            .map_err(internal)?
            .into_iter()
            .find(|s| s.agent == primary && s.agent_source == AgentSource::Session)
            .ok_or_else(|| {
                IpcError::new(
                    codes::NO_SESSION,
                    format!("no local session record for {senator}; its directory is unknown"),
                )
            })
    }

    pub(super) async fn resolve_restore(
        &self,
        senator: &SenatorName,
    ) -> Result<RestoreTarget, IpcError> {
        let session = self.find_local_senator(senator).await?;
        self.refuse_running_host(&Paths::new(&self.home), &session.session_id)?;
        if session.is_live() && session.claude_pid.is_some_and(session::pid_alive) {
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
        self.launch_host(
            target.runtime,
            target.committee,
            None,
            target.cwd,
            Some(target.session_id),
            limit,
        )
        .await
    }

    fn refuse_running_host(&self, paths: &Paths, session_id: &str) -> Result<(), IpcError> {
        if let Some(id) = self.store.session_host(session_id).map_err(internal)?
            && host::running(paths, &id).map_err(IpcError::internal)?
        {
            return Err(IpcError::bad_request(
                "this senator already has a running terminal host; use senate attach",
            ));
        }
        Ok(())
    }

    pub(super) async fn on_attach(&self, senator: SenatorName) -> Result<PathBuf, IpcError> {
        let session = self.find_local_senator(&senator).await?;
        let paths = Paths::new(&self.home);
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

    async fn launch_host(
        &self,
        runtime: HostedRuntime,
        committee: CommitteeId,
        alias: Option<SenatorName>,
        cwd: PathBuf,
        resume: Option<String>,
        limit: Option<usize>,
    ) -> Result<HostedLaunch, IpcError> {
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
        {
            let mut launches = lock(&self.hosts.0);
            if let Some(limit) = limit {
                Self::check_host_capacity(&paths, &launches, limit)?;
            }
            if let Some(id) = &resume {
                if launches
                    .values()
                    .any(|pending| pending.as_ref() == Some(id))
                {
                    return Err(IpcError::bad_request(
                        "this senator is already being restored",
                    ));
                }
                self.refuse_running_host(&paths, id)?;
            }
            launches.insert(id.clone(), resume.clone());
        }
        let _pending = PendingGuard {
            launches: &self.hosts,
            id: id.clone(),
        };

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
                .map_err(|_| IpcError::bad_request("timed out starting the Codex thread"))?
                .map_err(IpcError::internal)?;
                app_server = Some(server);
                id
            }
        };
        let remote = (runtime == HostedRuntime::Codex).then(|| self.codex_transport.remote());
        let launch = host::SessionCommand {
            session_id: &session_id,
            resume: restoring,
            codex_remote: remote.as_deref(),
        };
        let session_runtime = Runtime::from(runtime);
        let senator = agent::session_name(session_runtime, &session_id);
        let facts = session::gather_git_facts(&cwd, Duration::from_secs(2)).await;
        let inferred = session::infer_committee(
            facts.branch.as_deref(),
            facts.committee_file.as_deref(),
            self.convention.as_ref(),
        );
        if let Some(alias) = &alias {
            tokio::time::timeout(
                Duration::from_secs(5),
                self.api.set_alias(&senator, alias, false),
            )
            .await
            .map_err(|_| self.join_name_timeout())?
            .map_err(server_error)?;
        }
        // A caller can send an assignment as soon as spawn returns. Seed
        // now so the stream's first join cannot skip that assignment.
        let generation = *self.generation.read().await;
        tokio::time::timeout(
            Duration::from_secs(5),
            self.ensure_seeded(&senator, &committee, generation),
        )
        .await
        .map_err(|_| IpcError::bad_request("timed out preparing the senator's inbox"))?
        .map_err(|error| match error {
            Intake::Resync(reason) => IpcError::bad_request(reason.to_string()),
            Intake::Failed(error) => IpcError::internal(error),
        })?;
        let child = {
            let _binding = self.session_bindings.write().await;
            self.store
                .register_hosted_session(
                    &SessionUpsert {
                        session_id: session_id.clone(),
                        cwd: cwd.clone(),
                        git_root: facts.toplevel,
                        branch: facts.branch,
                        inferred_committee: inferred,
                        socket: None,
                        token: None,
                        claude_pid: None,
                        transcript_path: None,
                        ancestor_pids: vec![],
                        runtime: session_runtime,
                        agent: senator.clone(),
                        agent_source: AgentSource::Session,
                    },
                    &id,
                    &committee,
                )
                .map_err(internal)?;
            host::start(&paths, &id, runtime, &cwd, &launch)
        };
        let result = match child {
            Ok((child, errors)) => await_ready(child, errors).await,
            Err(error) => Err(IpcError::internal(error)),
        };
        drop(app_server);
        match result {
            Ok(process) => {
                self.committees_changed.notify_one();
                self.work.notify_one();
                if runtime == HostedRuntime::Codex {
                    self.reconcile_approvals();
                }
                Ok(HostedLaunch {
                    reply: SpawnReply {
                        senator,
                        session_id,
                        committee,
                    },
                    process,
                })
            }
            Err(error) => {
                let _binding = self.session_bindings.write().await;
                self.store
                    .set_session_state(&session_id, SessionState::Ended)
                    .map_err(internal)?;
                self.committees_changed.notify_one();
                Err(error)
            }
        }
    }

    /// Count pending launches and running hosts once, under the same lock
    /// used to reserve a launch. A host can hold its owner lock while the
    /// reservation is still pending, including across daemon restarts.
    fn check_host_capacity(
        paths: &Paths,
        launches: &BTreeMap<EventId, Option<String>>,
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

async fn await_ready(mut child: Child, errors: std::fs::File) -> Result<HostProcess, IpcError> {
    let Some(mut ready) = child.stdout.take() else {
        let _ = stop_child(&mut child).await;
        return Err(IpcError::internal(
            "the terminal host has no readiness pipe",
        ));
    };
    let result = tokio::select! {
        status = child.wait() => status.map_err(IpcError::internal).and_then(|status| {
            Err(IpcError::bad_request(format!("the terminal host exited during startup ({status})")))
        }),
        marker = tokio::time::timeout(host::START_TIMEOUT, ready.read_u8()) => match marker {
            Ok(Ok(host::READY)) => Ok(()),
            Ok(_) => Err(IpcError::bad_request("the terminal host closed its startup pipe")),
            Err(_) => Err(IpcError::bad_request("timed out starting the terminal host")),
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
