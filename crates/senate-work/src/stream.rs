//! Claude Code's `--output-format stream-json` (one JSON object per line),
//! read for progress and the final result.
//!
//! The lines this reads:
//!
//! - `{"type":"assistant","message":{"content":[{"type":"tool_use","id":…,"name":…,"input":{…}}]}}`
//!   — a tool call starts: a step.
//! - `{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":…}]}}`
//!   — it finished: the step is done.
//! - `{"type":"result","subtype":"success"|"error_max_turns"|…,"is_error":…,"result":…,"num_turns":…}`
//!   — the run's end.
//!
//! Anything else (system lines, text, unknown shapes) is skipped. Labels
//! never carry code, command lines or file contents: a file is named by its
//! path relative to the checkout, a command by its program if it is a
//! well-known build tool. Labels are in the order's locale.

use senate_core::Locale;
use serde_json::Value;

use crate::runner::RunProgress;

/// Where the checkout is mounted in the container.
pub const CONTAINER_WORKDIR: &str = "/work";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamItem {
    Progress(RunProgress),
    Result(AgentResult),
}

/// The final `result` line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentResult {
    /// `success`, `error_max_turns`, `error_during_execution`, …
    pub subtype: String,
    pub is_error: bool,
    /// The agent's final message.
    pub text: String,
    pub num_turns: Option<u64>,
}

impl AgentResult {
    pub fn succeeded(&self) -> bool {
        self.subtype == "success" && !self.is_error
    }
}

/// Reads a stream line by line, remembering each step's label so that the
/// step's completion carries it too.
#[derive(Debug, Default)]
pub struct StreamParser {
    locale: Locale,
    labels: std::collections::HashMap<String, String>,
}

impl StreamParser {
    pub fn new(locale: Locale) -> Self {
        Self {
            locale,
            labels: Default::default(),
        }
    }

    pub fn push_line(&mut self, line: &str) -> Vec<StreamItem> {
        let mut items = parse_line(line, self.locale);
        for item in &mut items {
            if let StreamItem::Progress(p) = item {
                if p.done {
                    if let Some(label) = self.labels.remove(&p.id) {
                        p.label = label;
                    }
                } else {
                    self.labels.insert(p.id.clone(), p.label.clone());
                }
            }
        }
        items
    }
}

/// Parse one stream-json line into zero or more items, statelessly: a
/// finished step has an empty label (see [`StreamParser`]).
pub fn parse_line(line: &str, locale: Locale) -> Vec<StreamItem> {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Vec::new();
    };
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") => content(&v)
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(|c| {
                let id = c.get("id").and_then(Value::as_str)?;
                let name = c.get("name").and_then(Value::as_str).unwrap_or("");
                let label = tool_label(name, c.get("input").unwrap_or(&Value::Null), locale);
                Some(StreamItem::Progress(RunProgress::started(
                    step_id(id),
                    label,
                )))
            })
            .collect(),
        Some("user") => content(&v)
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("tool_result"))
            .filter_map(|c| {
                let id = c.get("tool_use_id").and_then(Value::as_str)?;
                Some(StreamItem::Progress(RunProgress {
                    id: step_id(id),
                    label: String::new(),
                    done: true,
                }))
            })
            .collect(),
        Some("result") => vec![StreamItem::Result(AgentResult {
            subtype: v
                .get("subtype")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            is_error: v.get("is_error").and_then(Value::as_bool).unwrap_or(false),
            text: v
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            num_turns: v.get("num_turns").and_then(Value::as_u64),
        })],
        _ => Vec::new(),
    }
}

fn content(v: &Value) -> impl Iterator<Item = &Value> {
    v.pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// Step ids are `tool-<id>`, keeping only safe characters.
fn step_id(tool_use_id: &str) -> String {
    let safe: String = tool_use_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .take(64)
        .collect();
    format!("tool-{safe}")
}

/// Build tools whose name alone is safe and useful on a card.
const KNOWN_PROGRAMS: &[&str] = &[
    "cargo",
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "node",
    "go",
    "make",
    "pytest",
    "python",
    "python3",
    "pip",
    "uv",
    "mvn",
    "gradle",
    "./gradlew",
    "swift",
    "xcodebuild",
    "dotnet",
    "bundle",
    "rake",
    "rspec",
    "jest",
    "tsc",
    "eslint",
    "ruff",
    "ls",
    "cat",
    "grep",
    "rg",
    "find",
];

/// A short, safe label for a tool call, in `locale`.
pub fn tool_label(name: &str, input: &Value, locale: Locale) -> String {
    let file = || locale.pick("文件", "file").to_owned();
    let path = || {
        input
            .get("file_path")
            .or_else(|| input.get("notebook_path"))
            .or_else(|| input.get("path"))
            .and_then(Value::as_str)
            .map(|p| display_path(p, locale))
            .unwrap_or_else(file)
    };
    let verb = |zh: &str, en: &str, what: String| match locale {
        Locale::Zh => format!("{zh} {what}"),
        Locale::En => format!("{en} {what}"),
    };
    match name {
        "Read" => verb("读", "Read", path()),
        "Edit" | "MultiEdit" | "NotebookEdit" => verb("改", "Edit", path()),
        "Write" => verb("写", "Write", path()),
        "Glob" | "Grep" | "LS" => locale.pick("搜索代码", "Search the code").to_owned(),
        "Bash" => match input
            .get("command")
            .and_then(Value::as_str)
            .and_then(command_program)
        {
            Some(p) => verb("运行", "Run", p),
            None => locale.pick("运行命令", "Run a command").to_owned(),
        },
        "TodoWrite" => locale.pick("更新计划", "Update the plan").to_owned(),
        "Task" | "Agent" => locale.pick("子任务", "Subtask").to_owned(),
        "WebFetch" | "WebSearch" => locale.pick("查资料", "Look something up").to_owned(),
        other
            if !other.is_empty()
                && other.len() <= 32
                && other
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_') =>
        {
            verb("使用", "Use", other.to_owned())
        }
        _ => locale.pick("工具调用", "Tool call").to_owned(),
    }
}

/// The program (and a plain subcommand) of a shell command, if it is a
/// known build tool and no environment assignment precedes it.
fn command_program(cmd: &str) -> Option<String> {
    let mut words = cmd.split_whitespace();
    let mut prog = words.next()?;
    if prog == "cd" {
        // `cd x && cargo test`: look past the cd.
        let rest = cmd.split_once("&&")?.1;
        return command_program(rest);
    }
    prog = prog.trim_start_matches('(');
    if !KNOWN_PROGRAMS.contains(&prog) {
        return None;
    }
    match words.next() {
        Some(sub)
            if sub.len() <= 16
                && sub
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'-' || b == b'_')
                && !sub.starts_with('-') =>
        {
            Some(format!("{prog} {sub}"))
        }
        _ => Some(prog.to_owned()),
    }
}

/// A path relative to the checkout, or just its file name if it is outside
/// or looks odd.
fn display_path(p: &str, locale: Locale) -> String {
    let rel = p
        .strip_prefix(CONTAINER_WORKDIR)
        .map(|r| r.trim_start_matches('/'))
        .unwrap_or(p);
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars().count() <= 60
            && !s.contains("..")
            && s.chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    };
    if !rel.starts_with('/') && safe(rel) {
        return rel.to_owned();
    }
    let base = rel.rsplit('/').next().unwrap_or("");
    if safe(base) {
        base.to_owned()
    } else {
        locale.pick("文件", "file").to_owned()
    }
}

/// The agent's closing `SUMMARY:` and `NEXT STEPS:` (see [`crate::prompt`]).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Closing {
    pub summary: Option<String>,
    pub next_steps: Vec<String>,
}

/// Find `SUMMARY:` and `NEXT STEPS:` in the agent's final message.
pub fn parse_closing(text: &str) -> Closing {
    let mut out = Closing::default();
    let mut in_steps = false;
    let mut summary = String::new();
    let mut in_summary = false;
    for line in text.lines() {
        let t = line.trim();
        let upper = t.to_ascii_uppercase();
        if upper.starts_with("SUMMARY:") {
            summary = t["SUMMARY:".len()..].trim().to_owned();
            in_summary = true;
            in_steps = false;
            continue;
        }
        if upper.starts_with("NEXT STEPS:") {
            in_steps = true;
            in_summary = false;
            let rest = t["NEXT STEPS:".len()..].trim();
            if !rest.is_empty() {
                out.next_steps.push(rest.to_owned());
            }
            continue;
        }
        if in_steps {
            let item = t
                .trim_start_matches(['-', '*', '•'])
                .trim_start_matches(|c: char| c.is_ascii_digit())
                .trim_start_matches(['.', ')'])
                .trim();
            if !item.is_empty() {
                out.next_steps.push(clip(item, 200));
            }
        } else if in_summary && !t.is_empty() {
            summary.push(' ');
            summary.push_str(t);
        } else if in_summary {
            in_summary = false;
        }
    }
    if !summary.trim().is_empty() {
        out.summary = Some(clip(summary.trim(), 600));
    }
    out.next_steps.truncate(5);
    out
}

/// At most `max` characters, with an ellipsis.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FIXTURE: &str = r#"{"type":"system","subtype":"init","session_id":"s1","tools":["Read","Edit","Bash"],"model":"claude-sonnet-4-5"}
{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","id":"toolu_01A","name":"Read","input":{"file_path":"/work/.senate/progress.md"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_01A","content":"retry path half done"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01B","name":"Edit","input":{"file_path":"/work/src/retry.rs","old_string":"let token = \"sk-secret\";","new_string":"fn retry() {}"}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01C","name":"Bash","input":{"command":"ANTHROPIC_API_KEY=sk-123 curl https://x"}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_01D","name":"Bash","input":{"command":"cargo test -p refund --quiet"}}]}}
not json at all
{"type":"result","subtype":"success","is_error":false,"num_turns":9,"result":"Done.\nSUMMARY: Finished the retry path with backoff.\nNEXT STEPS:\n- Review the backoff cap\n- Merge after CI"}
"#;

    fn progress(items: &[StreamItem]) -> Vec<RunProgress> {
        items
            .iter()
            .filter_map(|i| match i {
                StreamItem::Progress(p) => Some(p.clone()),
                StreamItem::Result(_) => None,
            })
            .collect()
    }

    #[test]
    fn fixture_maps_to_steps_and_result() {
        let items: Vec<StreamItem> = FIXTURE
            .lines()
            .flat_map(|l| parse_line(l, Locale::Zh))
            .collect();
        let steps = progress(&items);
        assert_eq!(
            steps,
            vec![
                RunProgress::started("tool-toolu_01A", "读 .senate/progress.md"),
                RunProgress {
                    id: "tool-toolu_01A".into(),
                    label: String::new(),
                    done: true
                },
                RunProgress::started("tool-toolu_01B", "改 src/retry.rs"),
                RunProgress::started("tool-toolu_01C", "运行命令"),
                RunProgress::started("tool-toolu_01D", "运行 cargo test"),
            ]
        );
        let Some(StreamItem::Result(r)) = items.last() else {
            panic!("no result")
        };
        assert!(r.succeeded());
        assert_eq!(r.num_turns, Some(9));
        let closing = parse_closing(&r.text);
        assert_eq!(
            closing.summary.as_deref(),
            Some("Finished the retry path with backoff.")
        );
        assert_eq!(
            closing.next_steps,
            ["Review the backoff cap", "Merge after CI"]
        );
    }

    #[test]
    fn labels_never_carry_code_or_secrets() {
        let items: Vec<StreamItem> = FIXTURE
            .lines()
            .flat_map(|l| parse_line(l, Locale::Zh))
            .collect();
        for p in progress(&items) {
            for bad in ["sk-", "secret", "curl", "fn ", "token"] {
                assert!(!p.label.contains(bad), "{bad} in {:?}", p.label);
            }
        }
    }

    #[test]
    fn parser_carries_labels_to_done_steps() {
        let mut parser = StreamParser::new(Locale::Zh);
        let items: Vec<StreamItem> = FIXTURE.lines().flat_map(|l| parser.push_line(l)).collect();
        assert_eq!(
            progress(&items)[1],
            RunProgress::finished("tool-toolu_01A", "读 .senate/progress.md")
        );
    }

    #[test]
    fn error_results() {
        let items = parse_line(
            r#"{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":200}"#,
            Locale::En,
        );
        let [StreamItem::Result(r)] = items.as_slice() else {
            panic!("{items:?}")
        };
        assert!(!r.succeeded());
        assert_eq!(r.subtype, "error_max_turns");
        assert_eq!(r.text, "");
    }

    #[test]
    fn tool_labels() {
        assert_eq!(
            tool_label("Write", &json!({"file_path": "/etc/passwd"}), Locale::Zh),
            "写 passwd"
        );
        assert_eq!(
            tool_label(
                "Edit",
                &json!({"file_path": "/work/../../etc/x"}),
                Locale::Zh
            ),
            "改 x"
        );
        assert_eq!(
            tool_label("Grep", &json!({"pattern": "password"}), Locale::Zh),
            "搜索代码"
        );
        assert_eq!(
            tool_label(
                "Bash",
                &json!({"command": "cd crates && cargo build"}),
                Locale::Zh
            ),
            "运行 cargo build"
        );
        assert_eq!(
            tool_label(
                "Bash",
                &json!({"command": "npm --token x publish"}),
                Locale::Zh
            ),
            "运行 npm"
        );
        assert_eq!(
            tool_label(
                "Bash",
                &json!({"command": "git push origin main"}),
                Locale::Zh
            ),
            "运行命令"
        );
        assert_eq!(
            tool_label("mcp__x__y", &json!({}), Locale::Zh),
            "使用 mcp__x__y"
        );
        assert_eq!(
            tool_label("weird name;rm", &json!({}), Locale::Zh),
            "工具调用"
        );
    }

    #[test]
    fn tool_labels_in_english() {
        let en = |name: &str, input: Value| tool_label(name, &input, Locale::En);
        assert_eq!(
            en("Read", json!({"file_path": "/work/.senate/PROGRESS.md"})),
            "Read .senate/PROGRESS.md"
        );
        assert_eq!(en("Edit", json!({})), "Edit file");
        assert_eq!(
            en("Bash", json!({"command": "cargo test -q"})),
            "Run cargo test"
        );
        assert_eq!(en("Bash", json!({"command": "git push"})), "Run a command");
        assert_eq!(en("Grep", json!({})), "Search the code");
        assert_eq!(en("mcp__x__y", json!({})), "Use mcp__x__y");
        assert_eq!(en("weird name;rm", json!({})), "Tool call");
        assert_eq!(
            en("Write", json!({"file_path": "/work/名字 with space"})),
            "Write file"
        );
    }

    #[test]
    fn closing_variants() {
        let c = parse_closing("summary: a\ncontinued\n\nNext steps: one\n1. two\n* three");
        assert_eq!(c.summary.as_deref(), Some("a continued"));
        assert_eq!(c.next_steps, ["one", "two", "three"]);
        assert_eq!(parse_closing("no markers here"), Closing::default());
    }
}
