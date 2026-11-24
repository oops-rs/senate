//! Bounded tail of native structured records. Tool previews use only the
//! runtime's call type and correlation key, never a transcript DTO.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::session::Runtime;

const MAX_STEP_BYTES: usize = 256 * 1024;
const MAX_ID_BYTES: usize = 256;

pub(crate) struct Tail {
    path: PathBuf,
    file: File,
    device: u64,
    inode: u64,
    offset: u64,
    skip_to_newline: bool,
    anchor: Vec<u8>,
}

pub(crate) struct Observation {
    pub call_id: String,
    pub kind: Option<String>,
    pub started: bool,
}

/// Resolve only the runtime-owned transcript namespace and filename for this native session.
pub(crate) fn verified_path(
    path: &Path,
    root: &Path,
    runtime: Runtime,
    id: &str,
) -> Option<PathBuf> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    let root = root.canonicalize().ok()?;
    if !path.symlink_metadata().ok()?.is_file() {
        return None;
    }
    let path = path.canonicalize().ok()?;
    if !path.starts_with(root) || path.extension()? != "jsonl" {
        return None;
    }
    let filename = path.file_stem()?.to_str()?;
    let owned_name = match runtime {
        Runtime::Claude => filename == id,
        Runtime::Codex => filename.strip_prefix("rollout-").is_some_and(|name| {
            name.strip_suffix(id)
                .is_some_and(|prefix| prefix.ends_with('-'))
        }),
        _ => false,
    };
    owned_name.then_some(path)
}

impl Tail {
    pub(crate) fn open(path: PathBuf, runtime: Runtime, native_id: &str) -> io::Result<Self> {
        let file = super::open_file(&path)?;
        let metadata = file.metadata()?;
        if runtime == Runtime::Codex && !codex_owner(&file, native_id)? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "transcript session does not match",
            ));
        }
        let offset = metadata.len();
        let anchor = anchor(&file, offset)?;
        Ok(Self {
            path,
            file,
            device: metadata.dev(),
            inode: metadata.ino(),
            offset,
            skip_to_newline: false,
            anchor,
        })
    }

    pub(crate) fn matches(&self, path: &Path) -> bool {
        self.path == path
            && path.symlink_metadata().is_ok_and(|metadata| {
                metadata.is_file()
                    && metadata.dev() == self.device
                    && metadata.ino() == self.inode
                    && metadata.len() >= self.offset
            })
            && anchor(&self.file, self.offset).is_ok_and(|value| value == self.anchor)
    }

    pub(crate) fn read(
        &mut self,
        runtime: Runtime,
        native_session: &str,
        aliases: &BTreeMap<String, String>,
    ) -> io::Result<Vec<Observation>> {
        let mut bytes = vec![0; MAX_STEP_BYTES];
        let len = self.file.read_at(&mut bytes, self.offset)?;
        bytes.truncate(len);
        let mut result = Vec::new();
        let mut consumed = 0;
        for record in bytes.split_inclusive(|byte| *byte == b'\n') {
            if !record.ends_with(b"\n") {
                break;
            }
            consumed += record.len();
            if self.skip_to_newline {
                self.skip_to_newline = false;
                continue;
            }
            if let Ok(value) = serde_json::from_slice(record) {
                classify(runtime, native_session, &value, aliases, &mut result);
            }
        }
        if consumed == 0 && len == MAX_STEP_BYTES {
            self.skip_to_newline = true;
            consumed = len;
        }
        self.offset += consumed as u64;
        if consumed > 0 {
            self.anchor = anchor(&self.file, self.offset)?;
        }
        Ok(result)
    }
}

fn codex_owner(file: &File, native_id: &str) -> io::Result<bool> {
    let mut bytes = vec![0; 64 * 1024];
    let len = file.read_at(&mut bytes, 0)?;
    let first_line = bytes[..len].iter().position(|&byte| byte == b'\n');
    Ok(first_line
        .and_then(|end| serde_json::from_slice::<Value>(&bytes[..end]).ok())
        .is_some_and(|meta| {
            meta["type"] == "session_meta"
                && meta["payload"]["id"]
                    .as_str()
                    .or_else(|| meta["payload"]["session_id"].as_str())
                    == Some(native_id)
        }))
}

fn anchor(file: &File, offset: u64) -> io::Result<Vec<u8>> {
    let mut bytes = vec![0; offset.min(32) as usize];
    file.read_exact_at(&mut bytes, offset.saturating_sub(32))?;
    Ok(bytes)
}

fn classify(
    runtime: Runtime,
    native_session: &str,
    record: &Value,
    aliases: &BTreeMap<String, String>,
    result: &mut Vec<Observation>,
) {
    let mut add = |id: &Value, prefix: &str, kind: Option<String>, started: bool| {
        if let Some(id) = id
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= MAX_ID_BYTES)
        {
            result.push(Observation {
                call_id: format!("{prefix}{id}"),
                kind,
                started,
            });
        }
    };
    match runtime {
        Runtime::Claude
            if record["isSidechain"] != true
                && record["sessionId"].as_str() == Some(native_session) =>
        {
            let assistant = record["type"] == "assistant";
            let user = record["type"] == "user";
            if !assistant && !user {
                return;
            }
            for block in record["message"]["content"]
                .as_array()
                .into_iter()
                .flatten()
            {
                match block["type"].as_str() {
                    Some("tool_use") if assistant => add(
                        &block["id"],
                        "",
                        Some(kind(block["name"].as_str(), aliases)),
                        true,
                    ),
                    Some("tool_result") if user => add(&block["tool_use_id"], "", None, false),
                    _ => {}
                }
            }
        }
        Runtime::Codex => {
            let payload = &record["payload"];
            if payload["thread_id"]
                .as_str()
                .is_some_and(|id| id != native_session)
            {
                return;
            }
            match (record["type"].as_str(), payload["type"].as_str()) {
                (
                    Some("response_item"),
                    Some("function_call" | "custom_tool_call" | "local_shell_call"),
                ) => {
                    let id = if payload["call_id"].is_string() {
                        &payload["call_id"]
                    } else {
                        &payload["id"]
                    };
                    add(
                        id,
                        "",
                        Some(kind(
                            payload["name"].as_str().or(Some("local_shell")),
                            aliases,
                        )),
                        true,
                    );
                }
                (
                    Some("response_item"),
                    Some("function_call_output" | "custom_tool_call_output"),
                ) => {
                    add(&payload["call_id"], "", None, false);
                }
                (Some("event_msg"), Some("item_started" | "item_completed")) => {
                    let item = &payload["item"];
                    let (prefix, class) = match item["type"].as_str() {
                        Some("CommandExecution") => ("command:", "command"),
                        Some("FileChange") => ("edit:", "edit"),
                        Some("Extension") if item["kind"] == "web.search" => ("web:", "web"),
                        _ => return,
                    };
                    add(
                        &item["id"],
                        prefix,
                        Some(class.into()),
                        payload["type"] == "item_started",
                    );
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn kind(name: Option<&str>, aliases: &BTreeMap<String, String>) -> String {
    let name = name.unwrap_or("");
    if let Some((server, _)) = name
        .strip_prefix("mcp__")
        .and_then(|tail| tail.split_once("__"))
    {
        return aliases
            .get(server)
            .map_or_else(|| "other".into(), |alias| format!("mcp:{alias}"));
    }
    match name {
        "Bash" | "exec_command" | "local_shell" | "shell" | "run_command" => "command",
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" | "apply_patch" | "write_file" => "edit",
        "Read" | "read_file" => "read",
        "Grep" | "Glob" | "grep" | "search" | "list_files" => "search",
        "WebSearch" | "WebFetch" | "web_search" | "fetch" => "web",
        _ => "other",
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_runtime_call_types_and_configured_aliases_are_classified() {
        let aliases = BTreeMap::from([("private-server".into(), "directory".into())]);
        let record = serde_json::json!({
            "type": "assistant",
            "sessionId": "native-session",
            "message": { "content": [
                { "type": "tool_use", "id": "call-a", "name": "Bash", "input": { "command": "secret-canary" } },
                { "type": "tool_use", "id": "call-b", "name": "mcp__private-server__find", "input": { "path": "/private" } },
                { "type": "tool_use", "id": "call-c", "name": "mcp__other__find" },
                { "type": "text", "text": "tool_use from terminal" }
            ] }
        });
        let mut observations = Vec::new();
        classify(
            Runtime::Claude,
            "native-session",
            &record,
            &aliases,
            &mut observations,
        );
        assert_eq!(
            observations
                .iter()
                .map(|row| row.kind.as_deref())
                .collect::<Vec<_>>(),
            [Some("command"), Some("mcp:directory"), Some("other")]
        );
        assert!(observations.iter().all(|row| row.started));
        let mut foreign = Vec::new();
        classify(
            Runtime::Codex,
            "native-session",
            &serde_json::json!({
                "type": "response_item", "payload": {
                    "thread_id": "elsewhere", "type": "function_call", "call_id": "call-d", "name": "Read"
                }
            }),
            &aliases,
            &mut foreign,
        );
        assert!(foreign.is_empty());
    }

    #[test]
    fn a_tail_starts_at_demand_and_correlates_appended_records() -> io::Result<()> {
        use std::io::Write;

        let mut file = tempfile::NamedTempFile::new()?;
        writeln!(
            file,
            "{}",
            serde_json::json!({"type":"session_meta","payload":{"id":"native"}})
        )?;
        assert!(Tail::open(file.path().to_owned(), Runtime::Codex, "other").is_err());
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "response_item", "payload": { "type": "function_call", "name": "Read", "call_id": "old" }
            })
        )?;
        let mut tail = Tail::open(file.path().to_owned(), Runtime::Codex, "native")?;
        assert!(
            tail.read(Runtime::Codex, "native", &BTreeMap::new())?
                .is_empty()
        );
        for kind in ["function_call", "function_call_output"] {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "type": "response_item", "payload": { "type": kind, "name": "Read", "call_id": "new" }
                })
            )?;
        }
        let calls = tail.read(Runtime::Codex, "native", &BTreeMap::new())?;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].call_id, calls[1].call_id);
        assert!(calls[0].started && !calls[1].started);
        assert!(
            tail.read(Runtime::Codex, "native", &BTreeMap::new())?
                .is_empty()
        );
        Ok(())
    }
    #[test]
    fn claude_records_from_another_session_are_ignored() {
        let mut observations = Vec::new();
        let record = serde_json::json!({
            "sessionId":"foreign","type":"assistant","message":{"content":[
                {"type":"tool_use","id":"x","name":"Bash"}
            ]}
        });
        classify(
            Runtime::Claude,
            "native",
            &record,
            &BTreeMap::new(),
            &mut observations,
        );
        assert!(observations.is_empty());
    }

    #[test]
    fn rejects_other_roots_and_runtime_filenames() -> io::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("projects");
        std::fs::create_dir(&root)?;
        let good = root.join("native.jsonl");
        let foreign = directory.path().join("native.jsonl");
        std::fs::write(&good, "")?;
        std::fs::write(&foreign, "")?;
        assert!(verified_path(&good, &root, Runtime::Claude, "native").is_some());
        assert!(verified_path(&foreign, &root, Runtime::Claude, "native").is_none());
        assert!(verified_path(&good, &root, Runtime::Claude, "other").is_none());
        Ok(())
    }
}
