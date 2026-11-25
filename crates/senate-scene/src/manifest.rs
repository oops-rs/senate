//! `manifest.json`, read strictly: exactly the contract's fields, each of
//! its type and within bounds, no key twice at any level, nothing else.

use std::collections::HashSet;
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};

use crate::{HOST_VERSION, MAX_MANIFEST, OBSERVE_MAJOR, Problem, valid_relative};

/// A scene's manifest. Its `name` is a label only: a scene is known by the
/// identity its host gives it, never by what it calls itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    /// A normalised bundle-relative path to an `.html` file.
    pub entry: String,
    pub observe_major: u32,
    pub required: Vec<String>,
    pub optional: Vec<String>,
    pub card_reads: Vec<String>,
    pub card_writes: Vec<String>,
    pub min_host_version: u32,
}

impl Manifest {
    /// Every Observe capability it names, required first.
    pub fn capabilities(&self) -> impl Iterator<Item = &str> {
        self.required
            .iter()
            .chain(&self.optional)
            .map(String::as_str)
    }

    /// It asks to see the Senate's direct links (a separate permission).
    pub fn wants_topology(&self) -> bool {
        self.capabilities().any(|c| c == "topology")
    }

    pub fn parse(bytes: &[u8]) -> Result<Self, Problem> {
        if bytes.len() > MAX_MANIFEST {
            return Err(Problem::ManifestTooLarge);
        }
        let mut de = serde_json::Deserializer::from_slice(bytes);
        match Unique.deserialize(&mut de).and_then(|()| de.end()) {
            Ok(()) => {}
            Err(e) => {
                let text = e.to_string();
                return Err(match text.strip_prefix(DUPLICATE) {
                    Some(rest) => Problem::DuplicateKey(
                        rest.split(" at line")
                            .next()
                            .unwrap_or(rest)
                            .chars()
                            .take(64)
                            .collect(),
                    ),
                    None => Problem::NotJson,
                });
            }
        }
        let Ok(Value::Object(root)) = serde_json::from_slice::<Value>(bytes) else {
            return Err(Problem::NotJson);
        };
        exactly(
            &root,
            &[
                "name",
                "version",
                "entry",
                "observe",
                "cards",
                "min_host_version",
            ],
            "",
        )?;

        let name = string(&root, "name")
            .filter(|n| valid_name(n))
            .ok_or_else(|| invalid("name"))?;
        let version = string(&root, "version")
            .filter(|v| valid_version(v))
            .ok_or_else(|| invalid("version"))?;
        let entry = string(&root, "entry")
            .filter(|e| {
                e.len() <= 256 && valid_relative(e) && e.to_ascii_lowercase().ends_with(".html")
            })
            .ok_or_else(|| invalid("entry"))?;

        let Some(Value::Object(observe)) = root.get("observe") else {
            return Err(invalid("observe"));
        };
        exactly(observe, &["major", "required", "optional"], "observe.")?;
        let major = integer(observe.get("major"))
            .filter(|m| *m == u64::from(OBSERVE_MAJOR))
            .ok_or_else(|| invalid("observe.major"))?;
        let required = list(
            observe.get("required"),
            "observe.required",
            32,
            valid_capability,
        )?;
        let optional = list(
            observe.get("optional"),
            "observe.optional",
            32,
            valid_capability,
        )?;
        if required.iter().any(|r| optional.contains(r)) {
            return Err(invalid("observe.optional"));
        }

        let Some(Value::Object(cards)) = root.get("cards") else {
            return Err(invalid("cards"));
        };
        exactly(cards, &["reads", "writes"], "cards.")?;
        let card_reads = list(cards.get("reads"), "cards.reads", 16, valid_schema)?;
        let card_writes = list(cards.get("writes"), "cards.writes", 16, valid_schema)?;

        let min_host = integer(root.get("min_host_version"))
            .filter(|m| *m >= 1)
            .ok_or_else(|| invalid("min_host_version"))?;
        if min_host > u64::from(HOST_VERSION) {
            return Err(Problem::NeedsNewerHost(
                u32::try_from(min_host).unwrap_or(u32::MAX),
            ));
        }
        Ok(Self {
            name,
            version,
            entry,
            observe_major: u32::try_from(major).unwrap_or(OBSERVE_MAJOR),
            required,
            optional,
            card_reads,
            card_writes,
            min_host_version: u32::try_from(min_host).unwrap_or(HOST_VERSION),
        })
    }
}

fn invalid(field: &str) -> Problem {
    Problem::Invalid(field.to_owned())
}

fn exactly(object: &Map<String, Value>, keys: &[&str], at: &str) -> Result<(), Problem> {
    let mut unknown: Vec<&String> = object
        .keys()
        .filter(|k| !keys.contains(&k.as_str()))
        .collect();
    unknown.sort();
    if let Some(key) = unknown.first() {
        return Err(Problem::UnknownField(format!(
            "{at}{}",
            key.chars().take(64).collect::<String>()
        )));
    }
    let mut missing: Vec<&&str> = keys.iter().filter(|k| !object.contains_key(**k)).collect();
    missing.sort();
    match missing.first() {
        Some(key) => Err(Problem::MissingField(format!("{at}{key}"))),
        None => Ok(()),
    }
}

fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// A JSON integer: not a boolean, not a fraction, below a million.
fn integer(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|n| *n < 1_000_000)
}

fn list(
    value: Option<&Value>,
    field: &str,
    max: usize,
    valid: fn(&str) -> bool,
) -> Result<Vec<String>, Problem> {
    let Some(Value::Array(items)) = value else {
        return Err(invalid(field));
    };
    if items.len() > max {
        return Err(invalid(field));
    }
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        match item.as_str() {
            Some(s) if valid(s) && !out.iter().any(|o| o == s) => out.push(s.to_owned()),
            _ => return Err(invalid(field)),
        }
    }
    Ok(out)
}

/// 1–64 characters, no surrounding space, nothing invisible or
/// direction-changing: no controls, format characters (bidirectional
/// overrides, zero-width joiners, tags), private-use characters,
/// noncharacters or line separators. It is shown in a host's own UI.
pub(crate) fn valid_name(name: &str) -> bool {
    let count = name.chars().count();
    (1..=64).contains(&count) && name.trim() == name && name.chars().all(|c| !hidden(c))
}

fn hidden(c: char) -> bool {
    let u = u32::from(c);
    c.is_control()
        || matches!(u,
            0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x180E
            | 0x200B..=0x200F | 0x2028..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
            | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F
            | 0xE000..=0xF8FF | 0xF0000..=0x10FFFF | 0xFDD0..=0xFDEF)
        || (u & 0xFFFE) == 0xFFFE
}

/// `major.minor.patch` with no leading zeros, each at most six digits, and
/// an optional `-prerelease` of 1–32 `[0-9A-Za-z.-]`; no build metadata.
pub(crate) fn valid_version(v: &str) -> bool {
    let (core, pre) = match v.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (v, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    numbers.len() == 3
        && numbers.iter().all(|n| {
            (1..=6).contains(&n.len())
                && n.bytes().all(|b| b.is_ascii_digit())
                && !(n.len() > 1 && n.starts_with('0'))
        })
        && pre.is_none_or(|p| {
            (1..=32).contains(&p.len())
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
}

/// `[a-z][a-z0-9_]{0,47}`.
pub(crate) fn valid_capability(c: &str) -> bool {
    let b = c.as_bytes();
    (1..=48).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || *x == b'_')
}

/// `town.look/v1`: one to four dotted lowercase words (each
/// `[a-z][a-z0-9-]{0,31}`), a slash, `v` and a version 1–999.
pub(crate) fn valid_schema(s: &str) -> bool {
    let Some((words, version)) = s.split_once('/') else {
        return false;
    };
    let word = |w: &str| {
        let b = w.as_bytes();
        (1..=32).contains(&b.len())
            && b[0].is_ascii_lowercase()
            && b.iter()
                .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || *x == b'-')
    };
    let parts: Vec<&str> = words.split('.').collect();
    let number = version.strip_prefix('v').unwrap_or("");
    (1..=4).contains(&parts.len())
        && parts.iter().all(|w| word(w))
        && (1..=3).contains(&number.len())
        && number.bytes().all(|b| b.is_ascii_digit())
        && !number.starts_with('0')
}

/// The prefix of the error a duplicate key raises (serde_json appends the position).
const DUPLICATE: &str = "duplicate key: ";

/// Well-formed JSON with no object naming a key twice (which a plain parse
/// would quietly resolve, so what the host checked and what the scene meant
/// could differ). Keys are compared decoded, so an escaped spelling is the
/// same key. Nesting is bounded by serde_json's recursion limit.
struct Unique;

impl<'de> DeserializeSeed<'de> for Unique {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Unique {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        while seq.next_element_seed(Unique)?.is_some() {}
        Ok(())
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut seen = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(de::Error::custom(format!("{DUPLICATE}{key}")));
            }
            map.next_value_seed(Unique)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Value {
        json!({
            "name": "Fixture", "version": "1.0.0", "entry": "index.html",
            "observe": {"major": 1, "required": ["state_patch"], "optional": ["app_links"]},
            "cards": {"reads": [], "writes": ["fixture.badge/v1"]}, "min_host_version": 1,
        })
    }

    fn with(changes: &[(&str, Option<Value>)]) -> Vec<u8> {
        let mut v = base();
        let o = v.as_object_mut().expect("object");
        for (k, change) in changes {
            match change {
                Some(x) => {
                    o.insert((*k).to_owned(), x.clone());
                }
                None => {
                    o.remove(*k);
                }
            }
        }
        serde_json::to_vec(&v).expect("json")
    }

    #[test]
    fn accepted() {
        let m = Manifest::parse(&with(&[])).expect("fixture");
        assert_eq!(
            (m.name.as_str(), m.entry.as_str()),
            ("Fixture", "index.html")
        );
        assert!(!m.wants_topology());
        for changes in [
            vec![("version", Some(json!("12.0.3-beta.2")))],
            vec![("entry", Some(json!("app/main/index.html")))],
            vec![("name", Some(json!("小镇 Town ✦")))],
            vec![(
                "observe",
                Some(json!({"major": 1, "required": [], "optional": ["topology"]})),
            )],
            vec![(
                "cards",
                Some(json!({"reads": ["town.look/v1", "a.b.c.d/v999"], "writes": []})),
            )],
        ] {
            assert!(Manifest::parse(&with(&changes)).is_ok(), "{changes:?}");
        }
    }

    #[test]
    fn refused() {
        let caps33: Vec<String> = (0..33).map(|i| format!("c{i}")).collect();
        let schemas17: Vec<String> = (0..17).map(|i| format!("s{i}/v1")).collect();
        let table: Vec<(&str, &str, Option<Value>)> = vec![
            ("name", "missing name", None),
            ("cards", "missing cards", None),
            ("description", "unknown field", Some(json!("x"))),
            ("name", "empty", Some(json!(""))),
            ("name", "long", Some(json!("x".repeat(65)))),
            ("name", "spaced", Some(json!(" Town"))),
            ("name", "control", Some(json!("Town\u{7}"))),
            ("name", "bidi", Some(json!("Town\u{202E}nwoT"))),
            ("name", "tag", Some(json!("Town\u{E0041}"))),
            ("name", "private use", Some(json!("Town\u{E000}"))),
            ("name", "not a string", Some(json!(5))),
            ("version", "short", Some(json!("1.0"))),
            ("version", "leading zero", Some(json!("01.0.0"))),
            ("version", "build", Some(json!("1.0.0+5"))),
            ("entry", "escapes", Some(json!("../index.html"))),
            ("entry", "absolute", Some(json!("/index.html"))),
            ("entry", "not html", Some(json!("index.js"))),
            ("entry", "empty segment", Some(json!("a//index.html"))),
            ("entry", "dot", Some(json!("./index.html"))),
            ("entry", "hidden", Some(json!(".index.html"))),
            ("entry", "backslash", Some(json!("a\\index.html"))),
            ("entry", "escaped", Some(json!("%2e%2e/index.html"))),
            ("entry", "non-ASCII", Some(json!("índex.html"))),
            (
                "observe",
                "major 2",
                Some(json!({"major": 2, "required": [], "optional": []})),
            ),
            (
                "observe",
                "major string",
                Some(json!({"major": "1", "required": [], "optional": []})),
            ),
            (
                "observe",
                "major bool",
                Some(json!({"major": true, "required": [], "optional": []})),
            ),
            (
                "observe",
                "major fraction",
                Some(json!({"major": 1.5, "required": [], "optional": []})),
            ),
            (
                "observe",
                "missing optional",
                Some(json!({"major": 1, "required": []})),
            ),
            (
                "observe",
                "extra",
                Some(json!({"major": 1, "required": [], "optional": [], "scope": "lines"})),
            ),
            (
                "observe",
                "cap number",
                Some(json!({"major": 1, "required": [1], "optional": []})),
            ),
            (
                "observe",
                "cap upper",
                Some(json!({"major": 1, "required": ["State"], "optional": []})),
            ),
            (
                "observe",
                "cap twice",
                Some(json!({"major": 1, "required": ["a", "a"], "optional": []})),
            ),
            (
                "observe",
                "cap both",
                Some(json!({"major": 1, "required": ["a"], "optional": ["a"]})),
            ),
            (
                "observe",
                "caps 33",
                Some(json!({"major": 1, "required": caps33, "optional": []})),
            ),
            (
                "cards",
                "no version",
                Some(json!({"reads": ["town.look"], "writes": []})),
            ),
            (
                "cards",
                "v0",
                Some(json!({"reads": ["town.look/v0"], "writes": []})),
            ),
            (
                "cards",
                "upper",
                Some(json!({"reads": ["Town.look/v1"], "writes": []})),
            ),
            (
                "cards",
                "path",
                Some(json!({"reads": [], "writes": ["a/b/v1"]})),
            ),
            (
                "cards",
                "17",
                Some(json!({"reads": schemas17, "writes": []})),
            ),
            ("min_host_version", "zero", Some(json!(0))),
            ("min_host_version", "bool", Some(json!(true))),
            ("min_host_version", "string", Some(json!("1"))),
        ];
        for (key, why, value) in table {
            assert!(Manifest::parse(&with(&[(key, value)])).is_err(), "{why}");
        }
        assert_eq!(
            Manifest::parse(&with(&[("min_host_version", Some(json!(2)))])),
            Err(Problem::NeedsNewerHost(2))
        );
    }

    #[test]
    fn malformed() {
        let base = String::from_utf8(with(&[])).expect("utf8");
        assert_eq!(
            Manifest::parse(&base.as_bytes()[..base.len() - 1]),
            Err(Problem::NotJson)
        );
        assert_eq!(Manifest::parse(b"[]"), Err(Problem::NotJson));
        assert_eq!(
            Manifest::parse(format!("{base} x").as_bytes()),
            Err(Problem::NotJson)
        );
        assert_eq!(
            Manifest::parse(br#"{"name":"A","name":"B"}"#),
            Err(Problem::DuplicateKey("name".into()))
        );
        assert_eq!(
            Manifest::parse(br#"{"name":"A","name":"B"}"#),
            Err(Problem::DuplicateKey("name".into()))
        );
        let nested = base.replace(r#""major":1"#, r#""major":1,"major":1"#);
        assert_eq!(
            Manifest::parse(nested.as_bytes()),
            Err(Problem::DuplicateKey("major".into()))
        );
        let big = base.replace(
            r#""name":"Fixture""#,
            &format!(r#""name":"Fixture"{}"#, " ".repeat(17_000)),
        );
        assert_eq!(
            Manifest::parse(big.as_bytes()),
            Err(Problem::ManifestTooLarge)
        );
        assert_eq!(
            Manifest::parse(&with(&[("x", Some(json!(1)))])),
            Err(Problem::UnknownField("x".into()))
        );
        assert_eq!(
            Manifest::parse(&with(&[("entry", None)])),
            Err(Problem::MissingField("entry".into()))
        );
    }
}
