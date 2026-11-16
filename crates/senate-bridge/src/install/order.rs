//! Keeping a JSON file's key order across an edit.
//!
//! `install` reads `settings.json` into a [`serde_json::Value`], merges into
//! it, and writes it back. A `Value` object keeps its keys sorted, so the
//! file's own order is recorded separately as a [`KeyOrder`] (read from the
//! same text) and the result is written with [`Ordered`]: every key the file
//! had comes in the file's order, at every depth, and keys the merge added
//! follow them.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::Value;

/// The order of the keys of every object in a JSON document, by position.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyOrder {
    /// This object's keys, as first written.
    keys: Vec<String>,
    /// The order inside each key's value.
    fields: HashMap<String, KeyOrder>,
    /// The order inside each element, when this is an array.
    items: Vec<KeyOrder>,
}

impl KeyOrder {
    /// The order of `text`; empty when it is not JSON.
    pub fn of(text: &str) -> Self {
        serde_json::from_str(text).unwrap_or_default()
    }
}

struct KeyOrderVisitor;

impl<'de> Visitor<'de> for KeyOrderVisitor {
    type Value = KeyOrder;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<KeyOrder, E> {
        Ok(KeyOrder::default())
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<KeyOrder, E> {
        Ok(KeyOrder::default())
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<KeyOrder, E> {
        Ok(KeyOrder::default())
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<KeyOrder, E> {
        Ok(KeyOrder::default())
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<KeyOrder, E> {
        Ok(KeyOrder::default())
    }

    fn visit_unit<E: de::Error>(self) -> Result<KeyOrder, E> {
        Ok(KeyOrder::default())
    }

    fn visit_none<E: de::Error>(self) -> Result<KeyOrder, E> {
        Ok(KeyOrder::default())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<KeyOrder, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element::<KeyOrder>()? {
            items.push(item);
        }
        Ok(KeyOrder {
            items,
            ..KeyOrder::default()
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<KeyOrder, A::Error> {
        let mut order = KeyOrder::default();
        while let Some(key) = map.next_key::<String>()? {
            let inner = map.next_value::<KeyOrder>()?;
            // A repeated key keeps its first place; its last value wins, as
            // in the parsed document.
            if !order.fields.contains_key(&key) {
                order.keys.push(key.clone());
            }
            order.fields.insert(key, inner);
        }
        Ok(order)
    }
}

impl<'de> Deserialize<'de> for KeyOrder {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(KeyOrderVisitor)
    }
}

/// `value`, serialized with its objects' keys in `order` first.
pub struct Ordered<'a> {
    pub value: &'a Value,
    pub order: Option<&'a KeyOrder>,
}

impl Serialize for Ordered<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.value {
            Value::Object(map) => {
                let mut out = serializer.serialize_map(Some(map.len()))?;
                let mut written: HashSet<&str> = HashSet::new();
                if let Some(order) = self.order {
                    for key in &order.keys {
                        if let Some(value) = map.get(key) {
                            written.insert(key);
                            out.serialize_entry(
                                key,
                                &Ordered {
                                    value,
                                    order: order.fields.get(key),
                                },
                            )?;
                        }
                    }
                }
                for (key, value) in map {
                    if !written.contains(key.as_str()) {
                        out.serialize_entry(key, &Ordered { value, order: None })?;
                    }
                }
                out.end()
            }
            Value::Array(items) => {
                let mut out = serializer.serialize_seq(Some(items.len()))?;
                for (i, value) in items.iter().enumerate() {
                    let order = self.order.and_then(|o| o.items.get(i));
                    out.serialize_element(&Ordered { value, order })?;
                }
                out.end()
            }
            other => other.serialize(serializer),
        }
    }
}

/// `value` as pretty JSON with `order` kept.
pub fn to_string_pretty(value: &Value, order: &KeyOrder) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&Ordered {
        value,
        order: Some(order),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_order_survives_and_new_keys_follow() {
        let text = r#"{"zeta": 1, "model": "opus", "alpha": {"b": 1, "a": [{"y": 1, "x": 2}]}}"#;
        let mut value: Value = serde_json::from_str(text).unwrap();
        value["alpha"]["c"] = Value::from(3);
        value["hooks"] = serde_json::json!({"z": 1, "a": 2});
        let out = to_string_pretty(&value, &KeyOrder::of(text)).unwrap();
        let at = |k: &str| out.find(&format!("\"{k}\"")).unwrap();
        assert!(
            at("zeta") < at("model") && at("model") < at("alpha"),
            "{out}"
        );
        assert!(at("b") < at("a") && at("a") < at("c"), "{out}");
        assert!(at("y") < at("x"), "inside arrays too: {out}");
        assert!(at("alpha") < at("hooks"), "{out}");
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), value);
    }

    #[test]
    fn anything_that_is_not_json_has_no_order() {
        assert_eq!(KeyOrder::of("not json"), KeyOrder::default());
        assert_eq!(KeyOrder::of(""), KeyOrder::default());
        let order = KeyOrder::of(r#"{"a": 1, "b": 2, "a": 3}"#);
        assert_eq!(order.keys, ["a", "b"]);
    }
}
