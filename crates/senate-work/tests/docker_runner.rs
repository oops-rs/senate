#![allow(clippy::unwrap_used, clippy::expect_used)]
// The fake is a `#!/bin/sh` script, which Windows cannot run.
#![cfg(unix)]

//! DockerRunner end to end with a fake `docker` (a shell script) and a
//! local bare repository as origin: real git, no Docker, no model, no network.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use senate_core::{BranchNaming, CommitteeId, Locale, Name};
use senate_work::{
    Budget, CheckpointFiles, DockerRunner, DockerRunnerConfig, RepoAllowlist, RepoRef, RunProgress,
    RunStatus, Runner, WorkOrder,
};
use tokio::sync::mpsc;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Alice",
            "-c",
            "user.email=alice@example.com",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    origin: PathBuf,
    alice_commit: String,
}

impl Fixture {
    /// origin `o/refund.git` with `main` and Alice's `senate/refund-v2/alice`.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let origin = root.join("o/refund.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--quiet", "--bare"]);
        let seed = root.join("seed");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "--quiet"]);
        std::fs::write(seed.join("README.md"), "refund\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "--quiet", "-m", "init"]);
        git(
            &seed,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(
            &seed,
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
        );
        std::fs::write(seed.join("retry.rs"), "// half done\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "--quiet", "-m", "wip retry"]);
        git(
            &seed,
            &[
                "push",
                "--quiet",
                "origin",
                "HEAD:refs/heads/senate/refund-v2/alice",
            ],
        );
        let alice_commit = git(&seed, &["rev-parse", "HEAD"]);
        Self {
            _tmp: tmp,
            root,
            origin,
            alice_commit,
        }
    }

    fn order(&self) -> WorkOrder {
        WorkOrder {
            committee: CommitteeId::new("refund-v2").unwrap(),
            run_id: 7,
            requester: Name::new("bob").unwrap(),
            on_behalf_of: "Bob".into(),
            target_agent: "claude".parse().unwrap(),
            instruction: "把 alice 的重试路径做完".into(),
            repo: RepoRef::from_url(&format!("file://{}", self.origin.display())).unwrap(),
            base_branch: "senate/refund-v2/alice".into(),
            base_commit: self.alice_commit.clone(),
            pr_base: "main".into(),
            checkpoint_files: CheckpointFiles {
                progress: Some("retry path half done".into()),
                decisions: Some("use exponential backoff".into()),
                open: None,
            },
            budget: Budget {
                wall_secs: 20,
                max_turns: 50,
            },
            branches: BranchNaming::default(),
            locale: Locale::Zh,
        }
    }

    /// A fake `docker`: records its arguments per subcommand, records the
    /// gateway key it was given through its environment, finds the mounted
    /// work tree, then runs `behavior` (a shell snippet using `$WORK`).
    fn runner(&self, behavior: &str) -> DockerRunner {
        let bin = self.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("docker");
        std::fs::write(
            &script,
            r#"#!/bin/sh
D="$(dirname "$0")"
printf '%s\n' "$@" > "$D/args.$1"
[ "$1" = kill ] && exit 0
printf '%s' "$ANTHROPIC_API_KEY" > "$D/key"
WORK=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--volume" ]; then WORK="${a%:/work}"; fi
  prev="$a"
done
. "$D/behavior.sh"
"#,
        )
        .unwrap();
        std::fs::write(bin.join("behavior.sh"), behavior).unwrap();
        make_executable(&script);
        let key = self.root.join("gateway_key");
        std::fs::write(&key, "sk-gateway-secret\n").unwrap();
        let mut c = DockerRunnerConfig::new("senate-runner:test", self.root.join("runs"));
        c.docker_bin = script;
        c.gateway_base_url = Some("http://gw:8080".into());
        c.gateway_key_file = Some(key);
        c.allowed_repos = RepoAllowlist::new(["o/refund"]);
        DockerRunner::new(c)
    }

    fn bin(&self, name: &str) -> PathBuf {
        self.root.join("bin").join(name)
    }

    fn origin_git(&self, args: &[&str]) -> String {
        git(&self.origin, args)
    }
}

#[cfg(unix)]
fn make_executable(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

async fn run(
    runner: &DockerRunner,
    order: &WorkOrder,
) -> (senate_work::RunOutcome, Vec<RunProgress>) {
    let (tx, mut rx) = mpsc::channel(256);
    let outcome = runner.run(order, tx).await;
    let mut steps = Vec::new();
    while let Ok(p) = rx.try_recv() {
        steps.push(p);
    }
    (outcome, steps)
}

const SUCCESS: &str = r#"
test -e "$WORK/.git" && echo leaked > "$WORK/LEAK"
test -f "$WORK/.senate/PROGRESS.md" && test -f "$WORK/.senate/DECISIONS.md" && echo ok > "$WORK/saw-notes"
echo 'fn retry() {}' > "$WORK/retry.rs"
cat <<'EOF'
{"type":"system","subtype":"init"}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/work/.senate/PROGRESS.md"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Edit","input":{"file_path":"/work/retry.rs"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2"}]}}
{"type":"result","subtype":"success","is_error":false,"num_turns":3,"result":"SUMMARY: Retry path finished.\nNEXT STEPS:\n- Review backoff"}
EOF
"#;

#[tokio::test]
async fn success_pushes_speaker_branch_and_leaves_alice_alone() {
    let fx = Fixture::new();
    let runner = fx.runner(SUCCESS);
    let order = fx.order();
    let (outcome, steps) = run(&runner, &order).await;

    assert_eq!(outcome.status, RunStatus::Succeeded, "{outcome:?}");
    assert_eq!(outcome.branch, "senate/refund-v2/speaker-7");
    assert_eq!(outcome.summary, "Retry path finished.");
    assert_eq!(outcome.next_steps, ["Review backoff"]);
    let pushed = outcome.pushed_commit.clone().expect("pushed");

    // The branch is on origin at the reported commit, by the Speaker.
    assert_eq!(
        fx.origin_git(&["rev-parse", "refs/heads/senate/refund-v2/speaker-7"]),
        pushed
    );
    assert_eq!(
        fx.origin_git(&["log", "-1", "--format=%an <%ae>|%cn|%s", &pushed]),
        "Senate Speaker <speaker@senate>|Senate Speaker|Speaker run 7 for [senator::bob]: 把 alice 的重试路径做完"
    );
    assert_eq!(
        fx.origin_git(&["rev-parse", &format!("{pushed}^")]),
        fx.alice_commit
    );
    let files = fx.origin_git(&["ls-tree", "-r", "--name-only", &pushed]);
    assert!(files.contains("retry.rs"));
    assert!(files.contains("saw-notes"), "notes were readable: {files}");
    assert!(
        !files.contains(".senate"),
        "notes are never committed: {files}"
    );
    assert!(!files.contains("LEAK"), "the agent saw a git dir: {files}");
    assert_eq!(
        fx.origin_git(&["show", &format!("{pushed}:retry.rs")]),
        "fn retry() {}"
    );
    // Alice's branch is untouched.
    assert_eq!(
        fx.origin_git(&["rev-parse", "refs/heads/senate/refund-v2/alice"]),
        fx.alice_commit
    );

    // The key reached the container by environment, never by argument.
    assert_eq!(
        std::fs::read_to_string(fx.bin("key")).unwrap(),
        "sk-gateway-secret"
    );
    let args = std::fs::read_to_string(fx.bin("args.run")).unwrap();
    assert!(!args.contains("sk-gateway-secret"));
    assert!(args.contains("\nANTHROPIC_API_KEY\n"));
    assert!(args.contains("\n--max-turns\n50\n"));

    // Card steps: setup, the agent's tool calls, the agent, the save.
    let labels: Vec<(&str, &str, bool)> = steps
        .iter()
        .map(|p| (p.id.as_str(), p.label.as_str(), p.done))
        .collect();
    assert!(labels.contains(&("tool-t1", "读 .senate/PROGRESS.md", true)));
    assert!(labels.contains(&("tool-t2", "改 retry.rs", false)));
    assert!(labels.iter().any(|(id, _, done)| *id == "setup" && *done));
    assert!(labels.iter().any(|(id, _, done)| *id == "save" && *done));
    assert!(steps.iter().all(|p| !p.label.contains("sk-")));

    // The checkout is gone.
    assert_eq!(std::fs::read_dir(fx.root.join("runs")).unwrap().count(), 0);
}

#[tokio::test]
async fn the_branch_prefix_is_the_deployments() {
    let fx = Fixture::new();
    let runner = fx.runner(SUCCESS);
    let mut order = fx.order();
    order.branches = BranchNaming::new("team");
    order.locale = Locale::En;
    let (outcome, steps) = run(&runner, &order).await;
    assert_eq!(outcome.status, RunStatus::Succeeded, "{outcome:?}");
    assert_eq!(outcome.branch, "team/refund-v2/speaker-7");
    let pushed = outcome.pushed_commit.expect("pushed");
    assert_eq!(
        fx.origin_git(&["rev-parse", "refs/heads/team/refund-v2/speaker-7"]),
        pushed
    );
    // The card's steps are in the order's language.
    assert!(steps.iter().any(|p| p.label == "Read .senate/PROGRESS.md"));
    assert!(
        steps
            .iter()
            .any(|p| p.label.starts_with("Branch from checkpoint"))
    );
    assert!(
        steps
            .iter()
            .any(|p| p.label == "Commit and push team/refund-v2/speaker-7")
    );
}

#[tokio::test(start_paused = true)]
async fn timeout_kills_container_and_pushes_partial_work() {
    let fx = Fixture::new();
    let runner = fx.runner(
        r#"
echo 'partial' > "$WORK/partial.rs"
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Write","input":{"file_path":"/work/partial.rs"}}]}}'
exec sleep 30
"#,
    );
    let mut order = fx.order();
    order.budget.wall_secs = 1;
    let (progress, mut steps) = mpsc::channel::<RunProgress>(256);
    let (recorded, witness) = tokio::sync::oneshot::channel();
    let (release_clock, hold_clock) = std::sync::mpsc::channel::<()>();
    // A blocking receiver inhibits the paused clock's auto-advance while
    // git and the fake container do real I/O. The tool report follows the
    // fixture's file write and is parsed inside the timed agent future.
    let clock = tokio::task::spawn_blocking(move || {
        while let Some(step) = steps.blocking_recv() {
            if step.id == "tool-t1" {
                recorded.send(()).unwrap();
                let _ = hold_clock.recv();
                break;
            }
        }
    });
    let run = runner.run(&order, progress);
    tokio::pin!(run);
    tokio::select! {
        ready = witness => ready.expect("the fake container wrote its partial work"),
        outcome = &mut run => panic!("the run finished before the tool report: {outcome:?}"),
    }
    tokio::time::advance(Duration::from_secs(order.budget.wall_secs)).await;
    tokio::time::resume();
    drop(release_clock);
    let outcome = run.await;
    clock.await.unwrap();

    assert_eq!(outcome.status, RunStatus::TimedOut, "{outcome:?}");
    let pushed = outcome.pushed_commit.clone().expect("partial work pushed");
    assert_eq!(
        fx.origin_git(&["rev-parse", "refs/heads/senate/refund-v2/speaker-7"]),
        pushed
    );
    assert!(
        fx.origin_git(&["log", "-1", "--format=%B", &pushed])
            .contains("Partial work: stopped at the 1s wall clock")
    );
    assert!(
        fx.origin_git(&["ls-tree", "-r", "--name-only", &pushed])
            .contains("partial.rs")
    );
    assert_eq!(outcome.next_steps.len(), 3);
    assert!(outcome.next_steps[2].contains("senate/refund-v2/speaker-7"));
    // The container was killed by name.
    let kill = std::fs::read_to_string(fx.bin("args.kill")).unwrap();
    assert_eq!(kill, "kill\nsenate-speaker-refund-v2-7\n");
}

#[tokio::test]
async fn failed_run_keeps_its_work() {
    let fx = Fixture::new();
    let runner = fx.runner(
        r#"
echo 'half' > "$WORK/half.rs"
echo '{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":50,"result":""}'
exit 1
"#,
    );
    let (outcome, _) = run(&runner, &fx.order()).await;
    assert_eq!(
        outcome.status,
        RunStatus::Failed {
            reason: "hit the turn cap (50)".into()
        }
    );
    let pushed = outcome.pushed_commit.expect("partial work pushed");
    assert!(
        fx.origin_git(&["ls-tree", "-r", "--name-only", &pushed])
            .contains("half.rs")
    );
    assert_eq!(outcome.next_steps.len(), 3);
}

#[tokio::test]
async fn nothing_changed_pushes_nothing() {
    let fx = Fixture::new();
    let runner = fx.runner(
        r#"echo '{"type":"result","subtype":"success","is_error":false,"result":"Nothing to do: it is already done."}'"#,
    );
    let (outcome, _) = run(&runner, &fx.order()).await;
    assert_eq!(outcome.status, RunStatus::Succeeded);
    assert_eq!(outcome.pushed_commit, None);
    assert_eq!(outcome.summary, "Nothing to do: it is already done.");
    let refs = fx.origin_git(&["for-each-ref", "--format=%(refname)"]);
    assert!(!refs.contains("speaker"), "{refs}");
}

#[tokio::test]
async fn refused_order_runs_nothing() {
    let fx = Fixture::new();
    let runner = fx.runner(SUCCESS);
    let mut order = fx.order();
    order.repo = RepoRef::from_url("git@github.com:o/elsewhere.git").unwrap();
    let (outcome, steps) = run(&runner, &order).await;
    assert!(
        matches!(&outcome.status, RunStatus::Refused { reason } if reason.contains("o/elsewhere")),
        "{outcome:?}"
    );
    assert!(steps.is_empty());
    assert!(!fx.bin("args.run").exists(), "docker must not run");
}

#[tokio::test]
async fn missing_checkpoint_commit_fails_before_the_agent() {
    let fx = Fixture::new();
    let runner = fx.runner(SUCCESS);
    let mut order = fx.order();
    order.base_commit = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into();
    let (outcome, _) = run(&runner, &order).await;
    assert!(
        matches!(&outcome.status, RunStatus::Failed { reason } if reason.contains("not in the repository")),
        "{outcome:?}"
    );
    assert!(!fx.bin("args.run").exists());
}

/// A real run: `SENATE_WORK_LIVE=1 SENATE_WORK_IMAGE=senate-runner:dev
/// SENATE_WORK_GATEWAY_URL=… SENATE_WORK_GATEWAY_KEY_FILE=… cargo test -p
/// senate-work -- --ignored live`. Needs Docker and the runner image; uses a
/// local origin, so nothing is pushed anywhere.
#[tokio::test]
#[ignore = "needs Docker, the runner image and a model gateway"]
async fn live_run_in_docker() {
    if std::env::var("SENATE_WORK_LIVE").as_deref() != Ok("1") {
        eprintln!("SENATE_WORK_LIVE is not 1; skipping");
        return;
    }
    let fx = Fixture::new();
    let mut c = DockerRunnerConfig::new(
        std::env::var("SENATE_WORK_IMAGE").unwrap_or_else(|_| "senate-runner:dev".into()),
        fx.root.join("runs"),
    );
    c.gateway_base_url = std::env::var("SENATE_WORK_GATEWAY_URL").ok();
    c.gateway_key_file = std::env::var("SENATE_WORK_GATEWAY_KEY_FILE")
        .ok()
        .map(PathBuf::from);
    c.model = std::env::var("SENATE_WORK_MODEL").ok();
    c.allowed_repos = RepoAllowlist::new(["o/refund"]);
    #[cfg(unix)]
    {
        // Let the container's user write the checkout.
        c.docker_user = Some(format!("{}:{}", uid(), gid()));
    }
    let mut order = fx.order();
    order.instruction =
        "Replace the comment in retry.rs with a Rust function `pub fn retry() {}`.".into();
    order.budget = Budget {
        wall_secs: 300,
        max_turns: 10,
    };
    let (outcome, steps) = run(&DockerRunner::new(c), &order).await;
    eprintln!("{outcome:#?}\n{steps:#?}");
    assert_eq!(outcome.status, RunStatus::Succeeded);
    let pushed = outcome.pushed_commit.expect("pushed");
    assert!(
        fx.origin_git(&["show", &format!("{pushed}:retry.rs")])
            .contains("fn retry")
    );
}

#[cfg(unix)]
fn uid() -> String {
    String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned()
}

#[cfg(unix)]
fn gid() -> String {
    String::from_utf8(Command::new("id").arg("-g").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned()
}
