use serde_json::Value;

use super::{Event, ToolResult, parse};
use crate::ipc::TimelineKind as Kind;

pub(super) fn events(record: &Value) -> Vec<Event> {
    if record["isSidechain"] == true {
        return Vec::new();
    }
    match record["type"].as_str() {
        Some("user") => user_events(record, true),
        Some("assistant") => record["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|block| match block["type"].as_str()? {
                "text" => Some(Event::Item(Kind::AssistantMessage {
                    text: block["text"].as_str()?.to_owned(),
                })),
                "thinking" => Some(Event::Item(Kind::Reasoning {
                    text: block["thinking"].as_str()?.to_owned(),
                })),
                "tool_use" => {
                    let name = block["name"].as_str()?;
                    let args = &block["input"];
                    parse::todo(name, args).or_else(|| {
                        Some(parse::tool(
                            block["id"].as_str()?,
                            name,
                            args,
                            record["cwd"].as_str(),
                        ))
                    })
                }
                _ => None,
            })
            .collect(),
        Some("system") => match record["subtype"].as_str() {
            Some("compact_boundary" | "microcompact_boundary") => {
                vec![parse::notice("info", "Context compacted")]
            }
            Some("api_error" | "error") => vec![parse::notice(
                "error",
                parse::string(record, &["content", "message"])
                    .unwrap_or_else(|| "Claude reported an error".into()),
            )],
            Some("informational") => parse::string(record, &["content"])
                .map(|text| parse::notice("info", text))
                .into_iter()
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

pub(super) fn user_events(record: &Value, include_results: bool) -> Vec<Event> {
    if record["type"] != "user" || record["isSidechain"] == true {
        return Vec::new();
    }
    let injected = record["isMeta"] == true
        || record["isVisibleInTranscriptOnly"] == true
        || record["isCompactSummary"] == true;
    let content = &record["message"]["content"];
    let user = |text: &str| {
        // Some versions mark a delivered Senate user message as meta.
        if injected && !text.trim_start().starts_with("[senate") {
            Vec::new()
        } else {
            parse::user_text(text)
        }
    };
    if let Some(text) = content.as_str() {
        return user(text);
    }
    let mut events = Vec::new();
    for block in content.as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => {
                if let Some(text) = block["text"].as_str() {
                    events.extend(user(text));
                }
            }
            Some("tool_result") if include_results => {
                if let Some(id) = block["tool_use_id"].as_str() {
                    events.push(result(id, block, &record["toolUseResult"]));
                }
            }
            _ => {}
        }
    }
    events
}

fn result(id: &str, block: &Value, metadata: &Value) -> Event {
    let output = if metadata["stdout"].is_string() || metadata["stderr"].is_string() {
        let parts: Vec<_> = ["stdout", "stderr"]
            .iter()
            .filter_map(|key| metadata[*key].as_str())
            .filter(|text| !text.is_empty())
            .collect();
        Some(parts.join("\n"))
    } else {
        let content = &block["content"];
        content
            .as_str()
            .map(str::to_owned)
            .or_else(|| parse::block_text(content, "text"))
    };
    Event::Result {
        id: id.to_owned(),
        result: ToolResult {
            failed: block["is_error"] == true || metadata["interrupted"] == true,
            output,
            exit_code: metadata["exit_code"]
                .as_i64()
                .or_else(|| metadata["exitCode"].as_i64()),
        },
    }
}
