//! A hosted session in a git worktree whose common `.git` directory is
//! outside the worktree: the runtime's sandbox lets it write only inside
//! its working directory, so every commit, branch or fetch writes outside
//! it and asks a person first. The fix is the runtime's own setting that
//! makes that directory writable too.
//!
//! [`find`] asks git; [`checks`] judges, given Codex's settings as text.
//! Only Codex in `workspace-write` (its default for a trusted project) is a
//! problem. Claude Code's sandbox already lets a linked worktree write the
//! main repository's shared `.git` (all but its `hooks/` and `config`), so
//! a Claude session never needs a setting; agy has none to name.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use senate_core::Locale;

use super::Check;
use crate::ipc::SessionRow;
use crate::session::Runtime;

/// A running hosted session whose worktree keeps its git data elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outside {
    pub label: String,
    pub runtime: Runtime,
    pub dir: PathBuf,
    /// `git rev-parse --git-common-dir`, absolute.
    pub common: PathBuf,
}

/// The settings that decide whether a path is writable.
#[derive(Debug, Clone, Default)]
pub struct Sandboxes {
    /// Codex's `config.toml`, path and text.
    pub codex: Option<(PathBuf, String)>,
}

impl Sandboxes {
    /// Read the file `setup install` would edit.
    pub fn read(codex: Option<&Path>) -> Self {
        Self {
            codex: codex.and_then(|p| Some((p.to_path_buf(), std::fs::read_to_string(p).ok()?))),
        }
    }
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The common git directory of `dir`, when `dir` is in a git repository.
fn common_dir(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    let path = PathBuf::from(text.trim());
    path.is_absolute().then(|| canonical(&path))
}

/// The running hosted sessions whose common git directory is outside
/// their directory.
pub fn find(rows: &[SessionRow]) -> Vec<Outside> {
    rows.iter()
        .filter(|row| row.is_running())
        .filter_map(|row| {
            let dir = canonical(&row.dir);
            let common = common_dir(&dir)?;
            (!common.starts_with(&dir)).then(|| Outside {
                label: row.label(),
                runtime: row.runtime,
                dir,
                common,
            })
        })
        .collect()
}

/// Whether Codex confines this session's writes, and the roots it lets
/// it write besides its directory.
fn codex_roots(config: &str) -> Option<Vec<PathBuf>> {
    let doc: toml::Table = toml::from_str(config).ok()?;
    let mode = doc.get("sandbox_mode").and_then(toml::Value::as_str);
    if matches!(mode, Some("danger-full-access")) {
        return None;
    }
    Some(
        doc.get("sandbox_workspace_write")
            .and_then(|t| t.get("writable_roots"))
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(toml::Value::as_str)
            .map(|p| canonical(Path::new(p)))
            .collect(),
    )
}

/// One warning per session whose git directory its sandbox cannot write.
pub fn checks(found: &[Outside], sandboxes: &Sandboxes, l: Locale) -> Vec<Check> {
    let mut checks = Vec::new();
    for outside in found {
        let common = outside.common.display().to_string();
        let quoted = serde_json::json!(common).to_string();
        let (roots, fix) = match outside.runtime {
            Runtime::Codex => {
                let Some((file, text)) = &sandboxes.codex else {
                    continue;
                };
                let fix = l.pick(
                    format!(
                        "在 {} 中设置 [sandbox_workspace_write] writable_roots = [{quoted}]（已有的根保留），然后 `codex app-server daemon restart`",
                        file.display()
                    ),
                    format!(
                        "in {}, set [sandbox_workspace_write] writable_roots = [{quoted}] (keeping any roots already there), then `codex app-server daemon restart`",
                        file.display()
                    ),
                );
                (codex_roots(text), fix)
            }
            // See the module documentation.
            Runtime::Claude | Runtime::Agy | Runtime::Acp => continue,
        };
        let Some(roots) = roots else {
            continue;
        };
        if roots.iter().any(|root| outside.common.starts_with(root)) {
            continue;
        }
        let label = &outside.label;
        checks.push(Check::warn(
            format!("sessions.git.{label}"),
            l.pick(
                format!(
                    "{label} 的工作树 {} 把 git 数据放在 {common}，在沙箱可写范围之外：每次 git 写入都要人批准",
                    outside.dir.display()
                ),
                format!(
                    "{label}'s worktree {} keeps its git data in {common}, outside what its sandbox can write: every git write needs an approval",
                    outside.dir.display()
                ),
            ),
            Some(fix),
        ));
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outside(runtime: Runtime) -> Outside {
        Outside {
            label: "senate::codex".into(),
            runtime,
            dir: "/r/.worktree/a".into(),
            common: "/r/.git".into(),
        }
    }

    fn codex(text: &str) -> Sandboxes {
        Sandboxes {
            codex: Some(("/h/.codex/config.toml".into(), text.into())),
        }
    }

    #[test]
    fn a_codex_worktree_outside_its_writable_roots_names_the_setting() {
        let checks = checks(&[outside(Runtime::Codex)], &codex(""), Locale::En);
        let [check] = checks.as_slice() else {
            panic!("{checks:#?}");
        };
        assert_eq!(check.id, "sessions.git.senate::codex");
        assert!(check.found.contains("/r/.git"), "{check:?}");
        let fix = check.fix.as_deref().unwrap();
        assert!(
            fix.contains("/h/.codex/config.toml")
                && fix.contains(r#"[sandbox_workspace_write] writable_roots = ["/r/.git"]"#),
            "{fix}"
        );
        // Already writable, or no sandbox at all: nothing to say.
        let covered = "[sandbox_workspace_write]\nwritable_roots = [\"/r\"]\n";
        assert!(checks_for(Runtime::Codex, &codex(covered)).is_empty());
        let open = "sandbox_mode = \"danger-full-access\"\n";
        assert!(checks_for(Runtime::Codex, &codex(open)).is_empty());
    }

    fn checks_for(runtime: Runtime, sandboxes: &Sandboxes) -> Vec<Check> {
        checks(&[outside(runtime)], sandboxes, Locale::En)
    }

    #[test]
    fn a_claude_worktree_never_warns() {
        // Its sandbox writes a linked worktree's shared `.git` already.
        assert!(checks_for(Runtime::Claude, &codex("")).is_empty());
    }

    #[test]
    fn git_tells_a_worktree_from_its_main_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let git = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "git {args:?}");
        };
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q"]);
        git(
            &repo,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "x",
            ],
        );
        let tree = tmp.path().join("tree");
        git(&repo, &["worktree", "add", "-q", tree.to_str().unwrap()]);
        assert_eq!(common_dir(&repo), Some(canonical(&repo.join(".git"))));
        let common = common_dir(&tree).unwrap();
        assert!(!common.starts_with(canonical(&tree)), "{common:?}");
        assert!(common_dir(tmp.path()).is_none());
    }
}
