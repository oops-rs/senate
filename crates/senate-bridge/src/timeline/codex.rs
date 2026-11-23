use serde_json::{Value, json};

use super::{Event, ToolResult, parse};
use crate::ipc::{
    TimelineKind as Kind, TimelineToolDetail as Detail, TimelineToolStatus as Status,
};

#[derive(Default)]
pub(super) struct Context {
    cwd: Option<String>,
    thread_id: Option<String>,
}

pub(super) fn events(record: &Value, context: &mut Context) -> Vec<Event> {
    let payload = &record["payload"];
    if let Some(root) = &context.thread_id
        && payload["thread_id"].as_str().is_some_and(|id| id != root)
    {
        return Vec::new();
    }
    match record["type"].as_str() {
        Some("session_meta" | "turn_context") => {
            if record["type"] == "session_meta" {
                context.thread_id = parse::string(payload, &["id", "session_id"]);
            }
            if let Some(dir) = payload["cwd"].as_str() {
                context.cwd = Some(dir.to_owned());
            }
            Vec::new()
        }
        Some("response_item") => response(payload, context.cwd.as_deref()),
        Some("event_msg") => native_event(payload),
        Some("compacted") => vec![parse::notice("info", "Context compacted")],
        _ => Vec::new(),
    }
}

fn response(payload: &Value, cwd: Option<&str>) -> Vec<Event> {
    let content = &payload["content"];
    match payload["type"].as_str() {
        Some("message") => match payload["role"].as_str() {
            Some("user") => content
                .as_array()
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "input_text")
                .filter_map(|block| block["text"].as_str())
                .flat_map(parse::user_text)
                .collect(),
            Some("assistant") => parse::block_text(content, "output_text")
                .map(|text| Event::Item(Kind::AssistantMessage { text }))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        },
        Some("reasoning") => parse::block_text(&payload["summary"], "summary_text")
            .or_else(|| parse::block_text(content, "reasoning_text"))
            .filter(|text| !text.is_empty())
            .map(|text| Event::Item(Kind::Reasoning { text }))
            .into_iter()
            .collect(),
        Some("function_call" | "custom_tool_call" | "local_shell_call") => {
            let Some(id) = payload["call_id"]
                .as_str()
                .or_else(|| payload["id"].as_str())
            else {
                return Vec::new();
            };
            let name = payload["name"].as_str().unwrap_or("local_shell");
            let args = arguments(payload, name);
            vec![parse::todo(name, &args).unwrap_or_else(|| parse::tool(id, name, &args, cwd))]
        }
        Some("function_call_output" | "custom_tool_call_output") => {
            let Some(id) = payload["call_id"].as_str() else {
                return Vec::new();
            };
            let raw = &payload["output"];
            let text = raw
                .as_str()
                .map(str::to_owned)
                .or_else(|| parse::block_text(raw, "input_text"))
                .or_else(|| parse::block_text(&raw["content"], "input_text"));
            let exit_code = raw["exit_code"]
                .as_i64()
                .or_else(|| text.as_deref().and_then(exit_code));
            let failed =
                raw["isError"] == true || raw["success"] == false || payload["is_error"] == true;
            vec![Event::Result {
                id: id.to_owned(),
                result: ToolResult {
                    failed,
                    output: text.map(shell_output),
                    exit_code,
                },
            }]
        }
        Some("web_search_call") => {
            let action = &payload["action"];
            let name = if action["type"] == "open_page" {
                "fetch"
            } else {
                "web_search"
            };
            let Some(id) = payload["id"].as_str() else {
                return Vec::new();
            };
            let mut kind = parse::tool_kind(id, name, action, cwd);
            if let Kind::ToolCall { status, .. } = &mut kind {
                *status = finished(payload["status"].as_str());
            }
            vec![Event::Item(kind)]
        }
        _ => Vec::new(),
    }
}

fn arguments(payload: &Value, name: &str) -> Value {
    let input = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .unwrap_or(&payload["action"]);
    if let Some(input) = input.as_str() {
        if let Ok(args) = serde_json::from_str(input) {
            return args;
        }
        if name == "apply_patch" {
            let path = input.lines().find_map(|line| {
                ["*** Update File: ", "*** Add File: ", "*** Delete File: "]
                    .iter()
                    .find_map(|prefix| line.strip_prefix(prefix))
            });
            return json!({"path": path, "patch": input});
        }
    }
    input.clone()
}

fn exit_code(output: &str) -> Option<i64> {
    output.lines().take(8).find_map(|line| {
        let rest = line.strip_prefix("Process exited with code ")?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

fn shell_output(text: String) -> String {
    if text.starts_with("Chunk ID:") {
        for marker in ["\nFinal output:\n", "\nOutput:\n"] {
            if let Some((_, output)) = text.split_once(marker) {
                return output.to_owned();
            }
        }
    }
    text
}

fn finished(value: Option<&str>) -> Status {
    match value {
        Some("failed" | "declined" | "cancelled" | "interrupted") => Status::Failed,
        _ => Status::Completed,
    }
}

fn native_event(payload: &Value) -> Vec<Event> {
    let kind = match payload["type"].as_str() {
        Some("turn_aborted") => return vec![parse::notice("warning", "Turn interrupted")],
        Some("context_compacted") => return vec![parse::notice("info", "Context compacted")],
        Some("error") => {
            return vec![parse::notice(
                "error",
                parse::string(payload, &["message"])
                    .unwrap_or_else(|| "Codex reported an error".into()),
            )];
        }
        Some("item_started" | "item_completed") => payload["type"] == "item_completed",
        _ => return Vec::new(),
    };
    native_tool(&payload["item"], kind)
}

/// Codex code-mode calls put their actual shell and edit operations in
/// native events. The outer call in response_item merely wraps those tools.
fn native_tool(item: &Value, complete: bool) -> Vec<Event> {
    let id = match item["id"].as_str() {
        Some(id) => id,
        None => return Vec::new(),
    };
    let status = if complete {
        finished(item["status"].as_str())
    } else {
        Status::Running
    };
    let failed = status == Status::Failed;
    match item["type"].as_str() {
        Some("CommandExecution") => {
            // Start events (when persisted) and completion events share the
            // same native ID. A completed-only log still shows the command.
            let key = format!("command:{id}");
            vec![Event::Call {
                kind: parse::tool_kind(&key, "exec_command", item, None),
                id: key,
                result: complete.then(|| ToolResult {
                    failed,
                    output: parse::string(item, &["aggregated_output"]),
                    exit_code: item["exit_code"].as_i64(),
                }),
            }]
        }
        Some("FileChange") if complete => item["changes"]
            .as_object()
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, (path, change))| {
                Event::Item(Kind::ToolCall {
                    call_id: format!("file_change:{id}:{index}"),
                    name: "apply_patch".into(),
                    status,
                    detail: Detail::Edit {
                        path: path.clone(),
                        unified_diff: parse::string(change, &["unified_diff"]),
                        old: None,
                        new: None,
                    },
                })
            })
            .collect(),
        Some("Extension") if complete && item["kind"] == "web.search" => {
            vec![Event::Item(Kind::ToolCall {
                call_id: format!("web_search:{id}"),
                name: "web_search".into(),
                status,
                detail: Detail::Search {
                    query: parse::string(item, &["query"]).unwrap_or_default(),
                },
            })]
        }
        _ => Vec::new(),
    }
}
