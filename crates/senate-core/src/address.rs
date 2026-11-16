//! Addresses: who an event is from and who it is to.
//!
//! - `senator::wendell` is a person; `senator::wendell/claude` is one of
//!   their agents. One prefix for both: the path says which.
//! - `senator::wendell/*` is whichever of wendell's agents should get it. A
//!   sender may write it; a stored event never carries it, because it is
//!   resolved against the roster before the event is appended
//!   ([`Roster::resolve_address`](crate::Roster::resolve_address)).
//! - `speaker` is the Speaker of the Senate whose log the event is in. In a
//!   nested Senate that is the senator presiding over it.
//! - `committee` means everyone on the committee.
//! - `guest::<platform>/<id>` is someone who is not a senator: a chat
//!   participant who is not on the roster, a caller arriving through an agent
//!   gateway. See [`Guest`].
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

use crate::ids::{IdError, Name};
use crate::senator::{SenatorPath, SenatorPathError};

pub(crate) const SENATOR_PREFIX: &str = "senator::";
pub(crate) const GUEST_PREFIX: &str = "guest::";
pub(crate) const SPEAKER: &str = "speaker";
pub(crate) const COMMITTEE: &str = "committee";
/// What follows a person's handle in `senator::wendell/*`.
pub(crate) const ANY_AGENT_SUFFIX: &str = "/*";

/// Longest [`Guest`] id, in bytes.
pub const MAX_GUEST_ID_LEN: usize = 128;

/// Someone who is not a senator, named by the platform they came through and
/// the handle that platform's gateway gave them: `guest::chat/u_8f3a`.
///
/// - `platform` is a slug like every other name here: 1-63 characters of
///   `[a-z0-9-]`, starting with a letter or digit.
/// - `id` is an opaque handle the gateway hands out. It is 1 to
///   [`MAX_GUEST_ID_LEN`] ASCII characters from `A-Z a-z 0-9 . _ - ~ @ : + =`
///   and is compared exactly, case included. That keeps it printable and
///   leaves out everything that would make an address or a bracketed mention
///   ambiguous: no whitespace, no `[` or `]`, no `/`, no `*`. A gateway whose
///   native ids do not fit hands out one that does (a hash, an encoding); the
///   native id stays on the gateway's side.
///
/// A guest may ask and be answered. A guest may not change state
/// ([`Address::may_change_state`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Guest {
    platform: Name,
    id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GuestError {
    #[error("a guest is written <platform>/<id>: {0:?}")]
    NoSeparator(String),
    #[error("guest platform: {0}")]
    Platform(#[source] IdError),
    #[error("guest id must be 1-{MAX_GUEST_ID_LEN} chars of [A-Za-z0-9._~@:+=-]: {0:?}")]
    InvalidId(String),
}

fn valid_guest_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_GUEST_ID_LEN
        && id.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(b, b'.' | b'_' | b'-' | b'~' | b'@' | b':' | b'+' | b'=')
        })
}

impl Guest {
    pub fn new(platform: &str, id: &str) -> Result<Self, GuestError> {
        let platform = Name::new(platform).map_err(|e| match e {
            IdError::InvalidSlug { value, .. } => GuestError::Platform(IdError::InvalidSlug {
                kind: "guest platform",
                value,
            }),
            other => GuestError::Platform(other),
        })?;
        if !valid_guest_id(id) {
            return Err(GuestError::InvalidId(id.to_string()));
        }
        Ok(Self {
            platform,
            id: id.to_string(),
        })
    }

    /// The platform the guest came through, as its gateway reports it.
    pub fn platform(&self) -> &str {
        self.platform.as_str()
    }

    /// The gateway's handle for the guest.
    pub fn id(&self) -> &str {
        &self.id
    }
}

/// `<platform>/<id>`, without the `guest::` prefix.
impl fmt::Display for Guest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.platform, self.id)
    }
}

impl FromStr for Guest {
    type Err = GuestError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (platform, id) = s
            .split_once('/')
            .ok_or_else(|| GuestError::NoSeparator(s.to_string()))?;
        Self::new(platform, id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Address {
    Senator(SenatorPath),
    /// `senator::<person>/*`: whichever of that person's agents should get
    /// it. Only ever a recipient a sender wrote. It names the person, not a
    /// level: in a nested Senate it means that person's agents seated there.
    AgentOf(Name),
    Speaker,
    Committee,
    Guest(Guest),
    /// A form this build does not know, kept verbatim.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressParseError {
    #[error(
        "unknown address form: {0:?} (expected senator::<path>, senator::<person>/*, speaker, committee, or guest::<platform>/<id>)"
    )]
    Unknown(String),
    #[error(transparent)]
    Senator(#[from] SenatorPathError),
    #[error(transparent)]
    Guest(#[from] GuestError),
}

impl Address {
    pub fn senator(path: &SenatorPath) -> Self {
        Self::Senator(path.clone())
    }

    /// The person `handle`.
    pub fn senior(handle: &Name) -> Self {
        Self::Senator(SenatorPath::senior(handle.clone()))
    }

    /// Whichever of `owner`'s agents should get it: `senator::<owner>/*`.
    pub fn agent_of(owner: &Name) -> Self {
        Self::AgentOf(owner.clone())
    }

    /// The senator behind a `senator::` address that names one. The wildcard
    /// names none.
    pub fn as_senator(&self) -> Option<&SenatorPath> {
        match self {
            Self::Senator(path) => Some(path),
            _ => None,
        }
    }

    /// The agent behind a `senator::` address that names an agent.
    pub fn as_agent(&self) -> Option<&SenatorPath> {
        self.as_senator().filter(|path| path.is_junior())
    }

    pub fn as_guest(&self) -> Option<&Guest> {
        match self {
            Self::Guest(guest) => Some(guest),
            _ => None,
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

    /// `senator::<person>/*`, which must be resolved before an event is
    /// stored.
    pub fn is_wildcard(&self) -> bool {
        matches!(self, Self::AgentOf(_))
    }

    /// Whether an event from this address may change state: record a
    /// decision, bind a group, invite, order work, reassign.
    ///
    /// Senators may, and so may the Speaker and the Senate itself
    /// (`committee`, which the server writes presence as). A guest may not: a
    /// guest asks and is answered. Neither may the wildcard, which is never
    /// a sender, nor a form this build does not know, which it cannot vouch
    /// for.
    pub fn may_change_state(&self) -> bool {
        match self {
            Self::Senator(_) | Self::Speaker | Self::Committee => true,
            Self::AgentOf(_) | Self::Guest(_) | Self::Other(_) => false,
        }
    }

    /// The address as it is written in running text: in square brackets
    /// (`[senator::wendell/claude]`), the form [`crate::Mention`] reads back.
    /// An unknown form is bracketed verbatim, so it still has edges.
    pub fn in_text(&self) -> String {
        format!("[{self}]")
    }
}

impl From<SenatorPath> for Address {
    fn from(path: SenatorPath) -> Self {
        Self::Senator(path)
    }
}

impl From<Guest> for Address {
    fn from(guest: Guest) -> Self {
        Self::Guest(guest)
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Senator(path) => write!(f, "{SENATOR_PREFIX}{path}"),
            Self::AgentOf(owner) => write!(f, "{SENATOR_PREFIX}{owner}{ANY_AGENT_SUFFIX}"),
            Self::Speaker => f.write_str(SPEAKER),
            Self::Committee => f.write_str(COMMITTEE),
            Self::Guest(guest) => write!(f, "{GUEST_PREFIX}{guest}"),
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
            _ => {
                if let Some(rest) = s.strip_prefix(SENATOR_PREFIX) {
                    // Only a person has "whichever agent": `wendell/claude/*`
                    // is not a form, and fails here as a bad name.
                    match rest.strip_suffix(ANY_AGENT_SUFFIX) {
                        Some(owner) => Ok(Self::AgentOf(
                            Name::new(owner).map_err(SenatorPathError::from)?,
                        )),
                        None => Ok(Self::Senator(rest.parse()?)),
                    }
                } else if let Some(rest) = s.strip_prefix(GUEST_PREFIX) {
                    Ok(Self::Guest(rest.parse()?))
                } else {
                    Err(AddressParseError::Unknown(s.to_string()))
                }
            }
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

    fn addr(s: &str) -> Address {
        s.parse().unwrap()
    }

    #[test]
    fn round_trips() {
        for s in [
            "senator::wendell",
            "senator::wendell/claude",
            "senator::wendell/claude/reviewer",
            "senator::wendell/*",
            "speaker",
            "committee",
            "guest::chat/u_8f3a",
            "guest::dial/Reviewer-7@example.com",
        ] {
            let a: Address = s.parse().unwrap();
            assert_eq!(a.to_string(), s);
            assert_eq!(a.in_text(), format!("[{s}]"));
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
            "guest:chat/u_1",
            "guest::chat",
        ] {
            assert!(s.parse::<Address>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn one_prefix_and_the_path_says_person_or_agent() {
        let person = addr("senator::wendell");
        let agent = addr("senator::wendell/claude");
        assert!(person.is_senior() && !person.is_junior());
        assert!(agent.is_junior() && !agent.is_senior());
        assert_eq!(person.as_agent(), None);
        assert_eq!(agent.as_agent(), agent.as_senator());
        assert!(!Address::Speaker.is_senior() && !Address::Speaker.is_junior());
        assert_eq!(Address::Committee.as_senator(), None);
    }

    #[test]
    fn whichever_agent_names_a_person_and_no_senator() {
        let any = addr("senator::wendell/*");
        assert_eq!(any, Address::agent_of(&Name::new("wendell").unwrap()));
        assert!(any.is_wildcard());
        assert_eq!(any.as_senator(), None);
        assert!(!any.is_senior() && !any.is_junior());
        assert!(!addr("senator::wendell/claude").is_wildcard());

        // Only directly under a person, and only as the whole last segment.
        for s in [
            "senator::*",
            "senator::/*",
            "senator::wendell/claude/*",
            "senator::wendell/*/claude",
            "senator::wendell/c*",
            "senator::Wendell/*",
        ] {
            assert!(s.parse::<Address>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn a_guest_is_a_platform_and_an_opaque_printable_id() {
        let guest = Guest::new("chat", "u_8f3a").unwrap();
        assert_eq!(guest.platform(), "chat");
        assert_eq!(guest.id(), "u_8f3a");
        assert_eq!(Address::from(guest.clone()).as_guest(), Some(&guest));
        // The id is opaque: case is kept and compared.
        assert_ne!(Guest::new("chat", "U_8F3A").unwrap(), guest);
        assert!(Guest::new("chat", &"x".repeat(MAX_GUEST_ID_LEN)).is_ok());

        for (platform, id) in [
            ("", "u1"),
            ("Chat", "u1"),
            ("chat room", "u1"),
            ("chat", ""),
            ("chat", "a b"),
            ("chat", "a/b"),
            ("chat", "a]b"),
            ("chat", "a[b"),
            ("chat", "*"),
            ("chat", "名字"),
            ("chat", "a\nb"),
        ] {
            assert!(Guest::new(platform, id).is_err(), "{platform:?} {id:?}");
        }
        assert!(Guest::new("chat", &"x".repeat(MAX_GUEST_ID_LEN + 1)).is_err());
        assert!("guest::chat/a/b".parse::<Address>().is_err());
    }

    #[test]
    fn guests_wildcards_and_unknown_forms_may_not_change_state() {
        for s in [
            "senator::wendell",
            "senator::wendell/claude",
            "speaker",
            "committee",
        ] {
            assert!(addr(s).may_change_state(), "{s}");
        }
        for a in [
            addr("guest::chat/u_1"),
            addr("senator::wendell/*"),
            Address::Other("role:reviewer".into()),
        ] {
            assert!(!a.may_change_state(), "{a}");
        }
    }

    #[test]
    fn unknown_forms_decode_as_other_and_round_trip() {
        for s in [
            "group:oc_1",
            "senator::Not-A-Slug",
            "role:reviewer",
            "guest::chat/has space",
            "senator::wendell/claude/*",
        ] {
            let a: Address = serde_json::from_value(serde_json::json!(s)).unwrap();
            assert_eq!(a, Address::Other(s.to_string()));
            assert_eq!(serde_json::to_value(&a).unwrap(), serde_json::json!(s));
            assert_eq!(a.as_senator(), None);
        }
        let known: Address = serde_json::from_value(serde_json::json!("guest::chat/u_1")).unwrap();
        assert!(known.as_guest().is_some());
    }
}
