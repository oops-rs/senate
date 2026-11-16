//! Ids in running text.
//!
//! In an event's fields an address stands alone. Inside a sentence it needs
//! edges, because not every language puts spaces between words:
//! `请wendell/claude看一下` cannot be split, `请[senator::wendell/claude]看一下`
//! can. So in text an id is always written in square brackets:
//!
//! - `[senator::wendell/claude]`: a senator.
//! - `[speaker]`: the Speaker.
//! - `[speaker::wendell/claude]`: the Speaker speaking on that senator's
//!   behalf. This is a label for who said something, not somewhere to send a
//!   reply; the reply goes to the senator.
//! - `[committee]`: everyone on the committee.
//!
//! The words are the same in every language. Senate, senator and Speaker are
//! not translated, so a label reads the same in a Chinese and an English
//! message and can be copied back into a tool call as it is.

use std::fmt;
use std::ops::Range;
use std::str::FromStr;

use crate::address::{Address, COMMITTEE, SENATOR_PREFIX, SPEAKER};
use crate::senator::{SenatorPath, SenatorPathError};

const SPEAKER_FOR_PREFIX: &str = "speaker::";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Mention {
    Senator(SenatorPath),
    Speaker,
    /// The Speaker answering for a senator.
    SpeakerFor(SenatorPath),
    Committee,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MentionParseError {
    #[error("a mention is written in square brackets: {0:?}")]
    NoBrackets(String),
    #[error(
        "unknown mention: {0:?} (expected [senator::<path>], [speaker], [speaker::<path>], or [committee])"
    )]
    Unknown(String),
    #[error(transparent)]
    Senator(#[from] SenatorPathError),
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
            (Address::Speaker, Some(Address::Senator(path))) => {
                Some(Self::SpeakerFor(path.clone()))
            }
            (_, Some(_)) => None,
            (from, None) => Self::of(from),
        }
    }

    /// The mention naming `address`.
    pub fn of(address: &Address) -> Option<Self> {
        match address {
            Address::Senator(path) => Some(Self::Senator(path.clone())),
            Address::Speaker => Some(Self::Speaker),
            Address::Committee => Some(Self::Committee),
            Address::Other(_) => None,
        }
    }

    /// Where a reply to this mention goes. The Speaker answering for a
    /// senator is reached at that senator.
    pub fn address(&self) -> Address {
        match self {
            Self::Senator(path) | Self::SpeakerFor(path) => Address::Senator(path.clone()),
            Self::Speaker => Address::Speaker,
            Self::Committee => Address::Committee,
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
                if let Some(path) = inner.strip_prefix(SENATOR_PREFIX) {
                    Ok(Self::Senator(path.parse()?))
                } else if let Some(path) = inner.strip_prefix(SPEAKER_FOR_PREFIX) {
                    Ok(Self::SpeakerFor(path.parse()?))
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
            Self::Senator(path) => write!(f, "[{SENATOR_PREFIX}{path}]"),
            Self::Speaker => write!(f, "[{SPEAKER}]"),
            Self::SpeakerFor(path) => write!(f, "[{SPEAKER_FOR_PREFIX}{path}]"),
            Self::Committee => write!(f, "[{COMMITTEE}]"),
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

    fn path(s: &str) -> SenatorPath {
        s.parse().unwrap()
    }

    #[test]
    fn round_trips() {
        for s in [
            "[senator::wendell]",
            "[senator::wendell/claude]",
            "[speaker]",
            "[speaker::wendell/claude]",
            "[committee]",
        ] {
            let m: Mention = s.parse().unwrap();
            assert_eq!(m.to_string(), s);
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
        ] {
            assert!(s.parse::<Mention>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn brackets_give_an_id_edges_where_there_are_no_spaces() {
        let text = "请[senator::wendell/claude]看一下，[speaker]已经记下了";
        let found = Mention::scan(text);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].mention, Mention::Senator(path("wendell/claude")));
        assert_eq!(&text[found[0].range.clone()], "[senator::wendell/claude]");
        assert_eq!(found[1].mention, Mention::Speaker);
        assert_eq!(&text[found[1].range.clone()], "[speaker]");
    }

    #[test]
    fn scan_leaves_other_brackets_alone() {
        let text = "see [1] and [the plan](https://example.com), [[senator::alice] ] []";
        let found = Mention::scan(text);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mention, Mention::Senator(path("alice")));
        assert_eq!(&text[found[0].range.clone()], "[senator::alice]");
        assert!(Mention::scan("no mentions here ] [").is_empty());
    }

    #[test]
    fn the_speaker_answering_for_a_senator_has_its_own_label() {
        let claude = Address::Senator(path("wendell/claude"));
        assert_eq!(
            Mention::author(&Address::Speaker, Some(&claude))
                .unwrap()
                .to_string(),
            "[speaker::wendell/claude]"
        );
        assert_eq!(
            Mention::author(&claude, None).unwrap().to_string(),
            "[senator::wendell/claude]"
        );
        assert_eq!(
            Mention::author(&Address::Speaker, None)
                .unwrap()
                .to_string(),
            "[speaker]"
        );
        // Only the Speaker speaks for others under a label.
        let alice = Address::Senator(path("alice"));
        assert_eq!(Mention::author(&alice, Some(&claude)), None);
        assert_eq!(Mention::author(&Address::Other("x:y".into()), None), None);
    }

    #[test]
    fn a_reply_to_the_speaker_answering_for_a_senator_goes_to_the_senator() {
        let label: Mention = "[speaker::wendell/claude]".parse().unwrap();
        assert_eq!(label.address().to_string(), "senator::wendell/claude");
        assert_eq!(Mention::Speaker.address(), Address::Speaker);
    }
}
