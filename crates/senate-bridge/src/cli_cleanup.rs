//! `senate cleanup`: show what a cleanup would do, ask once, and apply it
//! with one authentication. The plan and its rules are in
//! [`crate::cleanup`]; the daemon derives and applies it.

use std::io::Write as _;
use std::time::Duration;

use clap::Args;
use clap_complete::engine::ArgValueCandidates;
use senate_core::CommitteeId;
use senate_core::alias::AliasName;

use super::{BridgeConfig, Output, completion, daemon_call, on_a_terminal, print_json, runtime};
use crate::cleanup::{self, Plan, Report, Scope};
use crate::ipc::{self, Request};
use crate::{Paths, mcp};

#[derive(Args)]
pub(super) struct Cleanup {
    /// Show the plan and change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Apply without asking first. You still authenticate, once.
    #[arg(long, short, conflicts_with = "dry_run")]
    yes: bool,
    /// Act on this committee only; repeat for several. All of them
    /// otherwise.
    #[arg(long = "committee", value_name = "COMMITTEE", add = ArgValueCandidates::new(completion::committees))]
    committees: Vec<CommitteeId>,
    /// Also archive open threads in which nothing was said for this long
    /// (2d, 12h, 1w).
    #[arg(long, value_name = "DURATION", value_parser = cleanup::parse_duration)]
    older_than: Option<Duration>,
    /// Leave a senator that was active within this long: one a chair just
    /// stopped may be restored. `0s` takes every dead one.
    #[arg(long, value_name = "DURATION", value_parser = cleanup::parse_duration, default_value = "1h")]
    ended_for: Duration,
    /// Never touch this senator: an alias, committee::alias or a full
    /// senator name. Repeat for several.
    #[arg(long, value_name = "SENATOR", value_parser = mcp::parse_alias_name)]
    keep: Vec<AliasName>,
    /// Also remove dead chairs and secretaries. They are listed either
    /// way; the server names a remaining member chair when there is one.
    #[arg(long)]
    include_roles: bool,
    /// Print the plan, or once applied the report, as JSON. Never asks:
    /// without --yes it is a dry run.
    #[arg(long)]
    json: bool,
}

pub(super) fn run(args: Cleanup, output: Output) -> anyhow::Result<()> {
    let paths = Paths::from_env()?;
    let locale = BridgeConfig::locale_at(&paths.config());
    let rt = runtime(false)?;
    let scope = Scope {
        committees: args.committees,
        older_than_secs: args.older_than.map(|d| d.as_secs()),
        ended_for_secs: args.ended_for.as_secs(),
        keep: args.keep,
        include_roles: args.include_roles,
    };
    let plan: Plan = rt
        .block_on(daemon_call(&paths, &Request::CleanupPlan { scope }))
        .map_err(|error| {
            // A daemon from before this command cannot read the request.
            if error.to_string().contains("unreadable request") {
                anyhow::anyhow!(
                    "{}",
                    locale.pick(
                        "正在运行的 Senate daemon 比这个 senate 旧，还不认识 cleanup；运行 `senate setup service install --yes` 换上当前版本。",
                        "The running Senate daemon is older than this senate and does not know `cleanup`; run `senate setup service install --yes` to start the current one."
                    )
                )
            } else {
                error
            }
        })?;
    let apply = !args.dry_run && (args.yes || !args.json);
    if args.json && !apply {
        return print_json(&plan);
    }
    if !args.json {
        print!(
            "{}",
            cleanup::render_plan(&plan, locale, output.verbose, chrono::Utc::now())
        );
    }
    if !apply || plan.totals().is_empty() {
        return Ok(());
    }
    if !args.yes {
        if !on_a_terminal() {
            println!(
                "{}",
                locale.pick(
                    "没有终端可以确认，什么都没有改动；加上 --yes 执行这份计划。",
                    "There is no terminal to confirm on, so nothing was changed; add --yes to apply this plan."
                )
            );
            return Ok(());
        }
        print!(
            "{}",
            locale.pick(
                "执行这份计划？只需认证一次。[y/N] ",
                "Apply this plan? You authenticate once. [y/N] "
            )
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            println!(
                "{}",
                locale.pick("什么都没有改动。", "Nothing was changed.")
            );
            return Ok(());
        }
    }
    let request = Request::CleanupApply {
        plan: Box::new(plan),
    };
    // The approved plan travels in the request: one line, like any other.
    anyhow::ensure!(
        serde_json::to_vec(&request)?.len() < ipc::MAX_LINE - 4096,
        "{}",
        locale.pick(
            "这份计划太大，一次请求装不下；用 --committee 分几次清理。",
            "This plan is too large for one request; clean up a few committees at a time with --committee."
        )
    );
    // The wait covers the authentication and then the run itself, which on
    // a large Senate is hundreds of writes.
    if cfg!(target_os = "macos") {
        eprintln!("Authenticate this command on the hosting Mac.");
    }
    let report: Report = rt.block_on(ipc::request(
        &paths.socket(),
        &request,
        Duration::from_secs(1),
        crate::app_confirmation::LIFETIME + Duration::from_secs(600),
    ))?;
    if args.json {
        print_json(&report)?;
    } else {
        print!("{}", cleanup::render_report(&report, locale));
    }
    anyhow::ensure!(
        report.failed.is_empty(),
        "{}",
        locale.pick(
            format!("有 {} 项没有完成", report.failed.len()),
            format!("{} item(s) were not done", report.failed.len())
        )
    );
    Ok(())
}
