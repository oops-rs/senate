//! Correlate a detached launch with the native session that reports its
//! one-use token. The long-lived binding is in the session row, so a daemon
//! restart needs no memory of which request created the host.

use std::io::Read as _;

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
    ) -> Result<SpawnReply, IpcError> {
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
        self.launch_host(runtime, committee, alias, cwd, None).await
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

    pub(super) async fn on_restore(&self, target: RestoreTarget) -> Result<SpawnReply, IpcError> {
        self.launch_host(
            target.runtime,
            target.committee,
            None,
            target.cwd,
            Some(target.session_id),
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
    ) -> Result<SpawnReply, IpcError> {
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

        if result.is_err()
            && let Some(pid) = child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::TERM);
        }
        // The host is intentionally detached. Reap it if it exits before
        // this daemon does, without tying its lifetime to this task.
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        result
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
