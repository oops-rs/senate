//! Which agent a coding session speaks as.
//!
//! A bridge holds one person's token and seats one senator per kind of
//! coding agent it serves. Each session is one of them, chosen when its
//! hooks register it, in this order:
//!
//! 1. **`SENATE_AGENT`** in the session's environment (Claude Code only: a
//!    Codex hook runs inside the app-server, whose environment is shared by
//!    every thread and describes none of them);
//! 2. **`.senate/agent`** at the worktree root (or the cwd outside git; `senate
//!    pin` writes it): its first line that is neither blank nor a `#`
//!    comment. It is kept per branch, like the committee file
//!    ([`crate::follow`]), and a worktree just added has none;
//! 3. **`[agents]`** in `senate.toml`, by harness: `claude` for a Claude
//!    Code session, `codex` for a Codex session;
//! 4. the harness's own name, `claude` or `codex`.
//!
//! A pin is one flat senator name ([`SenatorName`]): `claude`, `reviewer`.
//! There is no owner in it and no path: the agent belongs to the person
//! whose machine runs it, and an agent that needs a team of its own gets a
//! child Senate. A pin that is not a valid name (a path such as
//! `claude/reviewer` included) is ignored with a warning and the next rule
//! applies, so a typo never leaves a session unregistered.

use std::fmt;

use senate_core::{Name, SenatorName};
use serde::{Deserialize, Serialize};

use crate::config::AgentNames;
use crate::session::Runtime;

/// The environment variable that pins a Claude Code session's agent name.
pub const AGENT_ENV: &str = "SENATE_AGENT";
/// The worktree file that pins the agent name of every session in it.
pub const AGENT_FILE: &str = ".senate/agent";

/// How a session's agent was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentSource {
    /// `SENATE_AGENT`.
    Env,
    /// `.senate/agent`.
    File,
    /// `[agents]` in `senate.toml`, or the harness's own name.
    Harness,
}

impl AgentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Env => "env",
            Self::File => "file",
            Self::Harness => "harness",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "env" => Some(Self::Env),
            "file" => Some(Self::File),
            "harness" => Some(Self::Harness),
            _ => None,
        }
    }
}

impl fmt::Display for AgentSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who this bridge's agents are: the person it authenticates as runs
/// them, and each is a flat name in the Senate it serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seating {
    person: Name,
}

impl Seating {
    pub fn new(person: Name) -> Self {
        Self { person }
    }

    pub fn person(&self) -> &Name {
        &self.person
    }

    /// The agent a session pinned to `name` speaks as: that name.
    pub fn place(&self, name: &SenatorName) -> SenatorName {
        name.clone()
    }

    /// Whether `agent` is one this bridge may speak as: any senator that is
    /// not the person (the server refuses one another person runs).
    pub fn owns(&self, agent: &SenatorName) -> bool {
        agent != &self.person
    }
}

/// The first line of a `.senate/agent` file that is neither blank nor a
/// `#` comment.
pub fn name_from_file(contents: &str) -> Option<&str> {
    contents
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
}

/// What a session's hooks report that bears on its agent.
#[derive(Debug, Clone, Copy, Default)]
pub struct Pins<'a> {
    /// `SENATE_AGENT`, as the hook saw it.
    pub env: Option<&'a str>,
    /// The contents of `.senate/agent`.
    pub file: Option<&'a str>,
}

/// The agent a session speaks as (module docs), and how it was chosen.
pub fn choose(names: &AgentNames, runtime: Runtime, pins: Pins<'_>) -> (SenatorName, AgentSource) {
    let env = match runtime {
        Runtime::Claude => pins.env,
        Runtime::Codex => None,
    };
    let candidates = [
        (env.map(str::trim), AgentSource::Env),
        (pins.file.and_then(name_from_file), AgentSource::File),
    ];
    for (raw, source) in candidates {
        let Some(raw) = raw.filter(|r| !r.is_empty()) else {
            continue;
        };
        match raw.parse::<SenatorName>() {
            Ok(name) => return (name, source),
            Err(e) => tracing::warn!(
                pin = raw,
                source = source.as_str(),
                "ignoring an agent pin that is not a senator name: {e}"
            ),
        }
    }
    (names.for_runtime(runtime).clone(), AgentSource::Harness)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    fn agent(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    #[test]
    fn the_harness_names_the_agent_by_default() {
        let names = AgentNames::default();
        let none = Pins::default();
        assert_eq!(
            choose(&names, Runtime::Claude, none),
            (agent("claude"), AgentSource::Harness)
        );
        assert_eq!(
            choose(&names, Runtime::Codex, none),
            (agent("codex"), AgentSource::Harness)
        );
        let renamed = AgentNames {
            claude: agent("cc"),
            codex: agent("cx"),
        };
        assert_eq!(choose(&renamed, Runtime::Codex, none).0, agent("cx"));
    }

    #[test]
    fn a_session_pins_its_name_by_env_then_by_file() {
        let names = AgentNames::default();
        let file = "# this worktree's agent\n\n reviewer \n";
        assert_eq!(name_from_file(file), Some("reviewer"));
        let both = Pins {
            env: Some("frontend"),
            file: Some(file),
        };
        assert_eq!(
            choose(&names, Runtime::Claude, both),
            (agent("frontend"), AgentSource::Env)
        );
        // Codex hooks share the app-server's environment: only the file counts.
        assert_eq!(
            choose(&names, Runtime::Codex, both),
            (agent("reviewer"), AgentSource::File)
        );
        // A pin that is not a senator name is skipped.
        let bad = Pins {
            env: Some("Front End"),
            file: Some("a//b\n"),
        };
        assert_eq!(
            choose(&names, Runtime::Claude, bad),
            (agent("claude"), AgentSource::Harness)
        );
        let empty = Pins {
            env: Some(""),
            file: Some("# only a comment\n"),
        };
        assert_eq!(
            choose(&names, Runtime::Claude, empty).1,
            AgentSource::Harness
        );
    }

    #[test]
    fn a_pinned_path_is_not_a_name_and_the_next_rule_applies() {
        // Replaces "a pin may be a path relative to the person": an agent's
        // name holds no path; a team of its own is a child Senate.
        let names = AgentNames::default();
        let deep = Pins {
            env: Some("claude/reviewer"),
            file: Some("reviewer\n"),
        };
        assert_eq!(
            choose(&names, Runtime::Claude, deep),
            (agent("reviewer"), AgentSource::File)
        );
        let only_paths = Pins {
            env: Some("claude/reviewer"),
            file: Some("alice/codex\n"),
        };
        assert_eq!(
            choose(&names, Runtime::Claude, only_paths),
            (agent("claude"), AgentSource::Harness)
        );
    }

    #[test]
    fn a_bridges_agents_are_flat_names_and_never_its_person() {
        // Replaces "agents sit under the person or under the presider" and
        // "a path relative to the person names the agent and so its Senate".
        let seating = Seating::new(name("alice"));
        assert_eq!(seating.place(&agent("claude")), agent("claude"));
        assert!(seating.owns(&agent("codex")));
        assert!(!seating.owns(&agent("alice")));
        assert_eq!(seating.person(), &name("alice"));
    }

    #[test]
    fn sources_round_trip() {
        for s in [AgentSource::Env, AgentSource::File, AgentSource::Harness] {
            assert_eq!(AgentSource::parse(s.as_str()), Some(s));
        }
        assert_eq!(AgentSource::parse("x"), None);
    }
}
