//! Coding-agent sessions on this machine: the registry record, how a
//! session's committee is inferred, and whether its process is still alive.
//!
//! Every session speaks as one agent ([`Session::agent`], chosen by
//! [`crate::agent`]), and only that agent's messages reach it.
//!
//! A session's committee is, in order:
//!
//! 1. an explicit `join` (the agent or its user said so; it wins),
//! 2. the git branch, when it is `<prefix>/<committee>/<person>`
//!    ([`BranchNaming`], prefix `feature` unless `bridge.toml` says
//!    otherwise),
//! 3. the first non-comment line of `.senate/committee` at the git toplevel
//!    (or the session's cwd outside git).
//!
//! The branch is the person's, not an agent's: two agents of one person
//! working on one committee share `feature/<committee>/<person>`, and both
//! are bound to that committee by it. Which agent a session is comes from
//! its harness or its pin, never from the branch.
//!
//! A session with no committee is *unbound* and receives nothing.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use chrono::{DateTime, Utc};
use senate_core::{BranchNaming, CommitteeId, SenatorPath};
use serde::{Deserialize, Serialize};

use crate::agent::{AGENT_FILE, AgentSource};

/// Relative path of the per-repo committee file.
pub const COMMITTEE_FILE: &str = ".senate/committee";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// Registered and believed reachable.
    Live,
    /// Its process is gone or its inbox socket refused a connection. A hook
    /// call from the session makes it live again.
    Stale,
    /// The SessionEnd hook ran.
    Ended,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Stale => "stale",
            Self::Ended => "ended",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "live" => Some(Self::Live),
            "stale" => Some(Self::Stale),
            "ended" => Some(Self::Ended),
            _ => None,
        }
    }
}

/// Where an inferred committee came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CommitteeSource {
    Branch,
    File,
    Join,
}

impl CommitteeSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::File => "file",
            Self::Join => "join",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "branch" => Some(Self::Branch),
            "file" => Some(Self::File),
            "join" => Some(Self::Join),
            _ => None,
        }
    }
}

/// Which coding agent runs a session, and so how pushes reach it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    /// Claude Code: pushes go through the session's inbox socket.
    #[default]
    Claude,
    /// Codex: pushes go through the Codex app-server the session's TUI is
    /// attached to. The session id is the Codex thread id.
    Codex,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }
}

/// One registered session. The inbox token never leaves the daemon: it is
/// skipped when a session is serialized onto the IPC wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Claude Code's `session_id`, or the Codex thread id.
    pub session_id: String,
    #[serde(default)]
    pub runtime: Runtime,
    /// The agent this session speaks as and receives for.
    pub agent: SenatorPath,
    /// How [`Session::agent`] was chosen.
    pub agent_source: AgentSource,
    pub cwd: PathBuf,
    #[serde(default)]
    pub git_root: Option<PathBuf>,
    #[serde(default)]
    pub branch: Option<String>,
    /// From the branch or the committee file.
    #[serde(default)]
    pub inferred_committee: Option<CommitteeId>,
    #[serde(default)]
    pub committee_source: Option<CommitteeSource>,
    /// From an explicit `join`; overrides the inferred committee.
    #[serde(default)]
    pub joined_committee: Option<CommitteeId>,
    /// `CLAUDE_CODE_MESSAGING_SOCKET` as the session's hooks saw it.
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// `CLAUDE_CODE_MESSAGING_TOKEN`.
    #[serde(skip)]
    pub token: Option<String>,
    #[serde(default)]
    pub claude_pid: Option<u32>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    pub state: SessionState,
    pub started_at: DateTime<Utc>,
    /// Last activity: a hook or a tool call from this session.
    pub last_seen: DateTime<Utc>,
    /// Result of the last inbox push: `None` before the first.
    #[serde(default)]
    pub socket_ok: Option<bool>,
}

impl Session {
    /// The committee this session acts on and receives for, if any.
    pub fn committee(&self) -> Option<&CommitteeId> {
        self.joined_committee
            .as_ref()
            .or(self.inferred_committee.as_ref())
    }

    /// How [`Session::committee`] was decided.
    pub fn effective_source(&self) -> Option<CommitteeSource> {
        if self.joined_committee.is_some() {
            Some(CommitteeSource::Join)
        } else if self.inferred_committee.is_some() {
            self.committee_source
        } else {
            None
        }
    }

    pub fn is_live(&self) -> bool {
        self.state == SessionState::Live
    }

    /// Whether the daemon can push into the session between its hooks: an
    /// inbox socket for Claude Code, the app-server for Codex.
    pub fn can_push(&self) -> bool {
        match self.runtime {
            Runtime::Claude => self.socket.is_some(),
            Runtime::Codex => true,
        }
    }
}

/// `<prefix>/<committee>/<person>` → `<committee>`. Any other shape
/// (including a bare `<prefix>/<committee>`, which is too common to bind
/// on) is no committee. The person segment is not checked against this
/// machine's person: a teammate's branch checked out here is still that
/// committee's work.
pub fn committee_from_branch(branch: &str, naming: &BranchNaming) -> Option<CommitteeId> {
    let rest = if naming.prefix().is_empty() {
        branch
    } else {
        branch.strip_prefix(naming.prefix())?.strip_prefix('/')?
    };
    let mut parts = rest.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(name), Some(person), None) if !person.is_empty() => CommitteeId::new(name).ok(),
        _ => None,
    }
}

/// The first line of a `.senate/committee` file that is neither blank nor a
/// `#` comment, if it is a valid committee id.
pub fn committee_from_file(contents: &str) -> Option<CommitteeId> {
    contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .and_then(|line| CommitteeId::new(line).ok())
}

/// Branch first, then the committee file.
pub fn infer_committee(
    branch: Option<&str>,
    committee_file: Option<&str>,
    naming: &BranchNaming,
) -> Option<(CommitteeId, CommitteeSource)> {
    branch
        .and_then(|b| committee_from_branch(b, naming))
        .map(|f| (f, CommitteeSource::Branch))
        .or_else(|| {
            committee_file
                .and_then(committee_from_file)
                .map(|f| (f, CommitteeSource::File))
        })
}

/// What a hook learns about a session's working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitFacts {
    /// Current branch; `None` when detached or not in a repository.
    pub branch: Option<String>,
    pub toplevel: Option<PathBuf>,
    /// Contents of `.senate/committee` at the toplevel (or cwd outside git).
    pub committee_file: Option<String>,
    /// Contents of `.senate/agent` there ([`crate::agent`]).
    pub agent_file: Option<String>,
}

/// Ask git about `cwd`. Each git call is bounded by `timeout`; anything that
/// fails or times out is simply absent.
pub async fn gather_git_facts(cwd: &Path, timeout: Duration) -> GitFacts {
    let (toplevel, branch) = tokio::join!(
        git_line(cwd, &["rev-parse", "--show-toplevel"], timeout),
        git_line(cwd, &["symbolic-ref", "--short", "-q", "HEAD"], timeout),
    );
    let toplevel = toplevel.map(PathBuf::from);
    let base = toplevel.clone().unwrap_or_else(|| cwd.to_path_buf());
    let (committee_path, agent_path) = (base.join(COMMITTEE_FILE), base.join(AGENT_FILE));
    let (committee_file, agent_file) =
        tokio::join!(read_small(&committee_path), read_small(&agent_path));
    GitFacts {
        branch,
        toplevel,
        committee_file,
        agent_file,
    }
}

async fn git_line(cwd: &Path, args: &[&str], timeout: Duration) -> Option<String> {
    let child = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, child).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8(output.stdout).ok()?;
    let line = line.trim();
    (!line.is_empty()).then(|| line.to_string())
}

async fn read_small(path: &Path) -> Option<String> {
    const MAX: usize = 4096;
    let bytes = tokio::fs::read(path).await.ok()?;
    let bytes = &bytes[..bytes.len().min(MAX)];
    Some(String::from_utf8_lossy(bytes).into_owned())
}

/// `start` and the processes above it, nearest first, at most `depth` pids,
/// stopping below pid 1. Each parent is read with `ps -o ppid= -p <pid>`
/// (portable across macOS and Linux) under `timeout`; a failure ends the
/// chain early.
pub async fn ancestor_pids(start: u32, depth: usize, timeout: Duration) -> Vec<u32> {
    let mut out = Vec::new();
    let mut pid = start;
    while out.len() < depth && pid > 1 && !out.contains(&pid) {
        out.push(pid);
        match parent_of(pid, timeout).await {
            Some(parent) => pid = parent,
            None => break,
        }
    }
    out
}

async fn parent_of(pid: u32, timeout: Duration) -> Option<u32> {
    let child = tokio::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(timeout, child).await.ok()?.ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// Whether a process with this pid exists (it may belong to another user).
#[allow(unsafe_code)]
pub fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(2) with signal 0 sends nothing; it only checks that the
    // process exists and may be signalled. It reads and writes no memory of
    // ours, so there is no invariant for Rust to uphold.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_inference_needs_three_segments() {
        let naming = BranchNaming::default();
        let of = |b: &str| committee_from_branch(b, &naming).map(|f| f.to_string());
        assert_eq!(of("feature/refund-v2/alice"), Some("refund-v2".into()));
        assert_eq!(of("feature/refund-v2"), None);
        assert_eq!(of("feature/refund-v2/alice/x"), None);
        assert_eq!(of("feature/Refund/alice"), None);
        assert_eq!(of("main"), None);
        assert_eq!(of("feature/refund-v2/"), None);
        assert_eq!(of("featurex/refund-v2/alice"), None);
    }

    #[test]
    fn the_branch_prefix_is_configurable() {
        let team = BranchNaming::new("team/work");
        assert_eq!(
            committee_from_branch("team/work/refund-v2/alice", &team).map(|f| f.to_string()),
            Some("refund-v2".into())
        );
        assert_eq!(
            committee_from_branch("feature/refund-v2/alice", &team),
            None
        );
        let bare = BranchNaming::new("");
        assert_eq!(
            committee_from_branch("refund-v2/alice", &bare).map(|f| f.to_string()),
            Some("refund-v2".into())
        );
        // The branch names the person, so both of alice's agents share it.
        let naming = BranchNaming::default();
        let committee = CommitteeId::new("refund-v2").unwrap();
        let person = senate_core::Name::new("alice").unwrap();
        let branch = naming.developer(&committee, &person);
        assert_eq!(committee_from_branch(&branch, &naming), Some(committee));
    }

    #[test]
    fn file_inference_skips_comments() {
        assert_eq!(
            committee_from_file("# bound by senate\n\n  refund-v2  \nother\n")
                .map(|f| f.to_string()),
            Some("refund-v2".into())
        );
        assert_eq!(committee_from_file("# nothing\n"), None);
        assert_eq!(committee_from_file("Not A Slug"), None);
    }

    #[test]
    fn branch_beats_file() {
        let n = BranchNaming::default();
        let (f, src) = infer_committee(Some("feature/a/me"), Some("b"), &n).unwrap();
        assert_eq!((f.as_str(), src), ("a", CommitteeSource::Branch));
        let (f, src) = infer_committee(Some("main"), Some("b"), &n).unwrap();
        assert_eq!((f.as_str(), src), ("b", CommitteeSource::File));
        assert!(infer_committee(None, None, &n).is_none());
    }

    #[test]
    fn own_pid_is_alive() {
        assert!(pid_alive(std::process::id()));
        assert!(!pid_alive(0));
    }

    #[tokio::test]
    async fn ancestors_start_at_the_given_pid_and_climb() {
        let me = std::process::id();
        let chain = ancestor_pids(me, 2, Duration::from_secs(5)).await;
        assert_eq!(chain.first(), Some(&me));
        let parent = std::os::unix::process::parent_id();
        if parent > 1 {
            assert_eq!(chain.get(1), Some(&parent), "{chain:?}");
        }
        assert!(ancestor_pids(1, 3, Duration::from_secs(5)).await.is_empty());
    }

    #[tokio::test]
    async fn git_facts_outside_a_repo_read_the_cwd_committee_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".senate")).unwrap();
        std::fs::write(dir.path().join(COMMITTEE_FILE), "refund-v2\n").unwrap();
        std::fs::write(dir.path().join(AGENT_FILE), "reviewer\n").unwrap();
        let facts = gather_git_facts(dir.path(), Duration::from_secs(5)).await;
        assert_eq!(facts.branch, None);
        assert_eq!(facts.toplevel, None);
        assert_eq!(facts.committee_file.as_deref(), Some("refund-v2\n"));
        assert_eq!(facts.agent_file.as_deref(), Some("reviewer\n"));
    }
}
