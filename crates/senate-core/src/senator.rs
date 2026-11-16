//! Who a senator is.
//!
//! A [`SenatorPath`] is a person's handle followed by zero or more agent
//! names:
//!
//! - `wendell` is the **senior senator**: the person. A person is global.
//!   There is one `wendell` however many Senates are nested, and a person is
//!   not a member of any one of them.
//! - `wendell/claude` is a **junior senator**: an agent of that person,
//!   seated in the top Senate.
//! - `wendell/claude/reviewer` is an agent seated one level down, in the
//!   Senate that `wendell/claude` presides over as its Speaker.
//!
//! So the path is the tree of who presides over whom, and its first segment
//! is always the person accountable for everything beneath it.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::{IdError, Name};

/// How many agent segments a path may have. A guard against runaway nesting,
/// not a design limit.
pub const MAX_AGENT_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SenatorPathError {
    #[error(transparent)]
    Name(#[from] IdError),
    #[error("senator path nests more than {max} agents: {path:?}")]
    TooDeep { path: String, max: usize },
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SenatorPath {
    owner: Name,
    agents: Vec<Name>,
}

impl SenatorPath {
    /// The person `owner`.
    pub fn senior(owner: Name) -> Self {
        Self {
            owner,
            agents: Vec::new(),
        }
    }

    pub fn new(owner: Name, agents: Vec<Name>) -> Result<Self, SenatorPathError> {
        let path = Self { owner, agents };
        if path.agents.len() > MAX_AGENT_DEPTH {
            return Err(SenatorPathError::TooDeep {
                path: path.to_string(),
                max: MAX_AGENT_DEPTH,
            });
        }
        Ok(path)
    }

    /// The person this senator is, or belongs to.
    pub fn owner(&self) -> &Name {
        &self.owner
    }

    /// The agent segments, outermost first. Empty for a person.
    pub fn agents(&self) -> &[Name] {
        &self.agents
    }

    /// A person.
    pub fn is_senior(&self) -> bool {
        self.agents.is_empty()
    }

    /// An agent, at any level.
    pub fn is_junior(&self) -> bool {
        !self.agents.is_empty()
    }

    /// How far down the agent sits: 0 for a person, 1 for an agent in the top
    /// Senate, 2 for an agent in a Senate one level down.
    pub fn level(&self) -> usize {
        self.agents.len()
    }

    /// The last segment: the person's handle, or the agent's own name.
    pub fn name(&self) -> &Name {
        self.agents.last().unwrap_or(&self.owner)
    }

    /// The person accountable for this senator. A person is their own.
    pub fn senior_path(&self) -> SenatorPath {
        Self::senior(self.owner.clone())
    }

    /// The agent that presides over the Senate this agent sits in, as that
    /// Senate's Speaker. `None` for a person, and for an agent in the top
    /// Senate, whose Speaker is not a senator.
    pub fn presider(&self) -> Option<SenatorPath> {
        (self.agents.len() >= 2).then(|| Self {
            owner: self.owner.clone(),
            agents: self.agents[..self.agents.len() - 1].to_vec(),
        })
    }

    /// The agent `name` directly beneath this senator.
    pub fn child(&self, name: Name) -> Result<SenatorPath, SenatorPathError> {
        let mut agents = self.agents.clone();
        agents.push(name);
        Self::new(self.owner.clone(), agents)
    }

    /// True when this senator is somewhere beneath `other`: every agent of a
    /// person is under that person, and `wendell/claude/reviewer` is under
    /// `wendell/claude`. A senator is not under itself.
    pub fn is_under(&self, other: &SenatorPath) -> bool {
        self.owner == other.owner
            && self.agents.len() > other.agents.len()
            && self.agents[..other.agents.len()] == other.agents[..]
    }
}

impl fmt::Display for SenatorPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.owner.as_str())?;
        for agent in &self.agents {
            write!(f, "/{agent}")?;
        }
        Ok(())
    }
}

impl FromStr for SenatorPath {
    type Err = SenatorPathError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut segments = s.split('/');
        // `split` always yields at least one item; an empty one fails as a name.
        let owner = Name::new(segments.next().unwrap_or_default())?;
        let agents = segments.map(Name::new).collect::<Result<Vec<_>, _>>()?;
        Self::new(owner, agents)
    }
}

impl Serialize for SenatorPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SenatorPath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> SenatorPath {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_prints() {
        for s in ["wendell", "wendell/claude", "wendell/claude/reviewer"] {
            assert_eq!(path(s).to_string(), s);
        }
        for s in [
            "",
            "/claude",
            "wendell/",
            "wendell//claude",
            "Wendell",
            "a b",
        ] {
            assert!(s.parse::<SenatorPath>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn a_person_is_senior_and_an_agent_is_junior() {
        assert!(path("wendell").is_senior());
        assert!(!path("wendell").is_junior());
        assert!(path("wendell/claude").is_junior());
        assert_eq!(path("wendell").level(), 0);
        assert_eq!(path("wendell/claude/reviewer").level(), 2);
        assert_eq!(path("wendell/claude").name().as_str(), "claude");
        assert_eq!(path("wendell").name().as_str(), "wendell");
    }

    #[test]
    fn the_person_is_the_same_at_every_level() {
        let deep = path("wendell/claude/reviewer");
        assert_eq!(deep.senior_path(), path("wendell"));
        assert_eq!(path("wendell").senior_path(), path("wendell"));
    }

    #[test]
    fn a_nested_agent_is_presided_over_by_its_parent() {
        assert_eq!(
            path("wendell/claude/reviewer").presider(),
            Some(path("wendell/claude"))
        );
        // The top Senate's Speaker is not a senator.
        assert_eq!(path("wendell/claude").presider(), None);
        assert_eq!(path("wendell").presider(), None);
    }

    #[test]
    fn under_follows_the_path() {
        assert!(path("wendell/claude").is_under(&path("wendell")));
        assert!(path("wendell/claude/reviewer").is_under(&path("wendell/claude")));
        assert!(!path("wendell/claude").is_under(&path("wendell/claude")));
        assert!(!path("wendell/codex/x").is_under(&path("wendell/claude")));
        assert!(!path("alice/claude").is_under(&path("wendell")));
    }

    #[test]
    fn child_extends_and_depth_is_bounded() {
        let claude = path("wendell").child(Name::new("claude").unwrap()).unwrap();
        assert_eq!(claude, path("wendell/claude"));

        let mut p = path("wendell");
        for _ in 0..MAX_AGENT_DEPTH {
            p = p.child(Name::new("a").unwrap()).unwrap();
        }
        assert!(matches!(
            p.child(Name::new("a").unwrap()),
            Err(SenatorPathError::TooDeep { .. })
        ));
    }

    #[test]
    fn serializes_as_a_string() {
        let json = serde_json::to_value(path("wendell/claude")).unwrap();
        assert_eq!(json, serde_json::json!("wendell/claude"));
        assert!(serde_json::from_value::<SenatorPath>(serde_json::json!("a//b")).is_err());
    }
}
