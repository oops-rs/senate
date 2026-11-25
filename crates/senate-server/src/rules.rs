//! What the server refuses to store, by who is speaking.
//!
//! Two rules live here because both the HTTP API and the in-process hub
//! apply them: what no senator may write (the Speaker's and the server's own
//! keys), and what a sender that may not change state (a guest) may not write
//! at all.

use senate_core::{Kind, NewEvent, meta};

pub(crate) const BRIDGE_KEYS: [&str; 1] = ["verified_origin"];

pub(crate) fn check_bridge_origin(event: &NewEvent, local: bool) -> Result<(), String> {
    if event.meta.contains_key(meta::DESK_WORKER_INTAKE) {
        return Err(format!(
            "meta.{} is written by the desk-worker intake endpoint",
            meta::DESK_WORKER_INTAKE
        ));
    }
    if !local
        && let Some(key) = BRIDGE_KEYS
            .iter()
            .find(|key| event.meta.contains_key(**key))
    {
        return Err(format!(
            "meta.{key} is reserved for the authenticated machine bridge"
        ));
    }
    Ok(())
}

/// Keys only the Speaker writes. A senator carrying one, whatever its value,
/// is refused, so no event in the log looks like one the Speaker wrote.
pub(crate) const SPEAKER_KEYS: [&str; 7] = [
    meta::BRIEF_TOP,
    meta::DIGEST,
    meta::TRIAGE,
    meta::OUTPUT_LEVEL,
    meta::WORK,
    meta::PROVISIONAL,
    // It takes a question for an offline child Senate off the desk.
    meta::CHILD_OFFLINE,
];

/// Kinds that change state: a binding, a decision, a presence transition, a
/// checkpoint, a task's status, and a request (a work order: it asks someone
/// to *do* something).
const STATE_KINDS: [Kind; 6] = [
    Kind::Binding,
    Kind::Decision,
    Kind::Presence,
    Kind::Checkpoint,
    Kind::Status,
    Kind::Request,
];

/// Meta keys that carry or cause a state change: bindings, status, decision
/// proposals and what they supersede, checkpoints, presence, duties, the
/// committee's memory and digests, and everything
/// the Speaker writes (triage, level, work runs, provisional answers,
/// takeover).
const STATE_KEYS: [&str; 16] = [
    meta::DIGEST,
    meta::MEMORY,
    meta::BINDING,
    meta::DUTY,
    meta::STATUS,
    meta::PROPOSAL,
    meta::PROPOSED_BY,
    meta::SUPERSEDES,
    meta::CHECKPOINT,
    meta::PRESENCE,
    meta::BRIEF_TOP,
    meta::TRIAGE,
    meta::OUTPUT_LEVEL,
    meta::WORK,
    meta::PROVISIONAL,
    meta::TAKEOVER,
];

/// A sender that [may not change state](senate_core::Address::may_change_state)
/// (a guest) may ask and say things, and be answered; it may not write a
/// kind or a meta key that changes state. `Ok` for every other sender.
pub(crate) fn check_may_change_state(event: &NewEvent) -> Result<(), String> {
    if event.from.may_change_state() {
        return Ok(());
    }
    if STATE_KINDS.contains(&event.kind) {
        return Err(format!(
            "{} may ask questions and read answers, not write {} events",
            event.from, event.kind
        ));
    }
    if let Some(key) = STATE_KEYS.iter().find(|k| event.meta.contains_key(**k)) {
        return Err(format!(
            "{} may not change state: meta.{key} is refused",
            event.from
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_private_route_accepts_verified_origin() {
        let note = NewEvent::new(
            "design".parse().unwrap(),
            Kind::Note,
            senate_core::Address::Speaker,
            "Fixture note",
        );
        assert!(check_bridge_origin(&note, false).is_ok());

        let stamped = note.with_meta("verified_origin", serde_json::json!("person-terminal"));
        assert!(check_bridge_origin(&stamped, true).is_ok());
        assert!(
            check_bridge_origin(&stamped, false)
                .unwrap_err()
                .contains("meta.verified_origin is reserved")
        );
    }
}
