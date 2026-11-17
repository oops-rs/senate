#![allow(clippy::unwrap_used, clippy::expect_used)]

//! GhForge against a fake `gh` (a shell script that keeps its PRs in a
//! file): command construction, the token by environment only, idempotency.

use std::path::{Path, PathBuf};

use senate_core::{BranchNaming, CommitteeId, Locale, Name};
use senate_work::{
    Budget, CheckpointFiles, Delivery, Forge, ForgeError, GhForge, GhForgeConfig, PrRef, PrRequest,
    RepoAllowlist, RepoRef, RunOutcome, RunStatus, WorkError, WorkOrder,
};

const FAKE_GH: &str = r#"#!/bin/sh
D="$(dirname "$0")"
{ printf 'CALL'; printf ' [%s]' "$@"; printf '\n'; } >> "$D/calls"
printf '%s\n' "$GH_TOKEN" >> "$D/tokens"
env | grep -q '^GITHUB_TOKEN=' && echo leaked >> "$D/tokens"
case "$1 $2" in
  "pr list")
    if [ -f "$D/pr" ]; then printf '[{"number":%s,"url":"https://github.com/o/refund/pull/%s"}]\n' "$(cat "$D/pr")" "$(cat "$D/pr")"; else echo '[]'; fi ;;
  "pr create")
    if [ -f "$D/fail-create" ]; then echo "a pull request already exists" >&2; echo 41 > "$D/pr"; exit 1; fi
    echo 42 > "$D/pr"
    echo "Creating pull request for feature/refund-v2/speaker-7 into main in o/refund"
    echo "https://github.com/o/refund/pull/42" ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;

struct Fx {
    _tmp: tempfile::TempDir,
    dir: PathBuf,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();
        let gh = dir.join("gh");
        std::fs::write(&gh, FAKE_GH).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(dir.join("token"), "ghp_supersecret\n").unwrap();
        Self { _tmp: tmp, dir }
    }

    fn forge(&self, repos: &[&str]) -> GhForge {
        GhForge::new(GhForgeConfig {
            gh_bin: self.dir.join("gh"),
            ..GhForgeConfig::new(
                self.dir.join("token"),
                RepoAllowlist::new(repos.iter().copied()),
            )
        })
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

fn request() -> PrRequest {
    let order = WorkOrder {
        committee: CommitteeId::new("refund-v2").unwrap(),
        run_id: 7,
        requester: Name::new("bob").unwrap(),
        on_behalf_of: "Bob".into(),
        target_agent: "claude".parse().unwrap(),
        instruction: "finish the retry path".into(),
        repo: RepoRef::from_url("git@github.com:o/refund.git").unwrap(),
        base_branch: "feature/refund-v2/alice".into(),
        base_commit: "abcdef0123".into(),
        pr_base: "main".into(),
        checkpoint_files: CheckpointFiles::default(),
        budget: Budget::default(),
        branches: BranchNaming::default(),
        locale: Locale::En,
    };
    let outcome = RunOutcome {
        status: RunStatus::Succeeded,
        branch: order.branch(),
        pushed_commit: Some("abc".into()),
        summary: "done".into(),
        next_steps: vec![],
    };
    PrRequest::for_run(&order, &outcome)
}

fn pr(n: u64) -> Delivery {
    Delivery::PullRequest(PrRef {
        url: format!("https://github.com/o/refund/pull/{n}"),
        number: n,
    })
}

#[tokio::test]
async fn opens_once_then_returns_the_same_pr() {
    let fx = Fx::new();
    let forge = fx.forge(&["o/refund"]);
    let req = request();

    assert_eq!(forge.open_pr(&req).await.unwrap(), pr(42));
    assert_eq!(forge.open_pr(&req).await.unwrap(), pr(42));

    let calls = fx.read("calls");
    // One call per "CALL " (the PR body spans lines).
    let lines: Vec<String> = calls
        .split("CALL ")
        .filter(|c| !c.is_empty())
        .map(|c| format!("CALL {}", c.trim_end()))
        .collect();
    // list, create, list: the second open finds the PR and creates nothing.
    assert_eq!(lines.len(), 3, "{calls}");
    assert_eq!(
        lines[0],
        "CALL [pr] [list] [--repo] [o/refund] [--head] [feature/refund-v2/speaker-7] [--state] [open] [--json] [number,url] [--limit] [1]"
    );
    assert!(lines[1].starts_with(
        "CALL [pr] [create] [--repo] [o/refund] [--base] [main] [--head] [feature/refund-v2/speaker-7] [--title] ["
    ));
    assert!(lines[1].contains("for [senator::bob]"));
    assert!(!lines[1].contains("--draft"));
    assert!(lines[2].starts_with("CALL [pr] [list]"));

    // The token reached gh by environment, every time, and never by argument.
    assert!(!calls.contains("ghp_"));
    assert_eq!(fx.read("tokens"), "ghp_supersecret\n".repeat(3));

    assert_eq!(
        forge.find_pr(&req.repo, &req.head).await.unwrap(),
        Some(PrRef {
            url: "https://github.com/o/refund/pull/42".into(),
            number: 42
        })
    );
}

#[tokio::test]
async fn always_draft_opens_a_finished_run_as_a_draft() {
    let fx = Fx::new();
    let forge = GhForge::new(GhForgeConfig {
        gh_bin: fx.path("gh"),
        always_draft: true,
        ..GhForgeConfig::new(fx.path("token"), RepoAllowlist::new(["o/refund"]))
    });
    let req = request();
    assert!(!req.draft, "a finished run");
    assert_eq!(forge.open_pr(&req).await.unwrap(), pr(42));
    let calls = fx.read("calls");
    assert_eq!(calls.matches("[--draft]").count(), 1, "{calls}");
}

#[tokio::test]
async fn create_race_falls_back_to_the_existing_pr() {
    let fx = Fx::new();
    std::fs::write(fx.path("fail-create"), "").unwrap();
    let forge = fx.forge(&["o/refund"]);
    assert_eq!(forge.open_pr(&request()).await.unwrap(), pr(41));
}

#[tokio::test]
async fn errors_are_scrubbed_of_the_token() {
    let fx = Fx::new();
    // A gh that echoes its token in the error.
    std::fs::write(
        fx.path("gh"),
        "#!/bin/sh\necho \"bad credentials: $GH_TOKEN\" >&2\nexit 1\n",
    )
    .unwrap();
    let err = fx
        .forge(&["o/refund"])
        .open_pr(&request())
        .await
        .unwrap_err();
    let ForgeError::Command(msg) = err else {
        panic!("{err:?}")
    };
    assert!(msg.contains("bad credentials: ***"), "{msg}");
    assert!(!msg.contains("ghp_"));
}

#[tokio::test]
async fn refuses_unlisted_repos_without_calling_gh() {
    let fx = Fx::new();
    let forge = fx.forge(&["o/other"]);
    let req = request();
    assert_eq!(
        forge.open_pr(&req).await,
        Err(ForgeError::Refused(WorkError::RepoNotAllowed(
            "o/refund".into()
        )))
    );
    assert!(matches!(
        forge.find_pr(&req.repo, &req.head).await,
        Err(ForgeError::Refused(_))
    ));
    assert!(!Path::new(&fx.path("calls")).exists());
}

#[tokio::test]
async fn missing_token_file_is_an_error_naming_the_path_only() {
    let fx = Fx::new();
    std::fs::remove_file(fx.path("token")).unwrap();
    let err = fx
        .forge(&["o/refund"])
        .open_pr(&request())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("cannot read the token file"),
        "{err}"
    );
}
