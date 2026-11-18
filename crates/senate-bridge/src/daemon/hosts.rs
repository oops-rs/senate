//! Correlate a detached launch with the native session that reports its
//! one-use token. The long-lived binding is in the session row, so a daemon
//! restart needs no memory of which request created the host.

use std::io::Read as _;

use tokio::process::Child;
use tokio::sync::oneshot;

use super::*;
use crate::host;
use crate::ipc::{HostedRuntime, SpawnReply};

#[derive(Default)]
pub(super) struct Launches(Mutex<HashMap<String, Pending>>);

struct Pending {
    id: EventId,
    runtime: HostedRuntime,
    cwd: PathBuf,
    committee: CommitteeId,
    alias: Option<SenatorName>,
    resume: Option<String>,
    reply: Option<tokio::sync::oneshot::Sender<Result<SpawnReply, IpcError>>>,
}

struct PendingGuard<'a> {
    launches: &'a Launches,
    token: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        lock(&self.launches.0).remove(&self.token);
    }
}

pub(super) struct Registration {
    id: EventId,
    committee: CommitteeId,
    alias: Option<SenatorName>,
    reply: tokio::sync::oneshot::Sender<Result<SpawnReply, IpcError>>,
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

impl Launches {
    pub(super) fn claim(&self, report: &HookReport) -> Result<Option<Registration>, IpcError> {
        if report.event != HookEvent::SessionStart {
            return Ok(None);
        }
        let Some(token) = &report.launch_token else {
            return Ok(None);
        };
        let mut launches = lock(&self.0);
        let Some(pending) = launches.get_mut(token) else {
            return Ok(None); // An ordinary hook, or this launch already completed.
        };
        if pending.reply.is_none() {
            return Ok(None);
        }
        if report.runtime != Runtime::from(pending.runtime)
            || report.cwd.canonicalize().ok().as_ref() != Some(&pending.cwd)
            || pending
                .resume
                .as_ref()
                .is_some_and(|id| id != &report.session_id)
        {
            return Err(IpcError::bad_request(
                "SessionStart did not match the pending terminal launch",
            ));
        }

        Ok(pending.reply.take().map(|reply| Registration {
            id: pending.id.clone(),
            committee: pending.committee.clone(),
            alias: pending.alias.clone(),
            reply,
        }))
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
        let mut secret = [0u8; 32];
        getrandom::fill(&mut secret).map_err(IpcError::internal)?;
        let token = hex::encode(secret);
        let (reply, result) = tokio::sync::oneshot::channel();
        {
            let mut launches = lock(&self.hosts.0);
            if let Some(limit) = limit {
                Self::check_host_capacity(&paths, &launches, limit)?;
            }
            if let Some(id) = &resume {
                if launches
                    .values()
                    .any(|pending| pending.resume.as_ref() == Some(id))
                {
                    return Err(IpcError::bad_request(
                        "this senator is already being restored",
                    ));
                }
                // Close the gap between the initial lookup and reserving
                // this ID, if another restore completed in the meantime.
                self.refuse_running_host(&paths, id)?;
            }
            launches.insert(
                token.clone(),
                Pending {
                    id: id.clone(),
                    runtime,
                    cwd: cwd.clone(),
                    committee,
                    alias,
                    resume: resume.clone(),
                    reply: Some(reply),
                },
            );
        }
        let _pending = PendingGuard {
            launches: &self.hosts,
            token: token.clone(),
        };
        let (mut child, errors) =
            host::start(&paths, &id, runtime, &cwd, resume.as_deref(), &token)
                .map_err(IpcError::internal)?;

        let result = tokio::select! {
            status = child.wait() => {
                let mut details = String::new();
                let _ = errors.take(4096).read_to_string(&mut details);
                let status = status.map_err(IpcError::internal)?;
                return Err(IpcError::bad_request(format!(
                    "the terminal host exited before registration ({status}){}",
                    if details.trim().is_empty() { String::new() } else { format!(": {}", details.trim()) }
                )));
            }
            result = tokio::time::timeout(host::START_TIMEOUT, result) => {
                result
                    .map_err(|_| IpcError::bad_request(
                        "the terminal host did not report SessionStart in time; check the runtime's Senate hooks and project trust",
                    ))
                    .and_then(|result| result.map_err(IpcError::internal))
                    .and_then(|result| result)
            }
        };

        let process = HostProcess::monitor(child);
        match result {
            Ok(reply) => Ok(HostedLaunch { reply, process }),
            Err(error) => {
                if let Err(stop_error) = process.stop().await {
                    tracing::warn!(%stop_error, "stopping a failed terminal launch");
                }
                Err(error)
            }
        }
    }

    /// Count pending launches and running hosts once, under the same lock
    /// used to reserve a launch. A host acquires its lock before registering,
    /// so it may already be in both sets while its request waits for the hook.
    fn check_host_capacity(
        paths: &Paths,
        launches: &HashMap<String, Pending>,
        limit: usize,
    ) -> Result<(), IpcError> {
        let mut used: BTreeSet<EventId> = launches.values().map(|p| p.id.clone()).collect();
        used.extend(host::running_ids(paths).map_err(IpcError::internal)?);
        if used.len() >= limit {
            return Err(IpcError::bad_request(format!(
                "the machine is at its hosted session limit ({limit})"
            )));
        }
        Ok(())
    }

    pub(super) async fn register_hosted(
        &self,
        registration: Registration,
        upsert: &SessionUpsert,
    ) -> Result<Session, IpcError> {
        let session = self.bind_hosted(&registration, upsert).await;
        let _ = registration.reply.send(
            session
                .as_ref()
                .map(|s| SpawnReply {
                    senator: s.agent.clone(),
                    session_id: s.session_id.clone(),
                    committee: registration.committee.clone(),
                })
                .map_err(Clone::clone),
        );
        session
    }

    async fn bind_hosted(
        &self,
        registration: &Registration,
        upsert: &SessionUpsert,
    ) -> Result<Session, IpcError> {
        if registration.reply.is_closed() {
            return Err(IpcError::bad_request("the terminal launch expired"));
        }
        if let Some(alias) = &registration.alias {
            tokio::time::timeout(
                Duration::from_secs(5),
                self.api.set_alias(&upsert.agent, alias, false),
            )
            .await
            .map_err(|_| self.join_name_timeout())?
            .map_err(server_error)?;
        }
        if registration.reply.is_closed() {
            return Err(IpcError::bad_request("the terminal launch expired"));
        }
        let session = self
            .store
            .register_hosted_session(upsert, &registration.id, &registration.committee)
            .map_err(internal)?;
        host::registered(
            &Paths::new(&self.home),
            &registration.id,
            &session.session_id,
        )
        .map_err(IpcError::internal)?;
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_matching_start_hook_can_claim_a_launch_once() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().canonicalize().unwrap();
        let launches = Launches::default();
        let (reply, _receipt) = oneshot::channel();
        let id = EventId::new();
        lock(&launches.0).insert(
            "launch-secret".into(),
            Pending {
                id: id.clone(),
                runtime: HostedRuntime::Claude,
                cwd: cwd.clone(),
                committee: "review".parse().unwrap(),
                alias: Some("reviewer".parse().unwrap()),
                resume: Some("old-session".into()),
                reply: Some(reply),
            },
        );
        let report: HookReport = serde_json::from_value(serde_json::json!({
            "event": "session_start",
            "session_id": "old-session",
            "launch_token": "launch-secret",
            "runtime": "claude",
            "cwd": cwd,
            "source": "resume",
        }))
        .unwrap();

        let mut wrong = report.clone();
        wrong.launch_token = None;
        assert!(launches.claim(&wrong).unwrap().is_none());

        wrong = report.clone();
        wrong.event = HookEvent::UserPromptSubmit;
        assert!(launches.claim(&wrong).unwrap().is_none());

        wrong = report.clone();
        wrong.session_id = "different-session".into();
        assert!(launches.claim(&wrong).is_err());

        wrong = report.clone();
        wrong.runtime = Runtime::Codex;
        assert!(launches.claim(&wrong).is_err());

        wrong = report.clone();
        wrong.cwd = cwd.join("another-project");
        assert!(launches.claim(&wrong).is_err());

        let registered = launches.claim(&report).unwrap().unwrap();
        assert_eq!(registered.id, id);
        assert_eq!(registered.committee.as_str(), "review");
        assert_eq!(registered.alias.unwrap().as_str(), "reviewer");
        assert!(launches.claim(&report).unwrap().is_none());
    }
}
