//! Durable receipt for one hosted spawn. The preparing state is safe to
//! retry. Once creation may have started, only a recorded receipt can tell
//! us which session it produced.

use serde::{Deserialize, Serialize};
use uuid::{Uuid, Version};

use super::host_policy::{SpawnInput, SpawnOrder};
use super::*;
use crate::ipc::{SpawnReply, spawn_outcome_unknown};

const LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1000;
const CLOCK_SKEW_MS: i64 = 60 * 1000;

#[derive(Deserialize, Serialize)]
enum Phase {
    Prepared,
    Launching,
    Created(SpawnReply),
}

#[derive(Deserialize, Serialize)]
struct Record {
    actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<SpawnInput>,
    order: SpawnOrder,
    phase: Phase,
}

pub(super) struct SpawnAttempt {
    id: String,
    expires: i64,
    record: Record,
    saved: String,
}

impl SpawnAttempt {
    fn save_phase(&mut self, store: &Store) -> Result<(), IpcError> {
        let next = serde_json::to_string(&self.record).map_err(IpcError::internal)?;
        // The listener may have been restarted while its old connection
        // handler was still active. Only the handler that claims Prepared
        // durably can cross the creation boundary.
        if !store
            .advance_spawn_attempt(&self.id, &self.saved, &next)
            .map_err(internal)?
        {
            return Err(changed(&self.id));
        }
        self.saved = next;
        Ok(())
    }

    fn advance(&mut self, store: &Store, next: Phase) -> Result<(), IpcError> {
        let previous = std::mem::replace(&mut self.record.phase, next);
        if let Err(error) = self.save_phase(store) {
            self.record.phase = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(super) fn begin_launch(&mut self, store: &Store) -> Result<(), IpcError> {
        if self.expires <= chrono::Utc::now().timestamp_millis() {
            return Err(expired(&self.id));
        }
        self.advance(store, Phase::Launching)
    }

    pub(super) fn record_created(
        &mut self,
        store: &Store,
        reply: &SpawnReply,
    ) -> Result<(), IpcError> {
        if matches!(&self.record.phase, Phase::Created(saved) if saved == reply) {
            return Ok(());
        }
        self.advance(store, Phase::Created(reply.clone()))
    }
}

fn expired(id: &str) -> IpcError {
    IpcError::new(
        codes::SPAWN_OUTCOME_UNKNOWN,
        format!(
            "Spawn outcome unknown (attempt {id}): the seven-day retry window has expired. \
             Check `senate session list`."
        ),
    )
}

fn changed(id: &str) -> IpcError {
    spawn_outcome_unknown(id, "the journal changed while processing this attempt")
}

impl Shared {
    pub(super) async fn on_spawn_attempt(
        &self,
        selector: &SessionSelector,
        input: SpawnInput,
        id: &str,
    ) -> Result<SpawnReply, IpcError> {
        let id = Uuid::parse_str(id)
            .ok()
            .filter(|id| id.get_version() == Some(Version::SortRand))
            .ok_or_else(|| IpcError::bad_request("spawn attempt ID must be a UUIDv7"))?;
        // UUIDv7's first 48 bits are its Unix timestamp in milliseconds.
        let created = i64::try_from(id.as_u128() >> 80).map_err(IpcError::internal)?;
        let expires = created + LIFETIME_MS;
        let id = id.to_string();

        let auth = self.authorize_host_action(selector).await?;
        auth.check_target(&input.committee, input.chair || input.secretary)?;
        let actor = auth.actor();

        // Wait for an in-flight launch before consulting its journal. The
        // connection handler finishes the operation even if its client leaves.
        let _guard = self.spawn_attempts.lock().await;
        let saved = self.store.spawn_attempt(&id).map_err(|error| {
            spawn_outcome_unknown(&id, format!("the journal could not be read: {error}"))
        })?;
        let now = chrono::Utc::now().timestamp_millis();
        if expires <= now {
            return Err(expired(&id));
        }
        if created > now + CLOCK_SKEW_MS {
            return Err(IpcError::bad_request(
                "spawn attempt ID is dated in the future",
            ));
        }
        let (record, saved) = match saved {
            Some(json) => {
                let record: Record = serde_json::from_str(&json).map_err(|error| {
                    spawn_outcome_unknown(
                        &id,
                        format!("the journal receipt is unreadable: {error}"),
                    )
                })?;
                let same_input = match &record.input {
                    Some(original) => original == &input,
                    // Older receipts recorded the concrete order only.
                    None => {
                        input.profile.is_none()
                            && self
                                .resolve_spawn_order(input)
                                .is_ok_and(|order| order == record.order)
                    }
                };
                if record.actor != actor || !same_input {
                    return Err(IpcError::bad_request(
                        "this spawn attempt ID belongs to a different caller or spawn order",
                    ));
                }
                match &record.phase {
                    Phase::Prepared => (record, json),
                    Phase::Launching => {
                        return Err(spawn_outcome_unknown(
                            &id,
                            "no session receipt was recorded",
                        ));
                    }
                    Phase::Created(reply) => return Ok(reply.clone()),
                }
            }
            None => {
                let record = Record {
                    actor,
                    order: self.resolve_spawn_order(input.clone())?,
                    input: Some(input),
                    phase: Phase::Prepared,
                };
                let json = serde_json::to_string(&record).map_err(IpcError::internal)?;
                if !self
                    .store
                    .insert_spawn_attempt(&id, &json, expires)
                    .map_err(internal)?
                {
                    return Err(changed(&id));
                }
                (record, json)
            }
        };

        let mut attempt = SpawnAttempt {
            id,
            expires,
            record,
            saved,
        };
        let result = self
            .spawn_authorized(&auth, attempt.record.order.clone(), Some(&mut attempt))
            .await;
        match result {
            Err(error)
                if error.code != codes::SPAWN_OUTCOME_UNKNOWN
                    && matches!(attempt.record.phase, Phase::Launching) =>
            {
                Err(spawn_outcome_unknown(&attempt.id, error))
            }
            result => result,
        }
    }
}
