//! The committee `senate join` binds when no name is given.
//!
//! The rule, in three lines:
//!
//! 1. The repository's name is the last path segment of the `origin` remote's
//!    URL without a trailing `.git`; with no `origin`, the folder of the main
//!    worktree (a bare layout: the bare directory without `.git`); outside
//!    git, the current directory's name.
//! 2. On the default branch (`refs/remotes/origin/HEAD`'s target, else `main`,
//!    `master` or `trunk`) or with a detached HEAD the committee is the
//!    repository name; on any other branch, `<repository>-<branch>`.
//! 3. Each part is lowercased (ASCII), every run of characters outside
//!    `[a-z0-9]` becomes one `-`, `-` is trimmed from the ends; over 63
//!    characters it is cut and ends in `-` and an 8-digit hash of the whole.
//!
//! [`choose`] is pure: it takes the [`Facts`]. [`gather`] reads them with
//! read-only git commands and writes nothing.

use std::path::Path;
use std::time::Duration;

use senate_core::CommitteeId;

/// The longest committee id.
const MAX_LEN: usize = 63;
/// `-` and the hash digits a truncated id ends in.
const HASH_DIGITS: usize = 8;

/// What the rule needs to know about where `senate join` is run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Facts {
    /// The `origin` remote's URL, when there is one.
    pub origin_url: Option<String>,
    /// The folder name of the repository's main worktree; `None` outside git.
    pub main_worktree: Option<String>,
    /// The current directory's name.
    pub directory: Option<String>,
    /// The current branch; `None` when detached or outside git.
    pub branch: Option<String>,
    /// What `refs/remotes/origin/HEAD` points at, as a branch name.
    pub default_branch: Option<String>,
}

/// Where the repository's name came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameFrom {
    /// The `origin` remote's URL.
    Origin,
    /// The folder of the main worktree (no `origin`).
    MainWorktree,
    /// The current directory (not a git repository).
    Directory,
}

/// What the branch contributed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchPart {
    /// Added to the name: a branch that is not the default one.
    Added(String),
    /// Adds nothing: the default branch.
    Default(String),
    /// Adds nothing: a detached HEAD.
    Detached,
    /// There is no branch: not a git repository.
    NoGit,
}

/// A name the rule made, with what it was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    pub id: CommitteeId,
    /// The repository's name, as found (before it was made an id).
    pub repository: String,
    pub from: NameFrom,
    pub branch: BranchPart,
}

/// Why no name can be made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cannot {
    /// The repository (or directory) name has no ASCII letter or digit.
    Repository(String),
    /// The branch name has no ASCII letter or digit.
    Branch(String),
}

/// A name given, or one derived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Given(CommitteeId),
    Derived(Derived),
}

impl Choice {
    pub fn id(&self) -> &CommitteeId {
        match self {
            Self::Given(id) => id,
            Self::Derived(d) => &d.id,
        }
    }
}

/// An explicit name always wins, untouched; otherwise the derived one.
pub fn choose(explicit: Option<CommitteeId>, facts: &Facts) -> Result<Choice, Cannot> {
    match explicit {
        Some(id) => Ok(Choice::Given(id)),
        None => derive(facts).map(Choice::Derived),
    }
}

/// The last path segment of a remote URL, without a trailing `.git`. Reads
/// `https://host/org/repo.git`, `git@host:org/repo.git`,
/// `ssh://git@host:22/org/repo.git`, local paths, trailing slashes.
pub fn repository_of_url(url: &str) -> Option<String> {
    let mut rest = url.trim().trim_end_matches(['/', '\\']);
    rest = rest.strip_suffix(".git").unwrap_or(rest);
    rest = rest.trim_end_matches(['/', '\\']);
    let segment = rest.rsplit(['/', '\\', ':']).next()?;
    (!segment.is_empty()).then(|| segment.to_string())
}

/// The name of a main worktree from the repository's common git dir: the
/// folder holding `.git`, or for a bare repository the directory itself
/// without a trailing `.git`.
pub fn main_worktree_name(common_dir: &Path) -> Option<String> {
    let name = common_dir.file_name()?.to_str()?;
    if name == ".git" {
        return common_dir
            .parent()?
            .file_name()?
            .to_str()
            .map(str::to_string);
    }
    Some(name.strip_suffix(".git").unwrap_or(name).to_string())
}

/// Lowercase, every run of other characters one `-`, `-` trimmed from the
/// ends. Empty when nothing usable remains.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(out.trim_end_matches('-').len());
    out
}

/// FNV-1a over the bytes, 64 bits, folded to 32: stable across versions and
/// platforms, which `DefaultHasher` does not promise.
fn short_hash(text: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", ((h >> 32) ^ h) & 0xffff_ffff)
}

/// A slug within 63 characters: longer ones are cut, and end in `-` and a
/// hash of the whole, so long names sharing a prefix stay apart.
fn fit(slug: String) -> String {
    if slug.len() <= MAX_LEN {
        return slug;
    }
    let keep = MAX_LEN - 1 - HASH_DIGITS;
    let head = slug[..keep].trim_end_matches('-');
    format!("{head}-{}", short_hash(&slug))
}

fn is_default(branch: &str, default_branch: Option<&str>) -> bool {
    match default_branch {
        Some(d) => branch == d,
        None => matches!(branch, "main" | "master" | "trunk"),
    }
}

/// The rule.
pub fn derive(facts: &Facts) -> Result<Derived, Cannot> {
    let (repository, from) = match (&facts.main_worktree, &facts.origin_url) {
        (Some(main), origin) => match origin.as_deref().and_then(repository_of_url) {
            Some(name) => (name, NameFrom::Origin),
            None => (main.clone(), NameFrom::MainWorktree),
        },
        (None, _) => (
            facts.directory.clone().unwrap_or_default(),
            NameFrom::Directory,
        ),
    };
    let base = slug(&repository);
    if base.is_empty() {
        return Err(Cannot::Repository(repository));
    }
    let in_git = facts.main_worktree.is_some();
    let (text, branch) = match (&facts.branch, in_git) {
        (_, false) => (base, BranchPart::NoGit),
        (None, true) => (base, BranchPart::Detached),
        (Some(b), true) if is_default(b, facts.default_branch.as_deref()) => {
            (base, BranchPart::Default(b.clone()))
        }
        (Some(b), true) => {
            let part = slug(b);
            if part.is_empty() {
                return Err(Cannot::Branch(b.clone()));
            }
            (format!("{base}-{part}"), BranchPart::Added(b.clone()))
        }
    };
    let id = CommitteeId::new(fit(text)).map_err(|_| Cannot::Repository(repository.clone()))?;
    Ok(Derived {
        id,
        repository,
        from,
        branch,
    })
}

/// Read the facts with read-only git commands; anything git does not say
/// is absent. Nothing is written, under `.git` or anywhere.
pub async fn gather(cwd: &Path, timeout: Duration) -> Facts {
    use crate::session::git_line;
    let (common, branch, origin, origin_head) = tokio::join!(
        git_line(cwd, &["rev-parse", "--git-common-dir"], timeout),
        git_line(cwd, &["symbolic-ref", "--short", "-q", "HEAD"], timeout),
        git_line(cwd, &["config", "--get", "remote.origin.url"], timeout),
        git_line(
            cwd,
            &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"],
            timeout
        ),
    );
    let main_worktree = common.and_then(|c| main_worktree_name(&crate::follow::real(&cwd.join(c))));
    Facts {
        origin_url: origin,
        branch: main_worktree.as_ref().and(branch),
        default_branch: origin_head
            .as_deref()
            .and_then(|r| r.strip_prefix("refs/remotes/origin/"))
            .map(str::to_string),
        main_worktree,
        directory: crate::follow::real(cwd)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_facts(origin: Option<&str>, main: &str, branch: Option<&str>) -> Facts {
        Facts {
            origin_url: origin.map(str::to_string),
            main_worktree: Some(main.to_string()),
            directory: Some("anywhere".to_string()),
            branch: branch.map(str::to_string),
            default_branch: None,
        }
    }

    fn id(facts: &Facts) -> String {
        derive(facts).unwrap().id.to_string()
    }

    #[test]
    fn every_origin_url_form_gives_the_repository_name() {
        for url in [
            "https://github.com/org/Senate.git",
            "https://github.com/org/Senate",
            "https://github.com/org/Senate/",
            "https://github.com/org/Senate.git/",
            "git@github.com:org/Senate.git",
            "git@github.com:org/Senate",
            "git@github.com:Senate.git",
            "ssh://git@github.com:22/org/Senate.git",
            "/srv/git/Senate.git",
            "/srv/git/Senate/",
            "../Senate",
            "C:\\work\\Senate.git",
        ] {
            let f = git_facts(Some(url), "folder", Some("main"));
            let d = derive(&f).unwrap();
            assert_eq!(d.id.as_str(), "senate", "{url}");
            assert_eq!(d.repository, "Senate", "{url}");
            assert_eq!(d.from, NameFrom::Origin, "{url}");
        }
    }

    #[test]
    fn a_folder_named_differently_from_the_origin_is_ignored() {
        let f = git_facts(Some("git@h:o/real.git"), "my-clone", Some("main"));
        assert_eq!(id(&f), "real");
    }

    #[test]
    fn without_an_origin_the_main_worktree_folder_names_it() {
        let d = derive(&git_facts(None, "My Repo", Some("main"))).unwrap();
        assert_eq!(d.id.as_str(), "my-repo");
        assert_eq!(d.from, NameFrom::MainWorktree);
        // An origin with nothing usable in it falls back the same way.
        let d = derive(&git_facts(Some("///"), "folder", Some("main"))).unwrap();
        assert_eq!(d.id.as_str(), "folder");
        assert_eq!(d.from, NameFrom::MainWorktree);
    }

    #[test]
    fn the_main_worktree_of_a_normal_or_bare_layout() {
        assert_eq!(
            main_worktree_name(Path::new("/w/proj/.git")).as_deref(),
            Some("proj")
        );
        assert_eq!(
            main_worktree_name(Path::new("/w/proj.git")).as_deref(),
            Some("proj")
        );
        assert_eq!(
            main_worktree_name(Path::new("/w/proj")).as_deref(),
            Some("proj")
        );
        assert_eq!(main_worktree_name(Path::new("/")), None);
    }

    #[test]
    fn a_bare_layout_derives_from_the_bare_directory() {
        let name = main_worktree_name(Path::new("/w/Proj.git")).unwrap();
        let f = git_facts(None, &name, Some("main"));
        assert_eq!(id(&f), "proj");
    }

    #[test]
    fn outside_git_the_directory_names_it_and_the_branch_is_not_looked_at() {
        let f = Facts {
            directory: Some("Scratch_Pad".to_string()),
            branch: Some("whatever".to_string()),
            ..Facts::default()
        };
        let d = derive(&f).unwrap();
        assert_eq!(d.id.as_str(), "scratch-pad");
        assert_eq!(d.from, NameFrom::Directory);
        assert_eq!(d.branch, BranchPart::NoGit);
    }

    #[test]
    fn the_default_branch_is_origin_head_else_the_well_known_names() {
        let mut f = git_facts(Some("h:o/r.git"), "x", Some("develop"));
        // No origin/HEAD, and `develop` is not a well-known name.
        assert_eq!(id(&f), "r-develop");
        for name in ["main", "master", "trunk"] {
            f.branch = Some(name.to_string());
            assert_eq!(id(&f), "r", "{name}");
        }
        // origin/HEAD decides when it exists.
        f.default_branch = Some("develop".to_string());
        f.branch = Some("develop".to_string());
        let d = derive(&f).unwrap();
        assert_eq!(d.id.as_str(), "r");
        assert_eq!(d.branch, BranchPart::Default("develop".to_string()));
        // And then `main` is just another branch.
        f.branch = Some("main".to_string());
        assert_eq!(id(&f), "r-main");
        // A default branch with a slash.
        f.default_branch = Some("release/1".to_string());
        f.branch = Some("release/1".to_string());
        assert_eq!(id(&f), "r");
    }

    #[test]
    fn a_feature_branch_with_slashes_dots_underscores_and_capitals() {
        let f = git_facts(Some("git@h:o/Senate.git"), "x", Some("Fix/Inbox_v1.2 beta"));
        let d = derive(&f).unwrap();
        assert_eq!(d.id.as_str(), "senate-fix-inbox-v1-2-beta");
        assert_eq!(
            d.branch,
            BranchPart::Added("Fix/Inbox_v1.2 beta".to_string())
        );
        let f = git_facts(Some("h:o/senate"), "x", Some("fix//inbox--"));
        assert_eq!(id(&f), "senate-fix-inbox");
    }

    #[test]
    fn a_detached_head_is_the_repository_alone() {
        let d = derive(&git_facts(Some("h:o/r"), "x", None)).unwrap();
        assert_eq!(d.id.as_str(), "r");
        assert_eq!(d.branch, BranchPart::Detached);
    }

    #[test]
    fn the_result_never_starts_with_a_separator_and_may_start_with_a_digit() {
        assert_eq!(id(&git_facts(None, "__.Repo-", Some("main"))), "repo");
        assert_eq!(id(&git_facts(None, "-x", Some("main"))), "x");
        assert_eq!(id(&git_facts(None, "2fa", Some("main"))), "2fa");
        assert_eq!(id(&git_facts(None, "r", Some("--2/x"))), "r-2-x");
    }

    #[test]
    fn long_names_are_cut_at_63_with_a_hash_and_stay_apart() {
        let prefix = "a".repeat(70);
        let one = git_facts(None, &format!("{prefix}one"), Some("main"));
        let two = git_facts(None, &format!("{prefix}two"), Some("main"));
        let (a, b) = (id(&one), id(&two));
        assert_eq!(a.len(), 63, "{a}");
        assert_eq!(b.len(), 63, "{b}");
        assert_ne!(a, b);
        assert!(a.starts_with(&"a".repeat(54)), "{a}");
        assert_eq!(a.as_bytes()[54], b'-');
        assert!(CommitteeId::new(a.clone()).is_ok());
        // Stable: the same facts give the same id.
        assert_eq!(a, id(&one));
        // A cut that lands on a `-` does not double it.
        let name = format!("{}-{}", "b".repeat(53), "c".repeat(30));
        let cut = id_of(&name);
        assert_eq!(cut.len(), 62, "{cut}");
        assert!(!cut.contains("--"), "{cut}");
        // Exactly 63 is left alone.
        let exact = "d".repeat(63);
        assert_eq!(super::slug(&exact), exact);
        assert_eq!(id_of(&exact), exact);
    }

    fn id_of(name: &str) -> String {
        id(&git_facts(None, name, Some("main")))
    }

    #[test]
    fn the_hash_is_pinned() {
        // FNV-1a/64 folded to 32 bits: a change here renames committees.
        assert_eq!(short_hash(""), "4fd0bfc1");
    }

    #[test]
    fn a_long_repository_and_branch_together_still_fit() {
        let f = git_facts(
            None,
            &"r".repeat(40),
            Some(&format!("fix/{}", "x".repeat(40))),
        );
        let got = id(&f);
        assert_eq!(got.len(), 63);
        assert!(CommitteeId::new(got).is_ok());
    }

    #[test]
    fn nothing_usable_is_refused_not_invented() {
        let f = git_facts(None, "仓库", Some("main"));
        assert_eq!(derive(&f), Err(Cannot::Repository("仓库".to_string())));
        let f = Facts {
            directory: Some("日本語".to_string()),
            ..Facts::default()
        };
        assert_eq!(derive(&f), Err(Cannot::Repository("日本語".to_string())));
        let f = Facts::default();
        assert_eq!(derive(&f), Err(Cannot::Repository(String::new())));
        // A branch with nothing usable must not collapse onto the default.
        let f = git_facts(None, "repo", Some("特性"));
        assert_eq!(derive(&f), Err(Cannot::Branch("特性".to_string())));
    }

    #[test]
    fn an_explicit_name_is_untouched() {
        let given = CommitteeId::new("refund-v2").unwrap();
        let f = git_facts(None, "仓库", Some("x/y"));
        assert_eq!(
            choose(Some(given.clone()), &f),
            Ok(Choice::Given(given.clone()))
        );
        assert_eq!(
            choose(Some(given.clone()), &Facts::default()).unwrap().id(),
            &given
        );
        assert!(choose(None, &f).is_err());
    }
}
