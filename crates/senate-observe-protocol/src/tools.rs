use std::fmt;
use std::str::FromStr;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ToolKind(String);

impl ToolKind {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ToolKind {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let known = matches!(
            value,
            "command" | "edit" | "read" | "search" | "web" | "other"
        );
        let alias = value.strip_prefix("mcp:").is_some_and(|alias| {
            !alias.is_empty()
                && alias.len() <= 63
                && alias.as_bytes()[0].is_ascii_alphanumeric()
                && alias
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        });
        if known || alias {
            Ok(Self(value.into()))
        } else {
            Err("invalid public tool kind")
        }
    }
}

impl TryFrom<String> for ToolKind {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Ok(value.parse().unwrap_or_else(|_| Self("other".into())))
    }
}

impl From<ToolKind> for String {
    fn from(value: ToolKind) -> Self {
        value.0
    }
}

impl fmt::Display for ToolKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl JsonSchema for ToolKind {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ToolKind".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "oneOf": [
                {"type": "string", "enum": ["command", "edit", "read", "search", "web", "other"]},
                {"type": "string", "pattern": "^mcp:[a-z0-9][a-z0-9-]{0,62}$"}
            ]
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, JsonSchema, Serialize, Deserialize)]
pub struct ToolActivity {
    pub senator_id: String,
    pub session_id: String,
    pub call_id: String,
    pub tool_kind: ToolKind,
}
