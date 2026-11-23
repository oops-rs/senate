//! Wire fields verified against the Antigravity CLI 1.5.15 descriptors.
//! Decode only the fields used by the timeline. The database is untrusted.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::ipc::{
    TimelineKind as Kind, TimelineToolDetail as Detail, TimelineToolStatus as Status,
};
use crate::timeline::{Event, parse};

#[derive(Debug)]
pub(super) struct Unsupported;

type Result<T> = std::result::Result<T, Unsupported>;

#[derive(Clone, Copy)]
enum Wire<'a> {
    Int(u64),
    Bytes(&'a [u8]),
    Fixed,
}

#[derive(Default)]
struct Fields<'a>(Vec<(u32, Wire<'a>)>);

impl<'a> Fields<'a> {
    fn read(mut input: &'a [u8]) -> Result<Self> {
        let mut fields = Vec::new();
        while !input.is_empty() {
            // Avoid expanding a malformed blob into millions of tiny fields.
            if fields.len() == 65_536 {
                return Err(Unsupported);
            }
            let key = varint(&mut input)?;
            let tag = u32::try_from(key >> 3).map_err(|_| Unsupported)?;
            if tag == 0 || tag >= 1 << 29 {
                return Err(Unsupported);
            }
            let field = match key & 7 {
                0 => Wire::Int(varint(&mut input)?),
                1 | 5 => {
                    take(&mut input, if key & 7 == 1 { 8 } else { 4 })?;
                    Wire::Fixed
                }
                2 => {
                    let len = usize::try_from(varint(&mut input)?).map_err(|_| Unsupported)?;
                    Wire::Bytes(take(&mut input, len)?)
                }
                _ => return Err(Unsupported),
            };
            fields.push((tag, field));
        }
        Ok(Self(fields))
    }

    fn at(&self, tag: u32) -> impl Iterator<Item = Wire<'a>> + '_ {
        self.0
            .iter()
            .filter_map(move |&(n, v)| (n == tag).then_some(v))
    }

    fn one(&self, tag: u32) -> Result<Option<Wire<'a>>> {
        let mut fields = self.at(tag);
        let value = fields.next();
        if fields.next().is_some() {
            return Err(Unsupported);
        }
        Ok(value)
    }

    fn number(&self, tag: u32) -> Result<Option<u64>> {
        match self.one(tag)? {
            Some(Wire::Int(value)) => Ok(Some(value)),
            None => Ok(None),
            _ => Err(Unsupported),
        }
    }

    fn bytes(&self, tag: u32) -> Result<Option<&'a [u8]>> {
        match self.one(tag)? {
            Some(Wire::Bytes(value)) => Ok(Some(value)),
            None => Ok(None),
            _ => Err(Unsupported),
        }
    }

    fn text(&self, tag: u32) -> Result<Option<&'a str>> {
        self.bytes(tag)?
            .map(|value| std::str::from_utf8(value).map_err(|_| Unsupported))
            .transpose()
    }

    fn sub(&self, tag: u32) -> Result<Self> {
        Self::read(self.bytes(tag)?.unwrap_or_default())
    }
}

fn take<'a>(input: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    let (value, tail) = input.split_at_checked(len).ok_or(Unsupported)?;
    *input = tail;
    Ok(value)
}

fn varint(input: &mut &[u8]) -> Result<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let (&byte, tail) = input.split_first().ok_or(Unsupported)?;
        *input = tail;
        if shift == 63 && byte > 1 {
            return Err(Unsupported);
        }
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    Err(Unsupported)
}

pub(super) const USER: i64 = 14;
pub(super) const PLANNER: i64 = 15;
pub(super) const ERROR: i64 = 17;
pub(super) const CHECKPOINT: i64 = 23;
pub(super) const SYSTEM: i64 = 101;
pub(super) const GENERIC: i64 = 132;

pub(super) fn timestamp(metadata: &[u8]) -> Result<Option<DateTime<Utc>>> {
    let meta = Fields::read(metadata)?;
    // CortexStepMetadata.created_at (1), google.protobuf.Timestamp.seconds (1), nanos (2).
    let Some(created) = meta.bytes(1)? else {
        return Ok(None);
    };
    let ts = Fields::read(created)?;
    let nanos = u32::try_from(ts.number(2)?.unwrap_or_default()).map_err(|_| Unsupported)?;
    DateTime::from_timestamp(ts.number(1)?.unwrap_or_default() as i64, nanos)
        .map(Some)
        .ok_or(Unsupported)
}

pub(super) fn has_parent(metadata: &[u8], root_id: &str) -> Result<bool> {
    let meta = Fields::read(metadata)?;
    // CortexTrajectoryMetadata.parent_conversation_id (5), root_conversation_id (6).
    Ok(meta.text(5)?.is_some_and(|id| !id.is_empty())
        || meta
            .text(6)?
            .is_some_and(|id| !id.is_empty() && id != root_id))
}

pub(super) fn running(status: i64) -> bool {
    // CortexStepStatus: pending, running, generating, waiting, queued.
    matches!(status, 1 | 2 | 8 | 9 | 11)
}

pub(super) fn decode(
    idx: i64,
    kind: i64,
    status: i64,
    metadata: &[u8],
    payload: &[u8],
) -> Result<Vec<(u64, Kind)>> {
    if kind == SYSTEM || status == 5 {
        // System context and cleared steps are internal.
        return Ok(Vec::new());
    }
    let step = Fields::read(payload)?;
    // gemini_coder.Step.type (1), status (4). The SQL columns mirror these fields.
    if step.number(1)? != Some(kind as u64) || step.number(4)?.unwrap_or(0) != status as u64 {
        return Err(Unsupported);
    }
    let mut items = Vec::new();
    match kind {
        USER => {
            let meta = Fields::read(metadata)?;
            // CortexStepMetadata.source (3): USER_EXPLICIT = 4, SYSTEM_SDK = 6.
            let source = meta.number(3)?;
            if !matches!(source, Some(4 | 6)) {
                return Ok(items);
            }
            // Step.user_input (19), CortexStepUserInput.user_response (2), items (3).
            let user = required_sub(&step, 19)?;
            let mut text = String::new();
            for item in user.at(3) {
                let Wire::Bytes(item) = item else {
                    return Err(Unsupported);
                };
                // TextOrScopeItem.text (1). Scope items carry no prompt text.
                if let Some(part) = Fields::read(item)?.text(1)? {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(part);
                }
            }
            // When there are text items, user_response duplicates them in 1.5.15.
            let text = if text.is_empty() {
                user.text(2)?.unwrap_or_default()
            } else {
                &text
            };
            for event in parse::user_text(text) {
                if let Event::Item(kind) = event
                    && (source == Some(4) || matches!(kind, Kind::SenateMessage { .. }))
                {
                    items.push((items.len() as u64, kind));
                }
            }
        }
        PLANNER => {
            // Step.planner_response (20). CortexStepPlannerResponse.thinking (3),
            // thinking_redacted (5), response (1), modified_response (8).
            let planner = required_sub(&step, 20)?;
            if planner.number(5)?.unwrap_or_default() == 0
                && let Some(text) = planner.text(3)?.filter(|s| !s.trim().is_empty())
            {
                items.push((0, Kind::Reasoning { text: text.into() }));
            }
            if let Some(text) = planner
                .text(8)?
                .or(planner.text(1)?)
                .filter(|s| !s.trim().is_empty())
            {
                items.push((1, Kind::AssistantMessage { text: text.into() }));
            }
        }
        GENERIC => {
            let kind = tool(idx, status, &Fields::read(metadata)?, &step)?;
            items.push((0, kind));
        }
        CHECKPOINT => {
            // Step.checkpoint (30). Included ranges are internal context.
            required_sub(&step, 30)?;
            items.push((0, notice("info", "Conversation compacted")));
        }
        ERROR => {
            // Step.error_message (24), CortexStepErrorMessage.error (3),
            // should_show_user (5), CortexErrorDetails.user_error_message (1).
            let error = required_sub(&step, 24)?;
            if error.number(5)? == Some(1) {
                let details = error.sub(3)?;
                items.push((
                    0,
                    notice(
                        "error",
                        details.text(1)?.unwrap_or("Antigravity reported an error"),
                    ),
                ));
            }
        }
        _ => return Err(Unsupported),
    }
    if kind == PLANNER && matches!(status, 6 | 12) {
        items.push((2, notice("warning", "Turn interrupted")));
    }
    Ok(items)
}

pub(super) fn notice(level: &str, text: &str) -> Kind {
    Kind::Notice {
        level: level.into(),
        text: text.into(),
    }
}

fn required_sub<'a>(parent: &Fields<'a>, tag: u32) -> Result<Fields<'a>> {
    Fields::read(parent.bytes(tag)?.ok_or(Unsupported)?)
}

fn tool(idx: i64, status: i64, meta: &Fields<'_>, step: &Fields<'_>) -> Result<Kind> {
    // CortexStepMetadata.tool_call (4), modified_tool_call (29).
    // ChatToolCall.id (1), name (2), arguments_json (3).
    let original = meta.sub(4)?;
    let modified = meta.sub(29)?;
    let call_id = original
        .text(1)?
        .map_or_else(|| format!("agy-step:{idx}"), str::to_owned);
    let name = modified.text(2)?.or(original.text(2)?).ok_or(Unsupported)?;
    let args_json = modified.text(3)?.or(original.text(3)?).unwrap_or_default();
    let args: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);

    // Step.generic (140), CortexStepGeneric.result (2), GenericStepResult.result (1).
    let generic = required_sub(step, 140)?;
    let result = generic.sub(2)?;
    let mut output = result.text(1)?.filter(|s| !s.is_empty()).map(str::to_owned);
    if output.is_none() {
        // Step.error (31), CortexErrorDetails.user_error_message (1).
        output = step.sub(31)?.text(1)?.map(str::to_owned);
    }
    let mut tool_status = if running(status) {
        Status::Running
    } else if status == 3 {
        Status::Completed
    } else if matches!(status, 4 | 6 | 7 | 12) {
        Status::Failed
    } else {
        return Err(Unsupported);
    };
    if tool_status == Status::Completed
        && let Some(Event::Item(todo)) = parse::todo(name, &args)
    {
        return Ok(todo);
    }

    let path = || parse::string(&args, &["Path"]).unwrap_or_default();
    let detail = match name {
        "run_command" => {
            // GenericStepResult.payload (6) is Any. Its type_url (1) guards
            // the value (2). RunCommand.exit_code (1), timed_out (2),
            // command_line (6), cwd (7), exit_code_unknown (8).
            let payload = result.sub(6)?;
            let shell = if payload.text(1)?
                == Some("type.googleapis.com/exa.cortex.core.tools.RunCommand")
            {
                Some(payload.sub(2)?)
            } else {
                None
            };
            let (command, cwd, exit_code) = if let Some(shell) = shell {
                let code = if shell.number(8)?.unwrap_or_default() != 0 {
                    None
                } else {
                    Some(i64::from(shell.number(1)?.unwrap_or_default() as i32))
                };
                if code.is_some_and(|n| n != 0) || shell.number(2)?.unwrap_or_default() != 0 {
                    tool_status = Status::Failed;
                }
                (
                    shell.text(6)?.map(str::to_owned),
                    shell.text(7)?.map(str::to_owned),
                    code,
                )
            } else {
                (None, None, None)
            };
            Detail::Shell {
                command: command
                    .or_else(|| parse::string(&args, &["CommandLine"]))
                    .unwrap_or_default(),
                cwd: cwd.or_else(|| parse::string(&args, &["Cwd"])),
                output: output.take(),
                exit_code,
            }
        }
        "read_file" => Detail::Read { path: path() },
        "edit_file" => Detail::Edit {
            path: path(),
            unified_diff: None,
            old: parse::string(&args, &["Target"]),
            new: parse::string(&args, &["Replacement"]),
        },
        "write_file" => Detail::Write { path: path() },
        "search_web" | "search_directory" | "grep_search" => Detail::Search {
            query: parse::string(&args, &["query", "Query"]).unwrap_or_default(),
        },
        "read_url_content" | "fetch_url" => Detail::Fetch {
            url: parse::string(&args, &["URL", "url"]).unwrap_or_default(),
        },
        "invoke_subagent" => Detail::SubAgent {
            description: parse::string(&args, &["Prompt", "Role"]).unwrap_or_default(),
        },
        _ => Detail::Other {
            summary: if args_json.is_empty() {
                name.into()
            } else {
                args_json.into()
            },
        },
    };
    Ok(Kind::ToolCall {
        call_id,
        name: name.into(),
        status: tool_status,
        output,
        detail,
    })
}
