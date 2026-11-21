//! The Antigravity CLI half of `install`. In `<gemini dir>` (`--gemini-dir`,
//! else `~/.gemini`), three of agy's own files gain Senate's entries:
//!
//! - `config/mcp_config.json`, agy's global MCP servers:
//!   `mcpServers.senate = {"command": "<abs exe>", "args": ["mcp", "--runtime", "agy"]}`.
//!   A server here keeps its name; one inside an agy plugin would be renamed
//!   `<plugin>_senate`.
//! - `config/hooks.json`, agy's global hooks, under one named hook `senate`
//!   that Senate owns whole: `SessionStart` → `hook session-start --runtime agy`
//!   and `PreInvocation` (before each model call) → `hook user-prompt-submit
//!   --runtime agy`, and `Stop` (the end of a turn) → `hook stop --runtime
//!   agy`, which keeps the agent going when messages arrived during the
//!   turn. agy has no hook for the end of a conversation.
//! - `antigravity-cli/settings.json` gains `mcp(senate/*)` in
//!   `permissions.allow`: Senate's own tools run unasked, the way Codex's
//!   `default_tools_approval_mode = "approve"` does it, so a hosted agy with
//!   nobody at its terminal does not block on its first Senate tool call.
//!   `mcp(senate)` alone does not match a tool; `mcp(senate/*)` does.
//!
//! Every other key, server, hook and grant is kept, in its order; each file
//! that changes is backed up as `<name>.bak`. An empty file (agy creates
//! `mcp_config.json` empty) is an empty object.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::{Map, Value, json};

use senate_core::Locale;

use super::order::{self, KeyOrder};
use super::{HOOK_TIMEOUT_SECS, runtime_hook_command, write_config};
use crate::mcp::SERVER_NAME;

/// The named hook in `hooks.json` that is Senate's.
const HOOK_NAME: &str = "senate";
/// The grant that approves every Senate tool.
pub const GRANT: &str = "mcp(senate/*)";
/// `(agy hook event, hook subcommand)`.
pub const HOOKS: [(&str, &str); 3] = [
    ("SessionStart", "session-start"),
    ("PreInvocation", "user-prompt-submit"),
    ("Stop", "stop"),
];

pub fn mcp_args() -> [&'static str; 3] {
    ["mcp", "--runtime", "agy"]
}

/// One of agy's JSON files, read and changed in memory.
#[derive(Debug, Clone, PartialEq)]
pub struct JsonPlan {
    pub path: PathBuf,
    pub existed: bool,
    pub after: Value,
    order: KeyOrder,
    pub changes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgyPlan {
    pub mcp: JsonPlan,
    pub hooks: JsonPlan,
    pub settings: JsonPlan,
}

impl AgyPlan {
    pub fn changes(&self) -> usize {
        self.mcp.changes.len() + self.hooks.changes.len() + self.settings.changes.len()
    }

    fn files(&self) -> [&JsonPlan; 3] {
        [&self.mcp, &self.hooks, &self.settings]
    }
}

fn read(path: &Path) -> anyhow::Result<(bool, Value, KeyOrder)> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok((true, json!({}), KeyOrder::default())),
        Ok(text) => Ok((
            true,
            serde_json::from_str(&text).with_context(|| {
                format!(
                    "{} is not valid JSON; fix it first (nothing was changed)",
                    path.display()
                )
            })?,
            KeyOrder::of(&text),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok((false, json!({}), KeyOrder::default()))
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn object<'a>(value: &'a mut Value, what: &str) -> Result<&'a mut Map<String, Value>, String> {
    value
        .as_object_mut()
        .ok_or_else(|| format!("{what} is not a JSON object"))
}

/// `mcpServers.senate`, keeping any other key the user gave it.
pub fn merge_mcp(config: &mut Value, exe: &str) -> Result<Vec<String>, String> {
    let servers = object(config, "mcp_config.json")?
        .entry("mcpServers")
        .or_insert_with(|| json!({}));
    let servers = object(servers, "`mcpServers` in mcp_config.json")?;
    let present = servers.contains_key(SERVER_NAME);
    let server = servers.entry(SERVER_NAME).or_insert_with(|| json!({}));
    let server = object(server, "`mcpServers.senate` in mcp_config.json")?;
    let args = json!(mcp_args());
    let want = format!("{exe} {}", mcp_args().join(" "));
    if server.get("command").and_then(Value::as_str) == Some(exe)
        && server.get("args") == Some(&args)
    {
        return Ok(Vec::new());
    }
    let was = format!(
        "{} {}",
        server
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        server
            .get("args")
            .and_then(Value::as_array)
            .map(|a| a
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "))
            .unwrap_or_default()
    );
    server.insert("command".into(), json!(exe));
    server.insert("args".into(), args);
    Ok(vec![if present {
        format!("~ mcpServers.{SERVER_NAME}: {}  →  {want}", was.trim())
    } else {
        format!("+ mcpServers.{SERVER_NAME}: {want}")
    }])
}

/// The `senate` hook as `install` writes it.
pub fn hook_entry(exe: &str) -> Value {
    let mut hook = Map::new();
    for (event, sub) in HOOKS {
        hook.insert(
            event.into(),
            json!([{
                "type": "command",
                "command": runtime_hook_command(exe, sub, "agy"),
                "timeout": HOOK_TIMEOUT_SECS,
            }]),
        );
    }
    Value::Object(hook)
}

/// The named hook `senate`, whole; other named hooks are not touched.
pub fn merge_hooks(hooks: &mut Value, exe: &str) -> Result<Vec<String>, String> {
    let hooks = object(hooks, "hooks.json")?;
    let want = hook_entry(exe);
    match hooks.get(HOOK_NAME) {
        Some(current) if *current == want => Ok(Vec::new()),
        current => {
            let change = if current.is_some() { '~' } else { '+' };
            hooks.insert(HOOK_NAME.into(), want);
            Ok(HOOKS
                .iter()
                .map(|(event, sub)| {
                    format!(
                        "{change} {HOOK_NAME}.{event}: {}",
                        runtime_hook_command(exe, sub, "agy")
                    )
                })
                .collect())
        }
    }
}

/// `mcp(senate/*)` in `permissions.allow`.
pub fn merge_settings(settings: &mut Value) -> Result<Vec<String>, String> {
    let permissions = object(settings, "settings.json")?
        .entry("permissions")
        .or_insert_with(|| json!({}));
    let allow = object(permissions, "`permissions` in settings.json")?
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or("`permissions.allow` in settings.json is not an array")?;
    if allow.iter().any(|grant| grant.as_str() == Some(GRANT)) {
        return Ok(Vec::new());
    }
    allow.push(json!(GRANT));
    Ok(vec![format!("+ permissions.allow: {GRANT}")])
}

fn plan_file(
    path: PathBuf,
    merge: impl FnOnce(&mut Value) -> Result<Vec<String>, String>,
) -> anyhow::Result<JsonPlan> {
    let (existed, mut after, order) = read(&path)?;
    let changes = merge(&mut after).map_err(anyhow::Error::msg)?;
    Ok(JsonPlan {
        path,
        existed,
        after,
        order,
        changes,
    })
}

/// Read agy's files under `gemini` and work out the changes. Writes nothing.
pub fn plan(gemini: &Path, exe: &str) -> anyhow::Result<AgyPlan> {
    let config = gemini.join("config");
    Ok(AgyPlan {
        mcp: plan_file(config.join("mcp_config.json"), |v| merge_mcp(v, exe))?,
        hooks: plan_file(config.join("hooks.json"), |v| merge_hooks(v, exe))?,
        settings: plan_file(
            gemini.join("antigravity-cli").join("settings.json"),
            merge_settings,
        )?,
    })
}

pub fn apply(plan: &AgyPlan) -> anyhow::Result<()> {
    for file in plan.files() {
        if file.changes.is_empty() {
            continue;
        }
        let mut text = order::to_string_pretty(&file.after, &file.order)?;
        text.push('\n');
        write_config(&file.path, file.existed, &text)?;
    }
    Ok(())
}

pub fn render(plan: &AgyPlan, applied: bool, locale: Locale, out: &mut String) {
    let zh = locale == Locale::Zh;
    for file in plan.files() {
        let _ = writeln!(out, "   {}", file.path.display());
        if file.changes.is_empty() {
            let _ = writeln!(
                out,
                "     {}",
                locale.pick("已是最新", "already up to date")
            );
            continue;
        }
        for c in &file.changes {
            let _ = writeln!(out, "     {c}");
        }
        if file.existed {
            let _ = writeln!(
                out,
                "     {}",
                locale.pick(
                    "其他内容保持不变；原文件另存为 .bak。",
                    "Everything else is kept; the previous file is saved as .bak."
                )
            );
        }
    }
    let _ = writeln!(
        out,
        "   {}",
        match (applied, zh) {
            (_, true) =>
                "agy 会话以 [senator::agy-<conversation-id>] 的身份加入；新启动的 agy 才会读取这些设置。\
                          Senate 的工具（mcp(senate/*)）无需逐次批准。",
            (_, false) =>
                "agy sessions join as [senator::agy-<conversation-id>]; an agy started after this \
                           reads the new settings. Senate's tools (mcp(senate/*)) run without asking.",
        }
    );
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const EXE: &str = "/opt/bin/senate";

    #[test]
    fn each_file_gains_senate_once_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        let cli = dir.path().join("antigravity-cli");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&cli).unwrap();
        // agy creates an empty mcp_config.json.
        std::fs::write(config.join("mcp_config.json"), "").unwrap();
        std::fs::write(
            config.join("hooks.json"),
            r#"{"lint":{"PostToolUse":[{"matcher":"run_command","hooks":[{"command":"./lint.sh"}]}]}}"#,
        )
        .unwrap();
        let settings =
            r#"{"model":"x","toolPermission":"request-review","trustedWorkspaces":["/w"]}"#;
        std::fs::write(cli.join("settings.json"), settings).unwrap();

        let dry = plan(dir.path(), EXE).unwrap();
        assert_eq!(dry.changes(), 5, "{dry:#?}");
        let mut out = String::new();
        render(&dry, false, Locale::En, &mut out);
        assert!(
            out.contains("+ mcpServers.senate: /opt/bin/senate mcp --runtime agy"),
            "{out}"
        );
        assert!(
            out.contains(
                "+ senate.PreInvocation: /opt/bin/senate hook user-prompt-submit --runtime agy"
            ),
            "{out}"
        );
        assert!(out.contains("+ permissions.allow: mcp(senate/*)"), "{out}");
        assert_eq!(
            std::fs::read_to_string(cli.join("settings.json")).unwrap(),
            settings,
            "a dry run writes nothing"
        );

        apply(&dry).unwrap();
        let read = |path: PathBuf| -> Value {
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let mcp = read(config.join("mcp_config.json"));
        assert_eq!(mcp["mcpServers"]["senate"]["command"], EXE);
        assert_eq!(
            mcp["mcpServers"]["senate"]["args"],
            json!(["mcp", "--runtime", "agy"])
        );
        let hooks = read(config.join("hooks.json"));
        assert_eq!(hooks["lint"]["PostToolUse"][0]["matcher"], "run_command");
        assert_eq!(
            hooks["senate"]["SessionStart"][0]["command"],
            "/opt/bin/senate hook session-start --runtime agy"
        );
        assert_eq!(
            hooks["senate"]["Stop"][0]["command"],
            "/opt/bin/senate hook stop --runtime agy"
        );
        let text = std::fs::read_to_string(cli.join("settings.json")).unwrap();
        assert!(
            text.find("\"model\"").unwrap() < text.find("\"permissions\"").unwrap(),
            "keys keep their order: {text}"
        );
        let after: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(after["permissions"]["allow"], json!(["mcp(senate/*)"]));
        assert_eq!(after["trustedWorkspaces"], json!(["/w"]));
        assert!(cli.join("settings.json.bak").exists());

        assert_eq!(
            plan(dir.path(), EXE).unwrap().changes(),
            0,
            "a second run changes nothing"
        );
    }

    #[test]
    fn a_moved_binary_is_replaced_and_user_keys_stay() {
        let mut mcp = json!({"mcpServers": {"senate": {
            "command": "/old/senate", "args": ["mcp", "--agy"], "env": {"X": "1"}
        }, "other": {"url": "https://x"}}});
        let changes = merge_mcp(&mut mcp, EXE).unwrap();
        assert_eq!(
            changes,
            ["~ mcpServers.senate: /old/senate mcp --agy  →  /opt/bin/senate mcp --runtime agy"]
        );
        assert_eq!(mcp["mcpServers"]["senate"]["env"]["X"], "1");
        assert_eq!(mcp["mcpServers"]["other"]["url"], "https://x");

        let mut hooks = json!({"senate": {"SessionStart": [{"command": "/old/senate hook session-start --agy"}]}});
        let changes = merge_hooks(&mut hooks, EXE).unwrap();
        assert!(changes.iter().all(|c| c.starts_with('~')), "{changes:?}");
        assert_eq!(hooks["senate"], hook_entry(EXE));

        let mut settings = json!({"permissions": {"allow": ["command(ls)", "mcp(senate/*)"]}});
        assert!(merge_settings(&mut settings).unwrap().is_empty());
    }

    #[test]
    fn odd_shapes_are_refused() {
        assert!(merge_mcp(&mut json!({"mcpServers": 1}), EXE).is_err());
        assert!(merge_hooks(&mut json!([]), EXE).is_err());
        assert!(merge_settings(&mut json!({"permissions": {"allow": "x"}})).is_err());
    }
}
