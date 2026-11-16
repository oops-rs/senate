//! Deterministic identities, so every append and post the Speaker makes is
//! idempotent across retries and restarts.

use senate_core::{CommitteeId, EventId, Seq};
use sha2::{Digest, Sha256};

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// botd's ceiling on an `operation_id`, in bytes.
const OPERATION_ID_MAX: usize = 64;
const OPERATION_PREFIX: &str = "senate:";
/// Hex digits of the hashed form: 192 bits.
const OPERATION_HASH_HEX: usize = 48;

/// An [`EventId`] that is a pure function of `parts`: the first 128 bits of
/// a SHA-256 over the NUL-separated parts, rendered as a ULID. Re-deciding
/// the same thing after a crash re-appends the same id, which the log stores
/// once.
pub(crate) fn derived_event_id(parts: &[&str]) -> EventId {
    let mut hasher = Sha256::new();
    hasher.update(b"senate-speaker\0");
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    let value = u128::from_be_bytes(bytes);
    let text: String = (0..26)
        .map(|i| CROCKFORD[((value >> (125 - 5 * i)) & 0x1f) as usize] as char)
        .collect();
    // Cannot fail: a ULID's alphabet and length by construction (tested).
    text.parse().unwrap_or_default()
}

/// botd's idempotency key for the copy of log event `seq` posted to `target`
/// (the group posted into; see [`senate_core::MirrorRef`]):
/// `senate:<epoch>:<committee>:<seq>:<target>`.
///
/// The epoch is what keeps a reset log (seq starting over at 1) from reusing
/// a key botd already holds, which botd would refuse as `operation_conflict`.
/// botd caps the key at 64 bytes, so a longer one is sent as `senate:` plus
/// 48 hex digits of its SHA-256; that form has no further `:`, so it never
/// equals a verbatim one.
pub(crate) fn operation_id(epoch: &str, committee: &CommitteeId, seq: Seq, target: &str) -> String {
    let full = format!("{OPERATION_PREFIX}{epoch}:{committee}:{seq}:{target}");
    if full.len() <= OPERATION_ID_MAX {
        return full;
    }
    let digest = hex::encode(Sha256::digest(full.as_bytes()));
    format!("{OPERATION_PREFIX}{}", &digest[..OPERATION_HASH_HEX])
}

/// The key for a reply in a group bound to no committee: there is no seq, so
/// it is derived from the message being answered.
pub(crate) fn unbound_operation_id(group_id: &str, message_ref: &str) -> String {
    let digest = hex::encode(Sha256::digest(
        format!("{group_id}\0{message_ref}").as_bytes(),
    ));
    format!("senate:unbound:{}", &digest[..20])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derived_ids_are_stable_valid_ulids() {
        let a = derived_event_id(&["relay", "refund-v2", "7"]);
        assert_eq!(a, derived_event_id(&["relay", "refund-v2", "7"]));
        assert_ne!(a, derived_event_id(&["relay", "refund-v2", "8"]));
        let text = a.to_string();
        assert_eq!(text.len(), 26);
        assert_eq!(text.parse::<EventId>().ok(), Some(a));
    }

    #[test]
    fn operation_ids_name_the_epoch_and_the_target_and_fit_botd() {
        let committee = CommitteeId::new("refund-v2").expect("committee");
        assert_eq!(
            operation_id("e1", &committee, Seq(120), "backend"),
            "senate:e1:refund-v2:120:backend"
        );
        // A reset log reuses the seq, never the key.
        assert_ne!(
            operation_id("e1", &committee, Seq(1), "backend"),
            operation_id("e2", &committee, Seq(1), "backend")
        );
        // A second copy of one event (phase 2) gets its own key.
        assert_ne!(
            operation_id("e1", &committee, Seq(1), "backend"),
            operation_id("e1", &committee, Seq(1), "backend#ack")
        );
        // Long parts are hashed down to botd's limit, still deterministic.
        let epoch = "01JBZ6V3K8X5Q2W9R7T4Y1M0N6";
        let alias = format!("oc_{}", "f".repeat(60));
        let long = operation_id(epoch, &committee, Seq(118), &alias);
        assert!(long.len() <= OPERATION_ID_MAX, "{long}");
        assert!(long.starts_with("senate:"));
        assert_eq!(long, operation_id(epoch, &committee, Seq(118), &alias));
        assert_ne!(long, operation_id(epoch, &committee, Seq(119), &alias));
        assert_ne!(
            unbound_operation_id("backend", "om_1"),
            unbound_operation_id("backend", "om_2")
        );
    }
}
