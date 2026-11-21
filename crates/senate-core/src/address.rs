//! Addresses: who an event is from and who it is to.
//!
//! - `senator::wendell` is a person, `senator::codex` an agent: one flat
//!   name ([`SenatorName`]), scoped to the Senate whose log this is. Which
//!   of the two a name is, the roster says.
//! - `senator::*` is whichever agent on this committee should get it. A
//!   sender may write it; a stored event never carries it, because it is
//!   resolved against the roster before the event is appended
//!   ([`Roster::resolve_address`](crate::Roster::resolve_address)).
//! - `speaker` is the Speaker of the committee whose log the event is in: a
//!   link, resolved to the senator who holds the Speaker's seat there (or
//!   the built-in Speaker). What is said in the Speaker's capacity is stored
//!   `from: speaker`, whoever held the seat.
//! - `chair` is the chair of the committee whose log the event is in: a
//!   link, like `speaker`, resolved to the committee's chair at delivery
//!   ([`crate::chair`]).
//! - `<child>::<committee>::speaker` is a child Senate of this one, reached
//!   through its Speaker's office for that committee:
//!   `north-desk::ledger::speaker`. A parent never needs to know a child's
//!   senators; what the child's Speaker's office says above is authored so.
//!
//! A qualified name ([`crate::Locator`]) names a seat anywhere; only the
//! forms above are stored as addresses.
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
use crate::senator::{SenatorName, SenatorNameError};

pub(crate) const SENATOR_PREFIX: &str = "senator::";
pub(crate) const GUEST_PREFIX: &str = "guest::";
pub(crate) const SPEAKER: &str = "speaker";
pub(crate) const COMMITTEE: &str = "committee";
pub(crate) const CHAIR: &str = "chair";
/// `senator::*`: whichever agent.
pub(crate) const ANY_AGENT: &str = "*";
/// What separates the parts of a qualified name and of the child-Senate
/// address.
pub(crate) const SEPARATOR: &str = "::";

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
    Senator(SenatorName),
    /// `senator::*`: whichever agent on this committee should get it. Only
    /// ever a recipient a sender wrote.
    AnyAgent,
    Speaker,
    /// The committee's chair: a link resolved at delivery.
    Chair,
    Committee,
    Guest(Guest),
    /// `<senate>::<committee>::speaker`: the Speaker's office of the child
    /// Senate `senate` for `committee`, as the parent's log names it.
    Child {
        senate: crate::ids::Name,
        committee: crate::ids::CommitteeId,
    },
    /// A form this build does not know, kept verbatim.
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressParseError {
    #[error(
        "unknown address form: {0:?} (expected senator::<name>, senator::*, speaker, committee, or guest::<platform>/<id>)"
    )]
    Unknown(String),
    #[error(transparent)]
    Senator(#[from] SenatorNameError),
    #[error(transparent)]
    Guest(#[from] GuestError),
}

impl Address {
    pub fn senator(name: &SenatorName) -> Self {
        Self::Senator(name.clone())
    }

    /// The person with `handle`: `senator::<handle>`. A handle that is an
    /// address keyword (never a listed person's) is kept verbatim as
    /// [`Address::Other`].
    pub fn senior(handle: &Name) -> Self {
        match SenatorName::new(handle.clone()) {
            Ok(name) => Self::Senator(name),
            Err(_) => Self::Other(format!("{SENATOR_PREFIX}{handle}")),
        }
    }

    /// Whichever agent should get it: `senator::*`.
    pub fn any_agent() -> Self {
        Self::AnyAgent
    }

    /// The senator behind a `senator::` address that names one. The wildcard
    /// names none.
    pub fn as_senator(&self) -> Option<&SenatorName> {
        match self {
            Self::Senator(name) => Some(name),
            _ => None,
        }
    }

    /// The Speaker's office of the child Senate `senate` for `committee`.
    pub fn child(senate: crate::ids::Name, committee: crate::ids::CommitteeId) -> Self {
        Self::Child { senate, committee }
    }

    /// The child Senate and committee a `<senate>::<committee>::speaker`
    /// address names.
    pub fn as_child(&self) -> Option<(&crate::ids::Name, &crate::ids::CommitteeId)> {
        match self {
            Self::Child { senate, committee } => Some((senate, committee)),
            _ => None,
        }
    }

    pub fn as_guest(&self) -> Option<&Guest> {
        match self {
            Self::Guest(guest) => Some(guest),
            _ => None,
        }
    }

    /// `senator::*`, which must be resolved before an event is stored.
    pub fn is_wildcard(&self) -> bool {
        matches!(self, Self::AnyAgent)
    }

    /// Whether an event from this address may change state: record a
    /// decision, bind a group, invite, order work, reassign.
    ///
    /// Senators may, and so may the Speaker and the Senate itself (`committee`, which the server writes presence
    /// as). A guest may not: a guest asks and is answered. Neither may the
    /// wildcard, which is never a sender, nor a form this build does not
    /// know, which it cannot vouch for.
    pub fn may_change_state(&self) -> bool {
        match self {
            Self::Senator(_)
            | Self::Speaker
            | Self::Chair
            | Self::Committee
            | Self::Child { .. } => true,
            Self::AnyAgent | Self::Guest(_) | Self::Other(_) => false,
        }
    }

    /// The address as it is written in running text: in square brackets
    /// (`[senator::codex]`), the form [`crate::Mention`] reads back. An
    /// unknown form is bracketed verbatim, so it still has edges.
    pub fn in_text(&self) -> String {
        format!("[{self}]")
    }
}

impl From<SenatorName> for Address {
    fn from(name: SenatorName) -> Self {
        Self::Senator(name)
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
            Self::Senator(name) => write!(f, "{SENATOR_PREFIX}{name}"),
            Self::AnyAgent => write!(f, "{SENATOR_PREFIX}{ANY_AGENT}"),
            Self::Speaker => f.write_str(SPEAKER),
            Self::Chair => f.write_str(CHAIR),
            Self::Committee => f.write_str(COMMITTEE),
            Self::Child { senate, committee } => {
                write!(f, "{senate}{SEPARATOR}{committee}{SEPARATOR}{SPEAKER}")
            }
            Self::Guest(guest) => write!(f, "{GUEST_PREFIX}{guest}"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

/// `<senate>::<committee>::speaker`, when `s` is exactly that: two slugs
/// that are no keyword, then `speaker`.
fn parse_child(s: &str) -> Option<Address> {
    let rest = s.strip_suffix(SPEAKER)?.strip_suffix(SEPARATOR)?;
    let (senate, committee) = rest.split_once(SEPARATOR)?;
    if senate.contains('/') {
        return None;
    }
    let senate = crate::ids::Name::new(senate).ok()?;
    let committee = crate::ids::CommitteeId::new(committee).ok()?;
    if crate::senate::check_not_keyword(senate.as_str(), "a Senate").is_err()
        || crate::senate::check_not_keyword(committee.as_str(), "a committee").is_err()
    {
        return None;
    }
    Some(Address::Child { senate, committee })
}

impl FromStr for Address {
    type Err = AddressParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(child) = parse_child(s) {
            return Ok(child);
        }
        match s {
            SPEAKER => Ok(Self::Speaker),
            CHAIR => Ok(Self::Chair),
            COMMITTEE => Ok(Self::Committee),
            _ => {
                if let Some(rest) = s.strip_prefix(SENATOR_PREFIX) {
                    if rest == ANY_AGENT {
                        Ok(Self::AnyAgent)
                    } else {
                        Ok(Self::Senator(rest.parse()?))
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
            "senator::codex",
            "senator::*",
            "speaker",
            "committee",
            "guest::chat/u_8f3a",
            "guest::web/Reviewer-7@example.com",
            "chair",
            "north-desk::ledger::speaker",
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
            "senator::wendell/claude",
            "senator::wendell/*",
            "senator::speaker",
            "user:wendell",
            "agent:wendell",
            "[speaker]",
            "guest:chat/u_1",
            "guest::chat",
            // A qualified name is a locator, not a stored address.
            "acme/north-desk::ledger::speaker",
            "ledger::senator::codex",
            "ledger::chair",
        ] {
            assert!(s.parse::<Address>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn a_senator_is_one_flat_name_and_the_roster_says_person_or_agent() {
        let person = addr("senator::wendell");
        let agent = addr("senator::codex");
        assert_eq!(person.as_senator().unwrap().as_str(), "wendell");
        assert_eq!(agent.as_senator().unwrap().as_str(), "codex");
        assert_eq!(Address::Speaker.as_senator(), None);
        assert_eq!(Address::Committee.as_senator(), None);
    }

    #[test]
    fn whichever_agent_names_no_senator() {
        let any = addr("senator::*");
        assert_eq!(any, Address::any_agent());
        assert!(any.is_wildcard());
        assert_eq!(any.as_senator(), None);
        assert!(!addr("senator::codex").is_wildcard());
        for s in [
            "senator::/*",
            "senator::codex/*",
            "senator::c*",
            "senator::**",
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
        for s in ["senator::wendell", "senator::codex", "speaker", "committee"] {
            assert!(addr(s).may_change_state(), "{s}");
        }
        for a in [
            addr("guest::chat/u_1"),
            addr("senator::*"),
            Address::Other("role:reviewer".into()),
        ] {
            assert!(!a.may_change_state(), "{a}");
        }
    }

    #[test]
    fn unknown_forms_decode_as_other_and_round_trip() {
        for s in [
            "group:g-1",
            "senator::Not-A-Slug",
            "role:reviewer",
            "guest::chat/has space",
            // The old owner-prefixed forms: an unconverted log still reads.
            "senator::wendell/claude",
            "senator::wendell/*",
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
