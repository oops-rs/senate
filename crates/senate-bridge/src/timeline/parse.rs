use serde_json::Value;

use super::Event;
use crate::ipc::{
    TimelineItem, TimelineKind as Kind, TimelineTodo, TimelineToolDetail as Detail,
    TimelineToolStatus,
};

pub(super) fn string(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| value[*key].as_str())
        .map(str::to_owned)
}

pub(super) fn block_text(value: &Value, kind: &str) -> Option<String> {
    let parts: Vec<_> = value
        .as_array()?
        .iter()
        .filter(|block| block["type"] == kind)
        .filter_map(|block| block["text"].as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

pub(super) fn user_text(text: &str) -> Vec<Event> {
    let text = text.trim();
    // Codex adds workspace context as a user role record. Claude can put
    // hook output in a reminder surrounding a genuine user prompt.
    let text = strip_reminders(text);
    let text = text.trim();
    if text.starts_with("[Request interrupted by user") && text.ends_with(']') {
        return vec![notice("warning", "Turn interrupted")];
    }
    if text.is_empty()
        || [
            "# AGENTS.md instructions",
            "<environment_context>",
            "<local-command",
            "<command-name>",
            "<user_action>",
            "<subagent_notification>",
            "<task-notification>",
            "<queued-command",
        ]
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        return Vec::new();
    }
    for prefix in [
        "\n[senate] The messages below come from the Senate committee log",
        "\n[senate] 下面的消息来自 Senate 委员会",
    ] {
        if let Some(index) = text.find(prefix) {
            let mut events = user_text(&text[..index]);
            events.extend(senate_messages(&text[index + 1..]));
            return events;
        }
    }
    if text.starts_with("[senate]") || text.starts_with("[senate · ") {
        return senate_messages(text);
    }
    vec![Event::Item(Kind::UserMessage {
        text: text.to_owned(),
    })]
}

fn strip_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut clean = String::new();
    let mut rest = text;
    while let Some((before, after)) = rest.split_once(OPEN) {
        clean.push_str(before);
        let Some((_, tail)) = after.split_once(CLOSE) else {
            return clean;
        };
        rest = tail;
    }
    clean.push_str(rest);
    clean
}

fn senate_messages(text: &str) -> Vec<Event> {
    let mut messages = Vec::new();
    let mut header = None;
    let mut body = Vec::new();
    for line in text.lines() {
        if let Some(fields) = senate_header(line) {
            if let Some(fields) = header.replace(fields) {
                messages.push(Event::Item(senate_item(fields, &body)));
            }
            body.clear();
        } else if let Some(line) = line.strip_prefix('│') {
            body.push(line.strip_prefix(' ').unwrap_or(line));
        } else if header.is_some() && !line.is_empty() && !line.starts_with('↳') {
            // Some older deliveries omit the body gutter.
            body.push(line);
        }
    }
    if let Some(fields) = header {
        messages.push(Event::Item(senate_item(fields, &body)));
    }
    messages
}

struct SenateHeader<'a> {
    committee: &'a str,
    seq: u64,
    from: &'a str,
    kind: &'a str,
    thread: u64,
}

fn senate_header(line: &str) -> Option<SenateHeader<'_>> {
    let line = line.strip_prefix("[senate · ")?.strip_suffix(']')?;
    let (committee, rest) = line.split_once(" · seq ")?;
    let (seq, rest) = rest.split_once(" · ")?;
    let seq = seq.parse().ok()?;
    let rest = rest
        .strip_prefix("from ")
        .or_else(|| rest.strip_prefix("来自 "))?;
    // Forwarded and cross-Senate deliveries can add byline segments.
    let mut segments = rest.rsplit(" · ");
    let mut label = segments.next()?;
    while label.starts_with("for ") || label.starts_with("替 [") || label.starts_with("给") {
        label = segments.next()?;
    }
    let (from, _) = rest.split_once(" · ")?;
    let from = if from.starts_with('[') {
        from.find(']').map_or(from, |end| &from[..=end])
    } else {
        from
    };
    let thread = label
        .split_once("thread ")
        .and_then(|(_, rest)| {
            let digits = rest
                .as_bytes()
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            rest[..digits].parse().ok()
        })
        .unwrap_or(seq);
    let kind = label.split([' ', '，', '(', '（']).next().unwrap_or(label);
    Some(SenateHeader {
        committee,
        seq,
        from,
        kind,
        thread,
    })
}

fn senate_item(header: SenateHeader<'_>, lines: &[&str]) -> Kind {
    Kind::SenateMessage {
        committee: Some(header.committee.into()),
        seq: Some(header.seq),
        thread: Some(header.thread),
        from: Some(header.from.into()),
        kind: Some(header.kind.into()),
        text: lines.join("\n"),
    }
}

pub(super) fn tool(id: &str, name: &str, args: &Value, cwd: Option<&str>) -> Event {
    Event::Call {
        id: id.to_owned(),
        kind: tool_kind(id, name, args, cwd),
        result: None,
    }
}

pub(super) fn tool_kind(id: &str, name: &str, args: &Value, cwd: Option<&str>) -> Kind {
    let path = || string(args, &["file_path", "path", "filename"]).unwrap_or_default();
    let detail = match name {
        "Bash" | "exec_command" | "shell_command" | "shell" | "local_shell" | "terminal" => {
            Detail::Shell {
                command: string(args, &["command", "cmd"])
                    .or_else(|| {
                        args["command"].as_array().map(|words| {
                            words
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(" ")
                        })
                    })
                    .unwrap_or_default(),
                cwd: string(args, &["workdir", "cwd"]).or_else(|| cwd.map(str::to_owned)),
                output: None,
                exit_code: None,
            }
        }
        "Read" | "read_file" | "view_image" => Detail::Read { path: path() },
        "Edit" | "MultiEdit" | "edit_file" | "apply_patch" => Detail::Edit {
            path: path(),
            unified_diff: string(args, &["unified_diff", "patch", "diff"]),
            old: string(args, &["old_string", "old"]),
            new: string(args, &["new_string", "new"]),
        },
        "Write" | "write_file" => Detail::Write { path: path() },
        "Grep" | "Glob" | "WebSearch" | "search" | "web_search" | "search_query" => {
            Detail::Search {
                query: string(args, &["query", "pattern", "q"]).unwrap_or_default(),
            }
        }
        "WebFetch" | "fetch" | "web_fetch" => Detail::Fetch {
            url: string(args, &["url"]).unwrap_or_default(),
        },
        "Agent" | "Task" | "spawn_agent" => Detail::SubAgent {
            description: string(args, &["description", "task_name", "message", "prompt"])
                .unwrap_or_default(),
        },
        _ => Detail::Other {
            summary: string(args, &["description", "summary"]).unwrap_or_else(|| name.to_owned()),
        },
    };
    Kind::ToolCall {
        call_id: id.to_owned(),
        name: name.to_owned(),
        status: TimelineToolStatus::Running,
        detail,
    }
}

pub(super) fn todo(name: &str, args: &Value) -> Option<Event> {
    let entries = match name {
        "TodoWrite" => args["todos"].as_array()?,
        "update_plan" => args["plan"].as_array()?,
        _ => return None,
    };
    let items = entries
        .iter()
        .filter_map(|entry| {
            Some(TimelineTodo {
                text: string(entry, &["content", "step", "text"])?,
                status: string(entry, &["status"])?,
            })
        })
        .collect();
    Some(Event::Item(Kind::Todo { items }))
}

pub(super) fn notice(level: &str, text: impl Into<String>) -> Event {
    Event::Item(Kind::Notice {
        level: level.to_owned(),
        text: text.into(),
    })
}

/// Each field is capped on a UTF-8 boundary. The additional encoded
/// budget ensures even one item with many control characters fits IPC.
pub(super) fn cap(item: &mut TimelineItem) {
    const MAX_FIELD: usize = 64 * 1024;
    let mut wire_left = 640 * 1024;
    let mut truncated = item.truncated;
    let mut field = |value: &mut String| {
        let mut len = 0;
        for ch in value.chars() {
            let bytes = ch.len_utf8();
            let encoded = match ch {
                '"' | '\\' => 2,
                '\u{0000}'..='\u{001f}' => 6,
                _ => bytes,
            };
            if len + bytes > MAX_FIELD || encoded > wire_left {
                break;
            }
            len += bytes;
            wire_left -= encoded;
        }
        if len != value.len() {
            value.truncate(len);
            truncated = true;
        }
    };
    match &mut item.kind {
        Kind::UserMessage { text } | Kind::AssistantMessage { text } | Kind::Reasoning { text } => {
            field(text)
        }
        Kind::ToolCall {
            call_id,
            name,
            detail,
            ..
        } => {
            field(call_id);
            field(name);
            match detail {
                Detail::Shell {
                    command,
                    cwd,
                    output,
                    ..
                } => {
                    field(command);
                    cwd.iter_mut().for_each(&mut field);
                    output.iter_mut().for_each(&mut field);
                }
                Detail::Edit {
                    path,
                    unified_diff,
                    old,
                    new,
                } => {
                    field(path);
                    unified_diff
                        .iter_mut()
                        .chain(old)
                        .chain(new)
                        .for_each(&mut field);
                }
                Detail::Read { path } | Detail::Write { path } => field(path),
                Detail::Search { query } => field(query),
                Detail::Fetch { url } => field(url),
                Detail::SubAgent { description } => field(description),
                Detail::Other { summary } => field(summary),
            }
        }
        Kind::Todo { items } => {
            let excess = items.len() > 300;
            items.truncate(300);
            for item in items {
                field(&mut item.text);
                field(&mut item.status);
            }
            truncated |= excess;
        }
        Kind::Notice { level, text } => {
            field(level);
            field(text);
        }
        Kind::SenateMessage {
            committee,
            from,
            kind,
            text,
            ..
        } => {
            field(text);
            committee
                .iter_mut()
                .chain(from)
                .chain(kind)
                .for_each(&mut field);
        }
    }
    item.truncated = truncated;
}
