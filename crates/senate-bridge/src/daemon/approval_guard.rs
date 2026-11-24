//! Who may answer a permission prompt through the daemon.
//!
//! An agent must not approve its own prompts, nor another agent's: a
//! permission prompt is a person's decision. So [`Request::ApproveSession`]
//! and [`Request::Approve`] are refused when the process that sent them
//! runs inside a coding session's process tree. The daemon takes the
//! caller's pid from the socket itself (`LOCAL_PEEREPID` on macOS,
//! `SO_PEERCRED` on Linux, through tokio's `peer_cred`), not from anything
//! the caller says, and walks its ancestors up to pid 1. It refuses when
//! any of them is:
//!
//! - the runtime process of a session on record that has not ended (the
//!   Claude pid its hooks reported);
//! - a running terminal host, or the TUI it runs (every Senate-hosted
//!   session, whatever its runtime);
//! - this daemon (a companion session it started, or a hosted one);
//! - a coding runtime by its executable's name (`claude`, `codex`, `agy`),
//!   for the agents Senate has no pid for, such as a Codex session started
//!   outside Senate.
//!
//! It also refuses when the chain cannot be read to its end, or the socket
//! names no pid. Environment markers (`CLAUDECODE`, a hosted session's
//! variables) are not consulted: a caller sets its own environment.
//!
//! This is defence in depth, not a boundary. A process that detached from
//! its agent (a daemonized child, re-parented to launchd or init) escapes
//! the ancestry check, and so does a runtime renamed past the names above
//! that Senate has no pid for. The person's own terminal, and the desktop
//! app, are outside every agent's tree, so they pass. Every decision, and
//! every refusal with its reason, goes to the daemon's log.

use std::collections::BTreeMap;

use super::*;
use crate::host;

/// The most ancestors walked; a deeper chain is refused.
const DEPTH: usize = 64;
const PS_TIMEOUT: Duration = Duration::from_secs(2);
/// Executable names of the coding runtimes.
const RUNTIMES: [&str; 3] = ["claude", "codex", "agy"];

/// The requests that answer a permission prompt.
pub(super) fn answers_a_prompt(request: &Request) -> bool {
    matches!(
        request,
        Request::Approve { .. } | Request::ApproveSession { .. }
    )
}

/// What only the person does, and what to tell an agent that asks: `None`
/// for any other request.
pub(super) fn person_only(request: &Request) -> Option<&'static str> {
    Some(match request {
        Request::Timeline { .. }
        | Request::SessionHistory { .. }
        | Request::Stats { .. }
        | Request::SessionSearch { .. }
        | Request::SessionForget { .. } => {
            "Read session transcripts from your own terminal or the Senate app"
        }
        // As the person (no session). With a session it is the Speaker's
        // office's, checked by the front and the server.
        Request::SetChair { session, .. }
            if session.session_id.is_none() && session.claude_pid.is_none() =>
        {
            "Naming a chair as the person is the person's to do: run `senate senator chair` from your own terminal"
        }
        // An agent names its own session with `join`, and the desk acts for
        // the office with its own tools.
        Request::SetAlias { .. }
        | Request::RemoveAlias { .. }
        | Request::SetCommitteeSeat { .. } => {
            "Aliases and seats set as the person are the person's to change: run the command from your own terminal (a session names itself with `join`; the Speaker's desk has speaker_alias and speaker_unseat)"
        }
        Request::ArchiveCommittee { .. } => {
            "Archiving a committee is the Speaker's own act: run `senate committees archive` from your own terminal"
        }
        Request::EnsureSecretary => {
            "Only the person starts the default secretary: ask it from your own terminal or the Senate app"
        }
        request if answers_a_prompt(request) => {
            "A permission prompt is a person's decision: answer it from your own terminal or the Senate app"
        }
        _ => return None,
    })
}

impl Front {
    /// Serve `request` unless its caller is inside a coding session's
    /// process tree ([`person_only`]).
    pub(super) async fn guarded_dispatch(
        self: &Arc<Self>,
        peer: Option<u32>,
        guidance: &str,
        request: Request,
    ) -> Reply {
        match self.refuse_agent_caller(peer, guidance).await {
            Ok(()) => self.dispatch(request).await,
            Err(refused) => Reply::err(refused),
        }
    }

    /// Refuse a caller inside a coding session's process tree.
    pub(super) async fn refuse_agent_caller(
        &self,
        peer: Option<u32>,
        guidance: &str,
    ) -> Result<(), IpcError> {
        let Some(peer) = peer else {
            return Err(refusal(
                "the daemon could not tell which process sent this request",
            ));
        };
        let me = std::process::id();
        // The daemon's own in-process clients (tests drive it so).
        if peer == me {
            return Ok(());
        }
        let _permit = self
            .process_checks
            .acquire()
            .await
            .expect("process check budget is never closed");
        let mut agents = BTreeMap::new();
        agents.insert(me, "the Senate daemon".to_owned());
        for shared in self.senates() {
            shared.agent_processes(&mut agents).map_err(|e| match e {
                StoreError::Io(ref source) if crate::fd::exhausted(source) => fd_refusal(peer),
                _ => internal(e),
            })?;
        }
        let chain = match ancestry(peer, process).await {
            Ok(chain) => chain,
            Err(ProcessLookup::Descriptors) => return Err(fd_refusal(peer)),
            Err(ProcessLookup::Unreadable) => {
                tracing::warn!(
                    peer,
                    guidance,
                    "refused person-only IPC: the caller's ancestry is unreadable"
                );
                return Err(refusal(
                    "the daemon could not read the calling process's ancestry",
                ));
            }
        };
        let owner = chain.iter().find_map(|(pid, name)| {
            agents.get(pid).cloned().or_else(|| {
                RUNTIMES
                    .contains(&name.as_str())
                    .then(|| format!("a `{name}` process (pid {pid})"))
            })
        });
        match owner {
            Some(owner) => {
                tracing::warn!(peer, %owner, guidance, "refused person-only IPC from inside a coding session's process tree");
                Err(refusal(&format!(
                    "this request comes from inside the process tree of {owner}. {guidance}"
                )))
            }
            None => {
                tracing::debug!(
                    peer,
                    guidance,
                    "person-only IPC passed the process-tree check"
                );
                Ok(())
            }
        }
    }
}

impl Shared {
    /// The processes of this Senate's coding sessions, each with whose it is.
    fn agent_processes(&self, out: &mut BTreeMap<u32, String>) -> Result<(), StoreError> {
        for session in self.store.sessions()? {
            if session.state == SessionState::Ended {
                continue;
            }
            if let Some(pid) = session.claude_pid {
                out.insert(pid, Address::senator(&session.agent).in_text());
            }
        }
        let paths = Paths::new(&self.home);
        for id in host::running_ids(&paths)? {
            let owner = match self.store.session_for_host(&id)? {
                Some(native) => self
                    .store
                    .session(&native)?
                    .map(|s| Address::senator(&s.agent).in_text()),
                None => None,
            }
            .unwrap_or_else(|| format!("terminal host {id}"));
            for pid in [host::host_pid(&paths, &id), host::child_pid(&paths, &id)]
                .into_iter()
                .flatten()
            {
                out.insert(pid, owner.clone());
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ProcessLookup {
    Unreadable,
    Descriptors,
}

impl From<std::io::Error> for ProcessLookup {
    fn from(error: std::io::Error) -> Self {
        if crate::fd::exhausted(&error) {
            Self::Descriptors
        } else {
            Self::Unreadable
        }
    }
}

fn fd_refusal(peer: u32) -> IpcError {
    tracing::warn!(
        peer,
        "refused person-only IPC: the daemon is out of file descriptors"
    );
    // Keep failing closed, but report a daemon error rather than implying
    // that the caller is an agent.
    IpcError::internal(
        "the daemon is out of file descriptors and could not verify the caller; try again",
    )
}

/// A process's parent and the name of its executable (the last part of
/// its first argument), by `ps`, portable across macOS and Linux.
async fn process(pid: u32) -> Result<(u32, String), ProcessLookup> {
    let child = tokio::process::Command::new("ps")
        .args(["-o", "ppid=", "-o", "args=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(PS_TIMEOUT, child)
        .await
        .map_err(|_| ProcessLookup::Unreadable)??;
    if !output.status.success() {
        return Err(ProcessLookup::Unreadable);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut words = text.split_whitespace();
    let parent = words
        .next()
        .and_then(|p| p.parse().ok())
        .ok_or(ProcessLookup::Unreadable)?;
    let name = words
        .next()
        .and_then(|program| program.rsplit('/').next())
        .unwrap_or_default()
        .to_owned();
    Ok((parent, name))
}

/// `start` and every process above it up to pid 1, each with its name, or
/// an error when a process cannot be read before then.
async fn ancestry<F, Fut>(start: u32, read: F) -> Result<Vec<(u32, String)>, ProcessLookup>
where
    F: Fn(u32) -> Fut,
    Fut: std::future::Future<Output = Result<(u32, String), ProcessLookup>>,
{
    let mut chain = Vec::new();
    let mut pid = start;
    loop {
        if chain.len() >= DEPTH || chain.iter().any(|(seen, _)| *seen == pid) {
            return Err(ProcessLookup::Unreadable);
        }
        if pid <= 1 {
            chain.push((pid, String::new()));
            return Ok(chain);
        }
        let (parent, name) = read(pid).await?;
        chain.push((pid, name));
        pid = parent;
    }
}

fn refusal(message: &str) -> IpcError {
    IpcError::new(codes::NOT_A_PERSON, message)
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_persons_own_acts_are_guarded_and_a_sessions_are_not() {
        use senate_core::CommitteeSeatAction;
        let person = SessionSelector::default();
        let session = SessionSelector {
            session_id: Some("s".into()),
            claude_pid: None,
        };
        let chair = |session| Request::SetChair {
            session,
            committee: None,
            senator: "codex-1".parse().unwrap(),
        };
        assert!(person_only(&chair(person)).is_some_and(|g| g.contains("senate senator chair")));
        assert_eq!(
            person_only(&chair(session.clone())),
            None,
            "the desk's speaker_chair"
        );
        for request in [
            Request::SetAlias {
                senator: senate_core::alias::AliasName::bare("codex-1".parse().unwrap()),
                alias: "main".parse().unwrap(),
                committee: None,
            },
            Request::RemoveAlias {
                primary: "codex-1".parse().unwrap(),
                alias: "main".parse().unwrap(),
                committee: None,
            },
            Request::SetCommitteeSeat {
                committee: senate_core::CommitteeId::new("ibot").unwrap(),
                request: senate_core::api::SetCommitteeSeat {
                    senator: "codex-1".parse().unwrap(),
                    action: CommitteeSeatAction::Unseat,
                    id: Default::default(),
                    acted_by: None,
                },
            },
        ] {
            assert!(
                person_only(&request).is_some_and(|g| g.contains("speaker_alias")),
                "{request:?}"
            );
        }
        // A session naming itself is the session's own act.
        assert_eq!(
            person_only(&Request::JoinAs {
                session,
                committee: senate_core::CommitteeId::new("ibot").unwrap(),
                senator: "main".parse().unwrap(),
            }),
            None
        );
        assert!(person_only(&Request::EnsureSecretary).is_some());
        assert_eq!(person_only(&Request::Ping), None);
    }

    use super::*;

    #[tokio::test]
    async fn the_chain_is_read_to_pid_1_or_not_at_all() {
        let table: BTreeMap<u32, (u32, &str)> =
            [(40, (30, "senate")), (30, (20, "zsh")), (20, (1, "claude"))].into();
        let read = |pid: u32| {
            let found = table.get(&pid).map(|(p, n)| (*p, (*n).to_owned()));
            async move { found.ok_or(ProcessLookup::Unreadable) }
        };
        let pids =
            |chain: Vec<(u32, String)>| chain.into_iter().map(|(p, _)| p).collect::<Vec<_>>();
        assert_eq!(pids(ancestry(40, read).await.unwrap()), vec![40, 30, 20, 1]);
        assert_eq!(ancestry(40, read).await.unwrap()[2].1, "claude");
        // A process that cannot be read fails closed.
        assert_eq!(ancestry(41, read).await, Err(ProcessLookup::Unreadable));
        // A loop is not a chain.
        let cycle: BTreeMap<u32, (u32, &str)> = [(40, (30, "a")), (30, (40, "b"))].into();
        let read = |pid: u32| {
            let found = cycle.get(&pid).map(|(p, n)| (*p, (*n).to_owned()));
            async move { found.ok_or(ProcessLookup::Unreadable) }
        };
        assert_eq!(ancestry(40, read).await, Err(ProcessLookup::Unreadable));
    }

    #[tokio::test]
    async fn descriptor_exhaustion_is_reported_as_a_daemon_error() {
        for errno in [libc::EMFILE, libc::ENFILE] {
            let read = |_| async move { Err(std::io::Error::from_raw_os_error(errno).into()) };
            assert_eq!(ancestry(40, read).await, Err(ProcessLookup::Descriptors));
        }
        assert!(
            fd_refusal(40)
                .to_string()
                .contains("out of file descriptors")
        );
    }

    #[tokio::test]
    async fn this_process_has_a_readable_chain() {
        let me = std::process::id();
        let chain = ancestry(me, process).await.unwrap();
        assert_eq!(chain.first().map(|(pid, _)| *pid), Some(me));
        assert_eq!(chain.last().map(|(pid, _)| *pid), Some(1));
    }
}
