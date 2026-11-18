//! Coding-agent sessions on this machine: the registry record, how a
//! session's committee is inferred, and whether its process is still alive.
//!
//! Every session speaks as one agent ([`Session::agent`], chosen by
//! [`crate::agent`]), and only that agent's messages reach it.
//!
//! A session's committee is, in order, the first that applies:
//!
//! 1. an explicit `join` (the agent or its user said so; this session only),
//! 2. the git branch, when it is `<prefix>/<committee>/<person>`
//!    ([`BranchNaming`], prefix `feature` unless `senate.toml` says
//!    otherwise) **and** `senate.toml` turns the convention on with
//!    `committee_from_branch = true`. It is off by default: a branch name is
//!    other tools' business too, and `branch_prefix` alone does not enable
//!    it,
//! 3. the first non-comment line of `.senate/committee` at the git toplevel
//!    (or the session's cwd outside git): `senate join` writes it.
//!
//! [`CommitteeSource`] says which one decided.
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
use senate_core::{BranchNaming, CommitteeId, Locale, SenatorName};
use serde::{Deserialize, Serialize};

use crate::agent::{AGENT_FILE, AgentSource};

/// Relative path of the per-worktree committee file.
pub const COMMITTEE_FILE: &str = ".senate/committee";
/// The directory Senate keeps its files in, in a worktree: the committee
/// and agent files and the checkpoint notes. The only directory Senate (or a
/// layer built on it) adds to a repository, and never anything under `.git`.
pub const SENATE_DIR: &str = ".senate";

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

/// Where a session's committee came from. `Branch` is the branch-name
/// convention (stored as `branch`).
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

    /// The words shown to people.
    pub fn label(self, locale: Locale) -> &'static str {
        match self {
            Self::Branch => locale.pick("分支名", "branch name"),
            Self::File => locale.pick("文件", "file"),
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
    /// Local Antigravity CLI with an explicitly registered agentapi endpoint.
    Agy,
    /// An ACP session on a connection opened by the Senate client.
    Acp,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Agy => "agy",
            Self::Acp => "acp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "agy" => Some(Self::Agy),
            "acp" => Some(Self::Acp),
            _ => None,
        }
    }

    fn session_prefix(self) -> Option<&'static str> {
        match self {
            Self::Claude | Self::Codex => None,
            Self::Agy => Some("agy:"),
            Self::Acp => Some("acp:"),
        }
    }

    /// Keep existing Claude and Codex keys; namespace new sessions before
    /// using them as keys in the store, queue, or MCP selector.
    pub fn session_key(self, id: &str) -> String {
        self.session_prefix()
            .map_or_else(|| id.to_owned(), |p| format!("{p}{id}"))
    }

    pub fn native_id(self, key: &str) -> Option<&str> {
        if let Some(prefix) = self.session_prefix() {
            key.strip_prefix(prefix).filter(|id| !id.is_empty())
        } else {
            (!key.is_empty() && !key.starts_with("agy:") && !key.starts_with("acp:")).then_some(key)
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
            Self::Agy => "Antigravity",
            Self::Acp => "ACP",
        }
    }
}

/// The Antigravity hooks and native agentapi use a canonical UUID.
pub fn agy_session_key(id: &str) -> Option<String> {
    (id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        }))
    .then(|| Runtime::Agy.session_key(&id.to_ascii_lowercase()))
}

/// One registered session. The inbox token never leaves the daemon: it is
/// skipped when a session is serialized onto the IPC wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Local session key. Antigravity and ACP keys carry a runtime prefix.
    pub session_id: String,
    #[serde(default)]
    pub runtime: Runtime,
    /// The agent this session speaks as and receives for.
    pub agent: SenatorName,
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
    /// A Claude inbox socket or a Senate-owned ACP listener.
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// Claude inbox token, or agy's SHA-256 launch proof.
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

/// The branch name first, only when `convention` is `Some`
/// (`committee_from_branch` is on), then the committee file.
pub fn infer_committee(
    branch: Option<&str>,
    committee_file: Option<&str>,
    convention: Option<&BranchNaming>,
) -> Option<(CommitteeId, CommitteeSource)> {
    convention
        .zip(branch)
        .and_then(|(naming, b)| committee_from_branch(b, naming))
        .map(|f| (f, CommitteeSource::Branch))
        .or_else(|| {
            committee_file
                .and_then(committee_from_file)
                .map(|f| (f, CommitteeSource::File))
        })
}

/// Create `<base>/.senate/` if it is missing and make sure it holds a
/// `.gitignore` of a single `*` line, so nothing in it is ever committed and
/// no repository's own ignore rules are touched. An existing `.gitignore`
/// is left as it is. Returns the `.senate` path.
pub fn ensure_senate_dir(base: &Path) -> std::io::Result<PathBuf> {
    let dir = base.join(SENATE_DIR);
    std::fs::create_dir_all(&dir)?;
    write_ignore(&dir)?;
    Ok(dir)
}

/// [`ensure_senate_dir`] for a `.senate/` that may already be there: gives
/// one found without a `.gitignore` its own. Creates nothing otherwise.
pub fn heal_senate_dir(base: &Path) -> std::io::Result<()> {
    let dir = base.join(SENATE_DIR);
    if dir.is_dir() {
        write_ignore(&dir)?;
    }
    Ok(())
}

fn write_ignore(dir: &Path) -> std::io::Result<()> {
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(".gitignore"))
    {
        Ok(mut file) => file.write_all(b"*\n"),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// What a hook learns about a session's working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitFacts {
    /// Current branch; `None` when detached or not in a repository.
    pub branch: Option<String>,
    pub toplevel: Option<PathBuf>,
    /// The repository's common git dir (a real path).
    pub common_dir: Option<PathBuf>,
    /// HEAD's commit.
    pub head: Option<String>,
    /// Contents of `.senate/committee` at the toplevel (or cwd outside git).
    pub committee_file: Option<String>,
    /// Contents of `.senate/agent` there ([`crate::agent`]).
    pub agent_file: Option<String>,
}

impl GitFacts {
    /// The worktree these facts describe, inside a git worktree.
    pub fn tree(&self) -> Option<crate::follow::Tree> {
        Some(crate::follow::Tree {
            root: self.toplevel.clone()?,
            common_dir: self.common_dir.clone()?,
            branch: self.branch.clone(),
            head: self.head.clone(),
        })
    }
}

/// What git says about `cwd`, with no file read. Each git call is bounded by
/// `timeout`; anything that fails or times out is simply absent.
async fn git_state(cwd: &Path, timeout: Duration) -> GitFacts {
    let (toplevel, branch, common, head) = tokio::join!(
        git_line(cwd, &["rev-parse", "--show-toplevel"], timeout),
        git_line(cwd, &["symbolic-ref", "--short", "-q", "HEAD"], timeout),
        git_line(cwd, &["rev-parse", "--git-common-dir"], timeout),
        git_line(cwd, &["rev-parse", "-q", "--verify", "HEAD"], timeout),
    );
    GitFacts {
        branch,
        toplevel: toplevel.map(|t| crate::follow::real(Path::new(&t))),
        // Relative to the directory git was asked in.
        common_dir: common.map(|c| crate::follow::real(&cwd.join(c))),
        head,
        committee_file: None,
        agent_file: None,
    }
}

/// Read the files of `.senate/` that bear on a session.
async fn read_senate_files(cwd: &Path, facts: &mut GitFacts) {
    let base = facts.toplevel.clone().unwrap_or_else(|| cwd.to_path_buf());
    // A `.senate/` found without its `.gitignore` gets one.
    if let Err(e) = heal_senate_dir(&base) {
        tracing::debug!("{}: {e}", base.display());
    }
    let (committee_path, agent_path) = (base.join(COMMITTEE_FILE), base.join(AGENT_FILE));
    let (committee_file, agent_file) =
        tokio::join!(read_small(&committee_path), read_small(&agent_path));
    facts.committee_file = committee_file;
    facts.agent_file = agent_file;
}

/// Ask git about `cwd` and read `.senate/` as it is, whatever branch it
/// belongs to. Entry points use [`gather_synced`].
pub async fn gather_git_facts(cwd: &Path, timeout: Duration) -> GitFacts {
    let mut facts = git_state(cwd, timeout).await;
    read_senate_files(cwd, &mut facts).await;
    facts
}

/// [`gather_git_facts`], after making `.senate/` match the checked-out
/// branch ([`crate::follow::sync`]), so what is read belongs to it. Also
/// says what the sync did.
pub async fn gather_synced(
    paths: &crate::paths::Paths,
    cwd: &Path,
    timeout: Duration,
) -> (GitFacts, Option<crate::follow::Outcome>) {
    let mut facts = git_state(cwd, timeout).await;
    let outcome = match facts.tree() {
        Some(tree) => {
            let paths = paths.clone();
            match tokio::task::spawn_blocking(move || crate::follow::sync(&paths, &tree)).await {
                Ok(Ok(outcome)) => Some(outcome),
                Ok(Err(e)) => {
                    tracing::warn!("keeping .senate/ in step with the branch: {e}");
                    None
                }
                Err(_) => None,
            }
        }
        None => None,
    };
    read_senate_files(cwd, &mut facts).await;
    (facts, outcome)
}

pub(crate) async fn git_line(cwd: &Path, args: &[&str], timeout: Duration) -> Option<String> {
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
        if out.len() == depth {
            break;
        }
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
    fn the_branch_name_binds_only_when_the_convention_is_on_and_beats_the_file() {
        let n = BranchNaming::default();
        let of = |b: Option<&str>, f: Option<&str>, on: Option<&BranchNaming>| {
            infer_committee(b, f, on).map(|(c, s)| (c.to_string(), s))
        };
        assert_eq!(
            of(Some("feature/a/me"), Some("b"), Some(&n)),
            Some(("a".into(), CommitteeSource::Branch))
        );
        // Off (the default): the file, or nothing.
        assert_eq!(
            of(Some("feature/a/me"), Some("b"), None),
            Some(("b".into(), CommitteeSource::File))
        );
        assert_eq!(of(Some("feature/a/me"), None, None), None);
        // On, but the branch is not the convention's shape.
        assert_eq!(
            of(Some("main"), Some("b"), Some(&n)),
            Some(("b".into(), CommitteeSource::File))
        );
        assert_eq!(of(None, None, Some(&n)), None);
    }

    #[test]
    fn sources_read_in_both_languages() {
        assert_eq!(CommitteeSource::Branch.label(Locale::En), "branch name");
        assert_eq!(CommitteeSource::Branch.label(Locale::Zh), "分支名");
        assert_eq!(CommitteeSource::File.label(Locale::En), "file");
        assert_eq!(CommitteeSource::File.label(Locale::Zh), "文件");
        assert_eq!(CommitteeSource::Join.label(Locale::Zh), "join");
        for s in [
            CommitteeSource::Branch,
            CommitteeSource::File,
            CommitteeSource::Join,
        ] {
            assert_eq!(CommitteeSource::parse(s.as_str()), Some(s));
        }
    }

    #[test]
    fn the_senate_directory_ignores_itself_and_keeps_an_existing_ignore_file() {
        let dir = tempfile::tempdir().unwrap();
        // Healing creates nothing where there is no `.senate/`.
        heal_senate_dir(dir.path()).unwrap();
        assert!(!dir.path().join(".senate").exists());
        let senate = ensure_senate_dir(dir.path()).unwrap();
        assert_eq!(senate, dir.path().join(".senate"));
        let ignore = senate.join(".gitignore");
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "*\n");
        std::fs::write(&ignore, "mine\n").unwrap();
        ensure_senate_dir(dir.path()).unwrap();
        heal_senate_dir(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "mine\n");
        // A directory found without one gets it.
        std::fs::remove_file(&ignore).unwrap();
        heal_senate_dir(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "*\n");
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
