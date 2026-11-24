//! Syntax of the app navigation URL. Authorization and destination lookup
//! belong to the app. A parsed link grants neither authority nor visibility.

use std::fmt::{self, Write};
use std::str::FromStr;

use crate::Seq;

const PREFIX: &str = "senate://open/v1/";
// V1 freezes the address keywords and path limit from senate-core.
const KEYWORDS: [&str; 5] = ["senator", "speaker", "chair", "committee", "guest"];
const MAX_SENATE_DEPTH: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppLink {
    target: Target,
    senate: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Committee(String),
    Session(String),
    Thread(String, Seq),
    Senator(String),
    Desk,
    DeskItem(String, Seq),
    Approvals,
    Inbox,
    History,
    HistoryEntry(String),
    Profile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidLink;

impl fmt::Display for InvalidLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid Senate app navigation URL")
    }
}

impl std::error::Error for InvalidLink {}

impl AppLink {
    pub fn target(&self) -> &Target {
        &self.target
    }

    pub fn senate(&self) -> Option<&str> {
        self.senate.as_deref()
    }

    /// The canonical link for a route written as its decoded values, such
    /// as `committee/senate`, `session/senate::agent` or `inbox`: each
    /// slash-separated part is encoded, then the whole is parsed by the one
    /// grammar. A value cannot hold a slash, and nothing here adds a Senate.
    pub fn from_route(route: &str) -> Result<Self, InvalidLink> {
        let encoded = route.split('/').map(encoded).collect::<Vec<_>>().join("/");
        format!("{PREFIX}{encoded}").parse()
    }

    pub fn with_target(self, target: Target) -> Result<Self, InvalidLink> {
        Self {
            target,
            senate: self.senate,
        }
        .to_string()
        .parse()
    }

    pub fn for_senate(target: Target, senate_id: &str) -> Result<Self, InvalidLink> {
        Self {
            target,
            senate: Some(format!("id:{senate_id}")),
        }
        .to_string()
        .parse()
    }
}

impl FromStr for AppLink {
    type Err = InvalidLink;

    fn from_str(url: &str) -> Result<Self, Self::Err> {
        if url.len() > 2048 {
            return Err(InvalidLink);
        }
        let rest = url.strip_prefix(PREFIX).ok_or(InvalidLink)?;
        let (route, senate) = match rest.split_once('?') {
            Some((route, query)) => {
                let selector = decode(query.strip_prefix("senate=").ok_or(InvalidLink)?, true)?;
                let valid = if let Some(id) = selector.strip_prefix("id:") {
                    !id.is_empty() && id.len() <= 128 && id.bytes().all(unreserved)
                } else if let Some(path) = selector.strip_prefix('/') {
                    path.split('/').count() <= MAX_SENATE_DEPTH && path.split('/').all(slug)
                } else {
                    false
                };
                if !valid {
                    return Err(InvalidLink);
                }
                (route, Some(selector))
            }
            None => (rest, None),
        };
        let parts = route.split('/').collect::<Vec<_>>();
        let target = match parts.as_slice() {
            ["committee", name] => Target::Committee(read_slug(name)?),
            ["session", selector] => {
                let selector = decode(selector, false)?;
                if !qualified(&selector) && !session_id(&selector) {
                    return Err(InvalidLink);
                }
                Target::Session(selector)
            }
            ["thread", committee, seq] => Target::Thread(read_slug(committee)?, read_seq(seq)?),
            ["senator", name] => {
                let name = decode(name, false)?;
                if !qualified(&name) && !selector_slug(&name) {
                    return Err(InvalidLink);
                }
                Target::Senator(name)
            }
            ["desk"] => Target::Desk,
            ["desk", committee, seq] => Target::DeskItem(read_slug(committee)?, read_seq(seq)?),
            ["approvals"] => Target::Approvals,
            ["inbox"] => Target::Inbox,
            ["history"] => Target::History,
            ["history", id] => {
                let id = decode(id, false)?;
                if !session_id(&id) {
                    return Err(InvalidLink);
                }
                Target::HistoryEntry(id)
            }
            ["profile"] => Target::Profile,
            _ => return Err(InvalidLink),
        };
        Ok(Self { target, senate })
    }
}

impl fmt::Display for AppLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(PREFIX)?;
        match &self.target {
            Target::Committee(value) => write!(formatter, "committee/{}", encoded(value))?,
            Target::Session(value) => write!(formatter, "session/{}", encoded(value))?,
            Target::Thread(committee, seq) => {
                write!(formatter, "thread/{}/{seq}", encoded(committee))?
            }
            Target::Senator(value) => write!(formatter, "senator/{}", encoded(value))?,
            Target::Desk => formatter.write_str("desk")?,
            Target::DeskItem(committee, seq) => {
                write!(formatter, "desk/{}/{seq}", encoded(committee))?
            }
            Target::Approvals => formatter.write_str("approvals")?,
            Target::Inbox => formatter.write_str("inbox")?,
            Target::History => formatter.write_str("history")?,
            Target::HistoryEntry(value) => write!(formatter, "history/{}", encoded(value))?,
            Target::Profile => formatter.write_str("profile")?,
        }
        if let Some(senate) = &self.senate {
            write!(formatter, "?senate={}", encoded(senate))?;
        }
        Ok(())
    }
}

fn read_slug(value: &str) -> Result<String, InvalidLink> {
    let value = decode(value, false)?;
    if committee_slug(&value) {
        Ok(value)
    } else {
        Err(InvalidLink)
    }
}

fn read_seq(value: &str) -> Result<Seq, InvalidLink> {
    decode(value, false)?.parse().map_err(|_| InvalidLink)
}

fn slug(value: &str) -> bool {
    committee_slug(value) && !KEYWORDS.contains(&value)
}

fn committee_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn qualified(value: &str) -> bool {
    value
        .split_once("::")
        .is_some_and(|(committee, alias)| committee_slug(committee) && selector_slug(alias))
}

fn selector_slug(value: &str) -> bool {
    if value.starts_with("obs-") {
        observer_selector(value)
    } else {
        slug(value)
    }
}

fn observer_selector(value: &str) -> bool {
    value
        .strip_prefix("obs-")
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(hint, digest)| {
            hint.len() == 12
                && digest.len() == 32
                && hint
                    .bytes()
                    .chain(digest.bytes())
                    .all(|digit| digit.is_ascii_digit() || matches!(digit, b'a'..=b'f'))
        })
}

fn session_id(value: &str) -> bool {
    if value.starts_with("obs-") {
        return observer_selector(value);
    }
    let id = value
        .strip_prefix("agy:")
        .or_else(|| value.strip_prefix("acp:"))
        .unwrap_or(value);
    !id.is_empty() && value.len() <= 256 && id.bytes().all(unreserved)
}

fn unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

fn decode(text: &str, allow_slash: bool) -> Result<String, InvalidLink> {
    let mut bytes = text.bytes();
    let mut decoded = Vec::with_capacity(text.len());
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = digit(bytes.next())?;
            let low = digit(bytes.next())?;
            decoded.push((high << 4) | low);
        } else if unreserved(byte) {
            decoded.push(byte);
        } else {
            return Err(InvalidLink);
        }
    }
    let decoded = String::from_utf8(decoded).map_err(|_| InvalidLink)?;
    if decoded.is_empty()
        || matches!(decoded.as_str(), "." | "..")
        || decoded
            .chars()
            .any(|c| c.is_control() || c == '%' || c == '\\' || (!allow_slash && c == '/'))
    {
        return Err(InvalidLink);
    }
    Ok(decoded)
}

fn digit(byte: Option<u8>) -> Result<u8, InvalidLink> {
    byte.and_then(|byte| char::from(byte).to_digit(16))
        .map(|digit| digit as u8)
        .ok_or(InvalidLink)
}

fn encoded(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if unreserved(byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
