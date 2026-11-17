//! Which agent a coding session speaks as.
//!
//! A bridge holds one person's token and seats one junior senator per kind
//! of coding agent it serves. Each session is one of them, chosen when its
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
//!    Code session, `codex` for a Codex session (each a name or a path
//!    relative to the person);
//! 4. the harness's own name, `claude` or `codex`.
//!
//! A pin is a path relative to the person ([`RelPath`]): one name
//! (`claude`, `reviewer`) or several (`claude/reviewer`). One that is not a
//! valid path is ignored with a warning and the next rule applies, so a
//! typo never leaves a session unregistered.
//!
//! # Which Senate
//!
//! The path selects the Senate the session sits in: an agent directly
//! under its person sits in the Senate above (or the top one), an agent
//! deeper sits in the Senate presided over by its parent path. A bridge
//! serving several Senates places `claude` at `alice/claude` and
//! `claude/reviewer` at `alice/claude/reviewer`, in the Senate
//! `alice/claude` presides over ([`Seating::place`]).
//!
//! A bridge serving one Senate keeps its old rule for a single name: it is
//! placed under the senator that Senate seats agents beneath ([`Seating`]):
//! the person in the top Senate (`alice/claude`), the presiding agent in a
//! Senate one level down (`alice/claude/reviewer`), as `GET /v1/me` reports
//! it. A longer pin is always relative to the person.

use std::fmt;
use std::str::FromStr;

use senate_core::{MAX_AGENT_DEPTH, Name, SenatorName};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::AgentNames;
use crate::session::Runtime;

/// The environment variable that pins a Claude Code session's agent name.
pub const AGENT_ENV: &str = "SENATE_AGENT";
/// The worktree file that pins the agent name of every session in it.
pub const AGENT_FILE: &str = ".senate/agent";

/// An agent's path relative to its person: `claude`, `claude/reviewer`.
/// One to [`MAX_AGENT_DEPTH`] senator names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(Vec<Name>);

impl RelPath {
    pub fn new(names: Vec<Name>) -> Result<Self, String> {
        if names.is_empty() || names.len() > MAX_AGENT_DEPTH {
            return Err(format!(
                "an agent's path has 1 to {MAX_AGENT_DEPTH} names, got {}",
                names.len()
            ));
        }
        Ok(Self(names))
    }

    /// One name.
    pub fn name(name: Name) -> Self {
        Self(vec![name])
    }

    pub fn names(&self) -> &[Name] {
        &self.0
    }

    /// One name only (`claude`), not a path (`claude/reviewer`).
    pub fn single(&self) -> Option<&Name> {
        match self.0.as_slice() {
            [one] => Some(one),
            _ => None,
        }
    }

    /// The agent this path names under `person`.
    pub fn under(&self, person: &Name) -> SenatorName {
        SenatorName::new(person.clone(), self.0.clone())
            .unwrap_or_else(|_| unreachable!("a RelPath is never deeper than an agent path"))
    }

    /// `agent` relative to its person; `None` for a person.
    pub fn of(agent: &SenatorName) -> Option<Self> {
        agent.is_junior().then(|| Self(agent.agents().to_vec()))
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.0.iter().map(Name::as_str).collect();
        f.write_str(&names.join("/"))
    }
}

impl FromStr for RelPath {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let names = s
            .trim()
            .split('/')
            .map(|n| Name::new(n).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(names)
    }
}

impl Serialize for RelPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RelPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

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

/// Where this bridge's agents sit: directly under `base`, which is the
/// person in the top Senate and the presiding agent in a nested one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seating {
    person: Name,
    presider: Option<SenatorName>,
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
    pub fn new(person: Name, presider: Option<SenatorName>) -> Self {
        Self { person, presider }
    }

    pub fn person(&self) -> &Name {
        &self.person
    }

    /// The Senate this bridge is connected to, by its presider (`None`: the
    /// top Senate).
    pub fn presider(&self) -> Option<&SenatorName> {
        self.presider.as_ref()
    }

    /// The senator agents are named under.
    pub fn base(&self) -> SenatorName {
        match &self.presider {
            Some(p) if p.owner() == &self.person && p.is_junior() => p.clone(),
            _ => SenatorName::senior(self.person.clone()),
        }
    }

    /// The agent called `name`.
    pub fn agent(&self, name: &Name) -> SenatorName {
        let base = self.base();
        // A path is at most MAX_AGENT_DEPTH deep; a presider already at the
        // bottom leaves the person's own level as the only one to use.
        base.child(name.clone()).unwrap_or_else(|_| {
            SenatorName::senior(self.person.clone())
                .child(name.clone())
                .unwrap_or_else(|_| unreachable!("one agent segment is always allowed"))
        })
    }

    /// Where a session pinned to `rel` sits: a single name under
    /// [`Seating::base`] (one Senate, see the module docs), a longer path
    /// under the person.
    pub fn place(&self, rel: &RelPath) -> SenatorName {
        match rel.single() {
            Some(name) => self.agent(name),
            None => rel.under(&self.person),
        }
    }

    /// Whether `agent` is one this bridge may speak as: an agent under its
    /// base. Anything else in a request is refused before it reaches the
    /// server.
    pub fn owns(&self, agent: &SenatorName) -> bool {
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

/// The agent path a session speaks as, relative to its person (module
/// docs), and how it was chosen.
pub fn choose(names: &AgentNames, runtime: Runtime, pins: Pins<'_>) -> (RelPath, AgentSource) {
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
        match raw.parse::<RelPath>() {
            Ok(path) => return (path, source),
            Err(e) => tracing::warn!(
                pin = raw,
                source = source.as_str(),
                "ignoring an agent pin that is not a path of senator names: {e}"
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

    fn path(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    fn rel(s: &str) -> RelPath {
        s.parse().unwrap()
    }

    #[test]
    fn the_harness_names_the_agent_by_default() {
        let names = AgentNames::default();
        let none = Pins::default();
        assert_eq!(
            choose(&names, Runtime::Claude, none),
            (rel("claude"), AgentSource::Harness)
        );
        assert_eq!(
            choose(&names, Runtime::Codex, none),
            (rel("codex"), AgentSource::Harness)
        );
        let renamed = AgentNames {
            claude: rel("cc"),
            codex: rel("cx"),
        };
        assert_eq!(choose(&renamed, Runtime::Codex, none).0, rel("cx"));
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
            (rel("frontend"), AgentSource::Env)
        );
        // Codex hooks share the app-server's environment: only the file counts.
        assert_eq!(
            choose(&names, Runtime::Codex, both),
            (rel("reviewer"), AgentSource::File)
        );
        // A pin may be a path relative to the person.
        let deep = Pins {
            env: Some("claude/reviewer"),
            file: None,
        };
        assert_eq!(
            choose(&names, Runtime::Claude, deep),
            (rel("claude/reviewer"), AgentSource::Env)
        );
        // A pin that is not a path of senator names is skipped.
        let bad = Pins {
            env: Some("Front End"),
            file: Some("a//b\n"),
        };
        assert_eq!(
            choose(&names, Runtime::Claude, bad),
            (rel("claude"), AgentSource::Harness)
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
    fn a_path_relative_to_the_person_names_the_agent_and_so_its_senate() {
        let alice = name("alice");
        assert_eq!(rel("claude").under(&alice), path("alice/claude"));
        assert_eq!(
            rel("claude/reviewer").under(&alice),
            path("alice/claude/reviewer")
        );
        assert_eq!(
            RelPath::of(&path("alice/claude/reviewer")),
            Some(rel("claude/reviewer"))
        );
        assert_eq!(RelPath::of(&path("alice")), None);
        assert!("".parse::<RelPath>().is_err());
        assert!("a/b/c/d/e/f/g/h/i".parse::<RelPath>().is_err());
        assert_eq!(rel("claude/reviewer").to_string(), "claude/reviewer");

        // One Senate: a single name sits under its base, a path under the person.
        let nested = Seating::new(alice.clone(), Some(path("alice/claude")));
        assert_eq!(
            nested.place(&rel("reviewer")),
            path("alice/claude/reviewer")
        );
        assert_eq!(
            nested.place(&rel("claude/reviewer")),
            path("alice/claude/reviewer")
        );
        let top = Seating::top(alice);
        assert_eq!(top.place(&rel("codex")), path("alice/codex"));
    }

    #[test]
    fn sources_round_trip() {
        for s in [AgentSource::Env, AgentSource::File, AgentSource::Harness] {
            assert_eq!(AgentSource::parse(s.as_str()), Some(s));
        }
        assert_eq!(AgentSource::parse("x"), None);
    }
}
