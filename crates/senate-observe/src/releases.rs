//! Demand-driven Git metadata from roots recorded by the bridge. Only typed
//! release rows leave this module.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use senate_observe_protocol::ProjectRelease;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::grants::lock;

const WINDOW: usize = 24;
const CACHE_AGE: Duration = Duration::from_secs(60);
const MAX_ROOTS: usize = 256;

type Entries = BTreeMap<PathBuf, Arc<tokio::sync::Mutex<Cached>>>;

pub(crate) struct Releases {
    entries: Mutex<Entries>,
    slots: tokio::sync::Semaphore,
}

#[derive(Default)]
struct Cached {
    rows: Option<Vec<ProjectRelease>>,
    checked: Option<Instant>,
}

impl Default for Releases {
    fn default() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            slots: tokio::sync::Semaphore::new(4),
        }
    }
}

impl Releases {
    pub(crate) async fn get(&self, path: &Path) -> Option<Vec<ProjectRelease>> {
        let entry = {
            let mut entries = lock(&self.entries);
            if entries.len() >= MAX_ROOTS && !entries.contains_key(path) {
                entries.retain(|_, entry| {
                    entry.try_lock().map_or(true, |entry| {
                        entry.checked.is_some_and(|at| at.elapsed() < CACHE_AGE)
                    })
                });
            }
            if entries.len() >= MAX_ROOTS && !entries.contains_key(path) {
                return None;
            }
            entries.entry(path.to_owned()).or_default().clone()
        };
        let mut entry = entry.lock().await;
        if entry.checked.is_none_or(|at| at.elapsed() >= CACHE_AGE) {
            let _slot = self.slots.acquire().await.ok()?;
            let rows = tokio::time::timeout(Duration::from_secs(7), list(path))
                .await
                .ok()
                .flatten();
            if let Some(rows) = rows {
                entry.rows = Some(rows);
            }
            entry.checked = Some(Instant::now());
        }
        entry.rows.clone()
    }
}

struct Tag {
    commit: String,
    name: Option<String>,
    date: DateTime<Utc>,
}

async fn list(root: &Path) -> Option<Vec<ProjectRelease>> {
    let toplevel = git(root, &["rev-parse", "--show-toplevel"], 4096).await?;
    if Path::new(toplevel.trim_end())
        .canonicalize()
        .ok()?
        .as_path()
        != root
    {
        return None;
    }
    let head = default_head(root).await?;
    let reachable = format!("--merged={head}");
    let limit = format!("--count={}", WINDOW + 1);
    let output = git(root, &[
        "for-each-ref", &reachable, &limit, "--sort=refname", "--sort=-creatordate",
        "--format=%(objectname)%00%(*objectname)%00%(refname:strip=2)%00%(creatordate:iso-strict)",
        "refs/tags",
    ], 64 * 1024).await?;
    let tags: Vec<_> = output.lines().filter_map(tag).collect();
    let mut releases = Vec::new();
    for (index, tag) in tags.iter().take(WINDOW).enumerate() {
        let Some(name) = &tag.name else {
            continue;
        };
        let revision = match tags.get(index + 1) {
            Some(previous) => format!("{}..{}", previous.commit, tag.commit),
            None if output.lines().count() <= WINDOW => tag.commit.clone(),
            None => continue,
        };
        let merges = git(
            root,
            &[
                "log",
                "-z",
                "--no-notes",
                "--no-show-signature",
                "--first-parent",
                "--merges",
                "--format=%s",
                &revision,
                "--",
            ],
            512 * 1024,
        )
        .await?;
        let mut counts = (0u32, 0u32);
        for subject in merges.split('\0').filter(|subject| !subject.is_empty()) {
            counts.0 = counts.0.saturating_add(1);
            counts.1 = counts.1.saturating_add(u32::from(fix_merge(subject)));
        }
        releases.push(ProjectRelease {
            tag: name.clone(),
            at: tag.date,
            merges: counts.0,
            fixes: counts.1,
        });
    }
    Some(releases)
}

async fn default_head(root: &Path) -> Option<String> {
    for name in [
        "refs/remotes/origin/HEAD^{commit}",
        "refs/heads/main^{commit}",
        "refs/heads/master^{commit}",
    ] {
        if let Some(result) = git(root, &["rev-parse", "--verify", name], 256).await
            && object_id(result.trim())
        {
            return Some(result.trim().to_owned());
        }
    }
    None
}

fn tag(line: &str) -> Option<Tag> {
    let mut parts = line.split('\0');
    let object = parts.next()?;
    let peeled = parts.next()?;
    let commit = if peeled.is_empty() { object } else { peeled };
    let name = parts.next()?;
    let date = parts.next()?.parse().ok()?;
    if !object_id(commit) {
        return None;
    }
    let name = (!name.is_empty()
        && name.len() <= 80
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+')))
    .then(|| name.to_owned());
    Some(Tag {
        commit: commit.into(),
        name,
        date,
    })
}

fn object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn fix_merge(subject: &str) -> bool {
    subject.starts_with("Merge fix/")
        || subject.starts_with("Merge branch 'fix/")
        || (subject.starts_with("Merge pull request #")
            && subject.split_once(" from ").is_some_and(|(_, branch)| {
                branch
                    .split_once('/')
                    .is_some_and(|(_, name)| name.starts_with("fix/"))
            }))
}

async fn git(root: &Path, arguments: &[&str], limit: usize) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut child = Command::new("git")
            .arg("--no-pager")
            .args(["-c", "commit.gpgsign=false"])
            .arg("-C")
            .arg(root)
            .args(arguments)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut bytes = Vec::new();
        child
            .stdout
            .take()?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .await
            .ok()?;
        if bytes.len() > limit || !child.wait().await.ok()?.success() {
            return None;
        }
        String::from_utf8(bytes).ok()
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn git_projection_counts_first_parent_fixes_without_subjects()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        for arguments in [
            vec!["init", "-b", "main", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
            vec!["tag", "v1"],
            vec!["switch", "-c", "fix/one", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "private-subject-canary",
            ],
            vec!["switch", "main", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "merge",
                "--no-ff",
                "-qm",
                "Merge fix/one",
                "fix/one",
            ],
        ] {
            assert!(
                git(&root, &arguments, 4096).await.is_some(),
                "{arguments:?}"
            );
        }
        assert!(
            git(
                &root,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "tag",
                    "-a",
                    "v2",
                    "-m",
                    "private-tag-note-canary"
                ],
                4096
            )
            .await
            .is_some()
        );
        let rows = list(&root).await.ok_or("release projection failed")?;
        let release = rows
            .iter()
            .find(|row| row.tag == "v2")
            .ok_or("missing v2")?;
        assert_eq!((release.merges, release.fixes), (1, 1));
        let data = serde_json::to_string(&rows)?;
        assert!(!data.contains("canary"));
        assert!(!data.contains("fix/"));
        Ok(())
    }
}
