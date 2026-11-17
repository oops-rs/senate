//! What the Speaker asks a [`crate::Runner`] to do, and the rules every
//! order must pass before anything runs: only configured repositories, on
//! behalf of a named person, from one named agent's checkpoint, never on a
//! developer's branch.

use std::fmt;

use senate_core::continuity::{CHECKPOINT_DIR, DECISIONS_FILE, OPEN_FILE, PROGRESS_FILE};
use senate_core::{BranchNaming, CheckpointFiles, CommitteeId, Locale, Name, SenatorName};
use serde::{Deserialize, Serialize};

/// Default wall clock for one run.
pub const DEFAULT_WALL_SECS: u64 = 30 * 60;
/// Default turn cap for one run.
pub const DEFAULT_MAX_TURNS: u32 = 200;
/// Default PR base when the checkpoint branch's upstream base is unknown.
pub const DEFAULT_PR_BASE: &str = "main";

/// One piece of work on request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkOrder {
    pub committee: CommitteeId,
    /// The Speaker's run number; the branch is
    /// [`BranchNaming::speaker`] of it.
    pub run_id: u64,
    /// The person who asked and confirmed. Always a person: agents and
    /// guests may not ask the Speaker to write code.
    pub requester: Name,
    /// How to name the requester in commits and the PR, e.g. "Bob".
    pub on_behalf_of: String,
    /// The agent whose checkpoint the run starts from. Always one agent,
    /// named in full: a person with several agents has said (or been asked)
    /// which one before the order is made, and the work may be handed
    /// between two agents of the same person.
    pub target_agent: SenatorName,
    /// The task, in the requester's words (after the restatement they confirmed).
    pub instruction: String,
    pub repo: RepoRef,
    /// The checkpoint branch, e.g. `feature/refund-v2/alice`. Read, never written.
    pub base_branch: String,
    /// The checkpoint commit the Speaker branch starts from.
    pub base_commit: String,
    /// What the PR merges into: the checkpoint branch's upstream base.
    #[serde(default = "default_pr_base")]
    pub pr_base: String,
    #[serde(default)]
    pub checkpoint_files: CheckpointFiles,
    #[serde(default)]
    pub budget: Budget,
    /// How branch names are put together; the run's own branch is
    /// [`BranchNaming::speaker`].
    #[serde(default)]
    pub branches: BranchNaming,
    /// The language of what the run writes for people: step labels, the
    /// summary when the agent left none, next steps, the PR.
    #[serde(default)]
    pub locale: Locale,
}

fn default_pr_base() -> String {
    DEFAULT_PR_BASE.to_owned()
}

/// `(file name under the notes directory, content)` for each of the
/// checkpoint's three notes that is present. The names are the ones the
/// agent itself keeps in its worktree ([`CHECKPOINT_DIR`]), so the coding
/// agent reads them where an agent would.
pub fn checkpoint_notes(files: &CheckpointFiles) -> Vec<(&'static str, &str)> {
    [
        (PROGRESS_FILE, &files.progress),
        (DECISIONS_FILE, &files.decisions),
        (OPEN_FILE, &files.open),
    ]
    .into_iter()
    .filter_map(|(name, body)| body.as_deref().map(|b| (name, b)))
    .collect()
}

/// Where the notes go in the checkout. Never committed.
pub const NOTES_DIR: &str = CHECKPOINT_DIR;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    #[serde(default = "default_wall_secs")]
    pub wall_secs: u64,
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
}

fn default_wall_secs() -> u64 {
    DEFAULT_WALL_SECS
}

fn default_max_turns() -> u32 {
    DEFAULT_MAX_TURNS
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            wall_secs: DEFAULT_WALL_SECS,
            max_turns: DEFAULT_MAX_TURNS,
        }
    }
}

/// A repository: where to clone it from and its `owner/name` on the forge.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RepoRef {
    /// The checkpoint's remote URL (`git@github.com:o/n.git`, `https://…`).
    pub url: String,
    /// `owner/name`.
    pub slug: String,
}

impl RepoRef {
    /// From a remote URL; the slug is its last two path segments.
    pub fn from_url(url: &str) -> Result<Self, WorkError> {
        let slug = slug_from_url(url).ok_or_else(|| WorkError::BadRepoUrl(url.to_owned()))?;
        Ok(Self {
            url: url.to_owned(),
            slug,
        })
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.slug)
    }
}

/// `owner/name` from `git@host:owner/name.git`, `https://host/owner/name`,
/// `ssh://git@host/owner/name.git`, `file:///x/owner/name.git`.
pub fn slug_from_url(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut parts = trimmed.rsplit(['/', ':']);
    let name = parts.next()?;
    let owner = parts.next()?;
    let slug = format!("{owner}/{name}");
    valid_slug(&slug).then_some(slug)
}

/// `owner/name`, each of `[A-Za-z0-9._-]`, not starting with `.` or `-`.
pub fn valid_slug(slug: &str) -> bool {
    let mut it = slug.split('/');
    let (Some(owner), Some(name), None) = (it.next(), it.next(), it.next()) else {
        return false;
    };
    [owner, name].iter().all(|p| {
        !p.is_empty()
            && p.len() <= 100
            && !p.starts_with(['.', '-'])
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

/// The repositories the server config lists (`[forge] repos = [...]`).
/// Matching is case-insensitive, as GitHub's is.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepoAllowlist(Vec<String>);

impl RepoAllowlist {
    pub fn new<I, S>(repos: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(repos.into_iter().map(Into::into).collect())
    }

    pub fn allows(&self, slug: &str) -> bool {
        self.0.iter().any(|r| r.eq_ignore_ascii_case(slug))
    }

    pub fn check(&self, slug: &str) -> Result<(), WorkError> {
        if self.allows(slug) {
            Ok(())
        } else {
            Err(WorkError::RepoNotAllowed(slug.to_owned()))
        }
    }
}

/// Why an order (or a PR request) was refused before anything ran.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkError {
    #[error("repository {0} is not in the configured list")]
    RepoNotAllowed(String),
    #[error("cannot tell owner/name from the remote URL {0:?}")]
    BadRepoUrl(String),
    #[error("repository slug {slug:?} does not match the remote URL {url:?}")]
    RepoMismatch { slug: String, url: String },
    #[error("not a usable branch name: {0:?}")]
    BadBranch(String),
    #[error("not a commit id: {0:?}")]
    BadCommit(String),
    #[error(
        "refusing to push to {0}: the Speaker only pushes its own <prefix>/<committee>/speaker-<n> branches"
    )]
    NotSpeakerBranch(String),
    #[error("refusing to push to {0}: it is the checkpoint branch")]
    BranchIsBase(String),
    #[error("the instruction is empty")]
    EmptyInstruction,
    #[error("a run starts from an agent's checkpoint, and {0} is a person")]
    TargetNotAnAgent(SenatorName),
}

/// One `/`-free component of a git ref, safe for `git check-ref-format`:
/// lowercase `[a-z0-9._-]`, other characters become `-`, no leading `.` or
/// `-`, no `..`, no trailing `.lock` or `.`; never empty (`x`).
pub fn sanitize_ref_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars().flat_map(char::to_lowercase) {
        let c = if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            c
        } else {
            '-'
        };
        if (c == '.' || c == '-') && out.ends_with(c) {
            continue;
        }
        out.push(c);
    }
    let mut s = out.trim_start_matches(['.', '-']).to_owned();
    while s.ends_with(".lock") || s.ends_with('.') || s.ends_with('-') {
        if let Some(stripped) = s.strip_suffix(".lock") {
            s = stripped.to_owned();
        } else {
            s.pop();
        }
    }
    s.truncate(63);
    if s.is_empty() { "x".to_owned() } else { s }
}

/// A branch name we are willing to hand to git as an argument: the rules of
/// `git check-ref-format --branch`, plus no leading `-` (no option injection).
pub fn valid_branch_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with(['-', '/'])
        && !name.ends_with(['/', '.'])
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && !name.contains("@{")
        && name != "@"
        && !name
            .split('/')
            .any(|c| c.starts_with('.') || c.ends_with(".lock"))
        && name.bytes().all(|b| {
            b > 0x20 && b != 0x7f && !matches!(b, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
        })
}

/// 7–64 hex digits (an abbreviated or full SHA-1/SHA-256 object id).
pub fn valid_commit(id: &str) -> bool {
    (7..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

impl WorkOrder {
    /// The branch this run creates and pushes.
    pub fn branch(&self) -> String {
        self.branches.speaker(&self.committee, self.run_id)
    }

    /// `branch@commit` of the checkpoint, short commit.
    pub fn checkpoint_ref(&self) -> String {
        let short = &self.base_commit[..self.base_commit.len().min(12)];
        format!("{}@{short}", self.base_branch)
    }

    /// The instruction's first non-empty line.
    pub fn instruction_headline(&self) -> &str {
        self.instruction
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
    }

    /// Every rule an order must pass before a runner touches anything.
    pub fn validate(&self, allowed: &RepoAllowlist) -> Result<(), WorkError> {
        // Whether the target is an agent is the roster's to say, and the
        // Speaker checks it before it orders anything; an order alone knows
        // one person, the requester, who is never the agent.
        if self.target_agent == self.requester {
            return Err(WorkError::TargetNotAnAgent(self.target_agent.clone()));
        }
        if !valid_slug(&self.repo.slug) {
            return Err(WorkError::BadRepoUrl(self.repo.slug.clone()));
        }
        match slug_from_url(&self.repo.url) {
            Some(s) if s.eq_ignore_ascii_case(&self.repo.slug) => {}
            Some(_) => {
                return Err(WorkError::RepoMismatch {
                    slug: self.repo.slug.clone(),
                    url: self.repo.url.clone(),
                });
            }
            None => return Err(WorkError::BadRepoUrl(self.repo.url.clone())),
        }
        allowed.check(&self.repo.slug)?;
        if self.repo.url.starts_with('-') {
            return Err(WorkError::BadRepoUrl(self.repo.url.clone()));
        }
        for b in [&self.base_branch, &self.pr_base] {
            if !valid_branch_name(b) {
                return Err(WorkError::BadBranch(b.clone()));
            }
        }
        if !valid_commit(&self.base_commit) {
            return Err(WorkError::BadCommit(self.base_commit.clone()));
        }
        if self.instruction_headline().is_empty() {
            return Err(WorkError::EmptyInstruction);
        }
        check_push_target(
            &self.branches,
            &self.committee,
            &self.base_branch,
            &self.branch(),
        )
    }
}

/// The push rule, so a developer's own branch is never written: the target
/// must be one of this committee's Speaker branches under `branches` and
/// must not be the branch the work started from.
pub fn check_push_target(
    branches: &BranchNaming,
    committee: &CommitteeId,
    base_branch: &str,
    target: &str,
) -> Result<(), WorkError> {
    if !valid_branch_name(target) {
        return Err(WorkError::BadBranch(target.to_owned()));
    }
    if target == base_branch {
        return Err(WorkError::BranchIsBase(target.to_owned()));
    }
    if !branches.is_speaker(committee, target) {
        return Err(WorkError::NotSpeakerBranch(target.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn sample_order() -> WorkOrder {
    WorkOrder {
        committee: CommitteeId::new("refund-v2").unwrap(),
        run_id: 7,
        requester: Name::new("bob").unwrap(),
        on_behalf_of: "Bob".into(),
        target_agent: "claude".parse().unwrap(),
        instruction: "把 alice 的重试路径做完\nwith tests".into(),
        repo: RepoRef::from_url("git@github.com:example/refund.git").unwrap(),
        base_branch: "feature/refund-v2/alice".into(),
        base_commit: "0123456789abcdef0123456789abcdef01234567".into(),
        pr_base: "main".into(),
        checkpoint_files: CheckpointFiles {
            progress: Some("retry path half done".into()),
            decisions: None,
            open: Some("backoff cap?".into()),
        },
        budget: Budget::default(),
        branches: BranchNaming::default(),
        locale: Locale::Zh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow() -> RepoAllowlist {
        RepoAllowlist::new(["example/refund"])
    }

    fn committee() -> CommitteeId {
        CommitteeId::new("refund-v2").unwrap()
    }

    #[test]
    fn branch_is_committee_speaker_run() {
        let o = sample_order();
        assert_eq!(o.branch(), "feature/refund-v2/speaker-7");
        assert!(o.branches.is_speaker(&o.committee, &o.branch()));
        assert!(valid_branch_name(&o.branch()));
    }

    #[test]
    fn the_branch_prefix_comes_from_the_order() {
        let mut o = sample_order();
        o.branches = BranchNaming::new("team");
        o.base_branch = "team/refund-v2/alice".into();
        assert_eq!(o.branch(), "team/refund-v2/speaker-7");
        o.validate(&allow()).unwrap();
        // A `feature/` Speaker branch is not this deployment's.
        assert_eq!(
            check_push_target(
                &o.branches,
                &o.committee,
                "main",
                "feature/refund-v2/speaker-7"
            ),
            Err(WorkError::NotSpeakerBranch(
                "feature/refund-v2/speaker-7".into()
            ))
        );
    }

    #[test]
    fn sanitize_ref_component_rules() {
        assert_eq!(sanitize_ref_component("Refund V2"), "refund-v2");
        assert_eq!(sanitize_ref_component("..a..b.lock"), "a.b");
        assert_eq!(sanitize_ref_component("-x--y-"), "x-y");
        assert_eq!(sanitize_ref_component("a:b~c^d"), "a-b-c-d");
        assert_eq!(sanitize_ref_component("重试"), "x");
        assert_eq!(sanitize_ref_component(""), "x");
        assert_eq!(sanitize_ref_component(&"a".repeat(100)).len(), 63);
        assert!(valid_branch_name(&format!(
            "feature/{}/speaker-1",
            sanitize_ref_component("@{weird} name.lock")
        )));
    }

    #[test]
    fn speaker_branch_detection() {
        let f = committee();
        let b = BranchNaming::default();
        assert!(b.is_speaker(&f, "feature/refund-v2/speaker-12"));
        assert!(!b.is_speaker(&f, "feature/refund-v2/alice"));
        assert!(!b.is_speaker(&f, "feature/refund-v2/speaker-"));
        assert!(!b.is_speaker(&f, "feature/refund-v2/speaker-1x"));
        assert!(!b.is_speaker(&f, "feature/other/speaker-1"));
        assert!(!b.is_speaker(&f, "feature/refund-v2/speaker-1/alice"));
    }

    #[test]
    fn branch_names() {
        for ok in ["main", "feature/refund-v2/alice", "release-1.2"] {
            assert!(valid_branch_name(ok), "{ok}");
        }
        for bad in [
            "",
            "-f",
            "a..b",
            "a b",
            "a~1",
            "a:b",
            "x.lock",
            "a/.hidden",
            "a/",
            "a@{1}",
            "@",
            "a//b",
        ] {
            assert!(!valid_branch_name(bad), "{bad}");
        }
    }

    #[test]
    fn slugs_from_urls() {
        for (url, slug) in [
            ("git@github.com:example/refund.git", "example/refund"),
            ("https://github.com/example/refund", "example/refund"),
            ("https://github.com/example/refund.git/", "example/refund"),
            ("ssh://git@github.com/o/n.git", "o/n"),
            ("file:///tmp/x/o/n.git", "o/n"),
        ] {
            assert_eq!(slug_from_url(url).as_deref(), Some(slug), "{url}");
        }
        assert_eq!(slug_from_url("refund"), None);
        assert_eq!(slug_from_url("https://github.com/-o/n"), None);
    }

    #[test]
    fn valid_order_passes() {
        sample_order().validate(&allow()).unwrap();
        // GitHub matches case-insensitively.
        sample_order()
            .validate(&RepoAllowlist::new(["Example/Refund"]))
            .unwrap();
    }

    #[test]
    fn refuses_repo_not_allowed() {
        let err = sample_order()
            .validate(&RepoAllowlist::new(["example/other"]))
            .unwrap_err();
        assert_eq!(err, WorkError::RepoNotAllowed("example/refund".into()));
        assert!(matches!(
            sample_order().validate(&RepoAllowlist::default()),
            Err(WorkError::RepoNotAllowed(_))
        ));
    }

    #[test]
    fn refuses_slug_that_does_not_match_url() {
        let mut o = sample_order();
        o.repo.slug = "example/other".into();
        let allowed = RepoAllowlist::new(["example/other", "example/refund"]);
        assert!(matches!(
            o.validate(&allowed),
            Err(WorkError::RepoMismatch { .. })
        ));
    }

    #[test]
    fn refuses_bad_refs() {
        let mut o = sample_order();
        o.base_branch = "--upload-pack=evil".into();
        assert!(matches!(o.validate(&allow()), Err(WorkError::BadBranch(_))));
        let mut o = sample_order();
        o.base_commit = "HEAD~1".into();
        assert!(matches!(o.validate(&allow()), Err(WorkError::BadCommit(_))));
        let mut o = sample_order();
        o.instruction = "  \n ".into();
        assert_eq!(o.validate(&allow()), Err(WorkError::EmptyInstruction));
    }

    #[test]
    fn refuses_developer_branch_and_base() {
        let f = committee();
        let b = BranchNaming::default();
        assert_eq!(
            check_push_target(&b, &f, "main", "feature/refund-v2/alice"),
            Err(WorkError::NotSpeakerBranch(
                "feature/refund-v2/alice".into()
            ))
        );
        assert_eq!(
            check_push_target(
                &b,
                &f,
                "feature/refund-v2/speaker-3",
                "feature/refund-v2/speaker-3"
            ),
            Err(WorkError::BranchIsBase(
                "feature/refund-v2/speaker-3".into()
            ))
        );
        assert!(matches!(
            check_push_target(&b, &f, "main", "main"),
            Err(WorkError::BranchIsBase(_))
        ));
        // A run continuing from an earlier Speaker branch is fine.
        let mut o = sample_order();
        o.base_branch = "feature/refund-v2/speaker-3".into();
        o.validate(&allow()).unwrap();
        // ...but not one whose own branch is the checkpoint branch.
        o.run_id = 3;
        assert!(matches!(
            o.validate(&allow()),
            Err(WorkError::BranchIsBase(_))
        ));
    }

    #[test]
    fn order_roundtrips_and_defaults() {
        let o = sample_order();
        let json = serde_json::to_value(&o).unwrap();
        assert_eq!(json["requester"], "bob");
        assert_eq!(json["target_agent"], "claude");
        assert_eq!(json["branches"], "feature");
        assert_eq!(json["locale"], "zh");
        let back: WorkOrder = serde_json::from_value(json).unwrap();
        assert_eq!(back, o);
        let minimal = serde_json::json!({
            "committee": "refund-v2", "run_id": 1, "requester": "bob", "on_behalf_of": "Bob",
            "target_agent": "claude",
            "instruction": "x", "repo": {"url": "git@github.com:o/n.git", "slug": "o/n"},
            "base_branch": "feature/refund-v2/alice", "base_commit": "abcdef0"
        });
        let m: WorkOrder = serde_json::from_value(minimal.clone()).unwrap();
        assert_eq!(m.pr_base, "main");
        assert_eq!(
            m.budget,
            Budget {
                wall_secs: 1800,
                max_turns: 200
            }
        );
        assert_eq!(m.checkpoint_files, CheckpointFiles::default());
        assert_eq!(m.branches, BranchNaming::default());
        assert_eq!(m.locale, Locale::default());
    }

    #[test]
    fn an_order_names_a_person_who_asked_and_one_agent() {
        let base = serde_json::to_value(sample_order()).unwrap();
        for (field, bad) in [
            ("requester", "bob/claude"),
            ("requester", "senator::bob"),
            ("target_agent", "*"),
            ("target_agent", "senator::claude"),
        ] {
            let mut v = base.clone();
            v[field] = bad.into();
            assert!(
                serde_json::from_value::<WorkOrder>(v).is_err(),
                "{field}={bad}"
            );
        }
        let mut o = sample_order();
        o.target_agent = o.requester.as_str().parse().unwrap();
        assert_eq!(
            o.validate(&allow()),
            Err(WorkError::TargetNotAnAgent(o.target_agent.clone()))
        );
        // Handing work between two agents of one person is an ordinary order.
        let mut o = sample_order();
        o.requester = Name::new("alice").unwrap();
        o.target_agent = "codex".parse().unwrap();
        o.validate(&allow()).unwrap();
    }

    #[test]
    fn headline_checkpoint_ref_and_notes() {
        let o = sample_order();
        assert_eq!(o.instruction_headline(), "把 alice 的重试路径做完");
        assert_eq!(o.checkpoint_ref(), "feature/refund-v2/alice@0123456789ab");
        assert_eq!(
            checkpoint_notes(&o.checkpoint_files),
            [
                ("PROGRESS.md", "retry path half done"),
                ("OPEN.md", "backoff cap?")
            ]
        );
    }
}
