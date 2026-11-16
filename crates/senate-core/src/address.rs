//! Addresses: who an event is from and who it is to.
//!
//! - `senator::wendell` is a person; `senator::wendell/claude` is one of
//!   their agents. One prefix for both: the path says which.
//! - `speaker` is the Speaker of the Senate whose log the event is in. In a
//!   nested Senate that is the senator presiding over it.
//! - `committee` means everyone on the committee.
//!
//! This is the form stored in an event's fields. In running text an id is
//! written in brackets instead; see [`crate::mention`].
//!
//! Parsing ([`FromStr`]) is strict: it is how input is validated. Decoding
//! (`Deserialize`) is lenient: a form this build does not know becomes
//! [`Address::Other`], so a newer server's events still decode, round-trip,
//! and are simply never pushed (the same rule [`crate::Kind::Other`] follows).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::senator::{SenatorPath, SenatorPathError};

pub(crate) const SENATOR_PREFIX: &str = "senator::";
pub(crate) const SPEAKER: &str = "speaker";
pub(crate) const COMMITTEE: &str = "committee";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Address {
    Senator(SenatorPath),
    Speaker,
    Committee,
    /// A form this build does not know, kept verbatim.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressParseError {
    #[error("unknown address form: {0:?} (expected senator::<path>, speaker, or committee)")]
    Unknown(String),
    #[error(transparent)]
    Senator(#[from] SenatorPathError),
}

impl Address {
    pub fn senator(path: &SenatorPath) -> Self {
        Self::Senator(path.clone())
    }

    /// The senator behind a `senator::` address.
    pub fn as_senator(&self) -> Option<&SenatorPath> {
        match self {
            Self::Senator(path) => Some(path),
            Self::Speaker | Self::Committee | Self::Other(_) => None,
        }
    }

    /// A person.
    pub fn is_senior(&self) -> bool {
        self.as_senator().is_some_and(SenatorPath::is_senior)
    }

    /// An agent.
    pub fn is_junior(&self) -> bool {
        self.as_senator().is_some_and(SenatorPath::is_junior)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Senator(path) => write!(f, "{SENATOR_PREFIX}{path}"),
            Self::Speaker => f.write_str(SPEAKER),
            Self::Committee => f.write_str(COMMITTEE),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl FromStr for Address {
    type Err = AddressParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            SPEAKER => Ok(Self::Speaker),
            COMMITTEE => Ok(Self::Committee),
            _ => match s.strip_prefix(SENATOR_PREFIX) {
                Some(path) => Ok(Self::Senator(path.parse()?)),
                None => Err(AddressParseError::Unknown(s.to_string())),
            },
        }
    }
}

impl Serialize for Address {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Address {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(s.parse().unwrap_or(Self::Other(s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        for s in [
            "senator::wendell",
            "senator::wendell/claude",
            "senator::wendell/claude/reviewer",
            "speaker",
            "committee",
        ] {
            let a: Address = s.parse().unwrap();
            assert_eq!(a.to_string(), s);
        }
    }

    #[test]
    fn rejects_other_forms() {
        for s in [
            "senator:wendell",
            "senator::",
            "senator::Wendell",
            "user:wendell",
            "agent:wendell",
            "steward",
            "feature",
            "[speaker]",
        ] {
            assert!(s.parse::<Address>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn one_prefix_and_the_path_says_person_or_agent() {
        let person: Address = "senator::wendell".parse().unwrap();
        let agent: Address = "senator::wendell/claude".parse().unwrap();
        assert!(person.is_senior() && !person.is_junior());
        assert!(agent.is_junior() && !agent.is_senior());
        assert!(!Address::Speaker.is_senior() && !Address::Speaker.is_junior());
        assert_eq!(Address::Committee.as_senator(), None);
    }

    #[test]
    fn unknown_forms_decode_as_other_and_round_trip() {
        for s in ["group:oc_1", "senator::Not-A-Slug", "role:reviewer"] {
            let a: Address = serde_json::from_value(serde_json::json!(s)).unwrap();
            assert_eq!(a, Address::Other(s.to_string()));
            assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::json!(s));
            assert_eq!(a.as_senator(), None);
        }
    }
}
