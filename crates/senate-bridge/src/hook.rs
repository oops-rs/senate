//! `senate hook <session-start|user-prompt-submit|session-end>`.
//!
//! Claude Code runs this with the hook JSON on stdin (`session_id`, `cwd`,
//! `transcript_path`, `hook_event_name`, `source`, `prompt`, `reason`, …) and
//! the session's environment, which carries `CLAUDE_CODE_MESSAGING_SOCKET`,
//! `CLAUDE_CODE_MESSAGING_TOKEN`, `CLAUDE_CODE_SESSION_ID` and `CLAUDE_PID`.
//!
//! The hook registers or refreshes the session with the daemon, reporting
//! what decides which agent the session is ([`crate::agent`]):
//! `SENATE_AGENT` from its environment and the worktree's `.senate/agent`.
//! For
//! SessionStart and UserPromptSubmit, prints
//! `{"hookSpecificOutput":{"hookEventName":…,"additionalContext":…}}` when
//! something is waiting for the session.
//!
//! Claude's pid ties the session to its MCP shim (whose parent is Claude).
//! The hook measures its parent chain when `CLAUDE_PID` is missing or the
//! hook runs under a hosted Claude TUI. A parent shell
//! exits quickly, so the chain is not evidence of liveness.
//!
//! Codex runs the same hooks (`hook <event> --runtime codex`, from its
//! `config.toml`) inside the app-server, with the same input shape and the
//! same `hookSpecificOutput`; `session_id` is the Codex thread id. A Codex
//! session reports no socket, token, pids or `SENATE_AGENT` (the
//! app-server's environment belongs to none of its threads). Its context
//! carries the
//! orientation and brief only: queued messages reach it as turns through
//! the app-server, since Codex leaves hook context unanswered. Without `--runtime codex` a Codex hook
//! is recognised by its rollout transcript ([`detect_runtime`]).
//!
//! agy runs `hook session-start|user-prompt-submit --runtime agy` as its
//! SessionStart and PreInvocation hooks (`~/.gemini/config/hooks.json`):
//! camelCase input naming the root `conversationId`, and `injectSteps` for
//! output ([`agy_output`]). It takes what is waiting for it there, before
//! each model call. A Senate-hosted agy's hook also names its host
//! ([`crate::host::HOSTED_AGY_ENV`]): agy picks its own conversation ID, and
//! the host's launch learns it from that first hook.
//!
//! Hook failures exit 0 with no output. A stalled stdin read has one
//! second, followed by up to 200 ms of best-effort diagnostics.
//! In a companion session (`SENATE_COMPANION` set, [`crate::companion`])
//! it does nothing at all.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::host_approval::observed_at;
use crate::ipc::{self, ClaudeApprovalSignal, HookEvent, HookReply, HookReport, Request};
use crate::session::{self, Runtime};
use senate_core::EventId;

mod diagnostics;
pub use diagnostics::{DROP_REPORT_BUDGET, STDIN_BUDGET, report_stdin_timeout};

/// How many pids above the hook are reported when measuring ancestry.
pub const ANCESTOR_DEPTH: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum HookKind {
    SessionStart,
    UserPromptSubmit,
    SessionEnd,
    HostedApproval,
    /// agy's `Stop`: the end of a turn, not of the conversation.
    Stop,
}

impl HookKind {
    /// The `hookEventName` Claude Code expects back.
    pub fn event_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionEnd => "SessionEnd",
            Self::HostedApproval => "HostedApproval",
            Self::Stop => "Stop",
        }
    }
}

/// The parts of Claude Code's hook input the bridge reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgyHookInput {
    conversation_id: String,
    cwd: Option<PathBuf>,
    transcript_path: Option<String>,
    #[serde(default)]
    workspace_paths: Vec<PathBuf>,
}

impl AgyHookInput {
    /// The input as Claude Code's. `cwd` is `None` when agy gave none:
    /// its TUI (1.5.14) sends an empty `cwd` and no `workspacePaths`, while
    /// its print mode sends both.
    fn normalize(mut self) -> Option<HookInput> {
        let cwd = self
            .cwd
            .take()
            .filter(|cwd| !cwd.as_os_str().is_empty())
            .or_else(|| (self.workspace_paths.len() == 1).then(|| self.workspace_paths.remove(0)));
        if cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
            return None;
        }
        Some(HookInput {
            session_id: Some(session::agy_session_key(&self.conversation_id)?),
            cwd,
            transcript_path: self.transcript_path,
            ..HookInput::default()
        })
    }
}

/// What the hook reads from its environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookEnv {
    pub socket: Option<PathBuf>,
    pub token: Option<String>,
    pub claude_pid: Option<u32>,
    pub session_id: Option<String>,
    /// `SENATE_AGENT`: the agent name this session pins.
    pub agent: Option<String>,
}

/// Codex hook input looks like Claude Code's; what tells them apart is the
/// transcript, a Codex rollout file (`…/sessions/…/rollout-<ts>-<thread>.jsonl`).
pub fn detect_runtime(input: &HookInput) -> Runtime {
    let rollout = input
        .transcript_path
        .as_deref()
        .and_then(|p| Path::new(p).file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
    if rollout {
        Runtime::Codex
    } else {
        Runtime::Claude
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

impl HookEnv {
    /// The pid comes only from `CLAUDE_PID`. The hook's parent may be a
    /// short-lived `sh -c` wrapper, and a wrong pid would get the session
    /// marked stale; with no pid, liveness rests on the inbox socket alone.
    ///
    /// A process confined to a test's directory ([`crate::testenv`]) takes
    /// no inbox socket from outside it, nor its token.
    pub fn from_process() -> Self {
        let socket = env_nonempty("CLAUDE_CODE_MESSAGING_SOCKET").map(PathBuf::from);
        let (socket, token) = match socket {
            Some(socket) if !crate::testenv::confined(&socket) => {
                eprintln!(
                    "senate hook: ignoring the inbox socket {}: outside the directory this \
                     process is confined to",
                    socket.display()
                );
                (None, None)
            }
            socket => (socket, env_nonempty("CLAUDE_CODE_MESSAGING_TOKEN")),
        };
        Self {
            socket,
            token,
            claude_pid: env_nonempty("CLAUDE_PID").and_then(|p| p.parse().ok()),
            session_id: env_nonempty("CLAUDE_CODE_SESSION_ID"),
            agent: env_nonempty(crate::agent::AGENT_ENV),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HookTimeouts {
    pub connect: Duration,
    pub request: Duration,
    pub git: Duration,
}

impl Default for HookTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_millis(300),
            request: Duration::from_secs(2),
            git: Duration::from_millis(800),
        }
    }
}

/// The JSON line to print, if any.
pub fn output_for(kind: HookKind, context: &str) -> String {
    json!({
        "hookSpecificOutput": {
            "hookEventName": kind.event_name(),
            "additionalContext": context,
        }
    })
    .to_string()
}

/// What an agy hook prints: `injectSteps` for agy to add to the
/// conversation. A `systemMessage` step carries system authority there, so
/// only the identity Senate itself constructs goes in one. Everything else
/// the daemon hands back (the brief, messages from the log) is one
/// `userMessage` step, already framed as peer input, the way agy's own
/// `send-message` would put it.
fn agy_output(kind: HookKind, reply: &HookReply) -> Option<String> {
    let mut steps = Vec::new();
    if kind == HookKind::SessionStart
        && let Some(committee) = reply.session.committee()
    {
        let seat = senate_core::Address::senator(&reply.session.agent).in_text();
        let identity = format!("[senate] {committee} · {seat}");
        steps.push(json!({"systemMessage": {"systemMessage": identity}}));
    }
    if let Some(context) = reply.context.as_deref().filter(|c| !c.is_empty()) {
        steps.push(json!({"userMessage": context}));
    }
    (!steps.is_empty()).then(|| json!({"injectSteps": steps}).to_string())
}

/// agy's `Stop` hook. When something is waiting for the session as its turn
/// ends, keep the agent going: its next model call's PreInvocation hook
/// hands the messages over as a user message. The reason, which agy adds
/// as a system message, is Senate's own fixed text.
async fn turn_end(
    daemon_socket: &Path,
    stdin: &str,
    runtime: Option<Runtime>,
    timeouts: HookTimeouts,
) -> Option<String> {
    if std::env::var_os(crate::companion::ENV_COMPANION).is_some() {
        return None;
    }
    let session_id = if runtime == Some(Runtime::Agy) {
        let input = serde_json::from_str::<AgyHookInput>(stdin).ok()?;
        session::agy_session_key(&input.conversation_id)?
    } else {
        serde_json::from_str::<HookInput>(stdin).ok()?.session_id?
    };
    let reply: ipc::WaitingReply = ipc::request(
        daemon_socket,
        &Request::Waiting { session_id },
        timeouts.connect,
        timeouts.request,
    )
    .await
    .ok()?;
    if !reply.unanswered.is_empty() {
        let threads = reply
            .unanswered
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Some(json!({
            "decision": if runtime == Some(Runtime::Agy) { "continue" } else { "block" },
            "reason": format!("[senate] Your relayed question is still open (thread {threads}). Use mcp__senate__answer on each thread to send your conclusion to the asker, or report progress with mcp__senate__status.")
        }).to_string());
    }
    (reply.waiting > 0).then(|| {
        json!({
            "decision": "continue",
            "reason": "[senate] New Senate messages arrived for this session. They are given to you next; read and act on them.",
        })
        .to_string()
    })
}

/// Run one hook against the daemon at `daemon_socket`. Returns the line to
/// print on stdout, or `None` (print nothing). Never fails.
pub async fn run(
    kind: HookKind,
    daemon_socket: &Path,
    stdin: &str,
    env: &HookEnv,
    timeouts: HookTimeouts,
) -> Option<String> {
    run_as(kind, daemon_socket, stdin, env, None, timeouts).await
}

/// [`run`] for a known runtime: `Some(Codex)` for `hook <event> --runtime codex`
/// (what `install` writes for Codex); `None` infers it from the input
/// ([`detect_runtime`]).
pub async fn run_as(
    kind: HookKind,
    daemon_socket: &Path,
    stdin: &str,
    env: &HookEnv,
    runtime: Option<Runtime>,
    timeouts: HookTimeouts,
) -> Option<String> {
    if kind == HookKind::Stop {
        return turn_end(daemon_socket, stdin, runtime, timeouts).await;
    }
    if kind == HookKind::HostedApproval {
        // The candidate marker is checked before any IPC connection. Most
        // PostToolUse hooks have no approval to finish.
        let request = prepare(kind, daemon_socket, stdin, env, runtime, timeouts.git).await?;
        let _: Value = ipc::request(daemon_socket, &request, timeouts.connect, timeouts.request)
            .await
            .ok()?;
        return None;
    }
    // Connect before asking git anything: a down daemon costs milliseconds.
    let mut client = ipc::IpcClient::connect(daemon_socket, timeouts.connect)
        .await
        .ok()?;
    let request = prepare(kind, daemon_socket, stdin, env, runtime, timeouts.git).await?;
    match kind {
        HookKind::SessionEnd => {
            let _: serde_json::Value = client.call(&request, timeouts.request).await.ok()?;
            None
        }
        _ => {
            let reply: HookReply = client.call(&request, timeouts.request).await.ok()?;
            if reply.session.runtime == Runtime::Agy {
                return agy_output(kind, &reply);
            }
            reply.context.map(|context| output_for(kind, &context))
        }
    }
}

/// Build a hook's IPC report without sending it.
pub async fn prepare(
    kind: HookKind,
    daemon_socket: &Path,
    stdin: &str,
    env: &HookEnv,
    runtime: Option<Runtime>,
    git_timeout: Duration,
) -> Option<Request> {
    // A companion session (a headless fork the daemon runs) is not an
    // interactive session: it must not register, nor take pushes.
    if std::env::var_os(crate::companion::ENV_COMPANION).is_some() {
        return None;
    }
    if kind == HookKind::HostedApproval {
        let host_id = std::env::var(crate::host::HOSTED_CLAUDE_ENV)
            .ok()?
            .parse()
            .ok()?;
        let hook: Value = serde_json::from_str(stdin).ok()?;
        let signal = approval_signal(&hook)?;
        if matches!(signal, ClaudeApprovalSignal::Finished { .. }) {
            let paths = crate::paths::Paths::new(daemon_socket.parent()?);
            if !crate::host::approval_candidate(&paths, &host_id).exists() {
                return None;
            }
        }
        return Some(Request::HostedClaudeApproval {
            host_id,
            at: observed_at(),
            session_id: hook.get("session_id")?.as_str()?.to_owned(),
            signal,
        });
    }
    let input: HookInput = if runtime == Some(Runtime::Agy) {
        // Antigravity's Stop ends an invocation, not the conversation.
        if kind == HookKind::SessionEnd {
            return None;
        }
        let mut input = serde_json::from_str::<AgyHookInput>(stdin)
            .ok()?
            .normalize()?;
        if input.cwd.is_none() {
            // agy runs its hooks in the directory of hooks.json; agy itself
            // runs where the session works.
            input.cwd = session::ancestor_cwd(git_timeout).await;
        }
        input
    } else {
        serde_json::from_str(stdin).ok()?
    };
    let runtime = runtime.unwrap_or_else(|| detect_runtime(&input));
    // A Codex hook runs in the app-server, whose environment may carry a
    // Claude Code session's variables (a daemon started from inside one):
    // none of them describe this session.
    let env = match runtime {
        Runtime::Claude => env.clone(),
        Runtime::Codex | Runtime::Agy | Runtime::Acp => HookEnv::default(),
    };
    let session_id = input.session_id.or_else(|| env.session_id.clone())?;
    // The Senate home is where the daemon's socket is.
    let home = crate::paths::Paths::new(daemon_socket.parent().unwrap_or(Path::new(".")));
    let request = match kind {
        HookKind::SessionEnd => {
            // The notes the daemon's last checkpoint reads belong to the
            // branch now checked out.
            if let Some(cwd) = input.cwd.clone() {
                let _ = session::gather_synced(&home, &cwd, git_timeout).await;
            }
            Request::SessionEnd {
                session_id,
                reason: input.reason,
            }
        }
        HookKind::SessionStart | HookKind::UserPromptSubmit => {
            let cwd = input.cwd.or_else(|| std::env::current_dir().ok())?;
            let event = if kind == HookKind::SessionStart {
                HookEvent::SessionStart
            } else {
                HookEvent::UserPromptSubmit
            };
            // A hosted agy's hook says which host it runs under; the
            // daemon checks that against the host's TUI among its ancestors.
            let host: Option<EventId> = (runtime == Runtime::Agy)
                .then(|| {
                    std::env::var(crate::host::HOSTED_AGY_ENV)
                        .ok()?
                        .parse()
                        .ok()
                })
                .flatten();
            let ancestors = async {
                if host.is_some() {
                    let parent = std::os::unix::process::parent_id();
                    return session::ancestor_pids(parent, ANCESTOR_DEPTH, git_timeout).await;
                }
                let in_host = std::env::var_os(crate::host::HOSTED_CLAUDE_ENV).is_some();
                if runtime != Runtime::Claude || (env.claude_pid.is_some() && !in_host) {
                    return Vec::new();
                }
                let parent = std::os::unix::process::parent_id();
                if env.claude_pid == Some(parent) {
                    return vec![parent];
                }
                session::ancestor_pids(parent, ANCESTOR_DEPTH, git_timeout).await
            };
            let ((facts, _), ancestor_pids) =
                tokio::join!(session::gather_synced(&home, &cwd, git_timeout), ancestors);
            Request::SessionHook(HookReport {
                event,
                session_id,
                cwd,
                transcript_path: input.transcript_path,
                source: input.source,
                branch: facts.branch,
                git_root: facts.toplevel,
                committee_file: facts.committee_file,
                socket: env.socket.clone(),
                token: env.token.clone(),
                claude_pid: env.claude_pid,
                ancestor_pids,
                runtime,
                agent_env: env.agent.clone(),
                agent_file: facts.agent_file,
                host,
            })
        }
        HookKind::HostedApproval | HookKind::Stop => return None,
    };
    Some(request)
}

// Claude 2.1.327 supplies tool_name and tool_input on PermissionRequest,
// with no tool_use_id. Its later result can be matched by the same input.
fn approval_signal(hook: &Value) -> Option<ClaudeApprovalSignal> {
    let event = hook.get("hook_event_name")?.as_str()?;
    if event == "Notification" {
        return (hook.get("notification_type")?.as_str()? == "permission_prompt")
            .then_some(ClaudeApprovalSignal::Pending);
    }
    let tool = hook.get("tool_name")?.as_str()?;
    let input = hook.get("tool_input")?;
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(tool, input)).ok()?)
    );
    match event {
        "PermissionRequest" => {
            let tool = input.get("command").and_then(Value::as_str).unwrap_or(tool);
            Some(ClaudeApprovalSignal::Requested {
                tool: crate::host_approval::tool_label(tool),
                fingerprint,
            })
        }
        "PostToolUse" | "PostToolUseFailure" => {
            Some(ClaudeApprovalSignal::Finished { fingerprint })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agy_input_without_a_directory_leaves_it_to_the_hook() {
        let id = "0b1f6e9a-2c3d-4e5f-8a9b-0c1d2e3f4a5b";
        let parse = |input: Value| {
            serde_json::from_value::<AgyHookInput>(input)
                .unwrap()
                .normalize()
        };
        // The TUI's input: an empty cwd and no workspace paths.
        let tui = parse(json!({"conversationId": id, "cwd": "", "workspacePaths": []})).unwrap();
        assert_eq!(tui.session_id.as_deref(), Some(&*format!("agy:{id}")));
        assert_eq!(tui.cwd, None);
        // Print mode's: the real one.
        let print = parse(json!({"conversationId": id, "cwd": "/w", "workspacePaths": ["/w"]}));
        assert_eq!(print.unwrap().cwd, Some(PathBuf::from("/w")));
        let one = parse(json!({"conversationId": id, "cwd": "", "workspacePaths": ["/x"]}));
        assert_eq!(one.unwrap().cwd, Some(PathBuf::from("/x")));
        assert!(parse(json!({"conversationId": id, "cwd": "rel"})).is_none());
    }

    #[test]
    fn output_shape() {
        let out: serde_json::Value =
            serde_json::from_str(&output_for(HookKind::UserPromptSubmit, "hi")).unwrap();
        assert_eq!(
            out,
            json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"hi"}})
        );
    }

    #[test]
    fn codex_is_recognised_by_its_rollout_transcript() {
        let input = |t: Option<&str>| HookInput {
            transcript_path: t.map(Into::into),
            ..HookInput::default()
        };
        assert_eq!(
            detect_runtime(&input(Some(
                "/Users/b/.codex/sessions/2026/11/08/rollout-2026-11-08T23-24-12-01a1bbc2.jsonl"
            ))),
            Runtime::Codex
        );
        assert_eq!(
            detect_runtime(&input(Some("/Users/b/.claude/projects/x/6c58.jsonl"))),
            Runtime::Claude
        );
        assert_eq!(detect_runtime(&input(None)), Runtime::Claude);
    }

    #[tokio::test(start_paused = true)]
    async fn no_daemon_means_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let stdin = r#"{"session_id":"s","cwd":"/","hook_event_name":"UserPromptSubmit"}"#;
        let started = tokio::time::Instant::now();
        let out = run(
            HookKind::UserPromptSubmit,
            &dir.path().join("bridge.sock"),
            stdin,
            &HookEnv::default(),
            HookTimeouts::default(),
        )
        .await;
        assert_eq!(out, None);
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "missing socket spent the IPC deadline"
        );
    }

    #[tokio::test]
    async fn garbage_input_means_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(
            HookKind::SessionStart,
            &dir.path().join("bridge.sock"),
            "not json",
            &HookEnv::default(),
            HookTimeouts::default(),
        )
        .await;
        assert_eq!(out, None);
    }
}
