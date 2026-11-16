//! Identifiers. Each is a validated newtype serialized as its plain string or
//! number, so the wire stays readable (`"committee": "refund-v2"`, `"seq": 118`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{kind} must be 1-63 chars of [a-z0-9-], starting with a letter or digit: {value:?}")]
    InvalidSlug { kind: &'static str, value: String },
    #[error("event id must be a ULID: {0:?}")]
    InvalidEventId(String),
}

fn valid_slug(s: &str) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

macro_rules! slug_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if valid_slug(&value) {
                    Ok(Self(value))
                } else {
                    Err(IdError::InvalidSlug { kind: $kind, value })
                }
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = IdError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Self::new(s)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                Self::new(s).map_err(serde::de::Error::custom)
            }
        }
    };
}

slug_id!(
    /// The unit of work, e.g. `refund-v2`: one log, its members, its threads.
    CommitteeId,
    "committee id"
);

slug_id!(
    /// One segment of a [`SenatorPath`](crate::SenatorPath): a person's
    /// handle (`wendell`) or an agent's name under its presider (`claude`).
    Name,
    "senator name"
);

/// Position in one committee's log. Assigned by the committee's single
/// writer, so it is a total order within that committee and starts at 1.
/// `Seq(0)` is the "before everything" cursor.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl Seq {
    pub const ZERO: Seq = Seq(0);

    pub fn next(self) -> Seq {
        Seq(self.0 + 1)
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Seq {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(Seq)
    }
}

/// Writer-chosen idempotency key for one event. A retry of an append carries
/// the same id, and the log answers with the event already stored rather than
/// a second copy.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventId(ulid::Ulid);

impl EventId {
    pub fn new() -> Self {
        Self(ulid::Ulid::new())
    }

    /// A deterministic id: the ULID whose time part is `ts_ms` and whose
    /// random part is a stable hash of `key`. Two writers that derive an id
    /// from the same facts get the same id, so the log stores the event once.
    pub fn derived(ts_ms: u64, key: &str) -> Self {
        // FNV-1a, twice with different offsets: stable across builds and
        // platforms, unlike `std`'s hasher.
        fn fnv(seed: u64, bytes: &[u8]) -> u64 {
            let mut h = seed;
            for b in bytes {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }
        let hi = fnv(0xcbf2_9ce4_8422_2325, key.as_bytes());
        let lo = fnv(0x8422_2325_cbf2_9ce4, key.as_bytes());
        let random = (u128::from(hi) << 64 | u128::from(lo)) & ((1u128 << 80) - 1);
        Self(ulid::Ulid::from_parts(ts_ms & ((1 << 48) - 1), random))
    }
}

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for EventId {
    type Err = IdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ulid::Ulid::from_string(s)
            .map(Self)
            .map_err(|_| IdError::InvalidEventId(s.to_string()))
    }
}

impl Serialize for EventId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for EventId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_validate() {
        assert!(CommitteeId::new("refund-v2").is_ok());
        assert!(CommitteeId::new("").is_err());
        assert!(CommitteeId::new("-x").is_err());
        assert!(CommitteeId::new("Refund").is_err());
        assert!(Name::new("wendell").is_ok());
        assert!(Name::new("a/b").is_err());
        assert!(Name::new("a:b").is_err());
    }

    #[test]
    fn event_id_round_trips() {
        let id = EventId::new();
        let json = serde_json::to_string(&id).unwrap();
        let back: EventId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
        assert!("nope".parse::<EventId>().is_err());
    }

    #[test]
    fn derived_ids_are_stable() {
        assert_eq!(EventId::derived(1, "k"), EventId::derived(1, "k"));
        assert_ne!(EventId::derived(1, "k"), EventId::derived(1, "j"));
    }
}
