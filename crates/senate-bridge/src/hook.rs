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
//! When `CLAUDE_PID` is not set, the hook reports its own parent chain
//! instead ([`ANCESTOR_DEPTH`] pids): Claude runs hooks directly or through
//! a shell, so Claude is among them. The chain is used only to find the
//! session, never for liveness, since a shell in it exits right away.
//!
//! Codex runs the same hooks (`hook <event> --codex`, from its
//! `config.toml`) inside the app-server, with the same input shape and the
//! same `hookSpecificOutput`; `session_id` is the Codex thread id. A Codex
//! session reports no socket, token, pids or `SENATE_AGENT` (the
//! app-server's environment belongs to none of its threads). Its context
//! carries the
//! orientation and brief only: queued messages reach it as turns through
//! the app-server, since Codex leaves hook context unanswered. Without `--codex` a Codex hook
//! is recognised by its rollout transcript ([`detect_runtime`]).
//!
//! It must never get in the user's way: every failure (daemon down, bad
//! input, timeout) ends in exit 0 with no output, within about a second.
//! In a companion session (`SENATE_COMPANION` set, [`crate::companion`])
//! it does nothing at all.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::ipc::{self, HookEvent, HookReply, HookReport, Request};
use crate::session::{self, Runtime};

/// How many pids above the hook are reported without `CLAUDE_PID`.
pub const ANCESTOR_DEPTH: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookKind {
    SessionStart,
    UserPromptSubmit,
    SessionEnd,
}

impl HookKind {
    /// The `hookEventName` Claude Code expects back.
    pub fn event_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::SessionEnd => "SessionEnd",
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
    pub fn from_process() -> Self {
        Self {
            socket: env_nonempty("CLAUDE_CODE_MESSAGING_SOCKET").map(PathBuf::from),
            token: env_nonempty("CLAUDE_CODE_MESSAGING_TOKEN"),
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

/// [`run`] for a known runtime: `Some(Codex)` for `hook <event> --codex`
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
    // A companion session (a headless fork the daemon runs) is not an
    // interactive session: it must not register, nor take pushes.
    if std::env::var_os(crate::companion::ENV_COMPANION).is_some() {
        return None;
    }
    let input: HookInput = serde_json::from_str(stdin).ok()?;
    let runtime = runtime.unwrap_or_else(|| detect_runtime(&input));
    // A Codex hook runs in the app-server, whose environment may carry a
    // Claude Code session's variables (a daemon started from inside one):
    // none of them describe this session.
    let env = match runtime {
        Runtime::Claude => env.clone(),
        Runtime::Codex => HookEnv::default(),
    };
    let session_id = input.session_id.or_else(|| env.session_id.clone())?;
    // Connect before asking git anything, so a down daemon costs milliseconds.
    let mut client = ipc::IpcClient::connect(daemon_socket, timeouts.connect)
        .await
        .ok()?;
    // The Senate home is where the daemon's socket is.
    let home = crate::paths::Paths::new(daemon_socket.parent().unwrap_or(Path::new(".")));
    let request = match kind {
        HookKind::SessionEnd => {
            // The notes the daemon's last checkpoint reads belong to the
            // branch now checked out.
            if let Some(cwd) = input.cwd.clone() {
                let _ = session::gather_synced(&home, &cwd, timeouts.git).await;
            }
            Request::SessionEnd {
                session_id,
                reason: input.reason,
            }
        }
        HookKind::SessionStart | HookKind::UserPromptSubmit => {
            let cwd = input.cwd.or_else(|| std::env::current_dir().ok())?;
            let ancestors = async {
                // Every Codex thread's hooks share the app-server's pids, so
                // they would name the wrong session.
                if env.claude_pid.is_some() || runtime == Runtime::Codex {
                    return Vec::new();
                }
                let parent = std::os::unix::process::parent_id();
                session::ancestor_pids(parent, ANCESTOR_DEPTH, timeouts.git).await
            };
            let ((facts, _), ancestor_pids) =
                tokio::join!(session::gather_synced(&home, &cwd, timeouts.git), ancestors);
            Request::Hook(HookReport {
                event: if kind == HookKind::SessionStart {
                    HookEvent::SessionStart
                } else {
                    HookEvent::UserPromptSubmit
                },
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
            })
        }
    };
    match kind {
        HookKind::SessionEnd => {
            let _: serde_json::Value = client.call(&request, timeouts.request).await.ok()?;
            None
        }
        _ => {
            let reply: HookReply = client.call(&request, timeouts.request).await.ok()?;
            reply.context.map(|c| output_for(kind, &c))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn no_daemon_means_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let stdin = r#"{"session_id":"s","cwd":"/","hook_event_name":"UserPromptSubmit"}"#;
        let started = std::time::Instant::now();
        let out = run(
            HookKind::UserPromptSubmit,
            &dir.path().join("bridge.sock"),
            stdin,
            &HookEnv::default(),
            HookTimeouts::default(),
        )
        .await;
        assert_eq!(out, None);
        assert!(started.elapsed() < Duration::from_secs(2));
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
