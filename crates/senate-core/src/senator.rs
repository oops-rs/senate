//! Who a senator is.
//!
//! A [`SenatorName`] is one flat name, scoped to one Senate: `wendell` (a
//! person), `codex` or `claude` (agents). The name says nothing about who
//! owns the senator or where it sits:
//!
//! - **Ownership comes from the Senate.** Every Senate has one senior, its
//!   owner, and every agent seated in it belongs to that senior. There is no
//!   owner prefix on an agent's name.
//! - **Whether a senator is a person or an agent is the roster's to say**
//!   ([`Roster::is_person`](crate::Roster::is_person)): person handles and
//!   agent names share one namespace within a Senate, so the same name is
//!   never both.
//! - **Nothing nests inside a name.** An agent that needs its own team gets
//!   a child Senate; only Senates nest ([`crate::senate`]).
//!
//! A seat is a senator on one committee; everything kept per seat (sessions,
//! inbox, cursors, presence, checkpoints) is keyed by committee and senator.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::{IdError, Name};
use crate::senate::{KeywordError, check_not_keyword};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SenatorNameError {
    #[error(transparent)]
    Name(#[from] IdError),
    #[error(
        "a senator is one name, with no path inside it: {0:?} (an agent that needs a team of its own gets a child Senate)"
    )]
    Path(String),
    #[error(transparent)]
    Keyword(#[from] KeywordError),
}

/// One senator's name within its Senate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SenatorName(Name);

impl SenatorName {
    /// The senator named `name`. Refused when `name` is an address keyword
    /// (`senator`, `speaker`, `committee`, `guest`), which would make a
    /// short name on the command line ambiguous.
    pub fn new(name: Name) -> Result<Self, SenatorNameError> {
        check_not_keyword(name.as_str(), "a senator")?;
        Ok(Self(name))
    }

    /// Namespace reserved for primary identities of interactive coding sessions.
    pub fn is_session_name(&self) -> bool {
        self.as_str().starts_with("codex-") || self.as_str().starts_with("claude-")
    }

    /// The name itself.
    pub fn name(&self) -> &Name {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for SenatorName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl FromStr for SenatorName {
    type Err = SenatorNameError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains('/') {
            return Err(SenatorNameError::Path(s.to_string()));
        }
        Self::new(Name::new(s)?)
    }
}

impl TryFrom<Name> for SenatorName {
    type Error = SenatorNameError;
    fn try_from(name: Name) -> Result<Self, Self::Error> {
        Self::new(name)
    }
}

impl From<SenatorName> for Name {
    fn from(senator: SenatorName) -> Self {
        senator.0
    }
}

impl PartialEq<Name> for SenatorName {
    fn eq(&self, other: &Name) -> bool {
        &self.0 == other
    }
}

impl Serialize for SenatorName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SenatorName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_senator_is_one_flat_name() {
        for s in ["wendell", "claude", "codex", "north-desk-2"] {
            let senator: SenatorName = s.parse().unwrap();
            assert_eq!(senator.to_string(), s);
            assert_eq!(senator.name().as_str(), s);
        }
        for s in ["", "Wendell", "a b", "*"] {
            assert!(s.parse::<SenatorName>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn a_path_inside_a_name_is_refused_and_says_why() {
        for s in ["wendell/claude", "claude/reviewer", "/claude", "wendell/"] {
            let err = s.parse::<SenatorName>().unwrap_err();
            assert!(matches!(err, SenatorNameError::Path(_)), "{s:?}");
            assert!(err.to_string().contains("child Senate"), "{err}");
        }
    }

    #[test]
    fn an_address_keyword_is_not_a_senator() {
        for s in ["senator", "speaker", "committee", "guest"] {
            assert!(
                matches!(s.parse::<SenatorName>(), Err(SenatorNameError::Keyword(_))),
                "{s}"
            );
        }
    }

    #[test]
    fn serializes_as_a_string() {
        let json = serde_json::to_value("claude".parse::<SenatorName>().unwrap()).unwrap();
        assert_eq!(json, serde_json::json!("claude"));
        assert!(serde_json::from_value::<SenatorName>(serde_json::json!("a/b")).is_err());
    }
}
