//! Ids in running text.
//!
//! In an event's fields an address stands alone. Inside a sentence it needs
//! edges, because not every language puts spaces between words:
//! `请codex看一下` cannot be split, `请[senator::codex]看一下` can. So in
//! text an id is always written in square brackets:
//!
//! - `[senator::codex]`: a senator of this Senate.
//! - `[speaker]`: the Speaker.
//! - `[speaker::codex]`: the Speaker speaking on that senator's behalf. This
//!   is a label for who said something, not somewhere to send a reply; the
//!   reply goes to the senator.
//! - `[committee]`: everyone on the committee.
//! - `[senator::*]`: whichever agent should get it.
//! - `[guest::chat/u_8f3a]`: someone who is not a senator.
//! - `[ibot::senator::codex]`, `[acme/north-desk::ibot::speaker]`: a
//!   qualified name ([`Locator`]), naming a seat or a Speaker elsewhere.
//!
//! The words are the same in every language. Senate, senator and Speaker are
//! not translated, so a label reads the same in a Chinese and an English
//! message and can be copied back into a tool call as it is.

use std::fmt;
use std::ops::Range;
use std::str::FromStr;

use crate::address::{
    ANY_AGENT, Address, COMMITTEE, GUEST_PREFIX, Guest, GuestError, SENATOR_PREFIX, SPEAKER,
};
use crate::locator::{Locator, LocatorParseError};
use crate::senator::{SenatorName, SenatorNameError};

const SPEAKER_FOR_PREFIX: &str = "speaker::";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Mention {
    Senator(SenatorName),
    /// Whichever agent should get it.
    AnyAgent,
    Speaker,
    /// The Speaker answering for a senator.
    SpeakerFor(SenatorName),
    Committee,
    Guest(Guest),
    /// A qualified name: a seat or a Speaker named with its committee, or
    /// its Senate and committee.
    Locator(Locator),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MentionParseError {
    #[error("a mention is written in square brackets: {0:?}")]
    NoBrackets(String),
    #[error(
        "unknown mention: {0:?} (expected [senator::<name>], [senator::*], [speaker], [speaker::<name>], [committee], [guest::<platform>/<id>], or a qualified name such as [ibot::senator::<name>])"
    )]
    Unknown(String),
    #[error(transparent)]
    Senator(#[from] SenatorNameError),
    #[error(transparent)]
    Guest(#[from] GuestError),
    #[error(transparent)]
    Locator(#[from] LocatorParseError),
}

/// A mention found in a text, with the bytes it covers, brackets included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub range: Range<usize>,
    pub mention: Mention,
}

impl Mention {
    /// The label for who wrote an event: its `from`, or the Speaker speaking
    /// for `on_behalf_of`. `None` for an address form this build does not
    /// know, and for one party other than the Speaker speaking for another,
    /// which has no label of its own.
    pub fn author(from: &Address, on_behalf_of: Option<&Address>) -> Option<Self> {
        match (from, on_behalf_of) {
            (Address::Speaker, Some(Address::Senator(name))) => {
                Some(Self::SpeakerFor(name.clone()))
            }
            (_, Some(_)) => None,
            (from, None) => Self::of(from),
        }
    }

    /// The mention naming `address`.
    pub fn of(address: &Address) -> Option<Self> {
        match address {
            Address::Senator(name) => Some(Self::Senator(name.clone())),
            Address::AnyAgent => Some(Self::AnyAgent),
            Address::Speaker => Some(Self::Speaker),
            Address::Committee => Some(Self::Committee),
            Address::Guest(guest) => Some(Self::Guest(guest.clone())),
            Address::Other(_) => None,
        }
    }

    /// Where a reply to this mention goes, as an address of this log. The
    /// Speaker answering for a senator is reached at that senator.
    /// `[senator::*]` gives the wildcard address, which is resolved against
    /// the roster before anything is stored. A qualified name that is not
    /// an address here (another committee, a Senate other than a child) is
    /// `None`: see [`Mention::locator`].
    pub fn address(&self) -> Option<Address> {
        Some(match self {
            Self::Senator(name) | Self::SpeakerFor(name) => Address::Senator(name.clone()),
            Self::AnyAgent => Address::AnyAgent,
            Self::Speaker => Address::Speaker,
            Self::Committee => Address::Committee,
            Self::Guest(guest) => Address::Guest(guest.clone()),
            Self::Locator(locator) => return locator.as_address(),
        })
    }

    /// Where a reply to this mention goes, qualified or not.
    pub fn locator(&self) -> Locator {
        match self {
            Self::Locator(locator) => locator.clone(),
            other => Locator::here(
                other
                    .address()
                    .unwrap_or_else(|| unreachable!("only a locator lacks an address")),
            ),
        }
    }

    /// Every mention in `text`, in order. Brackets that hold anything else
    /// (`[1]`, `[draft]`, a Markdown link's text) are left alone.
    pub fn scan(text: &str) -> Vec<Found> {
        let mut found = Vec::new();
        let mut open = None;
        for (i, c) in text.char_indices() {
            match c {
                '[' => open = Some(i),
                ']' => {
                    if let Some(start) = open.take()
                        && let Ok(mention) = Self::parse_inner(&text[start + 1..i])
                    {
                        found.push(Found {
                            range: start..i + 1,
                            mention,
                        });
                    }
                }
                _ => {}
            }
        }
        found
    }

    fn parse_inner(inner: &str) -> Result<Self, MentionParseError> {
        match inner {
            SPEAKER => Ok(Self::Speaker),
            COMMITTEE => Ok(Self::Committee),
            _ => {
                if let Some(rest) = inner.strip_prefix(SENATOR_PREFIX) {
                    if rest == ANY_AGENT {
                        Ok(Self::AnyAgent)
                    } else {
                        Ok(Self::Senator(rest.parse()?))
                    }
                } else if let Some(name) = inner.strip_prefix(SPEAKER_FOR_PREFIX) {
                    Ok(Self::SpeakerFor(name.parse()?))
                } else if let Some(guest) = inner.strip_prefix(GUEST_PREFIX) {
                    Ok(Self::Guest(guest.parse()?))
                } else if inner.contains("::") {
                    let locator: Locator = inner.parse()?;
                    if locator.is_here() {
                        return Err(MentionParseError::Unknown(inner.to_string()));
                    }
                    Ok(Self::Locator(locator))
                } else {
                    Err(MentionParseError::Unknown(inner.to_string()))
                }
            }
        }
    }
}

impl fmt::Display for Mention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Senator(name) => write!(f, "[{SENATOR_PREFIX}{name}]"),
            Self::AnyAgent => write!(f, "[{SENATOR_PREFIX}{ANY_AGENT}]"),
            Self::Guest(guest) => write!(f, "[{GUEST_PREFIX}{guest}]"),
            Self::Speaker => write!(f, "[{SPEAKER}]"),
            Self::SpeakerFor(name) => write!(f, "[{SPEAKER_FOR_PREFIX}{name}]"),
            Self::Committee => write!(f, "[{COMMITTEE}]"),
            Self::Locator(locator) => write!(f, "[{locator}]"),
        }
    }
}

impl FromStr for Mention {
    type Err = MentionParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let inner = s
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .ok_or_else(|| MentionParseError::NoBrackets(s.to_string()))?;
        Self::parse_inner(inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> SenatorName {
        s.parse().unwrap()
    }

    #[test]
    fn round_trips() {
        for s in [
            "[senator::wendell]",
            "[senator::codex]",
            "[speaker]",
            "[speaker::codex]",
            "[committee]",
            "[senator::*]",
            "[guest::chat/u_8f3a]",
            "[ibot::senator::codex]",
            "[acme/north-desk::ibot::speaker]",
        ] {
            let m: Mention = s.parse().unwrap();
            assert_eq!(m.to_string(), s);
            // A mention and the address it names are written the same way.
            if !s.starts_with("[speaker::") {
                assert_eq!(m.locator().in_text(), s);
            }
            if let Some(address) = m.address()
                && !s.starts_with("[speaker::")
            {
                assert_eq!(address.in_text(), s);
                assert_eq!(Mention::of(&address), Some(m));
            }
        }
    }

    #[test]
    fn rejects_other_forms() {
        for s in [
            "senator::wendell",
            "<senator::wendell>",
            "[senator:wendell]",
            "[senator::]",
            "[speaker::]",
            "[ speaker ]",
            "[议长]",
            "[元老::wendell]",
            "[senator::wendell/claude]",
            "[senator::wendell/*]",
            "[speaker::wendell/claude]",
            "[guest::chat]",
            "[guest::chat/a b]",
            "[ibot::codex]",
        ] {
            assert!(s.parse::<Mention>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn brackets_give_an_id_edges_where_there_are_no_spaces() {
        let text = "请[senator::claude]看一下，[speaker]已经记下了";
        let found = Mention::scan(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].mention, Mention::Senator(name("claude")));
        assert_eq!(&text[found[0].range.clone()], "[senator::claude]");
        assert_eq!(found[1].mention, Mention::Speaker);
        assert_eq!(&text[found[1].range.clone()], "[speaker]");
    }

    #[test]
    fn scan_finds_whichever_agent_guests_and_qualified_names() {
        let text = "让[senator::*]看看[guest::chat/u_8f3a]问的事，再问[ibot::senator::codex] [guest::chat/a]b]";
        let found = Mention::scan(text);
        assert_eq!(found.len(), 4);
        assert_eq!(found[0].mention, Mention::AnyAgent);
        assert_eq!(
            found[0].mention.address().unwrap().to_string(),
            "senator::*"
        );
        assert_eq!(
            found[1].mention,
            Mention::Guest(Guest::new("chat", "u_8f3a").unwrap())
        );
        assert_eq!(
            found[2].mention.locator().to_string(),
            "ibot::senator::codex"
        );
        // Another committee is not an address of this log.
        assert_eq!(found[2].mention.address(), None);
        // A guest id cannot hold `]`, so the mention ends at the first one.
        assert_eq!(&text[found[3].range.clone()], "[guest::chat/a]");
    }

    #[test]
    fn scan_leaves_other_brackets_alone() {
        let text = "see [1] and [the plan](https://example.com), [[senator::alice] ] []";
        let found = Mention::scan(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mention, Mention::Senator(name("alice")));
        assert_eq!(&text[found[0].range.clone()], "[senator::alice]");
        assert!(Mention::scan("no mentions here ] [").is_empty());
    }

    #[test]
    fn the_speaker_answering_for_a_senator_has_its_own_label() {
        let claude = Address::Senator(name("claude"));
        assert_eq!(
            Mention::author(&Address::Speaker, Some(&claude))
                .unwrap()
                .to_string(),
            "[speaker::claude]"
        );
        assert_eq!(
            Mention::author(&claude, None).unwrap().to_string(),
            "[senator::claude]"
        );
        assert_eq!(
            Mention::author(&Address::Speaker, None)
                .unwrap()
                .to_string(),
            "[speaker]"
        );
        // Only the Speaker speaks for others under a label.
        let alice = Address::Senator(name("alice"));
        assert_eq!(Mention::author(&alice, Some(&claude)), None);
        assert_eq!(Mention::author(&Address::Other("x:y".into()), None), None);
    }

    #[test]
    fn a_reply_to_the_speaker_answering_for_a_senator_goes_to_the_senator() {
        let label: Mention = "[speaker::claude]".parse().unwrap();
        assert_eq!(label.address().unwrap().to_string(), "senator::claude");
        assert_eq!(Mention::Speaker.address(), Some(Address::Speaker));
    }
}
