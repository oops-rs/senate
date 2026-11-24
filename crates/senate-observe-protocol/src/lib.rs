//! Public Observe protocol. Construct these rows from authorized projections,
//! never by serializing a privileged Senate event or IPC reply.

mod event;
pub mod link;
pub mod recording;
mod state;
mod tools;

pub use event::*;
pub use state::*;
pub use tools::*;

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u8 = 1;
pub const STATE_PATH: &str = "/v1/observe/state";
pub const EVENTS_PATH: &str = "/v1/observe/events";
pub const MAX_STATE_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

/// The literal wire major. A different number is not a v1 document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct V1;

impl Serialize for V1 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u8(SCHEMA_VERSION)
    }
}

impl<'de> Deserialize<'de> for V1 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match u8::deserialize(deserializer)? {
            SCHEMA_VERSION => Ok(Self),
            _ => Err(serde::de::Error::custom(
                "unsupported Observe schema version",
            )),
        }
    }
}

/// A committed, committee-local sequence. The decimal wire encoding avoids
/// precision loss in JSON readers that represent numbers as doubles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub NonZeroU64);

impl FromStr for Seq {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.starts_with('0') || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("expected a canonical nonzero decimal sequence");
        }
        text.parse::<NonZeroU64>()
            .map(Self)
            .map_err(|_| "expected a nonzero u64 sequence")
    }
}

impl fmt::Display for Seq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for Seq {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Seq {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Cursor(String);

impl Cursor {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for Cursor {
    type Err = &'static str;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        if text.len() == 22
            && text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            && matches!(text.as_bytes()[21], b'A' | b'Q' | b'g' | b'w')
        {
            Ok(Self(text.into()))
        } else {
            Err("expected an unpadded base64url Observe cursor")
        }
    }
}

impl TryFrom<String> for Cursor {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Cursor> for String {
    fn from(value: Cursor) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
}

impl JsonSchema for V1 {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "V1".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "integer", "const": SCHEMA_VERSION})
    }
}

impl JsonSchema for Seq {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Seq".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "pattern": "^[1-9][0-9]{0,19}$"})
    }
}

impl JsonSchema for Cursor {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Cursor".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({"type": "string", "pattern": "^[A-Za-z0-9_-]{21}[AQgw]$"})
    }
}
