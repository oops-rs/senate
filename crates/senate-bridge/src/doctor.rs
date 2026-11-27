//! `senate setup doctor`: check this machine and say how to fix what is
//! wrong. Each check is a status (ok, warn, fail), one line of what it
//! found and, where one command fixes it, that command.
//!
//! Nothing here has rules of its own about what is right: a check asks the
//! code that sets the thing up what it would do now. The service file is
//! current when `setup service install` would write the same file; Claude
//! Code, Codex and agy are registered when `setup install` has nothing to
//! change; a leftover is a bridge's hook or MCP server that is still there
//! once `setup install` has done its work.
//!
//! The checks are split in two. [`local`] reads files only and runs with
//! the daemon down; [`live`] judges what the daemon and the Senates
//! answered, gathered by [`gather`]. Both are pure given their inputs, so
//! each is tested against a fake home.

use std::fmt::Write as _;
use std::path::Path;

use senate_core::Locale;
use serde::Serialize;

use crate::install::{self, InstallOptions};
use crate::service::{self, Found};
use crate::{Paths, binding};

mod leftovers;
mod live;
mod worktree;
mod wsl;

pub(crate) use live::cursors_ahead;
pub use live::{CodexProbe, Live, Probe, gather, live};
pub use wsl::Wsl;

/// The command that brings the registrations up to date.
const INSTALL: &str = "senate setup install --yes";
/// The command that rewrites the service file, records the coding agents'
/// programs and restarts the daemon.
const SERVICE_INSTALL: &str = "senate setup service install --yes";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Info,
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One thing checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Stable, dotted: `service.file`, `install.codex.approval`.
    pub id: String,
    pub status: Status,
    /// What was found, one line, in the machine's language.
    pub found: String,
    /// The command that fixes it, when one does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

impl Check {
    pub fn info(id: impl Into<String>, found: impl Into<String>) -> Self {
        Self::new(id, Status::Info, found, None::<String>)
    }

    pub fn ok(id: impl Into<String>, found: impl Into<String>) -> Self {
        Self::new(id, Status::Ok, found, None::<String>)
    }

    pub fn warn(
        id: impl Into<String>,
        found: impl Into<String>,
        fix: Option<impl Into<String>>,
    ) -> Self {
        Self::new(id, Status::Warn, found, fix)
    }

    pub fn fail(
        id: impl Into<String>,
        found: impl Into<String>,
        fix: Option<impl Into<String>>,
    ) -> Self {
        Self::new(id, Status::Fail, found, fix)
    }

    fn new(
        id: impl Into<String>,
        status: Status,
        found: impl Into<String>,
        fix: Option<impl Into<String>>,
    ) -> Self {
        Self {
            id: id.into(),
            status,
            found: found.into(),
            fix: fix.map(Into::into),
        }
    }
}

/// How many checks ended each way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub ok: usize,
    pub warn: usize,
    pub fail: usize,
}

/// Every check, in the order they ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    pub summary: Summary,
}

impl Report {
    pub fn new(checks: Vec<Check>) -> Self {
        let mut summary = Summary::default();
        for check in &checks {
            match check.status {
                Status::Info => {}
                Status::Ok => summary.ok += 1,
                Status::Warn => summary.warn += 1,
                Status::Fail => summary.fail += 1,
            }
        }
        Self { checks, summary }
    }

    /// Whether any check failed: `doctor` then exits non-zero.
    pub fn failed(&self) -> bool {
        self.summary.fail > 0
    }

    /// One line per check, its fix indented below it; then the counts.
    pub fn render(&self, l: Locale) -> String {
        let width = self.checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
        let mut out = String::new();
        for check in &self.checks {
            let _ = writeln!(
                out,
                "{:4}  {:width$}  {}",
                check.status.word(),
                check.id,
                check.found
            );
            if let Some(fix) = &check.fix {
                let _ = writeln!(
                    out,
                    "{:4}  {:width$}  {} {fix}",
                    "",
                    "",
                    l.pick("修复：", "fix:")
                );
            }
        }
        let Summary { ok, warn, fail } = self.summary;
        let _ = writeln!(
            out,
            "\n{}",
            l.pick(
                format!("{ok} 项正常，{warn} 项警告，{fail} 项失败"),
                format!("{ok} ok, {warn} warn, {fail} fail")
            )
        );
        out
    }
}

/// What the file checks read: this machine's paths, and where Claude Code,
/// Codex, agy and the service are, as `setup install` and `setup service
/// install` would find them.
pub struct Local<'a> {
    pub paths: &'a Paths,
    pub locale: Locale,
    /// Claude Code, Codex and agy as `setup install` resolves them; the
    /// error when it cannot.
    pub install: Result<&'a InstallOptions, String>,
    /// The daemon's service; `None` where there is no service manager.
    pub service: Option<&'a service::Spec>,
    /// `senated`, as `setup service install` finds it.
    pub senated: Option<&'a Path>,
    pub manager: &'a dyn service::Manager,
    /// Set when this runs inside WSL.
    pub wsl: Option<&'a Wsl>,
}

/// Hosted Codex sessions whose worktree keeps its git data where Codex's
/// sandbox cannot write ([`worktree`]). Asks git, so after [`gather`].
pub fn worktree_checks(live: &Live, install: Option<&InstallOptions>, l: Locale) -> Vec<Check> {
    let Some(sessions) = &live.sessions else {
        return Vec::new();
    };
    let found = worktree::find(&sessions.rows);
    if found.is_empty() {
        return Vec::new();
    }
    let sandboxes = worktree::Sandboxes::read(install.and_then(|o| o.codex_config.as_deref()));
    worktree::checks(&found, &sandboxes, l)
}

/// Every check that reads files only.
pub fn local(m: &Local<'_>) -> Vec<Check> {
    let l = m.locale;
    let mut checks = Vec::new();
    if let Some(spec) = m.service {
        checks.extend(service_checks(spec, m.senated, m.manager, l));
    }
    match m.install {
        Ok(opts) => {
            checks.extend(install_checks(opts, l));
            checks.extend(leftovers::checks(opts, l));
        }
        Err(ref error) => checks.push(Check::fail(
            "install",
            l.pick(
                format!("无法确定 Claude Code 的位置：{error}"),
                format!("cannot tell where Claude Code is: {error}"),
            ),
            None::<String>,
        )),
    }
    checks.extend(binding_checks(&binding::list(m.paths), l));
    if let Some(wsl) = m.wsl {
        checks.extend(wsl::checks(wsl, m.service, l));
    }
    checks
}

// ---- the service -----------------------------------------------------------

/// The `PATH` launchd or systemd starts a service with when its file sets
/// none.
fn manager_default_path(platform: service::Platform) -> &'static str {
    match platform {
        service::Platform::Macos => "/usr/bin:/bin:/usr/sbin:/sbin",
        service::Platform::Linux => "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    }
}

/// The service runs Senate's copy of `senated` (and `senate`); a later
/// upgrade reaches it only at the next `setup service install`.
fn copy_check(spec: &service::Spec, senated: &Path, l: Locale) -> Check {
    let stale = service::stale_copies(spec, senated);
    if stale.is_empty() {
        return Check::ok(
            "service.binary",
            l.pick(
                format!("服务运行的副本与 {} 相同", senated.display()),
                format!("the service runs a current copy of {}", senated.display()),
            ),
        );
    }
    let copy = &stale[0];
    let running = service::program_version(&copy.to);
    let found = service::program_version(&copy.from);
    let ours = env!("CARGO_PKG_VERSION");
    let text = match (&running, &found) {
        (Some(running), Some(found)) if running != found => l.pick(
            format!(
                "服务运行 {running}（{}），已安装的是 {found}（{}；这个 senate 是 {ours}）",
                copy.to.display(),
                copy.from.display()
            ),
            format!(
                "the service runs {running} ({}), {found} is installed ({}; this senate is {ours})",
                copy.to.display(),
                copy.from.display()
            ),
        ),
        (None, _) if !copy.to.exists() => l.pick(
            format!("服务的副本 {} 不存在", copy.to.display()),
            format!("the service's copy {} is missing", copy.to.display()),
        ),
        _ => l.pick(
            format!(
                "服务的副本 {} 与已安装的 {} 不同",
                copy.to.display(),
                copy.from.display()
            ),
            format!(
                "the service's copy {} differs from the installed {}",
                copy.to.display(),
                copy.from.display()
            ),
        ),
    };
    Check::warn("service.binary", text, Some(SERVICE_INSTALL))
}

fn executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| crate::fsmode::runnable(p, &m))
}

/// The service file is what `setup service install` writes now, the
/// service runs, and the daemon it starts finds `claude`, `codex` and
/// `agy`.
pub fn service_checks(
    spec: &service::Spec,
    senated: Option<&Path>,
    manager: &dyn service::Manager,
    l: Locale,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let file = spec.file();
    let installed = std::fs::read_to_string(&file).ok();
    match (senated, &installed) {
        (None, _) => checks.push(Check::fail(
            "service.file",
            l.pick(
                "找不到 senated：它既不在 senate 旁边，也不在 PATH 上",
                "`senated` is neither next to `senate` nor on PATH",
            ),
            Some("cargo install senate senated"),
        )),
        (Some(_), None) => checks.push(Check::warn(
            "service.file",
            l.pick(
                format!(
                    "没有安装服务（{}）：守护进程只在手动启动时运行",
                    file.display()
                ),
                format!(
                    "no service ({}): the daemon runs only while started by hand",
                    file.display()
                ),
            ),
            Some(SERVICE_INSTALL),
        )),
        (Some(senated), Some(now)) => checks.push(file_check(spec, senated, now, l)),
    }
    if let (Some(senated), Some(now)) = (senated, &installed)
        && now.contains(&*spec.managed_senated().to_string_lossy())
    {
        checks.push(copy_check(spec, senated, l));
    }
    if installed.is_some() {
        let status = service::status(spec, manager);
        checks.push(match status.running {
            Some(true) => Check::ok(
                "service.running",
                l.pick("服务在运行", "the service is running"),
            ),
            Some(false) => match &status.last_exit {
                Some(exit) if exit.codesigning => Check::fail(
                    "service.running",
                    l.pick(
                        format!(
                            "服务没有运行：macOS 拒绝启动守护进程（{}），它的程序文件在签名后被原地覆盖（例如 cargo install）。重新安装服务会让它运行 Senate 自己的副本",
                            exit.detail
                        ),
                        format!(
                            "the service is not running: macOS refused to start the daemon ({}) because its binary was overwritten in place after it was signed (cargo install, say). Reinstalling the service runs Senate's own copy instead",
                            exit.detail
                        ),
                    ),
                    Some(SERVICE_INSTALL),
                ),
                exit => Check::fail(
                    "service.running",
                    l.pick(
                        format!(
                            "服务已安装但没有运行{}；日志：{}",
                            exit.as_ref().map_or_else(String::new, |e| format!("（{}）", e.detail)),
                            status.log.display()
                        ),
                        format!(
                            "the service is installed but not running{}; log: {}",
                            exit.as_ref().map_or_else(String::new, |e| format!(" ({})", e.detail)),
                            status.log.display()
                        ),
                    ),
                    Some(SERVICE_INSTALL),
                ),
            },
            None => Check::warn(
                "service.running",
                l.pick("无法询问服务管理器", "could not ask the service manager"),
                None::<String>,
            ),
        });
    }
    // The daemon looks a program up on the service's PATH unless its path
    // is recorded; without a service, on the PATH it was started with.
    let path = match &installed {
        Some(text) => Some(
            service::path_in_file(spec.platform, text)
                .unwrap_or_else(|| manager_default_path(spec.platform).to_owned()),
        ),
        None => spec.path.clone(),
    };
    let daemon = service::agent_programs(&spec.config, path.as_deref());
    let shell = service::agent_programs(&spec.config, spec.path.as_deref());
    for (program, here) in daemon.iter().zip(&shell) {
        checks.push(program_check(program, here, installed.is_some(), l));
    }
    checks
}

/// The installed service file against what `setup service install` would
/// write now. The `PATH` it records is taken from the shell that installed
/// it, and a shell run over SSH has a far shorter one, so a file that
/// differs only in its `PATH` is not stale: it is a warning only when the
/// daemon's `PATH` misses a program this shell finds.
fn file_check(spec: &service::Spec, senated: &Path, now: &str, l: Locale) -> Check {
    let file = spec.file();
    let plan = service::plan_install(spec, senated);
    if plan.unchanged {
        return Check::ok(
            "service.file",
            l.pick(
                format!("{} 是最新的", file.display()),
                format!("{} is current", file.display()),
            ),
        );
    }
    let installed_path = service::path_in_file(spec.platform, now);
    let as_installed = service::Spec {
        path: installed_path.clone(),
        ..spec.clone()
    };
    if service::service_file_contents(&as_installed, &spec.managed_senated()) != now {
        let (have, want) = first_difference(now, &plan.contents);
        return Check::fail(
            "service.file",
            l.pick(
                format!(
                    "{} 与 setup service install 现在会写的不同：现为 `{have}`，应为 `{want}`",
                    file.display()
                ),
                format!(
                    "{} differs from what setup service install writes now: has `{have}`, would write `{want}`",
                    file.display()
                ),
            ),
            Some(SERVICE_INSTALL),
        );
    }
    let daemon_path =
        installed_path.unwrap_or_else(|| manager_default_path(spec.platform).to_owned());
    let daemon = service::agent_programs(&spec.config, Some(&daemon_path));
    let shell = service::agent_programs(&spec.config, spec.path.as_deref());
    let missed: Vec<&str> = daemon
        .iter()
        .zip(&shell)
        .filter(|(d, s)| matches!(d.found, Found::Missing) && matches!(s.found, Found::Resolved(_)))
        .map(|(d, _)| d.name)
        .collect();
    if missed.is_empty() {
        Check::ok(
            "service.file",
            l.pick(
                format!(
                    "{} 是最新的（它的 PATH 与这个 shell 的不同，守护进程用它能找到所需程序）",
                    file.display()
                ),
                format!(
                    "{} is current (its PATH differs from this shell's and finds the programs the daemon needs)",
                    file.display()
                ),
            ),
        )
    } else {
        let missed = missed.join(", ");
        Check::warn(
            "service.file",
            l.pick(
                format!(
                    "{} 的 PATH 找不到 {missed}，这个 shell 的 PATH 能找到",
                    file.display()
                ),
                format!(
                    "{}'s PATH does not find {missed}, which this shell's PATH does",
                    file.display()
                ),
            ),
            Some(SERVICE_INSTALL),
        )
    }
}

/// The first line where two texts differ, each trimmed (`(end)` past the
/// end of one).
fn first_difference(have: &str, want: &str) -> (String, String) {
    let mut a = have.lines();
    let mut b = want.lines();
    loop {
        match (a.next(), b.next()) {
            (Some(x), Some(y)) if x == y => continue,
            (None, None) => return (String::new(), String::new()),
            (x, y) => {
                let show =
                    |s: Option<&str>| s.map_or_else(|| "(end)".to_owned(), |s| s.trim().to_owned());
                return (show(x), show(y));
            }
        }
    }
}

/// Whether the daemon finds one coding agent's program. `daemon` is the
/// program as the daemon looks it up; `shell` as this shell would.
fn program_check(
    daemon: &service::AgentProgram,
    shell: &service::AgentProgram,
    service: bool,
    l: Locale,
) -> Check {
    let id = format!("service.program.{}", daemon.name);
    let name = daemon.name;
    match &daemon.found {
        Found::Configured(path) if !executable(path) => Check::fail(
            id,
            l.pick(
                format!(
                    "{} 记录的 {} 不存在：在 senate.toml 中删掉 {}，再运行 {SERVICE_INSTALL}",
                    daemon.setting,
                    path.display(),
                    daemon.setting
                ),
                format!(
                    "{} records {}, which is gone: remove {} from senate.toml, then run {SERVICE_INSTALL}",
                    daemon.setting,
                    path.display(),
                    daemon.setting
                ),
            ),
            None::<String>,
        ),
        Found::Configured(path) | Found::Resolved(path) if crate::wsl::is_windows_path(path) => {
            Check::fail(
                id,
                l.pick(
                    format!("{name} 是 Windows 程序（{}）：守护进程需要 Linux 版", path.display()),
                    format!(
                        "{name} is a Windows program ({}): the daemon needs the Linux one",
                        path.display()
                    ),
                ),
                Some(SERVICE_INSTALL),
            )
        }
        Found::Configured(path) => Check::ok(
            id,
            l.pick(
                format!("{name}：{}（{}）", path.display(), daemon.setting),
                format!("{name}: {} ({})", path.display(), daemon.setting),
            ),
        ),
        Found::Resolved(path) => Check::ok(
            id,
            l.pick(
                format!("{name}：{}（在守护进程的 PATH 上）", path.display()),
                format!("{name}: {} (on the daemon's PATH)", path.display()),
            ),
        ),
        Found::Missing => match &shell.found {
            Found::Resolved(here) => Check::fail(
                id,
                l.pick(
                    format!(
                        "守护进程找不到 {name}；这个 shell 里它在 {}",
                        here.display()
                    ),
                    format!(
                        "the daemon cannot find {name}; this shell has it at {}",
                        here.display()
                    ),
                ),
                service.then_some(SERVICE_INSTALL),
            ),
            _ if name == "claude" => Check::warn(
                id,
                l.pick(
                    "找不到 claude：无法启动托管的 Claude Code 会话",
                    "claude is not found: hosted Claude Code sessions cannot start",
                ),
                None::<String>,
            ),
            _ => Check::ok(
                id,
                l.pick(format!("{name} 未安装"), format!("{name} is not installed")),
            ),
        },
    }
}

// ---- registrations ---------------------------------------------------------

/// Claude Code, Codex and agy are registered the way `setup install` would
/// register them now. Codex's approval mode and agy's grant are checked on
/// their own: without them a hosted session blocks on its first Senate
/// tool call.
pub fn install_checks(opts: &InstallOptions, l: Locale) -> Vec<Check> {
    let plan = match install::plan(opts) {
        Ok(plan) => plan,
        Err(error) => {
            return vec![Check::fail("install", format!("{error:#}"), None::<String>)];
        }
    };
    let mut checks = Vec::new();

    let mut claude = plan.settings_changes.clone();
    match &plan.mcp {
        install::McpPlan::Present => {}
        install::McpPlan::Add => claude.push(format!("+ mcpServers.{}", crate::mcp::SERVER_NAME)),
        install::McpPlan::Replace { current } => claude.push(format!(
            "~ mcpServers.{}: {current}",
            crate::mcp::SERVER_NAME
        )),
    }
    if plan.command == install::CommandPlan::Write {
        claude.push(format!("~ {}", opts.command_path().display()));
    }
    checks.push(drift("install.claude", "Claude Code", &claude, l));
    if plan.command == install::CommandPlan::Foreign {
        checks.push(Check::warn(
            "install.claude.command",
            l.pick(
                format!(
                    "{} 不是 Senate 写的，没有改动",
                    opts.command_path().display()
                ),
                format!(
                    "{} is not Senate's and was left alone",
                    opts.command_path().display()
                ),
            ),
            None::<String>,
        ));
    }

    match &plan.codex {
        None => checks.push(Check::ok(
            "install.codex",
            l.pick(
                "codex 不在 PATH 上，未检查",
                "codex is not on PATH; not checked",
            ),
        )),
        Some(codex) => {
            let (approval, rest): (Vec<String>, Vec<String>) = codex
                .changes
                .iter()
                .cloned()
                .partition(|c| c.contains(install::codex::APPROVAL_KEY));
            checks.push(drift("install.codex", "Codex", &rest, l));
            checks.push(if approval.is_empty() {
                Check::ok(
                    "install.codex.approval",
                    l.pick(
                        "Codex 无需逐次批准 Senate 的工具",
                        "Codex runs Senate's tools without asking",
                    ),
                )
            } else {
                Check::fail(
                    "install.codex.approval",
                    l.pick(
                        format!(
                            "{} 缺少 mcp_servers.senate.default_tools_approval_mode = \"approve\"：托管的 Codex 会卡在第一次调用 Senate 工具",
                            codex.path.display()
                        ),
                        format!(
                            "{} lacks mcp_servers.senate.default_tools_approval_mode = \"approve\": a hosted Codex blocks on its first Senate tool call",
                            codex.path.display()
                        ),
                    ),
                    Some(INSTALL),
                )
            });
        }
    }

    match &plan.agy {
        None => checks.push(Check::ok(
            "install.agy",
            l.pick(
                "agy 不在 PATH 上，未检查",
                "agy is not on PATH; not checked",
            ),
        )),
        Some(agy) => {
            let rest: Vec<String> = agy
                .mcp
                .changes
                .iter()
                .chain(&agy.hooks.changes)
                .cloned()
                .collect();
            checks.push(drift("install.agy", "agy", &rest, l));
            checks.push(if agy.settings.changes.is_empty() {
                Check::ok(
                    "install.agy.grant",
                    l.pick(
                        format!("agy 允许 {}", install::agy::GRANT),
                        format!("agy allows {}", install::agy::GRANT),
                    ),
                )
            } else {
                Check::fail(
                    "install.agy.grant",
                    l.pick(
                        format!(
                            "{} 的 permissions.allow 缺少 {}：托管的 agy 会卡在第一次调用 Senate 工具",
                            agy.settings.path.display(),
                            install::agy::GRANT
                        ),
                        format!(
                            "{} lacks {} in permissions.allow: a hosted agy blocks on its first Senate tool call",
                            agy.settings.path.display(),
                            install::agy::GRANT
                        ),
                    ),
                    Some(INSTALL),
                )
            });
        }
    }
    checks
}

/// Ok when `setup install` would change nothing for `what`; else a failure
/// naming the first change and how many there are.
fn drift(id: &str, what: &str, changes: &[String], l: Locale) -> Check {
    match changes {
        [] => Check::ok(
            id,
            l.pick(
                format!("{what} 的 MCP 服务器和钩子是最新的"),
                format!("{what}: MCP server and hooks are current"),
            ),
        ),
        [first, rest @ ..] => {
            let more = if rest.is_empty() {
                String::new()
            } else {
                l.pick(
                    format!("（另有 {} 处）", rest.len()),
                    format!(" (and {} more)", rest.len()),
                )
            };
            Check::fail(
                id,
                l.pick(
                    format!("{what} 需要更新：{first}{more}"),
                    format!("{what} needs updating: {first}{more}"),
                ),
                Some(INSTALL),
            )
        }
    }
}

// ---- worktree bindings -----------------------------------------------------

/// Saved worktree bindings whose repository, branch or worktree is gone.
pub fn binding_checks(rows: &[binding::Row], l: Locale) -> Vec<Check> {
    let stale: Vec<&binding::Row> = rows.iter().filter(|r| r.stale.is_some()).collect();
    vec![match stale.as_slice() {
        [] => Check::ok(
            "bindings.saved",
            l.pick(
                format!("{} 份保存的工作区绑定都有效", rows.len()),
                format!("{} saved worktree bindings, none expired", rows.len()),
            ),
        ),
        [first, ..] => Check::warn(
            "bindings.saved",
            l.pick(
                format!(
                    "{} 份保存的绑定已过期，例如 {}（{}）",
                    stale.len(),
                    first.worktree.display(),
                    first.stale.unwrap_or_default()
                ),
                format!(
                    "{} saved bindings have expired, e.g. {} ({})",
                    stale.len(),
                    first.worktree.display(),
                    first.stale.unwrap_or_default()
                ),
            ),
            Some("senate setup clean --yes"),
        ),
    }]
}

#[cfg(test)]
#[cfg(unix)]
mod tests;
