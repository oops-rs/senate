//! Opaque profile documents. The local bridge authorizes the resolved key
//! before calling this store. Observe has no write route.

use chrono::{DateTime, Utc};
use senate_core::{CommitteeId, SenatorName};
use senate_observe_protocol::Card;
use serde::{Deserialize, Serialize, de};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::grants::{GrantError, lock};

const MAX_INGRESS: usize = 4 * 1024;
const MAX_CARD: usize = 2 * 1024;
const MAX_ENTRIES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CardKey {
    Committee {
        committee_id: CommitteeId,
    },
    Seat {
        committee_id: CommitteeId,
        senator_id: SenatorName,
    },
    Person {
        senator_id: SenatorName,
    },
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    key: CardKey,
    revision: String,
    updated_at: DateTime<Utc>,
    value: Option<Map<String, Value>>,
}

#[derive(Default)]
struct State {
    cards: BTreeMap<CardKey, Entry>,
    by_subject: BTreeMap<CardKey, VecDeque<Instant>>,
    by_writer: BTreeMap<SenatorName, VecDeque<Instant>>,
    global: VecDeque<Instant>,
}

pub(crate) struct Cards {
    path: PathBuf,
    state: Mutex<State>,
}

impl Cards {
    pub(crate) fn open(directory: &Path) -> Result<Self, GrantError> {
        let path = directory.join("observe-cards.json");
        let entries: Vec<Entry> = match File::open(&path) {
            Ok(file) => serde_json::from_reader(file)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => vec![],
            Err(error) => return Err(error.into()),
        };
        if entries.len() > MAX_ENTRIES {
            return Err(GrantError::Invalid("too many stored cards"));
        }
        Ok(Self {
            path,
            state: Mutex::new(State {
                cards: entries
                    .into_iter()
                    .map(|entry| (entry.key.clone(), entry))
                    .collect(),
                ..State::default()
            }),
        })
    }

    pub(crate) fn get(&self, key: &CardKey) -> Option<Card> {
        let state = lock(&self.state);
        let entry = state.cards.get(key)?;
        Some(Card {
            revision: entry.revision.clone(),
            updated_at: entry.updated_at,
            value: entry.value.clone()?,
        })
    }

    pub(crate) fn set(
        &self,
        key: CardKey,
        writer: &SenatorName,
        input: &str,
    ) -> Result<Option<String>, GrantError> {
        let value = parse(input)?;
        let mut locked = lock(&self.state);
        let state = &mut *locked;
        if state
            .cards
            .get(&key)
            .is_some_and(|prior| prior.value == value)
            || (value.is_none() && !state.cards.contains_key(&key))
        {
            return Ok(None);
        }
        if state.cards.len() >= MAX_ENTRIES && !state.cards.contains_key(&key) {
            return Err(GrantError::Invalid("card store limit reached"));
        }
        let now = Instant::now();
        let minute = Duration::from_secs(60);
        state.by_writer.retain(|_, times| {
            trim(times, now, Duration::from_secs(2));
            !times.is_empty()
        });
        state.by_subject.retain(|_, times| {
            trim(times, now, minute);
            !times.is_empty()
        });
        trim(&mut state.global, now, minute);
        let by_subject = state.by_subject.entry(key.clone()).or_default();
        trim(by_subject, now, minute);
        let by_writer = state.by_writer.entry(writer.clone()).or_default();
        if by_subject.len() >= 4 || by_writer.len() >= 2 || state.global.len() >= 120 {
            return Err(GrantError::Invalid("card update limit reached"));
        }
        let revision = String::from(crate::journal::new_cursor().map_err(|_| GrantError::Random)?);
        let entry = Entry {
            key,
            revision: revision.clone(),
            updated_at: Utc::now(),
            value,
        };
        self.save(
            state
                .cards
                .values()
                .filter(|old| old.key != entry.key)
                .chain([&entry]),
        )?;
        by_subject.push_back(now);
        by_writer.push_back(now);
        state.global.push_back(now);
        state.cards.insert(entry.key.clone(), entry);
        Ok(Some(revision))
    }

    fn save<'a>(&self, entries: impl Iterator<Item = &'a Entry>) -> Result<(), GrantError> {
        let directory = self
            .path
            .parent()
            .ok_or(GrantError::Invalid("missing card directory"))?;
        let mut temp = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(&mut temp, &entries.collect::<Vec<_>>())?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn trim(times: &mut VecDeque<Instant>, now: Instant, window: Duration) {
    while times
        .front()
        .is_some_and(|at| now.duration_since(*at) >= window)
    {
        times.pop_front();
    }
}

fn parse(input: &str) -> Result<Option<Map<String, Value>>, GrantError> {
    if input.len() > MAX_INGRESS {
        return Err(GrantError::Invalid("card input is larger than 4 KiB"));
    }
    let UniqueValue(value) = serde_json::from_str(input)
        .map_err(|_| GrantError::Invalid("expected JSON without duplicate keys"))?;
    if serde_json::to_vec(&value)?.len() > MAX_CARD {
        return Err(GrantError::Invalid("compact card exceeds 2 KiB"));
    }
    let mut nodes = 0;
    check_tree(&value, 1, &mut nodes)?;
    match value {
        Value::Object(object) => Ok(Some(object)),
        Value::Null => Ok(None),
        _ => Err(GrantError::Invalid("a card must be a JSON object or null")),
    }
}

fn check_tree(value: &Value, depth: usize, nodes: &mut usize) -> Result<(), GrantError> {
    if depth > 8 {
        return Err(GrantError::Invalid("card exceeds maximum depth eight"));
    }
    match value {
        Value::Object(object) => {
            *nodes += object.len();
            for value in object.values() {
                check_tree(value, depth + 1, nodes)?;
            }
        }
        Value::Array(array) => {
            for value in array {
                check_tree(value, depth + 1, nodes)?;
            }
        }
        _ => *nodes += 1,
    }
    if *nodes > 128 {
        return Err(GrantError::Invalid(
            "card exceeds 128 keys and scalar values",
        ));
    }
    Ok(())
}

struct UniqueValue(Value);

impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueVisitor)
    }
}

struct UniqueVisitor;

impl<'de> de::Visitor<'de> for UniqueVisitor {
    type Value = UniqueValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::from(value)))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::from(value)))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::from(value)))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::from(value)))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::from(value)))
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(UniqueValue(Value::Null))
    }
    fn visit_seq<A: de::SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::new();
        while let Some(UniqueValue(value)) = access.next_element()? {
            values.push(value);
        }
        Ok(UniqueValue(Value::Array(values)))
    }
    fn visit_map<A: de::MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let mut values = Map::new();
        while let Some((key, UniqueValue(value))) = access.next_entry()? {
            if values.insert(key, value).is_some() {
                return Err(de::Error::custom("duplicate card object key"));
            }
        }
        Ok(UniqueValue(Value::Object(values)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_recursive_object_limits_and_duplicate_keys() {
        for bad in [
            "[]",
            "true",
            r#"{"t":{"key":1,"key":2}}"#,
            r#"{"a":[[[[[[[[1]]]]]]]]}"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        assert!(parse(&format!(r#"{{"large":"{}"}}"#, "x".repeat(2048))).is_err());
        assert!(matches!(parse("null"), Ok(None)));
        assert!(matches!(parse(r#"{"town":{"colour":2}}"#), Ok(Some(_))));
    }

    #[test]
    fn identical_card_writes_do_not_consume_the_rate_budget()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let cards = Cards::open(dir.path())?;
        let writer = "card-writer".parse()?;
        let key = CardKey::Person {
            senator_id: "card-writer".parse()?,
        };
        let first = cards.set(key.clone(), &writer, r#"{"town":{}}"#)?;
        assert!(first.is_some());
        assert!(cards.set(key.clone(), &writer, r#"{"town":{}}"#)?.is_none());
        assert!(cards.set(key.clone(), &writer, "null")?.is_some());
        assert!(cards.get(&key).is_none());
        assert!(Cards::open(dir.path())?.get(&key).is_none());
        Ok(())
    }
}
