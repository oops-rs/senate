//! What `git` says about a directory, read from the repository's own
//! files where that is certain.
//!
//! Every hook of every coding session asks four things of the directory it
//! runs in: the worktree's root, the common git directory, the branch and
//! the commit `HEAD` names. Four `git` processes answered them, 25 ms each
//! on a busy machine and most of what a hook cost. For an ordinary
//! repository the answers are in a handful of small files: `.git` (a
//! directory, or a file naming one for a linked worktree), `commondir`,
//! `HEAD`, the branch's loose ref and `packed-refs`.
//!
//! [`read`] answers only what it can be sure `git` would say, and says
//! [`Answer::Uncertain`] for everything else, which the caller then asks
//! `git`: an environment that moves the repository (`GIT_DIR` and its
//! kin), a bare repository or a directory inside `.git`, a submodule or
//! anything else with `core.worktree`, configuration that is included from
//! elsewhere or names an extension, the reftable format, a repository
//! another user owns (`git` refuses those without `safe.directory`), a
//! symbolic ref outside `refs/heads/`, and a branch whose short name `git`
//! would qualify because a tag, a remote or another ref shares it.

use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// What the repository's files say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Repository {
    /// `git rev-parse --show-toplevel`, canonical.
    pub toplevel: PathBuf,
    /// `git rev-parse --git-common-dir`, canonical.
    pub common_dir: PathBuf,
    /// `git symbolic-ref --short -q HEAD`.
    pub branch: Option<String>,
    /// `git rev-parse -q --verify HEAD`.
    pub head: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Answer {
    Repository(Repository),
    /// No repository at the directory or above it, up to the edge of its
    /// filesystem, where `git` stops looking too.
    NotARepository,
    /// Ask `git`.
    Uncertain,
}

/// Variables of `git`'s that change nothing read here.
const PLAIN: [&str; 8] = [
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_PAGER",
    "GIT_TERMINAL_PROMPT",
    "GIT_ASKPASS",
    "GIT_OPTIONAL_LOCKS",
    "GIT_SSH",
    "GIT_SSH_COMMAND",
];
const PLAIN_PREFIXES: [&str; 2] = ["GIT_AUTHOR_", "GIT_COMMITTER_"];

fn plain_environment() -> bool {
    std::env::vars_os().all(|(key, _)| {
        let key = key.to_string_lossy();
        !key.starts_with("GIT_")
            || PLAIN.contains(&key.as_ref())
            || PLAIN_PREFIXES.iter().any(|p| key.starts_with(p))
    })
}

/// A full object name: SHA-1 or SHA-256, lowercase.
fn object_name(text: &str) -> Option<&str> {
    (matches!(text.len(), 40 | 64)
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(text)
}

/// A branch name that is safe to join to a path and that `git` would have
/// written itself.
fn plain_ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(['/', '-', '.'])
        && !name.ends_with(['/', '.'])
        && !name.contains("//")
        && !name.contains("..")
        && !name.contains("/.")
        && !name.contains("@{")
        && !name.ends_with(".lock")
        && name
            .chars()
            .all(|c| !c.is_control() && !" ~^:?*[\\".contains(c))
}

/// Whether the repository's configuration leaves everything read here
/// where it usually is: nothing included from another file, no extension,
/// no `core.worktree`, not bare.
fn plain_config(config: &str) -> bool {
    let mut section = String::new();
    for line in config.lines().map(str::trim) {
        if line.ends_with('\\') {
            return false;
        }
        if let Some(header) = line.strip_prefix('[') {
            section = header
                .split([' ', ']', '"'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if matches!(section.as_str(), "include" | "includeif" | "extensions") {
                return false;
            }
            continue;
        }
        if section != "core" {
            continue;
        }
        let (key, value) = line.split_once('=').unwrap_or((line, "true"));
        match key.trim().to_ascii_lowercase().as_str() {
            "worktree" => return false,
            "bare" if !value.trim().to_ascii_lowercase().starts_with("false") => return false,
            _ => {}
        }
    }
    true
}

fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// The directory's answer, or that `git` must be asked.
pub(crate) fn read(cwd: &Path) -> Answer {
    read_at(cwd).unwrap_or(Answer::Uncertain)
}

fn read_at(cwd: &Path) -> Option<Answer> {
    if !plain_environment() || !cwd.is_absolute() {
        return None;
    }
    let cwd = std::fs::canonicalize(cwd).ok()?;
    let start = std::fs::metadata(&cwd).ok()?;
    if !start.is_dir() {
        return None;
    }
    for dir in cwd.ancestors() {
        // `git` does not look across a mount point.
        if std::fs::metadata(dir).ok()?.dev() != start.dev() {
            return Some(Answer::NotARepository);
        }
        let dot = dir.join(".git");
        match std::fs::metadata(&dot) {
            Ok(meta) if meta.is_dir() => return repository(dir, &dot, None),
            Ok(meta) if meta.is_file() => {
                let text = std::fs::read_to_string(&dot).ok()?;
                let named = text
                    .strip_prefix("gitdir: ")?
                    .trim_end_matches(['\n', '\r']);
                return repository(dir, &dir.join(named), Some(&dot));
            }
            Ok(_) => return None,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // A dangling `.git` link is not `git`'s to skip silently.
                if exists(&dot) {
                    return None;
                }
            }
            Err(_) => return None,
        }
        // A bare repository, or somewhere inside a git directory.
        if dir.join("HEAD").is_file()
            && (exists(&dir.join("commondir"))
                || (exists(&dir.join("objects")) && exists(&dir.join("refs"))))
        {
            return None;
        }
    }
    Some(Answer::NotARepository)
}

fn repository(worktree: &Path, git_dir: &Path, git_file: Option<&Path>) -> Option<Answer> {
    let git_dir = std::fs::canonicalize(git_dir).ok()?;
    let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(text) => {
            std::fs::canonicalize(git_dir.join(text.trim_end_matches(['\n', '\r']))).ok()?
        }
        Err(error) if error.kind() == ErrorKind::NotFound => git_dir.clone(),
        Err(_) => return None,
    };
    // What `git` itself requires of a git directory.
    if !git_dir.join("HEAD").is_file()
        || !common_dir.join("objects").is_dir()
        || !common_dir.join("refs").is_dir()
    {
        return None;
    }
    // `git` refuses a repository someone else owns unless told it is safe.
    let me = rustix::process::geteuid().as_raw();
    for path in [Some(worktree), Some(git_dir.as_path()), git_file]
        .into_iter()
        .flatten()
    {
        if std::fs::metadata(path).ok()?.uid() != me {
            return None;
        }
    }
    if exists(&common_dir.join("reftable")) {
        return None;
    }
    if !plain_config(&std::fs::read_to_string(common_dir.join("config")).ok()?) {
        return None;
    }

    let head_text = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head_text = head_text.trim_end_matches(['\n', '\r']);
    let (branch, head) = match head_text.strip_prefix("ref: ") {
        None => (None, Some(object_name(head_text)?.to_owned())),
        Some(target) => {
            let name = target.strip_prefix("refs/heads/")?;
            if !plain_ref_name(name) {
                return None;
            }
            let packed = match std::fs::read_to_string(common_dir.join("packed-refs")) {
                Ok(text) => text,
                Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
                Err(_) => return None,
            };
            let packed_ref = |full: &str| {
                packed.lines().find_map(|line| {
                    let (object, name) = line.split_once(' ')?;
                    (name == full).then_some(object)
                })
            };
            // `--short` qualifies a name any other ref shares.
            if exists(&git_dir.join(name)) || exists(&common_dir.join(name)) {
                return None;
            }
            for shared in [
                format!("refs/{name}"),
                format!("refs/tags/{name}"),
                format!("refs/remotes/{name}"),
                format!("refs/remotes/{name}/HEAD"),
            ] {
                if exists(&common_dir.join(&shared))
                    || exists(&git_dir.join(&shared))
                    || packed_ref(&shared).is_some()
                {
                    return None;
                }
            }
            let full = format!("refs/heads/{name}");
            let head = match std::fs::read_to_string(common_dir.join(&full)) {
                Ok(text) => Some(object_name(text.trim_end_matches(['\n', '\r']))?.to_owned()),
                Err(error) if error.kind() == ErrorKind::NotFound => match packed_ref(&full) {
                    Some(object) => Some(object_name(object)?.to_owned()),
                    // A branch with no commit yet.
                    None => None,
                },
                Err(_) => return None,
            };
            (Some(name.to_owned()), head)
        }
    };
    Some(Answer::Repository(Repository {
        toplevel: worktree.to_path_buf(),
        common_dir,
        branch,
        head,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) -> Option<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        (output.status.success() && !text.trim().is_empty()).then(|| text.trim().to_owned())
    }

    /// What the four `git` calls say, as [`crate::session`] reads them.
    fn by_git(cwd: &Path) -> Answer {
        let toplevel = git(cwd, &["rev-parse", "--show-toplevel"]);
        let common = git(cwd, &["rev-parse", "--git-common-dir"]);
        match (toplevel, common) {
            (Some(toplevel), Some(common)) => Answer::Repository(Repository {
                toplevel: std::fs::canonicalize(toplevel).unwrap(),
                common_dir: std::fs::canonicalize(cwd.join(common)).unwrap(),
                branch: git(cwd, &["symbolic-ref", "--short", "-q", "HEAD"]),
                head: git(cwd, &["rev-parse", "-q", "--verify", "HEAD"]),
            }),
            (None, None) => Answer::NotARepository,
            _ => Answer::Uncertain,
        }
    }

    /// The files' answer is `git`'s, wherever the files give one.
    fn agrees(cwd: &Path) -> Answer {
        let read = read(cwd);
        if read != Answer::Uncertain {
            assert_eq!(read, by_git(cwd), "{}", cwd.display());
        }
        read
    }

    fn answered(cwd: &Path) -> Repository {
        match agrees(cwd) {
            Answer::Repository(repository) => repository,
            other => panic!("{}: {other:?}", cwd.display()),
        }
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        (dir, root)
    }

    fn commit(root: &Path, file: &str) {
        std::fs::write(root.join(file), file).unwrap();
        git(root, &["add", file]);
        git(root, &["commit", "-q", "-m", file]);
    }

    #[test]
    fn an_ordinary_repository_is_read_as_git_reads_it() {
        if !plain_environment() {
            return;
        }
        let (_dir, root) = repo();
        // No commit yet: a branch, and nothing it names.
        let unborn = answered(&root);
        assert_eq!(unborn.branch.as_deref(), Some("main"));
        assert_eq!(unborn.head, None);

        commit(&root, "a");
        let first = answered(&root);
        assert_eq!(first.branch.as_deref(), Some("main"));
        assert!(first.head.is_some());

        // From a directory below the root.
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        assert_eq!(answered(&root.join("src/deep")), first);

        // A branch with a slash, then its ref packed away.
        git(&root, &["checkout", "-q", "-b", "feat/hook-speed"]);
        commit(&root, "b");
        let branch = answered(&root);
        assert_eq!(branch.branch.as_deref(), Some("feat/hook-speed"));
        assert_ne!(branch.head, first.head);
        git(&root, &["pack-refs", "--all"]);
        assert_eq!(answered(&root), branch);
        commit(&root, "c");
        assert_ne!(answered(&root).head, branch.head);

        // Detached.
        git(&root, &["checkout", "-q", "--detach"]);
        let detached = answered(&root);
        assert_eq!(detached.branch, None);
        assert!(detached.head.is_some());
    }

    #[test]
    fn a_linked_worktree_names_its_own_root_and_the_shared_directory() {
        if !plain_environment() {
            return;
        }
        let (dir, root) = repo();
        commit(&root, "a");
        let linked = dir.path().join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "side",
                linked.to_str().unwrap(),
            ],
        );
        let side = answered(&linked);
        assert_eq!(side.branch.as_deref(), Some("side"));
        assert_eq!(side.toplevel, std::fs::canonicalize(&linked).unwrap());
        assert_eq!(side.common_dir, answered(&root).common_dir);
        git(&linked, &["checkout", "-q", "--detach"]);
        assert_eq!(answered(&linked).branch, None);
    }

    #[test]
    fn what_the_files_cannot_settle_is_left_to_git() {
        if !plain_environment() {
            return;
        }
        let (dir, root) = repo();
        commit(&root, "a");

        // A tag of the branch's name: `git` prints `heads/main`.
        git(&root, &["tag", "main"]);
        assert_eq!(read(&root), Answer::Uncertain);
        assert_eq!(
            git(&root, &["symbolic-ref", "--short", "-q", "HEAD"]).as_deref(),
            Some("heads/main")
        );
        git(&root, &["tag", "-d", "main"]);
        assert_ne!(agrees(&root), Answer::Uncertain);

        // The same tag, packed.
        git(&root, &["tag", "main"]);
        git(&root, &["pack-refs", "--all"]);
        assert_eq!(read(&root), Answer::Uncertain);
        git(&root, &["tag", "-d", "main"]);

        // Inside the git directory, and in a bare repository.
        assert_eq!(read(&root.join(".git")), Answer::Uncertain);
        assert_eq!(read(&root.join(".git/refs")), Answer::Uncertain);
        let bare = dir.path().join("bare.git");
        git(
            dir.path(),
            &["init", "-q", "--bare", bare.to_str().unwrap()],
        );
        assert_eq!(read(&bare), Answer::Uncertain);

        // A worktree set by configuration.
        git(&root, &["config", "core.worktree", root.to_str().unwrap()]);
        assert_eq!(read(&root), Answer::Uncertain);
        git(&root, &["config", "--unset", "core.worktree"]);
        assert_ne!(agrees(&root), Answer::Uncertain);

        // A relative path, and one that is not there.
        assert_eq!(read(Path::new("relative")), Answer::Uncertain);
        assert_eq!(read(&dir.path().join("missing")), Answer::Uncertain);
    }

    #[test]
    fn a_directory_in_no_repository_agrees_with_git() {
        if !plain_environment() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        // Whatever is above a temp directory on this machine, the files
        // and `git` say the same of it.
        assert_ne!(agrees(&plain), Answer::Uncertain);
    }

    /// A real checkout, with whatever history, worktrees and packed refs
    /// it has: this crate's own.
    #[test]
    fn this_checkout_is_read_as_git_reads_it() {
        agrees(Path::new(env!("CARGO_MANIFEST_DIR")));
    }

    #[test]
    fn names_are_checked_before_they_become_paths() {
        assert!(plain_ref_name("main"));
        assert!(plain_ref_name("feat/hook-speed"));
        for bad in [
            "", "/x", "x/", "a//b", "a/../b", "..", ".hidden", "a b", "x.lock", "a@{1}",
        ] {
            assert!(!plain_ref_name(bad), "{bad:?}");
        }
        assert!(object_name(&"a".repeat(40)).is_some());
        assert!(object_name(&"A".repeat(40)).is_none());
        assert!(object_name("abc").is_none());
    }

    #[test]
    fn configuration_that_moves_things_is_not_plain() {
        let usual = "[core]\n\trepositoryformatversion = 0\n\tbare = false\n\
                     [branch \"fix/worktree-include\"]\n\tremote = origin\n";
        assert!(plain_config(usual));
        for moved in [
            "[core]\n\tworktree = /elsewhere\n",
            "[core]\n\tbare = true\n",
            "[core]\n\tbare\n",
            "[include]\n\tpath = other\n",
            "[includeIf \"gitdir:~/work/\"]\n\tpath = other\n",
            "[extensions]\n\trefStorage = reftable\n",
            "[core]\n\teditor = a \\\n\tb\n",
        ] {
            assert!(!plain_config(moved), "{moved:?}");
        }
    }
}
