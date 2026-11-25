//! `.senate/` follows the branch.
//!
//! A worktree's `.senate/` directory holds what belongs to one line of
//! work: `committee`, optionally `agent`, and the checkpoint notes
//! `PROGRESS.md`, `DECISIONS.md` and `OPEN.md`. It is untracked, so a plain
//! `git switch` leaves it where it is, and the next branch would inherit
//! the old one's committee and notes, and overwrite them. So Senate keeps
//! one saved copy per (repository, branch) in the Senate home, and swaps
//! when the branch changes:
//!
//! ```text
//! $SENATE_HOME/branches/<repository id>/<slot id>/
//!     committee agent PROGRESS.md DECISIONS.md OPEN.md   (those that exist)
//!     slot.json   the repository's common git dir, the worktree, the branch,
//!                 the HEAD commit and the time at save
//! ```
//!
//! * The repository id is the real path of the common git dir
//!   (`git rev-parse --git-common-dir`, which every worktree shares) as a
//!   short readable name plus a hash of the path; the slot id is the branch
//!   name lowercased and cleaned plus a hash of the exact name (a
//!   case-insensitive file system keeps `Fix` and `fix` apart). The real
//!   path and the exact branch are in `slot.json`.
//! * A detached HEAD is a slot of its own *per worktree* (`@detached-<hash
//!   of the worktree>`), so a rebase or a bisect does not scatter notes, and
//!   when HEAD returns to a branch, that branch's slot comes back.
//! * Only the five files Senate defines are saved and restored. Any other
//!   file in `.senate/`, and `.gitignore`, stay where they are.
//!
//! Senate is not told about `git switch`, and installs no git hooks. The
//! worktree records which slot its `.senate/` currently belongs to in
//! `.senate/.branch` (untracked like the rest), and [`sync`] makes `.senate/`
//! match the checked-out branch. Every entry point that reads or writes
//! `.senate/` runs it first: the session hooks (start, prompt, end), the
//! daemon before a checkpoint reads the notes (the tool, and the triggers on
//! a new commit, on a timer and at session end), the MCP shim's pin lookup,
//! and the `senate` commands that resolve a committee, join, leave, pin,
//! unpin, where.
//!
//! # A swap, step by step
//!
//! 1. Save the files in `.senate/` into the slot the marker names, whole:
//!    written under a temporary name, then renamed over the old copy.
//! 2. Write the marker with the slot being restored as `pending`.
//! 3. Stage the new slot's files in `.senate/.stage`, remove the managed
//!    files, move the staged ones into place.
//! 4. Write the marker for the new slot.
//!
//! A crash anywhere leaves a state the next run completes: until step 2 the
//! worktree still holds everything and step 1 repeats; once the marker says
//! `pending`, the restore is redone from the slot and nothing is saved over
//! it. A lock on the repository (`branches/<repository id>/.lock`) keeps two
//! processes from swapping at once.
//!
//! # First run, and renames
//!
//! A `.senate/` with no marker is adopted as the current branch's: nothing
//! is saved over an existing slot. A worktree with no `.senate/` where the
//! current branch has a slot gets the slot's files back (a line of work
//! comes back with its branch, also in a new worktree); with no slot there
//! is nothing to do, and no `.senate/` is created just to hold a marker.
//!
//! A renamed branch (`git branch -m`) is not a switch: when the recorded
//! branch no longer exists, the checked-out branch has no slot, and either
//! HEAD's reflog says `Branch: renamed refs/heads/<old> to refs/heads/<new>`
//! or HEAD is still exactly the commit recorded at the last sync, the files
//! stay in place and the old slot, if any, is moved to the new name.
//!
//! Known limits: a brand-new worktree on a brand-new branch starts unbound
//! until `senate join`; moving a repository leaves its slots stale (their
//! recorded path is gone) until `senate setup clean`.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// The marker file in `.senate/`: which slot its contents belong to.
pub const MARKER: &str = ".branch";
/// The files Senate defines in `.senate/`: the ones that follow the branch.
pub const MANAGED: [&str; 5] = [
    "committee",
    "agent",
    "PROGRESS.md",
    "DECISIONS.md",
    "OPEN.md",
];
const STAGE: &str = ".stage";
const SLOT_FILE: &str = "slot.json";
const LOCK_WAIT: Duration = Duration::from_secs(3);

/// What a `.senate/` belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SlotKey {
    Branch {
        branch: String,
    },
    /// A detached HEAD, one slot per worktree.
    Detached,
}

impl SlotKey {
    pub fn branch(&self) -> Option<&str> {
        match self {
            Self::Branch { branch } => Some(branch),
            Self::Detached => None,
        }
    }
}

/// A checked-out worktree, as git says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    /// The worktree root (a real path).
    pub root: PathBuf,
    /// The repository identity: the common git dir (a real path).
    pub common_dir: PathBuf,
    /// The checked-out branch; `None` when detached.
    pub branch: Option<String>,
    /// HEAD's commit; `None` on an unborn branch.
    pub head: Option<String>,
}

impl Tree {
    pub fn key(&self) -> SlotKey {
        match &self.branch {
            Some(branch) => SlotKey::Branch {
                branch: branch.clone(),
            },
            None => SlotKey::Detached,
        }
    }

    pub fn senate_dir(&self) -> PathBuf {
        self.root.join(crate::session::SENATE_DIR)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Marker {
    key: SlotKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    head: Option<String>,
    /// A swap in progress: the slot being restored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<SlotKey>,
}

pub(crate) fn checkpoint_marker_matches(bytes: &[u8], branch: Option<&str>) -> bool {
    serde_json::from_slice::<Marker>(bytes)
        .is_ok_and(|marker| marker.pending.is_none() && marker.key.branch() == branch)
}

/// What `slot.json` says about a saved copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotMeta {
    /// The repository's common git dir.
    pub repository: PathBuf,
    /// The worktree it was saved from.
    pub worktree: PathBuf,
    pub key: SlotKey,
    #[serde(default)]
    pub head: Option<String>,
    pub saved_at: DateTime<Utc>,
}

/// What [`sync`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing to keep in step: no `.senate/` and no saved copy.
    Nothing,
    /// `.senate/` already belongs to the checked-out branch.
    InStep,
    /// A `.senate/` with no marker, taken as the current branch's.
    Adopted,
    /// A worktree with no `.senate/` got the branch's saved copy.
    Restored,
    /// The branch changed: the old files were saved and the new ones
    /// restored (or, with no saved copy, removed).
    Swapped { from: SlotKey, to: SlotKey },
    /// The branch was renamed: the files stayed.
    Renamed { from: String, to: String },
    /// Another process holds the repository's lock; nothing was changed.
    Busy,
}

/// Where in a swap a test makes the process "crash".
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    AfterSave,
    AfterPending,
    MidRestore,
}

/// The real path of `p`, or `p` itself when it cannot be resolved.
pub fn real(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// `s` lowercased, cut to `max` characters, with anything but letters,
/// digits, `.`, `_` and `-` as `_`, and never starting with a dot.
fn slug(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .take(max)
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.starts_with('.') {
        out.insert(0, '_');
    }
    out
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A name for a repository that is stable, safe on every file system, and
/// readable.
pub fn repo_id(common_dir: &Path) -> String {
    let name = if common_dir.file_name().is_some_and(|n| n == ".git") {
        common_dir.parent().and_then(Path::file_name)
    } else {
        common_dir.file_name()
    };
    format!(
        "{}-{:016x}",
        slug(&name.unwrap_or_default().to_string_lossy(), 24),
        fnv(common_dir.to_string_lossy().as_bytes())
    )
}

/// A name for a slot.
pub fn slot_id(key: &SlotKey, root: &Path) -> String {
    match key {
        SlotKey::Branch { branch } => {
            format!("{}-{:016x}", slug(branch, 40), fnv(branch.as_bytes()))
        }
        SlotKey::Detached => format!("@detached-{:016x}", fnv(root.to_string_lossy().as_bytes())),
    }
}

/// Where a repository keeps its slots.
pub fn repo_dir(paths: &Paths, common_dir: &Path) -> PathBuf {
    paths.branches().join(repo_id(common_dir))
}

/// Where `key`'s copy is kept.
pub fn slot_dir(paths: &Paths, common_dir: &Path, key: &SlotKey, root: &Path) -> PathBuf {
    repo_dir(paths, common_dir).join(slot_id(key, root))
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// What git says about `cwd`'s worktree (read-only); `None` outside one.
pub fn tree_of(cwd: &Path) -> Option<Tree> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let common = git(cwd, &["rev-parse", "--git-common-dir"])?;
    Some(Tree {
        root: real(Path::new(&root)),
        common_dir: real(&cwd.join(common)),
        branch: git(cwd, &["symbolic-ref", "--short", "-q", "HEAD"]),
        head: git(cwd, &["rev-parse", "-q", "--verify", "HEAD"]),
    })
}

fn branch_exists(tree: &Tree, branch: &str) -> bool {
    git(
        &tree.root,
        &[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_some()
}

/// The repository's branch `branch`, read-only, from its common git dir:
/// `Some(false)` only when git answered that it is not there.
pub fn branch_in_repository(common_dir: &Path, branch: &str) -> Option<bool> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(common_dir)
        .args(["rev-parse", "-q", "--verify"])
        .arg(format!("refs/heads/{branch}"))
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

// ---- the lock --------------------------------------------------------------

/// The repository's lock, held while a swap, or a write that must not be
/// swapped from under, is under way.
pub struct Lock {
    _file: File,
}

#[allow(unsafe_code)]
fn try_flock(file: &File) -> bool {
    use std::os::fd::AsRawFd;
    // SAFETY: flock(2) on a file descriptor we own, for the life of `file`.
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

/// Take the lock of `tree`'s repository, waiting a few seconds; `None` when
/// another process still holds it.
pub fn acquire(paths: &Paths, tree: &Tree) -> io::Result<Option<Lock>> {
    let dir = repo_dir(paths, &tree.common_dir);
    std::fs::create_dir_all(&dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".lock"))?;
    let started = Instant::now();
    loop {
        if try_flock(&file) {
            return Ok(Some(Lock { _file: file }));
        }
        if started.elapsed() > LOCK_WAIT {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

// ---- small file helpers ----------------------------------------------------

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let mut file = File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

fn read_marker(senate: &Path) -> Option<Marker> {
    serde_json::from_slice(&std::fs::read(senate.join(MARKER)).ok()?).ok()
}

fn write_marker(senate: &Path, marker: &Marker) -> io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(marker).map_err(io::Error::other)?;
    bytes.push(b'\n');
    atomic_write(&senate.join(MARKER), &bytes)
}

fn present(dir: &Path) -> Vec<&'static str> {
    MANAGED
        .into_iter()
        .filter(|name| dir.join(name).is_file())
        .collect()
}

fn remove_dir(dir: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Finish or undo what a crash left in a repository's slot directory:
/// half-written copies are dropped, a copy that was being replaced is put
/// back when its replacement never arrived.
fn recover(repo: &Path) -> io::Result<()> {
    let Ok(entries) = std::fs::read_dir(repo) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(".tmp-") {
            remove_dir(&entry.path())?;
        } else if let Some((base, _)) = name.split_once(".old-") {
            let slot = repo.join(base);
            if slot.exists() {
                remove_dir(&entry.path())?;
            } else {
                std::fs::rename(entry.path(), slot)?;
            }
        }
    }
    Ok(())
}

/// Replace the slot `key` with the managed files now in `senate`.
fn save(
    paths: &Paths,
    tree: &Tree,
    key: &SlotKey,
    senate: &Path,
    head: Option<&str>,
) -> io::Result<()> {
    let dir = slot_dir(paths, &tree.common_dir, key, &tree.root);
    let parent = dir.parent().unwrap_or(&dir).to_path_buf();
    std::fs::create_dir_all(&parent)?;
    let id = slot_id(key, &tree.root);
    let tmp = parent.join(format!("{id}.tmp-{}", std::process::id()));
    remove_dir(&tmp)?;
    let files = present(senate);
    if !files.is_empty() {
        std::fs::create_dir(&tmp)?;
        for name in &files {
            std::fs::copy(senate.join(name), tmp.join(name))?;
        }
        let meta = SlotMeta {
            repository: tree.common_dir.clone(),
            worktree: tree.root.clone(),
            key: key.clone(),
            head: head.map(str::to_string),
            saved_at: Utc::now(),
        };
        let bytes = serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?;
        std::fs::write(tmp.join(SLOT_FILE), bytes)?;
    }
    if dir.exists() {
        let old = parent.join(format!("{id}.old-{}", std::process::id()));
        remove_dir(&old)?;
        std::fs::rename(&dir, &old)?;
        if !files.is_empty() {
            std::fs::rename(&tmp, &dir)?;
        }
        remove_dir(&old)?;
    } else if !files.is_empty() {
        std::fs::rename(&tmp, &dir)?;
    }
    Ok(())
}

/// Make `senate`'s managed files those of `slot` (none when it is absent).
fn restore(slot: &Path, senate: &Path, stop: Option<Stop>) -> io::Result<()> {
    let stage = senate.join(STAGE);
    remove_dir(&stage)?;
    let files = present(slot);
    if !files.is_empty() {
        std::fs::create_dir(&stage)?;
        for name in &files {
            std::fs::copy(slot.join(name), stage.join(name))?;
        }
    }
    for name in present(senate) {
        std::fs::remove_file(senate.join(name))?;
    }
    if stop == Some(Stop::MidRestore) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "simulated crash",
        ));
    }
    for name in files {
        std::fs::rename(stage.join(name), senate.join(name))?;
    }
    remove_dir(&stage)
}

fn read_meta(slot: &Path) -> Option<SlotMeta> {
    serde_json::from_slice(&std::fs::read(slot.join(SLOT_FILE)).ok()?).ok()
}

/// Whether `from` was renamed to `to` rather than left for it.
fn renamed(tree: &Tree, from: &str, to: &str, recorded_head: Option<&str>) -> bool {
    if branch_exists(tree, from) {
        return false;
    }
    let said = git(
        &tree.root,
        &["reflog", "show", "--format=%gs", "-n", "100", "HEAD"],
    )
    .is_some_and(|log| {
        let want = format!("Branch: renamed refs/heads/{from} to refs/heads/{to}");
        log.lines().any(|l| l.trim() == want)
    });
    said || (recorded_head.is_some() && recorded_head == tree.head.as_deref())
}

fn ensure_ignore(senate: &Path) -> io::Result<()> {
    let ignore = senate.join(".gitignore");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&ignore)
    {
        Ok(mut f) => f.write_all(b"*\n"),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Make `.senate/` match the checked-out branch.
pub fn sync(paths: &Paths, tree: &Tree) -> io::Result<Outcome> {
    let Some(lock) = acquire(paths, tree)? else {
        return Ok(Outcome::Busy);
    };
    sync_locked(paths, tree, &lock, None)
}

/// [`sync`] with the lock already held, and, for tests, a simulated crash.
#[doc(hidden)]
pub fn sync_locked(
    paths: &Paths,
    tree: &Tree,
    _lock: &Lock,
    stop: Option<Stop>,
) -> io::Result<Outcome> {
    let senate = tree.senate_dir();
    let current = tree.key();
    recover(&repo_dir(paths, &tree.common_dir))?;
    if !senate.is_dir() {
        let slot = slot_dir(paths, &tree.common_dir, &current, &tree.root);
        if present(&slot).is_empty() {
            return Ok(Outcome::Nothing);
        }
        std::fs::create_dir_all(&senate)?;
        ensure_ignore(&senate)?;
        restore(&slot, &senate, stop)?;
        write_marker(
            &senate,
            &Marker {
                key: current,
                head: tree.head.clone(),
                pending: None,
            },
        )?;
        return Ok(Outcome::Restored);
    }
    ensure_ignore(&senate)?;
    let Some(mut marker) = read_marker(&senate) else {
        write_marker(
            &senate,
            &Marker {
                key: current,
                head: tree.head.clone(),
                pending: None,
            },
        )?;
        return Ok(Outcome::Adopted);
    };
    // A swap a crash cut short: finish its restore, save nothing over it.
    if let Some(pending) = marker.pending.clone() {
        let slot = slot_dir(paths, &tree.common_dir, &pending, &tree.root);
        restore(&slot, &senate, stop)?;
        marker = Marker {
            key: pending,
            head: tree.head.clone(),
            pending: None,
        };
        write_marker(&senate, &marker)?;
    }
    if marker.key == current {
        if marker.head != tree.head {
            marker.head.clone_from(&tree.head);
            write_marker(&senate, &marker)?;
        }
        return Ok(Outcome::InStep);
    }
    let slot_of = |key: &SlotKey| slot_dir(paths, &tree.common_dir, key, &tree.root);
    if let (SlotKey::Branch { branch: from }, SlotKey::Branch { branch: to }) =
        (&marker.key, &current)
        && present(&slot_of(&current)).is_empty()
        && renamed(tree, from, to, marker.head.as_deref())
    {
        let (old, new) = (slot_of(&marker.key), slot_of(&current));
        if old.is_dir() {
            std::fs::rename(&old, &new)?;
            if let Some(mut meta) = read_meta(&new) {
                meta.key = current.clone();
                std::fs::write(
                    new.join(SLOT_FILE),
                    serde_json::to_vec_pretty(&meta).map_err(io::Error::other)?,
                )?;
            }
        }
        write_marker(
            &senate,
            &Marker {
                key: current.clone(),
                head: tree.head.clone(),
                pending: None,
            },
        )?;
        return Ok(Outcome::Renamed {
            from: from.clone(),
            to: to.clone(),
        });
    }
    save(paths, tree, &marker.key, &senate, marker.head.as_deref())?;
    if stop == Some(Stop::AfterSave) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "simulated crash",
        ));
    }
    write_marker(
        &senate,
        &Marker {
            key: marker.key.clone(),
            head: marker.head.clone(),
            pending: Some(current.clone()),
        },
    )?;
    if stop == Some(Stop::AfterPending) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "simulated crash",
        ));
    }
    restore(&slot_of(&current), &senate, stop)?;
    write_marker(
        &senate,
        &Marker {
            key: current.clone(),
            head: tree.head.clone(),
            pending: None,
        },
    )?;
    Ok(Outcome::Swapped {
        from: marker.key,
        to: current,
    })
}

/// Everything a writer of `.senate/` needs: the lock, the directory made
/// (with its `.gitignore`) and in step with the branch, and a marker.
/// Hold the returned lock while writing.
pub fn prepare(paths: &Paths, tree: &Tree) -> io::Result<(Lock, PathBuf)> {
    let lock = acquire(paths, tree)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            "another Senate process is changing this repository's .senate/ copies; try again",
        )
    })?;
    sync_locked(paths, tree, &lock, None)?;
    let senate = tree.senate_dir();
    std::fs::create_dir_all(&senate)?;
    ensure_ignore(&senate)?;
    if read_marker(&senate).is_none() {
        write_marker(
            &senate,
            &Marker {
                key: tree.key(),
                head: tree.head.clone(),
                pending: None,
            },
        )?;
    }
    Ok((lock, senate))
}

/// The slot the worktree's `.senate/` belongs to now (the marker's), if
/// there is a marker.
pub fn belongs_to(tree: &Tree) -> Option<SlotKey> {
    read_marker(&tree.senate_dir()).map(|m| m.key)
}

// ---- looking at the saved copies ------------------------------------------

/// One saved copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub dir: PathBuf,
    pub meta: SlotMeta,
    /// The first line of the copy's `committee` file that names one.
    pub committee: Option<String>,
}

/// Every slot under `paths`, by repository then branch.
pub fn slots(paths: &Paths) -> Vec<Slot> {
    let mut out = Vec::new();
    let Ok(repos) = std::fs::read_dir(paths.branches()) else {
        return out;
    };
    for repo in repos.flatten().filter(|e| e.path().is_dir()) {
        let Ok(entries) = std::fs::read_dir(repo.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let Some(meta) = read_meta(&dir) else {
                continue;
            };
            let committee = std::fs::read_to_string(dir.join("committee"))
                .ok()
                .and_then(|t| {
                    t.lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty() && !l.starts_with('#'))
                        .map(str::to_string)
                });
            out.push(Slot {
                dir,
                meta,
                committee,
            });
        }
    }
    out.sort_by(|a, b| {
        (&a.meta.repository, a.meta.key.branch(), &a.meta.worktree).cmp(&(
            &b.meta.repository,
            b.meta.key.branch(),
            &b.meta.worktree,
        ))
    });
    out
}

/// Why a slot is no longer wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stale {
    RepositoryGone,
    BranchGone,
    WorktreeGone,
}

/// Whether a slot has expired (read-only). A slot for a branch that still
/// exists is never stale, however old. Where git cannot answer, it is not.
pub fn stale(meta: &SlotMeta) -> Option<Stale> {
    if !meta.repository.exists() {
        return Some(Stale::RepositoryGone);
    }
    match &meta.key {
        SlotKey::Branch { branch } => (branch_in_repository(&meta.repository, branch)
            == Some(false))
        .then_some(Stale::BranchGone),
        SlotKey::Detached => (!meta.worktree.exists()).then_some(Stale::WorktreeGone),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_safe_and_tell_case_apart() {
        let repo = repo_id(Path::new("/home/a/My Repo/.git"));
        assert!(repo.starts_with("my_repo-"), "{repo}");
        assert_eq!(repo, repo_id(Path::new("/home/a/My Repo/.git")));
        assert_ne!(repo, repo_id(Path::new("/home/b/My Repo/.git")));
        let key = |b: &str| SlotKey::Branch { branch: b.into() };
        let root = Path::new("/w");
        let a = slot_id(&key("Fix/Thing"), root);
        let b = slot_id(&key("fix/thing"), root);
        assert!(a.starts_with("fix_thing-") && b.starts_with("fix_thing-"));
        assert_ne!(a, b, "a case-insensitive file system must not merge them");
        assert!(!a.contains('/') && !slot_id(&key("../.."), root).starts_with('.'));
        assert!(slot_id(&SlotKey::Detached, root).starts_with("@detached-"));
        assert_ne!(
            slot_id(&SlotKey::Detached, root),
            slot_id(&SlotKey::Detached, Path::new("/other"))
        );
        let long = slot_id(&key(&"x".repeat(500)), root);
        assert!(long.len() < 100);
    }
}
