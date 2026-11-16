//! `senate-bridge install [--yes]`: wire the bridge into Claude Code.
//!
//! 1. Register the MCP server `senate` at user scope through the `claude`
//!    CLI (`claude mcp add --scope user senate -- <abs exe> mcp`).
//! 2. Merge three hooks into `<claude config dir>/settings.json`
//!    (SessionStart, UserPromptSubmit, SessionEnd → `<abs exe> hook …`),
//!    keeping every other key and hook.
//! 3. Set `"crossSessionInbound": "accept"` in the same file.
//! 4. Write the `/senate` slash command to `<claude config dir>/commands/senate.md`
//!    (`/senate inbox`, `/senate take <seq>`, `/senate join <committee>`). A
//!    `senate.md` that `install` did not write is left alone.
//!
//! 5. With Codex (`codex` on PATH, or `--codex`; `--no-codex` skips it):
//!    the same MCP server and hooks in `<codex home>/config.toml`
//!    ([`codex`]), and what to do once by hand (trust the hooks, run the
//!    managed app-server and `codex --remote`).
//!
//! What `install` prints, and the slash command it writes (which the agent
//! reads), are in the locale of `bridge.toml` (English when there is none
//! yet). The hooks and the MCP server are the same in every language.
//!
//! Without `--yes` it prints exactly what it would do and writes nothing.
//! Running it again changes nothing. The Claude config dir is `--claude-dir`,
//! else `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
//!
//! `settings.json` is rewritten with its keys in their original order
//! ([`order`]: recorded from the file and written back, at every depth; keys
//! `install` adds follow them). A
//! symlinked `settings.json` (a dotfiles checkout, say) stays a symlink: the
//! file it points to is replaced, atomically, with the same permission mode,
//! and both the file and its directory are synced before `install` reports
//! success. The previous contents are kept as `settings.json.bak` next to
//! the path Claude reads.

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use senate_core::Locale;
use serde_json::{Map, Value, json};

use crate::mcp::SERVER_NAME;

pub mod codex;
mod order;

use order::KeyOrder;

/// `(settings.json event name, hook subcommand)`.
pub const HOOKS: [(&str, &str); 3] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("SessionEnd", "session-end"),
];

/// Seconds Claude Code gives each hook before killing it.
pub(crate) const HOOK_TIMEOUT_SECS: u64 = 10;

fn why_cross_session(locale: Locale) -> &'static str {
    locale.pick(
        "为什么要设 \"crossSessionInbound\": \"accept\"：bridge daemon 把问题写进会话的 inbox \
         socket，但它不是那个会话的子进程，所以 Claude Code 把它的消息当作跨会话输入。不设 \
         \"accept\" 的话，这些消息可能会等你批准、5 分钟后被丢弃，队友的问题就会悄无声息地丢掉。",
        "Why \"crossSessionInbound\": \"accept\": the bridge daemon writes questions into a \
         session's inbox socket, but it is not a child process of that session, so Claude Code \
         treats its messages as cross-session input. Without \"accept\" they may be held for your \
         approval and dropped after 5 minutes, and a teammate's question would silently go \
         missing.",
    )
}

/// Marks a `commands/senate.md` as ours, so `install` may update it. The
/// same in every language, so a file written in one language is still
/// recognised and rewritten in another.
const COMMAND_MARKER: &str = "installed by senate-bridge";

/// The `/senate` slash command, for the agent to read, in `locale`.
pub fn command_md(locale: Locale) -> String {
    match locale {
        Locale::Zh => "---
description: Senate 收件箱、接管、加入和交接 (installed by senate-bridge)
argument-hint: inbox | take <seq> | join <committee> | handoff
---
Senate 命令：`$ARGUMENTS`

- `inbox`，或什么都不写：调用 mcp__senate__inbox，原样显示它的列表。
- `take <seq>`：用 thread=<seq> 调用 mcp__senate__take。然后告诉我这个 thread 现在归我了：\
它不会再交给任何 agent，由我自己在这里或在委员会的聊天里回答。
- `join <committee>`：用 committee=<committee> 调用 mcp__senate__join。
- `handoff`：我要离开一阵。先根据这个会话把三个 checkpoint 文件更新到最新：\
`.senate/PROGRESS.md`（已完成 / 进行中 / 下一步）、`.senate/DECISIONS.md`（本地的决定，\
每条注明来自哪个 Senate seq）和 `.senate/OPEN.md`（在等别人的问题，注明 thread seq）。\
然后调用 mcp__senate__checkpoint，push=true，附一行摘要，把三个文件的新内容作为 progress、\
decisions 和 open 传进去。告诉我它记录的分支和提交，如果推送被拒绝或失败，直说。

其他任何参数：说明 /senate 支持 inbox、take <seq>、join <committee> 和 handoff。

"
        .to_string(),
        Locale::En => "---
description: Senate inbox, take, join and handoff (installed by senate-bridge)
argument-hint: inbox | take <seq> | join <committee> | handoff
---
Senate command: `$ARGUMENTS`

- `inbox`, or nothing: call mcp__senate__inbox and show its list as it is.
- `take <seq>`: call mcp__senate__take with thread=<seq>. Then tell me the thread is mine now: \
nothing more of it goes to agents, and I answer it myself, here or in the committee's chat.
- `join <committee>`: call mcp__senate__join with committee=<committee>.
- `handoff`: I am about to step away. First bring the three checkpoint files up to date from \
this session: `.senate/PROGRESS.md` (done / in progress / next), `.senate/DECISIONS.md` (local \
decisions, each with the Senate seq it came from) and `.senate/OPEN.md` (questions waiting on \
others, with their thread seqs). Then call mcp__senate__checkpoint with push=true, a one-line \
summary, and the three files' new contents as progress, decisions and open. Tell me the branch \
and commit it recorded, and say plainly if the push was refused or failed.

For anything else, say that /senate supports inbox, take <seq>, join <committee> and handoff.

"
        .to_string(),
    }
}

/// What `install` does about the slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandPlan {
    Present,
    Write,
    /// A `senate.md` that is not ours.
    Foreign,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOptions {
    /// Absolute path of the `senate-bridge` binary to register.
    pub exe: PathBuf,
    /// Holds `settings.json`.
    pub claude_dir: PathBuf,
    /// Exported as `CLAUDE_CONFIG_DIR` to the `claude` CLI when the config
    /// dir is not the default.
    pub config_dir_env: Option<PathBuf>,
    /// Where Claude keeps user-scope MCP servers (`~/.claude.json`, or
    /// `<config dir>/.claude.json`). Read only, to see what is registered.
    pub user_config: PathBuf,
    /// The `claude` CLI.
    pub claude_bin: PathBuf,
    /// Codex's `config.toml`, when Codex is set up too.
    pub codex_config: Option<PathBuf>,
    /// The language of the report and of the slash command.
    pub locale: Locale,
}

/// Whether `install` sets up Codex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexChoice {
    /// When `codex` is on PATH.
    Auto,
    Yes,
    No,
}

/// `name` as an executable file in one of `$PATH`'s directories.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

impl InstallOptions {
    pub fn resolve(
        exe: PathBuf,
        claude_dir: Option<PathBuf>,
        claude_bin: Option<PathBuf>,
        codex: CodexChoice,
        codex_home: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let mut opts = Self::resolve_claude(exe, claude_dir, claude_bin)?;
        let wanted = match codex {
            CodexChoice::Yes => true,
            CodexChoice::No => false,
            CodexChoice::Auto => codex_home.is_some() || on_path("codex"),
        };
        if wanted {
            let home = match codex_home.or_else(|| {
                std::env::var_os("CODEX_HOME")
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            }) {
                Some(home) => home,
                None => dirs::home_dir()
                    .context("cannot find the home directory")?
                    .join(".codex"),
            };
            opts.codex_config = Some(home.join("config.toml"));
        }
        Ok(opts)
    }

    fn resolve_claude(
        exe: PathBuf,
        claude_dir: Option<PathBuf>,
        claude_bin: Option<PathBuf>,
    ) -> anyhow::Result<Self> {
        let claude_bin = claude_bin.unwrap_or_else(|| PathBuf::from("claude"));
        let override_dir = claude_dir.or_else(|| {
            std::env::var_os("CLAUDE_CONFIG_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        });
        Ok(match override_dir {
            Some(dir) => Self {
                exe,
                user_config: dir.join(".claude.json"),
                config_dir_env: Some(dir.clone()),
                claude_dir: dir,
                claude_bin,
                codex_config: None,
                locale: Locale::default(),
            },
            None => {
                let home = dirs::home_dir().context("cannot find the home directory")?;
                Self {
                    exe,
                    claude_dir: home.join(".claude"),
                    config_dir_env: None,
                    user_config: home.join(".claude.json"),
                    claude_bin,
                    codex_config: None,
                    locale: Locale::default(),
                }
            }
        })
    }

    /// Write the report and the slash command in `locale`.
    pub fn with_locale(mut self, locale: Locale) -> Self {
        self.locale = locale;
        self
    }

    pub fn command_path(&self) -> PathBuf {
        self.claude_dir.join("commands").join("senate.md")
    }

    pub fn settings_path(&self) -> PathBuf {
        self.claude_dir.join("settings.json")
    }

    fn exe_str(&self) -> String {
        self.exe.to_string_lossy().into_owned()
    }
}

/// POSIX-shell quoting, only when needed.
pub fn shell_quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:=@%,".contains(c));
    if plain {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn hook_command(exe: &str, subcommand: &str) -> String {
    format!("{} hook {subcommand}", shell_quote(exe))
}

/// A hook command an earlier install wrote, perhaps for another path.
fn is_ours(command: &str, subcommand: &str) -> bool {
    command.contains("senate-bridge") && command.trim_end().ends_with(&format!("hook {subcommand}"))
}

/// Merge the bridge's hooks and `crossSessionInbound` into `settings`.
/// Returns one line per change; empty when already up to date.
pub fn merge_settings(settings: &mut Value, exe: &str) -> Result<Vec<String>, String> {
    let obj = settings
        .as_object_mut()
        .ok_or("settings.json is not a JSON object")?;
    let mut changes = Vec::new();
    let hooks = obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or("`hooks` in settings.json is not an object")?;
    for (event, sub) in HOOKS {
        let want = hook_command(exe, sub);
        let groups = hooks
            .entry(event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| format!("`hooks.{event}` in settings.json is not an array"))?;
        let mut found = false;
        for group in groups.iter_mut() {
            let Some(list) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            for hook in list.iter_mut() {
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    continue;
                };
                let command = command.to_string();
                if command == want {
                    found = true;
                } else if is_ours(&command, sub) {
                    hook["command"] = Value::String(want.clone());
                    changes.push(format!("~ hooks.{event}: {command}  →  {want}"));
                    found = true;
                }
            }
        }
        if !found {
            groups.push(json!({
                "hooks": [{ "type": "command", "command": want, "timeout": HOOK_TIMEOUT_SECS }]
            }));
            changes.push(format!("+ hooks.{event}: {want}"));
        }
    }
    let current = obj.get("crossSessionInbound").cloned();
    if current.as_ref().and_then(Value::as_str) != Some("accept") {
        obj.insert("crossSessionInbound".into(), json!("accept"));
        changes.push(match current {
            Some(was) => format!("~ crossSessionInbound: {was}  →  \"accept\""),
            None => "+ crossSessionInbound: \"accept\"".into(),
        });
    }
    Ok(changes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpPlan {
    /// Registered with this exe already.
    Present,
    Add,
    /// Registered with another command, shown here.
    Replace {
        current: String,
    },
}

fn mcp_plan(opts: &InstallOptions) -> McpPlan {
    let Some(entry) = std::fs::read_to_string(&opts.user_config)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("mcpServers")?.get(SERVER_NAME).cloned())
    else {
        return McpPlan::Add;
    };
    let command = entry.get("command").and_then(Value::as_str).unwrap_or("");
    let args: Vec<&str> = entry
        .get("args")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if command == opts.exe_str() && args == ["mcp"] {
        McpPlan::Present
    } else {
        McpPlan::Replace {
            current: format!("{command} {}", args.join(" ")).trim().to_string(),
        }
    }
}

fn add_args(opts: &InstallOptions) -> Vec<String> {
    ["mcp", "add", "--scope", "user", SERVER_NAME, "--"]
        .iter()
        .map(|s| s.to_string())
        .chain([opts.exe_str(), "mcp".to_string()])
        .collect()
}

fn remove_args() -> Vec<String> {
    ["mcp", "remove", "--scope", "user", SERVER_NAME]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct InstallPlan {
    pub settings_path: PathBuf,
    pub settings_existed: bool,
    pub settings_after: Value,
    /// The key order `settings.json` had, kept when it is written back.
    pub settings_order: KeyOrder,
    pub settings_changes: Vec<String>,
    pub mcp: McpPlan,
    pub command: CommandPlan,
    pub codex: Option<codex::CodexPlan>,
}

impl InstallPlan {
    pub fn is_noop(&self) -> bool {
        self.settings_changes.is_empty()
            && self.mcp == McpPlan::Present
            && self.command != CommandPlan::Write
            && self.codex.as_ref().is_none_or(|c| c.changes.is_empty())
    }
}

fn command_plan(opts: &InstallOptions) -> CommandPlan {
    match std::fs::read_to_string(opts.command_path()) {
        Ok(text) if text == command_md(opts.locale) => CommandPlan::Present,
        Ok(text) if text.contains(COMMAND_MARKER) => CommandPlan::Write,
        Ok(_) => CommandPlan::Foreign,
        Err(_) => CommandPlan::Write,
    }
}

/// Work out the changes. Reads only.
pub fn plan(opts: &InstallOptions) -> anyhow::Result<InstallPlan> {
    let path = opts.settings_path();
    let (existed, mut settings, settings_order) = match std::fs::read_to_string(&path) {
        Ok(text) if text.trim().is_empty() => (true, json!({}), KeyOrder::default()),
        Ok(text) => (
            true,
            serde_json::from_str(&text).with_context(|| {
                format!(
                    "{} is not valid JSON; fix it first (nothing was changed)",
                    path.display()
                )
            })?,
            KeyOrder::of(&text),
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (false, json!({}), KeyOrder::default())
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let changes = merge_settings(&mut settings, &opts.exe_str()).map_err(anyhow::Error::msg)?;
    Ok(InstallPlan {
        settings_path: path,
        settings_existed: existed,
        settings_after: settings,
        settings_order,
        settings_changes: changes,
        mcp: mcp_plan(opts),
        command: command_plan(opts),
        codex: opts
            .codex_config
            .as_deref()
            .map(|path| codex::plan(path, &opts.exe_str()))
            .transpose()?,
    })
}

fn display_cli(opts: &InstallOptions, args: &[String]) -> String {
    let mut out = String::new();
    if let Some(dir) = &opts.config_dir_env {
        let _ = write!(
            out,
            "CLAUDE_CONFIG_DIR={} ",
            shell_quote(&dir.to_string_lossy())
        );
    }
    out.push_str(&shell_quote(&opts.claude_bin.to_string_lossy()));
    for a in args {
        out.push(' ');
        out.push_str(&shell_quote(a));
    }
    out
}

/// The report: what will change (or changed, when `applied`), in the
/// options' locale.
pub fn render(plan: &InstallPlan, opts: &InstallOptions, applied: bool) -> String {
    let l = opts.locale;
    let zh = l == Locale::Zh;
    let mut out = String::new();
    let verb = match (applied, zh) {
        (true, true) => "已运行",
        (false, true) => "将运行",
        (true, false) => "ran",
        (false, false) => "will run",
    };
    let _ = writeln!(
        out,
        "senate-bridge install{}",
        match (applied, zh) {
            (true, _) => "",
            (false, true) => "（演练：不加 --yes 什么都不写）",
            (false, false) => " (dry run: nothing is written without --yes)",
        }
    );
    let _ = writeln!(
        out,
        "\n1. {}",
        match l {
            Locale::Zh => format!("MCP server `{SERVER_NAME}`（用户级）"),
            Locale::En => format!("MCP server `{SERVER_NAME}` at user scope"),
        }
    );
    match &plan.mcp {
        McpPlan::Present => {
            let _ = writeln!(
                out,
                "   {}: {} mcp",
                l.pick("已注册", "already registered"),
                opts.exe.display()
            );
        }
        McpPlan::Add => {
            let _ = writeln!(out, "   {verb}: {}", display_cli(opts, &add_args(opts)));
        }
        McpPlan::Replace { current } => {
            let _ = match l {
                Locale::Zh => writeln!(out, "   当前注册为 `{current}`；替换它："),
                Locale::En => writeln!(out, "   registered as `{current}`; replacing it:"),
            };
            let _ = writeln!(out, "   {verb}: {}", display_cli(opts, &remove_args()));
            let _ = writeln!(out, "   {verb}: {}", display_cli(opts, &add_args(opts)));
        }
    }
    let _ = writeln!(
        out,
        "   {}",
        l.pick(
            "Claude Code 为每个会话启动 `senate-bridge mcp`；工具显示为 \
             mcp__senate__ask / answer / catch_up / join / search / post / status / escalate / …。\
             Claude Code 会话默认以 <person>/claude 的身份加入；SENATE_AGENT 环境变量或工作区里的 \
             .senate/agent 文件可以换一个名字。",
            "Claude Code starts `senate-bridge mcp` for each session; its tools appear as \
             mcp__senate__ask / answer / catch_up / join / search / post / status / escalate / …. \
             Claude Code sessions join as <person>/claude by default; SENATE_AGENT or a \
             .senate/agent file in the worktree picks another name."
        )
    );
    let _ = writeln!(out, "\n2. {}", plan.settings_path.display());
    if plan.settings_changes.is_empty() {
        let _ = writeln!(out, "   {}", l.pick("已是最新", "already up to date"));
    } else {
        for c in &plan.settings_changes {
            let _ = writeln!(out, "   {c}");
        }
        let excerpt = json!({
            "hooks": plan.settings_after.get("hooks").cloned().unwrap_or(Value::Null),
            "crossSessionInbound": plan.settings_after.get("crossSessionInbound").cloned().unwrap_or(Value::Null),
        });
        let _ = writeln!(
            out,
            "   {}",
            l.pick(
                "得到的 hooks 和 crossSessionInbound：",
                "Resulting hooks and crossSessionInbound:"
            )
        );
        for line in serde_json::to_string_pretty(&excerpt)
            .unwrap_or_default()
            .lines()
        {
            let _ = writeln!(out, "     {line}");
        }
        let _ = writeln!(
            out,
            "   {}{}",
            l.pick(
                "其他所有键和钩子都保留，顺序不变",
                "Every other key and hook is kept, in its original order"
            ),
            match (plan.settings_existed, zh) {
                (true, true) => "；原文件另存为 settings.json.bak。",
                (true, false) => "; the previous file is saved as settings.json.bak.",
                (false, true) => "。",
                (false, false) => ".",
            }
        );
    }
    let _ = writeln!(out, "\n3. {}", opts.command_path().display());
    let _ = match (&plan.command, zh) {
        (CommandPlan::Present, _) => {
            writeln!(out, "   {}", l.pick("已是最新", "already up to date"))
        }
        (CommandPlan::Write, true) => writeln!(
            out,
            "   {}：/senate 斜杠命令（inbox、take <seq>、join <committee>、handoff）",
            if applied { "已写入" } else { "将写入" }
        ),
        (CommandPlan::Write, false) => writeln!(
            out,
            "   {}: the /senate slash command (inbox, take <seq>, join <committee>, handoff)",
            if applied { "wrote" } else { "will write" }
        ),
        (CommandPlan::Foreign, true) => writeln!(
            out,
            "   保持不动：它已存在且不是 senate-bridge 写的，所以 /senate 归你自己"
        ),
        (CommandPlan::Foreign, false) => writeln!(
            out,
            "   left alone: it exists and senate-bridge did not write it, so /senate is yours"
        ),
    };
    match &plan.codex {
        Some(c) => {
            let _ = writeln!(out, "\n4. {} (Codex)", c.path.display());
            codex::render(c, &opts.exe_str(), applied, l, &mut out);
        }
        None => {
            let _ = writeln!(
                out,
                "\n4. {}",
                l.pick(
                    "Codex：未设置（`codex` 不在 PATH 上，或用了 --no-codex；--codex 会设置它）",
                    "Codex: not set up (`codex` is not on PATH, or --no-codex; --codex sets it up)"
                )
            );
        }
    }
    let _ = writeln!(out, "\n{}", why_cross_session(l));
    let _ = writeln!(
        out,
        "\n{}",
        if plan.is_noop() {
            l.pick(
                "无事可做：已经安装好了。",
                "Nothing to do: already installed.",
            )
        } else if applied {
            l.pick(
                "完成。用 `senate-bridge daemon` 启动 daemon（它读取 ~/.senate/bridge.toml）。",
                "Done. Start the daemon with `senate-bridge daemon` (it reads ~/.senate/bridge.toml).",
            )
        } else {
            l.pick(
                "什么都没写。加 --yes 重新运行以应用。",
                "Nothing was written. Re-run with --yes to apply.",
            )
        }
    );
    out
}

fn run_claude(opts: &InstallOptions, args: &[String]) -> anyhow::Result<()> {
    let mut cmd = Command::new(&opts.claude_bin);
    cmd.args(args);
    if let Some(dir) = &opts.config_dir_env {
        cmd.env("CLAUDE_CONFIG_DIR", dir);
    }
    let out = cmd
        .output()
        .with_context(|| format!("running {}", opts.claude_bin.display()))?;
    if !out.status.success() {
        bail!(
            "`{}` failed: {}",
            display_cli(opts, args),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The file `path` finally names, following symlinks (even to a target that
/// does not exist yet).
fn resolve_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&current)?;
                current = match current.parent() {
                    Some(dir) if target.is_relative() => dir.join(target),
                    _ => target,
                };
            }
            _ => return Ok(current),
        }
    }
    Err(io::Error::other(format!(
        "{}: too many levels of symbolic links",
        path.display()
    )))
}

/// Write `text` to `target` atomically: a temp file beside it with `mode`,
/// synced, renamed over it, then the directory synced.
fn replace_file(target: &Path, text: &str, mode: Option<u32>) -> io::Result<()> {
    let dir = target
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings.json".into());
    let tmp = dir.join(format!(".{name}.senate-tmp"));
    if std::fs::symlink_metadata(&tmp).is_ok() {
        std::fs::remove_file(&tmp)?;
    }
    let write = || -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        if let Some(mode) = mode {
            options.mode(mode);
        }
        let mut file = options.open(&tmp)?;
        if let Some(mode) = mode {
            // The umask may have narrowed the mode `open` used.
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, target)?;
        File::open(dir)?.sync_all()
    };
    let result = write();
    if result.is_err() && std::fs::symlink_metadata(&tmp).is_ok() {
        // Best effort: the error being returned is the one that matters.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn write_settings(
    path: &Path,
    existed: bool,
    value: &Value,
    order: &KeyOrder,
) -> anyhow::Result<()> {
    let mut text = order::to_string_pretty(value, order)?;
    text.push('\n');
    write_config(path, existed, &text)
}

/// Replace a config file the user owns: through a symlink, atomically, in
/// its old mode, the previous contents kept as `<name>.bak` beside `path`.
fn write_config(path: &Path, existed: bool, text: &str) -> anyhow::Result<()> {
    let target = resolve_symlinks(path).with_context(|| format!("resolving {}", path.display()))?;
    if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mode = match std::fs::metadata(&target) {
        Ok(meta) => Some(meta.permissions().mode() & 0o7777),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", target.display())),
    };
    if existed {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".bak");
        let backup = path.with_file_name(name);
        std::fs::copy(path, &backup)
            .with_context(|| format!("backing up to {}", backup.display()))?;
    }
    replace_file(&target, text, mode).with_context(|| format!("replacing {}", target.display()))?;
    Ok(())
}

/// Carry out a plan.
pub fn apply(plan: &InstallPlan, opts: &InstallOptions) -> anyhow::Result<()> {
    if !plan.settings_changes.is_empty() {
        write_settings(
            &plan.settings_path,
            plan.settings_existed,
            &plan.settings_after,
            &plan.settings_order,
        )?;
    }
    if plan.command == CommandPlan::Write {
        let path = opts.command_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        replace_file(&path, &command_md(opts.locale), None)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    if let Some(c) = plan.codex.as_ref().filter(|c| !c.changes.is_empty()) {
        write_config(&c.path, c.existed, &c.after)?;
    }
    match &plan.mcp {
        McpPlan::Present => {}
        McpPlan::Add => run_claude(opts, &add_args(opts))?,
        McpPlan::Replace { .. } => {
            run_claude(opts, &remove_args())?;
            run_claude(opts, &add_args(opts))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/usr/bin/x"), "/usr/bin/x");
        assert_eq!(shell_quote("/a b/x"), "'/a b/x'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn merge_adds_once_and_keeps_others() {
        let mut s = json!({
            "model": "opus",
            "hooks": {
                "SessionStart": [{"matcher": "startup", "hooks": [{"type": "command", "command": "echo hi"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "say done"}]}]
            }
        });
        let changes = merge_settings(&mut s, "/bin/senate-bridge").unwrap();
        assert_eq!(changes.len(), 4);
        assert_eq!(s["model"], "opus");
        assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "say done");
        assert_eq!(
            s["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "echo hi"
        );
        assert_eq!(
            s["hooks"]["SessionStart"][1]["hooks"][0]["command"],
            "/bin/senate-bridge hook session-start"
        );
        assert_eq!(s["crossSessionInbound"], "accept");
        let again = merge_settings(&mut s, "/bin/senate-bridge").unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn merge_updates_a_moved_binary() {
        let mut s = json!({});
        merge_settings(&mut s, "/old/senate-bridge").unwrap();
        let changes = merge_settings(&mut s, "/new/senate-bridge").unwrap();
        assert_eq!(changes.len(), 3);
        assert!(changes.iter().all(|c| c.starts_with('~')));
        assert_eq!(s["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
        assert_eq!(
            s["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
            "/new/senate-bridge hook session-end"
        );
    }

    #[test]
    fn writing_keeps_key_order_mode_and_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let claude = dir.path().join("claude");
        std::fs::create_dir_all(&dotfiles).unwrap();
        std::fs::create_dir_all(&claude).unwrap();
        let real = dotfiles.join("claude-settings.json");
        let original =
            "{\n  \"zeta\": 1,\n  \"model\": \"opus\",\n  \"alpha\": {\"b\": 1, \"a\": 2}\n}\n";
        std::fs::write(&real, original).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = claude.join("settings.json");
        std::os::unix::fs::symlink("../dotfiles/claude-settings.json", &link).unwrap();

        let opts = InstallOptions {
            exe: "/opt/senate-bridge".into(),
            claude_dir: claude.clone(),
            config_dir_env: None,
            user_config: claude.join(".claude.json"),
            claude_bin: "/nonexistent/claude".into(),
            codex_config: None,
            locale: Locale::default(),
        };
        let plan = plan(&opts).unwrap();
        write_settings(
            &plan.settings_path,
            plan.settings_existed,
            &plan.settings_after,
            &plan.settings_order,
        )
        .unwrap();

        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink(), "the link is kept");
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the target keeps its mode");
        let text = std::fs::read_to_string(&real).unwrap();
        let at = |k: &str| text.find(&format!("\"{k}\"")).unwrap();
        assert!(
            at("zeta") < at("model") && at("model") < at("alpha"),
            "{text}"
        );
        assert!(at("b") < at("a"), "nested order kept: {text}");
        assert!(at("alpha") < at("hooks"), "new keys go last: {text}");
        assert_eq!(
            std::fs::read_to_string(claude.join("settings.json.bak")).unwrap(),
            original
        );
        let leftovers: Vec<_> = std::fs::read_dir(&dotfiles)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1, "no temp file left: {leftovers:?}");
    }

    #[test]
    fn a_new_settings_file_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c/settings.json");
        write_settings(&path, false, &json!({"a": 1}), &KeyOrder::default()).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&std::fs::read_to_string(&path).unwrap()).unwrap(),
            json!({"a": 1})
        );
        assert!(!path.with_extension("json.bak").exists());
    }

    #[test]
    fn the_slash_command_and_report_follow_the_locale() {
        let dir = tempfile::tempdir().unwrap();
        let claude = dir.path().join("claude");
        let opts = InstallOptions {
            exe: "/opt/senate-bridge".into(),
            claude_dir: claude.clone(),
            config_dir_env: None,
            user_config: claude.join(".claude.json"),
            claude_bin: "/nonexistent/claude".into(),
            codex_config: None,
            locale: Locale::Zh,
        };
        let zh = command_md(Locale::Zh);
        let en = command_md(Locale::En);
        for md in [&zh, &en] {
            assert!(md.contains(COMMAND_MARKER));
            assert!(md.contains("mcp__senate__take") && md.contains("$ARGUMENTS"));
            assert!(!md.to_lowercase().contains(&["fei", "shu"].concat()));
        }
        assert!(zh.contains("告诉我这个 thread 现在归我了"));
        assert!(en.contains("tell me the thread is mine now"));
        std::fs::create_dir_all(claude.join("commands")).unwrap();
        // Ours in one language is rewritten in the other.
        std::fs::write(opts.command_path(), &en).unwrap();
        assert_eq!(command_plan(&opts), CommandPlan::Write);
        std::fs::write(opts.command_path(), &zh).unwrap();
        assert_eq!(command_plan(&opts), CommandPlan::Present);
        let p = plan(&opts).unwrap();
        let text = render(&p, &opts, false);
        assert!(
            text.starts_with("senate-bridge install（演练：不加 --yes 什么都不写）"),
            "{text}"
        );
        assert!(
            text.contains("什么都没写。加 --yes 重新运行以应用。"),
            "{text}"
        );
        let text = render(&p, &opts.clone().with_locale(Locale::En), false);
        assert!(
            text.contains("(dry run: nothing is written without --yes)"),
            "{text}"
        );
        assert!(
            text.contains("Nothing was written. Re-run with --yes to apply."),
            "{text}"
        );
    }

    #[test]
    fn merge_refuses_odd_shapes() {
        assert!(merge_settings(&mut json!([]), "/x").is_err());
        assert!(merge_settings(&mut json!({"hooks": []}), "/x").is_err());
        assert!(merge_settings(&mut json!({"hooks": {"SessionStart": {}}}), "/x").is_err());
    }
}
