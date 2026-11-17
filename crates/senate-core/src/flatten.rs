//! Converting a log written when senators were paths.
//!
//! Before senators were flat names, an agent was named under its person
//! (`senator::wendell/claude`), "whichever of a person's agents" was
//! `senator::wendell/*`, and the stores kept the same paths in cursor
//! owners and heartbeats. A log written then is converted once, in place:
//!
//! | Stored | Becomes |
//! | --- | --- |
//! | `senator::wendell` (a person) | unchanged |
//! | `senator::wendell/claude` | `senator::claude` |
//! | `senator::wendell/*` | `senator::*` |
//! | `wendell/claude` where a payload names an agent | `claude` |
//! | anything deeper (`senator::wendell/claude/reviewer`) | refused |
//!
//! It is applied wherever a name is stored: an event's `from`, `to` and
//! `on_behalf_of`; inside `meta`, every string that is an address
//! (`proposed_by`, `mentions[].address`, ...) and every bare path under the
//! keys that hold an agent's name (`agent`, `new_owner`, `candidates`,
//! `target_agent`: presence, picks, work runs); cursor owners
//! (`senator::<path>`); heartbeat agents. Bodies are text and are left as
//! written. Nothing else changes: ids, seqs, times, the epoch.
//!
//! The conversion refuses rather than guesses ([`FlattenError`]): a path
//! deeper than one agent (a Senate nested under an agent, which no longer
//! exists), the same agent name under two people (names are unique in a
//! Senate now), or an agent named like a person of the log.
//!
//! [`Flattener`] is the pure part: first record everything
//! ([`Flattener::see_envelope`] and the others), then [`Flattener::check`],
//! then convert with [`Flattener::envelope`], [`Flattener::address`],
//! [`Flattener::meta`] and [`Flattener::agent`]. The log stores apply it
//! (`senate-log`: the embedded store automatically, Postgres only when
//! asked), and so does a developer machine's `state.db` (`senate-bridge`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

/// The keys of a `meta` payload whose bare string values name an agent.
pub const AGENT_KEYS: [&str; 4] = ["agent", "new_owner", "candidates", "target_agent"];

const SENATOR: &str = "senator::";

/// Where in the log something was found, for a refusal a person can act on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    Event { committee: String, seq: i64 },
    Cursor { committee: String, owner: String },
    Heartbeat { committee: String, agent: String },
}

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Event { committee, seq } => write!(f, "the event {committee} seq {seq}"),
            Self::Cursor { committee, owner } => {
                write!(f, "the cursor of {owner} on {committee}")
            }
            Self::Heartbeat { committee, agent } => {
                write!(f, "the heartbeat of {agent} on {committee}")
            }
        }
    }
}

/// Why a log cannot be converted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FlattenError {
    #[error(
        "{at} names {value}, nested more than one agent deep; a Senate nested under an agent no longer exists, so this log is not converted (convert or remove that Senate's log by hand)"
    )]
    Deeper { at: Place, value: String },
    #[error(
        "the agent name {agent} is used by {first} and {second} (first in {at}); a name is unique in a Senate now, so this log is not converted"
    )]
    TwoOwners {
        agent: String,
        first: String,
        second: String,
        at: Place,
    },
    #[error(
        "the agent {owner}/{agent} would be named {agent}, which is a person of this log (first in {at}); this log is not converted"
    )]
    NamedLikeAPerson {
        owner: String,
        agent: String,
        at: Place,
    },
}

/// What a scan found, and the conversion it allows.
#[derive(Debug, Default)]
pub struct Flattener {
    /// Agent name → its person, and where it was first seen.
    owners: BTreeMap<String, (String, Place)>,
    /// Handles seen as people (`senator::<handle>`, and every owner).
    people: BTreeMap<String, Place>,
    first_error: Option<FlattenError>,
}

/// One senator path split: `(owner, agent)` for one agent deep, `None` for a
/// person; an error string for deeper.
fn split(path: &str) -> Result<Option<(&str, &str)>, ()> {
    let mut parts = path.split('/');
    let owner = parts.next().unwrap_or_default();
    match (parts.next(), parts.next()) {
        (None, _) => {
            let _ = owner;
            Ok(None)
        }
        (Some(agent), None) => Ok(Some((owner, agent))),
        (Some(_), Some(_)) => Err(()),
    }
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A bare old path, `owner/agent` (or deeper) of slugs, as payloads stored
/// an agent; `None` for anything else.
fn old_path(s: &str) -> Option<&str> {
    (s.contains('/') && s.split('/').all(|p| is_slug(p) || p == "*")).then_some(s)
}

impl Flattener {
    pub fn new() -> Self {
        Self::default()
    }

    fn fail(&mut self, error: FlattenError) {
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
    }

    fn see_person(&mut self, handle: &str, at: &Place) {
        self.people
            .entry(handle.to_string())
            .or_insert_with(|| at.clone());
    }

    /// Record one path (`wendell`, `wendell/claude`, `wendell/*`) seen at
    /// `at`.
    fn see_path(&mut self, path: &str, at: &Place) {
        match split(path) {
            Err(()) => self.fail(FlattenError::Deeper {
                at: at.clone(),
                value: path.to_string(),
            }),
            Ok(None) => self.see_person(path, at),
            Ok(Some((owner, "*"))) => self.see_person(owner, at),
            Ok(Some((owner, agent))) => {
                self.see_person(owner, at);
                match self.owners.get(agent) {
                    Some((first, _)) if first != owner => {
                        let first = first.clone();
                        self.fail(FlattenError::TwoOwners {
                            agent: agent.to_string(),
                            first: format!("{first}/{agent}"),
                            second: format!("{owner}/{agent}"),
                            at: at.clone(),
                        });
                    }
                    Some(_) => {}
                    None => {
                        self.owners
                            .insert(agent.to_string(), (owner.to_string(), at.clone()));
                    }
                }
            }
        }
    }

    /// Record an address string (`senator::…`), if it is one.
    pub fn see_address(&mut self, value: &str, at: &Place) {
        if let Some(path) = value.strip_prefix(SENATOR) {
            self.see_path(path, at);
        }
    }

    /// Record a bare agent path stored in a payload.
    pub fn see_agent(&mut self, value: &str, at: &Place) {
        if let Some(path) = old_path(value) {
            self.see_path(path, at);
        }
    }

    /// Record everything in a `meta` object or value.
    pub fn see_meta(&mut self, meta: &Value, at: &Place) {
        walk(meta, None, &mut |key, s| {
            if s.starts_with(SENATOR) {
                self.see_address(s, at);
            } else if key.is_some_and(|k| AGENT_KEYS.contains(&k)) {
                self.see_agent(s, at);
            }
        });
    }

    /// Record a whole stored event envelope.
    pub fn see_envelope(&mut self, envelope: &Value, at: &Place) {
        for key in ["from", "on_behalf_of"] {
            if let Some(s) = envelope.get(key).and_then(Value::as_str) {
                self.see_address(s, at);
            }
        }
        if let Some(to) = envelope.get("to").and_then(Value::as_array) {
            for s in to.iter().filter_map(Value::as_str) {
                self.see_address(s, at);
            }
        }
        if let Some(meta) = envelope.get("meta") {
            self.see_meta(meta, at);
        }
    }

    /// Whether anything old-form was seen at all.
    pub fn has_paths(&self) -> bool {
        !self.owners.is_empty() || self.first_error.is_some()
    }

    /// Refuse what cannot be converted: the first problem found.
    pub fn check(&self) -> Result<(), Box<FlattenError>> {
        if let Some(error) = &self.first_error {
            return Err(Box::new(error.clone()));
        }
        for (agent, (owner, at)) in &self.owners {
            if self.people.contains_key(agent) {
                return Err(Box::new(FlattenError::NamedLikeAPerson {
                    owner: owner.clone(),
                    agent: agent.clone(),
                    at: at.clone(),
                }));
            }
        }
        Ok(())
    }

    /// The agents converted, with the person each was named under: who
    /// runs it now.
    pub fn owners(&self) -> BTreeMap<String, String> {
        self.owners
            .iter()
            .map(|(agent, (owner, _))| (agent.clone(), owner.clone()))
            .collect()
    }

    /// A path, flattened: `wendell/claude` → `claude`, `wendell/*` → `*`,
    /// a person unchanged.
    fn flat_path(path: &str) -> String {
        match split(path) {
            Ok(Some((_, agent))) => agent.to_string(),
            _ => path.to_string(),
        }
    }

    /// An address string, converted (`senator::wendell/claude` →
    /// `senator::claude`); anything that is not a senator address unchanged.
    pub fn address(value: &str) -> String {
        match value.strip_prefix(SENATOR) {
            Some(path) => format!("{SENATOR}{}", Self::flat_path(path)),
            None => value.to_string(),
        }
    }

    /// A bare agent path stored in a payload, converted.
    pub fn agent(value: &str) -> String {
        match old_path(value) {
            Some(path) => Self::flat_path(path),
            None => value.to_string(),
        }
    }

    /// A `meta` object or value, converted.
    pub fn meta(meta: &Value) -> Value {
        convert(meta, None)
    }

    /// A stored event envelope, converted.
    pub fn envelope(envelope: &Value) -> Value {
        let mut out = envelope.clone();
        if let Some(object) = out.as_object_mut() {
            for key in ["from", "on_behalf_of"] {
                if let Some(Value::String(s)) = object.get_mut(key) {
                    *s = Self::address(s);
                }
            }
            if let Some(Value::Array(to)) = object.get_mut("to") {
                for v in to.iter_mut() {
                    if let Value::String(s) = v {
                        *s = Self::address(s);
                    }
                }
            }
            if let Some(meta) = object.get_mut("meta") {
                *meta = Self::meta(meta);
            }
        }
        out
    }
}

/// Every string in `value`, with the key it sits under (an array's items
/// sit under the array's key).
fn walk(value: &Value, key: Option<&str>, f: &mut impl FnMut(Option<&str>, &str)) {
    match value {
        Value::String(s) => f(key, s),
        Value::Array(items) => items.iter().for_each(|v| walk(v, key, f)),
        Value::Object(map) => map.iter().for_each(|(k, v)| walk(v, Some(k), f)),
        _ => {}
    }
}

fn convert(value: &Value, key: Option<&str>) -> Value {
    match value {
        Value::String(s) if s.starts_with(SENATOR) => Value::String(Flattener::address(s)),
        Value::String(s) if key.is_some_and(|k| AGENT_KEYS.contains(&k)) => {
            Value::String(Flattener::agent(s))
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| convert(v, key)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), convert(v, Some(k))))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The handles a scan saw as people, for a caller that seats them.
pub fn people(flattener: &Flattener) -> BTreeSet<String> {
    flattener.people.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(seq: i64) -> Place {
        Place::Event {
            committee: "refund".into(),
            seq,
        }
    }

    #[test]
    fn addresses_and_agent_payloads_are_flattened_and_people_kept() {
        assert_eq!(Flattener::address("senator::wendell"), "senator::wendell");
        assert_eq!(
            Flattener::address("senator::wendell/claude"),
            "senator::claude"
        );
        assert_eq!(Flattener::address("senator::wendell/*"), "senator::*");
        assert_eq!(Flattener::address("speaker"), "speaker");
        assert_eq!(Flattener::address("guest::chat/u_1"), "guest::chat/u_1");
        assert_eq!(Flattener::agent("wendell/codex"), "codex");
        assert_eq!(
            Flattener::agent("feature/refund-v2/wendell"),
            "feature/refund-v2/wendell"
        );

        let envelope = json!({
            "from": "senator::wendell/claude",
            "to": ["senator::wendell", "senator::wendell/codex", "speaker"],
            "on_behalf_of": "senator::wendell/codex",
            "body": "ask [senator::wendell/claude]",
            "meta": {
                "presence": { "agent": "wendell/claude", "new_owner": "wendell/codex" },
                "pick": { "owner": "wendell", "candidates": ["wendell/claude", "wendell/codex"] },
                "work": { "target_agent": "wendell/codex", "branch": "wendell/fix" },
                "proposed_by": "senator::wendell/claude",
                "mentions": [{ "address": "senator::wendell/codex", "name": "Codex" }],
                "checkpoint": { "branch": "feature/x/wendell", "commit": "abc" }
            }
        });
        let out = Flattener::envelope(&envelope);
        assert_eq!(out["from"], "senator::claude");
        assert_eq!(
            out["to"],
            json!(["senator::wendell", "senator::codex", "speaker"])
        );
        assert_eq!(out["on_behalf_of"], "senator::codex");
        // A body is text, as written.
        assert_eq!(out["body"], "ask [senator::wendell/claude]");
        let meta = &out["meta"];
        assert_eq!(meta["presence"]["agent"], "claude");
        assert_eq!(meta["presence"]["new_owner"], "codex");
        assert_eq!(meta["pick"]["owner"], "wendell");
        assert_eq!(meta["pick"]["candidates"], json!(["claude", "codex"]));
        assert_eq!(meta["work"]["target_agent"], "codex");
        // A branch is not a senator, wherever it is.
        assert_eq!(meta["work"]["branch"], "wendell/fix");
        assert_eq!(meta["checkpoint"]["branch"], "feature/x/wendell");
        assert_eq!(meta["proposed_by"], "senator::claude");
        assert_eq!(meta["mentions"][0]["address"], "senator::codex");

        let mut scan = Flattener::new();
        scan.see_envelope(&envelope, &at(1));
        assert!(scan.has_paths());
        assert_eq!(scan.check(), Ok(()));
        assert_eq!(scan.owners()["claude"], "wendell");
    }

    #[test]
    fn a_nested_agent_is_refused_naming_the_event() {
        let mut scan = Flattener::new();
        scan.see_envelope(
            &json!({ "from": "senator::wendell/claude/reviewer", "to": [] }),
            &at(7),
        );
        let err = *scan.check().unwrap_err();
        assert!(matches!(err, FlattenError::Deeper { .. }));
        let text = err.to_string();
        assert!(text.contains("refund seq 7"), "{text}");
        assert!(text.contains("wendell/claude/reviewer"), "{text}");
    }

    #[test]
    fn one_name_under_two_people_is_refused() {
        let mut scan = Flattener::new();
        scan.see_envelope(
            &json!({ "from": "senator::alice/claude", "to": [] }),
            &at(1),
        );
        scan.see_envelope(&json!({ "from": "senator::bob/claude", "to": [] }), &at(2));
        assert!(matches!(
            scan.check().map_err(|e| *e),
            Err(FlattenError::TwoOwners { .. })
        ));
    }

    #[test]
    fn an_agent_named_like_a_person_is_refused() {
        let mut scan = Flattener::new();
        scan.see_envelope(
            &json!({ "from": "senator::alice/bob", "to": ["senator::bob"] }),
            &at(1),
        );
        assert!(matches!(
            scan.check().map_err(|e| *e),
            Err(FlattenError::NamedLikeAPerson { .. })
        ));
    }

    #[test]
    fn a_flat_log_has_nothing_to_convert() {
        let mut scan = Flattener::new();
        scan.see_envelope(
            &json!({ "from": "senator::claude", "to": ["senator::*", "senator::wendell"] }),
            &at(1),
        );
        assert!(!scan.has_paths());
        assert_eq!(scan.check(), Ok(()));
    }
}
