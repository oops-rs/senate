//! The language a text is rendered in.
//!
//! Chinese and English are both first-class. Every function in this crate
//! that produces text for people takes a [`Locale`] and writes that language
//! properly; none of them falls back to the other.
//!
//! Three things never depend on a locale:
//!
//! - **What is stored.** An event carries fields (who spoke for whom,
//!   takeover, state); the words are chosen when it is shown.
//! - **Identifiers.** Addresses, kinds, states on the wire, `seq`, branch
//!   names and tool names read the same everywhere, and an address in text
//!   is written as a bracketed mention ([`crate::mention`]).
//! - **Diagnostics.** Error messages (`Display` of the error types here, the
//!   `check` functions' refusals) are for whoever is debugging a client, and
//!   are English.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Locale {
    /// Chinese, written `zh`.
    Zh,
    /// English, written `en`. The default: the language a deployment gets
    /// when its configuration names none.
    #[default]
    En,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown locale: {0:?} (expected zh or en, optionally with a region: zh-CN, en-US)")]
pub struct LocaleParseError(pub String);

impl Locale {
    pub const ALL: [Locale; 2] = [Locale::Zh, Locale::En];

    /// The short tag: `zh` or `en`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    /// The one of two texts that is in this language.
    pub fn pick<T>(self, zh: T, en: T) -> T {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Reads a language tag by its language part alone, whatever the case and
/// whichever separator follows: `zh`, `zh-CN`, `zh_CN`, `zh-Hans`,
/// `zh_CN.UTF-8`, `en`, `en-US`, `EN_gb`. Any other language is an error.
impl FromStr for Locale {
    type Err = LocaleParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let language = s
            .trim()
            .split(['-', '_', '.'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        match language.as_str() {
            "zh" => Ok(Self::Zh),
            "en" => Ok(Self::En),
            _ => Err(LocaleParseError(s.to_string())),
        }
    }
}

impl Serialize for Locale {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Decoding accepts whatever [`FromStr`] does and refuses the rest. A locale
/// is configuration, and a mistyped one should be an error where it is
/// written, not a silent switch to the default.
impl<'de> Deserialize<'de> for Locale {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_language_tag_by_its_language() {
        for s in [
            "zh",
            "ZH",
            "zh-CN",
            "zh_CN",
            "zh-Hans",
            "zh-Hans-CN",
            "zh_CN.UTF-8",
            " zh-TW ",
        ] {
            assert_eq!(s.parse::<Locale>(), Ok(Locale::Zh), "{s:?}");
        }
        for s in ["en", "EN", "en-US", "en_GB", "en_US.UTF-8"] {
            assert_eq!(s.parse::<Locale>(), Ok(Locale::En), "{s:?}");
        }
        for s in ["", "fr", "english", "中文", "z", "-zh", "C", "POSIX"] {
            assert!(s.parse::<Locale>().is_err(), "{s:?}");
        }
    }

    #[test]
    fn is_written_as_a_short_string() {
        assert_eq!(serde_json::to_value(Locale::Zh).unwrap(), "zh");
        assert_eq!(serde_json::to_value(Locale::En).unwrap(), "en");
        assert_eq!(Locale::Zh.to_string(), "zh");
        let zh: Locale = serde_json::from_value(serde_json::json!("zh-CN")).unwrap();
        assert_eq!(zh, Locale::Zh);
        assert!(serde_json::from_value::<Locale>(serde_json::json!("fr")).is_err());
    }

    #[test]
    fn defaults_to_english_and_picks_its_own_text() {
        assert_eq!(Locale::default(), Locale::En);
        assert_eq!(Locale::Zh.pick("在线", "online"), "在线");
        assert_eq!(Locale::En.pick("在线", "online"), "online");
        assert_eq!(Locale::ALL.len(), 2);
    }
}
