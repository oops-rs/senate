//! The companion session: when a teammate's question for one of this
//! bridge's agents arrives and that agent has no live interactive session
//! on its committee (or none active for [`CompanionConfig::idle_secs`]),
//! the daemon runs a headless fork of that agent's latest Claude Code
//! session on the committee to answer it, as that agent, then lets it exit.
//! Only Claude Code sessions can be forked, so a question for an agent with
//! no Claude Code session on record waits for one of its own sessions.
//!
//! ```text
//! claude -p --resume <latest session> --fork-session --output-format json \
//!        --allowedTools <read-only tools + mcp__senate__answer / catch_up / search> \
//!        --disallowedTools <tools that change things>
//! ```
//!
//! The prompt (the question as it would have been pushed, framed) goes on
//! stdin; the process runs in that session's cwd. Its environment names a
//! synthetic session id ([`ENV_SESSION`]) that the daemon binds to the
//! committee, so the fork's `senate` MCP shim ([`crate::mcp::selector_from_env`])
//! speaks as the companion, and [`ENV_COMPANION`] makes the fork's hooks do
//! nothing (it must never register as an interactive session).
//!
//! The daemon lets a companion only answer its own question's thread, read
//! (`catch_up` without moving the `read` cursor, `search`, the brief) and
//! nothing else; its answer carries
//! [`meta::COMPANION`](senate_core::meta::COMPANION), shown after the
//! author's label (`[senator::bob/claude] companion`,
//! [`Event::byline`](senate_core::Event::byline)). One runs at a time,
//! killed after
//! [`CompanionConfig::max_secs`]. Requests and notes are never given to it.
//! A companion that exits without answering hands its question back to the
//! queue (a later session gets it); each question gets one try per daemon
//! run.
//!
//! It spends the developer's own model budget, so it is opt-in:
//!
//! ```toml
//! [companion]
//! enabled = true
//! claude_bin = "/opt/homebrew/bin/claude"   # default: `claude` on PATH
//! max_secs = 300
//! idle_secs = 1800
//! ```

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use senate_core::{Address, CommitteeId, Event, Locale, SenatorPath, Seq};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

/// The companion's synthetic session id, read by the MCP shim.
pub const ENV_SESSION: &str = "SENATE_SESSION_ID";
/// Set in a companion's environment: its hooks do nothing.
pub const ENV_COMPANION: &str = "SENATE_COMPANION";
/// What a companion may use.
pub const ALLOWED_TOOLS: &str =
    "Read,Grep,Glob,LS,mcp__senate__answer,mcp__senate__catch_up,mcp__senate__search";
/// Refused outright, so nothing it does changes the worktree or reaches out.
pub const DISALLOWED_TOOLS: &str = "Bash,Edit,MultiEdit,Write,NotebookEdit,WebFetch,WebSearch,Task";
/// Prefix of companion session ids.
pub const SESSION_PREFIX: &str = "companion-";

/// `[companion]` in `bridge.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CompanionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_bin")]
    pub claude_bin: PathBuf,
    /// Killed after this long.
    #[serde(default = "default_max_secs")]
    pub max_secs: u64,
    /// A live session not seen for this long counts as inactive.
    #[serde(default = "default_idle_secs")]
    pub idle_secs: u64,
}

fn default_bin() -> PathBuf {
    PathBuf::from("claude")
}

fn default_max_secs() -> u64 {
    300
}

fn default_idle_secs() -> u64 {
    1800
}

impl Default for CompanionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            claude_bin: default_bin(),
            max_secs: default_max_secs(),
            idle_secs: default_idle_secs(),
        }
    }
}

#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    companion: CompanionConfig,
}

impl CompanionConfig {
    /// The `[companion]` table of the bridge config at `path`; the defaults
    /// (off) when the file or the table is missing.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|e| format!("parsing [companion] in {}: {e}", path.display()))?;
        if file.companion.max_secs == 0 {
            return Err("companion.max_secs must be at least 1".into());
        }
        Ok(file.companion)
    }

    pub fn max(&self) -> Duration {
        Duration::from_secs(self.max_secs)
    }

    pub fn idle(&self) -> Duration {
        Duration::from_secs(self.idle_secs)
    }
}

/// A companion the daemon is running.
#[derive(Debug, Clone)]
pub struct Running {
    pub session_id: String,
    /// The agent it answers as: the one the question is for.
    pub agent: SenatorPath,
    pub committee: CommitteeId,
    /// The question it was started for.
    pub root: Seq,
    /// The resumed session's cwd, branch and worktree.
    pub cwd: PathBuf,
    pub branch: Option<String>,
    pub git_root: Option<PathBuf>,
    pub answered: bool,
}

/// What the companion is told, before the question itself.
pub fn prompt(agent: &SenatorPath, question: &Event, pushed: &str, locale: Locale) -> String {
    let me = Address::senator(agent).in_text();
    let committee = &question.committee;
    let seq = question.seq;
    match locale {
        Locale::Zh => format!(
            "[senate companion] 你是 {me} 上一个 Claude Code 会话（Senate 委员会 `{committee}`）的\
             临时伴随会话。{me} 现在没有打开的会话，而一位队友的问题到了。用 \
             mcp__senate__answer(thread={seq}) 回答一次，依据这个会话已经知道的和仓库里能看到的。\
             你只有只读工具：什么都不要改，什么都不要运行。需要的话，mcp__senate__catch_up 和 \
             mcp__senate__search 可以读委员会的日志。不确定的地方直说；答不了，就回答 {me} 之后会跟进。\
             你的回答会标为伴随会话写的。然后停止。\n\n{pushed}"
        ),
        Locale::En => format!(
            "[senate companion] You are a short-lived companion of {me}'s last Claude Code \
             session on the Senate committee `{committee}`. {me} has no session open right now, \
             and a teammate's question arrived. Answer it once, with \
             mcp__senate__answer(thread={seq}), from what this session already knows and what \
             the repository shows. You have read-only tools: change nothing, run nothing. \
             mcp__senate__catch_up and mcp__senate__search read the committee's log if it helps. \
             Say plainly what you are unsure of; if you cannot answer, answer that {me} will \
             follow up. Your answer is labelled as the companion's. Then stop.\n\n{pushed}"
        ),
    }
}

/// How a companion run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// It exited; `result` is the `result` field of its JSON output.
    Exited {
        success: bool,
        result: Option<String>,
    },
    /// Killed at `max_secs`.
    TimedOut,
    Failed(String),
}

/// Run one companion to completion (or `max_secs`): resume `resume` as a
/// fork in `cwd`, as session `session_id`, the daemon's home being `home`.
pub async fn run(
    config: &CompanionConfig,
    resume: &str,
    cwd: &Path,
    session_id: &str,
    home: &Path,
    prompt: &str,
) -> Outcome {
    let spawned = tokio::process::Command::new(&config.claude_bin)
        .args(["-p", "--resume", resume, "--fork-session"])
        .args(["--output-format", "json"])
        .args(["--allowedTools", ALLOWED_TOOLS])
        .args(["--disallowedTools", DISALLOWED_TOOLS])
        .current_dir(cwd)
        .env(ENV_SESSION, session_id)
        .env(ENV_COMPANION, "1")
        .env(crate::paths::HOME_ENV, home)
        // An interactive session's variables would make the shim and hooks
        // speak for that session.
        .env_remove("CLAUDE_CODE_SESSION_ID")
        .env_remove("CLAUDE_PID")
        .env_remove("CLAUDE_CODE_MESSAGING_SOCKET")
        .env_remove("CLAUDE_CODE_MESSAGING_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            return Outcome::Failed(format!("starting {}: {e}", config.claude_bin.display()));
        }
    };
    // Dropping stdin after the write closes it, which ends the prompt.
    if let Some(mut stdin) = child.stdin.take()
        && let Err(e) = stdin.write_all(prompt.as_bytes()).await
    {
        return Outcome::Failed(format!("writing the prompt: {e}"));
    }
    match tokio::time::timeout(config.max(), child.wait_with_output()).await {
        // Dropping the future kills the child (kill_on_drop).
        Err(_) => Outcome::TimedOut,
        Ok(Err(e)) => Outcome::Failed(e.to_string()),
        Ok(Ok(out)) => {
            let result = serde_json::from_slice::<serde_json::Value>(&out.stdout)
                .ok()
                .and_then(|v| v.get("result")?.as_str().map(str::to_string));
            if !out.status.success() {
                let err = String::from_utf8_lossy(&out.stderr);
                tracing::warn!(
                    status = %out.status,
                    "the companion session failed: {}",
                    err.trim().chars().take(500).collect::<String>()
                );
            }
            Outcome::Exited {
                success: out.status.success(),
                result,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_names_the_agent_in_either_language() {
        use senate_core::{Kind, NewEvent};
        let agent: SenatorPath = "bob/claude".parse().unwrap();
        let q = NewEvent::new(
            CommitteeId::new("refund-v2").unwrap(),
            Kind::Question,
            "senator::alice/claude".parse().unwrap(),
            "which header?",
        )
        .into_event(Seq(9), chrono::Utc::now());
        let en = prompt(&agent, &q, "PUSHED", Locale::En);
        assert!(en.contains("companion of [senator::bob/claude]'s last Claude Code session"));
        assert!(en.contains("mcp__senate__answer(thread=9)") && en.ends_with("\n\nPUSHED"));
        let zh = prompt(&agent, &q, "PUSHED", Locale::Zh);
        assert!(zh.contains("你是 [senator::bob/claude] 上一个 Claude Code 会话"));
        assert!(zh.contains("mcp__senate__answer(thread=9)") && zh.ends_with("\n\nPUSHED"));
    }

    #[test]
    fn config_defaults_off() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            CompanionConfig::load(&dir.path().join("none.toml")).unwrap(),
            CompanionConfig::default()
        );
        let path = dir.path().join("bridge.toml");
        std::fs::write(
            &path,
            "server_url = \"x\"\n[companion]\nenabled = true\nmax_secs = 60\n",
        )
        .unwrap();
        let c = CompanionConfig::load(&path).unwrap();
        assert!(c.enabled);
        assert_eq!(c.max(), Duration::from_secs(60));
        assert_eq!(c.claude_bin, PathBuf::from("claude"));
        assert_eq!(c.idle(), Duration::from_secs(1800));
        std::fs::write(&path, "[companion]\nmax_secs = 0\n").unwrap();
        assert!(CompanionConfig::load(&path).is_err());
    }
}
