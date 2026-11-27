//! The Codex half of `install`: `<codex home>/config.toml` (`--codex-home`,
//! else `$CODEX_HOME`, else `~/.codex`) gains
//!
//! ```toml
//! [mcp_servers.senate]
//! command = "<abs exe>"
//! args = ["mcp", "--runtime", "codex"]
//! default_tools_approval_mode = "approve"
//!
//! [[hooks.SessionStart]]
//! [[hooks.SessionStart.hooks]]
//! type = "command"
//! command = "<abs exe> hook session-start --runtime codex"
//! timeout = 10
//! ```
//!
//! and the same for UserPromptSubmit, SessionEnd and Stop. The file is edited with
//! `toml_edit`, so every other table, key and comment stays as it was; keys
//! the user added to `[mcp_servers.senate]` are kept. Senate's own tools are
//! approved up front: a hosted Codex has nobody at its terminal to answer
//! an approval prompt, and would block on its first Senate tool call. The
//! hosted TUI cannot set this for itself: `codex resume --remote` does not
//! pass `mcp_servers.*` overrides on to the app-server, where the tools
//! run. Codex runs a hook only once it is trusted in `/hooks`; `install`
//! records Codex's declaration hash for exact Senate hook groups.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context;
use sha2::{Digest, Sha256};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value, value};

use senate_core::Locale;

use super::{HOOK_TIMEOUT_SECS, HOOKS, shell_quote};
use crate::mcp::SERVER_NAME;

/// The `[mcp_servers.senate]` key that lets Senate's tools run unasked.
pub const APPROVAL_KEY: &str = "default_tools_approval_mode";
const APPROVE: &str = "approve";

pub fn hook_command(exe: &str, subcommand: &str) -> String {
    super::runtime_hook_command(exe, subcommand, "codex")
}

/// What happened to one hook entry.
enum Seen {
    Absent,
    Present,
    Moved(String),
}

/// Look through one group's `hooks` list for ours; point a stale one at
/// `want`.
fn scan_group(group_hooks: Option<&mut Item>, sub: &str, want: &str, previous_exe: &str) -> Seen {
    let mut seen = Seen::Absent;
    let mut check = |command: Option<&str>| -> Option<String> {
        let command = command?;
        if command == want {
            seen = Seen::Present;
            None
        } else if command == hook_command(previous_exe, sub)
            || command == format!("{} hook {sub} --codex", shell_quote(previous_exe))
        {
            seen = Seen::Moved(command.to_string());
            Some(want.to_string())
        } else {
            None
        }
    };
    match group_hooks {
        Some(Item::ArrayOfTables(list)) => {
            for hook in list.iter_mut() {
                if let Some(new) = check(hook.get("command").and_then(Item::as_str)) {
                    hook["command"] = value(new);
                }
            }
        }
        Some(Item::Value(Value::Array(list))) => {
            for hook in list.iter_mut() {
                if let Value::InlineTable(hook) = hook
                    && let Some(new) = check(hook.get("command").and_then(Value::as_str))
                {
                    hook.insert("command", Value::from(new));
                }
            }
        }
        _ => {}
    }
    seen
}

/// Codex clamps a SessionEnd hook to 3 s (and warns about a longer one);
/// `hook session-end` needs about one.
const SESSION_END_TIMEOUT_SECS: u64 = 3;

fn timeout_for(event: &str) -> i64 {
    let secs = if event == "SessionEnd" {
        SESSION_END_TIMEOUT_SECS
    } else {
        HOOK_TIMEOUT_SECS
    };
    i64::try_from(secs).unwrap_or(i64::MAX)
}

fn hook_table(want: &str, event: &str) -> Table {
    let mut hook = Table::new();
    hook["type"] = value("command");
    hook["command"] = value(want);
    hook["timeout"] = value(timeout_for(event));
    hook
}

/// Merge the `senate` MCP server and the four hooks into `doc`. One line
/// per change; empty when already up to date.
pub fn merge(doc: &mut DocumentMut, exe: &str) -> Result<Vec<String>, String> {
    let mut changes = Vec::new();
    let root = doc.as_table_mut();

    // [mcp_servers.senate]
    let servers = root
        .entry("mcp_servers")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_like_mut()
        .ok_or("`mcp_servers` in config.toml is not a table")?;
    let present = servers.contains_key(SERVER_NAME);
    let server = servers
        .entry(SERVER_NAME)
        .or_insert(Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| format!("`mcp_servers.{SERVER_NAME}` in config.toml is not a table"))?;
    let command = server
        .get("command")
        .and_then(Item::as_str)
        .map(str::to_string);
    let args: Option<Vec<String>> = server.get("args").and_then(Item::as_array).map(|a| {
        a.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect()
    });
    let want_args = ["mcp", "--runtime", "codex"];
    let want = format!("{} mcp --runtime codex", shell_quote(exe));
    if command.as_deref() != Some(exe)
        || args.as_deref() != Some(&want_args.map(str::to_string)[..])
    {
        server.insert("command", value(exe));
        server.insert("args", value(Array::from_iter(want_args)));
        changes.push(if present {
            format!(
                "~ mcp_servers.{SERVER_NAME}: {} {}  →  {want}",
                command.as_deref().unwrap_or_default(),
                args.unwrap_or_default().join(" ")
            )
        } else {
            format!("+ mcp_servers.{SERVER_NAME}: {want}")
        });
    }
    let approval = server
        .get(APPROVAL_KEY)
        .and_then(Item::as_str)
        .map(str::to_string);
    if approval.as_deref() != Some(APPROVE) {
        server.insert(APPROVAL_KEY, value(APPROVE));
        changes.push(match approval {
            Some(was) => format!("~ mcp_servers.{SERVER_NAME}.{APPROVAL_KEY}: {was}  →  {APPROVE}"),
            None => format!("+ mcp_servers.{SERVER_NAME}.{APPROVAL_KEY} = \"{APPROVE}\""),
        });
    }

    let previous_exe = command
        .as_deref()
        .filter(|path| {
            matches!(
                Path::new(path).file_name().and_then(|name| name.to_str()),
                Some("senate" | "senate-bridge")
            )
        })
        .unwrap_or(exe);
    // [[hooks.<Event>]]
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_mut()
        .ok_or("`hooks` in config.toml is not a table")?;
    for (event, sub) in HOOKS {
        let want = hook_command(exe, sub);
        let mut found = false;
        let mut note = |seen: Seen, changes: &mut Vec<String>| match seen {
            Seen::Absent => {}
            Seen::Present => found = true,
            Seen::Moved(was) => {
                found = true;
                changes.push(format!("~ hooks.{event}: {was}  →  {want}"));
            }
        };
        match hooks.get_mut(event) {
            None => {
                hooks.insert(event, Item::ArrayOfTables(ArrayOfTables::new()));
            }
            Some(Item::ArrayOfTables(groups)) => {
                for group in groups.iter_mut() {
                    let seen = scan_group(group.get_mut("hooks"), sub, &want, previous_exe);
                    note(seen, &mut changes);
                }
            }
            Some(Item::Value(Value::Array(groups))) => {
                for group in groups.iter_mut() {
                    let Value::InlineTable(group) = group else {
                        continue;
                    };
                    let mut list = group.get("hooks").cloned().map(Item::Value);
                    let seen = scan_group(list.as_mut(), sub, &want, previous_exe);
                    if let Seen::Moved(_) = &seen
                        && let Some(Item::Value(v)) = list
                    {
                        group.insert("hooks", v);
                    }
                    note(seen, &mut changes);
                }
            }
            Some(_) => return Err(format!("`hooks.{event}` in config.toml is not an array")),
        }
        if found {
            continue;
        }
        match hooks.get_mut(event) {
            Some(Item::ArrayOfTables(groups)) => {
                let mut list = ArrayOfTables::new();
                list.push(hook_table(&want, event));
                let mut group = Table::new();
                group.insert("hooks", Item::ArrayOfTables(list));
                groups.push(group);
            }
            Some(Item::Value(Value::Array(groups))) => {
                let mut hook = InlineTable::new();
                hook.insert("type", Value::from("command"));
                hook.insert("command", Value::from(want.clone()));
                hook.insert("timeout", Value::from(timeout_for(event)));
                let mut group = InlineTable::new();
                group.insert("hooks", Value::Array(Array::from_iter([hook])));
                groups.push(group);
            }
            _ => unreachable!("hooks.{event} was made an array above"),
        }
        changes.push(format!("+ hooks.{event}: {want}"));
    }
    Ok(changes)
}

/// Codex hashes sorted compact JSON of its normalized handler, including
/// the default async=false. Local command hooks have no plugin content hash.
fn trust_exact_hooks(doc: &mut DocumentMut, exe: &str, path: &Path) -> anyhow::Result<Vec<String>> {
    let parsed: toml::Value = toml::from_str(&doc.to_string())?;
    let hooks = doc["hooks"].as_table_mut().expect("merge created hooks");
    let mut changes = Vec::new();
    for (event, sub) in HOOKS {
        let command = hook_command(exe, sub);
        let label = sub.replace('-', "_");
        let expected = serde_json::json!({"hooks": [{
            "type": "command", "command": command, "timeout": timeout_for(event)
        }]});
        // Compare the complete group, including all handler fields, before
        // using Codex's normalized identity. Both inline and table arrays
        // deserialize to the same group.
        let groups: Vec<serde_json::Value> = parsed["hooks"][event].clone().try_into()?;
        for (index, group) in groups
            .iter()
            .enumerate()
            .filter(|(_, group)| *group == &expected)
        {
            let mut identity = group.clone();
            identity["hooks"][0]["async"] = false.into();
            identity["event_name"] = label.clone().into();
            identity.sort_all_objects();
            let hash = format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&identity)?)
            );
            let key = format!("{}:{label}:{index}:0", path.display());
            let states = hooks
                .entry("state")
                .or_insert(Item::Table(Table::new()))
                .as_table_like_mut()
                .context("hooks.state must be a table")?;
            let state = states
                .entry(&key)
                .or_insert(Item::Table(Table::new()))
                .as_table_like_mut()
                .context("hook state must be a table")?;
            if state.get("trusted_hash").and_then(Item::as_str) != Some(&hash) {
                state.insert("trusted_hash", value(hash));
                changes.push(format!("+ hooks.state.{key}.trusted_hash"));
            }
        }
    }
    Ok(changes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexPlan {
    pub path: PathBuf,
    pub existed: bool,
    pub after: String,
    pub trust_changed: bool,
    pub changes: Vec<String>,
}

/// Codex canonicalizes an explicit CODEX_HOME. On a first install the
/// directory may not exist yet, so resolve its existing parent first.
pub(super) fn absolute_home(home: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(home)?;
    for parent in absolute.ancestors() {
        match parent.canonicalize() {
            Ok(real) => return Ok(real.join(absolute.strip_prefix(parent).expect("ancestor"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(absolute)
}

/// Read `path` and work out the changes. Writes nothing.
pub fn plan(path: &Path, exe: &str) -> anyhow::Result<CodexPlan> {
    let (existed, text) = match std::fs::read_to_string(path) {
        Ok(text) => (true, text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (false, String::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut doc: DocumentMut = text.parse().with_context(|| {
        format!(
            "{} is not valid TOML; fix it first (nothing was changed)",
            path.display()
        )
    })?;
    let mut changes = merge(&mut doc, exe).map_err(anyhow::Error::msg)?;
    let trusts = trust_exact_hooks(&mut doc, exe, &std::path::absolute(path)?)?;
    let trust_changed = !trusts.is_empty();
    changes.extend(trusts);
    Ok(CodexPlan {
        path: path.to_path_buf(),
        existed,
        trust_changed,
        after: doc.to_string(),
        changes,
    })
}

/// What `merge` writes, as TOML, for the report.
pub fn excerpt(exe: &str) -> String {
    let mut doc = DocumentMut::new();
    // A fresh document has nothing to refuse.
    let _ = merge(&mut doc, exe);
    doc.to_string()
}

pub fn render(plan: &CodexPlan, exe: &str, applied: bool, locale: Locale, out: &mut String) {
    let zh = locale == Locale::Zh;
    if plan.changes.is_empty() {
        let _ = writeln!(out, "   {}", locale.pick("已是最新", "already up to date"));
    } else {
        for c in &plan.changes {
            let _ = writeln!(out, "   {c}");
        }
        let _ = writeln!(out, "   {}", locale.pick("写入的条目：", "The entries:"));
        for line in excerpt(exe).lines() {
            let _ = writeln!(out, "     {line}");
        }
        let _ = writeln!(
            out,
            "   {}{}",
            locale.pick(
                "其他所有表、键和注释都保持不变",
                "Every other table, key and comment is kept"
            ),
            match (plan.existed, zh) {
                (true, true) => "；原文件另存为 config.toml.bak。",
                (true, false) => "; the previous file is saved as config.toml.bak.",
                (false, true) => "。",
                (false, false) => ".",
            }
        );
    }
    if applied && plan.trust_changed {
        let _ = writeln!(
            out,
            "   {}",
            locale.pick(
                "已为与安装配置完全匹配的 Senate 钩子写入 Codex 信任记录。",
                "Pre-trusted matching Senate hooks in Codex.",
            )
        );
    }
    let _ = writeln!(
        out,
        "   {}",
        match (applied, zh) {
            (true, true) => "需要手动做一次：",
            (false, true) => "需要手动做一次（加 --yes 之后）：",
            (true, false) => "Once, by hand:",
            (false, false) => "Once, by hand (after --yes):",
        }
    );
    let _ = writeln!(
        out,
        "   - {}\n\
         \x20      codex app-server daemon start\n\
         \x20      codex --remote unix://        (e.g. alias cx='codex --remote unix://')\n\
         \x20    {}",
        locale.pick(
            "让 Codex 连到托管的 app-server（bridge 通过它推送消息）：",
            "Run Codex against the managed app-server, which the bridge pushes through:"
        ),
        locale.pick(
            "如果 app-server 已在运行，执行 `codex app-server daemon restart` 后才会加载新的 MCP server 和钩子。",
            "A daemon that was already running loads the new MCP server and hooks after\n\
             \x20    `codex app-server daemon restart`."
        )
    );
    let _ = writeln!(
        out,
        "   - {}",
        locale.pick(
            "Codex 会话默认以 <person>/codex 的身份加入；senate.toml 的 [agents] 可以改名，\
             工作区里的 .senate/agent 文件可以为该工作区固定一个名字。",
            "Codex sessions join as <person>/codex by default; [agents] in senate.toml renames \
             it, and a .senate/agent file pins a name for one worktree."
        )
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = "/opt/bin/senate";

    #[test]
    fn trust_changes_are_reported_once_and_only_when_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let fresh = plan(&path, EXE).unwrap();
        for applied in [false, true] {
            let mut output = String::new();
            render(&fresh, EXE, applied, Locale::En, &mut output);
            assert_eq!(output.contains("Pre-trusted"), applied);
        }
        std::fs::write(&path, fresh.after).unwrap();
        assert!(!plan(&path, EXE).unwrap().trust_changed);
    }

    #[test]
    fn only_the_installers_exact_declaration_receives_trust() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let command = hook_command(EXE, "stop");
        let handler = format!(r#"type = "command", command = "{command}", timeout = 10"#);
        let key = format!("{}:stop:0:0", path.display());
        for group in [
            format!("hooks = [{{ {handler}, async = false }}]"),
            format!("hooks = [{{ {handler}, commandWindows = 'other' }}]"),
            format!("matcher = 'startup', hooks = [{{ {handler} }}]"),
            format!("hooks = [{{ {handler} }}, {{ {handler} }}]"),
            format!("hooks = [{{ {} }}]", handler.replace("10", "11")),
            format!(
                "hooks = [{{ {} }}]",
                handler.replace("/opt/bin", "/foreign")
            ),
            format!(
                "hooks = [{{ {} }}]",
                handler.replace(" hook", " extra hook")
            ),
            format!(
                "hooks = [{{ {} }}]",
                handler.replace(" --runtime codex", "")
            ),
        ] {
            let original = format!("hooks.Stop = [{{ {group} }}]\n");
            std::fs::write(&path, &original).unwrap();
            let installed = plan(&path, EXE).unwrap();
            let parsed: toml::Value = installed.after.parse().unwrap();
            assert!(parsed["hooks"]["state"].get(&key).is_none(), "{group}");
            let before: toml::Value = original.parse().unwrap();
            assert_eq!(parsed["hooks"]["Stop"][0], before["hooks"]["Stop"][0]);
            std::fs::write(&path, installed.after).unwrap();
            assert!(plan(&path, EXE).unwrap().changes.is_empty(), "{group}");
        }
    }

    #[test]
    fn reinstall_does_not_refresh_trust_for_an_edited_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let installed = plan(&path, EXE).unwrap();
        let changed = installed.after.replace(
            "hook stop --runtime codex",
            "hook stop --runtime codex extra",
        );
        std::fs::write(&path, &changed).unwrap();
        let reinstalled = plan(&path, EXE).unwrap();
        let parsed: toml::Value = reinstalled.after.parse().unwrap();
        let original: toml::Value = installed.after.parse().unwrap();
        let key = format!("{}:stop:0:0", path.display());
        let new_key = format!("{}:stop:1:0", path.display());
        let states = &parsed["hooks"]["state"];
        assert_eq!(states[&key], original["hooks"]["state"][&key]);
        assert_eq!(states[&new_key], states[&key]);
        assert_eq!(
            parsed["hooks"]["Stop"][0]["hooks"][0]["command"].as_str(),
            Some("/opt/bin/senate hook stop --runtime codex extra")
        );
    }

    #[test]
    fn codex_hash_is_bound_to_the_exact_hook_at_its_group_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let old = format!("{}:stop:0:0", path.display());
        let key = format!("{}:stop:1:0", path.display());
        std::fs::write(
            &path,
            format!(
                r#"hooks.Stop = [{{ hooks = [{{ type = "command", command = "other" }}] }}]
[hooks.state.'{old}']
trusted_hash = "keep"
[hooks.state.'{key}']
enabled = false
"#
            ),
        )
        .unwrap();
        let installed = plan(&path, EXE).unwrap();
        let parsed: toml::Value = installed.after.parse().unwrap();
        let states = &parsed["hooks"]["state"];
        assert_eq!(states[&old]["trusted_hash"].as_str(), Some("keep"));
        assert_eq!(states[&key]["enabled"].as_bool(), Some(false));
        assert_eq!(
            states[&key]["trusted_hash"].as_str(),
            Some("sha256:b274be346cf8a365201b3cc41fa6c7a555aa8df778d70863895749eb72118b4f")
        );
    }

    #[test]
    fn merge_adds_once_and_keeps_the_rest() {
        let original = r#"# my codex config
model = "gpt-6"

[mcp_servers.other]
url = "https://other.example/mcp" # keep me

[[hooks.SessionStart]]
matcher = "startup"
[[hooks.SessionStart.hooks]]
type = "command"
command = "echo hi"

[hooks.state."/x/.codex/hooks.json:session_start:0:0"]
trusted_hash = "sha256:abc"
"#;
        let mut doc: DocumentMut = original.parse().unwrap();
        let changes = merge(&mut doc, EXE).unwrap();
        assert_eq!(changes.len(), 6, "{changes:?}");
        assert!(
            changes.contains(
                &"+ mcp_servers.senate.default_tools_approval_mode = \"approve\"".to_owned()
            ),
            "{changes:?}"
        );
        let text = doc.to_string();
        assert!(
            text.starts_with("# my codex config\nmodel = \"gpt-6\""),
            "{text}"
        );
        assert!(
            text.contains("url = \"https://other.example/mcp\" # keep me"),
            "{text}"
        );
        assert!(text.contains("trusted_hash = \"sha256:abc\""), "{text}");
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["senate"]["command"].as_str(),
            Some(EXE)
        );
        assert_eq!(
            parsed["mcp_servers"]["senate"]["args"],
            toml::Value::Array(vec!["mcp".into(), "--runtime".into(), "codex".into()])
        );
        assert_eq!(
            parsed["mcp_servers"]["senate"]["default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        let start = parsed["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start.len(), 2);
        assert_eq!(start[0]["hooks"][0]["command"].as_str(), Some("echo hi"));
        assert_eq!(
            start[1]["hooks"][0]["command"].as_str(),
            Some("/opt/bin/senate hook session-start --runtime codex")
        );
        assert_eq!(start[1]["hooks"][0]["timeout"].as_integer(), Some(10));
        for event in ["UserPromptSubmit", "SessionEnd", "Stop"] {
            assert_eq!(parsed["hooks"][event].as_array().unwrap().len(), 1);
        }
        assert_eq!(
            parsed["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"].as_integer(),
            Some(3),
            "Codex clamps SessionEnd hooks to 3 s"
        );

        let again = merge(&mut doc, EXE).unwrap();
        assert!(again.is_empty(), "{again:?}");
        assert_eq!(doc.to_string(), text, "a second merge changes nothing");
    }

    #[test]
    fn a_moved_binary_is_updated_in_place_and_user_keys_stay() {
        let mut doc = DocumentMut::new();
        merge(&mut doc, "/old/senate-bridge").unwrap();
        doc["mcp_servers"]["senate"]["default_tools_approval_mode"] = value("approve");
        let changes = merge(&mut doc, "/new/senate").unwrap();
        assert_eq!(changes.len(), 5, "{changes:?}");
        assert!(changes.iter().all(|c| c.starts_with('~')), "{changes:?}");
        let parsed: toml::Value = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["senate"]["default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        assert_eq!(parsed["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
        assert_eq!(
            parsed["hooks"]["SessionEnd"][0]["hooks"][0]["command"].as_str(),
            Some("/new/senate hook session-end --runtime codex")
        );
    }

    #[test]
    fn senate_tools_are_approved_once_and_a_dry_run_shows_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[mcp_servers.senate]\ncommand = \"/opt/bin/senate\"\nargs = [\"mcp\", \"--runtime\", \"codex\"]\n\
                        default_tools_approval_mode = \"prompt\"\nstartup_timeout_sec = 20\n";
        std::fs::write(&path, original).unwrap();
        // The dry run names the change and writes nothing.
        let dry = plan(&path, EXE).unwrap();
        // The hooks are new here too; the server's command was already ours.
        assert_eq!(
            dry.changes[0],
            "~ mcp_servers.senate.default_tools_approval_mode: prompt  →  approve"
        );
        assert!(
            dry.changes[1..].iter().all(|c| c.starts_with("+ hooks.")),
            "{:?}",
            dry.changes
        );
        let mut out = String::new();
        render(&dry, EXE, false, Locale::En, &mut out);
        assert!(
            out.contains("~ mcp_servers.senate.default_tools_approval_mode: prompt  →  approve"),
            "{out}"
        );
        assert!(
            out.contains("default_tools_approval_mode = \"approve\""),
            "{out}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        // Applied, the key is set once; the rest of the table is the user's.
        std::fs::write(&path, &dry.after).unwrap();
        let parsed: toml::Value = toml::from_str(&dry.after).unwrap();
        let server = &parsed["mcp_servers"]["senate"];
        assert_eq!(
            server["default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        assert_eq!(server["startup_timeout_sec"].as_integer(), Some(20));
        assert_eq!(dry.after.matches("default_tools_approval_mode").count(), 1);
        assert!(plan(&path, EXE).unwrap().changes.is_empty());
    }

    #[test]
    fn inline_hook_arrays_are_understood() {
        let mut doc: DocumentMut = "mcp_servers.senate.command = \"/old/senate\"\nhooks.SessionStart = [{ hooks = [{ type = \"command\", \
             command = \"/old/senate hook session-start --codex\" }] }]\n"
            .parse()
            .unwrap();
        let changes = merge(&mut doc, EXE).unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.starts_with("~ hooks.SessionStart")),
            "{changes:?}"
        );
        let parsed: toml::Value = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(
            parsed["hooks"]["SessionStart"][0]["hooks"][0]["command"].as_str(),
            Some("/opt/bin/senate hook session-start --runtime codex")
        );
        assert!(merge(&mut doc, EXE).unwrap().is_empty());
    }

    #[test]
    fn legacy_mcp_args_are_migrated_to_explicit_codex_runtime() {
        let mut doc: DocumentMut =
            "[mcp_servers.senate]\ncommand = \"/opt/bin/senate\"\nargs = [\"mcp\"]\n\
             default_tools_approval_mode = \"approve\"\nstartup_timeout_sec = 20\n"
                .parse()
                .unwrap();
        let changes = merge(&mut doc, EXE).unwrap();
        assert_eq!(
            changes[0],
            "~ mcp_servers.senate: /opt/bin/senate mcp  →  /opt/bin/senate mcp --runtime codex"
        );
        let parsed: toml::Value = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["senate"]["args"],
            toml::Value::Array(vec!["mcp".into(), "--runtime".into(), "codex".into()])
        );
        assert_eq!(
            parsed["mcp_servers"]["senate"]["startup_timeout_sec"].as_integer(),
            Some(20)
        );
    }

    #[test]
    fn odd_shapes_are_refused() {
        for bad in [
            "hooks = 1\n",
            "mcp_servers = 3\n",
            "[hooks]\nSessionStart = \"x\"\n",
        ] {
            let mut doc: DocumentMut = bad.parse().unwrap();
            assert!(merge(&mut doc, EXE).is_err(), "{bad}");
        }
    }
}
