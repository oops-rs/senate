//! Who someone is, as opposed to where a message goes.
//!
//! A seat ([`crate::Locator`]) names a senator on a committee: where a
//! message is delivered. A **senator id** names a senator of a Senate, with
//! no committee: who someone is. Configuration and the roster use it
//! wherever they refer to a senator (the Speaker, a secretary, a chair, the
//! principal that runs an agent):
//!
//! ```text
//! [<absolute senate path>::]senator::<name>
//!
//! senator::wendell          a senator of the Senate the text belongs to
//! /acme::senator::wendell   wendell of the Senate /acme
//! ```
//!
//! With one name before the address, a seat and a senator id would read the
//! same (`acme::senator::wendell`: the committee `acme`, or the Senate
//! `acme`?). So a senator id's Senate path must be absolute, starting with
//! `/`; the bare form is refused with both meanings spelled out
//! ([`SenatorIdParseError::Ambiguous`]). It is printed with the slash.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::address::{SENATOR_PREFIX, SEPARATOR};
use crate::senate::{SenatePath, SenatePathError};
use crate::senator::{SenatorName, SenatorNameError};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SenatorIdParseError {
    #[error("{0:?} is not a senator id: write senator::<name>, or /<senate>::senator::<name>")]
    Form(String),
    #[error(
        "{written:?} is ambiguous: as a seat it is the senator {name} on the committee {first}; as a senator id it is {name} of a Senate {first}, which is written /{first}::senator::{name} (a Senate path in a senator id starts with /)"
    )]
    Ambiguous {
        written: String,
        first: String,
        name: String,
    },
    #[error(transparent)]
    Senate(#[from] SenatePathError),
    #[error(transparent)]
    Name(#[from] SenatorNameError),
}

/// A senator of a Senate: see the module docs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SenatorId {
    senate: Option<SenatePath>,
    name: SenatorName,
}

impl SenatorId {
    /// `senator::<name>`: a senator of the Senate the text belongs to.
    pub fn here(name: SenatorName) -> Self {
        Self { senate: None, name }
    }

    /// `/<senate>::senator::<name>`. The path is taken as absolute.
    pub fn of(senate: SenatePath, name: SenatorName) -> Self {
        Self {
            senate: Some(senate.absolute()),
            name,
        }
    }

    /// The Senate it names, when it names one (absolute).
    pub fn senate(&self) -> Option<&SenatePath> {
        self.senate.as_ref()
    }

    pub fn name(&self) -> &SenatorName {
        &self.name
    }

    /// Bracketed for running text: `[/acme::senator::wendell]`.
    pub fn in_text(&self) -> String {
        format!("[{self}]")
    }
}

impl fmt::Display for SenatorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(senate) = &self.senate {
            write!(f, "{senate}{SEPARATOR}")?;
        }
        write!(f, "{SENATOR_PREFIX}{}", self.name)
    }
}

impl FromStr for SenatorId {
    type Err = SenatorIdParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let form = || SenatorIdParseError::Form(s.to_string());
        let (head, name) = match s.rsplit_once(SENATOR_PREFIX) {
            Some(parts) => parts,
            None => return Err(form()),
        };
        let name: SenatorName = name.parse()?;
        if head.is_empty() {
            return Ok(Self::here(name));
        }
        let senate = head.strip_suffix(SEPARATOR).ok_or_else(form)?;
        if senate.is_empty() || senate.contains(SEPARATOR) {
            return Err(form());
        }
        if !senate.starts_with('/') {
            return Err(SenatorIdParseError::Ambiguous {
                written: s.to_string(),
                first: senate.to_string(),
                name: name.to_string(),
            });
        }
        Ok(Self::of(senate.parse()?, name))
    }
}

impl Serialize for SenatorId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SenatorId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> SenatorId {
        s.parse().unwrap()
    }

    #[test]
    fn a_senator_id_is_a_name_with_or_without_an_absolute_senate() {
        let here = id("senator::wendell");
        assert_eq!(here.senate(), None);
        assert_eq!(here.name().as_str(), "wendell");
        assert_eq!(here.to_string(), "senator::wendell");

        let above = id("/acme::senator::wendell");
        assert_eq!(above.senate().unwrap().to_string(), "/acme");
        assert!(above.senate().unwrap().is_absolute());
        assert_eq!(above.to_string(), "/acme::senator::wendell");
        assert_eq!(above.in_text(), "[/acme::senator::wendell]");
        assert_eq!(
            id("/acme/north-desk::senator::claude").to_string(),
            "/acme/north-desk::senator::claude"
        );
        assert_eq!(
            SenatorId::of("acme".parse().unwrap(), "wendell".parse().unwrap()),
            above
        );
    }

    #[test]
    fn a_bare_senate_is_refused_with_both_meanings() {
        let err = "acme::senator::wendell".parse::<SenatorId>().unwrap_err();
        assert!(matches!(err, SenatorIdParseError::Ambiguous { .. }));
        let text = err.to_string();
        assert!(text.contains("on the committee acme"), "{text}");
        assert!(text.contains("/acme::senator::wendell"), "{text}");
    }

    #[test]
    fn what_is_not_a_senator_id_is_refused() {
        for bad in [
            "wendell",
            "speaker",
            "senator::",
            "senator::wendell/claude",
            "/acme::ledger::senator::wendell",
            "::senator::wendell",
            "/acme:senator::wendell",
            "senator::speaker",
        ] {
            assert!(bad.parse::<SenatorId>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn serializes_as_a_string() {
        let value = serde_json::to_value(id("/acme::senator::wendell")).unwrap();
        assert_eq!(value, serde_json::json!("/acme::senator::wendell"));
        assert_eq!(
            serde_json::from_value::<SenatorId>(value).unwrap(),
            id("/acme::senator::wendell")
        );
    }
}
