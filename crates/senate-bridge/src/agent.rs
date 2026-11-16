//! Which agent a coding session speaks as.
//!
//! A bridge holds one person's token and seats one junior senator per kind
//! of coding agent it serves. Each session is one of them, chosen when its
//! hooks register it, in this order:
//!
//! 1. **`SENATE_AGENT`** in the session's environment (Claude Code only: a
//!    Codex hook runs inside the app-server, whose environment is shared by
//!    every thread and describes none of them);
//! 2. **`.senate/agent`** at the worktree root (or the cwd outside git): its
//!    first line that is neither blank nor a `#` comment;
//! 3. **`[agents]`** in `bridge.toml`, by harness: `claude` for a Claude
//!    Code session, `codex` for a Codex session;
//! 4. the harness's own name, `claude` or `codex`.
//!
//! A pin must be a valid senator name ([`Name`]); one that is not is
//! ignored with a warning and the next rule applies, so a typo never leaves
//! a session unregistered.
//!
//! The name is placed under the senator this bridge seats agents beneath
//! ([`Seating`]): the person in the top Senate (`alice/claude`), the
//! presiding agent in a Senate one level down
//! (`alice/claude/reviewer`), as `GET /v1/me` reports it. That is the only
//! place the level enters.

use std::fmt;

use senate_core::{Name, SenatorPath};
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
    /// `[agents]` in `bridge.toml`, or the harness's own name.
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

/// Where this bridge's agents sit: directly under `base`, which is the
/// person in the top Senate and the presiding agent in a nested one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seating {
    person: Name,
    presider: Option<SenatorPath>,
}

impl Seating {
    /// Agents of `person` in the top Senate.
    pub fn top(person: Name) -> Self {
        Self {
            person,
            presider: None,
        }
    }

    /// Agents of `person` in the Senate `presider` presides over, as
    /// `GET /v1/me` names it. A presider that is not one of the person's own
    /// agents cannot have the person's agents seated under it
    /// ([`Roster::may_act_as`](senate_core::Roster::may_act_as)); the bridge
    /// then seats under the person and lets the server refuse, so the
    /// refusal is visible rather than guessed around.
    pub fn new(person: Name, presider: Option<SenatorPath>) -> Self {
        Self { person, presider }
    }

    pub fn person(&self) -> &Name {
        &self.person
    }

    /// The Senate this bridge is connected to, by its presider (`None`: the
    /// top Senate).
    pub fn presider(&self) -> Option<&SenatorPath> {
        self.presider.as_ref()
    }

    /// The senator agents are named under.
    pub fn base(&self) -> SenatorPath {
        match &self.presider {
            Some(p) if p.owner() == &self.person && p.is_junior() => p.clone(),
            _ => SenatorPath::senior(self.person.clone()),
        }
    }

    /// The agent called `name`.
    pub fn agent(&self, name: &Name) -> SenatorPath {
        let base = self.base();
        // A path is at most MAX_AGENT_DEPTH deep; a presider already at the
        // bottom leaves the person's own level as the only one to use.
        base.child(name.clone()).unwrap_or_else(|_| {
            SenatorPath::senior(self.person.clone())
                .child(name.clone())
                .unwrap_or_else(|_| unreachable!("one agent segment is always allowed"))
        })
    }

    /// Whether `agent` is one this bridge may speak as: an agent under its
    /// base. Anything else in a request is refused before it reaches the
    /// server.
    pub fn owns(&self, agent: &SenatorPath) -> bool {
        let parent = agent.presider().unwrap_or_else(|| agent.senior_path());
        agent.is_junior() && parent == self.base()
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

/// The agent name a session speaks as (module docs), and how it was chosen.
pub fn choose(names: &AgentNames, runtime: Runtime, pins: Pins<'_>) -> (Name, AgentSource) {
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
        match Name::new(raw) {
            Ok(name) => return (name, source),
            Err(e) => tracing::warn!(
                pin = raw,
                source = source.as_str(),
                "ignoring an agent name that is not a senator name: {e}"
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

    fn path(s: &str) -> SenatorPath {
        s.parse().unwrap()
    }

    #[test]
    fn the_harness_names_the_agent_by_default() {
        let names = AgentNames::default();
        let none = Pins::default();
        assert_eq!(
            choose(&names, Runtime::Claude, none),
            (name("claude"), AgentSource::Harness)
        );
        assert_eq!(
            choose(&names, Runtime::Codex, none),
            (name("codex"), AgentSource::Harness)
        );
        let renamed = AgentNames {
            claude: name("cc"),
            codex: name("cx"),
        };
        assert_eq!(choose(&renamed, Runtime::Codex, none).0, name("cx"));
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
            (name("frontend"), AgentSource::Env)
        );
        // Codex hooks share the app-server's environment: only the file counts.
        assert_eq!(
            choose(&names, Runtime::Codex, both),
            (name("reviewer"), AgentSource::File)
        );
        // A pin that is not a senator name is skipped.
        let bad = Pins {
            env: Some("Front End"),
            file: Some("a/b\n"),
        };
        assert_eq!(
            choose(&names, Runtime::Claude, bad),
            (name("claude"), AgentSource::Harness)
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
    fn agents_sit_under_the_person_or_under_the_presider() {
        let top = Seating::top(name("alice"));
        assert_eq!(top.agent(&name("claude")), path("alice/claude"));
        assert!(top.owns(&path("alice/codex")));
        assert!(!top.owns(&path("alice")));
        assert!(!top.owns(&path("bob/claude")));
        assert!(!top.owns(&path("alice/claude/reviewer")));

        let nested = Seating::new(name("alice"), Some(path("alice/claude")));
        assert_eq!(nested.presider(), Some(&path("alice/claude")));
        assert_eq!(
            nested.agent(&name("reviewer")),
            path("alice/claude/reviewer")
        );
        assert!(nested.owns(&path("alice/claude/reviewer")));
        assert!(!nested.owns(&path("alice/codex")));

        // Someone else's Senate: seat under the person; the server decides.
        let foreign = Seating::new(name("alice"), Some(path("bob/claude")));
        assert_eq!(foreign.agent(&name("claude")), path("alice/claude"));
    }

    #[test]
    fn sources_round_trip() {
        for s in [AgentSource::Env, AgentSource::File, AgentSource::Harness] {
            assert_eq!(AgentSource::parse(s.as_str()), Some(s));
        }
        assert_eq!(AgentSource::parse("x"), None);
    }
}
