//! `senate setup service`: keep `senated --machine` running without a terminal.
//!
//! On macOS a launchd user agent (`~/Library/LaunchAgents/dev.senate.senated.plist`),
//! on Linux a systemd user unit (`~/.config/systemd/user/senated.service`).
//! Both run `senated --machine`, start when the user logs in, restart it
//! when it dies, and write stdout and stderr to `senated.log` in the Senate
//! home.
//!
//! The service runs Senate's own copy of the programs, in `bin/` of the
//! Senate home, never the `senated` that was found (next to the running
//! `senate`, else on `PATH`). `install` copies `senated`, and the `senate`
//! beside it that the daemon starts hosted terminals with, into a new file
//! each and renames it into place. So an upgrade that overwrites the
//! installed binaries in place (`cargo install`) never changes a file the
//! running daemon or its hosts were started from: on macOS that left
//! launchd unable to start the daemon again (`OS_REASON_CODESIGNING`). The
//! price is that an upgrade reaches the service only at the next
//! `install`; `senate setup doctor` says when the copy is behind.
//! `SENATE_HOME` is passed to the service only when the home is not the
//! default, `~/.senate`.
//!
//! The file contents ([`launchd_plist`], [`systemd_unit`]) and the commands
//! ([`install_steps`], [`uninstall_steps`], [`status_command`]) are pure
//! functions of a [`Spec`]. Everything that touches the service manager goes
//! through the [`Manager`] seam, which tests replace: nothing here ever
//! calls the real `launchctl` or `systemctl` except [`SystemManager`], and
//! every path is taken from the [`Spec`], never from the real home.
//!
//! A service manager starts the daemon with a `PATH` of its own, so
//! `install` also resolves the coding agents the daemon starts (`claude`,
//! `codex`, `agy`) on the installing shell's `PATH` and records their
//! absolute paths in the Senate config, where the daemon already reads them
//! (`[companion] claude_bin`, `[codex] bin`, `[agy] bin`). A path already configured
//! there is kept. The shell's `PATH` is still written into the service, but
//! only as the fallback, for `git` and what the agents run themselves.
//!
//! Like `senate setup install`, `senate setup service install` and `uninstall` are dry
//! runs that print the file and the commands unless told `--yes`.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use senate_core::Locale;

/// The launchd label, and the plist's name without its extension.
pub const LABEL: &str = "dev.senate.senated";
/// The systemd user unit's name.
pub const UNIT: &str = "senated.service";
/// The log file in the Senate home.
pub const LOG_FILE: &str = "senated.log";

/// Which service manager this machine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// launchd, a user agent.
    Macos,
    /// systemd, a user unit.
    Linux,
}

impl Platform {
    /// The platform this binary runs on; `None` where there is no service
    /// manager `senate` knows.
    pub fn current() -> Option<Self> {
        if cfg!(target_os = "macos") {
            Some(Self::Macos)
        } else if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else {
            None
        }
    }

    fn manager_name(self) -> &'static str {
        match self {
            Self::Macos => "launchd",
            Self::Linux => "systemd (user)",
        }
    }
}

/// Where the service lives, all of it explicit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    pub platform: Platform,
    /// The user's home: the service file goes below it.
    pub user_home: PathBuf,
    /// The Senate home (`~/.senate`, or `$SENATE_HOME`).
    pub senate_home: PathBuf,
    /// The user's numeric id, for launchd's `gui/<uid>` domain.
    pub uid: u32,
    /// The `PATH` the daemon runs with. A service manager starts a program
    /// with a minimal `PATH`, and the daemon runs `git` and the coding
    /// agents' own programs, so the one in effect at install time is kept.
    /// `None` leaves the service manager's default.
    pub path: Option<String>,
    /// The Senate config file, where the coding agents' programs are
    /// recorded.
    pub config: PathBuf,
}

impl Spec {
    /// The service file.
    pub fn file(&self) -> PathBuf {
        match self.platform {
            Platform::Macos => self
                .user_home
                .join("Library/LaunchAgents")
                .join(format!("{LABEL}.plist")),
            Platform::Linux => self.user_home.join(".config/systemd/user").join(UNIT),
        }
    }

    /// Where the service's own copies of the programs are.
    pub fn bin_dir(&self) -> PathBuf {
        self.senate_home.join("bin")
    }

    /// The `senated` the service runs: Senate's copy.
    pub fn managed_senated(&self) -> PathBuf {
        self.bin_dir().join("senated")
    }

    /// Where the daemon's output goes.
    pub fn log(&self) -> PathBuf {
        self.senate_home.join(LOG_FILE)
    }

    /// `SENATE_HOME` for the service, when the home is not `~/.senate`.
    fn home_override(&self) -> Option<&Path> {
        (self.senate_home != self.user_home.join(".senate")).then_some(self.senate_home.as_path())
    }

    /// launchd's name for the service.
    fn target(&self) -> String {
        format!("gui/{}/{LABEL}", self.uid)
    }

    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }
}

fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The launchd user agent's property list.
/// How long the service manager waits for senated to exit after asking it
/// to: above [`crate::config::SHUTDOWN_PAUSE_SECS`], so a shutdown pause
/// finishes.
pub const STOP_TIMEOUT_SECS: u64 = 120;

pub fn launchd_plist(spec: &Spec, senated: &Path) -> String {
    let log = xml(&text(&spec.log()));
    let mut out = String::new();
    out.push_str(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    let _ = writeln!(out, "  <key>Label</key>\n  <string>{LABEL}</string>");
    #[cfg(target_os = "macos")]
    if crate::app_confirmation::team().is_some() {
        let service = crate::app_confirmation::service_name(&spec.senate_home);
        let scope = crate::mac_scope::service(&spec.senate_home);
        let _ = writeln!(
            out,
            "  <key>MachServices</key>\n  <dict>\n    <key>{service}</key>\n    <true/>\n    <key>{scope}</key>\n    <true/>\n  </dict>"
        );
    }
    let _ = writeln!(
        out,
        "  <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    \
         <string>--machine</string>\n  </array>",
        xml(&text(senated))
    );
    out.push_str("  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n");
    for key in ["SoftResourceLimits", "HardResourceLimits"] {
        let limit = crate::fd::SERVICE_LIMIT;
        let _ = writeln!(
            out,
            "  <key>{key}</key>\n  <dict>\n    <key>NumberOfFiles</key>\n    <integer>{limit}</integer>\n  </dict>"
        );
    }
    // The daemon hosts interactive PTYs and their coding agents.
    out.push_str("  <key>ProcessType</key>\n  <string>Interactive</string>\n");
    // Room for `pause_on_shutdown` (90 s at most) before launchd kills it.
    let _ = writeln!(
        out,
        "  <key>ExitTimeOut</key>\n  <integer>{STOP_TIMEOUT_SECS}</integer>"
    );
    let _ = writeln!(
        out,
        "  <key>StandardOutPath</key>\n  <string>{log}</string>\n  \
         <key>StandardErrorPath</key>\n  <string>{log}</string>"
    );
    let mut env = Vec::new();
    if let Some(path) = &spec.path {
        env.push(("PATH", path.clone()));
    }
    if let Some(home) = spec.home_override() {
        env.push(("SENATE_HOME", text(home)));
    }
    if !env.is_empty() {
        out.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        for (key, value) in env {
            let _ = writeln!(
                out,
                "    <key>{key}</key>\n    <string>{}</string>",
                xml(&value)
            );
        }
        out.push_str("  </dict>\n");
    }
    out.push_str("</dict>\n</plist>\n");
    out
}

/// A value inside systemd double quotes: `\` and `"` escaped, and `%` and
/// `$` doubled (specifiers and variable expansion).
fn systemd_quoted(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A value taken whole to the end of the line (`StandardOutput=append:`):
/// only `%` needs care.
fn systemd_rest_of_line(s: &str) -> String {
    s.replace('%', "%%").replace('\n', " ")
}

/// The systemd user unit.
pub fn systemd_unit(spec: &Spec, senated: &Path) -> String {
    let log = systemd_rest_of_line(&text(&spec.log()));
    let mut out = String::new();
    out.push_str("[Unit]\nDescription=Senate daemon for this machine (senated --machine)\n\n");
    out.push_str("[Service]\nType=simple\n");
    let _ = writeln!(out, "LimitNOFILE={}", crate::fd::SERVICE_LIMIT);
    let _ = writeln!(
        out,
        "ExecStart={} --machine",
        systemd_quoted(&text(senated))
    );
    // The detached terminal hosts stay in the unit's cgroup. Only
    // senated should receive the service manager's stop signal.
    out.push_str("KillMode=process\nRestart=always\nRestartSec=5\n");
    // Room for `pause_on_shutdown` (90 s at most) before systemd kills it.
    let _ = writeln!(out, "TimeoutStopSec={STOP_TIMEOUT_SECS}");
    if let Some(path) = &spec.path {
        let _ = writeln!(
            out,
            "Environment={}",
            systemd_quoted(&format!("PATH={path}"))
        );
    }
    if let Some(home) = spec.home_override() {
        let _ = writeln!(
            out,
            "Environment={}",
            systemd_quoted(&format!("SENATE_HOME={}", text(home)))
        );
    }
    let _ = writeln!(out, "StandardOutput=append:{log}");
    let _ = writeln!(out, "StandardError=append:{log}");
    out.push_str("\n[Install]\nWantedBy=default.target\n");
    out
}

/// The file's contents for the platform.
pub fn service_file_contents(spec: &Spec, senated: &Path) -> String {
    match spec.platform {
        Platform::Macos => launchd_plist(spec, senated),
        Platform::Linux => systemd_unit(spec, senated),
    }
}

/// The `PATH` a service file written by [`service_file_contents`] gives
/// the daemon; `None` when it sets none (the manager's default applies).
pub fn path_in_file(platform: Platform, contents: &str) -> Option<String> {
    match platform {
        Platform::Macos => {
            let mut lines = contents.lines().map(str::trim);
            lines.find(|l| *l == "<key>PATH</key>")?;
            let value = lines
                .next()?
                .strip_prefix("<string>")?
                .strip_suffix("</string>")?;
            Some(
                value
                    .replace("&quot;", "\"")
                    .replace("&apos;", "'")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&amp;", "&"),
            )
        }
        Platform::Linux => contents.lines().find_map(|line| {
            let quoted = line
                .trim()
                .strip_prefix("Environment=\"")?
                .strip_suffix('"')?;
            let mut out = String::new();
            let mut chars = quoted.chars().peekable();
            while let Some(c) = chars.next() {
                match (c, chars.peek()) {
                    ('\\', Some(&n)) => {
                        out.push(if n == 'n' { '\n' } else { n });
                        chars.next();
                    }
                    ('%', Some('%')) | ('$', Some('$')) => {
                        out.push(c);
                        chars.next();
                    }
                    _ => out.push(c),
                }
            }
            out.strip_prefix("PATH=").map(str::to_owned)
        }),
    }
}

/// One command to hand to the service manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// A failure is expected and fine (stopping what is not running).
    pub may_fail: bool,
}

impl Cmd {
    fn new(program: &str, args: &[&str], may_fail: bool) -> Self {
        Self {
            program: program.to_string(),
            args: args.iter().map(ToString::to_string).collect(),
            may_fail,
        }
    }

    /// As typed in a shell.
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(crate::install::shell_quote)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The commands that load the service after its file is written.
pub fn install_steps(spec: &Spec) -> Vec<Cmd> {
    let file = text(&spec.file());
    match spec.platform {
        Platform::Macos => vec![
            // A service that is already loaded would refuse to be loaded
            // again; unloading first makes install idempotent.
            Cmd::new("launchctl", &["bootout", &spec.target()], true),
            Cmd::new("launchctl", &["bootstrap", &spec.domain(), &file], false),
        ],
        Platform::Linux => vec![
            Cmd::new("systemctl", &["--user", "daemon-reload"], false),
            Cmd::new("systemctl", &["--user", "enable", "--now", UNIT], false),
            // The unit file may have changed under a running daemon.
            Cmd::new("systemctl", &["--user", "restart", UNIT], false),
        ],
    }
}

/// The commands that stop the service, before its file is removed.
pub fn uninstall_steps(spec: &Spec) -> Vec<Cmd> {
    match spec.platform {
        Platform::Macos => vec![Cmd::new("launchctl", &["bootout", &spec.target()], true)],
        Platform::Linux => vec![Cmd::new(
            "systemctl",
            &["--user", "disable", "--now", UNIT],
            true,
        )],
    }
}

/// What runs after the file is removed.
pub fn after_uninstall_steps(spec: &Spec) -> Vec<Cmd> {
    match spec.platform {
        Platform::Macos => Vec::new(),
        Platform::Linux => vec![Cmd::new("systemctl", &["--user", "daemon-reload"], true)],
    }
}

/// The one command that asks the manager about the service.
pub fn status_command(spec: &Spec) -> Cmd {
    match spec.platform {
        Platform::Macos => Cmd::new("launchctl", &["print", &spec.target()], true),
        Platform::Linux => Cmd::new(
            "systemctl",
            &[
                "--user",
                "show",
                UNIT,
                "--property=LoadState",
                "--property=ActiveState",
                "--property=Result",
                "--property=ExecMainStatus",
            ],
            true,
        ),
    }
}

/// What a command printed and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Ran {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The seam to the service manager. [`SystemManager`] runs the real
/// commands; tests give a fake that records them.
pub trait Manager {
    /// Run one command. `Err` when it could not be started at all.
    fn run(&self, cmd: &Cmd) -> io::Result<Ran>;
    /// Wait between tries; a fake returns at once.
    fn sleep(&self, d: Duration);
}

/// The real thing: runs `launchctl` or `systemctl`.
pub struct SystemManager;

impl Manager for SystemManager {
    fn run(&self, cmd: &Cmd) -> io::Result<Ran> {
        let out = std::process::Command::new(&cmd.program)
            .args(&cmd.args)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(Ran {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// How often, and how long, install waits for launchd to let go of the
/// old service after `bootout`, which returns before it has.
const BOOTOUT_POLL: Duration = Duration::from_millis(100);
const BOOTOUT_POLLS: usize = 50;
/// `bootstrap` sometimes fails with "5: Input/output error" though
/// nothing is wrong (seen right after a `bootout`); trying again a moment
/// later works.
const BOOTSTRAP_TRIES: usize = 3;
const BOOTSTRAP_PAUSE: Duration = Duration::from_secs(1);

fn output(r: &Ran) -> &str {
    if r.stderr.trim().is_empty() {
        r.stdout.trim()
    } else {
        r.stderr.trim()
    }
}

/// launchd's transient `bootstrap` failure.
fn io_error_5(r: &Ran) -> bool {
    output(r).contains("5: Input/output error")
}

/// Whether `launchctl print` shows the service running `program`.
fn launchd_runs(manager: &dyn Manager, target: &str, program: &Path) -> bool {
    let Ok(r) = manager.run(&Cmd::new("launchctl", &["print", target], true)) else {
        return false;
    };
    let want = text(program);
    r.ok && r.stdout.lines().any(|l| {
        l.trim()
            .strip_prefix("program = ")
            .is_some_and(|p| p.trim() == want)
    }) && r
        .stdout
        .lines()
        .any(|l| l.trim() == "state = running" || l.trim().starts_with("pid = "))
}

/// Load the launchd service: `bootout`, wait until the old one is gone,
/// then `bootstrap`, tried again on launchd's transient error 5. When the
/// tries run out the label may be loaded all the same: if it runs
/// `program`, that is success; otherwise `kickstart -k` gets it going.
fn load_launchd(
    manager: &dyn Manager,
    bootout: &Cmd,
    bootstrap: &Cmd,
    target: &str,
    program: &Path,
) -> anyhow::Result<()> {
    run_all(manager, std::slice::from_ref(bootout))?;
    let print = Cmd::new("launchctl", &["print", target], true);
    for _ in 0..BOOTOUT_POLLS {
        match manager.run(&print) {
            Ok(r) if r.ok => manager.sleep(BOOTOUT_POLL),
            _ => break,
        }
    }
    let mut last = String::new();
    for attempt in 1..=BOOTSTRAP_TRIES {
        let r = match manager.run(bootstrap) {
            Ok(r) if r.ok => return Ok(()),
            Ok(r) => r,
            Err(e) => anyhow::bail!("could not run `{}`: {e}", bootstrap.display()),
        };
        last = output(&r).to_owned();
        if !io_error_5(&r) {
            anyhow::bail!("`{}` failed: {last}", bootstrap.display());
        }
        tracing::debug!("{} (try {attempt}): {last}", bootstrap.display());
        if attempt < BOOTSTRAP_TRIES {
            manager.sleep(BOOTSTRAP_PAUSE);
        }
    }
    if launchd_runs(manager, target, program) {
        return Ok(());
    }
    let kickstart = Cmd::new("launchctl", &["kickstart", "-k", target], false);
    match manager.run(&kickstart) {
        Ok(r) if r.ok => Ok(()),
        Ok(r) => anyhow::bail!(
            "`{}` failed {BOOTSTRAP_TRIES} times ({last}), and `{}` failed too ({}). \
             Run them by hand, or log out and back in, then `senate setup service install`",
            bootstrap.display(),
            kickstart.display(),
            output(&r)
        ),
        Err(e) => anyhow::bail!(
            "`{}` failed {BOOTSTRAP_TRIES} times ({last}), and `{}` could not run: {e}",
            bootstrap.display(),
            kickstart.display()
        ),
    }
}

fn run_all(manager: &dyn Manager, steps: &[Cmd]) -> anyhow::Result<()> {
    for step in steps {
        match manager.run(step) {
            Ok(r) if r.ok || step.may_fail => {}
            Ok(r) => anyhow::bail!("`{}` failed: {}", step.display(), output(&r)),
            Err(e) if step.may_fail => {
                tracing::debug!("{}: {e}", step.display());
            }
            Err(e) => anyhow::bail!("could not run `{}`: {e}", step.display()),
        }
    }
    Ok(())
}

/// Where `senated` is: next to the running `senate` (as installed, and as
/// the symlink target when `senate` is a link), else on `PATH`.
pub fn locate_senated(current_exe: &Path, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = current_exe.parent() {
        dirs.push(dir.to_path_buf());
    }
    if let Some(dir) = std::fs::canonicalize(current_exe)
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        dirs.push(dir);
    }
    if let Some(path) = path_var {
        dirs.extend(std::env::split_paths(path));
    }
    dirs.into_iter()
        .map(|d| d.join("senated"))
        .find(|p| executable(p))
}

/// A coding agent the daemon starts, and the config key its program is
/// read from.
struct Agent {
    name: &'static str,
    table: &'static str,
    key: &'static str,
}

const AGENTS: [Agent; 3] = [
    Agent {
        name: "claude",
        table: "companion",
        key: "claude_bin",
    },
    Agent {
        name: "codex",
        table: "codex",
        key: "bin",
    },
    Agent {
        name: "agy",
        table: "agy",
        key: "bin",
    },
];

/// Where a coding agent's program is, as `install` finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    /// Already configured as an absolute path: left as it is.
    Configured(PathBuf),
    /// Found on the installing shell's `PATH`: to be recorded.
    Resolved(PathBuf),
    /// Not on the installing shell's `PATH`.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProgram {
    /// `claude`, `codex` or `agy`.
    pub name: &'static str,
    /// `[companion] claude_bin`, `[codex] bin` or `[agy] bin`.
    pub setting: String,
    pub found: Found,
}

/// One program the service gets its own copy of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramCopy {
    pub from: PathBuf,
    pub to: PathBuf,
}

impl ProgramCopy {
    /// Whether the copy has the same bytes as what it is copied from.
    pub fn is_current(&self) -> bool {
        match (std::fs::read(&self.from), std::fs::read(&self.to)) {
            (Ok(from), Ok(to)) => from == to,
            _ => false,
        }
    }
}

/// The programs the service gets copies of: `senated`, and the `senate`
/// beside it, which the daemon starts hosted terminals with
/// (`current_exe().with_file_name("senate")`).
pub fn program_copies(spec: &Spec, senated: &Path) -> Vec<ProgramCopy> {
    let dir = spec.bin_dir();
    let mut copies = vec![ProgramCopy {
        from: senated.to_path_buf(),
        to: dir.join("senated"),
    }];
    let senate = senated.with_file_name("senate");
    if senate.is_file() {
        copies.push(ProgramCopy {
            from: senate,
            to: dir.join("senate"),
        });
    }
    copies
        .into_iter()
        .filter(|copy| copy.from != copy.to)
        .collect()
}

/// Copy `from` to a new file beside `to` and rename it into place, so
/// whatever runs from the old file keeps it, and `to` is never seen half
/// written.
fn replace_program(copy: &ProgramCopy) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let dir = copy
        .to
        .parent()
        .context("a program copy has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = copy
        .to
        .file_name()
        .context("a program copy has no name")?
        .to_string_lossy();
    let staged = dir.join(format!(".{name}.{}.new", std::process::id()));
    let written = (|| -> anyhow::Result<()> {
        let bytes = std::fs::read(&copy.from)
            .with_context(|| format!("reading {}", copy.from.display()))?;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(&staged)
            .with_context(|| format!("writing {}", staged.display()))?;
        file.write_all(&bytes)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        file.sync_all()?;
        std::fs::rename(&staged, &copy.to)
            .with_context(|| format!("replacing {}", copy.to.display()))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

/// What `<program> --version` prints, its last word (`senated 0.5.0` →
/// `0.5.0`); `None` when it does not run.
pub fn program_version(program: &Path) -> Option<String> {
    let out = std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .last()
        .map(str::to_owned)
}

/// The service's copies that are not what `install` would copy now: an
/// upgrade since the last install has not reached the service.
pub fn stale_copies(spec: &Spec, senated: &Path) -> Vec<ProgramCopy> {
    program_copies(spec, senated)
        .into_iter()
        .filter(|copy| !copy.is_current())
        .collect()
}

/// Whether the installed service runs Senate's copy of the programs.
pub fn runs_managed_copy(spec: &Spec) -> bool {
    std::fs::read_to_string(spec.file())
        .is_ok_and(|file| file.contains(&*text(&spec.managed_senated())))
}

/// What to tell someone who upgraded Senate while the service runs an
/// older copy: `None` when there is no service, or its copy is current.
pub fn upgrade_note(spec: &Spec, senated: &Path, l: Locale) -> Option<String> {
    if !runs_managed_copy(spec) {
        return None;
    }
    let copy = stale_copies(spec, senated).into_iter().next()?;
    let versions = program_version(&copy.to).zip(program_version(&copy.from));
    Some(match versions {
        Some((running, found)) if running != found => l.pick(
            format!(
                "Senate 服务运行 {running}，已安装的是 {found}：运行 `senate setup service install --yes` 来更新它。"
            ),
            format!(
                "The Senate service runs {running}, {found} is installed: run `senate setup service install --yes` to update it."
            ),
        ),
        _ => l.pick(
            format!(
                "Senate 服务运行的副本与 {} 不同：运行 `senate setup service install --yes` 来更新它。",
                copy.from.display()
            ),
            format!(
                "The Senate service runs a copy that differs from {}: run `senate setup service install --yes` to update it.",
                copy.from.display()
            ),
        ),
    })
}

/// What `install` would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    /// The programs copied into Senate's own `bin/` for the service.
    pub copies: Vec<ProgramCopy>,
    pub file: PathBuf,
    pub contents: String,
    /// Whether the file is already there with these contents.
    pub unchanged: bool,
    pub steps: Vec<Cmd>,
    pub platform: Platform,
    /// launchd's `gui/<uid>/<label>`.
    pub target: String,
    /// The program the service file starts.
    pub program: PathBuf,
    pub log: PathBuf,
    /// The Senate config file the programs are recorded in.
    pub config: PathBuf,
    pub programs: Vec<AgentProgram>,
}

/// Work out the install. Reads only.
pub fn plan_install(spec: &Spec, senated: &Path) -> InstallPlan {
    let contents = service_file_contents(spec, &spec.managed_senated());
    let file = spec.file();
    InstallPlan {
        copies: program_copies(spec, senated),
        unchanged: std::fs::read_to_string(&file).is_ok_and(|t| t == contents),
        file,
        contents,
        steps: install_steps(spec),
        platform: spec.platform,
        target: spec.target(),
        program: spec.managed_senated(),
        log: spec.log(),
        config: spec.config.clone(),
        programs: agent_programs(&spec.config, spec.path.as_deref()),
    }
}

/// Each coding agent's program: the absolute path configured for it, else
/// its configured name (or its own) looked up on `path`. A symlink is kept
/// as found: a shim may dispatch on the name it was started under.
pub fn agent_programs(config: &Path, path: Option<&str>) -> Vec<AgentProgram> {
    use std::os::unix::fs::PermissionsExt;
    let doc: toml_edit::DocumentMut = std::fs::read_to_string(config)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or_default();
    let executable = |p: &Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    AGENTS
        .iter()
        .map(|agent| {
            let configured = doc
                .get(agent.table)
                .and_then(|table| table.get(agent.key))
                .and_then(toml_edit::Item::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from);
            let found = match configured {
                Some(program) if program.is_absolute() => Found::Configured(program),
                configured => {
                    let program = configured.unwrap_or_else(|| PathBuf::from(agent.name));
                    path.into_iter()
                        .flat_map(std::env::split_paths)
                        .filter(|dir| dir.is_absolute())
                        .map(|dir| dir.join(&program))
                        .find(|candidate| executable(candidate))
                        .map_or(Found::Missing, Found::Resolved)
                }
            };
            AgentProgram {
                name: agent.name,
                setting: format!("[{}] {}", agent.table, agent.key),
                found,
            }
        })
        .collect()
}

/// Record each resolved program in the Senate config, keeping everything
/// else in it as it was.
fn record_programs(plan: &InstallPlan) -> anyhow::Result<()> {
    if !plan
        .programs
        .iter()
        .any(|p| matches!(p.found, Found::Resolved(_)))
    {
        return Ok(());
    }
    let before = match std::fs::read_to_string(&plan.config) {
        Ok(before) => before,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", plan.config.display())),
    };
    let mut doc: toml_edit::DocumentMut = before
        .parse()
        .with_context(|| format!("{} is not valid TOML", plan.config.display()))?;
    for (agent, program) in AGENTS.iter().zip(&plan.programs) {
        let Found::Resolved(path) = &program.found else {
            continue;
        };
        let table = doc
            .entry(agent.table)
            .or_insert_with(toml_edit::table)
            .as_table_like_mut()
            .with_context(|| {
                format!(
                    "[{}] in {} is not a table",
                    agent.table,
                    plan.config.display()
                )
            })?;
        table.insert(agent.key, toml_edit::value(text(path)));
    }
    if let Some(dir) = plan.config.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&plan.config, doc.to_string())
        .with_context(|| format!("writing {}", plan.config.display()))
}

/// Write the file and load the service.
pub fn apply_install(
    plan: &InstallPlan,
    senated: &Path,
    manager: &dyn Manager,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        senated.is_file(),
        "{} is not a file: pass the daemon's path with --senated",
        senated.display()
    );
    if let Some(dir) = plan.file.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // launchd and systemd open the log file themselves, in a directory that
    // must exist.
    if let Some(dir) = plan.log.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Each install makes new files: launchd (re)starts the daemon from a
    // file nothing has written into since it was signed.
    for copy in &plan.copies {
        replace_program(copy)?;
    }
    std::fs::write(&plan.file, &plan.contents)
        .with_context(|| format!("writing {}", plan.file.display()))?;
    // Before the daemon (re)starts: it reads them when it starts.
    record_programs(plan)?;
    match (plan.platform, plan.steps.as_slice()) {
        (Platform::Macos, [bootout, bootstrap]) => {
            load_launchd(manager, bootout, bootstrap, &plan.target, &plan.program)
        }
        _ => run_all(manager, &plan.steps),
    }
}

/// What `uninstall` would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub file: PathBuf,
    pub exists: bool,
    pub stop: Vec<Cmd>,
    pub after: Vec<Cmd>,
}

pub fn plan_uninstall(spec: &Spec) -> UninstallPlan {
    UninstallPlan {
        file: spec.file(),
        exists: spec.file().exists(),
        stop: uninstall_steps(spec),
        after: after_uninstall_steps(spec),
    }
}

/// Stop the service and remove its file.
pub fn apply_uninstall(plan: &UninstallPlan, manager: &dyn Manager) -> anyhow::Result<()> {
    run_all(manager, &plan.stop)?;
    match std::fs::remove_file(&plan.file) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("removing {}", plan.file.display())),
    }
    run_all(manager, &plan.after)
}

/// What the service is like now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub platform: Platform,
    pub file: PathBuf,
    pub file_exists: bool,
    /// What the service manager says; `None` when it could not be asked.
    pub loaded: Option<bool>,
    pub running: Option<bool>,
    pub log: PathBuf,
    pub log_exists: bool,
    /// How the daemon last ended, when the manager says it ended badly.
    pub last_exit: Option<LastExit>,
}

/// How the service's daemon last ended, as the manager tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastExit {
    /// The manager's own words, one line.
    pub detail: String,
    /// macOS would not run the binary: it changed after it was signed,
    /// typically overwritten in place by an upgrade.
    pub codesigning: bool,
}

/// How the daemon last ended badly, from the status command's output; `None`
/// when it never ended, or ended cleanly. Pure.
pub fn parse_last_exit(platform: Platform, stdout: &str) -> Option<LastExit> {
    let value = |key: &str, sep: &str| {
        stdout.lines().find_map(|l| {
            l.trim()
                .strip_prefix(key)
                .and_then(|v| v.trim_start().strip_prefix(sep))
                .map(str::trim)
                .filter(|v| !v.is_empty())
        })
    };
    let parts: Vec<String> = match platform {
        Platform::Macos => [
            ("last exit code", value("last exit code", "=")),
            ("last exit reason", value("last exit reason", "=")),
            (
                "last terminating signal",
                value("last terminating signal", "="),
            ),
        ]
        .into_iter()
        .filter_map(|(key, v)| {
            v.filter(|v| !matches!(*v, "0" | "(never exited)"))
                .map(|v| format!("{key} = {v}"))
        })
        .collect(),
        Platform::Linux => {
            let result = value("Result", "=").filter(|r| *r != "success");
            let status = value("ExecMainStatus", "=").filter(|s| *s != "0");
            match (result, status) {
                (None, None) => Vec::new(),
                (result, status) => result
                    .map(|r| format!("Result={r}"))
                    .into_iter()
                    .chain(status.map(|s| format!("ExecMainStatus={s}")))
                    .collect(),
            }
        }
    };
    if parts.is_empty() {
        return None;
    }
    let detail = parts.join("; ");
    Some(LastExit {
        codesigning: detail.contains("CODESIGNING"),
        detail,
    })
}

/// Read what the manager reports. Pure given the command's output.
pub fn parse_status(spec: &Spec, ran: &io::Result<Ran>) -> (Option<bool>, Option<bool>) {
    let Ok(ran) = ran else {
        return (None, None);
    };
    match spec.platform {
        Platform::Macos => {
            if !ran.ok {
                return (Some(false), Some(false));
            }
            let running = ran
                .stdout
                .lines()
                .any(|l| l.trim() == "state = running" || l.trim().starts_with("pid = "));
            (Some(true), Some(running))
        }
        Platform::Linux => {
            let value = |key: &str| {
                ran.stdout
                    .lines()
                    .find_map(|l| l.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
                    .map(str::trim)
            };
            match (value("LoadState"), value("ActiveState")) {
                (Some(load), Some(active)) => (Some(load == "loaded"), Some(active == "active")),
                _ => (None, None),
            }
        }
    }
}

pub fn status(spec: &Spec, manager: &dyn Manager) -> Status {
    let ran = manager.run(&status_command(spec));
    let (loaded, running) = parse_status(spec, &ran);
    let last_exit = ran
        .as_ref()
        .ok()
        .filter(|ran| ran.ok)
        .and_then(|ran| parse_last_exit(spec.platform, &ran.stdout));
    Status {
        last_exit,
        platform: spec.platform,
        file: spec.file(),
        file_exists: spec.file().exists(),
        loaded,
        running,
        log: spec.log(),
        log_exists: spec.log().exists(),
    }
}

fn yes_no(l: Locale, v: bool) -> &'static str {
    l.pick(if v { "是" } else { "否" }, if v { "yes" } else { "no" })
}

/// What `senate setup service install` prints; `applied` once it was done.
pub fn render_install(plan: &InstallPlan, platform: Platform, applied: bool, l: Locale) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}",
        l.pick(
            format!("1. {}（{}）", plan.file.display(), platform.manager_name()),
            format!("1. {} ({})", plan.file.display(), platform.manager_name()),
        )
    );
    for copy in &plan.copies {
        let _ = writeln!(
            out,
            "   {}",
            l.pick(
                format!(
                    "{} → {}（{}）",
                    copy.from.display(),
                    copy.to.display(),
                    if applied { "已复制" } else { "将复制" }
                ),
                format!(
                    "{} → {} ({})",
                    copy.from.display(),
                    copy.to.display(),
                    if applied { "copied" } else { "would copy" }
                ),
            )
        );
    }
    if !plan.copies.iter().any(|c| c.to.ends_with("senate")) {
        let _ = writeln!(
            out,
            "   {}",
            l.pick(
                "⚠ senated 旁边没有 senate：守护进程无法启动托管终端，直到 senate 与 senated 装在同一目录",
                "⚠ no `senate` beside `senated`: the daemon cannot start hosted terminals until both are installed in one directory"
            )
        );
    }
    if plan.unchanged {
        let _ = writeln!(out, "   {}", l.pick("内容已是最新", "already up to date"));
    } else {
        let _ = writeln!(
            out,
            "   {}",
            l.pick(
                if applied {
                    "已写入："
                } else {
                    "将写入："
                },
                if applied { "written:" } else { "would write:" }
            )
        );
        for line in plan.contents.lines() {
            let _ = writeln!(out, "     {line}");
        }
    }
    let _ = writeln!(
        out,
        "\n2. {}",
        l.pick(
            if applied {
                "已运行："
            } else {
                "将运行："
            },
            if applied { "ran:" } else { "would run:" }
        )
    );
    for step in &plan.steps {
        let _ = writeln!(out, "   {}", step.display());
    }
    let _ = writeln!(
        out,
        "\n3. {}",
        l.pick(
            format!("编码代理，记录在 {}：", plan.config.display()),
            format!("coding agents, recorded in {}:", plan.config.display()),
        )
    );
    for program in &plan.programs {
        let line = match &program.found {
            Found::Configured(path) => l.pick(
                format!("{}：{}（已配置，保持不变）", program.name, path.display()),
                format!(
                    "{}: {} (already configured, kept)",
                    program.name,
                    path.display()
                ),
            ),
            Found::Resolved(path) => l.pick(
                format!(
                    "{}：{}（{} {}）",
                    program.name,
                    path.display(),
                    program.setting,
                    if applied { "已记录" } else { "将记录" }
                ),
                format!(
                    "{}: {} ({} as {})",
                    program.name,
                    path.display(),
                    if applied {
                        "recorded"
                    } else {
                        "would be recorded"
                    },
                    program.setting
                ),
            ),
            Found::Missing => l.pick(
                format!(
                    "{}：不在 PATH 上；设置 {} 之前，`senate spawn {}` 会失败",
                    program.name, program.setting, program.name
                ),
                format!(
                    "{}: not found on PATH; `senate spawn {}` will fail until {} is set",
                    program.name, program.name, program.setting
                ),
            ),
        };
        let _ = writeln!(out, "   {line}");
        if let Found::Configured(path) = &program.found
            && crate::wsl::distro().is_some()
            && crate::wsl::is_windows_path(path)
        {
            let _ = writeln!(
                out,
                "   {}",
                l.pick(
                    format!(
                        "⚠ 这是 Windows 程序：在 WSL 里请把 {} 改成发行版内的 Linux {}",
                        program.setting, program.name
                    ),
                    format!(
                        "⚠ that is a Windows program: in WSL, set {} to the Linux {} inside the distro",
                        program.setting, program.name
                    ),
                )
            );
        }
    }
    let _ = writeln!(out, "\n{}: {}", l.pick("日志", "log"), plan.log.display());
    if platform == Platform::Linux {
        let _ = writeln!(
            out,
            "{}",
            l.pick(
                "systemd 的用户服务只在你登录时运行；要开机即运行，请执行 `loginctl enable-linger`。",
                "A systemd user service runs only while you are logged in; for one that \
                 runs from boot, run `loginctl enable-linger`."
            )
        );
    }
    if !applied {
        let _ = writeln!(
            out,
            "\n{}",
            l.pick(
                "这只是预演，什么都没有改动。加 --yes 才会执行。",
                "This is a dry run; nothing was changed. Add --yes to do it."
            )
        );
    }
    out
}

pub fn render_uninstall(plan: &UninstallPlan, applied: bool, l: Locale) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "1. {}",
        l.pick(
            if applied {
                "已运行："
            } else {
                "将运行："
            },
            if applied { "ran:" } else { "would run:" }
        )
    );
    for step in plan.stop.iter().chain(&plan.after) {
        let _ = writeln!(out, "   {}", step.display());
    }
    let _ = writeln!(
        out,
        "\n2. {} {}",
        l.pick(
            if applied { "已删除" } else { "将删除" },
            if applied { "removed" } else { "would remove" }
        ),
        plan.file.display()
    );
    if !plan.exists {
        let _ = writeln!(
            out,
            "   {}",
            l.pick("（它本来就不存在）", "(it was not there)")
        );
    }
    if !applied {
        let _ = writeln!(
            out,
            "\n{}",
            l.pick(
                "这只是预演，什么都没有改动。加 --yes 才会执行。",
                "This is a dry run; nothing was changed. Add --yes to do it."
            )
        );
    }
    out
}

/// The everyday `senate setup service status`: whether it runs, and its log.
pub fn render_status_brief(
    s: &Status,
    l: Locale,
    terminal: Option<crate::terminal::Terminal>,
) -> String {
    let manager = s.platform.manager_name();
    let state = match (s.file_exists, s.loaded, s.running) {
        (_, _, Some(true)) => l.pick(
            format!("运行中（{manager}，登录时启动）"),
            format!("running ({manager}, starts at login)"),
        ),
        (false, Some(false) | None, _) => l
            .pick(
                "未安装 · senate setup service install --yes",
                "not installed · senate setup service install --yes",
            )
            .to_owned(),
        (_, Some(true), Some(false)) => l.pick(
            format!("已加载但未运行（{manager}）· 查看日志"),
            format!("loaded but not running ({manager}) · see the log"),
        ),
        (true, Some(false), _) => l
            .pick(
                "已安装但未加载 · senate setup service install --yes",
                "installed but not loaded · senate setup service install --yes",
            )
            .to_owned(),
        _ => l.pick(
            format!("无法查询 {manager}"),
            format!("{manager} could not be asked"),
        ),
    };
    let log = if s.log_exists {
        crate::status::home_relative(&s.log)
    } else {
        l.pick(
            format!("{}（还没有）", crate::status::home_relative(&s.log)),
            format!("{} (not yet)", crate::status::home_relative(&s.log)),
        )
    };
    crate::terminal::fields(
        terminal,
        &[
            (l.pick("服务", "Service"), state),
            (l.pick("日志", "Log"), log),
        ],
    )
}

pub fn render_status(s: &Status, l: Locale) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {}  ({})",
        l.pick("服务文件", "service file"),
        s.file.display(),
        l.pick(
            if s.file_exists { "存在" } else { "不存在" },
            if s.file_exists { "exists" } else { "missing" }
        )
    );
    let manager = match (s.loaded, s.running) {
        (Some(loaded), Some(running)) => format!(
            "{}: {} {}, {} {}",
            s.platform.manager_name(),
            l.pick("已加载", "loaded"),
            yes_no(l, loaded),
            l.pick("运行中", "running"),
            yes_no(l, running)
        ),
        _ => format!(
            "{}: {}",
            s.platform.manager_name(),
            l.pick("无法查询", "could not be asked")
        ),
    };
    let _ = writeln!(out, "{} {manager}", l.pick("服务管理器", "service manager"));
    if let Some(exit) = &s.last_exit {
        let _ = writeln!(out, "{} {}", l.pick("上次退出", "last exit"), exit.detail);
    }
    let _ = writeln!(
        out,
        "{} {}  ({})",
        l.pick("日志", "log"),
        s.log.display(),
        l.pick(
            if s.log_exists { "存在" } else { "还没有" },
            if s.log_exists { "exists" } else { "not yet" }
        )
    );
    if !s.file_exists && s.loaded != Some(true) {
        let _ = writeln!(
            out,
            "{}",
            l.pick(
                "没有安装：`senate setup service install --yes`。",
                "Not installed: `senate setup service install --yes`."
            )
        );
    }
    out
}

/// The current user's numeric id.
#[allow(unsafe_code)]
pub fn current_uid() -> u32 {
    // SAFETY: getuid(2) takes no arguments, cannot fail and touches no
    // memory of ours.
    unsafe { libc::getuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn the_brief_status_says_whether_it_runs_and_what_to_do_when_it_does_not() {
        let status = |file_exists, loaded, running| Status {
            platform: Platform::Macos,
            file: "/nowhere/dev.senate.senated.plist".into(),
            file_exists,
            loaded,
            running,
            log: "/nowhere/senated.log".into(),
            log_exists: false,
            last_exit: None,
        };
        let brief = |s: &Status| render_status_brief(s, Locale::En, None);
        assert_eq!(
            brief(&status(true, Some(true), Some(true))),
            "Service  running (launchd, starts at login)\nLog      /nowhere/senated.log (not yet)\n"
        );
        assert!(
            brief(&status(false, Some(false), Some(false)))
                .contains("not installed · senate setup service install --yes")
        );
        assert!(brief(&status(true, Some(true), Some(false))).contains("loaded but not running"));
        assert!(brief(&status(true, None, None)).contains("could not be asked"));
        let zh = render_status_brief(&status(true, Some(true), Some(true)), Locale::Zh, None);
        assert!(zh.contains("运行中"), "{zh}");
    }

    fn spec(platform: Platform, user: &str, home: &str) -> Spec {
        Spec {
            platform,
            user_home: user.into(),
            senate_home: home.into(),
            uid: 501,
            path: None,
            config: Path::new(home).join("config.toml"),
        }
    }

    #[test]
    fn the_launchd_plist_for_the_default_home() {
        let s = spec(Platform::Macos, "/Users/alice", "/Users/alice/.senate");
        let plist = launchd_plist(&s, Path::new("/Users/alice/.cargo/bin/senated"));
        assert_eq!(
            plist,
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>dev.senate.senated</string>
  <key>ProgramArguments</key>
  <array>
    <string>/Users/alice/.cargo/bin/senated</string>
    <string>--machine</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>SoftResourceLimits</key>
  <dict>
    <key>NumberOfFiles</key>
    <integer>10240</integer>
  </dict>
  <key>HardResourceLimits</key>
  <dict>
    <key>NumberOfFiles</key>
    <integer>10240</integer>
  </dict>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>ExitTimeOut</key>
  <integer>120</integer>
  <key>StandardOutPath</key>
  <string>/Users/alice/.senate/senated.log</string>
  <key>StandardErrorPath</key>
  <string>/Users/alice/.senate/senated.log</string>
</dict>
</plist>
"#
        );
        assert_eq!(
            s.file(),
            PathBuf::from("/Users/alice/Library/LaunchAgents/dev.senate.senated.plist")
        );
    }

    #[test]
    fn a_non_default_home_is_passed_and_paths_with_spaces_and_markup_survive() {
        let s = spec(
            Platform::Macos,
            "/Users/al ice",
            "/Volumes/Work & Play/<senate>",
        );
        let plist = launchd_plist(&s, Path::new("/Applications/My Tools/senated"));
        assert!(plist.contains("<string>/Applications/My Tools/senated</string>"));
        assert!(plist.contains(
            "<key>SENATE_HOME</key>\n    <string>/Volumes/Work &amp; Play/&lt;senate&gt;</string>"
        ));
        assert!(plist.contains(
            "<key>StandardOutPath</key>\n  <string>/Volumes/Work &amp; Play/&lt;senate&gt;/senated.log</string>"
        ));
        // The default home passes nothing.
        let default = spec(Platform::Macos, "/Users/al ice", "/Users/al ice/.senate");
        assert!(!launchd_plist(&default, Path::new("/x/senated")).contains("SENATE_HOME"));
    }

    #[test]
    fn the_systemd_unit_for_the_default_home() {
        let s = spec(Platform::Linux, "/home/alice", "/home/alice/.senate");
        let unit = systemd_unit(&s, Path::new("/home/alice/.cargo/bin/senated"));
        assert_eq!(
            unit,
            "[Unit]\n\
             Description=Senate daemon for this machine (senated --machine)\n\
             \n\
             [Service]\n\
             Type=simple\n\
             LimitNOFILE=10240\n\
             ExecStart=\"/home/alice/.cargo/bin/senated\" --machine\n\
             KillMode=process\n\
             Restart=always\n\
             RestartSec=5\n\
             TimeoutStopSec=120\n\
             StandardOutput=append:/home/alice/.senate/senated.log\n\
             StandardError=append:/home/alice/.senate/senated.log\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        );
        assert_eq!(
            s.file(),
            PathBuf::from("/home/alice/.config/systemd/user/senated.service")
        );
    }

    #[test]
    fn systemd_quotes_spaces_and_specifiers_and_passes_a_non_default_home() {
        let s = spec(Platform::Linux, "/home/al ice", "/srv/sen ate/100%/$x");
        let unit = systemd_unit(&s, Path::new("/opt/my \"tools\"/senated"));
        assert!(
            unit.contains("ExecStart=\"/opt/my \\\"tools\\\"/senated\" --machine\n"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=\"SENATE_HOME=/srv/sen ate/100%%/$$x\"\n"),
            "{unit}"
        );
        assert!(
            unit.contains("StandardOutput=append:/srv/sen ate/100%%/$x/senated.log\n"),
            "{unit}"
        );
    }

    /// An executable that prints `name version` for `--version`.
    fn versioned(dir: &Path, name: &str, version: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\necho '{name} {version}'\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_service_runs_its_own_copy_replaced_by_a_new_file_each_install() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let s = temp_spec(Platform::Macos, dir.path());
        let cargo_bin = dir.path().join("cargo bin");
        let senated = versioned(&cargo_bin, "senated", "0.5.0");
        versioned(&cargo_bin, "senate", "0.5.0");
        let plan = plan_install(&s, &senated);
        assert!(plan.contents.contains(&*text(&s.managed_senated())));
        assert!(!plan.contents.contains("cargo bin"), "{}", plan.contents);
        assert_eq!(
            plan.copies.iter().map(|c| c.to.clone()).collect::<Vec<_>>(),
            [s.bin_dir().join("senated"), s.bin_dir().join("senate")]
        );
        apply_install(&plan, &senated, &Fake::default()).unwrap();
        let copy = s.managed_senated();
        assert_eq!(
            std::fs::read(&copy).unwrap(),
            std::fs::read(&senated).unwrap()
        );
        assert_eq!(program_version(&copy).as_deref(), Some("0.5.0"));
        assert!(s.bin_dir().join("senate").is_file());
        assert!(stale_copies(&s, &senated).is_empty());
        assert_eq!(upgrade_note(&s, &senated, Locale::En), None);
        let first = std::fs::metadata(&copy).unwrap().ino();

        // An upgrade overwrites the installed binary in place: the
        // service's copy is untouched, and says it is behind.
        std::fs::write(&senated, "#!/bin/sh\necho 'senated 0.5.1'\n").unwrap();
        assert_eq!(program_version(&copy).as_deref(), Some("0.5.0"));
        assert_eq!(stale_copies(&s, &senated).len(), 1);
        let note = upgrade_note(&s, &senated, Locale::En).unwrap();
        assert!(
            note.contains("runs 0.5.0, 0.5.1 is installed")
                && note.contains("senate setup service install --yes"),
            "{note}"
        );

        // The next install renames a new file into place.
        apply_install(&plan_install(&s, &senated), &senated, &Fake::default()).unwrap();
        assert_ne!(std::fs::metadata(&copy).unwrap().ino(), first);
        assert_eq!(program_version(&copy).as_deref(), Some("0.5.1"));
        assert!(stale_copies(&s, &senated).is_empty());
        let leftovers: Vec<_> = std::fs::read_dir(s.bin_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|n| n.to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn the_last_exit_is_read_from_the_manager() {
        let mac = "\tstate = not running\n\tlast exit code = 78: Function not implemented\n\tlast exit reason = OS_REASON_CODESIGNING\n";
        let exit = parse_last_exit(Platform::Macos, mac).unwrap();
        assert!(exit.codesigning, "{exit:?}");
        assert!(exit.detail.contains("last exit code = 78"), "{exit:?}");
        assert_eq!(
            parse_last_exit(Platform::Macos, "\tlast exit code = (never exited)\n"),
            None
        );
        assert_eq!(
            parse_last_exit(Platform::Macos, "\tlast exit code = 0\n"),
            None
        );
        let killed =
            parse_last_exit(Platform::Macos, "\tlast terminating signal = Killed: 9\n").unwrap();
        assert!(!killed.codesigning);
        assert_eq!(killed.detail, "last terminating signal = Killed: 9");
        assert_eq!(
            parse_last_exit(
                Platform::Linux,
                "LoadState=loaded\nActiveState=active\nResult=success\nExecMainStatus=0\n"
            ),
            None
        );
        assert_eq!(
            parse_last_exit(
                Platform::Linux,
                "ActiveState=failed\nResult=exit-code\nExecMainStatus=1\n"
            )
            .unwrap()
            .detail,
            "Result=exit-code; ExecMainStatus=1"
        );
    }

    #[test]
    fn the_commands_per_platform() {
        let mac = spec(Platform::Macos, "/Users/a", "/Users/a/.senate");
        let shown = |steps: Vec<Cmd>| steps.iter().map(Cmd::display).collect::<Vec<_>>();
        assert_eq!(
            shown(install_steps(&mac)),
            [
                "launchctl bootout gui/501/dev.senate.senated",
                "launchctl bootstrap gui/501 /Users/a/Library/LaunchAgents/dev.senate.senated.plist"
            ]
        );
        assert!(install_steps(&mac)[0].may_fail && !install_steps(&mac)[1].may_fail);
        assert_eq!(
            shown(uninstall_steps(&mac)),
            ["launchctl bootout gui/501/dev.senate.senated"]
        );
        assert_eq!(
            shown(vec![status_command(&mac)]),
            ["launchctl print gui/501/dev.senate.senated"]
        );
        let spaced = spec(Platform::Macos, "/Users/a b", "/Users/a b/.senate");
        assert_eq!(
            install_steps(&spaced)[1].display(),
            "launchctl bootstrap gui/501 '/Users/a b/Library/LaunchAgents/dev.senate.senated.plist'"
        );

        let linux = spec(Platform::Linux, "/home/a", "/home/a/.senate");
        assert_eq!(
            shown(install_steps(&linux)),
            [
                "systemctl --user daemon-reload",
                "systemctl --user enable --now senated.service",
                "systemctl --user restart senated.service"
            ]
        );
        assert_eq!(
            shown(uninstall_steps(&linux)),
            ["systemctl --user disable --now senated.service"]
        );
        assert_eq!(
            shown(after_uninstall_steps(&linux)),
            ["systemctl --user daemon-reload"]
        );
    }

    #[test]
    fn status_is_read_from_what_the_manager_prints() {
        let mac = spec(Platform::Macos, "/Users/a", "/Users/a/.senate");
        let ran = |ok: bool, out: &str| {
            Ok(Ran {
                ok,
                stdout: out.into(),
                stderr: String::new(),
            })
        };
        assert_eq!(
            parse_status(&mac, &ran(true, "x = {\n\tstate = running\n\tpid = 42\n}")),
            (Some(true), Some(true))
        );
        assert_eq!(
            parse_status(&mac, &ran(true, "x = {\n\tstate = waiting\n}")),
            (Some(true), Some(false))
        );
        assert_eq!(
            parse_status(&mac, &ran(false, "")),
            (Some(false), Some(false))
        );
        let none: io::Result<Ran> = Err(io::ErrorKind::NotFound.into());
        assert_eq!(parse_status(&mac, &none), (None, None));
        let linux = spec(Platform::Linux, "/home/a", "/home/a/.senate");
        assert_eq!(
            parse_status(&linux, &ran(true, "LoadState=loaded\nActiveState=active\n")),
            (Some(true), Some(true))
        );
        assert_eq!(
            parse_status(
                &linux,
                &ran(true, "LoadState=not-found\nActiveState=inactive\n")
            ),
            (Some(false), Some(false))
        );
    }

    #[test]
    fn senated_is_found_beside_senate_then_on_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (bin, other) = (dir.path().join("bin"), dir.path().join("other dir"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let make = |p: PathBuf| {
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let senate = make(bin.join("senate"));
        let path = std::env::join_paths([&other]).unwrap();
        assert_eq!(locate_senated(&senate, Some(&path)), None);
        let on_path = make(other.join("senated"));
        assert_eq!(locate_senated(&senate, Some(&path)), Some(on_path));
        let beside = make(bin.join("senated"));
        assert_eq!(locate_senated(&senate, Some(&path)), Some(beside));
        // Not executable: not it.
        std::fs::set_permissions(bin.join("senated"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert_ne!(
            locate_senated(&senate, Some(&path)),
            Some(bin.join("senated"))
        );
    }

    /// Records every command; nothing real is run.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<String>>,
        fail: Option<&'static str>,
        answer: Ran,
    }

    impl Manager for Fake {
        fn run(&self, cmd: &Cmd) -> io::Result<Ran> {
            self.calls.borrow_mut().push(cmd.display());
            if self.fail.is_some_and(|f| cmd.display().contains(f)) {
                return Ok(Ran {
                    ok: false,
                    stderr: "boom".into(),
                    ..Ran::default()
                });
            }
            Ok(Ran {
                ok: true,
                ..self.answer.clone()
            })
        }

        fn sleep(&self, _: Duration) {}
    }

    #[test]
    fn the_path_at_install_time_is_given_to_the_daemon() {
        let mut mac = spec(Platform::Macos, "/Users/alice", "/Users/alice/.senate");
        mac.path = Some("/Users/alice/.local/bin:/usr/bin & co".into());
        let plist = launchd_plist(&mac, Path::new("/x/senated"));
        assert!(
            plist.contains(
                "<key>PATH</key>\n    <string>/Users/alice/.local/bin:/usr/bin &amp; co</string>"
            ),
            "{plist}"
        );
        assert!(!plist.contains("SENATE_HOME"));

        let mut linux = spec(Platform::Linux, "/home/alice", "/srv/sen");
        linux.path = Some("/home/alice/.local/bin:/usr/bin".into());
        let unit = systemd_unit(&linux, Path::new("/x/senated"));
        assert!(
            unit.contains("Environment=\"PATH=/home/alice/.local/bin:/usr/bin\"\n"),
            "{unit}"
        );
        assert!(unit.contains("Environment=\"SENATE_HOME=/srv/sen\"\n"));

        let none = spec(Platform::Macos, "/Users/alice", "/Users/alice/.senate");
        assert!(!launchd_plist(&none, Path::new("/x/senated")).contains("EnvironmentVariables"));
    }

    #[test]
    fn the_path_a_service_file_gives_reads_back() {
        let odd = "/Users/alice/.local/bin:/opt/a & \"b\" <c>:/x/100%/$HOME\\y";
        for platform in [Platform::Macos, Platform::Linux] {
            let mut s = spec(platform, "/Users/alice", "/srv/sen");
            s.path = Some(odd.into());
            let file = service_file_contents(&s, Path::new("/x/senated"));
            assert_eq!(
                path_in_file(platform, &file).as_deref(),
                Some(odd),
                "{file}"
            );
            s.path = None;
            let file = service_file_contents(&s, Path::new("/x/senated"));
            assert_eq!(path_in_file(platform, &file), None, "{file}");
        }
    }

    fn temp_spec(platform: Platform, dir: &Path) -> Spec {
        Spec {
            platform,
            user_home: dir.join("user home"),
            senate_home: dir.join("user home/.senate"),
            uid: 501,
            path: None,
            config: dir.join("user home/.senate/config.toml"),
        }
    }

    #[test]
    fn install_writes_under_the_given_home_and_runs_the_steps_through_the_seam() {
        for platform in [Platform::Macos, Platform::Linux] {
            let dir = tempfile::tempdir().unwrap();
            let s = temp_spec(platform, dir.path());
            let senated = dir.path().join("bin dir/senated");
            std::fs::create_dir_all(senated.parent().unwrap()).unwrap();
            std::fs::write(&senated, "x").unwrap();
            let plan = plan_install(&s, &senated);
            assert!(!plan.unchanged);
            // Planning wrote nothing.
            assert!(!s.file().exists());
            // After bootout, launchd no longer knows the service.
            let fake = Fake {
                fail: Some("launchctl print"),
                ..Fake::default()
            };
            apply_install(&plan, &senated, &fake).unwrap();
            assert_eq!(std::fs::read_to_string(s.file()).unwrap(), plan.contents);
            assert!(s.senate_home.is_dir(), "the log's directory exists");
            let calls = fake.calls.borrow().clone();
            let mut want: Vec<_> = plan.steps.iter().map(Cmd::display).collect();
            if platform == Platform::Macos {
                want.insert(1, "launchctl print gui/501/dev.senate.senated".into());
            }
            assert_eq!(calls, want);
            // Again: the file is up to date.
            assert!(plan_install(&s, &senated).unchanged);
        }
    }

    #[test]
    fn install_records_the_coding_agents_found_on_the_installers_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("local bin");
        std::fs::create_dir_all(&bin).unwrap();
        let claude = bin.join("claude");
        std::fs::write(&claude, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Not executable: not a program.
        std::fs::write(bin.join("codex"), "").unwrap();
        let mut s = temp_spec(Platform::Macos, dir.path());
        s.path = Some(format!("relative:{}", bin.display()));
        let senated = dir.path().join("senated");
        std::fs::write(&senated, "x").unwrap();
        std::fs::create_dir_all(&s.senate_home).unwrap();
        std::fs::write(&s.config, "# mine\n[companion]\nenabled = false\n").unwrap();

        let plan = plan_install(&s, &senated);
        assert_eq!(plan.programs[0].found, Found::Resolved(claude.clone()));
        assert_eq!(plan.programs[1].found, Found::Missing);
        let dry = render_install(&plan, Platform::Macos, false, Locale::En);
        assert!(
            dry.contains(&format!(
                "claude: {} (would be recorded as [companion] claude_bin)",
                claude.display()
            )),
            "{dry}"
        );
        assert!(
            dry.contains(
                "codex: not found on PATH; `senate spawn codex` will fail until [codex] bin is set"
            ),
            "{dry}"
        );
        // The dry run wrote nothing.
        assert_eq!(
            std::fs::read_to_string(&s.config).unwrap(),
            "# mine\n[companion]\nenabled = false\n"
        );

        apply_install(&plan, &senated, &Fake::default()).unwrap();
        let written = std::fs::read_to_string(&s.config).unwrap();
        assert!(
            written.starts_with("# mine\n[companion]\nenabled = false\n"),
            "{written}"
        );
        let parsed: toml::Value = toml::from_str(&written).unwrap();
        assert_eq!(
            parsed["companion"]["claude_bin"].as_str(),
            Some(claude.to_str().unwrap())
        );
        assert!(parsed.get("codex").is_none(), "{written}");
        // The shell's PATH still reaches the service, as the fallback.
        assert!(plan.contents.contains("local bin"), "{}", plan.contents);

        // Recorded once, it is configured: kept, whatever PATH says next.
        s.path = None;
        let again = plan_install(&s, &senated);
        assert_eq!(again.programs[0].found, Found::Configured(claude.clone()));
        let text = render_install(&again, Platform::Macos, true, Locale::En);
        assert!(text.contains("(already configured, kept)"), "{text}");
        apply_install(&again, &senated, &Fake::default()).unwrap();
        assert_eq!(std::fs::read_to_string(&s.config).unwrap(), written);
    }

    #[test]
    fn a_failing_step_is_an_error_unless_it_may_fail() {
        let dir = tempfile::tempdir().unwrap();
        let s = temp_spec(Platform::Macos, dir.path());
        let senated = dir.path().join("senated");
        std::fs::write(&senated, "x").unwrap();
        let plan = plan_install(&s, &senated);
        // bootout may fail (nothing loaded); bootstrap may not.
        let fake = Fake {
            fail: Some("bootout"),
            ..Fake::default()
        };
        apply_install(&plan, &senated, &fake).unwrap();
        let fake = Fake {
            fail: Some("bootstrap"),
            ..Fake::default()
        };
        let err = apply_install(&plan, &senated, &fake)
            .unwrap_err()
            .to_string();
        assert!(err.contains("bootstrap") && err.contains("boom"), "{err}");
        // A missing daemon is refused before anything is written.
        let gone = dir.path().join("nothing");
        let fresh = temp_spec(Platform::Macos, &dir.path().join("fresh"));
        let plan = plan_install(&fresh, &gone);
        assert!(apply_install(&plan, &gone, &Fake::default()).is_err());
        assert!(!fresh.file().exists());
    }

    /// Answers each command from a script, in order, and counts the
    /// pauses.
    struct Scripted {
        script: RefCell<std::collections::VecDeque<(&'static str, bool, String)>>,
        calls: RefCell<Vec<String>>,
        slept: RefCell<Vec<Duration>>,
    }

    impl Scripted {
        fn new(script: Vec<(&'static str, bool, String)>) -> Self {
            Self {
                script: RefCell::new(script.into()),
                calls: RefCell::default(),
                slept: RefCell::default(),
            }
        }

        fn done(&self) {
            assert!(self.script.borrow().is_empty(), "left: {:?}", self.script);
        }
    }

    impl Manager for Scripted {
        fn run(&self, cmd: &Cmd) -> io::Result<Ran> {
            let shown = cmd.display();
            self.calls.borrow_mut().push(shown.clone());
            let (want, ok, out) = self
                .script
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| panic!("unscripted `{shown}`"));
            assert!(shown.starts_with(want), "ran `{shown}`, expected `{want}`");
            Ok(Ran {
                ok,
                stdout: if ok { out.clone() } else { String::new() },
                stderr: if ok { String::new() } else { out },
            })
        }

        fn sleep(&self, d: Duration) {
            self.slept.borrow_mut().push(d);
        }
    }

    const IO5: &str = "Bootstrap failed: 5: Input/output error";

    fn launchd_plan(dir: &Path) -> (InstallPlan, PathBuf) {
        let s = temp_spec(Platform::Macos, dir);
        let senated = dir.join("senated");
        std::fs::write(&senated, "x").unwrap();
        (plan_install(&s, &senated), senated)
    }

    fn gone() -> (&'static str, bool, String) {
        ("launchctl print", false, "Could not find service".into())
    }

    fn running(program: &Path) -> String {
        format!(
            "gui/501/dev.senate.senated = {{\n\tstate = running\n\tprogram = {}\n\tpid = 42\n}}\n",
            program.display()
        )
    }

    #[test]
    fn install_waits_for_bootout_to_finish_before_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, senated) = launchd_plan(dir.path());
        let fake = Scripted::new(vec![
            ("launchctl bootout", true, String::new()),
            ("launchctl print", true, "state = running".into()),
            ("launchctl print", true, "state = not running".into()),
            gone(),
            ("launchctl bootstrap", true, String::new()),
        ]);
        apply_install(&plan, &senated, &fake).unwrap();
        fake.done();
        assert_eq!(*fake.slept.borrow(), vec![BOOTOUT_POLL; 2]);
    }

    #[test]
    fn bootout_that_never_finishes_still_goes_on_to_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, senated) = launchd_plan(dir.path());
        let mut script = vec![("launchctl bootout", false, "No such process".into())];
        script.extend((0..BOOTOUT_POLLS).map(|_| ("launchctl print", true, String::new())));
        script.push(("launchctl bootstrap", true, String::new()));
        let fake = Scripted::new(script);
        apply_install(&plan, &senated, &fake).unwrap();
        fake.done();
    }

    #[test]
    fn bootstrap_error_5_is_tried_again_after_a_pause() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, senated) = launchd_plan(dir.path());
        let fake = Scripted::new(vec![
            ("launchctl bootout", true, String::new()),
            gone(),
            ("launchctl bootstrap", false, IO5.into()),
            ("launchctl bootstrap", true, String::new()),
        ]);
        apply_install(&plan, &senated, &fake).unwrap();
        fake.done();
        assert_eq!(*fake.slept.borrow(), vec![BOOTSTRAP_PAUSE]);
    }

    #[test]
    fn error_5_with_the_service_loaded_and_running_the_new_program_is_success() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, senated) = launchd_plan(dir.path());
        let mut script = vec![("launchctl bootout", true, String::new()), gone()];
        script.extend((0..BOOTSTRAP_TRIES).map(|_| ("launchctl bootstrap", false, IO5.into())));
        script.push(("launchctl print", true, running(&plan.program)));
        let fake = Scripted::new(script);
        apply_install(&plan, &senated, &fake).unwrap();
        fake.done();
        assert!(!fake.calls.borrow().iter().any(|c| c.contains("kickstart")));
    }

    #[test]
    fn error_5_every_time_ends_with_a_kickstart() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, senated) = launchd_plan(dir.path());
        let mut script = vec![("launchctl bootout", true, String::new()), gone()];
        script.extend((0..BOOTSTRAP_TRIES).map(|_| ("launchctl bootstrap", false, IO5.into())));
        // Loaded, but still the old program.
        script.push(("launchctl print", true, running(Path::new("/old/senated"))));
        script.push((
            "launchctl kickstart -k gui/501/dev.senate.senated",
            true,
            String::new(),
        ));
        let fake = Scripted::new(script);
        apply_install(&plan, &senated, &fake).unwrap();
        fake.done();
        assert_eq!(
            *fake.slept.borrow(),
            vec![BOOTSTRAP_PAUSE; BOOTSTRAP_TRIES - 1]
        );
    }

    #[test]
    fn when_the_kickstart_fails_too_the_error_names_both() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, senated) = launchd_plan(dir.path());
        let mut script = vec![("launchctl bootout", true, String::new()), gone()];
        script.extend((0..BOOTSTRAP_TRIES).map(|_| ("launchctl bootstrap", false, IO5.into())));
        script.push(gone());
        script.push((
            "launchctl kickstart",
            false,
            "Could not find service".into(),
        ));
        let fake = Scripted::new(script);
        let err = apply_install(&plan, &senated, &fake)
            .unwrap_err()
            .to_string();
        fake.done();
        assert!(err.contains("launchctl bootstrap gui/501"), "{err}");
        assert!(err.contains("failed 3 times"), "{err}");
        assert!(err.contains("5: Input/output error"), "{err}");
        assert!(err.contains("launchctl kickstart -k"), "{err}");
        assert!(err.contains("Could not find service"), "{err}");
    }

    #[test]
    fn another_bootstrap_error_fails_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let (plan, senated) = launchd_plan(dir.path());
        let fake = Scripted::new(vec![
            ("launchctl bootout", true, String::new()),
            gone(),
            (
                "launchctl bootstrap",
                false,
                "Bootstrap failed: 122: Path had bad ownership".into(),
            ),
        ]);
        let err = apply_install(&plan, &senated, &fake)
            .unwrap_err()
            .to_string();
        fake.done();
        assert!(err.contains("bad ownership"), "{err}");
        assert!(fake.slept.borrow().is_empty());
    }

    #[test]
    fn uninstall_stops_then_removes_and_a_missing_file_is_fine() {
        for platform in [Platform::Macos, Platform::Linux] {
            let dir = tempfile::tempdir().unwrap();
            let s = temp_spec(platform, dir.path());
            let senated = dir.path().join("senated");
            std::fs::write(&senated, "x").unwrap();
            apply_install(&plan_install(&s, &senated), &senated, &Fake::default()).unwrap();
            let plan = plan_uninstall(&s);
            assert!(plan.exists);
            let fake = Fake::default();
            apply_uninstall(&plan, &fake).unwrap();
            assert!(!s.file().exists());
            let calls = fake.calls.borrow().clone();
            let want: Vec<String> = plan
                .stop
                .iter()
                .chain(&plan.after)
                .map(Cmd::display)
                .collect();
            assert_eq!(calls, want);
            // Nothing there any more; stopping failures are ignored.
            let again = plan_uninstall(&s);
            assert!(!again.exists);
            let failing = Fake {
                fail: Some("o"),
                ..Fake::default()
            };
            apply_uninstall(&again, &failing).unwrap();
        }
    }

    #[test]
    fn status_asks_the_manager_once_and_reports_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let s = temp_spec(Platform::Linux, dir.path());
        let fake = Fake {
            answer: Ran {
                ok: true,
                stdout: "LoadState=loaded\nActiveState=active\n".into(),
                stderr: String::new(),
            },
            ..Fake::default()
        };
        let st = status(&s, &fake);
        assert_eq!(fake.calls.borrow().len(), 1);
        assert_eq!((st.loaded, st.running), (Some(true), Some(true)));
        assert!(!st.file_exists && !st.log_exists);
        let en = render_status(&st, Locale::En);
        assert!(
            en.contains("service file") && en.contains("missing"),
            "{en}"
        );
        assert!(
            en.contains("systemd (user): loaded yes, running yes"),
            "{en}"
        );
        assert!(en.contains("senated.log"), "{en}");
        let zh = render_status(&st, Locale::Zh);
        assert!(zh.contains("服务文件") && zh.contains("已加载 是"), "{zh}");
    }

    #[test]
    fn the_dry_run_says_so_in_both_languages() {
        let dir = tempfile::tempdir().unwrap();
        let s = temp_spec(Platform::Linux, dir.path());
        let senated = dir.path().join("senated");
        let plan = plan_install(&s, &senated);
        let en = render_install(&plan, Platform::Linux, false, Locale::En);
        assert!(
            en.contains("would write:") && en.contains("would run:"),
            "{en}"
        );
        assert!(
            en.contains("Add --yes") && en.contains("enable-linger"),
            "{en}"
        );
        let zh = render_install(&plan, Platform::Linux, false, Locale::Zh);
        assert!(zh.contains("将写入：") && zh.contains("加 --yes"), "{zh}");
        let done = render_install(&plan, Platform::Linux, true, Locale::En);
        assert!(
            done.contains("written:") && !done.contains("dry run"),
            "{done}"
        );
        let un = render_uninstall(&plan_uninstall(&s), false, Locale::En);
        assert!(
            un.contains("would remove") && un.contains("it was not there"),
            "{un}"
        );
    }
}
