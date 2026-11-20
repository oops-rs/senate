//! What the server refuses to store, by who is speaking.
//!
//! Two rules live here because both the HTTP API and the in-process hub
//! apply them: what no senator may write (the Speaker's and the server's own
//! keys), and what a sender that may not change state (a guest) may not write
//! at all.

use senate_core::{Kind, NewEvent, meta};

/// Keys only the Speaker writes. A senator carrying one, whatever its value,
/// is refused, so no event in the log looks like one the Speaker wrote.
pub(crate) const SPEAKER_KEYS: [&str; 5] = [
    meta::BRIEF_TOP,
    meta::TRIAGE,
    meta::OUTPUT_LEVEL,
    meta::WORK,
    meta::PROVISIONAL,
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
/// proposals and what they supersede, checkpoints, presence, duties, and everything
/// the Speaker writes (triage, level, work runs, provisional answers,
/// takeover).
const STATE_KEYS: [&str; 14] = [
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
