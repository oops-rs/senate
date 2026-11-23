//! A session's conversation, as the daemon reads it from the runtime's
//! transcript ([`senate_bridge::ipc::TimelineReply`]): pages read on demand
//! and a watch that follows it live. Like the snapshot, what is written for
//! people is in the app's language, and senators are bracketed mentions.

use std::time::SystemTime;

use senate_bridge::ipc::{
    TimelineItem, TimelineKind, TimelineReply, TimelineToolDetail, TimelineToolStatus,
    TimelineUnavailable, TimelineUpdate,
};
use senate_bridge::session::Runtime;
use senate_core::Locale;

/// One page of a session's conversation, oldest first.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TimelinePage {
    pub session_id: String,
    /// Senate can read this session's transcript.
    pub available: bool,
    /// Why there is nothing to read, when there is not.
    pub unavailable: Option<String>,
    pub items: Vec<TimelineEntry>,
    /// There is more before the first item.
    pub has_older: bool,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    /// The runtime is mid-turn.
    pub working: bool,
}

/// What a watch says: items appended or replaced in place (same `seq`),
/// whether the runtime works, or that the transcript changed and the page
/// is to be read again (`reset`).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TimelineDelta {
    pub items: Vec<TimelineEntry>,
    pub working: bool,
    pub reset: bool,
}

/// One item of a conversation.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TimelineEntry {
    /// Stable: a later update of the item (a tool call completing) has the
    /// same seq.
    pub seq: u64,
    pub at: Option<SystemTime>,
    /// The daemon cut it short.
    pub truncated: bool,
    pub body: EntryBody,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum EntryBody {
    /// What the person (or whoever drove the session) typed.
    User {
        text: String,
    },
    /// The agent's reply, Markdown.
    Assistant {
        text: String,
    },
    /// Its reasoning, shown dimmed and folded.
    Reasoning {
        text: String,
    },
    Tool {
        call: ToolCall,
    },
    Todo {
        items: Vec<TodoLine>,
    },
    /// A line from the runtime itself (`level`: `info`, `warning`, `error`).
    Notice {
        level: String,
        text: String,
    },
    /// A Senate message delivered to the session.
    Senate {
        message: SenateLine,
    },
}

/// One tool call: what it ran or touched, how it went, what came of it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ToolCall {
    pub call_id: String,
    /// The tool's own name (`Bash`, `Read`, `mcp__senate__ask`).
    pub name: String,
    /// What kind: `shell`, `read`, `edit`, `write`, `search`, `fetch`,
    /// `sub_agent`, `other`. The app picks an icon by it.
    pub kind: String,
    /// `running`, `completed`, `failed`.
    pub status: String,
    pub status_label: String,
    /// The command, path, query, URL or description: its one line.
    pub title: String,
    /// Where a command ran.
    pub cwd: Option<String>,
    /// What a command printed.
    pub output: Option<String>,
    pub exit_code: Option<i64>,
    /// An edit as a unified diff.
    pub diff: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct TodoLine {
    pub text: String,
    /// `pending`, `in_progress`, `completed`.
    pub status: String,
}

/// A Senate message the session received.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SenateLine {
    pub committee: Option<String>,
    pub seq: Option<u64>,
    /// The root of its thread, which the card opens.
    pub thread: Option<u64>,
    /// Who sent it, as a mention: `[senator::codex]`.
    pub from: Option<String>,
    /// `question`, `note`, ….
    pub kind: Option<String>,
    pub kind_label: Option<String>,
    pub text: String,
}

/// Why a session has nothing to read.
pub fn unavailable(locale: Locale, runtime: Runtime) -> String {
    if crate::tree::readable(runtime) {
        locale
            .pick(
                "还没有可读的记录：会话开始工作后这里会显示它的对话。",
                "No transcript to read yet: its conversation shows here once it starts working.",
            )
            .to_string()
    } else {
        match locale {
            Locale::Zh => format!(
                "Senate 还读不了 {} 会话的对话。接入终端可以看到它。",
                runtime.as_str()
            ),
            Locale::En => format!(
                "Senate cannot read {} sessions' conversations yet. Attach to see it.",
                runtime.as_str()
            ),
        }
    }
}

pub fn page(reply: TimelineReply, locale: Locale) -> TimelinePage {
    TimelinePage {
        unavailable: (!reply.available).then(|| {
            match reply.unavailable_reason {
            Some(TimelineUnavailable::UnsupportedSchema) => locale.pick(
                "Antigravity 的对话记录格式暂不支持。请接入终端查看。",
                "This Antigravity conversation format is unsupported. Attach to see it.",
            ).into(),
            Some(TimelineUnavailable::TemporarilyUnreadable) => locale.pick(
                "暂时无法读取 Antigravity 的对话记录。请重试或接入终端查看。",
                "This Antigravity conversation could not be read. Try again or attach to see it.",
            ).into(),
            None => unavailable(locale, reply.runtime),
        }
        }),
        session_id: reply.session_id,
        available: reply.available,
        items: reply.items.iter().map(|i| entry(i, locale)).collect(),
        has_older: reply.has_older,
        first_seq: reply.first_seq,
        last_seq: reply.last_seq,
        working: reply.working,
    }
}

pub fn delta(update: TimelineUpdate, locale: Locale) -> TimelineDelta {
    TimelineDelta {
        items: update.items.iter().map(|i| entry(i, locale)).collect(),
        working: update.working,
        reset: update.reset,
    }
}

fn status(locale: Locale, s: TimelineToolStatus) -> (&'static str, &'static str) {
    match s {
        TimelineToolStatus::Running => ("running", locale.pick("运行中", "running")),
        TimelineToolStatus::Completed => ("completed", locale.pick("完成", "done")),
        TimelineToolStatus::Failed => ("failed", locale.pick("失败", "failed")),
    }
}

pub fn entry(item: &TimelineItem, locale: Locale) -> TimelineEntry {
    let body = match &item.kind {
        TimelineKind::UserMessage { text } => EntryBody::User { text: text.clone() },
        TimelineKind::AssistantMessage { text } => EntryBody::Assistant { text: text.clone() },
        TimelineKind::Reasoning { text } => EntryBody::Reasoning { text: text.clone() },
        TimelineKind::ToolCall {
            call_id,
            name,
            status: s,
            output,
            detail,
        } => {
            let (wire, label) = status(locale, *s);
            let mut call = ToolCall {
                call_id: call_id.clone(),
                name: name.clone(),
                kind: String::new(),
                status: wire.to_string(),
                status_label: label.to_string(),
                title: String::new(),
                cwd: None,
                output: output.clone(),
                exit_code: None,
                diff: None,
            };
            let (kind, title) = match detail {
                TimelineToolDetail::Shell {
                    command,
                    cwd,
                    output,
                    exit_code,
                } => {
                    call.cwd.clone_from(cwd);
                    if output.is_some() {
                        call.output.clone_from(output);
                    }
                    call.exit_code = *exit_code;
                    ("shell", command.clone())
                }
                TimelineToolDetail::Read { path } => ("read", path.clone()),
                TimelineToolDetail::Edit {
                    path,
                    unified_diff,
                    old,
                    new,
                } => {
                    call.diff = unified_diff
                        .clone()
                        .or_else(|| replace_diff(old.as_deref(), new.as_deref()));
                    ("edit", path.clone())
                }
                TimelineToolDetail::Write { path } => ("write", path.clone()),
                TimelineToolDetail::Search { query } => ("search", query.clone()),
                TimelineToolDetail::Fetch { url } => ("fetch", url.clone()),
                TimelineToolDetail::SubAgent { description } => ("sub_agent", description.clone()),
                TimelineToolDetail::Other { summary } => ("other", summary.clone()),
            };
            call.kind = kind.to_string();
            call.title = if title.trim().is_empty() {
                name.clone()
            } else {
                title
            };
            EntryBody::Tool { call }
        }
        TimelineKind::Todo { items } => EntryBody::Todo {
            items: items
                .iter()
                .map(|t| TodoLine {
                    text: t.text.clone(),
                    status: t.status.clone(),
                })
                .collect(),
        },
        TimelineKind::Notice { level, text } => EntryBody::Notice {
            level: level.clone(),
            text: text.clone(),
        },
        TimelineKind::SenateMessage {
            committee,
            seq,
            thread,
            from,
            kind,
            text,
        } => EntryBody::Senate {
            message: SenateLine {
                committee: committee.clone(),
                seq: *seq,
                thread: thread.or(*seq),
                from: from.as_deref().map(mention),
                kind_label: kind
                    .as_deref()
                    .map(|k| crate::text::kind(locale, &senate_core::Kind::parse(k))),
                kind: kind.clone(),
                text: text.clone(),
            },
        },
    };
    TimelineEntry {
        seq: item.seq,
        at: item.ts.map(Into::into),
        truncated: item.truncated,
        body,
    }
}

/// `senator::codex` (or a bare `codex`) as the bracketed mention the app
/// names: `[senator::codex]`.
fn mention(from: &str) -> String {
    let from = from.trim().trim_start_matches('[').trim_end_matches(']');
    if from.contains("::") {
        format!("[{from}]")
    } else {
        format!("[senator::{from}]")
    }
}

/// An edit given as the text replaced and its replacement, as a diff.
fn replace_diff(old: Option<&str>, new: Option<&str>) -> Option<String> {
    if old.is_none() && new.is_none() {
        return None;
    }
    let mut out = String::new();
    for l in old.unwrap_or_default().lines() {
        out.push('-');
        out.push_str(l);
        out.push('\n');
    }
    for l in new.unwrap_or_default().lines() {
        out.push('+');
        out.push_str(l);
        out.push('\n');
    }
    Some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use senate_bridge::ipc::TimelineTodo;

    fn item(kind: TimelineKind) -> TimelineItem {
        TimelineItem {
            seq: 7,
            ts: None,
            truncated: false,
            kind,
        }
    }

    #[test]
    fn agy_unavailability_is_specific_and_tool_results_reach_the_existing_output_view() {
        let old_reply = serde_json::json!({
            "session_id":"agy:fixture", "runtime":"agy", "available":false, "items":[],
            "has_older":false, "has_newer":false, "working":true
        });
        let mut reply: TimelineReply = serde_json::from_value(old_reply).unwrap();
        assert!(
            page(reply.clone(), Locale::En)
                .unavailable
                .unwrap()
                .contains("No transcript")
        );
        reply.unavailable_reason = Some(TimelineUnavailable::UnsupportedSchema);
        let unavailable = page(reply, Locale::En);
        assert!(!unavailable.available && unavailable.working);
        assert!(
            unavailable
                .unavailable
                .unwrap()
                .contains("format is unsupported")
        );

        let entry = entry(
            &item(TimelineKind::ToolCall {
                call_id: "read-fixture".into(),
                name: "read_file".into(),
                status: TimelineToolStatus::Completed,
                output: Some("fixture result".into()),
                detail: TimelineToolDetail::Read {
                    path: "fixture.txt".into(),
                },
            }),
            Locale::En,
        );
        let EntryBody::Tool { call } = entry.body else {
            panic!("expected tool")
        };
        assert_eq!(call.title, "fixture.txt");
        assert_eq!(call.output.as_deref(), Some("fixture result"));
    }

    #[test]
    fn a_shell_call_reads_as_its_command_and_output() {
        let e = entry(
            &item(TimelineKind::ToolCall {
                call_id: "c".into(),
                name: "Bash".into(),
                status: TimelineToolStatus::Failed,
                output: None,
                detail: TimelineToolDetail::Shell {
                    command: "cargo test".into(),
                    cwd: None,
                    output: Some("1 failed".into()),
                    exit_code: Some(101),
                },
            }),
            Locale::Zh,
        );
        let EntryBody::Tool { call } = e.body else {
            panic!("a tool call")
        };
        assert_eq!(
            (
                call.kind.as_str(),
                call.title.as_str(),
                call.status.as_str()
            ),
            ("shell", "cargo test", "failed")
        );
        assert_eq!(call.status_label, "失败");
        assert_eq!(call.exit_code, Some(101));
    }

    #[test]
    fn an_edit_without_a_diff_shows_what_it_replaced() {
        let e = entry(
            &item(TimelineKind::ToolCall {
                call_id: "c".into(),
                name: "Edit".into(),
                status: TimelineToolStatus::Completed,
                output: None,
                detail: TimelineToolDetail::Edit {
                    path: "a.rs".into(),
                    unified_diff: None,
                    old: Some("a\nb".into()),
                    new: Some("c".into()),
                },
            }),
            Locale::En,
        );
        let EntryBody::Tool { call } = e.body else {
            panic!("a tool call")
        };
        assert_eq!(call.diff.as_deref(), Some("-a\n-b\n+c\n"));
    }

    #[test]
    fn a_senate_message_names_its_sender_and_thread() {
        let e = entry(
            &item(TimelineKind::SenateMessage {
                committee: Some("refund-v2".into()),
                seq: Some(131),
                thread: None,
                from: Some("senator::codex".into()),
                kind: Some("question".into()),
                text: "which env?".into(),
            }),
            Locale::En,
        );
        let EntryBody::Senate { message } = e.body else {
            panic!("a Senate message")
        };
        assert_eq!(message.from.as_deref(), Some("[senator::codex]"));
        assert_eq!(message.thread, Some(131));
        assert!(message.kind_label.is_some());
    }

    #[test]
    fn todos_keep_their_order_and_state() {
        let e = entry(
            &item(TimelineKind::Todo {
                items: vec![TimelineTodo {
                    text: "x".into(),
                    status: "in_progress".into(),
                }],
            }),
            Locale::En,
        );
        assert!(matches!(e.body, EntryBody::Todo { items } if items[0].status == "in_progress"));
    }

    #[test]
    fn why_there_is_nothing_to_read_in_either_language() {
        assert!(unavailable(Locale::En, Runtime::Agy).contains("No transcript"));
        assert!(unavailable(Locale::Zh, Runtime::Claude).contains("记录"));
    }
}
