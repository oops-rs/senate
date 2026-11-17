//! Senates and their names.
//!
//! Only Senates nest. Every Senate has a name, a slug set in its
//! configuration ([`Name`]), and its place in the tree is the path of names
//! from the top Senate down to it, parent first, `/` between:
//! `/acme/north-desk` is the Senate `north-desk` under `acme` ([`SenatePath`]).
//!
//! A path that starts with `/` is **absolute**: it names the Senate from the
//! root. One without is relative: its first name is looked for nearest
//! first, from where the writer stands outward. An absolute path is always
//! printed with its leading `/`.
//!
//! What a Senate holds is scoped to it: its committees (flat ids) and its
//! senators (flat names, [`crate::SenatorName`]). A committee and a senator
//! are found from outside by qualifying them with the Senate's name; see
//! [`crate::Locator`].
//!
//! A Senate name and a committee id may not be an address keyword
//! (`senator`, `speaker`, `chair`, `committee`, `guest`): a qualified name
//! is read from the right, and a keyword in a qualifier's place could not
//! be told from the address. [`check_not_keyword`] is that rule.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::ids::{IdError, Name};

/// The words an address is made of. Nothing that qualifies an address (a
/// Senate name, a committee id) and no senator may be named after one.
pub const KEYWORDS: [&str; 5] = ["senator", "speaker", "chair", "committee", "guest"];

/// How deep a Senate path may go. A guard against runaway nesting, not a
/// design limit.
pub const MAX_SENATE_DEPTH: usize = 16;

/// A name that is an address keyword, where it would make a qualified name
/// ambiguous.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{what} may not be named {name:?}: it is an address keyword (senator, speaker, chair, committee, guest)"
)]
pub struct KeywordError {
    pub what: &'static str,
    pub name: String,
}

/// Refuse `name` for `what` (`"a Senate"`, `"a committee"`, `"a senator"`)
/// when it is one of [`KEYWORDS`].
pub fn check_not_keyword(name: &str, what: &'static str) -> Result<(), KeywordError> {
    if KEYWORDS.contains(&name) {
        Err(KeywordError {
            what,
            name: name.to_string(),
        })
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SenatePathError {
    #[error("a Senate path is one or more Senate names, parent first: {0:?}")]
    Empty(String),
    #[error(transparent)]
    Name(#[from] IdError),
    #[error(transparent)]
    Keyword(#[from] KeywordError),
    #[error("a Senate path nests more than {MAX_SENATE_DEPTH} Senates: {0:?}")]
    TooDeep(String),
}

/// Where a Senate sits: the names from the top Senate down to it, parent
/// first. `/acme/north-desk` from the root; `north-desk` relative.
///
/// A relative path in a qualified name is looked for nearest first, from
/// where the writer stands outward (see [`crate::Locator`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SenatePath {
    names: Vec<Name>,
    absolute: bool,
}

impl SenatePath {
    /// A root Senate: one with no Senate above. Absolute.
    pub fn top(name: Name) -> Result<Self, SenatePathError> {
        Ok(Self::new(vec![name])?.absolute())
    }

    /// A relative path of `names`.
    pub fn new(names: Vec<Name>) -> Result<Self, SenatePathError> {
        if names.is_empty() {
            return Err(SenatePathError::Empty(String::new()));
        }
        if names.len() > MAX_SENATE_DEPTH {
            let joined: Vec<&str> = names.iter().map(Name::as_str).collect();
            return Err(SenatePathError::TooDeep(joined.join("/")));
        }
        for name in &names {
            check_not_keyword(name.as_str(), "a Senate")?;
        }
        Ok(Self {
            names,
            absolute: false,
        })
    }

    /// The same names, from the root.
    pub fn absolute(mut self) -> Self {
        self.absolute = true;
        self
    }

    /// Whether the path names the Senate from the root (`/acme`).
    pub fn is_absolute(&self) -> bool {
        self.absolute
    }

    /// The names, top first.
    pub fn names(&self) -> &[Name] {
        &self.names
    }

    /// This Senate's own name: the last one.
    pub fn name(&self) -> &Name {
        self.names
            .last()
            .unwrap_or_else(|| unreachable!("a Senate path is never empty"))
    }

    /// The Senate above; `None` for a path of one name.
    pub fn parent(&self) -> Option<SenatePath> {
        (self.names.len() > 1).then(|| Self {
            names: self.names[..self.names.len() - 1].to_vec(),
            absolute: self.absolute,
        })
    }

    /// The child Senate `name` of this one.
    pub fn child(&self, name: Name) -> Result<SenatePath, SenatePathError> {
        let mut names = self.names.clone();
        names.push(name);
        let mut path = Self::new(names)?;
        path.absolute = self.absolute;
        Ok(path)
    }

    /// Whether this path ends with `other`'s names: `/acme/north-desk` ends
    /// with `north-desk` and with itself. An absolute `other` must be all of
    /// this path.
    pub fn ends_with(&self, other: &SenatePath) -> bool {
        if other.absolute {
            self.names == other.names
        } else {
            self.names.ends_with(&other.names)
        }
    }

    /// Whether both name the same Senates, absolute or not.
    pub fn same_names(&self, other: &SenatePath) -> bool {
        self.names == other.names
    }

    pub fn depth(&self) -> usize {
        self.names.len()
    }
}

impl fmt::Display for SenatePath {
    /// An absolute path is printed with its leading `/`: the one place this
    /// is decided.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.absolute {
            f.write_str("/")?;
        }
        let names: Vec<&str> = self.names.iter().map(Name::as_str).collect();
        f.write_str(&names.join("/"))
    }
}

impl FromStr for SenatePath {
    type Err = SenatePathError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (absolute, rest) = match s.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        if rest.is_empty() {
            return Err(SenatePathError::Empty(s.to_string()));
        }
        let names = rest
            .split('/')
            .map(Name::new)
            .collect::<Result<Vec<_>, _>>()?;
        let mut path = Self::new(names)?;
        path.absolute = absolute;
        Ok(path)
    }
}

impl Serialize for SenatePath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SenatePath {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A Senate's name as configuration gives it: a slug that is not an
/// address keyword.
pub fn senate_name(s: &str) -> Result<Name, SenatePathError> {
    let name = Name::new(s)?;
    check_not_keyword(name.as_str(), "a Senate")?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> SenatePath {
        s.parse().unwrap()
    }

    #[test]
    fn a_path_is_names_parent_first_and_absolute_from_the_root() {
        let p = path("/acme/north-desk");
        assert!(p.is_absolute());
        assert_eq!(p.to_string(), "/acme/north-desk");
        assert_eq!(p.name().as_str(), "north-desk");
        assert_eq!(p.parent(), Some(path("/acme")));
        assert_eq!(path("/acme").parent(), None);
        assert_eq!(
            path("/acme").child(Name::new("north-desk").unwrap()),
            Ok(p.clone())
        );
        let relative = path("acme/north-desk");
        assert!(!relative.is_absolute());
        assert_eq!(relative.to_string(), "acme/north-desk");
        assert_ne!(relative, p);
        assert!(relative.same_names(&p));
        assert!(p.ends_with(&path("north-desk")));
        assert!(p.ends_with(&path("/acme/north-desk")));
        assert!(!p.ends_with(&path("/north-desk")));
        assert!(!p.ends_with(&path("acme")));
        assert_eq!(
            SenatePath::top(Name::new("acme").unwrap()),
            Ok(path("/acme"))
        );
        for bad in [
            "",
            "/",
            "//acme",
            "acme/",
            "acme//x",
            "Acme",
            "acme/speaker",
            "/chair",
        ] {
            assert!(bad.parse::<SenatePath>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_keyword_names_no_senate() {
        for k in KEYWORDS {
            assert!(senate_name(k).is_err(), "{k}");
            assert!(check_not_keyword(k, "a committee").is_err());
        }
        assert!(senate_name("north-desk").is_ok());
        let err = check_not_keyword("speaker", "a committee").unwrap_err();
        assert!(err.to_string().contains("a committee"), "{err}");
    }
}
