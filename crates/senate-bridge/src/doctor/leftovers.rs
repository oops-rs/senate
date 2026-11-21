//! Leftovers: hooks and MCP servers of a bridge that are still in Claude
//! Code's, Codex's or agy's files once `setup install` has done its work.
//! These are another bridge (`commons-bridge`), or Senate under an old
//! name or at an old path. Each one runs on every prompt, or offers a
//! second set of tools, beside Senate's own.
//!
//! The files are read as `setup install` would leave them (its plan's
//! "after"), so whatever install itself would fix is not counted twice.
//! A bridge is a program named `senate`, `senate-bridge`, or `*-bridge`.

use std::path::{Path, PathBuf};

use senate_core::Locale;
use serde_json::Value;

use super::Check;
use crate::install::{self, InstallOptions};
use crate::mcp::SERVER_NAME;

/// One hook or server that should not be there.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Leftover {
    /// `claude`, `codex` or `agy`.
    runtime: &'static str,
    /// `mcp` or `hook`.
    kind: &'static str,
    /// The server's name, or the hook's event (`SessionStart`; for agy,
    /// `<hook name>.<event>`).
    name: String,
    command: String,
    file: PathBuf,
}

/// The program a command runs, unquoted.
pub(super) fn program_of(command: &str) -> PathBuf {
    let command = command.trim_start();
    let program = match command.chars().next() {
        Some(q @ ('\'' | '"')) => command[1..].split(q).next().unwrap_or_default(),
        _ => command.split_whitespace().next().unwrap_or_default(),
    };
    PathBuf::from(program)
}

fn is_bridge(program: &Path) -> bool {
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    name == "senate" || name == "senate-bridge" || name.ends_with("-bridge")
}

/// A hook command left over: a bridge's that is not this `senate`'s.
fn stray_hook(command: &str, exe: &Path) -> bool {
    let program = program_of(command);
    is_bridge(&program) && program != exe
}

/// Every string under a `command` key in `value`, with its path.
fn commands(value: &Value, at: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (key, v) in map {
                match (key.as_str(), v) {
                    ("command", Value::String(c)) => out.push((at.to_owned(), c.clone())),
                    _ => commands(v, &join(at, key), out),
                }
            }
        }
        Value::Array(items) => {
            for v in items {
                commands(v, at, out);
            }
        }
        _ => {}
    }
}

fn join(at: &str, key: &str) -> String {
    if at.is_empty() {
        key.to_owned()
    } else {
        format!("{at}.{key}")
    }
}

/// The hooks in `doc` (under `root`, or all of it) that are left over.
fn hooks(
    runtime: &'static str,
    doc: &Value,
    root: Option<&str>,
    file: &Path,
    exe: &Path,
) -> Vec<Leftover> {
    let (doc, at) = match root {
        Some(key) => (doc.get(key).unwrap_or(&Value::Null), key),
        None => (doc, ""),
    };
    let mut found = Vec::new();
    commands(doc, at, &mut found);
    found
        .into_iter()
        .filter(|(_, command)| stray_hook(command, exe))
        .map(|(at, command)| Leftover {
            runtime,
            kind: "hook",
            // Named by its event (and agy's hook name), not the nesting.
            name: at
                .split('.')
                .filter(|s| *s != "hooks")
                .collect::<Vec<_>>()
                .join("."),
            command,
            file: file.to_owned(),
        })
        .collect()
}

/// The MCP servers in `servers` that are a bridge under another name.
fn servers(runtime: &'static str, servers: Option<&Value>, file: &Path) -> Vec<Leftover> {
    let Some(Value::Object(servers)) = servers else {
        return Vec::new();
    };
    servers
        .iter()
        .filter(|(name, _)| name.as_str() != SERVER_NAME)
        .filter_map(|(name, server)| {
            let command = server.get("command")?.as_str()?;
            let args: Vec<&str> = server
                .get("args")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            is_bridge(&program_of(command)).then(|| Leftover {
                runtime,
                kind: "mcp",
                name: name.clone(),
                command: format!("{command} {}", args.join(" ")).trim().to_owned(),
                file: file.to_owned(),
            })
        })
        .collect()
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every leftover, as `setup install` would leave the files.
fn find(opts: &InstallOptions) -> Vec<Leftover> {
    let Ok(plan) = install::plan(opts) else {
        // `install_checks` reports why.
        return Vec::new();
    };
    let exe = opts.exe.as_path();
    let mut found = Vec::new();
    if let Some(config) = read_json(&opts.user_config) {
        found.extend(servers(
            "claude",
            config.get("mcpServers"),
            &opts.user_config,
        ));
    }
    found.extend(hooks(
        "claude",
        &plan.settings_after,
        Some("hooks"),
        &plan.settings_path,
        exe,
    ));
    if let Some(codex) = &plan.codex
        && let Ok(doc) = toml::from_str::<toml::Value>(&codex.after)
        && let Ok(doc) = serde_json::to_value(doc)
    {
        found.extend(servers("codex", doc.get("mcp_servers"), &codex.path));
        found.extend(hooks("codex", &doc, Some("hooks"), &codex.path, exe));
    }
    if let Some(agy) = &plan.agy {
        found.extend(servers(
            "agy",
            agy.mcp.after.get("mcpServers"),
            &agy.mcp.path,
        ));
        found.extend(hooks("agy", &agy.hooks.after, None, &agy.hooks.path, exe));
    }
    found
}

/// The command that removes a leftover, where one does.
fn fix(leftover: &Leftover) -> Option<String> {
    match (leftover.runtime, leftover.kind) {
        ("claude", "mcp") => Some(format!(
            "claude mcp remove --scope user {}",
            install::shell_quote(&leftover.name)
        )),
        ("codex", "mcp") => Some(format!(
            "codex mcp remove {}",
            install::shell_quote(&leftover.name)
        )),
        _ => None,
    }
}

pub fn checks(opts: &InstallOptions, l: Locale) -> Vec<Check> {
    let found = find(opts);
    if found.is_empty() {
        return vec![Check::ok(
            "leftovers",
            l.pick(
                "没有其他 bridge 的钩子或 MCP 服务器",
                "no other bridge's hooks or MCP servers",
            ),
        )];
    }
    found
        .iter()
        .map(|left| {
            let gone =
                program_of(&left.command).is_absolute() && !program_of(&left.command).exists();
            let gone = if gone {
                l.pick("（程序已不存在）", " (its program is gone)")
            } else {
                ""
            };
            let what = match left.kind {
                "mcp" => l.pick("MCP 服务器", "MCP server"),
                _ => l.pick("钩子", "hook"),
            };
            let found = match fix(left) {
                Some(_) => l.pick(
                    format!("{what} {} 运行 `{}`{gone}", left.name, left.command),
                    format!("{what} {} runs `{}`{gone}", left.name, left.command),
                ),
                None => l.pick(
                    format!(
                        "{what} {} 运行 `{}`{gone}：从 {} 中删掉它",
                        left.name,
                        left.command,
                        left.file.display()
                    ),
                    format!(
                        "{what} {} runs `{}`{gone}: remove it from {}",
                        left.name,
                        left.command,
                        left.file.display()
                    ),
                ),
            };
            Check::warn(
                format!("leftovers.{}.{}.{}", left.runtime, left.kind, left.name),
                found,
                fix(left),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_read_quoted_or_bare() {
        assert_eq!(program_of("/a/b hook x"), PathBuf::from("/a/b"));
        assert_eq!(
            program_of("'/My Tools/senate' hook x"),
            PathBuf::from("/My Tools/senate")
        );
        assert_eq!(program_of("\"/q/senate\" mcp"), PathBuf::from("/q/senate"));
    }

    #[test]
    fn bridges_are_senate_or_named_bridge() {
        assert!(is_bridge(Path::new("/x/commons-bridge")));
        assert!(is_bridge(Path::new("senate-bridge")));
        assert!(is_bridge(Path::new("/old/senate")));
        assert!(!is_bridge(Path::new("/usr/bin/npx")));
        assert!(!is_bridge(Path::new("/x/senated")));
    }
}
