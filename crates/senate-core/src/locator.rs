//! Qualified names: a seat, a Speaker or a committee named from anywhere.
//!
//! An [`Address`] is stored in one committee's log and means something only
//! there. A [`Locator`] says where, too: positional, read from the right,
//! with no labels:
//!
//! ```text
//! [<senate path>::][<committee>::]<address>
//!
//! acme/north-desk::ledger::senator::codex   the seat codex on ledger in acme/north-desk
//! ledger::senator::codex                    the same, in the nearest Senate that has ledger
//! ledger::speaker                           the Speaker of ledger
//! senator::codex                          here: this Senate, this committee
//! ```
//!
//! One name before the address is a committee; two are a Senate path and a
//! committee. A part that starts with `/` or holds a `/` is a Senate path
//! (`/acme/north-desk` from the root, `acme/north-desk` relative): a single
//! such part before the address is refused, because there is no form with a
//! Senate and no committee (that is a senator id,
//! [`crate::SenatorId`], not a seat). A Senate name and a committee id are
//! never an address keyword (`senator`, `speaker`, `chair`, `committee`,
//! `guest`, see [`crate::senate::KEYWORDS`]), so the address starts at the
//! first keyword. An absolute Senate path is printed with its leading `/`.
//!
//! What a locator leaves out comes from where the writer stands: a missing
//! committee is the writer's own, a missing Senate is looked for nearest
//! first (the writer's Senate, then the one above, outward). A locator with
//! both is [complete](Locator::is_complete); whether its Senate path starts
//! at the top is something only resolution knows.
//!
//! Parsing ([`FromStr`]) is strict. Tools also accept a short form
//! ([`Locator::parse_short`]), where a bare name is a senator: `codex` is
//! `senator::codex`, `ledger::codex` is `ledger::senator::codex`, `*` is
//! `senator::*`.
//!
//! In running text a locator is bracketed like any id:
//! `[ledger::senator::codex]` ([`Locator::in_text`]).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::address::{ANY_AGENT, Address, AddressParseError, SENATOR_PREFIX, SEPARATOR};
use crate::ids::CommitteeId;
use crate::senate::{KEYWORDS, SenatePath, check_not_keyword};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocatorParseError {
    #[error(
        "{0:?} names no address: a qualified name ends with senator::<name>, senator::*, speaker, committee or guest::<platform>/<id>"
    )]
    NoAddress(String),
    #[error("{0:?} has too many names before the address: at most a Senate path and a committee")]
    TooManyQualifiers(String),
    #[error("{0}")]
    Qualifier(String),
    #[error(
        "{0:?} names a Senate and no committee: a seat is <senate>::<committee>::<address>; a senator of a Senate is written /<senate>::senator::<name>"
    )]
    NoCommittee(String),
    #[error(transparent)]
    Address(#[from] AddressParseError),
}

/// A qualified name. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Locator {
    senate: Option<SenatePath>,
    committee: Option<CommitteeId>,
    address: Address,
}

impl Locator {
    /// `address` here: this Senate, this committee.
    pub fn here(address: Address) -> Self {
        Self {
            senate: None,
            committee: None,
            address,
        }
    }

    /// `address` on `committee`, in the nearest Senate that has it.
    pub fn on(committee: CommitteeId, address: Address) -> Self {
        Self {
            senate: None,
            committee: Some(committee),
            address,
        }
    }

    /// `address` on `committee` in `senate`.
    pub fn at(senate: SenatePath, committee: CommitteeId, address: Address) -> Self {
        Self {
            senate: Some(senate),
            committee: Some(committee),
            address,
        }
    }

    pub fn senate(&self) -> Option<&SenatePath> {
        self.senate.as_ref()
    }

    pub fn committee(&self) -> Option<&CommitteeId> {
        self.committee.as_ref()
    }

    pub fn address(&self) -> &Address {
        &self.address
    }

    /// Neither a Senate nor a committee: an address here.
    pub fn is_here(&self) -> bool {
        self.senate.is_none() && self.committee.is_none()
    }

    /// Both a Senate and a committee.
    pub fn is_complete(&self) -> bool {
        self.senate.is_some() && self.committee.is_some()
    }

    /// The parts left out filled in from where the writer stands: the
    /// writer's committee, and its Senate. A part already given is kept.
    pub fn completed(&self, senate: &SenatePath, committee: &CommitteeId) -> Self {
        Self {
            senate: Some(self.senate.clone().unwrap_or_else(|| senate.clone())),
            committee: Some(self.committee.clone().unwrap_or_else(|| committee.clone())),
            address: self.address.clone(),
        }
    }

    /// The same locator with its Senate replaced.
    pub fn in_senate(&self, senate: SenatePath) -> Self {
        Self {
            senate: Some(senate),
            ..self.clone()
        }
    }

    /// The same locator with its committee replaced.
    pub fn on_committee(&self, committee: CommitteeId) -> Self {
        Self {
            committee: Some(committee),
            ..self.clone()
        }
    }

    /// The address this locator is in the log of wherever it is read: the
    /// address itself when nothing qualifies it; `None` otherwise.
    pub fn as_address(&self) -> Option<Address> {
        match (&self.senate, &self.committee, &self.address) {
            (None, None, address) => Some(address.clone()),
            (Some(senate), Some(committee), Address::Speaker)
                if senate.depth() == 1 && !senate.is_absolute() =>
            {
                Some(Address::Child {
                    senate: senate.name().clone(),
                    committee: committee.clone(),
                })
            }
            _ => None,
        }
    }

    /// Written in running text: `[ledger::senator::codex]`.
    pub fn in_text(&self) -> String {
        format!("[{self}]")
    }

    /// The short form tools accept besides the strict one: a bare last
    /// name is a senator (`codex`, `ledger::codex`, `acme::ledger::codex`) and
    /// `*` is `senator::*`.
    pub fn parse_short(s: &str) -> Result<Self, LocatorParseError> {
        let s = s.trim();
        let parts: Vec<&str> = s.split(SEPARATOR).collect();
        if parts.iter().any(|p| KEYWORDS.contains(p)) {
            return s.parse();
        }
        let (last, qualifiers) = parts
            .split_last()
            .unwrap_or_else(|| unreachable!("split yields one part at least"));
        let address = if *last == ANY_AGENT {
            Address::AnyAgent
        } else {
            format!("{SENATOR_PREFIX}{last}").parse()?
        };
        Self::assemble(s, qualifiers, address)
    }

    fn assemble(
        whole: &str,
        qualifiers: &[&str],
        address: Address,
    ) -> Result<Self, LocatorParseError> {
        if matches!(address, Address::Other(_) | Address::Child { .. }) {
            return Err(LocatorParseError::NoAddress(whole.to_string()));
        }
        let committee = |raw: &str| -> Result<CommitteeId, LocatorParseError> {
            let id =
                CommitteeId::new(raw).map_err(|e| LocatorParseError::Qualifier(e.to_string()))?;
            check_not_keyword(id.as_str(), "a committee")
                .map_err(|e| LocatorParseError::Qualifier(e.to_string()))?;
            Ok(id)
        };
        let senate = |raw: &str| -> Result<SenatePath, LocatorParseError> {
            raw.parse().map_err(|e: crate::senate::SenatePathError| {
                LocatorParseError::Qualifier(e.to_string())
            })
        };
        match qualifiers {
            [] => Ok(Self::here(address)),
            [one] if one.contains('/') => Err(LocatorParseError::NoCommittee(whole.to_string())),
            [c] => Ok(Self::on(committee(c)?, address)),
            [s, c] => Ok(Self::at(senate(s)?, committee(c)?, address)),
            _ => Err(LocatorParseError::TooManyQualifiers(whole.to_string())),
        }
    }
}

/// Where a message started, when that is not where it is stored
/// ([`meta::ORIGIN`](crate::meta::ORIGIN)): the seat that wrote it, as a
/// complete qualified name, and the root of its thread there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub locator: Locator,
    pub thread: crate::ids::Seq,
}

impl Origin {
    pub fn of(event: &crate::event::Event) -> Option<Self> {
        serde_json::from_value(event.meta.get(crate::event::meta::ORIGIN)?.clone()).ok()
    }

    pub fn value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// [`Origin::of`] for an event not stored yet.
    pub fn of_new(event: &crate::event::NewEvent) -> Option<Self> {
        serde_json::from_value(event.meta.get(crate::event::meta::ORIGIN)?.clone()).ok()
    }
}

/// The qualified name a message is meant for, when it is not where it is
/// stored ([`meta::INTENDED`](crate::meta::INTENDED)).
pub fn intended(event: &crate::event::Event) -> Option<Locator> {
    event
        .meta
        .get(crate::event::meta::INTENDED)?
        .as_str()?
        .parse()
        .ok()
}

impl From<Address> for Locator {
    /// An address as a locator: a child Senate's Speaker is qualified with
    /// the child's name (relative to the log it is in) and committee;
    /// anything else is here.
    fn from(address: Address) -> Self {
        match address {
            Address::Child { senate, committee } => {
                Self {
                    senate: Some(SenatePath::new(vec![senate]).unwrap_or_else(|_| {
                        unreachable!("a child address holds a valid Senate name")
                    })),
                    committee: Some(committee),
                    address: Address::Speaker,
                }
            }
            other => Self::here(other),
        }
    }
}

impl fmt::Display for Locator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(senate) = &self.senate {
            write!(f, "{senate}{SEPARATOR}")?;
        }
        if let Some(committee) = &self.committee {
            write!(f, "{committee}{SEPARATOR}")?;
        }
        write!(f, "{}", self.address)
    }
}

impl FromStr for Locator {
    type Err = LocatorParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(SEPARATOR).collect();
        // The address starts at the first keyword: no qualifier is one.
        let start = parts
            .iter()
            .position(|p| KEYWORDS.contains(p))
            .ok_or_else(|| LocatorParseError::NoAddress(s.to_string()))?;
        let address: Address = parts[start..].join(SEPARATOR).parse()?;
        Self::assemble(s, &parts[..start], address)
    }
}

impl Serialize for Locator {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Locator {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(s: &str) -> Locator {
        s.parse().unwrap()
    }

    #[test]
    fn round_trips_every_form() {
        for s in [
            "/acme/north-desk::ledger::senator::codex",
            "acme/north-desk::ledger::senator::codex",
            "/acme::ledger::speaker",
            "north-desk::ledger::senator::codex",
            "ledger::senator::codex",
            "ledger::speaker",
            "ledger::committee",
            "senator::codex",
            "senator::*",
            "speaker",
            "ledger::guest::chat/u_1",
            "acme::ledger::guest::web/a::b",
        ] {
            let l = loc(s);
            assert_eq!(l.to_string(), s);
            assert_eq!(l.in_text(), format!("[{s}]"));
        }
    }

    #[test]
    fn read_from_the_right() {
        let l = loc("acme/north-desk::ledger::senator::codex");
        assert_eq!(l.senate().unwrap().to_string(), "acme/north-desk");
        assert_eq!(l.committee().unwrap().as_str(), "ledger");
        assert_eq!(l.address().to_string(), "senator::codex");
        assert!(l.is_complete());

        let l = loc("ledger::speaker");
        assert_eq!(l.senate(), None);
        assert_eq!(l.committee().unwrap().as_str(), "ledger");
        assert_eq!(l.address(), &Address::Speaker);
        assert!(!l.is_complete() && !l.is_here());

        assert!(loc("senator::codex").is_here());
        // A guest id may hold `::`; the address starts at the first keyword.
        assert_eq!(
            loc("acme::ledger::guest::web/a::b").address().to_string(),
            "guest::web/a::b"
        );
    }

    #[test]
    fn refuses_what_cannot_be_read() {
        for s in [
            "",
            "codex",
            "ledger::codex",
            "a::b::c::senator::codex",
            "speaker::ledger::senator::codex",
            "acme::speaker::senator::codex",
            "Ledger::senator::codex",
            "ledger::senator::wendell/claude",
            "ledger::",
            "::speaker",
            "north-desk::ledger::speaker::x",
            "/acme::senator::codex",
            "acme/north-desk::senator::codex",
            "ledger::chair::x",
        ] {
            assert!(s.parse::<Locator>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn one_name_with_a_slash_is_a_senate_and_needs_a_committee() {
        let err = "/acme::senator::wendell".parse::<Locator>().unwrap_err();
        assert!(matches!(err, LocatorParseError::NoCommittee(_)), "{err}");
        assert!(
            err.to_string().contains("/<senate>::senator::<name>"),
            "{err}"
        );
        // One plain name is a committee.
        assert_eq!(
            loc("acme::senator::wendell").committee().unwrap().as_str(),
            "acme"
        );
        let absolute = loc("/acme/north-desk::ledger::senator::codex");
        assert!(absolute.senate().unwrap().is_absolute());
        assert_eq!(
            absolute.in_text(),
            "[/acme/north-desk::ledger::senator::codex]"
        );
    }

    #[test]
    fn the_short_form_reads_a_bare_name_as_a_senator() {
        for (short, full) in [
            ("codex", "senator::codex"),
            ("*", "senator::*"),
            ("ledger::codex", "ledger::senator::codex"),
            ("ledger::*", "ledger::senator::*"),
            (
                "acme/north-desk::ledger::claude",
                "acme/north-desk::ledger::senator::claude",
            ),
            ("ledger::speaker", "ledger::speaker"),
            ("speaker", "speaker"),
            (" senator::codex ", "senator::codex"),
        ] {
            assert_eq!(Locator::parse_short(short).unwrap(), loc(full), "{short}");
        }
        for bad in ["a::b::c::codex", "Codex", "wendell/claude", ""] {
            assert!(Locator::parse_short(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn missing_parts_come_from_where_the_writer_stands() {
        let here: SenatePath = "acme/north-desk".parse().unwrap();
        let committee = CommitteeId::new("refund").unwrap();
        assert_eq!(
            loc("senator::codex").completed(&here, &committee),
            loc("acme/north-desk::refund::senator::codex")
        );
        assert_eq!(
            loc("ledger::speaker").completed(&here, &committee),
            loc("acme/north-desk::ledger::speaker")
        );
        assert_eq!(
            loc("acme::ledger::speaker").completed(&here, &committee),
            loc("acme::ledger::speaker")
        );
    }

    #[test]
    fn only_an_unqualified_locator_is_an_address_here() {
        assert_eq!(
            loc("senator::codex").as_address(),
            Some("senator::codex".parse().unwrap())
        );
        assert_eq!(loc("ledger::senator::codex").as_address(), None);
        // A child Senate's Speaker, named relative to the parent's log.
        assert_eq!(
            loc("north-desk::ledger::speaker").as_address(),
            Some("north-desk::ledger::speaker".parse().unwrap())
        );
        assert_eq!(loc("/north-desk::ledger::speaker").as_address(), None);
        assert_eq!(loc("ledger::chair").address(), &Address::Chair);
        assert_eq!(
            Locator::from(Address::Speaker),
            Locator::here(Address::Speaker)
        );
    }

    #[test]
    fn serializes_as_a_string() {
        let l = loc("ledger::senator::codex");
        assert_eq!(
            serde_json::to_value(&l).unwrap(),
            serde_json::json!("ledger::senator::codex")
        );
        assert_eq!(
            serde_json::from_value::<Locator>(serde_json::json!("ledger::senator::codex")).unwrap(),
            l
        );
    }
}
