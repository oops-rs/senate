//! What the server keeps in the store's registry beside the log
//! ([`LogStore::registry`]): facts that are no committee's events and must
//! survive a restart.
//!
//! | Key | Value |
//! | --- | --- |
//! | `seat:<agent>` | `{"owner": "<principal>"}`: who runs the agent |
//! | `secretaries:<senator>` | `["<secretary>", ...]`, in order |
//! | `child:<name>` | a [`ChildSenate`] |
//! | `note:<key>` | `true`: a start note already said once |
//!
//! Configuration seeds the roster; what was registered at runtime is
//! loaded over it at start.

use std::collections::BTreeMap;

use senate_core::api::ChildSenate;
use senate_core::{Name, SenatorName};
use senate_log::{LogError, LogStore};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const SEAT: &str = "seat:";
pub(crate) const SECRETARIES: &str = "secretaries:";
pub(crate) const CHILD: &str = "child:";
pub(crate) const NOTE: &str = "note:";

/// `seat:<agent>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SeatRecord {
    pub owner: Name,
}

/// Entries under `prefix`, each decoded; one that does not decode is
/// logged and skipped.
async fn entries<K, V>(store: &dyn LogStore, prefix: &str) -> Result<Vec<(K, V)>, LogError>
where
    K: std::str::FromStr,
    V: serde::de::DeserializeOwned,
{
    let mut out = Vec::new();
    for (key, value) in store.registry(prefix).await? {
        let Some(raw) = key.strip_prefix(prefix) else {
            continue;
        };
        let Ok(k) = raw.parse::<K>() else {
            tracing::warn!(%key, "registry key does not name what it should; skipped");
            continue;
        };
        match serde_json::from_value::<V>(value) {
            Ok(v) => out.push((k, v)),
            Err(e) => tracing::warn!(%key, "registry entry does not decode; skipped: {e}"),
        }
    }
    Ok(out)
}

pub(crate) async fn seats(store: &dyn LogStore) -> Result<Vec<(SenatorName, Name)>, LogError> {
    Ok(entries::<SenatorName, SeatRecord>(store, SEAT)
        .await?
        .into_iter()
        .map(|(agent, record)| (agent, record.owner))
        .collect())
}

pub(crate) async fn put_seat(
    store: &dyn LogStore,
    agent: &SenatorName,
    owner: &Name,
) -> Result<(), LogError> {
    let record = SeatRecord {
        owner: owner.clone(),
    };
    store
        .put_registry(
            &format!("{SEAT}{agent}"),
            Some(serde_json::to_value(record).unwrap_or(Value::Null)),
        )
        .await
}

pub(crate) async fn secretaries(
    store: &dyn LogStore,
) -> Result<Vec<(SenatorName, Vec<SenatorName>)>, LogError> {
    entries(store, SECRETARIES).await
}

pub(crate) async fn put_secretaries(
    store: &dyn LogStore,
    principal: &SenatorName,
    secretaries: &[SenatorName],
) -> Result<(), LogError> {
    let value =
        (!secretaries.is_empty()).then(|| serde_json::to_value(secretaries).unwrap_or(Value::Null));
    store
        .put_registry(&format!("{SECRETARIES}{principal}"), value)
        .await
}

pub(crate) async fn children(
    store: &dyn LogStore,
) -> Result<BTreeMap<Name, ChildSenate>, LogError> {
    Ok(entries::<Name, ChildSenate>(store, CHILD)
        .await?
        .into_iter()
        .collect())
}

pub(crate) async fn put_child(store: &dyn LogStore, child: &ChildSenate) -> Result<(), LogError> {
    store
        .put_registry(
            &format!("{CHILD}{}", child.name),
            Some(serde_json::to_value(child).unwrap_or(Value::Null)),
        )
        .await
}

/// Whether the note `key` was said before; marks it said. A store that
/// keeps no registry says every note at every start.
pub(crate) async fn first_time(store: &dyn LogStore, key: &str) -> bool {
    let key = format!("{NOTE}{key}");
    match store.registry(&key).await {
        Ok(found) if found.iter().any(|(k, _)| k == &key) => false,
        _ => {
            if let Err(e) = store.put_registry(&key, Some(Value::Bool(true))).await {
                tracing::debug!(%key, "the note will be said again next start: {e}");
            }
            true
        }
    }
}

/// One registry write replaces the complete mapping, including release of the previous alias.
pub(crate) async fn aliases(
    store: &dyn LogStore,
) -> Result<senate_core::api::SenatorAliases, LogError> {
    let Some((_, value)) = store
        .registry("senator-aliases")
        .await?
        .into_iter()
        .find(|(key, _)| key == "senator-aliases")
    else {
        return Ok(BTreeMap::new());
    };
    serde_json::from_value(value).map_err(|e| LogError::Invalid(e.to_string()))
}

pub(crate) async fn put_aliases(
    store: &dyn LogStore,
    aliases: &senate_core::api::SenatorAliases,
) -> Result<(), LogError> {
    let value = serde_json::to_value(aliases).map_err(|e| LogError::Invalid(e.to_string()))?;
    store.put_registry("senator-aliases", Some(value)).await
}
